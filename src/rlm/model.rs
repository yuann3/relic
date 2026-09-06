use crate::rlm::protocol::*;

/// Packet of evidence items sent to a child model call.
#[derive(Debug, Clone)]
pub struct EvidencePacket {
    pub packet_id: String,
    pub items: Vec<EvidenceItem>,
}

#[derive(Debug, Clone)]
pub struct EvidenceItem {
    pub item_id: String,
    pub evidence_id: String,
    pub safe_text: String,
}

/// Trait for the model boundary. Implemented by FakeModel for tests.
pub trait ModelBoundary {
    fn child_call(
        &self,
        packet: &EvidencePacket,
        objective: &str,
    ) -> Result<ChildResultV1, ModelError>;

    fn root_call(
        &self,
        state_json: &str,
    ) -> Result<RootActionV1, ModelError>;
}

#[derive(Debug)]
pub enum ModelError {
    Refused(String),
    InvalidResponse(String),
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for ModelError {}

type ChildHandler = Box<dyn Fn(&EvidencePacket) -> ChildResultV1 + Send>;

/// A fake model that returns scripted responses for testing.
#[derive(Default)]
pub struct FakeModel {
    child_responses: std::sync::Mutex<Vec<ChildHandler>>,
    root_responses: std::sync::Mutex<Vec<RootActionV1>>,
}

impl FakeModel {
    pub fn new() -> Self {
        Self {
            child_responses: std::sync::Mutex::new(Vec::new()),
            root_responses: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Add a child response generator. Called in FIFO order for each batch.
    pub fn add_child_response<F>(&self, f: F)
    where
        F: Fn(&EvidencePacket) -> ChildResultV1 + Send + 'static,
    {
        self.child_responses.lock().unwrap().push(Box::new(f));
    }

    /// Add a root response. Returned in FIFO order for each root call.
    pub fn add_root_response(&self, action: RootActionV1) {
        self.root_responses.lock().unwrap().push(action);
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
                        text: format!("Evidence found: {}", &item.safe_text[..item.safe_text.len().min(50)]),
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
    fn child_call(
        &self,
        packet: &EvidencePacket,
        _objective: &str,
    ) -> Result<ChildResultV1, ModelError> {
        let mut responses = self.child_responses.lock().unwrap();
        if responses.is_empty() {
            return Err(ModelError::Refused("no more scripted child responses".into()));
        }
        let handler = responses.remove(0);
        Ok(handler(packet))
    }

    fn root_call(
        &self,
        _state_json: &str,
    ) -> Result<RootActionV1, ModelError> {
        let mut responses = self.root_responses.lock().unwrap();
        if responses.is_empty() {
            return Err(ModelError::Refused("no more scripted root responses".into()));
        }
        Ok(responses.remove(0))
    }
}
