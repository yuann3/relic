use std::io::{self, BufRead};

use super::{Lines, RECORD_CAP, RawRecord, decode};
use crate::domain::ObservationKind;

/// v2: unmetafied, CRLF-tolerant, capped per record, whitespace-only
/// commands dropped, trailing partial record withheld, the space zsh pads a
/// final backslash with removed.
pub const PARSER_CONTRACT: &str = "zsh-v2";

/// Streams zsh history records (plain and extended format).
///
/// Extended format: `: <timestamp>:<duration>;<command>`
/// Plain format: `<command>` (one per logical line)
/// Backslash-newline joins continuation lines into one logical record.
pub struct Parser<R> {
    lines: Lines<R>,
    stable_len: u64,
    incomplete_tail: Option<RawRecord>,
}

impl<R: BufRead> Parser<R> {
    pub fn new(reader: R) -> Self {
        Self {
            lines: Lines::new(reader, true),
            stable_len: 0,
            incomplete_tail: None,
        }
    }

    /// Byte end of the last complete record read so far, stored or not.
    pub fn stable_len(&self) -> u64 {
        self.stable_len
    }

    /// The input's last record once the input is exhausted, if it ended
    /// without its newline or on a dangling continuation. Never yielded: the
    /// shell may still be writing it.
    pub fn incomplete_tail(&self) -> Option<&RawRecord> {
        self.incomplete_tail.as_ref()
    }

    /// How many Meta bytes had nothing to decode.
    pub fn dangling_meta(&self) -> u32 {
        self.lines.dangling_meta
    }
}

impl<R: BufRead> Iterator for Parser<R> {
    type Item = io::Result<RawRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let start = self.lines.offset;
            let mut bytes = Vec::new();
            let mut lossy = false;
            let complete = loop {
                let line = match self.lines.read_into(&mut bytes) {
                    Ok(Some(line)) => line,
                    Ok(None) => break false,
                    Err(e) => return Some(Err(e)),
                };
                lossy |= line.lossy();
                if !line.terminated {
                    break false;
                }
                if line.last != Some(b'\\') {
                    // zsh writes a command ending in `\` as `\ ` so the
                    // backslash is not read as a continuation, and when it
                    // reads the file back drops one space from a line ending
                    // in a backslash and spaces.
                    let spaces = bytes.iter().rev().take_while(|&&b| b == b' ').count();
                    if !line.cut && spaces > 0 && bytes[..bytes.len() - spaces].ends_with(b"\\") {
                        bytes.pop();
                    }
                    break true;
                }
                // A cut line's backslash was never buffered.
                if !line.cut {
                    bytes.pop();
                }
                if bytes.len() < RECORD_CAP {
                    bytes.push(b'\n');
                } else {
                    lossy = true;
                }
            };
            let end = self.lines.offset;
            if end == start {
                return None;
            }
            let record = record(bytes, start, end, lossy);
            if !complete {
                self.incomplete_tail = Some(record);
                return None;
            }
            self.stable_len = end;
            if !record.command.trim().is_empty() {
                return Some(Ok(record));
            }
        }
    }
}

fn record(bytes: Vec<u8>, start: u64, end: u64, mut lossy: bool) -> RawRecord {
    let line = decode(bytes, &mut lossy);
    match split_extended(&line) {
        Some((timestamp, duration, command)) => RawRecord {
            kind: ObservationKind::ZshCommand,
            timestamp,
            duration,
            command: command.to_string(),
            start,
            end,
            lossy,
        },
        None => RawRecord {
            kind: ObservationKind::ZshCommand,
            timestamp: None,
            duration: None,
            command: line,
            start,
            end,
            lossy,
        },
    }
}

/// Whether a line is in zsh's extended-history format.
pub fn is_extended(line: &[u8]) -> bool {
    std::str::from_utf8(line).is_ok_and(|line| split_extended(line).is_some())
}

/// Split `: <timestamp>:<duration>;<command>`. A field of digits too long
/// for an i64 is absent rather than a reason to store the prefix as command
/// text.
fn split_extended(line: &str) -> Option<(Option<i64>, Option<i64>, &str)> {
    let (timestamp, rest) = line.strip_prefix(": ")?.split_once(':')?;
    let (duration, command) = rest.split_once(';')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(timestamp) || !digits(duration) {
        return None;
    }
    Some((timestamp.parse().ok(), duration.parse().ok(), command))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::testing::{FailsAfter, FailsOnce};
    use std::io::BufReader;

    fn parse(input: &[u8]) -> Vec<RawRecord> {
        Parser::new(input).map(Result::unwrap).collect()
    }

    fn commands(input: &[u8]) -> Vec<String> {
        parse(input).into_iter().map(|r| r.command).collect()
    }

    #[test]
    fn parse_extended_commands() {
        let records = parse(b": 1700000000:0;cargo build\n: 1700000001:5;cargo test\n");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].timestamp, Some(1_700_000_000));
        assert_eq!(records[0].duration, Some(0));
        assert_eq!(records[0].command, "cargo build");
        assert_eq!(records[1].duration, Some(5));
        assert_eq!(records[1].command, "cargo test");
    }

    #[test]
    fn parse_plain_format() {
        let records = parse(b"ls -la\ncd /tmp\n");
        assert_eq!(records[0].command, "ls -la");
        assert_eq!(records[0].timestamp, None);
        assert_eq!(records[1].command, "cd /tmp");
    }

    #[test]
    fn parse_backslash_newline_continuation() {
        assert_eq!(
            commands(b": 1700000000:0;echo hello \\\nworld\n"),
            ["echo hello \nworld"]
        );
    }

    #[test]
    fn parse_mixed_extended_and_plain() {
        let records = parse(b": 1700000000:0;cargo build\nplain command\n");
        assert_eq!(records[0].timestamp, Some(1_700_000_000));
        assert_eq!(records[1].timestamp, None);
        assert_eq!(records[1].command, "plain command");
    }

    #[test]
    fn parse_empty_input() {
        assert!(parse(b"").is_empty());
    }

    #[test]
    fn records_carry_their_start_and_end_offsets() {
        let records = parse(b": 1700000000:0;first\n\n: 1700000001:0;se\\\ncond\n");
        assert_eq!((records[0].start, records[0].end), (0, 21));
        assert_eq!((records[1].start, records[1].end), (22, 46));
    }

    #[test]
    fn parse_malformed_extended_falls_back_to_plain() {
        let records = parse(b": 1700000000:0 missing semicolon\n");
        assert_eq!(records[0].timestamp, None);
        assert_eq!(records[0].command, ": 1700000000:0 missing semicolon");
    }

    #[test]
    fn yields_the_first_record_before_the_source_fails() {
        let input = b": 1700000000:0;first\n: 1700000001:0;second\n: 1700000002:0;third\n";
        // The source dies inside the second record. An 8-byte buffer: the
        // parser cannot have read ahead to the failure.
        let mut parser = Parser::new(BufReader::with_capacity(8, FailsAfter(&input[..30])));
        assert_eq!(parser.next().unwrap().unwrap().command, "first");
        assert!(parser.next().unwrap().is_err());
        assert_eq!(parser.stable_len(), 21);
    }

    #[test]
    fn nothing_is_yielded_after_the_source_fails_even_if_it_recovers() {
        let input = b"first\nsecond record\nthird\n";
        let source = FailsOnce {
            before: &input[..12],
            after: &input[12..],
            failed: false,
        };
        let mut parser = Parser::new(BufReader::with_capacity(4, source));
        assert_eq!(parser.next().unwrap().unwrap().command, "first");
        assert!(parser.next().unwrap().is_err());
        assert!(parser.next().is_none());
        assert_eq!(parser.stable_len(), 6);
        assert!(parser.incomplete_tail().is_none());
    }

    #[test]
    fn the_space_zsh_pads_a_final_backslash_with_is_dropped() {
        // Only the one space zsh added, whatever precedes the backslash.
        assert_eq!(
            commands(
                b": 1700000000:0;echo a\\ \necho b\\   \necho c\\\\ \necho d  \necho e\\ x \n"
            ),
            [
                "echo a\\",
                "echo b\\  ",
                "echo c\\\\",
                "echo d  ",
                "echo e\\ x "
            ]
        );
    }

    #[test]
    fn meta_pairs_decode_before_utf8() {
        // zsh 5.9 writes U+90E8 (E9 83 A8) as E9 83 A3 A8.
        let records = parse(b": 1700000000:0;echo \xE9\x83\xA3\xA8\n");
        assert_eq!(records[0].command, "echo \u{90E8}");
        assert!(!records[0].lossy);
    }

    #[test]
    fn a_metafied_backslash_or_cr_is_handled_after_decoding() {
        // Meta 0x7C is `\`, Meta 0x2D is CR.
        assert_eq!(
            commands(b"echo a\x83\x7C\nb\x83\x2D\nnext\n"),
            ["echo a\nb", "next"]
        );
    }

    #[test]
    fn meta_at_eof_warns_and_marks_the_withheld_record_lossy() {
        let mut parser = Parser::new(&b"echo ok\necho \x83"[..]);
        assert_eq!(
            parser
                .by_ref()
                .map(|r| r.unwrap().command)
                .collect::<Vec<_>>(),
            ["echo ok"]
        );
        assert_eq!(parser.dangling_meta(), 1);
        let tail = parser.incomplete_tail().unwrap();
        assert_eq!(tail.command, "echo ");
        assert!(tail.lossy);
        assert_eq!(parser.stable_len(), 8);
    }

    #[test]
    fn meta_before_a_newline_is_dangling_and_keeps_the_record_boundary() {
        let mut parser = Parser::new(&b"echo a\x83\necho b\n"[..]);
        let records: Vec<_> = parser.by_ref().map(Result::unwrap).collect();
        assert_eq!(records[0].command, "echo a");
        assert!(records[0].lossy);
        assert_eq!(records[1].command, "echo b");
        assert!(!records[1].lossy);
        assert_eq!(parser.dangling_meta(), 1);
    }

    #[test]
    fn invalid_utf8_is_flagged_lossy() {
        let records = parse(b"echo \xFF\n");
        assert_eq!(records[0].command, "echo \u{FFFD}");
        assert!(records[0].lossy);
    }

    #[test]
    fn an_oversized_undecodable_record_is_stored_within_the_cap() {
        let mut input = vec![0xFF; RECORD_CAP + 16];
        input.extend_from_slice(b"\nnext\n");
        let records = parse(&input);
        assert!(records[0].lossy);
        assert!(records[0].command.len() <= RECORD_CAP);
        assert!(records[0].command.len() > RECORD_CAP - 3);
        assert_eq!(records[1].command, "next");
    }

    #[test]
    fn a_record_without_its_newline_is_withheld() {
        let mut parser = Parser::new(&b": 1700000000:0;done\n: 1700000001:0;cargo bui"[..]);
        assert_eq!(parser.by_ref().count(), 1);
        assert_eq!(parser.stable_len(), 20);
        assert_eq!(parser.incomplete_tail().unwrap().command, "cargo bui");
    }

    #[test]
    fn a_dangling_continuation_is_withheld() {
        let mut parser = Parser::new(&b"done\necho one \\\n"[..]);
        assert_eq!(parser.by_ref().count(), 1);
        assert_eq!(parser.stable_len(), 5);
        assert_eq!(parser.incomplete_tail().unwrap().command, "echo one \n");
    }

    #[test]
    fn crlf_endings_are_stripped_and_a_lone_cr_is_kept() {
        assert_eq!(
            commands(b": 1700000000:0;echo a\\\r\nb\r\nprintf 'x\ry'\r\n"),
            ["echo a\nb", "printf 'x\ry'"]
        );
    }

    #[test]
    fn whitespace_only_commands_are_dropped_but_advance_the_boundary() {
        let mut parser = Parser::new(&b"ls\n\n: 1700000000:0;  \t\n   \n"[..]);
        assert_eq!(
            parser
                .by_ref()
                .map(|r| r.unwrap().command)
                .collect::<Vec<_>>(),
            ["ls"]
        );
        assert_eq!(parser.stable_len(), 27);
        assert!(parser.incomplete_tail().is_none());
    }

    #[test]
    fn a_timestamp_too_long_for_i64_is_absent() {
        let records = parse(b": 99999999999999999999:0;echo late\n");
        assert_eq!(records[0].timestamp, None);
        assert_eq!(records[0].command, "echo late");
    }

    #[test]
    fn an_oversized_record_is_cut_at_the_cap_and_the_next_one_survives() {
        let mut input = b": 1700000000:0;".to_vec();
        input.resize(input.len() + RECORD_CAP + 4096, b'a');
        input.extend_from_slice(b"\\\ntail of the same record\n: 1700000001:0;next\n");
        let end_of_first = (input.len() - b": 1700000001:0;next\n".len()) as u64;

        let records = parse(&input);
        assert_eq!(records.len(), 2);
        assert!(records[0].lossy);
        assert_eq!(records[0].timestamp, Some(1_700_000_000));
        assert_eq!(
            records[0].command.len(),
            RECORD_CAP - b": 1700000000:0;".len()
        );
        assert!(records[0].command.bytes().all(|b| b == b'a'));
        assert_eq!(records[0].end, end_of_first);
        assert_eq!(records[1].command, "next");
        assert!(!records[1].lossy);
    }
}
