use super::RawRecord;

/// Parse zsh history bytes (supports both plain and extended format).
///
/// Extended format: `: <timestamp>:<duration>;<command>`
/// Plain format: `<command>` (one per logical line)
/// Backslash-newline joins continuation lines into one logical record.
pub fn parse(input: &[u8]) -> Vec<RawRecord> {
    let mut records = Vec::new();
    let mut pos = 0;
    let len = input.len();

    while pos < len {
        let record_start = pos;

        // Collect one logical line (joining backslash-newline continuations).
        let (line, next_pos) = read_logical_line(input, pos);
        pos = next_pos;

        if line.is_empty() {
            continue;
        }

        // Try extended format: `: <timestamp>:<duration>;<command>`
        if let Some(record) = try_parse_extended(&line, record_start) {
            records.push(record);
        } else {
            // Plain format: the whole line is the command.
            records.push(RawRecord {
                timestamp: None,
                duration: None,
                command: line,
                byte_offset: record_start,
            });
        }
    }

    records
}

/// Read one logical line from the byte buffer starting at `pos`.
/// Joins backslash-newline continuations. Returns (decoded_line, next_pos).
fn read_logical_line(input: &[u8], mut pos: usize) -> (String, usize) {
    let mut buf = Vec::new();
    let len = input.len();

    while pos < len {
        let b = input[pos];
        if b == b'\n' {
            pos += 1;
            // Check if the previous byte was a backslash (continuation).
            if buf.last() == Some(&b'\\') {
                buf.pop(); // remove the backslash
                buf.push(b'\n'); // preserve the newline in the command text
                continue;
            }
            break;
        }
        buf.push(b);
        pos += 1;
    }

    let line = String::from_utf8_lossy(&buf).into_owned();
    (line, pos)
}

/// Try parsing a line as zsh extended format: `: <timestamp>:<duration>;<command>`
fn try_parse_extended(line: &str, byte_offset: usize) -> Option<RawRecord> {
    let rest = line.strip_prefix(": ")?;

    let colon_pos = rest.find(':')?;
    let timestamp: i64 = rest[..colon_pos].parse().ok()?;

    let after_colon = &rest[colon_pos + 1..];
    let semi_pos = after_colon.find(';')?;
    let duration: i64 = after_colon[..semi_pos].parse().ok()?;

    let command = after_colon[semi_pos + 1..].to_string();

    Some(RawRecord {
        timestamp: Some(timestamp),
        duration: Some(duration),
        command,
        byte_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extended_single_command() {
        let input = b": 1700000000:0;cargo build --release\n";
        let records = parse(input);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].timestamp, Some(1_700_000_000));
        assert_eq!(records[0].duration, Some(0));
        assert_eq!(records[0].command, "cargo build --release");
    }

    #[test]
    fn parse_extended_multiple_commands() {
        let input = b": 1700000000:0;cargo build\n: 1700000001:5;cargo test\n: 1700000010:2;git push\n";
        let records = parse(input);

        assert_eq!(records.len(), 3);
        assert_eq!(records[0].command, "cargo build");
        assert_eq!(records[1].command, "cargo test");
        assert_eq!(records[1].duration, Some(5));
        assert_eq!(records[2].command, "git push");
        assert_eq!(records[2].timestamp, Some(1_700_000_010));
    }

    #[test]
    fn parse_plain_format() {
        let input = b"ls -la\ncd /tmp\npwd\n";
        let records = parse(input);

        assert_eq!(records.len(), 3);
        assert_eq!(records[0].command, "ls -la");
        assert_eq!(records[0].timestamp, None);
        assert_eq!(records[1].command, "cd /tmp");
        assert_eq!(records[2].command, "pwd");
    }

    #[test]
    fn parse_backslash_newline_continuation() {
        let input = b": 1700000000:0;echo hello \\\nworld\n";
        let records = parse(input);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].command, "echo hello \nworld");
    }

    #[test]
    fn parse_mixed_extended_and_plain() {
        // This shouldn't normally happen, but the parser should handle it.
        let input = b": 1700000000:0;cargo build\nplain command\n";
        let records = parse(input);

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].timestamp, Some(1_700_000_000));
        assert_eq!(records[1].timestamp, None);
        assert_eq!(records[1].command, "plain command");
    }

    #[test]
    fn parse_empty_input() {
        let records = parse(b"");
        assert!(records.is_empty());
    }

    #[test]
    fn parse_no_trailing_newline() {
        let input = b": 1700000000:0;cargo build";
        let records = parse(input);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].command, "cargo build");
    }

    #[test]
    fn parse_preserves_byte_offsets() {
        let input = b": 1700000000:0;first\n: 1700000001:0;second\n";
        let records = parse(input);

        assert_eq!(records[0].byte_offset, 0);
        assert_eq!(records[1].byte_offset, 21);
    }

    #[test]
    fn parse_skips_empty_lines() {
        let input = b"cargo build\n\n\ncargo test\n";
        let records = parse(input);

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].command, "cargo build");
        assert_eq!(records[1].command, "cargo test");
    }

    #[test]
    fn parse_malformed_extended_falls_back_to_plain() {
        // Missing semicolon — should be treated as plain.
        let input = b": 1700000000:0 missing semicolon\n";
        let records = parse(input);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].timestamp, None);
        assert_eq!(records[0].command, ": 1700000000:0 missing semicolon");
    }
}
