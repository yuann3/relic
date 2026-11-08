pub mod bash;
pub mod claude;
pub mod git;
pub mod hook;
pub mod zsh;

use std::io::{self, BufRead};

use crate::domain::ObservationKind;

/// Hard cap on one record's command bytes. A longer record is cut here and
/// flagged lossy; the rest of it is read past without being buffered.
pub const RECORD_CAP: usize = 1 << 20;

/// zsh's Meta byte: on disk it precedes a byte stored XORed with 0x20.
const META: u8 = 0x83;

/// A raw parsed record before redaction or storage. Parsers yield only
/// complete records: one whose terminating newline has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRecord {
    /// What the parser read: one source can hold several kinds.
    pub kind: ObservationKind,
    /// Seconds since epoch, if the file gives one that fits.
    pub timestamp: Option<i64>,
    /// Duration in seconds, if available.
    pub duration: Option<i64>,
    /// The raw command text (not yet redacted).
    pub command: String,
    /// Byte offset in the source file where this record starts.
    pub start: u64,
    /// Byte offset just past this record's terminating newline.
    pub end: u64,
    /// `command` is not the file's exact bytes: it was cut at `RECORD_CAP`,
    /// had a dangling Meta dropped, or was not valid UTF-8.
    pub lossy: bool,
}

/// Decode a record's bytes. Invalid UTF-8 still becomes U+FFFD, but never
/// silently: it sets `lossy`.
fn decode(bytes: Vec<u8>, lossy: &mut bool) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| {
        *lossy = true;
        let mut text = String::from_utf8_lossy(e.as_bytes()).into_owned();
        // U+FFFD is three bytes for one bad byte: the cap is on what is
        // stored, so cut again.
        text.truncate(text.floor_char_boundary(RECORD_CAP));
        text
    })
}

/// What `Lines::read_into` learned about one physical line.
struct Line {
    /// The line ended with a newline rather than at end of input.
    terminated: bool,
    /// Bytes were dropped because the output buffer was at `RECORD_CAP`.
    cut: bool,
    /// A Meta byte had nothing to decode.
    dangling_meta: bool,
    /// The last content byte, known even when it was cut.
    last: Option<u8>,
}

impl Line {
    fn lossy(&self) -> bool {
        self.cut || self.dangling_meta
    }

    fn push(&mut self, out: &mut Vec<u8>, byte: u8) {
        self.last = Some(byte);
        if out.len() < RECORD_CAP {
            out.push(byte);
        } else {
            self.cut = true;
        }
    }
}

/// The byte reader both parsers sit on: physical lines with the terminator
/// and a CR before it removed, zsh's metafication undone first, and never
/// more than `RECORD_CAP` bytes held however long the line is.
struct Lines<R> {
    reader: R,
    /// Bytes consumed so far.
    offset: u64,
    unmetafy: bool,
    dangling_meta: u32,
    /// The reader failed. The bytes consumed before the failure are gone, so
    /// reading on would yield the rest of that line as a clean record.
    failed: bool,
}

impl<R: BufRead> Lines<R> {
    fn new(reader: R, unmetafy: bool) -> Self {
        Self {
            reader,
            offset: 0,
            unmetafy,
            dangling_meta: 0,
            failed: false,
        }
    }

    /// Append the next line's content to `out`. `None` at end of input, and
    /// on every call after an error.
    fn read_into(&mut self, out: &mut Vec<u8>) -> io::Result<Option<Line>> {
        if self.failed {
            return Ok(None);
        }
        let begin = self.offset;
        let mut line = Line {
            terminated: false,
            cut: false,
            dangling_meta: false,
            last: None,
        };
        let mut meta = false;
        // A CR is content unless the newline follows it.
        let mut held_cr = false;
        loop {
            let chunk = match self.reader.fill_buf() {
                Ok(chunk) => chunk,
                // A signal interrupted the syscall: nothing was read or lost,
                // so retrying is not hiding a source failure.
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.failed = true;
                    return Err(e);
                }
            };
            if chunk.is_empty() {
                break;
            }
            let mut used = 0;
            for &raw in chunk {
                used += 1;
                let byte = if raw == b'\n' {
                    // zsh never metafies a newline, so Meta before one is
                    // dangling. Decoding the pair to `*` instead would swallow
                    // a record boundary and shift every later offset.
                    line.dangling_meta |= meta;
                    line.terminated = true;
                    break;
                } else if meta {
                    meta = false;
                    raw ^ 0x20
                } else if self.unmetafy && raw == META {
                    meta = true;
                    continue;
                } else {
                    raw
                };
                if held_cr {
                    line.push(out, b'\r');
                }
                held_cr = byte == b'\r';
                if !held_cr {
                    line.push(out, byte);
                }
            }
            self.reader.consume(used);
            self.offset += used as u64;
            if line.terminated {
                break;
            }
        }
        if !line.terminated {
            if self.offset == begin {
                return Ok(None);
            }
            line.dangling_meta |= meta;
            if held_cr {
                line.push(out, b'\r');
            }
        }
        if line.dangling_meta {
            self.dangling_meta = self.dangling_meta.saturating_add(1);
        }
        Ok(Some(line))
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::io::{self, Read};

    /// Serves `data`, then fails every later read: a source that goes away
    /// partway through.
    pub struct FailsAfter<'a>(pub &'a [u8]);

    impl Read for FailsAfter<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Err(io::Error::other("source went away"));
            }
            self.0.read(buf)
        }
    }

    /// Serves `before`, fails one read, then serves `after`: a source that
    /// recovers, which a parser must not resume from.
    pub struct FailsOnce<'a> {
        pub before: &'a [u8],
        pub after: &'a [u8],
        pub failed: bool,
    }

    impl Read for FailsOnce<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.before.is_empty() {
                return self.before.read(buf);
            }
            if !self.failed {
                self.failed = true;
                return Err(io::Error::other("transient failure"));
            }
            self.after.read(buf)
        }
    }
}
