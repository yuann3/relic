use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::domain::StoredObservation;
use crate::rlm::budget::{BudgetExhausted, BudgetLedger, Reservation};
use crate::rlm::evidence::{EvidenceId, HandleId, HandleRegistry};
use crate::rlm::executor::{self, ExecError};
use crate::rlm::finish;
use crate::rlm::calls::{Calls, Job, Stop, call_all};
use crate::rlm::model::egress::{PayloadAudit, Role};
use crate::rlm::model::{EvidenceItem, EvidencePacket, ModelBoundary, ModelError, parse_output};
use crate::rlm::protocol::*;
use crate::rlm::validate::{self, HandleKind, Op, ValidationError};
use crate::store::Store;

/// Terminal result of an investigation.
#[derive(Debug)]
pub struct InvestigationResult {
    pub status: InvestigationStatus,
    /// Past the finish gate: every citation in it resolves.
    pub finish: Option<FinishV1>,
    /// The observation behind each evidence id `finish` cites.
    pub cited: HashMap<EvidenceId, StoredObservation>,
    pub turns: u32,
    pub child_calls: u16,
}

#[derive(Debug, PartialEq)]
pub enum InvestigationStatus {
    Completed,
    Partial,
    Abstained,
    BudgetExhausted,
    Cancelled,
    Failed(String),
}

/// What an investigation is doing, for a progress display; the UI maps it
/// to its own events (ADR 0009).
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    RootCall { turn: u32 },
    Op { op_id: String },
    ChildCall { op_id: String, packet_id: String },
    /// A note to the root: a rejected child result or a too-broad search.
    Note(String),
    /// A request is about to leave: what, where and how big (ADR 0004).
    Egress(PayloadAudit),
    /// A model call came back; `returned_model` is what actually answered.
    Replied { role: Role, returned_model: Option<String> },
    /// A failed call is sent again, as the retry rule allows once.
    Retry { role: Role, attempt: u8, reason: String },
}

/// One line for a person watching: the CLI's stderr, the TUI's steps.
impl std::fmt::Display for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let role = |r: &Role| match r {
            Role::Root => "root",
            Role::Child => "child",
        };
        match self {
            Self::RootCall { turn } => write!(f, "root call {turn}: plan the next step"),
            Self::Op { op_id } => write!(f, "run {op_id}"),
            Self::ChildCall { op_id, packet_id } => write!(f, "child call for {op_id}, {packet_id}"),
            Self::Note(note) => write!(f, "note: {note}"),
            Self::Egress(a) => write!(
                f,
                "send a {} request of {} bytes to {} ({}), request {}",
                role(&a.role),
                a.bytes,
                a.origin,
                a.profile,
                a.request_hash
            ),
            Self::Replied { role: r, returned_model } => {
                write!(f, "{} reply from {}", role(r), returned_model.as_deref().unwrap_or("an unnamed model"))
            }
            Self::Retry { role: r, attempt, reason } => write!(f, "retry the {} call (attempt {attempt}): {reason}", role(r)),
        }
    }
}

#[derive(Debug)]
pub enum CoordinatorError {
    Validation(ValidationError),
    Execution(ExecError),
    Model(ModelError),
    Budget(BudgetExhausted),
    InvalidAction(String),
}

impl std::fmt::Display for CoordinatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for CoordinatorError {}

/// What one investigation's steps share.
struct Run<'a> {
    store: &'a Store,
    model: Arc<dyn ModelBoundary>,
    registry: HandleRegistry,
    ledger: BudgetLedger,
    turns: u32,
    child_calls: u16,
    last_inspection: Option<EvidencePacket>,
    /// What the host tells the root about the last plan, besides its handles.
    notes: Vec<String>,
    cancel: &'a Arc<AtomicBool>,
    progress: &'a mut dyn FnMut(Progress),
    /// When `wall_ms` runs out; every model call gives up by then.
    deadline: Instant,
    /// Most child calls in flight at once.
    concurrency: usize,
}

impl Run<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn calls(&mut self) -> Calls<'_> {
        Calls {
            model: &self.model,
            ledger: &mut self.ledger,
            cancel: self.cancel,
            progress: &mut *self.progress,
            deadline: self.deadline,
            concurrency: self.concurrency,
            child_calls: &mut self.child_calls,
        }
    }
}

/// Run a bounded RLM investigation. Setting `cancel` from any thread stops
/// it at the next admission point, or mid-statement in SQLite, with
/// [`InvestigationStatus::Cancelled`].
pub fn run_investigation(
    question: &str,
    store: &Store,
    model: Arc<dyn ModelBoundary>,
    budget: BudgetV1,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(Progress),
) -> InvestigationResult {
    let investigation_id = 1u64; // Single investigation for prototype.
    let installation_key = b"prototype-key".to_vec();

    let deadline = Instant::now() + Duration::from_millis(budget.wall_ms);
    let mut run = Run {
        store,
        model,
        deadline,
        concurrency: usize::from(budget.max_concurrency),
        registry: HandleRegistry::new(investigation_id, installation_key, budget.evidence_handles as usize),
        ledger: BudgetLedger::new(budget),
        turns: 0,
        child_calls: 0,
        last_inspection: None,
        notes: Vec::new(),
        cancel,
        progress,
    };
    let (status, finish) = investigate(&mut run, question);
    let cited = finish
        .iter()
        .flat_map(|f| {
            let claims = f.claims.iter().flat_map(|c| c.supports.iter().chain(&c.contradicts));
            claims.chain(f.procedure.iter().flat_map(|s| &s.supports))
        })
        .filter_map(|id| Some((id.clone(), run.registry.resolve_evidence_id(id)?.1.clone())))
        .collect();
    InvestigationResult { status, finish, cited, turns: run.turns, child_calls: run.child_calls }
}

fn investigate(run: &mut Run, question: &str) -> (InvestigationStatus, Option<FinishV1>) {
    let mut state_version: u32 = 0;
    loop {
        let state = build_root_state(
            question,
            state_version,
            &run.registry,
            &run.last_inspection,
            &std::mem::take(&mut run.notes),
            &run.ledger,
            run.store,
        );
        let job = Job { role: Role::Root, payload: state, admitted: Progress::RootCall { turn: run.turns + 1 } };
        let reply = match call_all(&mut run.calls(), vec![job]).map(|mut r| r.remove(0)) {
            Ok(Ok(reply)) => reply,
            Ok(Err(e)) => return (InvestigationStatus::Failed(format!("model error: {e}")), None),
            Err(Stop::Cancelled) => return (InvestigationStatus::Cancelled, None),
            Err(Stop::Budget(_)) => return (InvestigationStatus::BudgetExhausted, None),
        };
        run.turns += 1;
        let action: RootActionV1 = match parse_output(&reply.body, ROOT_RESULT_MAX) {
            Ok(action) => action,
            Err(e) => return (InvestigationStatus::Failed(format!("root action: {e:?}")), None),
        };
        // A reply that lands after cancel is dropped, not acted on: a late
        // finish must not pass for a completed investigation.
        if run.cancelled() {
            return (InvestigationStatus::Cancelled, None);
        }

        match action.kind {
            RootActionKind::Plan => {
                let validated = match validate::validate_plan(&action, state_version) {
                    Ok(v) => v,
                    Err(e) => return (InvestigationStatus::Failed(format!("validation: {e}")), None),
                };

                // Admission caps a plan at 8 ops.
                let plan_ops = u16::try_from(validated.ops.len()).unwrap_or(u16::MAX);
                if run.ledger.reserve(&Reservation { plan_ops, ..Default::default() }).is_err() {
                    return (InvestigationStatus::BudgetExhausted, None);
                }

                // Execute ops sequentially.
                let mut plan_outputs: HashMap<String, HandleId> = HashMap::new();
                for op in &validated.ops {
                    if run.cancelled() {
                        return (InvestigationStatus::Cancelled, None);
                    }
                    (run.progress)(Progress::Op { op_id: validate::op_id(op).to_string() });
                    let noted = run.notes.len();
                    let out = execute_op(run, op, &plan_outputs);
                    for note in &run.notes[noted..] {
                        (run.progress)(Progress::Note(note.clone()));
                    }
                    match out {
                        Ok(Some((op_id, handle_id))) => {
                            plan_outputs.insert(op_id, handle_id);
                        }
                        Ok(None) => {}
                        Err(CoordinatorError::Execution(ExecError::Cancelled)) => {
                            return (InvestigationStatus::Cancelled, None);
                        }
                        Err(CoordinatorError::Budget(_) | CoordinatorError::Execution(ExecError::Budget(_))) => {
                            return (InvestigationStatus::BudgetExhausted, None);
                        }
                        Err(e) => return (InvestigationStatus::Failed(format!("exec: {e}")), None),
                    }
                }

                if run.cancelled() {
                    return (InvestigationStatus::Cancelled, None);
                }
                // Observation committed — increment state version.
                state_version += 1;
            }

            RootActionKind::Finish => {
                let Some(finish) = action.finish else {
                    return (InvestigationStatus::Failed("finish action missing finish body".into()), None);
                };
                // An answer resting on a citation that does not resolve has
                // no evidence behind it: it abstains, saying why (ADR 0009).
                let finish = match finish::validate_finish(&finish, &run.registry) {
                    Ok(checked) => checked.finish,
                    Err(e) => {
                        let why = format!("the finish gate refused the answer: {e}");
                        (run.progress)(Progress::Note(why.clone()));
                        finish::force_abstention(&finish, vec![why])
                    }
                };
                let status = match finish.status {
                    AnswerStatus::Answered => InvestigationStatus::Completed,
                    AnswerStatus::Partial => InvestigationStatus::Partial,
                    AnswerStatus::Abstained => InvestigationStatus::Abstained,
                };
                return (status, Some(finish));
            }
        }
    }
}

fn execute_op(
    run: &mut Run,
    op: &Op,
    plan_outputs: &HashMap<String, HandleId>,
) -> Result<Option<(String, HandleId)>, CoordinatorError> {
    let Run { store, model, registry, ledger, child_calls, last_inspection, notes, cancel, progress, deadline, concurrency, .. } =
        run;
    match op {
        Op::Search(s) => {
            // Reserve rows.
            ledger
                .reserve(&Reservation {
                    rows_returned: s.limit as u64,
                    evidence_handles: 1,
                    ..Default::default()
                })
                .map_err(CoordinatorError::Budget)?;

            let budget = crate::app::Budget { cancel: Some(Arc::clone(cancel)), ..crate::app::Budget::start() };
            let hid = executor::execute_search(store, s, registry, &budget, notes)
                .map_err(CoordinatorError::Execution)?;

            // Release unused row reservation.
            let actual = registry.get(&hid).map(|h| h.count as u64).unwrap_or(0);
            let reserved = s.limit as u64;
            if actual < reserved {
                ledger.release_rows(reserved - actual);
            }

            Ok(Some((s.op_id.clone(), hid)))
        }

        Op::Expand(e) => {
            ledger
                .reserve(&Reservation {
                    rows_returned: e.limit as u64,
                    evidence_handles: 1,
                    ..Default::default()
                })
                .map_err(CoordinatorError::Budget)?;

            let hid = executor::execute_expand(store, e, registry, plan_outputs, cancel)
                .map_err(CoordinatorError::Execution)?;

            let actual = registry.get(&hid).map(|h| h.count as u64).unwrap_or(0);
            let reserved = e.limit as u64;
            if actual < reserved {
                ledger.release_rows(reserved - actual);
            }

            Ok(Some((e.op_id.clone(), hid)))
        }

        Op::Assess(a) => {
            ledger
                .reserve(&Reservation {
                    evidence_handles: 1,
                    ..Default::default()
                })
                .map_err(CoordinatorError::Budget)?;

            // One child call admitted at a time, so a later stop leaves the
            // rest unsent.
            let mut call_children = |packets: &[EvidencePacket], objective: &str| {
                let jobs = packets
                    .iter()
                    .map(|p| Job {
                        role: Role::Child,
                        payload: executor::child_payload(objective, p),
                        admitted: Progress::ChildCall { op_id: a.op_id.clone(), packet_id: p.packet_id.clone() },
                    })
                    .collect();
                let mut calls = Calls {
                    model,
                    ledger,
                    cancel,
                    progress: &mut **progress,
                    deadline: *deadline,
                    concurrency: *concurrency,
                    child_calls,
                };
                let replies = call_all(&mut calls, jobs).map_err(|stop| match stop {
                    Stop::Cancelled => ExecError::Cancelled,
                    Stop::Budget(b) => ExecError::Budget(b),
                })?;
                Ok(replies.into_iter().map(|r| r.map(|reply| reply.body)).collect())
            };
            let hid = executor::execute_assess(a, registry, plan_outputs, &mut call_children, notes)
                .map_err(CoordinatorError::Execution)?;

            Ok(Some((a.op_id.clone(), hid)))
        }

        Op::Inspect(i) => {
            let source_hid = executor::resolve_input(&i.input, plan_outputs)
                .map_err(CoordinatorError::Execution)?;

            let obs = registry
                .get_observations(&source_hid)
                .ok_or_else(|| {
                    CoordinatorError::Execution(ExecError::HandleNotFound(source_hid.clone()))
                })?;
            let ev_handles = registry
                .get_evidence_handles(&source_hid)
                .ok_or_else(|| {
                    CoordinatorError::Execution(ExecError::HandleNotFound(source_hid.clone()))
                })?;

            let limit = i.limit as usize;
            let count = obs.len().min(limit);

            let items: Vec<EvidenceItem> = (0..count)
                .map(|idx| {
                    let text = &obs[idx].safe_text;
                    let sent = text.floor_char_boundary(i.max_item_bytes as usize);
                    EvidenceItem {
                        item_id: format!("item_{idx}"),
                        evidence_id: ev_handles[idx].id.clone(),
                        safe_text: text[..sent].to_string(),
                        truncation: (sent < text.len())
                            .then_some(TruncationV1 { original_bytes: text.len(), sent_bytes: sent }),
                    }
                })
                .collect();

            *last_inspection = Some(EvidencePacket {
                packet_id: format!("inspect_{}", i.op_id),
                items,
            });

            Ok(None) // Inspect doesn't produce a handle — it produces a root observation.
        }

        Op::Select(k) => {
            ledger
                .reserve(&Reservation {
                    evidence_handles: 1,
                    ..Default::default()
                })
                .map_err(CoordinatorError::Budget)?;

            let hid = executor::execute_select(k, registry, plan_outputs).map_err(CoordinatorError::Execution)?;

            Ok(Some((k.op_id.clone(), hid)))
        }
    }
}

/// How many items of an assessment set got each verdict. Unassessed items
/// are counted apart, never as support (ADR 0009).
fn verdict_counts(slots: &[Option<ItemAssessmentV1>]) -> serde_json::Value {
    let count = |v: Option<Verdict>| slots.iter().filter(|s| s.as_ref().map(|a| a.verdict.clone()) == v).count();
    serde_json::json!({
        "supports": count(Some(Verdict::Supports)),
        "contradicts": count(Some(Verdict::Contradicts)),
        "unclear": count(Some(Verdict::Unclear)),
        "irrelevant": count(Some(Verdict::Irrelevant)),
        "unassessed": count(None),
    })
}

/// Most bytes of a root reply; well over 1536 output tokens of JSON.
pub const ROOT_RESULT_MAX: usize = 32 * 1024;

/// The root state the model is shown; the egress seam caps it at 64 KiB.
fn build_root_state(
    question: &str,
    state_version: u32,
    registry: &HandleRegistry,
    last_inspection: &Option<EvidencePacket>,
    notes: &[String],
    ledger: &BudgetLedger,
    store: &Store,
) -> serde_json::Value {
    let obs_count = store.observation_count().unwrap_or(0);
    let summaries = registry.summaries();
    let remaining = ledger.remaining();

    let mut state = serde_json::json!({
        "question": question,
        "state_version": state_version,
        "corpus": { "observation_count": obs_count },
        "handles": summaries.iter().map(|s| {
            let mut h = serde_json::json!({
                "id": s.id,
                "kind": format!("{:?}", s.kind),
                "count": s.count,
            });
            if let Some(slots) = registry.get_assessments(&s.id).filter(|_| s.kind == HandleKind::AssessmentSet) {
                h["verdicts"] = verdict_counts(slots);
            }
            h
        }).collect::<Vec<_>>(),
        "notes": notes,
        "budget_remaining": {
            "root_calls": remaining.root_calls,
            "child_calls": remaining.child_calls,
            "plan_ops": remaining.plan_ops,
            "rows_returned": remaining.rows_returned,
        },
    });

    if let Some(inspection) = last_inspection {
        state["last_inspection"] = serde_json::json!(inspection);
    }

    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Observation, ObservationKind, Provenance};
    use crate::import::SourceKind;
    use crate::store::Cursor;
    use crate::rlm::model::FakeModel;

    fn seed_corpus(store: &mut Store) -> i64 {
        // 50 commands: most generic, with "certbot renew" at ordinal 25
        // and "certbot status" at ordinal 26.
        let cmds: Vec<String> = (0..50)
            .map(|i| match i {
                25 => "certbot renew --nginx -d example.com".to_string(),
                26 => "certbot status".to_string(),
                _ => format!("generic cmd_{i}"),
            })
            .collect();

        let mut rev = store.lock_source(b"/h").unwrap().new_revision(SourceKind::Zsh).unwrap();
        let rev_id = rev.revision_id();
        for (i, cmd) in cmds.iter().enumerate() {
            rev.insert(&Observation {
                kind: ObservationKind::ZshCommand,
                observed_at_ms: Some(1_700_000_000_000 + i as i64),
                safe_text: cmd.clone(),
                flags: 0,
                locator: vec![],
            })
            .unwrap();
        }
        rev.commit(&Cursor::default(), Provenance::COMMAND).unwrap();
        rev_id
    }

    #[test]
    fn two_turn_investigation_end_to_end() {
        let mut store = Store::open_memory().unwrap();
        seed_corpus(&mut store);

        let model = Arc::new(FakeModel::new());

        // Turn 1: search → expand → assess → inspect.
        model.add_root_response(RootActionV1 {
            version: 1,
            expected_state_version: 0,
            kind: RootActionKind::Plan,
            plan: Some(PlanV1 {
                ops: vec![
                    PlanOpV1::Search(SearchV1 {
                        op_id: "s1".to_string(),
                        terms: vec!["certbot".to_string()],
                        match_mode: MatchMode::Any,
                        limit: 50,
                    }),
                    PlanOpV1::Expand(ExpandV1 {
                        op_id: "e1".to_string(),
                        input: InputRefV1::Output("s1".to_string()),
                        scope: ExpandScope::SameSource,
                        before: 3,
                        after: 3,
                        limit: 100,
                    }),
                    PlanOpV1::Assess(AssessV1 {
                        op_id: "a1".to_string(),
                        input: InputRefV1::Output("e1".to_string()),
                        objective: "identify certificate renewal steps".to_string(),
                        max_items: 8,
                    }),
                    PlanOpV1::Inspect(InspectV1 {
                        op_id: "i1".to_string(),
                        input: InputRefV1::Output("e1".to_string()),
                        limit: 8,
                        max_item_bytes: 2048,
                    }),
                ],
            }),
            finish: None,
        });

        // Child calls for assess: 2 batches of 4.
        model.add_child_response(FakeModel::default_supports_handler());
        model.add_child_response(FakeModel::default_supports_handler());

        // Turn 2: finish citing the inspected renewal, by the id this
        // investigation gave it.
        model.add_root_fn(|state| {
            let items = state["last_inspection"]["items"].as_array().unwrap();
            let renew = items.iter().find(|i| i["safe_text"].as_str().unwrap().contains("renew")).unwrap();
            let cite = renew["evidence_id"].clone();
            serde_json::json!({"version": 1, "expected_state_version": 1, "kind": "finish", "plan": null, "finish": {
                "status": "answered", "answer": "Certificate was renewed using certbot renew.",
                "claims": [{"text": "certbot renew was executed", "supports": [cite]}],
                "procedure": [{"text": "Run certbot renew --nginx -d example.com", "supports": [cite]}]}})
            .to_string()
        });

        let budget = BudgetV1::prototype_default();
        let result = run_investigation(
            "What certificate renewal steps were observed?",
            &store,
            model.clone(),
            budget,
            &Arc::default(),
            &mut |_| {},
        );

        // Core assertions for the wayfinder question:
        assert_eq!(result.status, InvestigationStatus::Completed);
        assert_eq!(result.turns, 2);
        assert!(result.child_calls >= 2, "at least 2 child calls for assess batches");
        assert!(result.finish.is_some());

        let finish = result.finish.unwrap();
        assert_eq!(finish.status, AnswerStatus::Answered);
        let cite = &finish.claims[0].supports[0];
        assert_eq!(result.cited[cite].safe_text, "certbot renew --nginx -d example.com");
        assert_eq!(&finish.procedure[0].supports[..], std::slice::from_ref(cite));
    }

    #[test]
    fn an_answer_citing_evidence_it_was_never_given_abstains() {
        let mut store = Store::open_memory().unwrap();
        seed_corpus(&mut store);
        let model = Arc::new(FakeModel::new());
        model.add_root_raw(serde_json::json!({"version": 1, "expected_state_version": 0, "kind": "finish", "plan": null, "finish": {
            "status": "answered", "answer": "certbot renew did it",
            "claims": [{"text": "certbot renew ran", "supports": ["ev1_00000000deadbeef"]}]}}).to_string());
        let mut notes = Vec::new();
        let result = run_investigation("q", &store, model, BudgetV1::prototype_default(), &Arc::default(), &mut |p| {
            if let Progress::Note(n) = p {
                notes.push(n)
            }
        });
        assert_eq!(result.status, InvestigationStatus::Abstained);
        let finish = result.finish.unwrap();
        assert!(finish.claims.is_empty() && finish.answer.is_empty() && result.cited.is_empty(), "{finish:?}");
        assert!(finish.missing[0].contains("ev1_00000000deadbeef") && notes == finish.missing, "{finish:?}");
    }

    /// Run search `certbot` -> assess -> select `verdicts` -> inspect with
    /// `child` judging, and return the root state the finish turn saw.
    fn select_through(
        child: impl Fn(&EvidencePacket) -> ChildResultV1 + Send + 'static,
        verdicts: Vec<Verdict>,
    ) -> serde_json::Value {
        let mut store = Store::open_memory().unwrap();
        seed_corpus(&mut store);
        let model = Arc::new(FakeModel::new());
        let op = |json: serde_json::Value| serde_json::from_value::<PlanOpV1>(json).unwrap();
        model.add_root_response(RootActionV1 {
            version: 1,
            expected_state_version: 0,
            kind: RootActionKind::Plan,
            plan: Some(PlanV1 {
                ops: vec![
                    op(serde_json::json!({"op": "search", "op_id": "s1", "terms": ["certbot"], "match": "any", "limit": 10})),
                    op(serde_json::json!({"op": "assess", "op_id": "a1", "input": {"output": "s1"}, "objective": "renewal", "max_items": 8})),
                    op(serde_json::json!({"op": "select", "op_id": "k1", "input": {"output": "a1"}, "verdicts": verdicts, "limit": 20})),
                    op(serde_json::json!({"op": "inspect", "op_id": "i1", "input": {"output": "k1"}, "limit": 8, "max_item_bytes": 2048})),
                ],
            }),
            finish: None,
        });
        model.add_child_response(child);
        model.add_root_response(RootActionV1 {
            version: 1,
            expected_state_version: 1,
            kind: RootActionKind::Finish,
            plan: None,
            finish: Some(FinishV1 {
                status: AnswerStatus::Abstained,
                answer: String::new(),
                claims: vec![],
                procedure: vec![],
                contradictions: vec![],
                missing: vec!["test".into()],
            }),
        });
        let result = run_investigation("q", &store, model.clone(), BudgetV1::prototype_default(), &Arc::default(), &mut |_| {});
        assert_eq!(result.status, InvestigationStatus::Abstained);
        serde_json::from_str(&model.root_states()[1]).unwrap()
    }

    /// Judge `certbot renew` with `renew` and `certbot status` with `status`;
    /// `None` leaves an item out of the result.
    fn judge(renew: Option<Verdict>, status: Option<Verdict>) -> impl Fn(&EvidencePacket) -> ChildResultV1 + Send {
        move |p| ChildResultV1 {
            version: 1,
            packet_id: p.packet_id.clone(),
            assessments: p
                .items
                .iter()
                .filter_map(|item| {
                    let verdict = if item.safe_text.contains("renew") { renew.clone() } else { status.clone() };
                    Some(ItemAssessmentV1 {
                        item_id: item.item_id.clone(),
                        verdict: verdict?,
                        confidence: Confidence::High,
                        claims: vec![],
                        missing: vec![],
                    })
                })
                .collect(),
        }
    }

    fn inspected(state: &serde_json::Value) -> Vec<&str> {
        state["last_inspection"]["items"].as_array().unwrap().iter().map(|i| i["safe_text"].as_str().unwrap()).collect()
    }

    #[test]
    fn select_passes_on_the_items_with_the_chosen_verdicts() {
        use Verdict::*;
        let state = select_through(judge(Some(Supports), Some(Irrelevant)), vec![Supports]);
        assert_eq!(inspected(&state), ["certbot renew --nginx -d example.com"]);
        // One verdict flipped, nothing else changed.
        let state = select_through(judge(Some(Irrelevant), Some(Irrelevant)), vec![Supports]);
        assert_eq!(inspected(&state), Vec::<&str>::new());
        let state = select_through(judge(Some(Irrelevant), Some(Supports)), vec![Supports]);
        assert_eq!(inspected(&state), ["certbot status"]);
    }

    #[test]
    fn an_item_the_child_skipped_never_passes_select() {
        use Verdict::*;
        let state = select_through(judge(Some(Contradicts), None), vec![Supports, Contradicts, Unclear, Irrelevant]);
        assert_eq!(inspected(&state), ["certbot renew --nginx -d example.com"]);
        let assessed = state["handles"].as_array().unwrap().iter().find(|h| h["kind"] == "AssessmentSet").unwrap();
        assert_eq!(
            assessed["verdicts"],
            serde_json::json!({"supports": 0, "contradicts": 1, "unclear": 0, "irrelevant": 0, "unassessed": 1})
        );
    }

    fn plan(ops: serde_json::Value) -> RootActionV1 {
        serde_json::from_value(serde_json::json!({"version": 1, "expected_state_version": 0, "kind": "plan", "plan": {"ops": ops}})).unwrap()
    }

    #[test]
    fn secrets_are_redacted_on_the_way_to_the_model_and_back() {
        let mut store = Store::open_memory().unwrap();
        seed_corpus(&mut store);
        let model = Arc::new(FakeModel::new());
        let finish = serde_json::json!({"version": 1, "expected_state_version": 0, "kind": "finish", "plan": null, "finish": {
            "status": "partial", "answer": "run mysql --password=modelsaid42 first",
            "claims": [], "procedure": [], "contradictions": [], "missing": ["m"]}});
        model.add_root_raw(finish.to_string());

        let q = "why did mysql --password=hunter2hunter2 fail?";
        let result = run_investigation(q, &store, model.clone(), BudgetV1::prototype_default(), &Arc::default(), &mut |_| {});
        let sent = model.requests();
        assert!(sent.iter().all(|r| !r.user().contains("hunter2hunter2")), "{:?}", sent[0].user());
        let answer = result.finish.unwrap().answer;
        assert!(!answer.contains("modelsaid42") && answer.contains("[REDACTED:"), "{answer}");
    }

    #[test]
    fn cancel_before_the_start_makes_no_root_call() {
        let mut store = Store::open_memory().unwrap();
        seed_corpus(&mut store);
        let model = Arc::new(FakeModel::new());
        model.add_root_response(plan(serde_json::json!([{"op": "search", "op_id": "s1", "terms": ["certbot"], "match": "any", "limit": 5}])));

        let mut events = Vec::new();
        let cancel = Arc::new(AtomicBool::new(true));
        let result = run_investigation("q", &store, model.clone(), BudgetV1::prototype_default(), &cancel, &mut |p| events.push(p));
        assert_eq!((result.status, result.turns), (InvestigationStatus::Cancelled, 0));
        assert!(model.root_states().is_empty() && events.is_empty(), "{events:?}");
    }

    #[test]
    fn a_root_reply_after_cancel_is_dropped() {
        let mut store = Store::open_memory().unwrap();
        seed_corpus(&mut store);
        let model = Arc::new(FakeModel::new());
        let finish = FinishV1 {
            status: AnswerStatus::Abstained,
            answer: String::new(),
            claims: vec![],
            procedure: vec![],
            contradictions: vec![],
            missing: vec!["m".into()],
        };
        model.add_root_response(RootActionV1 { version: 1, expected_state_version: 0, kind: RootActionKind::Finish, plan: None, finish: Some(finish) });

        // Ctrl-C while the root call is out.
        let cancel = Arc::new(AtomicBool::new(false));
        let result = run_investigation("q", &store, model.clone(), BudgetV1::prototype_default(), &cancel, &mut |_| cancel.store(true, Ordering::Relaxed));
        assert_eq!(model.root_states().len(), 1, "the call was made");
        assert_eq!(result.status, InvestigationStatus::Cancelled);
        assert!(result.finish.is_none());
    }

    #[test]
    fn cancel_during_a_child_call_admits_no_other() {
        let mut store = Store::open_memory().unwrap();
        seed_corpus(&mut store);
        let model = Arc::new(FakeModel::new());
        // 8 items: two packets of 4.
        model.add_root_response(plan(serde_json::json!([
            {"op": "search", "op_id": "s1", "terms": ["generic"], "match": "any", "limit": 8},
            {"op": "assess", "op_id": "a1", "input": {"output": "s1"}, "objective": "o", "max_items": 8},
        ])));
        let cancel = Arc::new(AtomicBool::new(false));
        let honest = FakeModel::default_supports_handler();
        let flag = Arc::clone(&cancel);
        // Ctrl-C while the first child is out.
        model.add_child_response(move |p| {
            flag.store(true, Ordering::Relaxed);
            honest(p)
        });
        model.add_child_response(FakeModel::default_supports_handler());

        let mut events = Vec::new();
        // One child at a time, so there is a next one to hold back.
        let budget = BudgetV1 { max_concurrency: 1, ..BudgetV1::prototype_default() };
        let result = run_investigation("q", &store, model.clone(), budget, &cancel, &mut |p| events.push(p));
        assert_eq!((result.status, result.child_calls), (InvestigationStatus::Cancelled, 1));
        events.retain(|e| !matches!(e, Progress::Egress(_) | Progress::Replied { .. }));
        assert_eq!(
            events,
            [
                Progress::RootCall { turn: 1 },
                Progress::Op { op_id: "s1".into() },
                Progress::Op { op_id: "a1".into() },
                Progress::ChildCall { op_id: "a1".into(), packet_id: "pkt_00".into() },
            ]
        );
    }

    #[test]
    fn root_state_stays_under_64kib() {
        let mut store = Store::open_memory().unwrap();
        seed_corpus(&mut store);

        // Run turn 1 manually to measure state size.
        let investigation_id = 1u64;
        let mut registry =
            HandleRegistry::new(investigation_id, b"key".to_vec(), 512);
        let budget = BudgetV1::prototype_default();
        let ledger = BudgetLedger::new(budget);

        // Simulate some handles in the registry.
        for i in 0..20 {
            let obs = vec![crate::domain::StoredObservation {
                id: i,
                revision_id: 1,
                ordinal: i,
                kind: ObservationKind::ZshCommand,
                observed_at_ms: Some(1_700_000_000_000),
                safe_text: format!("command number {i} with some padding text"),
                flags: 0,
                locator: vec![],
            }];
            registry.insert_evidence_set(obs);
        }

        let inspection = EvidencePacket {
            packet_id: "inspect_test".to_string(),
            items: (0..12)
                .map(|i| EvidenceItem {
                    item_id: format!("item_{i}"),
                    evidence_id: format!("ev1_{i:016x}"),
                    safe_text: format!("Long command text for item {i} with extra detail"),
                    truncation: None,
                })
                .collect(),
        };

        let state_json = build_root_state(
            "What certificate renewal steps were observed?",
            3,
            &registry,
            &Some(inspection),
            &[],
            &ledger,
            &store,
        );

        let size = state_json.to_string().len();
        assert!(
            size < 64 * 1024,
            "root state is {size} bytes, must be < 65536"
        );
    }

    #[test]
    fn budget_exhaustion_stops_investigation() {
        let mut store = Store::open_memory().unwrap();
        seed_corpus(&mut store);

        let model = Arc::new(FakeModel::new());
        // Model would return a plan, but budget is exhausted before the call.
        model.add_root_response(RootActionV1 {
            version: 1,
            expected_state_version: 0,
            kind: RootActionKind::Plan,
            plan: Some(PlanV1 {
                ops: vec![PlanOpV1::Search(SearchV1 {
                    op_id: "s1".to_string(),
                    terms: vec!["cargo".to_string()],
                    match_mode: MatchMode::Any,
                    limit: 50,
                })],
            }),
            finish: None,
        });

        // Budget with 0 root calls — exhausted immediately.
        let mut budget = BudgetV1::prototype_default();
        budget.root_calls = 0;

        let result = run_investigation("test", &store, model.clone(), budget, &Arc::default(), &mut |_| {});
        assert_eq!(result.status, InvestigationStatus::BudgetExhausted);
        assert_eq!(result.turns, 0);
    }

    #[test]
    fn abstained_investigation() {
        let mut store = Store::open_memory().unwrap();
        seed_corpus(&mut store);

        let model = Arc::new(FakeModel::new());
        model.add_root_response(RootActionV1 {
            version: 1,
            expected_state_version: 0,
            kind: RootActionKind::Finish,
            plan: None,
            finish: Some(FinishV1 {
                status: AnswerStatus::Abstained,
                answer: String::new(),
                claims: vec![],
                procedure: vec![],
                contradictions: vec![],
                missing: vec!["no relevant evidence found".to_string()],
            }),
        });

        let result = run_investigation(
            "How was the database migrated?",
            &store,
            model.clone(),
            BudgetV1::prototype_default(),
            &Arc::default(),
            &mut |_| {},
        );
        assert_eq!(result.status, InvestigationStatus::Abstained);
        assert_eq!(result.turns, 1);
        let finish = result.finish.unwrap();
        assert!(!finish.missing.is_empty());
    }
}
