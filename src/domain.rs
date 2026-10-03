/// Kind of observation source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ObservationKind {
    ZshCommand = 1,
    BashCommand = 2,
    HookFrame = 3,
    AgentMessage = 4,
}

impl ObservationKind {
    pub fn as_i64(self) -> i64 {
        self as i64
    }

    pub fn from_i64(v: i64) -> Option<Self> {
        match v {
            1 => Some(Self::ZshCommand),
            2 => Some(Self::BashCommand),
            3 => Some(Self::HookFrame),
            4 => Some(Self::AgentMessage),
            _ => None,
        }
    }
}

/// A sanitized observation ready for storage.
#[derive(Debug, Clone)]
pub struct Observation {
    pub revision_id: i64,
    pub ordinal: i64,
    pub kind: ObservationKind,
    pub observed_at_ms: Option<i64>,
    pub safe_text: String,
    pub safe_hash: Vec<u8>,
    pub keyed_fingerprint: Vec<u8>,
    pub flags: i64,
    pub provenance: Vec<u8>,
    pub locator: Vec<u8>,
}

/// An observation retrieved from the store, with its row ID.
#[derive(Debug, Clone)]
pub struct StoredObservation {
    pub id: i64,
    pub revision_id: i64,
    pub ordinal: i64,
    pub kind: ObservationKind,
    pub observed_at_ms: Option<i64>,
    pub safe_text: String,
    pub safe_hash: Vec<u8>,
    pub keyed_fingerprint: Vec<u8>,
    pub flags: i64,
    pub provenance: Vec<u8>,
    pub locator: Vec<u8>,
}

/// A ranked search hit from FTS.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: i64,
    pub revision_id: i64,
    pub ordinal: i64,
    pub observed_at_ms: Option<i64>,
    pub kind: ObservationKind,
    pub safe_text: String,
    pub score: f64,
}
