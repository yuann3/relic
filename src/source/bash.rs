use super::RawRecord;

/// Parse Bash history bytes.
///
/// With HISTTIMEFORMAT: `#<epoch-seconds>` marker followed by command line(s).
/// Without timestamps: each physical line is one record.
///
/// When timestamp markers are present, all physical lines between two markers
/// form one multiline entry. Without markers, multiline commands are ambiguous
/// so each physical line is imported separately.
pub fn parse(input: &[u8]) -> Vec<RawRecord> {
    let mut records = Vec::new();
    let mut pos = 0;
    let len = input.len();

    // First pass: detect whether file uses timestamp markers at all.
    let has_timestamps = has_timestamp_markers(input);

    if has_timestamps {
        parse_with_timestamps(input, &mut records);
    } else {
        // No timestamps — each physical line is one record.
        while pos < len {
            let line_start = pos;
            let end = memchr_newline(input, pos);
            let line = &input[pos..end];
            pos = if end < len { end + 1 } else { end };

            if line.is_empty() {
                continue;
            }

            let command = String::from_utf8_lossy(line).into_owned();
            records.push(RawRecord {
                timestamp: None,
                duration: None,
                command,
                byte_offset: line_start,
            });
        }
    }

    records
}

fn has_timestamp_markers(input: &[u8]) -> bool {
    let mut pos = 0;
    let len = input.len();
    // Check first few lines for a `#<digits>` pattern.
    for _ in 0..10 {
        if pos >= len {
            break;
        }
        let end = memchr_newline(input, pos);
        let line = &input[pos..end];
        if is_timestamp_marker(line) {
            return true;
        }
        pos = if end < len { end + 1 } else { len };
    }
    false
}

fn is_timestamp_marker(line: &[u8]) -> bool {
    if line.len() < 2 || line[0] != b'#' {
        return false;
    }
    line[1..].iter().all(|&b| b.is_ascii_digit())
}

fn parse_timestamp_value(line: &[u8]) -> Option<i64> {
    if !is_timestamp_marker(line) {
        return None;
    }
    std::str::from_utf8(&line[1..]).ok()?.parse().ok()
}

fn parse_with_timestamps(input: &[u8], records: &mut Vec<RawRecord>) {
    let len = input.len();
    let mut pos = 0;
    let mut current_ts: Option<i64> = None;
    let mut command_parts: Vec<u8> = Vec::new();
    let mut record_start: usize = 0;

    while pos < len {
        let line_start = pos;
        let end = memchr_newline(input, pos);
        let line = &input[pos..end];
        pos = if end < len { end + 1 } else { end };

        if let Some(ts) = parse_timestamp_value(line) {
            // Flush previous command if any.
            if current_ts.is_some() && !command_parts.is_empty() {
                let command = String::from_utf8_lossy(&command_parts).into_owned();
                records.push(RawRecord {
                    timestamp: current_ts,
                    duration: None,
                    command,
                    byte_offset: record_start,
                });
                command_parts.clear();
            }
            current_ts = Some(ts);
            record_start = line_start;
        } else if current_ts.is_some() {
            if !command_parts.is_empty() {
                command_parts.push(b'\n');
            }
            command_parts.extend_from_slice(line);
        } else if !line.is_empty() {
            // Lines before any timestamp marker — treat as plain.
            let command = String::from_utf8_lossy(line).into_owned();
            records.push(RawRecord {
                timestamp: None,
                duration: None,
                command,
                byte_offset: line_start,
            });
        }
    }

    // Flush trailing command.
    if current_ts.is_some() && !command_parts.is_empty() {
        let command = String::from_utf8_lossy(&command_parts).into_owned();
        records.push(RawRecord {
            timestamp: current_ts,
            duration: None,
            command,
            byte_offset: record_start,
        });
    }
}

fn memchr_newline(input: &[u8], start: usize) -> usize {
    input[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(input.len(), |p| start + p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_timestamped_commands() {
        let input = b"#1700000000\ncargo build\n#1700000005\ngit push\n";
        let records = parse(input);

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].timestamp, Some(1_700_000_000));
        assert_eq!(records[0].command, "cargo build");
        assert_eq!(records[1].timestamp, Some(1_700_000_005));
        assert_eq!(records[1].command, "git push");
    }

    #[test]
    fn parse_multiline_with_timestamps() {
        let input = b"#1700000000\nfor i in 1 2 3\ndo echo $i\ndone\n#1700000010\nls\n";
        let records = parse(input);

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].command, "for i in 1 2 3\ndo echo $i\ndone");
        assert_eq!(records[1].command, "ls");
    }

    #[test]
    fn parse_plain_no_timestamps() {
        let input = b"ls -la\ncd /tmp\npwd\n";
        let records = parse(input);

        assert_eq!(records.len(), 3);
        assert_eq!(records[0].command, "ls -la");
        assert_eq!(records[0].timestamp, None);
        assert_eq!(records[1].command, "cd /tmp");
    }

    #[test]
    fn parse_empty_input() {
        let records = parse(b"");
        assert!(records.is_empty());
    }

    #[test]
    fn parse_skips_empty_lines() {
        let input = b"cargo build\n\ncargo test\n";
        let records = parse(input);

        assert_eq!(records.len(), 2);
    }

    #[test]
    fn parse_no_trailing_newline() {
        let input = b"#1700000000\ncargo build";
        let records = parse(input);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].command, "cargo build");
    }

    #[test]
    fn parse_preserves_byte_offsets() {
        let input = b"#1700000000\ncargo build\n#1700000005\ngit push\n";
        let records = parse(input);

        assert_eq!(records[0].byte_offset, 0);
        // Second record starts at the second #timestamp marker.
        // "#1700000000\ncargo build\n" = 24 bytes
        assert_eq!(records[1].byte_offset, 24);
    }

    #[test]
    fn parse_comment_like_line_not_timestamp() {
        // A line starting with # but not followed by all digits is not a timestamp.
        let input = b"# this is a comment\nls\n";
        let records = parse(input);

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].command, "# this is a comment");
        assert_eq!(records[1].command, "ls");
    }
}
