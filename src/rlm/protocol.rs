use serde::{Deserialize, Serialize};

// Every struct a model writes denies unknown fields: a field the host does
// not read is a model fault or an injection attempt, and fails closed (ADR 0009).

// --- Plan operators ---

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PlanOpV1 {
    Search(SearchV1),
    Expand(ExpandV1),
    // Named so that admission can say they are not built (ADR 0009); their
    // fields are ignored, never read.
    Relate,
    Combine,
    Assess(AssessV1),
    Select(SelectV1),
    Inspect(InspectV1),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SearchV1 {
    pub op_id: String,
    pub terms: Vec<String>,
    #[serde(rename = "match")]
    pub match_mode: MatchMode,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    Any,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExpandV1 {
    pub op_id: String,
    pub input: InputRefV1,
    pub scope: ExpandScope,
    pub before: u32,
    pub after: u32,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ExpandScope {
    SameSource,
    SameSession,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AssessV1 {
    pub op_id: String,
    pub input: InputRefV1,
    pub objective: String,
    pub max_items: u32,
}

/// Marks an item the host cut to fit its packet. Truncated evidence cannot
/// support a claim about what was cut (research, evidence sufficiency 5).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TruncationV1 {
    pub original_bytes: usize,
    pub sent_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SelectV1 {
    pub op_id: String,
    pub input: InputRefV1,
    pub verdicts: Vec<Verdict>,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InspectV1 {
    pub op_id: String,
    pub input: InputRefV1,
    pub limit: u32,
    pub max_item_bytes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InputRefV1 {
    Handle(String),
    Output(String),
}

// --- Plan and root action ---

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlanV1 {
    pub ops: Vec<PlanOpV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RootActionV1 {
    pub version: u8,
    pub expected_state_version: u32,
    pub kind: RootActionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish: Option<FinishV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RootActionKind {
    Plan,
    Finish,
}

// --- Finish and claims ---

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FinishV1 {
    pub status: AnswerStatus,
    pub answer: String,
    pub claims: Vec<FinalClaimV1>,
    #[serde(default)]
    pub procedure: Vec<ProcedureStepV1>,
    #[serde(default)]
    pub contradictions: Vec<String>,
    #[serde(default)]
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AnswerStatus {
    Answered,
    Partial,
    Abstained,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FinalClaimV1 {
    pub text: String,
    pub supports: Vec<String>,
    #[serde(default)]
    pub contradicts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProcedureStepV1 {
    pub text: String,
    pub supports: Vec<String>,
}

// --- Child result ---

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ChildResultV1 {
    pub version: u8,
    pub packet_id: String,
    pub assessments: Vec<ItemAssessmentV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ItemAssessmentV1 {
    pub item_id: String,
    pub verdict: Verdict,
    pub confidence: Confidence,
    pub claims: Vec<ClaimV1>,
    #[serde(default)]
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Supports,
    Contradicts,
    Unclear,
    Irrelevant,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClaimV1 {
    pub text: String,
    #[serde(default)]
    pub supports: Vec<String>,
    #[serde(default)]
    pub contradicts: Vec<String>,
}

// --- Budget ---

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetV1 {
    pub wall_ms: u64,
    pub root_calls: u16,
    pub plan_ops: u16,
    pub child_calls: u16,
    pub provider_attempts: u16,
    pub max_concurrency: u8,
    pub max_depth: u8,
    pub rows_returned: u64,
    pub evidence_handles: u32,
    pub local_materialized_bytes: u64,
    pub model_egress_bytes: u64,
    pub estimated_input_tokens: u64,
    pub reserved_output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cost_microusd: Option<u64>,
}

impl BudgetV1 {
    /// Recommended prototype defaults from the spec.
    pub fn prototype_default() -> Self {
        Self {
            wall_ms: 180_000,
            root_calls: 8,
            plan_ops: 48,
            child_calls: 16,
            provider_attempts: 18,
            max_concurrency: 4,
            max_depth: 1,
            rows_returned: 2_000,
            evidence_handles: 512,
            local_materialized_bytes: 4 * 1024 * 1024,
            model_egress_bytes: 256 * 1024,
            estimated_input_tokens: 96_000,
            reserved_output_tokens: 24_000,
            max_cost_microusd: Some(1_000_000),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Golden JSON round-trips for each PlanOpV1 variant ---

    #[test]
    fn search_op_round_trips() {
        let json = r#"{"op":"search","op_id":"s1","terms":["cargo","build"],"match":"any","limit":50}"#;
        let op: PlanOpV1 = serde_json::from_str(json).unwrap();
        assert!(matches!(&op, PlanOpV1::Search(s) if s.op_id == "s1" && s.terms.len() == 2 && s.limit == 50));
        let rt: PlanOpV1 = serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        assert_eq!(op, rt);
    }

    #[test]
    fn expand_op_round_trips() {
        let json = r#"{"op":"expand","op_id":"e1","input":{"output":"s1"},"scope":"same_source","before":5,"after":10,"limit":100}"#;
        let op: PlanOpV1 = serde_json::from_str(json).unwrap();
        assert!(matches!(&op, PlanOpV1::Expand(e) if e.op_id == "e1" && e.before == 5 && e.after == 10));
        let rt: PlanOpV1 = serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        assert_eq!(op, rt);
    }

    #[test]
    fn assess_op_round_trips() {
        let json = r#"{"op":"assess","op_id":"a1","input":{"output":"e1"},"objective":"find renewal steps","max_items":32}"#;
        let op: PlanOpV1 = serde_json::from_str(json).unwrap();
        assert!(matches!(&op, PlanOpV1::Assess(a) if a.max_items == 32));
        let rt: PlanOpV1 = serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        assert_eq!(op, rt);
    }

    #[test]
    fn select_op_round_trips() {
        let json = r#"{"op":"select","op_id":"k1","input":{"output":"a1"},"verdicts":["supports","contradicts"],"limit":20}"#;
        let op: PlanOpV1 = serde_json::from_str(json).unwrap();
        assert!(matches!(&op, PlanOpV1::Select(s) if s.verdicts.len() == 2));
        let rt: PlanOpV1 = serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        assert_eq!(op, rt);
    }

    #[test]
    fn inspect_op_round_trips() {
        let json = r#"{"op":"inspect","op_id":"i1","input":{"output":"k1"},"limit":12,"max_item_bytes":2048}"#;
        let op: PlanOpV1 = serde_json::from_str(json).unwrap();
        assert!(matches!(&op, PlanOpV1::Inspect(i) if i.limit == 12 && i.max_item_bytes == 2048));
        let rt: PlanOpV1 = serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        assert_eq!(op, rt);
    }

    #[test]
    fn unknown_op_rejected() {
        let json = r#"{"op":"hack","op_id":"x1"}"#;
        assert!(serde_json::from_str::<PlanOpV1>(json).is_err());
    }

    #[test]
    fn unknown_field_rejected_at_every_depth() {
        let op = r#"{"op":"search","op_id":"s1","terms":["git"],"match":"any","limit":5}"#;
        let action = |op: &str| format!(r#"{{"version":1,"expected_state_version":0,"kind":"plan","plan":{{"ops":[{op}]}}}}"#);
        let child = |claim: &str| format!(
            r#"{{"version":1,"packet_id":"p","assessments":[{{"item_id":"i","verdict":"supports","confidence":"low","claims":[{claim}]}}]}}"#
        );
        // The same documents parse without the extra field, so only it can fail them.
        assert!(serde_json::from_str::<RootActionV1>(&action(op)).is_ok());
        assert!(serde_json::from_str::<ChildResultV1>(&child(r#"{"text":"t"}"#)).is_ok());

        let op_extra = op.replace(r#""limit":5"#, r#""limit":5,"sql":"x""#);
        let action_extra = action(op).replacen('{', r#"{"note":"x","#, 1);
        let finish_extra = r#"{"version":1,"expected_state_version":0,"kind":"finish","finish":{"status":"partial","answer":"","claims":[{"text":"t","supports":[],"why":"x"}]}}"#;
        assert!(serde_json::from_str::<RootActionV1>(&finish_extra.replace(r#","why":"x""#, "")).is_ok());
        assert!(serde_json::from_str::<RootActionV1>(&action(&op_extra)).is_err(), "inside the tagged op");
        assert!(serde_json::from_str::<RootActionV1>(&action_extra).is_err(), "on the action");
        assert!(serde_json::from_str::<RootActionV1>(finish_extra).is_err(), "on a final claim");
        assert!(serde_json::from_str::<ChildResultV1>(&child(r#"{"text":"t","evidence":"x"}"#)).is_err(), "on a child claim");
    }

    // --- RootActionV1 round-trips ---

    #[test]
    fn root_action_plan_round_trips() {
        let json = r#"{
            "version": 1,
            "expected_state_version": 3,
            "kind": "plan",
            "plan": {
                "ops": [
                    {"op":"search","op_id":"s1","terms":["certificate","renew"],"match":"any","limit":120}
                ]
            }
        }"#;
        let action: RootActionV1 = serde_json::from_str(json).unwrap();
        assert_eq!(action.version, 1);
        assert_eq!(action.kind, RootActionKind::Plan);
        assert!(action.plan.is_some());
        assert!(action.finish.is_none());
        let rt: RootActionV1 = serde_json::from_str(&serde_json::to_string(&action).unwrap()).unwrap();
        assert_eq!(action, rt);
    }

    #[test]
    fn root_action_finish_round_trips() {
        let json = r#"{
            "version": 1,
            "expected_state_version": 5,
            "kind": "finish",
            "finish": {
                "status": "answered",
                "answer": "The certificate was renewed via certbot.",
                "claims": [
                    {"text": "certbot renew was run", "supports": ["ev1_abc"], "contradicts": []}
                ],
                "procedure": [
                    {"text": "Run certbot renew", "supports": ["ev1_abc"]}
                ],
                "contradictions": [],
                "missing": []
            }
        }"#;
        let action: RootActionV1 = serde_json::from_str(json).unwrap();
        assert_eq!(action.kind, RootActionKind::Finish);
        assert!(action.finish.is_some());
        assert!(action.plan.is_none());
        let f = action.finish.as_ref().unwrap();
        assert_eq!(f.status, AnswerStatus::Answered);
        assert_eq!(f.claims.len(), 1);
        assert_eq!(f.procedure.len(), 1);
    }

    #[test]
    fn spec_example_plan_round_trips() {
        let json = r#"{
            "version": 1,
            "expected_state_version": 3,
            "kind": "plan",
            "plan": {
                "ops": [
                    {"op":"search","op_id":"s1","terms":["certificate","renew"],"match":"any","limit":120},
                    {"op":"expand","op_id":"e1","input":{"output":"s1"},"scope":"same_session","before":8,"after":16,"limit":200},
                    {"op":"relate","op_id":"g1","input":{"output":"e1"},"relation":"git_state","per_seed":2,"limit":80},
                    {"op":"assess","op_id":"a1","input":{"output":"g1"},"objective":"find observed renewal steps and contrary outcomes","max_items":32},
                    {"op":"select","op_id":"k1","input":{"output":"a1"},"verdicts":["supports","contradicts"],"limit":20},
                    {"op":"inspect","op_id":"i1","input":{"output":"k1"},"limit":12,"max_item_bytes":2048}
                ]
            }
        }"#;
        let action: RootActionV1 = serde_json::from_str(json).unwrap();
        assert_eq!(action.plan.as_ref().unwrap().ops.len(), 6);
    }

    // --- ChildResultV1 round-trip ---

    #[test]
    fn child_result_round_trips() {
        let json = r#"{
            "version": 1,
            "packet_id": "pkt_001",
            "assessments": [
                {
                    "item_id": "item_0",
                    "verdict": "supports",
                    "confidence": "high",
                    "claims": [
                        {"text": "certbot was invoked", "supports": ["ev1_abc"]}
                    ],
                    "missing": []
                },
                {
                    "item_id": "item_1",
                    "verdict": "irrelevant",
                    "confidence": "low",
                    "claims": [],
                    "missing": ["no renewal context"]
                }
            ]
        }"#;
        let result: ChildResultV1 = serde_json::from_str(json).unwrap();
        assert_eq!(result.assessments.len(), 2);
        assert_eq!(result.assessments[0].verdict, Verdict::Supports);
        assert_eq!(result.assessments[1].verdict, Verdict::Irrelevant);
        let rt: ChildResultV1 = serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();
        assert_eq!(result, rt);
    }

    // --- BudgetV1 ---

    #[test]
    fn budget_round_trips() {
        let budget = BudgetV1::prototype_default();
        let json = serde_json::to_string(&budget).unwrap();
        let rt: BudgetV1 = serde_json::from_str(&json).unwrap();
        assert_eq!(budget, rt);
        assert_eq!(rt.root_calls, 8);
        assert_eq!(rt.child_calls, 16);
        assert_eq!(rt.rows_returned, 2_000);
    }

    // --- FinishV1 with abstention ---

    #[test]
    fn abstained_finish_round_trips() {
        let json = r#"{
            "status": "abstained",
            "answer": "",
            "claims": [],
            "missing": ["no certificate renewal commands found"]
        }"#;
        let finish: FinishV1 = serde_json::from_str(json).unwrap();
        assert_eq!(finish.status, AnswerStatus::Abstained);
        assert!(finish.claims.is_empty());
        assert_eq!(finish.missing.len(), 1);
    }

    // --- InputRefV1 variants ---

    #[test]
    fn input_ref_handle_round_trips() {
        let json = r#"{"handle":"h_abc123"}"#;
        let r: InputRefV1 = serde_json::from_str(json).unwrap();
        assert!(matches!(&r, InputRefV1::Handle(id) if id == "h_abc123"));
    }

    #[test]
    fn input_ref_output_round_trips() {
        let json = r#"{"output":"s1"}"#;
        let r: InputRefV1 = serde_json::from_str(json).unwrap();
        assert!(matches!(&r, InputRefV1::Output(id) if id == "s1"));
    }
}
