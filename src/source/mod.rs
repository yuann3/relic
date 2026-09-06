pub mod zsh;
pub mod bash;

/// A raw parsed record before redaction or storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRecord {
    /// Seconds since epoch, if available.
    pub timestamp: Option<i64>,
    /// Duration in seconds, if available.
    pub duration: Option<i64>,
    /// The raw command text (not yet redacted).
    pub command: String,
    /// Byte offset in the source file where this record started.
    pub byte_offset: usize,
}
