//! Optional fzf picker over one bounded [`SearchPage`].
//!
//! Contract (docs/research/tui-fzf-integration.md, "fzf contract"): fixed argv,
//! no shell, no query or history text in argv, NUL-framed records on stdin,
//! at most one bounded record back on stdout, and the returned id must be a
//! member of the page. Display text from fzf is discarded.

use crate::app::SearchPage;

#[derive(Debug)]
pub enum PickError {
    /// fzf is not on PATH. Callers fall back to indexed search.
    Unavailable,
    /// fzf is present but older than the tested minimum.
    TooOld { found: String, minimum: &'static str },
    Io(std::io::Error),
    /// fzf answered with something outside the protocol.
    Protocol(String),
}

impl std::fmt::Display for PickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => write!(f, "fzf unavailable; using indexed search"),
            Self::TooOld { found, minimum } => {
                write!(f, "fzf {found} is older than the supported {minimum}")
            }
            Self::Io(e) => write!(f, "fzf failed: {e}"),
            Self::Protocol(m) => write!(f, "fzf returned an invalid selection: {m}"),
        }
    }
}

impl std::error::Error for PickError {}

/// Let the user pick one hit. `Ok(None)` means they cancelled.
pub fn pick(_page: &SearchPage) -> Result<Option<i64>, PickError> {
    Err(PickError::Unavailable)
}
