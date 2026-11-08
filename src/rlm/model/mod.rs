pub mod chat;
pub mod egress;
pub mod prompt;

use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

use crate::redact::Redactor;
use crate::rlm::protocol::*;
use egress::{EgressRequest, Role, Target};

/// Packet of evidence items sent to a child model call.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvidencePacket {
    pub packet_id: String,
    pub items: Vec<EvidenceItem>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvidenceItem {
    pub item_id: String,
    pub evidence_id: String,
    pub safe_text: String,
    pub truncation: Option<TruncationV1>,
}

/// The model boundary: one request, one attempt. Retrying is the kernel's
/// rule, the same for every adapter (ADR 0010).
pub trait ModelBoundary: Send + Sync {
    /// Where requests go, for the payload audit.
    fn target(&self) -> Target;

    /// The model's reply body, unparsed: the kernel caps and checks it.
    /// Must give up by `deadline`.
    fn call(&self, req: &EgressRequest, deadline: Instant) -> Result<Reply, ModelError>;

    /// What a token costs, when the profile says; `None` leaves the cost
    /// unknown rather than zero.
    fn prices(&self) -> Option<Prices> {
        None
    }
}

/// Micro-US-dollars per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prices {
    pub input_per_mtok: u64,
    pub output_per_mtok: u64,
}

impl Prices {
    /// Micro-US-dollars for these token counts, rounded up.
    pub fn cost(self, input_tokens: u64, output_tokens: u64) -> u64 {
        input_tokens
            .saturating_mul(self.input_per_mtok)
            .saturating_add(output_tokens.saturating_mul(self.output_per_mtok))
            .div_ceil(1_000_000)
    }
}

/// One completed model call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reply {
    pub body: String,
    /// What the provider says answered, which an alias may hide.
    pub returned_model: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// How a call failed, in the classes the retry rule tells apart.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelError {
    /// Failed before any request byte was sent; nothing can have run.
    BeforeSend(String),
    /// 429: the provider refused the request without running it.
    RateLimited {
        retry_after: Option<Duration>,
    },
    /// Any other non-2xx status, 5xx and 529 included: it may have run.
    /// `message` is what the endpoint said, cut short and redacted.
    Status {
        code: u16,
        message: Option<String>,
    },
    /// Lost after the request went out (disconnect, timeout): it may have run.
    Ambiguous(String),
    /// The response body was over the transport's cap.
    Oversize,
    /// A 2xx whose body is not a Chat Completions response with one choice.
    InvalidResponse(String),
    Refused(String),
    /// Cut off at the output-token limit (`finish_reason: length`).
    Truncated,
}

impl ModelError {
    /// How long to wait before the one retry the rule allows, if it allows
    /// one: before send at once, a 429 only with a `Retry-After` within `max`.
    pub fn retry_after(&self, max: Duration) -> Option<Duration> {
        match self {
            Self::BeforeSend(_) => Some(Duration::ZERO),
            Self::RateLimited {
                retry_after: Some(d),
            } if *d <= max => Some(*d),
            _ => None,
        }
    }
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeSend(e) => write!(f, "could not reach the model: {e}"),
            Self::RateLimited {
                retry_after: Some(d),
            } => write!(f, "rate limited; retry after {} s", d.as_secs()),
            Self::RateLimited { retry_after: None } => f.write_str("rate limited"),
            Self::Status {
                code,
                message: None,
            } => write!(f, "model endpoint answered HTTP {code}"),
            Self::Status {
                code,
                message: Some(m),
            } => write!(f, "model endpoint answered HTTP {code}: {m}"),
            Self::Ambiguous(e) => write!(f, "lost the model call after sending it: {e}"),
            Self::Oversize => f.write_str("model response over the size cap"),
            Self::InvalidResponse(e) => write!(f, "not a chat completion: {e}"),
            Self::Refused(e) => write!(f, "the model refused: {e}"),
            Self::Truncated => f.write_str("the model hit its output-token limit"),
        }
    }
}

impl std::error::Error for ModelError {}

/// Why a model's JSON output was refused before any check of its meaning.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputError {
    Oversize { bytes: usize },
    Malformed(String),
}

impl std::fmt::Display for OutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Oversize { bytes } => write!(f, "{bytes} bytes, over the cap"),
            Self::Malformed(e) => write!(f, "malformed: {e}"),
        }
    }
}

/// Parse a model's JSON output: refused over `max` bytes before parsing, every
/// string redacted before anything stores or shows it (ADR 0004), then
/// typed, which refuses unknown fields.
pub fn parse_output<T: DeserializeOwned>(raw: &str, max: usize) -> Result<T, OutputError> {
    if raw.len() > max {
        return Err(OutputError::Oversize { bytes: raw.len() });
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| OutputError::Malformed(e.to_string()))?;
    let value = egress::redact_strings(value, &Redactor::new());
    serde_json::from_value(value).map_err(|e| OutputError::Malformed(e.to_string()))
}

type ChildHandler = Box<dyn Fn(&EvidencePacket) -> String + Send>;
type RootHandler = Box<dyn Fn(&serde_json::Value) -> String + Send>;

/// A fake model that returns scripted responses for testing.
#[derive(Default)]
pub struct FakeModel {
    child_responses: std::sync::Mutex<Vec<ChildHandler>>,
    root_responses: std::sync::Mutex<Vec<RootHandler>>,
    requests: std::sync::Mutex<Vec<EgressRequest>>,
}

impl FakeModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every root state it was shown, in order.
    pub fn root_states(&self) -> Vec<String> {
        let requests = self.requests.lock().unwrap();
        requests
            .iter()
            .filter(|r| r.role() == Role::Root)
            .map(|r| r.user().to_string())
            .collect()
    }

    /// Every request it was sent, in order.
    pub fn requests(&self) -> Vec<EgressRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// Add a child response generator. Called in FIFO order for each batch.
    pub fn add_child_response<F>(&self, f: F)
    where
        F: Fn(&EvidencePacket) -> ChildResultV1 + Send + 'static,
    {
        self.add_child_raw(move |p| serde_json::to_string(&f(p)).unwrap());
    }

    /// Add a child reply body exactly as the model would send it.
    pub fn add_child_raw<F>(&self, f: F)
    where
        F: Fn(&EvidencePacket) -> String + Send + 'static,
    {
        self.child_responses.lock().unwrap().push(Box::new(f));
    }

    /// Add a root response. Returned in FIFO order for each root call.
    pub fn add_root_response(&self, action: RootActionV1) {
        self.add_root_raw(serde_json::to_string(&action).unwrap());
    }

    /// Add a root reply body exactly as the model would send it.
    pub fn add_root_raw(&self, body: String) {
        self.add_root_fn(move |_| body.clone());
    }

    /// Add a root reply body made from the root state it is shown, so a
    /// finish can cite the evidence ids of this investigation.
    pub fn add_root_fn<F>(&self, f: F)
    where
        F: Fn(&serde_json::Value) -> String + Send + 'static,
    {
        self.root_responses.lock().unwrap().push(Box::new(f));
    }

    /// Default child handler: mark all items as "supports" with high confidence.
    pub fn default_supports_handler() -> impl Fn(&EvidencePacket) -> ChildResultV1 + Send + 'static
    {
        move |packet: &EvidencePacket| ChildResultV1 {
            version: 1,
            packet_id: packet.packet_id.clone(),
            assessments: packet
                .items
                .iter()
                .map(|item| ItemAssessmentV1 {
                    item_id: item.item_id.clone(),
                    verdict: Verdict::Supports,
                    confidence: Confidence::High,
                    claims: vec![ClaimV1 {
                        text: format!(
                            "Evidence found: {}",
                            &item.safe_text[..item.safe_text.len().min(50)]
                        ),
                        supports: vec![item.evidence_id.clone()],
                        contradicts: vec![],
                    }],
                    missing: vec![],
                })
                .collect(),
        }
    }
}

impl ModelBoundary for FakeModel {
    fn target(&self) -> Target {
        Target {
            profile: "fake".into(),
            origin: "fake:".into(),
        }
    }

    fn call(&self, req: &EgressRequest, _deadline: Instant) -> Result<Reply, ModelError> {
        self.requests.lock().unwrap().push(req.clone());
        let body = match req.role() {
            Role::Root => {
                let mut responses = self.root_responses.lock().unwrap();
                if responses.is_empty() {
                    return Err(ModelError::Refused(
                        "no more scripted root responses".into(),
                    ));
                }
                let state =
                    serde_json::from_str(req.user()).expect("a root request holds the root state");
                responses.remove(0)(&state)
            }
            Role::Child => {
                #[derive(serde::Deserialize)]
                struct Sent {
                    packet: EvidencePacket,
                }
                let sent: Sent =
                    serde_json::from_str(req.user()).expect("a child request holds a packet");
                let mut responses = self.child_responses.lock().unwrap();
                if responses.is_empty() {
                    return Err(ModelError::Refused(
                        "no more scripted child responses".into(),
                    ));
                }
                responses.remove(0)(&sent.packet)
            }
        };
        Ok(Reply {
            body,
            returned_model: Some("fake".into()),
            usage: None,
        })
    }
}
