use crate::domain::StoredObservation;
use crate::rlm::budget::BudgetExhausted;
use crate::rlm::evidence::{EvidenceId, HandleId, HandleKind, HandleRegistry};
use crate::rlm::model::egress::{self, Role};
use crate::rlm::model::{EvidenceItem, EvidencePacket, OutputError, parse_output};
use crate::rlm::protocol::*;
use crate::search::{self, Budget, Order, SearchRequest};
use crate::store::{Store, is_interrupt};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
pub enum ExecError {
    Store(rusqlite::Error),
    Search(search::SearchError),
    HandleNotFound(String),
    /// A handle reference named a set of the wrong kind for its operator.
    WrongKind {
        handle: HandleId,
        expected: HandleKind,
    },
    Budget(BudgetExhausted),
    /// The investigation's cancel flag was set.
    Cancelled,
}

/// Why a child result was rejected whole (ADR 0009).
#[derive(Debug, PartialEq)]
pub enum ChildRejection {
    Oversize {
        bytes: usize,
    },
    Malformed(String),
    Version(u8),
    PacketId(String),
    UnknownItem(String),
    DuplicateItem(String),
    Citation {
        item_id: String,
        evidence_id: String,
    },
}

impl std::fmt::Display for ChildRejection {
    #[expect(
        clippy::use_debug,
        reason = "quotes the ids the model wrote, with their control characters escaped"
    )]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Oversize { bytes } => {
                write!(f, "{bytes} bytes, over the {CHILD_RESULT_MAX}-byte cap")
            }
            Self::Malformed(e) => write!(f, "not a child result: {e}"),
            Self::Version(v) => write!(f, "version {v}, not 1"),
            Self::PacketId(id) => write!(f, "packet_id {id:?} is not the packet sent"),
            Self::UnknownItem(id) => write!(f, "item_id {id:?} is not in the packet"),
            Self::DuplicateItem(id) => write!(f, "item_id {id:?} assessed twice"),
            Self::Citation {
                item_id,
                evidence_id,
            } => {
                write!(
                    f,
                    "{item_id:?} cites {evidence_id:?}, which is not in the packet"
                )
            }
        }
    }
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "reading the store: {e}"),
            Self::Search(e) => e.fmt(f),
            Self::HandleNotFound(h) => write!(f, "no result named {h}"),
            Self::WrongKind { handle, expected } => write!(f, "{handle} is not {expected}"),
            Self::Budget(b) => b.fmt(f),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for ExecError {}

/// Execute a search operator against the store.
pub fn execute_search(
    store: &Store,
    search: &SearchV1,
    registry: &mut HandleRegistry,
    budget: &Budget,
    notes: &mut Vec<String>,
) -> Result<HandleId, ExecError> {
    // The shared search has no OR: a word matches whole and every word must
    // match. So `any` is one search per term, merged newest first.
    let all;
    let queries: Vec<&str> = match search.match_mode {
        MatchMode::Any => search.terms.iter().map(String::as_str).collect(),
        MatchMode::All => {
            all = search.terms.join(" ");
            vec![&all]
        }
    };
    let limit = search.limit as usize;
    let mut hits = Vec::new();
    for query in queries {
        // Through the shared use case, so the model's searches run under the
        // same word cap and deadline as the user's (ADR 0006). One budget for
        // all the terms: the step as a whole has the deadline.
        let req = SearchRequest {
            query,
            limit,
            mode: search::MatchMode::Words,
            order: Order::RecentlyImported,
        };
        match search::search(store, &req, budget) {
            Ok(page) => hits.extend(page.hits),
            // A step result, not a failed investigation: the root sees an
            // empty set and why, and can plan rarer terms (#26 handoff).
            Err(e @ search::SearchError::TooBroad) => {
                notes.push(format!("{}: {e}", search.op_id));
                return Ok(registry.insert_evidence_set(Vec::<StoredObservation>::new()));
            }
            Err(search::SearchError::Cancelled) => return Err(ExecError::Cancelled),
            Err(e) => return Err(ExecError::Search(e)),
        }
    }
    // Row id order is import order, the order each page already has.
    hits.sort_by_key(|h| std::cmp::Reverse(h.id));
    hits.dedup_by_key(|h| h.id);
    hits.truncate(limit);

    // Fetch full StoredObservation for each hit.
    let mut observations = Vec::with_capacity(hits.len());
    for hit in &hits {
        if let Some(obs) = store.get_observation(hit.id).map_err(ExecError::Store)? {
            observations.push(obs);
        }
    }

    Ok(registry.insert_evidence_set(observations))
}

/// Execute an expand operator: get neighboring observations around each seed.
pub fn execute_expand(
    store: &Store,
    expand: &ExpandV1,
    registry: &mut HandleRegistry,
    plan_outputs: &PlanOutputs,
    cancel: &Arc<AtomicBool>,
) -> Result<HandleId, ExecError> {
    let source_handle_id = resolve_input(&expand.input, plan_outputs)?;
    let seed_obs = registry
        .get_observations(source_handle_id)
        .ok_or_else(|| ExecError::HandleNotFound(source_handle_id.to_string()))?;

    let stop = {
        let cancel = Arc::clone(cancel);
        move || cancel.load(Ordering::Relaxed)
    };
    let all_neighbors = store
        .interruptible(stop, |store| {
            let mut seen_ids = std::collections::HashSet::new();
            let mut all_neighbors: Vec<StoredObservation> = Vec::new();
            for obs in seed_obs {
                for n in store.get_neighbors(
                    obs.revision_id,
                    obs.ordinal,
                    expand.before,
                    expand.after,
                )? {
                    if seen_ids.insert(n.id) {
                        all_neighbors.push(n);
                    }
                }
                if all_neighbors.len() >= expand.limit as usize {
                    all_neighbors.truncate(expand.limit as usize);
                    break;
                }
            }
            Ok(all_neighbors)
        })
        .map_err(|e| {
            if is_interrupt(&e) {
                ExecError::Cancelled
            } else {
                ExecError::Store(e)
            }
        })?;

    Ok(registry.insert_evidence_set(all_neighbors))
}

/// Most items in one child call (research, operator algebra: assess).
pub const CHILD_ITEMS_MAX: usize = 4;
/// Most bytes of one note to the root; root state has a 64 KiB cap.
const NOTE_MAX: usize = 256;

/// Most bytes of a child's reply. Checked before parsing, so a huge body
/// costs nothing to refuse (research, security boundary 7).
pub const CHILD_RESULT_MAX: usize = 16 * 1024;

/// The verdicts in `raw` on each item of `packet`, in packet order; `None`
/// where the child skipped an item. Any fault rejects the whole result.
pub fn check_child_result(
    packet: &EvidencePacket,
    raw: &str,
) -> Result<Vec<Option<ItemAssessmentV1>>, ChildRejection> {
    let result: ChildResultV1 = parse_output(raw, CHILD_RESULT_MAX).map_err(|e| match e {
        OutputError::Oversize { bytes } => ChildRejection::Oversize { bytes },
        OutputError::Malformed(e) => ChildRejection::Malformed(e),
    })?;
    if result.version != 1 {
        return Err(ChildRejection::Version(result.version));
    }
    if result.packet_id != packet.packet_id {
        return Err(ChildRejection::PacketId(result.packet_id));
    }
    let mut slots: Vec<Option<ItemAssessmentV1>> = packet.items.iter().map(|_| None).collect();
    for a in result.assessments {
        let Some(i) = packet
            .items
            .iter()
            .position(|item| item.item_id == a.item_id)
        else {
            return Err(ChildRejection::UnknownItem(a.item_id));
        };
        let cited = a
            .claims
            .iter()
            .flat_map(|c| c.supports.iter().chain(&c.contradicts));
        if let Some(bad) = cited.into_iter().find(|ev| {
            EvidenceId::parse(ev)
                .is_none_or(|id| !packet.items.iter().any(|item| item.evidence_id == id))
        }) {
            return Err(ChildRejection::Citation {
                item_id: a.item_id,
                evidence_id: bad.clone(),
            });
        }
        if slots[i].is_some() {
            return Err(ChildRejection::DuplicateItem(a.item_id));
        }
        slots[i] = Some(a);
    }
    Ok(slots)
}

/// What a child call shows the model.
pub fn child_payload(objective: &str, packet: &EvidencePacket) -> serde_json::Value {
    serde_json::json!({"objective": objective, "packet": packet})
}

/// Bytes the child request for `packet` counts against its cap.
fn packet_bytes(objective: &str, packet: &EvidencePacket) -> usize {
    egress::request_bytes(Role::Child, &child_payload(objective, packet))
}

/// Split `items` in order into packets of at most [`CHILD_ITEMS_MAX`] items
/// whose child requests fit [`egress::CHILD_MAX`], instructions and schema
/// included. An item too big for a packet of its own
/// is cut at a char boundary and marked with [`TruncationV1`].
pub fn packets(objective: &str, items: Vec<EvidenceItem>) -> Vec<EvidencePacket> {
    // Fixed width, so moving an item to the next packet cannot grow it:
    // admission caps an assess at 64 items, so at most 64 packets.
    let id = |n: usize| format!("pkt_{n:02}");
    let mut out: Vec<EvidencePacket> = Vec::new();
    let over = |p: &EvidencePacket| {
        p.items.len() > CHILD_ITEMS_MAX || packet_bytes(objective, p) > egress::CHILD_MAX
    };
    let mut packet = EvidencePacket {
        packet_id: id(0),
        items: Vec::new(),
    };
    for item in items {
        packet.items.push(item);
        if packet.items.len() > 1 && over(&packet) {
            let moved = packet.items.pop().into_iter().collect();
            let next = EvidencePacket {
                packet_id: id(out.len() + 1),
                items: moved,
            };
            out.push(std::mem::replace(&mut packet, next));
        }
        if over(&packet) {
            truncate_to_fit(objective, &mut packet);
        }
    }
    if !packet.items.is_empty() {
        out.push(packet);
    }
    out
}

/// Cut the one item in `packet` to the longest prefix that fits.
fn truncate_to_fit(objective: &str, packet: &mut EvidencePacket) {
    let text = std::mem::take(&mut packet.items[0].safe_text);
    let fits = |packet: &mut EvidencePacket, n: usize| {
        let item = &mut packet.items[0];
        item.safe_text.clear();
        item.safe_text.push_str(&text[..n]);
        item.truncation = Some(TruncationV1 {
            original_bytes: text.len(),
            sent_bytes: n,
        });
        packet_bytes(objective, packet) <= egress::CHILD_MAX
    };
    // Size only grows with the prefix, so binary search over byte lengths;
    // the empty prefix fits because admission caps the objective.
    let (mut lo, mut hi) = (0, text.len());
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(packet, text.floor_char_boundary(mid)) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    fits(packet, text.floor_char_boundary(lo));
}

/// Makes the child calls for a batch of packets: each packet's reply body,
/// or why it has none.
pub type CallChildren<'a> =
    dyn FnMut(&[EvidencePacket], &str) -> Result<Vec<Result<String, String>>, ExecError> + 'a;

/// Execute an assess operator: batch evidence into child model calls.
pub fn execute_assess(
    assess: &AssessV1,
    registry: &mut HandleRegistry,
    plan_outputs: &PlanOutputs,
    call_children: &mut CallChildren,
    notes: &mut Vec<String>,
) -> Result<HandleId, ExecError> {
    let source_handle_id = resolve_input(&assess.input, plan_outputs)?;
    let source_obs = registry
        .get_observations(source_handle_id)
        .ok_or_else(|| ExecError::HandleNotFound(source_handle_id.to_string()))?;

    let evidence_handles = registry
        .get_evidence_handles(source_handle_id)
        .ok_or_else(|| ExecError::HandleNotFound(source_handle_id.to_string()))?;

    // Select up to max_items observations.
    let selected_count = source_obs.len().min(assess.max_items as usize);

    let items = (0..selected_count)
        .map(|i| EvidenceItem {
            item_id: format!("item_{i}"),
            evidence_id: evidence_handles[i].id,
            safe_text: source_obs[i].safe_text.clone(),
            truncation: None,
        })
        .collect();
    // Redacted once here so the packets are sized as they will be sent: the
    // egress seam redacts again, and safe text redacts to itself.
    let objective = crate::redact::Redactor::new().redact(&assess.objective);
    let packets = packets(&objective, items);
    let replies = call_children(&packets, &objective)?;
    let mut assessments = Vec::with_capacity(selected_count);
    for (packet, reply) in packets.iter().zip(replies) {
        let checked = reply
            .map_err(|e| format!("child call for {} failed: {e}", packet.packet_id))
            .and_then(|raw| {
                check_child_result(packet, &raw)
                    .map_err(|why| format!("child result for {} rejected: {why}", packet.packet_id))
            });
        match checked {
            Ok(slots) => assessments.extend(slots),
            // The other packets' verdicts stand; this one's items stay
            // unassessed and the root is told why.
            Err(why) => {
                let mut note = format!("{}: {why}", assess.op_id);
                // It quotes the model's ids, which only the 16 KiB body cap bounds.
                note.truncate(note.floor_char_boundary(NOTE_MAX));
                notes.push(note);
                assessments.extend(packet.items.iter().map(|_| None));
            }
        }
    }

    let assessed_obs: Vec<_> = source_obs[..selected_count]
        .iter()
        .map(Arc::clone)
        .collect();
    Ok(registry.insert_assessment_set(assessed_obs, assessments))
}

/// Execute a select operator: the items of an assessment set whose verdict
/// is one of `select.verdicts`, in order. Unassessed items never pass.
pub fn execute_select(
    select: &SelectV1,
    registry: &mut HandleRegistry,
    plan_outputs: &PlanOutputs,
) -> Result<HandleId, ExecError> {
    let source = resolve_input(&select.input, plan_outputs)?;
    let handle = registry
        .get(source)
        .ok_or_else(|| ExecError::HandleNotFound(source.to_string()))?;
    // Plan admission checks plan outputs; a handle from an earlier turn
    // is checked here.
    if handle.kind != HandleKind::AssessmentSet {
        return Err(ExecError::WrongKind {
            handle: source,
            expected: HandleKind::AssessmentSet,
        });
    }
    let observations = registry.get_observations(source).unwrap_or_default();
    let assessments = registry.get_assessments(source).unwrap_or_default();
    let picked: Vec<_> = observations
        .iter()
        .zip(assessments)
        .filter(|(_, a)| {
            a.as_ref()
                .is_some_and(|a| select.verdicts.contains(&a.verdict))
        })
        .map(|(o, _)| Arc::clone(o))
        .take(select.limit as usize)
        .collect();
    Ok(registry.insert_evidence_set(picked))
}

/// The handle each op of the running plan made, by its op_id.
pub type PlanOutputs<'p> = std::collections::HashMap<&'p str, HandleId>;

/// Resolve an InputRefV1 to the id of the handle it names. A handle the
/// root names that no set has is found missing when it is read.
pub fn resolve_input(
    input: &InputRefV1,
    plan_outputs: &PlanOutputs,
) -> Result<HandleId, ExecError> {
    match input {
        InputRefV1::Output(op_id) => plan_outputs.get(op_id.as_str()).copied(),
        InputRefV1::Handle(handle_id) => HandleId::parse(handle_id),
    }
    .ok_or_else(|| match input {
        InputRefV1::Output(name) | InputRefV1::Handle(name) => {
            ExecError::HandleNotFound(name.clone())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SourceKind;
    use crate::domain::{Observation, ObservationKind, Provenance};
    use crate::rlm::evidence::HandleRegistry;
    use crate::rlm::model::{FakeModel, ModelBoundary};

    /// The child calls for `packets`, one after another through the egress
    /// seam, unmetered.
    fn fake_children(
        model: &FakeModel,
        packets: &[EvidencePacket],
        objective: &str,
    ) -> Result<Vec<Result<String, String>>, ExecError> {
        let mut ledger = crate::rlm::budget::BudgetLedger::new(BudgetV1::prototype_default());
        let call = |p: &EvidencePacket| {
            let req = egress::EgressRequest::new(
                Role::Child,
                child_payload(objective, p),
                model.target(),
                &mut ledger,
                &mut |_| {},
            )
            .unwrap();
            model
                .call(std::sync::Arc::new(req), std::time::Instant::now())
                .map(|r| r.body)
                .map_err(|e| e.to_string())
        };
        Ok(packets.iter().map(call).collect())
    }
    use crate::store::{Cursor, Store};
    use std::collections::HashMap;

    fn seed_store(store: &mut Store) -> i64 {
        let rev = store
            .lock_source(b"/h")
            .unwrap()
            .new_revision(SourceKind::Zsh)
            .unwrap();
        let id = rev.revision_id();
        rev.commit(&Cursor::default(), Provenance::COMMAND).unwrap();
        id
    }

    fn insert_commands(store: &mut Store, rev_id: i64, commands: &[&str]) {
        let mut rev = store
            .lock_source(b"/h")
            .unwrap()
            .append(SourceKind::Zsh)
            .unwrap();
        assert_eq!(rev.revision_id(), rev_id);
        for (i, cmd) in commands.iter().enumerate() {
            rev.insert(&Observation {
                kind: ObservationKind::ZshCommand,
                observed_at_ms: Some(1_700_000_000_000 + i as i64),
                safe_text: cmd.to_string(),
                flags: 0,
                locator: vec![],
            })
            .unwrap();
        }
        rev.commit(&Cursor::default(), Provenance::COMMAND).unwrap();
    }

    #[test]
    fn search_finds_matching_records() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = seed_store(&mut store);
        insert_commands(
            &mut store,
            rev_id,
            &[
                "cargo build --release",
                "git push origin main",
                "cargo test --lib",
                "docker compose up",
            ],
        );

        let mut registry = HandleRegistry::new(64);
        let search = SearchV1 {
            op_id: "s1".to_string(),
            terms: vec!["cargo".to_string()],
            match_mode: MatchMode::Any,
            limit: 50,
        };

        let hid = execute_search(
            &store,
            &search,
            &mut registry,
            &Budget::start(),
            &mut Vec::new(),
        )
        .unwrap();
        let handle = registry.get(hid).unwrap();
        assert_eq!(handle.kind, HandleKind::EvidenceSet);
        assert_eq!(handle.count, 2);

        let obs = registry.get_observations(hid).unwrap();
        assert!(obs.iter().all(|o| o.safe_text.contains("cargo")));
    }

    #[test]
    fn search_any_matches_either_term_and_all_needs_both() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = seed_store(&mut store);
        insert_commands(
            &mut store,
            rev_id,
            &[
                "cargo build",
                "git push",
                "cargo git-hook",
                "docker ps",
                "git or cargo",
            ],
        );
        let mut registry = HandleRegistry::new(64);
        let mut run = |match_mode, limit| {
            let search = SearchV1 {
                op_id: "s1".into(),
                terms: vec!["cargo".into(), "git".into()],
                match_mode,
                limit,
            };
            let hid = execute_search(
                &store,
                &search,
                &mut registry,
                &Budget::start(),
                &mut Vec::new(),
            )
            .unwrap();
            registry
                .get_observations(hid)
                .unwrap()
                .iter()
                .map(|o| o.safe_text.clone())
                .collect::<Vec<_>>()
        };
        // Newest first, each once, and not only rows with the word "or".
        assert_eq!(
            run(MatchMode::Any, 10),
            ["git or cargo", "cargo git-hook", "git push", "cargo build"]
        );
        assert_eq!(run(MatchMode::Any, 2), ["git or cargo", "cargo git-hook"]);
        assert_eq!(run(MatchMode::All, 10), ["git or cargo", "cargo git-hook"]);
    }

    #[test]
    fn search_respects_limit() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = seed_store(&mut store);
        let cmds: Vec<String> = (0..20).map(|i| format!("cargo cmd_{i}")).collect();
        let cmd_refs: Vec<&str> = cmds.iter().map(String::as_str).collect();
        insert_commands(&mut store, rev_id, &cmd_refs);

        let mut registry = HandleRegistry::new(64);
        let search = SearchV1 {
            op_id: "s1".to_string(),
            terms: vec!["cargo".to_string()],
            match_mode: MatchMode::Any,
            limit: 5,
        };

        let hid = execute_search(
            &store,
            &search,
            &mut registry,
            &Budget::start(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(registry.get(hid).unwrap().count, 5);
    }

    #[test]
    fn expand_gets_neighbors() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = seed_store(&mut store);
        let cmds: Vec<String> = (0..50)
            .map(|i| {
                if i == 25 {
                    "certbot renew".to_string()
                } else {
                    format!("cmd_{i}")
                }
            })
            .collect();
        let cmd_refs: Vec<&str> = cmds.iter().map(String::as_str).collect();
        insert_commands(&mut store, rev_id, &cmd_refs);

        let mut registry = HandleRegistry::new(64);

        // Search for certbot first.
        let search = SearchV1 {
            op_id: "s1".to_string(),
            terms: vec!["certbot".to_string()],
            match_mode: MatchMode::Any,
            limit: 10,
        };
        let s_hid = execute_search(
            &store,
            &search,
            &mut registry,
            &Budget::start(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(registry.get(s_hid).unwrap().count, 1);

        // Expand around it.
        let expand = ExpandV1 {
            op_id: "e1".to_string(),
            input: InputRefV1::Output("s1".to_string()),
            scope: ExpandScope::SameSource,
            before: 3,
            after: 3,
            limit: 100,
        };
        let mut plan_outputs = HashMap::new();
        plan_outputs.insert("s1", s_hid);

        let e_hid = execute_expand(
            &store,
            &expand,
            &mut registry,
            &plan_outputs,
            &Arc::default(),
        )
        .unwrap();
        let obs = registry.get_observations(e_hid).unwrap();

        // Should have ordinals 22..=28 (7 items: 3 before + center + 3 after)
        assert_eq!(obs.len(), 7);
        let ordinals: Vec<i64> = obs.iter().map(|o| o.ordinal).collect();
        assert_eq!(ordinals, vec![22, 23, 24, 25, 26, 27, 28]);
    }

    #[test]
    fn expand_deduplicates_across_seeds() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = seed_store(&mut store);
        // Two adjacent certbot commands — their expand windows overlap.
        let cmds: Vec<String> = (0..20)
            .map(|i| {
                if i == 10 || i == 11 {
                    "certbot renew".to_string()
                } else {
                    format!("cmd_{i}")
                }
            })
            .collect();
        let cmd_refs: Vec<&str> = cmds.iter().map(String::as_str).collect();
        insert_commands(&mut store, rev_id, &cmd_refs);

        let mut registry = HandleRegistry::new(64);
        let search = SearchV1 {
            op_id: "s1".to_string(),
            terms: vec!["certbot".to_string()],
            match_mode: MatchMode::Any,
            limit: 10,
        };
        let s_hid = execute_search(
            &store,
            &search,
            &mut registry,
            &Budget::start(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(registry.get(s_hid).unwrap().count, 2);

        let expand = ExpandV1 {
            op_id: "e1".to_string(),
            input: InputRefV1::Output("s1".to_string()),
            scope: ExpandScope::SameSource,
            before: 2,
            after: 2,
            limit: 100,
        };
        let mut plan_outputs = HashMap::new();
        plan_outputs.insert("s1", s_hid);

        let e_hid = execute_expand(
            &store,
            &expand,
            &mut registry,
            &plan_outputs,
            &Arc::default(),
        )
        .unwrap();
        let obs = registry.get_observations(e_hid).unwrap();

        // Ordinals 8..=13 — no duplicates despite overlapping windows.
        let ordinals: Vec<i64> = obs.iter().map(|o| o.ordinal).collect();
        let unique: std::collections::HashSet<i64> = ordinals.iter().copied().collect();
        assert_eq!(ordinals.len(), unique.len(), "no duplicates");
    }

    #[test]
    fn a_too_broad_search_is_an_empty_set_and_a_note() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = seed_store(&mut store);
        let cmds: Vec<String> = (0..2_000).map(|i| format!("cargo cmd_{i}")).collect();
        insert_commands(
            &mut store,
            rev_id,
            &cmds.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let mut registry = HandleRegistry::new(64);
        let search = SearchV1 {
            op_id: "s1".into(),
            terms: vec!["cargo".into()],
            match_mode: MatchMode::Any,
            limit: 50,
        };

        let spent = Budget {
            deadline: std::time::Instant::now(),
            cancel: None,
        };
        let mut notes = Vec::new();
        let hid = execute_search(&store, &search, &mut registry, &spent, &mut notes).unwrap();
        assert_eq!(registry.get(hid).unwrap().count, 0);
        assert!(
            notes.len() == 1 && notes[0].starts_with("s1: query too broad"),
            "{notes:?}"
        );

        // A plan built on it runs on: expanding nothing finds nothing.
        let expand = ExpandV1 {
            op_id: "e1".into(),
            input: InputRefV1::Handle(hid.to_string()),
            scope: ExpandScope::SameSource,
            before: 2,
            after: 2,
            limit: 10,
        };
        let e_hid = execute_expand(
            &store,
            &expand,
            &mut registry,
            &HashMap::new(),
            &Arc::default(),
        )
        .unwrap();
        assert_eq!(registry.get(e_hid).unwrap().count, 0);
    }

    // --- Batching ---

    fn item(i: usize, text: String) -> EvidenceItem {
        EvidenceItem {
            item_id: format!("item_{i}"),
            evidence_id: EvidenceId::parse(&format!("ev1_{i:016x}")).unwrap(),
            safe_text: text,
            truncation: None,
        }
    }

    #[test]
    fn packets_hold_at_most_four_items_and_24_kib_in_order() {
        let sizes = [10, 10, 9000, 9000, 9000, 10, 30_000, 10, 10, 10, 10, 10];
        let items = || -> Vec<_> {
            sizes
                .iter()
                .enumerate()
                .map(|(i, &n)| item(i, "x".repeat(n)))
                .collect()
        };
        let packets = packets("objective", items());

        // Worked by hand: a 5th item or a 3rd 9000-byte one starts a packet,
        // and the 30 000-byte item fills one alone.
        let counts: Vec<usize> = packets.iter().map(|p| p.items.len()).collect();
        assert_eq!(counts, [4, 2, 1, 4, 1]);
        for p in &packets {
            assert!(
                packet_bytes("objective", p) <= egress::CHILD_MAX,
                "{} is over",
                p.packet_id
            );
        }
        let sent: Vec<&EvidenceItem> = packets.iter().flat_map(|p| &p.items).collect();
        for (sent, original) in sent.iter().zip(&items()) {
            assert_eq!(sent.item_id, original.item_id);
            if sent.item_id != "item_6" {
                assert_eq!(
                    (&sent.safe_text, &sent.truncation),
                    (&original.safe_text, &None)
                );
            }
        }
        let ids: std::collections::HashSet<_> = packets.iter().map(|p| &p.packet_id).collect();
        assert_eq!(ids.len(), packets.len(), "packet ids are distinct");
    }

    #[test]
    fn an_item_over_the_cap_is_sent_cut_and_marked() {
        // Two-byte chars, so a cut at an odd byte would split one.
        let text = "é".repeat(15 * 1024);
        let packets = packets("objective", vec![item(0, text.clone())]);
        let [packet] = packets.as_slice() else {
            panic!("one packet, got {}", packets.len())
        };
        let sent = &packet.items[0];
        let t = sent.truncation.as_ref().expect("marked as truncated");
        assert_eq!(t.original_bytes, 30 * 1024);
        assert_eq!(t.sent_bytes, sent.safe_text.len());
        assert!(text.starts_with(&sent.safe_text));
        // As much as fits: within one char of the cap, not cut to nothing.
        let bytes = packet_bytes("objective", packet);
        assert!(
            bytes <= egress::CHILD_MAX && bytes > egress::CHILD_MAX - 2,
            "{bytes} bytes"
        );
    }

    // --- Assess tests ---

    #[test]
    fn assess_batches_child_calls() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = seed_store(&mut store);
        let cmds: Vec<String> = (0..8).map(|i| format!("cargo task_{i}")).collect();
        let cmd_refs: Vec<&str> = cmds.iter().map(String::as_str).collect();
        insert_commands(&mut store, rev_id, &cmd_refs);

        let mut registry = HandleRegistry::new(64);

        // Search to get 8 items.
        let search = SearchV1 {
            op_id: "s1".to_string(),
            terms: vec!["cargo".to_string()],
            match_mode: MatchMode::Any,
            limit: 50,
        };
        let s_hid = execute_search(
            &store,
            &search,
            &mut registry,
            &Budget::start(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(registry.get(s_hid).unwrap().count, 8);

        // The host batches 4 small items per call: 2 calls.
        let model = FakeModel::new();
        model.add_child_response(FakeModel::default_supports_handler());
        model.add_child_response(FakeModel::default_supports_handler());

        let assess = AssessV1 {
            op_id: "a1".to_string(),
            input: InputRefV1::Output("s1".to_string()),
            objective: "find build tasks".to_string(),
            max_items: 8,
        };

        let mut plan_outputs = HashMap::new();
        plan_outputs.insert("s1", s_hid);

        let mut child_calls = 0;
        let a_hid = execute_assess(
            &assess,
            &mut registry,
            &plan_outputs,
            &mut |p, o| {
                child_calls += p.len();
                fake_children(&model, p, o)
            },
            &mut Vec::new(),
        )
        .unwrap();

        // 2 child calls made (8 / 4).
        assert_eq!(child_calls, 2);

        // Result is an assessment set.
        let handle = registry.get(a_hid).unwrap();
        assert_eq!(handle.kind, HandleKind::AssessmentSet);
        assert_eq!(handle.count, 8);
    }

    #[test]
    fn child_result_rejected_whole_on_any_fault() {
        let packet = EvidencePacket {
            packet_id: "pkt_00".into(),
            items: vec![item(0, "a".into()), item(1, "b".into())],
        };
        let (ev0, ev1) = (packet.items[0].evidence_id, packet.items[1].evidence_id);
        let good = serde_json::json!({"version": 1, "packet_id": "pkt_00", "assessments": [
            {"item_id": "item_0", "verdict": "supports", "confidence": "high",
             "claims": [{"text": "t", "supports": [ev0], "contradicts": [ev1]}]},
            {"item_id": "item_1", "verdict": "irrelevant", "confidence": "low", "claims": []},
        ]});
        let with = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut v = good.clone();
            edit(&mut v);
            check_child_result(&packet, &v.to_string())
        };

        let verdicts = |r: Result<Vec<Option<ItemAssessmentV1>>, _>| {
            r.unwrap()
                .into_iter()
                .map(|a| a.map(|a| a.verdict))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            verdicts(with(&|_| {})),
            [Some(Verdict::Supports), Some(Verdict::Irrelevant)]
        );
        let omitted = with(&|v| drop(v["assessments"].as_array_mut().unwrap().pop()));
        assert_eq!(
            verdicts(omitted),
            [Some(Verdict::Supports), None],
            "a skipped item is unassessed"
        );

        let foreign = item(9, String::new()).evidence_id;
        assert_eq!(
            with(&|v| v["packet_id"] = "pkt_01".into()),
            Err(ChildRejection::PacketId("pkt_01".into()))
        );
        assert_eq!(
            with(&|v| v["version"] = 2.into()),
            Err(ChildRejection::Version(2))
        );
        assert_eq!(
            with(&|v| v["assessments"][1]["item_id"] = "item_9".into()),
            Err(ChildRejection::UnknownItem("item_9".into()))
        );
        assert_eq!(
            with(&|v| v["assessments"][1]["item_id"] = "item_0".into()),
            Err(ChildRejection::DuplicateItem("item_0".into()))
        );
        let cited = |field: &'static str| {
            with(&|v| v["assessments"][0]["claims"][0][field] = serde_json::json!([foreign]))
        };
        let outside = Err(ChildRejection::Citation {
            item_id: "item_0".into(),
            evidence_id: foreign.to_string(),
        });
        assert_eq!(cited("supports"), outside);
        assert_eq!(cited("contradicts"), outside);
        assert!(matches!(
            with(&|v| v["assessments"][1]["note"] = "x".into()),
            Err(ChildRejection::Malformed(e)) if e.contains("unknown field")
        ));
        // A valid result padded past the cap: refused for its size alone.
        let padded = format!("{good}{}", " ".repeat(CHILD_RESULT_MAX));
        assert_eq!(
            check_child_result(&packet, &padded),
            Err(ChildRejection::Oversize {
                bytes: padded.len()
            })
        );
    }

    #[test]
    fn a_rejected_or_failed_child_call_leaves_only_its_packet_unassessed() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = seed_store(&mut store);
        let cmds: Vec<String> = (0..12).map(|i| format!("cargo task_{i}")).collect();
        insert_commands(
            &mut store,
            rev_id,
            &cmds.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let mut registry = HandleRegistry::new(64);
        let search = SearchV1 {
            op_id: "s1".into(),
            terms: vec!["cargo".into()],
            match_mode: MatchMode::Any,
            limit: 50,
        };
        let s_hid = execute_search(
            &store,
            &search,
            &mut registry,
            &Budget::start(),
            &mut Vec::new(),
        )
        .unwrap();
        // Packet 1's evidence, which packet 0's child was never sent.
        let other_packet = registry.get_evidence_handles(s_hid).unwrap()[4]
            .id
            .to_string();

        let model = FakeModel::new();
        let honest = FakeModel::default_supports_handler();
        model.add_child_response(move |p| {
            let mut r = honest(p);
            r.assessments[0].claims[0].supports.push(other_packet);
            r
        });
        model.add_child_response(FakeModel::default_supports_handler());
        model.add_child_response(FakeModel::default_supports_handler());

        let assess = AssessV1 {
            op_id: "a1".into(),
            input: InputRefV1::Handle(s_hid.to_string()),
            objective: "o".into(),
            max_items: 12,
        };
        let mut notes = Vec::new();
        // The call for packet 1 fails outright, as an HTTP 500 would.
        let mut call = |p: &[EvidencePacket], o: &str| {
            let mut replies = fake_children(&model, p, o)?;
            replies[1] = Err("HTTP 500".into());
            Ok(replies)
        };
        let a_hid = execute_assess(
            &assess,
            &mut registry,
            &HashMap::new(),
            &mut call,
            &mut notes,
        )
        .unwrap();

        let verdicts: Vec<_> = registry
            .get_assessments(a_hid)
            .unwrap()
            .iter()
            .map(|a| a.as_ref().map(|a| a.verdict))
            .collect();
        assert_eq!(
            verdicts,
            [vec![None; 8], vec![Some(Verdict::Supports); 4]].concat()
        );
        assert_eq!(notes.len(), 2);
        assert!(
            notes[0].contains("pkt_00") && notes[0].contains("not in the packet"),
            "{}",
            notes[0]
        );
        assert!(
            notes[1].contains("pkt_01") && notes[1].contains("HTTP 500"),
            "{}",
            notes[1]
        );
    }
}
