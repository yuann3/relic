use std::io::{self, BufRead};

use super::{Line, Lines, RECORD_CAP, RawRecord, decode};
use crate::domain::ObservationKind;

/// v2: markers detected per record, CRLF-tolerant, capped per record,
/// whitespace-only commands dropped, whitespace-only lines trimmed from both
/// ends of a timed record, trailing partial record withheld.
pub const PARSER_CONTRACT: &str = "bash-v2";

/// Streams Bash history records.
///
/// A `#<epoch-seconds>` line (written under HISTTIMEFORMAT) times the command
/// after it. In a file that starts with a marker, every line up to the next
/// marker is that one multiline command; in any other file only the next
/// line is, so one stray `#123` comment among untimed commands cannot swallow
/// everything after it. This is bash's own rule for reading its history. A
/// line with no marker open is one untimed record.
pub struct Parser<R> {
    lines: Lines<R>,
    multiline: bool,
    /// Reused for each physical line: a marker must be recognised even when
    /// the record it ends is already at the cap.
    line: Vec<u8>,
    marker: Option<Marker>,
    stable_len: u64,
    incomplete_tail: Option<RawRecord>,
}

/// A marker line that has been read, whose command lines have not.
struct Marker {
    timestamp: Option<i64>,
    start: u64,
}

impl<R: BufRead> Parser<R> {
    /// `multiline`: the file starts with a marker (see [`is_marker`]). Passed
    /// in because a parse resumed mid-file cannot see the first line.
    pub fn new(reader: R, multiline: bool) -> Self {
        Self {
            lines: Lines::new(reader, false),
            multiline,
            line: Vec::new(),
            marker: None,
            stable_len: 0,
            incomplete_tail: None,
        }
    }

    /// Byte end of the last complete record read so far, stored or not.
    pub fn stable_len(&self) -> u64 {
        self.stable_len
    }

    /// The input's last record once the input is exhausted, if it ended
    /// without its newline or was a marker with no command. Never yielded:
    /// the shell may still be writing it.
    pub fn incomplete_tail(&self) -> Option<&RawRecord> {
        self.incomplete_tail.as_ref()
    }

    fn read_line(&mut self) -> io::Result<Option<Line>> {
        self.line.clear();
        self.lines.read_into(&mut self.line)
    }

    /// The untimed record or marker on the next line.
    fn next_untimed(&mut self) -> io::Result<Option<RawRecord>> {
        loop {
            let start = self.lines.offset;
            let Some(line) = self.read_line()? else {
                return Ok(None);
            };
            if let Some(timestamp) = marker_time(&self.line, &line) {
                self.marker = Some(Marker { timestamp, start });
                return self.next_timed();
            }
            let mut lossy = line.lossy();
            let command = decode(std::mem::take(&mut self.line), &mut lossy);
            let record = RawRecord {
                kind: ObservationKind::BashCommand,
                timestamp: None,
                duration: None,
                command,
                start,
                end: self.lines.offset,
                lossy,
            };
            if !line.terminated {
                self.incomplete_tail = Some(record);
                return Ok(None);
            }
            self.stable_len = record.end;
            if !record.command.trim().is_empty() {
                return Ok(Some(record));
            }
        }
    }

    /// The record of the open marker: its lines up to the next marker.
    fn next_timed(&mut self) -> io::Result<Option<RawRecord>> {
        while let Some(marker) = self.marker.take() {
            let mut bytes = Vec::new();
            let mut lossy = false;
            let mut terminated = false;
            let mut end = self.lines.offset;
            // Length of `bytes` up to its last line with a command on it.
            let mut content_len = 0;
            while let Some(line) = self.read_line()? {
                if let Some(timestamp) = marker_time(&self.line, &line) {
                    self.marker = Some(Marker {
                        timestamp,
                        start: end,
                    });
                    break;
                }
                terminated = line.terminated;
                end = self.lines.offset;
                let blank = self.line.iter().all(u8::is_ascii_whitespace);
                // Whitespace-only lines around the command are not part of
                // it; the ones inside it are.
                if blank && bytes.is_empty() {
                    continue;
                }
                if !bytes.is_empty() {
                    if bytes.len() < RECORD_CAP {
                        bytes.push(b'\n');
                    } else {
                        lossy = true;
                    }
                }
                let room = RECORD_CAP - bytes.len();
                lossy |= line.lossy() || self.line.len() > room;
                bytes.extend_from_slice(&self.line[..self.line.len().min(room)]);
                if !blank {
                    content_len = bytes.len();
                    if !self.multiline {
                        break;
                    }
                }
            }
            bytes.truncate(content_len);
            let command = decode(bytes, &mut lossy);
            let blank = command.trim().is_empty();
            let record = RawRecord {
                kind: ObservationKind::BashCommand,
                timestamp: marker.timestamp,
                duration: None,
                command,
                start: marker.start,
                end,
                lossy,
            };
            // At end of input a marker with no command is still waiting for one.
            if self.marker.is_none() && (blank || !terminated) {
                self.incomplete_tail = Some(record);
                return Ok(None);
            }
            self.stable_len = end;
            if !blank {
                return Ok(Some(record));
            }
        }
        self.next_untimed()
    }
}

impl<R: BufRead> Iterator for Parser<R> {
    type Item = io::Result<RawRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_timed().transpose()
    }
}

/// `Some` when the line is a `#<digits>` marker; the time inside is absent
/// when the digits do not fit an i64.
fn marker_time(bytes: &[u8], line: &Line) -> Option<Option<i64>> {
    if line.cut || !is_marker(bytes) {
        return None;
    }
    Some(std::str::from_utf8(&bytes[1..]).ok()?.parse().ok())
}

/// Whether a line, without its line ending, is a `#<digits>` marker.
pub fn is_marker(line: &[u8]) -> bool {
    line.strip_prefix(b"#")
        .is_some_and(|digits| !digits.is_empty() && digits.iter().all(u8::is_ascii_digit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::testing::{FailsAfter, FailsOnce};
    use std::io::BufReader;

    fn parse(input: &[u8]) -> Vec<RawRecord> {
        let first = input.split(|b| *b == b'\n').next().unwrap_or_default();
        Parser::new(input, is_marker(first))
            .map(Result::unwrap)
            .collect()
    }

    fn commands(input: &[u8]) -> Vec<String> {
        parse(input).into_iter().map(|r| r.command).collect()
    }

    #[test]
    fn parse_timestamped_commands() {
        let records = parse(b"#1700000000\ncargo build\n#1700000005\ngit push\n");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].timestamp, Some(1_700_000_000));
        assert_eq!(records[0].command, "cargo build");
        assert_eq!((records[0].start, records[0].end), (0, 24));
        assert_eq!(records[1].timestamp, Some(1_700_000_005));
        assert_eq!(records[1].command, "git push");
        assert_eq!((records[1].start, records[1].end), (24, 45));
    }

    #[test]
    fn parse_multiline_with_timestamps() {
        assert_eq!(
            commands(b"#1700000000\nfor i in 1 2 3\ndo echo $i\ndone\n#1700000010\nls\n"),
            ["for i in 1 2 3\ndo echo $i\ndone", "ls"]
        );
    }

    #[test]
    fn parse_plain_no_timestamps() {
        let records = parse(b"ls -la\n\ncd /tmp\n");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].command, "ls -la");
        assert_eq!(records[0].timestamp, None);
        assert_eq!((records[1].start, records[1].end), (8, 16));
    }

    #[test]
    fn parse_empty_input() {
        assert!(parse(b"").is_empty());
    }

    #[test]
    fn a_marker_in_a_file_that_does_not_start_with_one_times_only_the_next_line() {
        let mut input = Vec::new();
        for i in 0..20 {
            input.extend_from_slice(format!("untimed {i}\n").as_bytes());
        }
        // A stray `#123` comment: everything after it must stay separate.
        input.extend_from_slice(b"#1700000000\nif true\nthen ls\nfi\n");

        let records = parse(&input);
        assert_eq!(records.len(), 23);
        assert_eq!(records[19].timestamp, None);
        assert_eq!(
            (records[20].timestamp, records[20].command.as_str()),
            (Some(1_700_000_000), "if true")
        );
        assert_eq!(
            (records[21].timestamp, records[21].command.as_str()),
            (None, "then ls")
        );
        assert_eq!(
            (records[22].timestamp, records[22].command.as_str()),
            (None, "fi")
        );
        assert_eq!(records[22].end, input.len() as u64);
    }

    #[test]
    fn yields_the_first_record_before_the_source_fails() {
        let input = b"#1700000000\nfirst\n#1700000001\nsecond\n#1700000002\nthird\n";
        // The source dies inside the second record's command.
        let mut parser = Parser::new(BufReader::with_capacity(8, FailsAfter(&input[..33])), true);
        assert_eq!(parser.next().unwrap().unwrap().command, "first");
        assert!(parser.next().unwrap().is_err());
        assert_eq!(parser.stable_len(), 18);
    }

    #[test]
    fn nothing_is_yielded_after_the_source_fails_even_if_it_recovers() {
        let input = b"#1700000000\nfirst\n#1700000001\nsecond record\n#1700000002\nthird\n";
        let source = FailsOnce {
            before: &input[..36],
            after: &input[36..],
            failed: false,
        };
        let mut parser = Parser::new(BufReader::with_capacity(4, source), true);
        assert_eq!(parser.next().unwrap().unwrap().command, "first");
        assert!(parser.next().unwrap().is_err());
        assert!(parser.next().is_none());
        assert_eq!(parser.stable_len(), 18);
        assert!(parser.incomplete_tail().is_none());
    }

    #[test]
    fn whitespace_only_lines_are_trimmed_around_a_timed_command_and_kept_inside_it() {
        let records = parse(b"#1\n \n\nfoo  \n\nbar\n\n   \n#2\nbaz\n");
        assert_eq!(records[0].command, "foo  \n\nbar");
        assert_eq!((records[0].start, records[0].end), (0, 22));
        assert_eq!(records[1].command, "baz");
        assert_eq!(commands(b"#1\nfoo\n\n\n"), ["foo"]);
    }

    #[test]
    fn invalid_utf8_is_flagged_lossy() {
        let records = parse(b"#1700000000\necho \xFF\n");
        assert_eq!(records[0].command, "echo \u{FFFD}");
        assert!(records[0].lossy);
    }

    #[test]
    fn a_record_without_its_newline_is_withheld() {
        for (input, stable_len) in [
            (&b"#1700000000\ndone\n#1700000001\ncargo bui"[..], 17),
            (b"done\ncargo bui", 5),
        ] {
            let mut parser = Parser::new(input, true);
            assert_eq!(
                parser
                    .by_ref()
                    .map(|r| r.unwrap().command)
                    .collect::<Vec<_>>(),
                ["done"]
            );
            assert_eq!(parser.stable_len(), stable_len);
            assert_eq!(parser.incomplete_tail().unwrap().command, "cargo bui");
        }
    }

    #[test]
    fn a_marker_with_no_command_yet_is_withheld() {
        let mut parser = Parser::new(&b"#1700000000\ndone\n#1700000001\n"[..], true);
        assert_eq!(parser.by_ref().count(), 1);
        assert_eq!(parser.stable_len(), 17);
        assert_eq!(
            parser.incomplete_tail().unwrap().timestamp,
            Some(1_700_000_001)
        );
    }

    #[test]
    fn crlf_endings_are_stripped_from_markers_and_commands() {
        let records = parse(b"#1700000000\r\ncargo build\r\n#1700000005\r\ngit push\r\n");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].timestamp, Some(1_700_000_000));
        assert_eq!(records[0].command, "cargo build");
        assert_eq!(records[1].command, "git push");
    }

    #[test]
    fn whitespace_only_commands_are_dropped() {
        assert_eq!(
            commands(b"ls\n   \n#1700000000\n \t\n#1700000001\npwd\n"),
            ["ls", "pwd"]
        );
    }

    #[test]
    fn a_timestamp_too_long_for_i64_is_absent() {
        let records = parse(b"#99999999999999999999\necho late\necho more\n");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].timestamp, None);
        assert_eq!(records[0].command, "echo late\necho more");
    }

    #[test]
    fn an_oversized_record_is_cut_at_the_cap_and_the_next_marker_is_still_seen() {
        let mut input = b"#1700000000\n".to_vec();
        input.resize(input.len() + RECORD_CAP + 4096, b'a');
        input.extend_from_slice(b"\nsecond line\n#1700000001\nnext\n");

        let records = parse(&input);
        assert_eq!(records.len(), 2);
        assert!(records[0].lossy);
        assert_eq!(records[0].command.len(), RECORD_CAP);
        assert_eq!(
            records[0].end,
            input.len() as u64 - "#1700000001\nnext\n".len() as u64
        );
        assert_eq!(records[1].timestamp, Some(1_700_000_001));
        assert_eq!(records[1].command, "next");
        assert!(!records[1].lossy);
    }
}
