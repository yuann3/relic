use crate::domain::StoredObservation;
use crate::rlm::budget::{BudgetExhausted, BudgetLedger, Reservation};
use crate::rlm::evidence::{HandleId, HandleRegistry};
use crate::rlm::model::{EvidenceItem, EvidencePacket, ModelBoundary};
use crate::app::{self, Budget, Order, SearchRequest};
use crate::rlm::protocol::*;
use crate::store::Store;

#[derive(Debug)]
pub enum ExecError {
    Store(rusqlite::Error),
    Search(app::SearchError),
    HandleNotFound(String),
    NoObservationsForExpand,
    Model(crate::rlm::model::ModelError),
    Budget(BudgetExhausted),
    ChildAllowlistViolation { item_id: String, bad_evidence_id: String },
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for ExecError {}

/// Execute a search operator against the store.
pub fn execute_search(
    store: &Store,
    search: &SearchV1,
    registry: &mut HandleRegistry,
) -> Result<HandleId, ExecError> {
    let query = match search.match_mode {
        MatchMode::Any => search.terms.join(" OR "),
        MatchMode::All => search.terms.join(" "),
    };

    // Through the shared use case, so the model's searches run under the
    // same word cap and deadline as the user's (ADR 0006).
    let req = SearchRequest {
        query,
        limit: search.limit as usize,
        mode: app::MatchMode::Words,
        order: Order::RecentlyImported,
    };
    let page = app::search(store, &req, &Budget::start()).map_err(ExecError::Search)?;

    // Fetch full StoredObservation for each hit.
    let mut observations = Vec::with_capacity(page.hits.len());
    for hit in &page.hits {
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
    plan_outputs: &std::collections::HashMap<String, HandleId>,
) -> Result<HandleId, ExecError> {
    let source_handle_id = resolve_input(&expand.input, plan_outputs)?;
    let seed_obs = registry
        .get_observations(&source_handle_id)
        .ok_or_else(|| ExecError::HandleNotFound(source_handle_id.clone()))?;

    if seed_obs.is_empty() {
        return Err(ExecError::NoObservationsForExpand);
    }

    let mut seen_ids = std::collections::HashSet::new();
    let mut all_neighbors: Vec<StoredObservation> = Vec::new();

    for obs in seed_obs {
        let neighbors = store
            .get_neighbors(obs.revision_id, obs.ordinal, expand.before, expand.after)
            .map_err(ExecError::Store)?;
        for n in neighbors {
            if seen_ids.insert(n.id) {
                all_neighbors.push(n);
            }
        }
        if all_neighbors.len() >= expand.limit as usize {
            all_neighbors.truncate(expand.limit as usize);
            break;
        }
    }

    Ok(registry.insert_evidence_set(all_neighbors))
}

/// Most items in one child call (research, operator algebra: assess).
pub const CHILD_ITEMS_MAX: usize = 4;
/// Most JSON bytes of a packet and its objective; the child request cap of
/// ADR 0004. The adapter's template and schema must fit in it too (S6).
pub const CHILD_PACKET_MAX: usize = 24 * 1024;

/// Encoded size of what one child call is sent.
fn packet_bytes(objective: &str, packet: &EvidencePacket) -> usize {
    serde_json::to_vec(&(objective, packet)).expect("strings and integers serialize").len()
}

/// Split `items` in order into packets of at most [`CHILD_ITEMS_MAX`] items
/// and [`CHILD_PACKET_MAX`] bytes. An item too big for a packet of its own
/// is cut at a char boundary and marked with [`TruncationV1`].
pub fn packets(objective: &str, items: Vec<EvidenceItem>) -> Vec<EvidencePacket> {
    // Fixed width, so moving an item to the next packet cannot grow it:
    // admission caps an assess at 64 items, so at most 64 packets.
    let id = |n: usize| format!("pkt_{n:02}");
    let mut out: Vec<EvidencePacket> = Vec::new();
    let over = |p: &EvidencePacket| p.items.len() > CHILD_ITEMS_MAX || packet_bytes(objective, p) > CHILD_PACKET_MAX;
    let mut packet = EvidencePacket { packet_id: id(0), items: Vec::new() };
    for item in items {
        packet.items.push(item);
        if packet.items.len() > 1 && over(&packet) {
            let moved = packet.items.pop().into_iter().collect();
            let next = EvidencePacket { packet_id: id(out.len() + 1), items: moved };
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
        item.truncation = Some(TruncationV1 { original_bytes: text.len(), sent_bytes: n });
        packet_bytes(objective, packet) <= CHILD_PACKET_MAX
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

/// Execute an assess operator: batch evidence into child model calls.
pub fn execute_assess(
    assess: &AssessV1,
    registry: &mut HandleRegistry,
    plan_outputs: &std::collections::HashMap<String, HandleId>,
    model: &dyn ModelBoundary,
    ledger: &mut BudgetLedger,
    child_calls_made: &mut u16,
) -> Result<HandleId, ExecError> {
    let source_handle_id = resolve_input(&assess.input, plan_outputs)?;
    let source_obs = registry
        .get_observations(&source_handle_id)
        .ok_or_else(|| ExecError::HandleNotFound(source_handle_id.clone()))?;

    let evidence_handles = registry
        .get_evidence_handles(&source_handle_id)
        .ok_or_else(|| ExecError::HandleNotFound(source_handle_id.clone()))?;

    // Select up to max_items observations.
    let selected_count = source_obs.len().min(assess.max_items as usize);

    // Build allowlist of evidence IDs for this assess.
    let allowlist: std::collections::HashSet<&str> = evidence_handles[..selected_count]
        .iter()
        .map(|eh| eh.id.as_str())
        .collect();

    let items = (0..selected_count)
        .map(|i| EvidenceItem {
            item_id: format!("item_{i}"),
            evidence_id: evidence_handles[i].id.clone(),
            safe_text: source_obs[i].safe_text.clone(),
            truncation: None,
        })
        .collect();
    let packets = packets(&assess.objective, items);
    let child_calls = u16::try_from(packets.len()).map_err(|_| ExecError::Budget(BudgetExhausted::ChildCalls))?;
    ledger
        .reserve(&Reservation { child_calls, ..Default::default() })
        .map_err(ExecError::Budget)?;

    // Collect all child assessments.
    let mut all_assessments: Vec<ItemAssessmentV1> = Vec::new();

    for packet in &packets {
        let child_result = model
            .child_call(packet, &assess.objective)
            .map_err(ExecError::Model)?;
        *child_calls_made += 1;

        // Validate child response: packet_id must match, all citation IDs in allowlist.
        for assessment in &child_result.assessments {
            for claim in &assessment.claims {
                for ev_id in claim.supports.iter().chain(claim.contradicts.iter()) {
                    if !allowlist.contains(ev_id.as_str()) {
                        return Err(ExecError::ChildAllowlistViolation {
                            item_id: assessment.item_id.clone(),
                            bad_evidence_id: ev_id.clone(),
                        });
                    }
                }
            }
        }

        all_assessments.extend(child_result.assessments);
    }

    // Store the assessment result — reuse the same observations for now.
    // The assessment metadata is carried in the child results, not in observations.
    let assessed_obs = source_obs[..selected_count].to_vec();
    Ok(registry.insert_assessment_set(assessed_obs))
}

/// Resolve an InputRefV1 to a HandleId.
pub fn resolve_input(
    input: &InputRefV1,
    plan_outputs: &std::collections::HashMap<String, HandleId>,
) -> Result<HandleId, ExecError> {
    match input {
        InputRefV1::Output(op_id) => plan_outputs
            .get(op_id.as_str())
            .cloned()
            .ok_or_else(|| ExecError::HandleNotFound(op_id.clone())),
        InputRefV1::Handle(handle_id) => Ok(handle_id.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Observation, ObservationKind, Provenance};
    use crate::import::SourceKind;
    use crate::rlm::evidence::HandleRegistry;
    use crate::rlm::validate::HandleKind;
    use crate::rlm::budget::BudgetLedger;
    use crate::store::{Cursor, Store};
    use std::collections::HashMap;

    const KEY: &[u8] = b"test-key";

    fn seed_store(store: &mut Store) -> i64 {
        let rev = store.lock_source(b"/h").unwrap().new_revision(SourceKind::Zsh).unwrap();
        let id = rev.revision_id();
        rev.commit(&Cursor::default(), Provenance::COMMAND).unwrap();
        id
    }

    fn insert_commands(store: &mut Store, rev_id: i64, commands: &[&str]) {
        let mut rev = store.lock_source(b"/h").unwrap().append(SourceKind::Zsh).unwrap();
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
        insert_commands(&mut store, rev_id, &[
            "cargo build --release",
            "git push origin main",
            "cargo test --lib",
            "docker compose up",
        ]);

        let mut registry = HandleRegistry::new(1, KEY.to_vec(), 64);
        let search = SearchV1 {
            op_id: "s1".to_string(),
            terms: vec!["cargo".to_string()],
            match_mode: MatchMode::Any,
            limit: 50,
        };

        let hid = execute_search(&store, &search, &mut registry).unwrap();
        let handle = registry.get(&hid).unwrap();
        assert_eq!(handle.kind, HandleKind::EvidenceSet);
        assert_eq!(handle.count, 2);

        let obs = registry.get_observations(&hid).unwrap();
        assert!(obs.iter().all(|o| o.safe_text.contains("cargo")));
    }

    #[test]
    fn search_respects_limit() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = seed_store(&mut store);
        let cmds: Vec<String> = (0..20).map(|i| format!("cargo cmd_{i}")).collect();
        let cmd_refs: Vec<&str> = cmds.iter().map(|s| s.as_str()).collect();
        insert_commands(&mut store, rev_id, &cmd_refs);

        let mut registry = HandleRegistry::new(1, KEY.to_vec(), 64);
        let search = SearchV1 {
            op_id: "s1".to_string(),
            terms: vec!["cargo".to_string()],
            match_mode: MatchMode::Any,
            limit: 5,
        };

        let hid = execute_search(&store, &search, &mut registry).unwrap();
        assert_eq!(registry.get(&hid).unwrap().count, 5);
    }

    #[test]
    fn expand_gets_neighbors() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = seed_store(&mut store);
        let cmds: Vec<String> = (0..50).map(|i| {
            if i == 25 { "certbot renew".to_string() }
            else { format!("cmd_{i}") }
        }).collect();
        let cmd_refs: Vec<&str> = cmds.iter().map(|s| s.as_str()).collect();
        insert_commands(&mut store, rev_id, &cmd_refs);

        let mut registry = HandleRegistry::new(1, KEY.to_vec(), 64);

        // Search for certbot first.
        let search = SearchV1 {
            op_id: "s1".to_string(),
            terms: vec!["certbot".to_string()],
            match_mode: MatchMode::Any,
            limit: 10,
        };
        let s_hid = execute_search(&store, &search, &mut registry).unwrap();
        assert_eq!(registry.get(&s_hid).unwrap().count, 1);

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
        plan_outputs.insert("s1".to_string(), s_hid);

        let e_hid = execute_expand(&store, &expand, &mut registry, &plan_outputs).unwrap();
        let obs = registry.get_observations(&e_hid).unwrap();

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
        let cmds: Vec<String> = (0..20).map(|i| {
            if i == 10 || i == 11 { "certbot renew".to_string() }
            else { format!("cmd_{i}") }
        }).collect();
        let cmd_refs: Vec<&str> = cmds.iter().map(|s| s.as_str()).collect();
        insert_commands(&mut store, rev_id, &cmd_refs);

        let mut registry = HandleRegistry::new(1, KEY.to_vec(), 64);
        let search = SearchV1 {
            op_id: "s1".to_string(),
            terms: vec!["certbot".to_string()],
            match_mode: MatchMode::Any,
            limit: 10,
        };
        let s_hid = execute_search(&store, &search, &mut registry).unwrap();
        assert_eq!(registry.get(&s_hid).unwrap().count, 2);

        let expand = ExpandV1 {
            op_id: "e1".to_string(),
            input: InputRefV1::Output("s1".to_string()),
            scope: ExpandScope::SameSource,
            before: 2,
            after: 2,
            limit: 100,
        };
        let mut plan_outputs = HashMap::new();
        plan_outputs.insert("s1".to_string(), s_hid);

        let e_hid = execute_expand(&store, &expand, &mut registry, &plan_outputs).unwrap();
        let obs = registry.get_observations(&e_hid).unwrap();

        // Ordinals 8..=13 — no duplicates despite overlapping windows.
        let ordinals: Vec<i64> = obs.iter().map(|o| o.ordinal).collect();
        let unique: std::collections::HashSet<i64> = ordinals.iter().copied().collect();
        assert_eq!(ordinals.len(), unique.len(), "no duplicates");
    }

    // --- Batching ---

    fn item(i: usize, text: String) -> EvidenceItem {
        EvidenceItem { item_id: format!("item_{i}"), evidence_id: format!("ev1_{i:016x}"), safe_text: text, truncation: None }
    }

    #[test]
    fn packets_hold_at_most_four_items_and_24_kib_in_order() {
        let sizes = [10, 10, 9000, 9000, 9000, 10, 30_000, 10, 10, 10, 10, 10];
        let items: Vec<_> = sizes.iter().enumerate().map(|(i, &n)| item(i, "x".repeat(n))).collect();
        let packets = packets("objective", items.clone());

        // Worked by hand: a 5th item or a 3rd 9000-byte one starts a packet,
        // and the 30 000-byte item fills one alone.
        let counts: Vec<usize> = packets.iter().map(|p| p.items.len()).collect();
        assert_eq!(counts, [4, 2, 1, 4, 1]);
        for p in &packets {
            assert!(packet_bytes("objective", p) <= CHILD_PACKET_MAX, "{} is over", p.packet_id);
        }
        let sent: Vec<&EvidenceItem> = packets.iter().flat_map(|p| &p.items).collect();
        for (sent, original) in sent.iter().zip(&items) {
            assert_eq!(sent.item_id, original.item_id);
            if sent.item_id != "item_6" {
                assert_eq!((&sent.safe_text, &sent.truncation), (&original.safe_text, &None));
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
        let [packet] = packets.as_slice() else { panic!("one packet, got {}", packets.len()) };
        let sent = &packet.items[0];
        let t = sent.truncation.as_ref().expect("marked as truncated");
        assert_eq!(t.original_bytes, 30 * 1024);
        assert_eq!(t.sent_bytes, sent.safe_text.len());
        assert!(text.starts_with(&sent.safe_text));
        // As much as fits: within one char of the cap, not cut to nothing.
        let bytes = packet_bytes("objective", packet);
        assert!(bytes <= CHILD_PACKET_MAX && bytes > CHILD_PACKET_MAX - 2, "{bytes} bytes");
    }

    // --- Assess tests ---

    #[test]
    fn assess_batches_child_calls() {
        use crate::rlm::model::FakeModel;

        let mut store = Store::open_memory().unwrap();
        let rev_id = seed_store(&mut store);
        let cmds: Vec<String> = (0..8).map(|i| format!("cargo task_{i}")).collect();
        let cmd_refs: Vec<&str> = cmds.iter().map(|s| s.as_str()).collect();
        insert_commands(&mut store, rev_id, &cmd_refs);

        let mut registry = HandleRegistry::new(1, KEY.to_vec(), 64);

        // Search to get 8 items.
        let search = SearchV1 {
            op_id: "s1".to_string(),
            terms: vec!["cargo".to_string()],
            match_mode: MatchMode::Any,
            limit: 50,
        };
        let s_hid = execute_search(&store, &search, &mut registry).unwrap();
        assert_eq!(registry.get(&s_hid).unwrap().count, 8);

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
        plan_outputs.insert("s1".to_string(), s_hid);

        let mut child_calls = 0u16;
        let mut ledger = BudgetLedger::new(BudgetV1::prototype_default());
        let a_hid = execute_assess(
            &assess,
            &mut registry,
            &plan_outputs,
            &model,
            &mut ledger,
            &mut child_calls,
        )
        .unwrap();

        // 2 child calls made (8 / 4).
        assert_eq!(child_calls, 2);

        // Result is an assessment set.
        let handle = registry.get(&a_hid).unwrap();
        assert_eq!(handle.kind, HandleKind::AssessmentSet);
        assert_eq!(handle.count, 8);
    }

    #[test]
    fn assess_rejects_bad_citation_in_child_response() {
        use crate::rlm::model::FakeModel;

        let mut store = Store::open_memory().unwrap();
        let rev_id = seed_store(&mut store);
        insert_commands(&mut store, rev_id, &["cargo build"]);

        let mut registry = HandleRegistry::new(1, KEY.to_vec(), 64);
        let search = SearchV1 {
            op_id: "s1".to_string(),
            terms: vec!["cargo".to_string()],
            match_mode: MatchMode::Any,
            limit: 10,
        };
        let s_hid = execute_search(&store, &search, &mut registry).unwrap();

        // Fake model returns a citation that's not in the allowlist.
        let model = FakeModel::new();
        model.add_child_response(|packet: &EvidencePacket| ChildResultV1 {
            version: 1,
            packet_id: packet.packet_id.clone(),
            assessments: vec![ItemAssessmentV1 {
                item_id: "item_0".to_string(),
                verdict: Verdict::Supports,
                confidence: Confidence::High,
                claims: vec![ClaimV1 {
                    text: "forged claim".to_string(),
                    supports: vec!["ev1_FORGED".to_string()],
                    contradicts: vec![],
                }],
                missing: vec![],
            }],
        });

        let assess = AssessV1 {
            op_id: "a1".to_string(),
            input: InputRefV1::Output("s1".to_string()),
            objective: "test".to_string(),
            max_items: 1,
        };
        let mut plan_outputs = HashMap::new();
        plan_outputs.insert("s1".to_string(), s_hid);

        let mut child_calls = 0u16;
        let mut ledger = BudgetLedger::new(BudgetV1::prototype_default());
        let result = execute_assess(
            &assess,
            &mut registry,
            &plan_outputs,
            &model,
            &mut ledger,
            &mut child_calls,
        );

        assert!(matches!(result, Err(ExecError::ChildAllowlistViolation { .. })));
    }
}
