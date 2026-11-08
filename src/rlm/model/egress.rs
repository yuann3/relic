//! The one way text leaves for a model (ADR 0004). An adapter takes only an
//! [`EgressRequest`], and its constructor is where every rule is applied:
//! redaction, the per-request cap, the egress-byte reservation and the audit.

use serde_json::{Value, json};

use super::prompt;
use crate::redact::{REDACTION_REVISION, Redactor};
use crate::rlm::budget::{BudgetExhausted, BudgetLedger, Reservation};

/// Most bytes of a root request, instructions and schema included.
pub const ROOT_MAX: usize = 64 * 1024;
/// Most bytes of a child request, instructions and schema included.
pub const CHILD_MAX: usize = 24 * 1024;
/// Room for what an adapter wraps around the messages and schema: the model
/// name, the output-mode and token-limit fields.
const WRAPPER_BYTES: usize = 512;
/// How much of the user message the audit keeps for display.
const PREVIEW_CHARS: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Root,
    Child,
}

impl Role {
    fn system(self) -> &'static str {
        match self {
            Role::Root => prompt::ROOT_SYSTEM,
            Role::Child => prompt::CHILD_SYSTEM,
        }
    }

    fn schema(self) -> Value {
        match self {
            Role::Root => prompt::root_schema(),
            Role::Child => prompt::child_schema(),
        }
    }

    fn max(self) -> usize {
        match self {
            Role::Root => ROOT_MAX,
            Role::Child => CHILD_MAX,
        }
    }

    /// The research's per-call output-token maxima.
    pub fn max_output_tokens(self) -> u32 {
        match self {
            Role::Root => 1536,
            Role::Child => 768,
        }
    }
}

/// Where a request goes, as the audit records it. Never a secret.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Target {
    pub profile: String,
    /// Scheme, host and port; no path, query or userinfo.
    pub origin: String,
}

/// What left, recorded before it is sent (research, security boundary 2).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PayloadAudit {
    pub profile: String,
    pub origin: String,
    pub role: Role,
    pub bytes: usize,
    pub request_hash: String,
    pub evidence_ids: Vec<String>,
    pub redaction_revision: &'static str,
    pub preview: String,
}

#[derive(Debug, PartialEq)]
pub enum EgressError {
    TooLarge { bytes: usize, max: usize },
    Budget(BudgetExhausted),
}

impl std::fmt::Display for EgressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { bytes, max } => write!(
                f,
                "model request of {bytes} bytes is over the {max}-byte cap"
            ),
            Self::Budget(b) => write!(f, "model egress budget: {b}"),
        }
    }
}

impl std::error::Error for EgressError {}

/// A model request that has been redacted, capped, paid for and audited.
#[derive(Debug, Clone)]
pub struct EgressRequest {
    role: Role,
    user: String,
    schema: Value,
    bytes: usize,
}

impl EgressRequest {
    /// `payload` becomes the user message. Every string in it goes through
    /// the current redactor again; keys are the host's and stay as they are.
    pub fn new(
        role: Role,
        payload: Value,
        target: &Target,
        ledger: &mut BudgetLedger,
        audit: &mut dyn FnMut(PayloadAudit),
    ) -> Result<Self, EgressError> {
        let payload = redact_strings(payload, &Redactor::new());
        let user = payload.to_string();
        let schema = role.schema();
        let bytes = encoded_bytes(role, &user, &schema);
        if bytes > role.max() {
            return Err(EgressError::TooLarge {
                bytes,
                max: role.max(),
            });
        }
        ledger
            .reserve(&Reservation {
                model_egress_bytes: bytes as u64,
                ..Default::default()
            })
            .map_err(EgressError::Budget)?;
        let mut evidence_ids = Vec::new();
        collect_evidence_ids(&payload, &mut evidence_ids);
        audit(PayloadAudit {
            profile: target.profile.clone(),
            origin: target.origin.clone(),
            role,
            bytes,
            request_hash: crate::rlm::evidence::hex_hash(
                format!("{}\0{user}", role.system()).as_bytes(),
            ),
            evidence_ids,
            redaction_revision: REDACTION_REVISION,
            preview: user.chars().take(PREVIEW_CHARS).collect(),
        });
        Ok(Self {
            role,
            user,
            schema,
            bytes,
        })
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn system(&self) -> &'static str {
        self.role.system()
    }

    /// The redacted JSON the model is shown.
    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn schema(&self) -> &Value {
        &self.schema
    }

    /// What the request counted against the egress budget.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Bytes a request for `payload` counts against its cap. The host sizes its
/// packets with this before building them; already-redacted text redacts
/// to itself, so the count does not change in [`EgressRequest::new`].
pub fn request_bytes(role: Role, payload: &Value) -> usize {
    encoded_bytes(role, &payload.to_string(), &role.schema())
}

fn encoded_bytes(role: Role, user: &str, schema: &Value) -> usize {
    let messages = json!([
        {"role": "system", "content": role.system()},
        {"role": "user", "content": user},
    ]);
    messages.to_string().len() + schema.to_string().len() + WRAPPER_BYTES
}

/// Every string leaf through `redactor`. Evidence ids, handles and packet ids
/// are host-made and redact to themselves; if one ever did not, the model's
/// citations of it would fail the strict checks, closed.
pub fn redact_strings(value: Value, redactor: &Redactor) -> Value {
    match value {
        Value::String(s) => Value::String(redactor.redact(&s)),
        Value::Array(a) => {
            Value::Array(a.into_iter().map(|v| redact_strings(v, redactor)).collect())
        }
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, v)| (k, redact_strings(v, redactor)))
                .collect(),
        ),
        other => other,
    }
}

fn collect_evidence_ids(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(o) => {
            for (k, v) in o {
                match (k.as_str(), v) {
                    ("evidence_id", Value::String(id)) => out.push(id.clone()),
                    _ => collect_evidence_ids(v, out),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|v| collect_evidence_ids(v, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rlm::protocol::BudgetV1;

    const SECRET: &str = "hunter2hunter2";

    fn target() -> Target {
        Target {
            profile: "openai".into(),
            origin: "https://api.example.com".into(),
        }
    }

    #[test]
    fn a_request_is_redacted_counted_and_audited_before_it_leaves() {
        let payload = json!({
            "objective": format!("mysql --password={SECRET}"),
            "packet": {"packet_id": "pkt_00", "items": [{"item_id": "item_0", "evidence_id": "ev1_00112233aabbccdd",
                "safe_text": format!("curl -u admin:{SECRET} https://x.example"), "truncation": null}]},
        });
        let mut ledger = BudgetLedger::new(BudgetV1::prototype_default());
        let mut audits = Vec::new();
        let req = EgressRequest::new(Role::Child, payload, &target(), &mut ledger, &mut |a| {
            audits.push(a)
        })
        .unwrap();

        assert!(!req.user().contains(SECRET), "{}", req.user());
        assert!(req.user().contains("[REDACTED:"), "{}", req.user());
        // Host ids come through unchanged, so citations of them still resolve.
        for id in ["pkt_00", "item_0", "ev1_00112233aabbccdd"] {
            assert!(req.user().contains(id), "{id} lost from {}", req.user());
        }
        let [audit] = audits.as_slice() else {
            panic!("one audit, got {audits:?}")
        };
        assert_eq!(
            (audit.role, audit.bytes, audit.evidence_ids.as_slice()),
            (
                Role::Child,
                req.bytes(),
                &["ev1_00112233aabbccdd".to_string()][..]
            )
        );
        assert_eq!(
            (audit.profile.as_str(), audit.origin.as_str()),
            ("openai", "https://api.example.com")
        );
        assert!(!audit.preview.contains(SECRET));
        let remaining = ledger.remaining().model_egress_bytes;
        assert_eq!(
            remaining,
            BudgetV1::prototype_default().model_egress_bytes - req.bytes() as u64
        );
        // The count covers the instructions and schema, not just the payload.
        assert!(req.bytes() > CHILD_SYSTEM_AND_SCHEMA_FLOOR);
    }

    /// The instructions and schema alone are well over a kilobyte; a count
    /// of the payload only would come in under this.
    const CHILD_SYSTEM_AND_SCHEMA_FLOOR: usize = 1500;

    #[test]
    fn a_request_over_its_cap_or_the_budget_never_leaves() {
        let mut ledger = BudgetLedger::new(BudgetV1::prototype_default());
        let mut audits = 0;
        let big = json!({"objective": "x".repeat(CHILD_MAX)});
        let err = EgressRequest::new(
            Role::Child,
            big.clone(),
            &target(),
            &mut ledger,
            &mut |_| audits += 1,
        )
        .unwrap_err();
        assert!(
            matches!(err, EgressError::TooLarge { max: CHILD_MAX, .. }),
            "{err}"
        );
        // The same payload fits a root request, whose cap is larger.
        EgressRequest::new(Role::Root, big, &target(), &mut ledger, &mut |_| {
            audits += 1
        })
        .unwrap();
        assert_eq!(audits, 1);

        let mut budget = BudgetV1::prototype_default();
        budget.model_egress_bytes = 1000;
        let mut ledger = BudgetLedger::new(budget);
        let err = EgressRequest::new(Role::Child, json!({}), &target(), &mut ledger, &mut |_| {
            audits += 1
        })
        .unwrap_err();
        assert_eq!(err, EgressError::Budget(BudgetExhausted::ModelEgressBytes));
        assert_eq!(audits, 1, "a refused request is not audited as sent");
    }
}
