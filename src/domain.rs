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

/// Which observation fields a revision's source file itself encodes. A field
/// outside the set is absent for that revision, never inferred. One value per
/// revision instead of a blob per row (ADR 0008); a single row still says
/// "no time" with a NULL `observed_at_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provenance(u8);

impl Provenance {
    pub const COMMAND: Self = Self(1);
    pub const OBSERVED_AT: Self = Self(2);

    pub fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn has(self, field: Self) -> bool {
        self.0 & field.0 == field.0
    }

    pub fn as_i64(self) -> i64 {
        self.0 as i64
    }

    pub fn from_i64(v: i64) -> Self {
        Self(v as u8)
    }
}

/// Bit in `Observation::flags`: `safe_text` is not the source's exact text.
/// The record was cut at the per-record cap, had a dangling zsh Meta byte
/// dropped, or was not valid UTF-8.
pub const FLAG_LOSSY: i64 = 1;

/// A sanitized observation ready for storage. The open revision assigns its
/// revision and ordinal.
#[derive(Debug, Clone)]
pub struct Observation {
    pub kind: ObservationKind,
    pub observed_at_ms: Option<i64>,
    pub safe_text: String,
    pub flags: i64,
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
    pub flags: i64,
    pub locator: Vec<u8>,
}

/// A ranked search hit from FTS.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: i64,
    pub observed_at_ms: Option<i64>,
    /// For an untimed hit: see [`crate::store::Store::observed_no_later_than`].
    pub observed_before_ms: Option<i64>,
    pub kind: ObservationKind,
    pub safe_text: String,
    pub score: f64,
}
