use std::collections::{HashMap, HashSet};

use crate::rlm::protocol::*;

/// The kind of result an operator produces or consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleKind {
    EvidenceSet,
    AssessmentSet,
    Inspection,
}

/// A plan that passed structural and semantic validation.
#[derive(Debug)]
pub struct ValidatedPlan {
    pub ops: Vec<Op>,
}

/// An operator the kernel executes: what is left of [`PlanOpV1`] once
/// admission has refused the ones not built.
#[derive(Debug)]
pub enum Op {
    Search(SearchV1),
    Expand(ExpandV1),
    Assess(AssessV1),
    Select(SelectV1),
    Inspect(InspectV1),
}

#[derive(Debug, PartialEq)]
pub enum ValidationError {
    EmptyPlan,
    NotExecuted(&'static str),
    TooManyOps {
        count: usize,
        max: usize,
    },
    DuplicateOpId(String),
    UnknownInputRef {
        op_id: String,
        ref_id: String,
    },
    ForwardReference {
        op_id: String,
        ref_id: String,
    },
    WrongInputKind {
        op_id: String,
        expected: HandleKind,
        got: HandleKind,
    },
    StaleStateVersion {
        expected: u32,
        got: u32,
    },
    SearchTooManyTerms {
        op_id: String,
        count: usize,
        max: usize,
    },
    SearchNoTerms {
        op_id: String,
    },
    LimitZero {
        op_id: String,
    },
    AssessObjectiveTooLong {
        op_id: String,
        bytes: usize,
        max: usize,
    },
    AssessMaxItemsExceeded {
        op_id: String,
        count: u32,
        max: u32,
    },
    ExpandWindowExceeded {
        op_id: String,
        per_side: u32,
        max: u32,
    },
}

impl std::fmt::Display for HandleKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::EvidenceSet => "an evidence set",
            Self::AssessmentSet => "an assessment set",
            Self::Inspection => "an inspection",
        })
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPlan => f.write_str("the plan has no operations"),
            Self::NotExecuted(op) => write!(f, "{op} is not run in this version"),
            Self::TooManyOps { count, max } => {
                write!(f, "{count} operations, over the {max} a plan may hold")
            }
            Self::DuplicateOpId(id) => write!(f, "op_id {id} is used twice"),
            Self::UnknownInputRef { op_id, ref_id } => {
                write!(f, "{op_id} reads {ref_id}, which no operation made")
            }
            Self::ForwardReference { op_id, ref_id } => {
                write!(f, "{op_id} reads {ref_id} before it is made")
            }
            Self::WrongInputKind {
                op_id,
                expected,
                got,
            } => write!(f, "{op_id} needs {expected}, not {got}"),
            Self::StaleStateVersion { expected, got } => {
                write!(
                    f,
                    "the plan is for state version {got}, and the state is at {expected}"
                )
            }
            Self::SearchTooManyTerms { op_id, count, max } => {
                write!(f, "{op_id} searches {count} terms, over the {max} allowed")
            }
            Self::SearchNoTerms { op_id } => write!(f, "{op_id} searches no terms"),
            Self::LimitZero { op_id } => write!(f, "{op_id} has a limit of 0"),
            Self::AssessObjectiveTooLong { op_id, bytes, max } => {
                write!(
                    f,
                    "{op_id}'s objective is {bytes} bytes, over the {max} allowed"
                )
            }
            Self::AssessMaxItemsExceeded { op_id, count, max } => {
                write!(f, "{op_id} assesses {count} items, over the {max} allowed")
            }
            Self::ExpandWindowExceeded {
                op_id,
                per_side,
                max,
            } => {
                write!(
                    f,
                    "{op_id} expands {per_side} records each side, over the {max} allowed"
                )
            }
        }
    }
}

impl std::error::Error for ValidationError {}

const MAX_OPS_PER_PLAN: usize = 8;
/// The one word cap every search shares (ADR 0006).
const MAX_SEARCH_TERMS: usize = crate::app::MAX_QUERY_WORDS;
const MAX_ASSESS_ITEMS: u32 = 64;
/// Goes into every child packet beside its items; this leaves a lone item
/// most of the packet cap, so truncation can always make it fit.
pub const MAX_OBJECTIVE_BYTES: usize = 1024;
const MAX_EXPAND_PER_SIDE: u32 = 32;

/// Validate a plan structurally and semantically.
pub fn validate_plan(
    action: &RootActionV1,
    current_state_version: u32,
) -> Result<ValidatedPlan, ValidationError> {
    if action.expected_state_version != current_state_version {
        return Err(ValidationError::StaleStateVersion {
            expected: current_state_version,
            got: action.expected_state_version,
        });
    }

    let plan = action.plan.as_ref().ok_or(ValidationError::EmptyPlan)?;

    if plan.ops.is_empty() {
        return Err(ValidationError::EmptyPlan);
    }
    if plan.ops.len() > MAX_OPS_PER_PLAN {
        return Err(ValidationError::TooManyOps {
            count: plan.ops.len(),
            max: MAX_OPS_PER_PLAN,
        });
    }

    let ops = plan
        .ops
        .iter()
        .map(|op| match op.clone() {
            PlanOpV1::Search(s) => Ok(Op::Search(s)),
            PlanOpV1::Expand(e) => Ok(Op::Expand(e)),
            PlanOpV1::Assess(a) => Ok(Op::Assess(a)),
            PlanOpV1::Select(s) => Ok(Op::Select(s)),
            PlanOpV1::Inspect(i) => Ok(Op::Inspect(i)),
            PlanOpV1::Relate => Err(ValidationError::NotExecuted("relate")),
            PlanOpV1::Combine => Err(ValidationError::NotExecuted("combine")),
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Check for duplicate op_ids and build index.
    let mut seen_ids = HashSet::new();
    let mut op_index: HashMap<&str, usize> = HashMap::new();
    for (i, op) in ops.iter().enumerate() {
        let id = op_id(op);
        if !seen_ids.insert(id) {
            return Err(ValidationError::DuplicateOpId(id.to_string()));
        }
        op_index.insert(id, i);
    }

    // Validate each op: resolve inputs, check kinds, check limits.
    let mut output_kinds: HashMap<&str, HandleKind> = HashMap::new();

    for (i, op) in ops.iter().enumerate() {
        let id = op_id(op);

        // Validate operator-specific limits.
        validate_op_limits(op)?;

        // Resolve inputs and check for forward references.
        for input in op_inputs(op) {
            if let InputRefV1::Output(ref_id) = input {
                match op_index.get(ref_id.as_str()) {
                    None => {
                        return Err(ValidationError::UnknownInputRef {
                            op_id: id.to_string(),
                            ref_id: ref_id.clone(),
                        });
                    }
                    Some(&ref_pos) if ref_pos >= i => {
                        return Err(ValidationError::ForwardReference {
                            op_id: id.to_string(),
                            ref_id: ref_id.clone(),
                        });
                    }
                    Some(_) => {}
                }

                // Check input kind compatibility.
                if let Some(&input_kind) = output_kinds.get(ref_id.as_str()) {
                    let expected = expected_input_kind(op);
                    if let Some(exp) = expected
                        && exp != input_kind
                    {
                        return Err(ValidationError::WrongInputKind {
                            op_id: id.to_string(),
                            expected: exp,
                            got: input_kind,
                        });
                    }
                }
            }
            // Handle references are validated at execution time (they refer to
            // pre-existing handles in the registry, not plan outputs).
        }

        output_kinds.insert(id, output_kind(op));
    }

    Ok(ValidatedPlan { ops })
}

pub fn op_id(op: &Op) -> &str {
    match op {
        Op::Search(s) => &s.op_id,
        Op::Expand(e) => &e.op_id,
        Op::Assess(a) => &a.op_id,
        Op::Select(s) => &s.op_id,
        Op::Inspect(i) => &i.op_id,
    }
}

fn op_inputs(op: &Op) -> Vec<&InputRefV1> {
    match op {
        Op::Search(_) => vec![],
        Op::Expand(e) => vec![&e.input],
        Op::Assess(a) => vec![&a.input],
        Op::Select(s) => vec![&s.input],
        Op::Inspect(i) => vec![&i.input],
    }
}

fn output_kind(op: &Op) -> HandleKind {
    match op {
        Op::Search(_) => HandleKind::EvidenceSet,
        Op::Expand(_) => HandleKind::EvidenceSet,
        Op::Assess(_) => HandleKind::AssessmentSet,
        Op::Select(_) => HandleKind::EvidenceSet,
        Op::Inspect(_) => HandleKind::Inspection,
    }
}

/// What input kind does this operator expect? None means "any".
fn expected_input_kind(op: &Op) -> Option<HandleKind> {
    match op {
        Op::Search(_) => None,
        Op::Expand(_) => Some(HandleKind::EvidenceSet),
        Op::Assess(_) => Some(HandleKind::EvidenceSet),
        Op::Select(_) => Some(HandleKind::AssessmentSet),
        Op::Inspect(_) => Some(HandleKind::EvidenceSet),
    }
}

fn validate_op_limits(op: &Op) -> Result<(), ValidationError> {
    match op {
        Op::Search(s) => {
            if s.terms.is_empty() {
                return Err(ValidationError::SearchNoTerms {
                    op_id: s.op_id.clone(),
                });
            }
            if s.terms.len() > MAX_SEARCH_TERMS {
                return Err(ValidationError::SearchTooManyTerms {
                    op_id: s.op_id.clone(),
                    count: s.terms.len(),
                    max: MAX_SEARCH_TERMS,
                });
            }
            if s.limit == 0 {
                return Err(ValidationError::LimitZero {
                    op_id: s.op_id.clone(),
                });
            }
        }
        Op::Expand(e) => {
            if e.limit == 0 {
                return Err(ValidationError::LimitZero {
                    op_id: e.op_id.clone(),
                });
            }
            if e.before > MAX_EXPAND_PER_SIDE {
                return Err(ValidationError::ExpandWindowExceeded {
                    op_id: e.op_id.clone(),
                    per_side: e.before,
                    max: MAX_EXPAND_PER_SIDE,
                });
            }
            if e.after > MAX_EXPAND_PER_SIDE {
                return Err(ValidationError::ExpandWindowExceeded {
                    op_id: e.op_id.clone(),
                    per_side: e.after,
                    max: MAX_EXPAND_PER_SIDE,
                });
            }
        }
        Op::Assess(a) => {
            if a.objective.len() > MAX_OBJECTIVE_BYTES {
                return Err(ValidationError::AssessObjectiveTooLong {
                    op_id: a.op_id.clone(),
                    bytes: a.objective.len(),
                    max: MAX_OBJECTIVE_BYTES,
                });
            }
            if a.max_items > MAX_ASSESS_ITEMS {
                return Err(ValidationError::AssessMaxItemsExceeded {
                    op_id: a.op_id.clone(),
                    count: a.max_items,
                    max: MAX_ASSESS_ITEMS,
                });
            }
            if a.max_items == 0 {
                return Err(ValidationError::LimitZero {
                    op_id: a.op_id.clone(),
                });
            }
        }
        Op::Select(s) => {
            if s.limit == 0 {
                return Err(ValidationError::LimitZero {
                    op_id: s.op_id.clone(),
                });
            }
        }
        Op::Inspect(i) => {
            if i.limit == 0 {
                return Err(ValidationError::LimitZero {
                    op_id: i.op_id.clone(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_action(state_version: u32, ops: Vec<PlanOpV1>) -> RootActionV1 {
        RootActionV1 {
            version: 1,
            expected_state_version: state_version,
            kind: RootActionKind::Plan,
            plan: Some(PlanV1 { ops }),
            finish: None,
        }
    }

    fn search(id: &str, terms: &[&str], limit: u32) -> PlanOpV1 {
        PlanOpV1::Search(SearchV1 {
            op_id: id.to_string(),
            terms: terms.iter().map(ToString::to_string).collect(),
            match_mode: MatchMode::Any,
            limit,
        })
    }

    fn expand(id: &str, input_op: &str) -> PlanOpV1 {
        PlanOpV1::Expand(ExpandV1 {
            op_id: id.to_string(),
            input: InputRefV1::Output(input_op.to_string()),
            scope: ExpandScope::SameSource,
            before: 5,
            after: 5,
            limit: 100,
        })
    }

    fn assess(id: &str, input_op: &str) -> PlanOpV1 {
        PlanOpV1::Assess(AssessV1 {
            op_id: id.to_string(),
            input: InputRefV1::Output(input_op.to_string()),
            objective: "test".to_string(),
            max_items: 32,
        })
    }

    fn select(id: &str, input_op: &str) -> PlanOpV1 {
        PlanOpV1::Select(SelectV1 {
            op_id: id.to_string(),
            input: InputRefV1::Output(input_op.to_string()),
            verdicts: vec![Verdict::Supports],
            limit: 20,
        })
    }

    fn inspect(id: &str, input_op: &str) -> PlanOpV1 {
        PlanOpV1::Inspect(InspectV1 {
            op_id: id.to_string(),
            input: InputRefV1::Output(input_op.to_string()),
            limit: 12,
            max_item_bytes: 2048,
        })
    }

    #[test]
    fn valid_plan_passes() {
        let action = make_action(
            0,
            vec![
                search("s1", &["cargo"], 50),
                expand("e1", "s1"),
                assess("a1", "e1"),
                select("k1", "a1"),
                inspect("i1", "k1"),
            ],
        );
        assert!(validate_plan(&action, 0).is_ok());
    }

    #[test]
    fn relate_and_combine_refused_at_admission() {
        let ops = [
            (
                "relate",
                r#"{"op":"relate","op_id":"r1","input":{"output":"s1"},"relation":"git_state","per_seed":2,"limit":80}"#,
            ),
            (
                "combine",
                r#"{"op":"combine","op_id":"c1","inputs":[{"output":"s1"}],"mode":"union","limit":20}"#,
            ),
        ];
        for (name, op) in ops {
            let json = format!(
                r#"{{"version":1,"expected_state_version":0,"kind":"plan","plan":{{"ops":[{{"op":"search","op_id":"s1","terms":["git"],"match":"any","limit":5}},{op}]}}}}"#
            );
            let action: RootActionV1 = serde_json::from_str(&json).unwrap();
            assert_eq!(
                validate_plan(&action, 0).unwrap_err(),
                ValidationError::NotExecuted(name)
            );
        }
    }

    #[test]
    fn empty_plan_rejected() {
        let action = make_action(0, vec![]);
        assert_eq!(
            validate_plan(&action, 0).unwrap_err(),
            ValidationError::EmptyPlan
        );
    }

    #[test]
    fn too_many_ops_rejected() {
        let ops: Vec<PlanOpV1> = (0..9)
            .map(|i| search(&format!("s{i}"), &["cargo"], 50))
            .collect();
        let action = make_action(0, ops);
        assert!(matches!(
            validate_plan(&action, 0).unwrap_err(),
            ValidationError::TooManyOps { count: 9, max: 8 }
        ));
    }

    #[test]
    fn duplicate_op_id_rejected() {
        let action = make_action(
            0,
            vec![search("s1", &["cargo"], 50), search("s1", &["git"], 50)],
        );
        assert_eq!(
            validate_plan(&action, 0).unwrap_err(),
            ValidationError::DuplicateOpId("s1".to_string())
        );
    }

    #[test]
    fn forward_reference_rejected() {
        let action = make_action(0, vec![expand("e1", "s1"), search("s1", &["cargo"], 50)]);
        assert!(matches!(
            validate_plan(&action, 0).unwrap_err(),
            ValidationError::ForwardReference { .. }
        ));
    }

    #[test]
    fn unknown_input_ref_rejected() {
        let action = make_action(0, vec![expand("e1", "nonexistent")]);
        assert!(matches!(
            validate_plan(&action, 0).unwrap_err(),
            ValidationError::UnknownInputRef { .. }
        ));
    }

    #[test]
    fn wrong_input_kind_rejected() {
        // select expects assessment_set, but search produces evidence_set
        let action = make_action(0, vec![search("s1", &["cargo"], 50), select("k1", "s1")]);
        assert!(matches!(
            validate_plan(&action, 0).unwrap_err(),
            ValidationError::WrongInputKind {
                expected: HandleKind::AssessmentSet,
                got: HandleKind::EvidenceSet,
                ..
            }
        ));
    }

    #[test]
    fn stale_state_version_rejected() {
        let action = make_action(2, vec![search("s1", &["cargo"], 50)]);
        assert_eq!(
            validate_plan(&action, 5).unwrap_err(),
            ValidationError::StaleStateVersion {
                expected: 5,
                got: 2
            }
        );
    }

    #[test]
    fn search_too_many_terms_rejected() {
        let terms: Vec<&str> = (0..13).map(|_| "x").collect();
        let action = make_action(0, vec![search("s1", &terms, 50)]);
        assert!(matches!(
            validate_plan(&action, 0).unwrap_err(),
            ValidationError::SearchTooManyTerms { .. }
        ));
    }

    #[test]
    fn search_no_terms_rejected() {
        let action = make_action(0, vec![search("s1", &[], 50)]);
        assert!(matches!(
            validate_plan(&action, 0).unwrap_err(),
            ValidationError::SearchNoTerms { .. }
        ));
    }

    #[test]
    fn limit_zero_rejected() {
        let action = make_action(0, vec![search("s1", &["cargo"], 0)]);
        assert!(matches!(
            validate_plan(&action, 0).unwrap_err(),
            ValidationError::LimitZero { .. }
        ));
    }

    #[test]
    fn assess_objective_over_cap_rejected() {
        let action = |bytes: usize| {
            make_action(
                0,
                vec![
                    search("s1", &["cargo"], 50),
                    PlanOpV1::Assess(AssessV1 {
                        op_id: "a1".to_string(),
                        input: InputRefV1::Output("s1".to_string()),
                        objective: "o".repeat(bytes),
                        max_items: 8,
                    }),
                ],
            )
        };
        assert!(validate_plan(&action(MAX_OBJECTIVE_BYTES), 0).is_ok());
        assert!(matches!(
            validate_plan(&action(MAX_OBJECTIVE_BYTES + 1), 0).unwrap_err(),
            ValidationError::AssessObjectiveTooLong { bytes: 1025, .. }
        ));
    }

    #[test]
    fn assess_max_items_exceeded_rejected() {
        let action = make_action(
            0,
            vec![
                search("s1", &["cargo"], 50),
                PlanOpV1::Assess(AssessV1 {
                    op_id: "a1".to_string(),
                    input: InputRefV1::Output("s1".to_string()),
                    objective: "test".to_string(),
                    max_items: 65,
                }),
            ],
        );
        assert!(matches!(
            validate_plan(&action, 0).unwrap_err(),
            ValidationError::AssessMaxItemsExceeded { .. }
        ));
    }

    #[test]
    fn expand_window_exceeded_rejected() {
        let action = make_action(
            0,
            vec![
                search("s1", &["cargo"], 50),
                PlanOpV1::Expand(ExpandV1 {
                    op_id: "e1".to_string(),
                    input: InputRefV1::Output("s1".to_string()),
                    scope: ExpandScope::SameSource,
                    before: 33,
                    after: 5,
                    limit: 100,
                }),
            ],
        );
        assert!(matches!(
            validate_plan(&action, 0).unwrap_err(),
            ValidationError::ExpandWindowExceeded { .. }
        ));
    }

    #[test]
    fn arbitrary_json_never_panics() {
        let bad_inputs = [
            "null",
            "42",
            "\"hello\"",
            "[]",
            "{}",
            r#"{"version":1,"expected_state_version":0,"kind":"plan","plan":{"ops":[null]}}"#,
            r#"{"version":1,"expected_state_version":0,"kind":"plan","plan":{"ops":[{"op":"search"}]}}"#,
            r#"{"op":"search","op_id":true}"#,
        ];
        for input in &bad_inputs {
            // Must not panic — Ok or Err both fine.
            let _ = serde_json::from_str::<RootActionV1>(input);
            let _ = serde_json::from_str::<ChildResultV1>(input);
            let _ = serde_json::from_str::<PlanOpV1>(input);
        }
    }
}
