use std::collections::HashMap;

use crate::rlm::budget::{BudgetExhausted, BudgetLedger, Reservation};
use crate::rlm::evidence::{HandleId, HandleRegistry};
use crate::rlm::executor::{self, ExecError};
use crate::rlm::model::{EvidenceItem, EvidencePacket, ModelBoundary, ModelError};
use crate::rlm::protocol::*;
use crate::rlm::validate::{self, HandleKind, Op, ValidationError};
use crate::store::Store;

/// Terminal result of an investigation.
#[derive(Debug)]
pub struct InvestigationResult {
    pub status: InvestigationStatus,
    pub finish: Option<FinishV1>,
    pub turns: u32,
    pub child_calls: u16,
}

#[derive(Debug, PartialEq)]
pub enum InvestigationStatus {
    Completed,
    Partial,
    Abstained,
    BudgetExhausted,
    Failed(String),
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

/// Run a bounded RLM investigation.
pub fn run_investigation(
    question: &str,
    store: &Store,
    model: &dyn ModelBoundary,
    budget: BudgetV1,
) -> InvestigationResult {
    let investigation_id = 1u64; // Single investigation for prototype.
    let installation_key = b"prototype-key".to_vec();

    let mut registry = HandleRegistry::new(
        investigation_id,
        installation_key,
        budget.evidence_handles as usize,
    );
    let mut ledger = BudgetLedger::new(budget);
    let mut state_version: u32 = 0;
    let mut turns: u32 = 0;
    let mut total_child_calls: u16 = 0;
    let mut last_inspection: Option<EvidencePacket> = None;
    // What the host tells the root about the last plan, besides its handles.
    let mut notes: Vec<String> = Vec::new();

    loop {
        // Reserve budget for a root call.
        if ledger.reserve(&Reservation {
            root_calls: 1,
            ..Default::default()
        }).is_err() {
            return InvestigationResult {
                status: InvestigationStatus::BudgetExhausted,
                finish: None,
                turns,
                child_calls: total_child_calls,
            };
        }

        // Build root state and call model.
        let state_json = build_root_state_json(
            question,
            state_version,
            &registry,
            &last_inspection,
            &std::mem::take(&mut notes),
            &ledger,
            store,
        );

        let action = match model.root_call(&state_json) {
            Ok(a) => a,
            Err(e) => {
                return InvestigationResult {
                    status: InvestigationStatus::Failed(format!("model error: {e}")),
                    finish: None,
                    turns,
                    child_calls: total_child_calls,
                };
            }
        };
        turns += 1;

        match action.kind {
            RootActionKind::Plan => {
                // Validate the plan.
                let validated = match validate::validate_plan(&action, state_version) {
                    Ok(v) => v,
                    Err(e) => {
                        return InvestigationResult {
                            status: InvestigationStatus::Failed(format!("validation: {e}")),
                            finish: None,
                            turns,
                            child_calls: total_child_calls,
                        };
                    }
                };

                // Reserve budget for plan ops.
                let op_count = validated.ops.len() as u16;
                if ledger.reserve(&Reservation {
                    plan_ops: op_count,
                    ..Default::default()
                }).is_err() {
                    return InvestigationResult {
                        status: InvestigationStatus::BudgetExhausted,
                        finish: None,
                        turns,
                        child_calls: total_child_calls,
                    };
                }

                // Execute ops sequentially.
                let mut plan_outputs: HashMap<String, HandleId> = HashMap::new();

                for op in &validated.ops {
                    match execute_op(
                        op,
                        store,
                        &mut registry,
                        &plan_outputs,
                        model,
                        &mut ledger,
                        &mut total_child_calls,
                        &mut last_inspection,
                        &mut notes,
                    ) {
                        Ok(Some((op_id, handle_id))) => {
                            plan_outputs.insert(op_id, handle_id);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            return InvestigationResult {
                                status: InvestigationStatus::Failed(format!("exec: {e}")),
                                finish: None,
                                turns,
                                child_calls: total_child_calls,
                            };
                        }
                    }
                }

                // Observation committed — increment state version.
                state_version += 1;
            }

            RootActionKind::Finish => {
                let finish = match action.finish {
                    Some(f) => f,
                    None => {
                        return InvestigationResult {
                            status: InvestigationStatus::Failed(
                                "finish action missing finish body".into(),
                            ),
                            finish: None,
                            turns,
                            child_calls: total_child_calls,
                        };
                    }
                };

                let status = match finish.status {
                    AnswerStatus::Answered => InvestigationStatus::Completed,
                    AnswerStatus::Partial => InvestigationStatus::Partial,
                    AnswerStatus::Abstained => InvestigationStatus::Abstained,
                };

                return InvestigationResult {
                    status,
                    finish: Some(finish),
                    turns,
                    child_calls: total_child_calls,
                };
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_op(
    op: &Op,
    store: &Store,
    registry: &mut HandleRegistry,
    plan_outputs: &HashMap<String, HandleId>,
    model: &dyn ModelBoundary,
    ledger: &mut BudgetLedger,
    child_calls: &mut u16,
    last_inspection: &mut Option<EvidencePacket>,
    notes: &mut Vec<String>,
) -> Result<Option<(String, HandleId)>, CoordinatorError> {
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

            let hid = executor::execute_search(store, s, registry, &crate::app::Budget::start(), notes)
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

            let hid = executor::execute_expand(store, e, registry, plan_outputs)
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

            let hid = executor::execute_assess(a, registry, plan_outputs, model, ledger, child_calls, notes)
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

/// Build a JSON root state for the model. Must stay under 64 KiB.
fn build_root_state_json(
    question: &str,
    state_version: u32,
    registry: &HandleRegistry,
    last_inspection: &Option<EvidencePacket>,
    notes: &[String],
    ledger: &BudgetLedger,
    store: &Store,
) -> String {
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

    serde_json::to_string(&state).unwrap_or_default()
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

        let model = FakeModel::new();

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

        // Turn 2: finish with cited claims.
        // The model will see inspection items with evidence IDs. We use a closure
        // to construct the finish after the root state is available.
        // For the prototype, we pre-construct with placeholder evidence IDs
        // since FakeModel root_call ignores state_json.
        model.add_root_response(RootActionV1 {
            version: 1,
            expected_state_version: 1,
            kind: RootActionKind::Finish,
            plan: None,
            finish: Some(FinishV1 {
                status: AnswerStatus::Answered,
                answer: "Certificate was renewed using certbot renew.".to_string(),
                claims: vec![FinalClaimV1 {
                    text: "certbot renew was executed".to_string(),
                    supports: vec!["placeholder_ev".to_string()],
                    contradicts: vec![],
                }],
                procedure: vec![ProcedureStepV1 {
                    text: "Run certbot renew --nginx -d example.com".to_string(),
                    supports: vec!["placeholder_ev".to_string()],
                }],
                contradictions: vec![],
                missing: vec![],
            }),
        });

        let budget = BudgetV1::prototype_default();
        let result = run_investigation(
            "What certificate renewal steps were observed?",
            &store,
            &model,
            budget,
        );

        // Core assertions for the wayfinder question:
        assert_eq!(result.status, InvestigationStatus::Completed);
        assert_eq!(result.turns, 2);
        assert!(result.child_calls >= 2, "at least 2 child calls for assess batches");
        assert!(result.finish.is_some());

        let finish = result.finish.unwrap();
        assert_eq!(finish.status, AnswerStatus::Answered);
        assert!(!finish.claims.is_empty());
        assert!(!finish.procedure.is_empty());
    }

    /// Run search `certbot` -> assess -> select `verdicts` -> inspect with
    /// `child` judging, and return the root state the finish turn saw.
    fn select_through(
        child: impl Fn(&EvidencePacket) -> ChildResultV1 + Send + 'static,
        verdicts: Vec<Verdict>,
    ) -> serde_json::Value {
        let mut store = Store::open_memory().unwrap();
        seed_corpus(&mut store);
        let model = FakeModel::new();
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
        let result = run_investigation("q", &store, &model, BudgetV1::prototype_default());
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

        let state_json = build_root_state_json(
            "What certificate renewal steps were observed?",
            3,
            &registry,
            &Some(inspection),
            &[],
            &ledger,
            &store,
        );

        let size = state_json.len();
        assert!(
            size < 64 * 1024,
            "root state is {size} bytes, must be < 65536"
        );
    }

    #[test]
    fn budget_exhaustion_stops_investigation() {
        let mut store = Store::open_memory().unwrap();
        seed_corpus(&mut store);

        let model = FakeModel::new();
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

        let result = run_investigation("test", &store, &model, budget);
        assert_eq!(result.status, InvestigationStatus::BudgetExhausted);
        assert_eq!(result.turns, 0);
    }

    #[test]
    fn abstained_investigation() {
        let mut store = Store::open_memory().unwrap();
        seed_corpus(&mut store);

        let model = FakeModel::new();
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
            &model,
            BudgetV1::prototype_default(),
        );
        assert_eq!(result.status, InvestigationStatus::Abstained);
        assert_eq!(result.turns, 1);
        let finish = result.finish.unwrap();
        assert!(!finish.missing.is_empty());
    }
}
