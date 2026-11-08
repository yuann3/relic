pub mod budget;
pub mod calls;
pub mod coordinator;
pub mod evidence;
pub mod executor;
pub mod finish;
pub mod model;
pub mod protocol;
pub mod validate;

use model::Usage;
use model::egress::{PayloadAudit, Role};

/// What an investigation is doing, for a progress display; the UI maps it
/// to its own events (ADR 0009).
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    RootCall {
        turn: u32,
    },
    Op {
        op_id: String,
    },
    ChildCall {
        op_id: String,
        packet_id: String,
    },
    /// A note to the root: a rejected child result or a too-broad search.
    Note(String),
    /// A request is about to leave: what, where and how big (ADR 0004).
    Egress(PayloadAudit),
    /// A model call came back; `returned_model` is what actually answered,
    /// redacted: the provider wrote it. `usage` is the tokens it says the
    /// call took, if it says.
    Replied {
        role: Role,
        returned_model: Option<String>,
        usage: Option<Usage>,
    },
    /// A failed call is sent again, as the retry rule allows once.
    Retry {
        role: Role,
        attempt: u8,
        reason: String,
    },
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
            Self::ChildCall { op_id, packet_id } => {
                write!(f, "child call for {op_id}, {packet_id}")
            }
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
            Self::Replied {
                role: r,
                returned_model,
                ..
            } => {
                write!(
                    f,
                    "{} reply from {}",
                    role(r),
                    returned_model.as_deref().unwrap_or("an unnamed model")
                )
            }
            Self::Retry {
                role: r,
                attempt,
                reason,
            } => write!(
                f,
                "retry the {} call (attempt {attempt}): {reason}",
                role(r)
            ),
        }
    }
}
