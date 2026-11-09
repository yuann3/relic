//! Claude Code session transcripts, `~/.claude/projects/<project>/*.jsonl`,
//! as a source, from any Claude Code version. Claude Code documents the
//! format as internal and changing between versions, so a `user` or
//! `assistant` line in a shape this parser does not know is counted as
//! unreadable and skipped, never guessed at
//! (docs/research/source-evidence-contracts.md).
//!
//! Only structured tool fields become records: what a Bash call ran and what
//! it printed, and what a Skill, file or search call named. Prompts, replies,
//! thinking, skill bodies and attachments never do. They are prose anyone
//! could have written, and would pass for evidence.

use std::io::{self, BufRead};

use serde::Deserialize;

use super::{Lines, RawRecord};
use crate::domain::ObservationKind;

/// v1: Bash calls and their results, and the tool calls in [`TOOLS`], from
/// the `user` and `assistant` records.
pub const PARSER_CONTRACT: &str = "claude-code-v1";

/// Most bytes kept of what a Bash call printed: how it began and ended,
/// not the log in between.
pub const OUTPUT_MAX: usize = 1024;
/// Most bytes kept of what another tool call named.
pub const ARG_MAX: usize = 4096;

/// Tool calls kept besides Bash, and the input field that says what each
/// was about.
const TOOLS: &[(&str, &str)] = &[
    ("Skill", "skill"),
    ("Read", "file_path"),
    ("Edit", "file_path"),
    ("Write", "file_path"),
    ("Grep", "pattern"),
    ("Glob", "pattern"),
    ("WebSearch", "query"),
    ("WebFetch", "url"),
    ("Agent", "description"),
];

/// Whether the first bytes of a file are a JSONL record.
pub fn looks_like(head: &[u8]) -> bool {
    head.starts_with(b"{\"")
}

/// Streams records out of one session file.
pub struct Parser<R> {
    lines: Lines<R>,
    line: Vec<u8>,
    stable_len: u64,
    incomplete_tail: bool,
    unreadable: usize,
    /// Bash calls not yet answered: a result says only which call it
    /// answers, and most carry nothing else that marks them as Bash's.
    pending: std::collections::HashSet<String>,
    /// Records starting before this offset are read for their calls but
    /// not yielded.
    from: u64,
}

impl<R: BufRead> Parser<R> {
    /// Parse a whole session, from its first byte.
    pub fn new(reader: R) -> Self {
        Self::resume(reader, 0)
    }

    /// Parse a session from its first byte, yielding only the records that
    /// start at `from` or later: a call before `from` still pairs with its
    /// result after it, exactly as in a whole parse.
    pub fn resume(reader: R, from: u64) -> Self {
        Self {
            lines: Lines::new(reader, false),
            line: Vec::new(),
            stable_len: 0,
            incomplete_tail: false,
            unreadable: 0,
            pending: std::collections::HashSet::new(),
            from,
        }
    }

    /// Byte end of the last line read and dealt with, kept or not.
    pub fn stable_len(&self) -> u64 {
        self.stable_len
    }

    /// The input ended in a line without its newline: Claude Code may still
    /// be writing it.
    pub fn incomplete_tail(&self) -> bool {
        self.incomplete_tail
    }

    /// `user` or `assistant` lines that were not the JSON this parser knows,
    /// or were cut at the record cap. Skipped.
    pub fn unreadable(&self) -> usize {
        self.unreadable
    }

    fn next_record(&mut self) -> io::Result<Option<RawRecord>> {
        loop {
            let start = self.lines.offset;
            self.line.clear();
            let Some(line) = self.lines.read_into(&mut self.line)? else {
                return Ok(None);
            };
            if !line.terminated {
                self.incomplete_tail = true;
                return Ok(None);
            }
            let end = self.lines.offset;
            match parse(&self.line, &mut self.pending) {
                Parsed::Record(kind, timestamp, text, lossy) => {
                    self.stable_len = end;
                    if start >= self.from {
                        return Ok(Some(RawRecord {
                            kind,
                            timestamp,
                            duration: None,
                            command: text,
                            start,
                            end,
                            lossy,
                        }));
                    }
                }
                Parsed::Nothing => self.stable_len = end,
                Parsed::Unreadable => {
                    self.unreadable += 1;
                    self.stable_len = end;
                }
            }
        }
    }
}

impl<R: BufRead> Iterator for Parser<R> {
    type Item = io::Result<RawRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_record().transpose()
    }
}

enum Parsed {
    Record(ObservationKind, Option<i64>, String, bool),
    /// A line with nothing to keep: another record type, prose, a result
    /// of a tool not kept.
    Nothing,
    Unreadable,
}

/// What decides whether a line is read further. Every line has a `type`;
/// only `user` and `assistant` lines are parsed whole.
#[derive(Deserialize)]
struct Head {
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct Envelope {
    timestamp: Option<String>,
    message: Message,
    #[serde(rename = "toolUseResult")]
    tool_use_result: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Message {
    content: Content,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Blocks(Vec<Block>),
    /// A prompt or reply as plain text: never kept.
    Text(#[expect(dead_code, reason = "only its shape is checked")] String),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    ToolUse {
        #[serde(default)]
        id: String,
        name: String,
        input: serde_json::Map<String, serde_json::Value>,
    },
    ToolResult {
        #[serde(default)]
        tool_use_id: String,
        content: Option<ResultContent>,
        #[serde(default)]
        is_error: bool,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ResultContent {
    Text(String),
    Blocks(Vec<TextBlock>),
}

#[derive(Deserialize)]
struct TextBlock {
    text: Option<String>,
}

/// One line. `pending` holds the Bash calls read so far and not answered.
/// A line cut at the record cap is a strict prefix of a JSON object, which
/// never parses, so it is unreadable too.
fn parse(line: &[u8], pending: &mut std::collections::HashSet<String>) -> Parsed {
    let Ok(head) = serde_json::from_slice::<Head>(line) else {
        return Parsed::Unreadable;
    };
    if !matches!(head.kind.as_deref(), Some("user" | "assistant")) {
        return Parsed::Nothing;
    }
    let Ok(envelope) = serde_json::from_slice::<Envelope>(line) else {
        return Parsed::Unreadable;
    };
    let Content::Blocks(blocks) = envelope.message.content else {
        return Parsed::Nothing;
    };
    let timestamp = envelope.timestamp.as_deref().and_then(iso_seconds);
    // One tool block per record, as every version seen writes them.
    for block in blocks {
        match block {
            Block::ToolUse { id, name, input } if name == "Bash" => {
                return match input.get("command").and_then(|c| c.as_str()) {
                    Some(command) => {
                        pending.insert(id);
                        Parsed::Record(
                            ObservationKind::AgentCommand,
                            timestamp,
                            command.to_string(),
                            false,
                        )
                    }
                    None => Parsed::Unreadable,
                };
            }
            Block::ToolUse { name, input, .. } => {
                let Some((_, field)) = TOOLS.iter().find(|(tool, _)| *tool == name) else {
                    return Parsed::Nothing;
                };
                let Some(arg) = input.get(*field).and_then(|a| a.as_str()) else {
                    return Parsed::Unreadable;
                };
                let mut text = format!("{name} {arg}");
                if let Some(args) = input
                    .get("args")
                    .and_then(|a| a.as_str())
                    .filter(|a| !a.is_empty() && name == "Skill")
                {
                    text.push(' ');
                    text.push_str(args);
                }
                let (text, lossy) = capped(text, ARG_MAX);
                return Parsed::Record(ObservationKind::AgentTool, timestamp, text, lossy);
            }
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if pending.remove(&tool_use_id) => {
                let interrupted = envelope
                    .tool_use_result
                    .as_ref()
                    .and_then(|r| r.get("interrupted"))
                    .and_then(serde_json::Value::as_bool);
                let said = match content {
                    Some(ResultContent::Text(text)) => text,
                    Some(ResultContent::Blocks(blocks)) => blocks
                        .into_iter()
                        .filter_map(|b| b.text)
                        .collect::<Vec<_>>()
                        .join("\n"),
                    None => String::new(),
                };
                let status = match (interrupted, is_error) {
                    (Some(true), _) => "interrupted: ",
                    (_, true) => "failed: ",
                    _ => "",
                };
                let said = if said.trim().is_empty() {
                    "(no output)".to_string()
                } else {
                    said
                };
                let (text, lossy) = ends(format!("{status}{said}"), OUTPUT_MAX);
                return Parsed::Record(ObservationKind::AgentOutput, timestamp, text, lossy);
            }
            Block::ToolResult { .. } => return Parsed::Nothing,
            Block::Other => {}
        }
    }
    Parsed::Nothing
}

/// `text` cut to at most `max` bytes, and whether it was.
fn capped(mut text: String, max: usize) -> (String, bool) {
    let cut = text.len() > max;
    text.truncate(text.floor_char_boundary(max));
    (text, cut)
}

/// What marks the bytes [`ends`] left out.
const ELIDED: &str = "\n[…]\n";

/// `text` cut to its first and last bytes, at most `max` in all with the
/// mark between them, and whether it was: an error or a summary is usually
/// at the end of what a command printed.
fn ends(text: String, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let half = (max - ELIDED.len()) / 2;
    let head = text.floor_char_boundary(half);
    let tail = text.ceil_char_boundary(text.len() - half);
    (format!("{}{ELIDED}{}", &text[..head], &text[tail..]), true)
}

/// Seconds since the epoch of `YYYY-MM-DDTHH:MM:SS[.fff]Z`, the only form
/// the checked versions write. Anything else is no time.
fn iso_seconds(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = b.get(r)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| part.iter().fold(0, |n, d| n * 10 + i64::from(d - b'0')))
    };
    let shape = b.len() >= 20
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':';
    let zulu = b.last() == Some(&b'Z')
        && (b.len() == 20 || (b.len() > 21 && b[19] == b'.' && num(20..b.len() - 1).is_some()));
    if !shape || !zulu {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(input: &[u8]) -> (Vec<RawRecord>, Parser<&[u8]>) {
        let mut parser = Parser::new(input);
        let records = parser.by_ref().collect::<io::Result<Vec<_>>>().unwrap();
        (records, parser)
    }

    fn kinds_and_texts(records: &[RawRecord]) -> Vec<(ObservationKind, &str)> {
        records
            .iter()
            .map(|r| (r.kind, r.command.as_str()))
            .collect()
    }

    /// A session Claude Code 2.1.318 recorded (`claude -p` in a scratch
    /// repository with one project skill), less the records that described
    /// the recording machine: its account, skills, tools and prompts.
    #[test]
    fn a_recorded_session_yields_its_tool_calls_and_bash_results_and_no_prose() {
        let fixture = std::fs::read("tests/fixtures/claude-code/2.1.318.jsonl").unwrap();
        let (records, parser) = parse_all(&fixture);
        let read = "Read /private/tmp/relic-cc-fixture.xvCa/.claude/skills/fixture-echo/SKILL.md";
        let status = "On branch main\nUntracked files:\n  (use \"git add <file>...\" to include in what will be committed)\n\t.claude/\n\nnothing added to commit but untracked files present (use \"git add\" to track)";
        assert_eq!(
            kinds_and_texts(&records),
            [
                (ObservationKind::AgentTool, "Skill fixture-echo"),
                (ObservationKind::AgentCommand, "echo fixture-greeting"),
                (ObservationKind::AgentOutput, "fixture-greeting"),
                (ObservationKind::AgentCommand, "git status"),
                (ObservationKind::AgentOutput, status),
                (ObservationKind::AgentTool, read),
            ]
        );
        // 2026-11-06T04:11:27.506Z, as `date -u -d` gives it.
        assert_eq!(records[0].timestamp, Some(1_793_938_287));
        assert!(records.iter().all(|r| !r.lossy));
        assert_eq!((parser.unreadable(), parser.incomplete_tail()), (0, false));
        assert_eq!(parser.stable_len(), fixture.len() as u64);
        // Each record spans exactly its own line.
        for r in &records {
            let line = &fixture[r.start as usize..r.end as usize];
            assert!(
                line.starts_with(b"{")
                    && line.ends_with(b"\n")
                    && !line[..line.len() - 1].contains(&b'\n')
            );
        }
    }

    fn line(kind: &str, version: &str, content: &str, extra: &str) -> String {
        format!(
            r#"{{"type":"{kind}","version":"{version}","timestamp":"2026-11-06T04:11:29.660Z","message":{{"content":{content}}}{extra}}}"#
        ) + "\n"
    }

    fn bash(id: &str, command: &str) -> String {
        let call = format!(
            r#"[{{"type":"tool_use","id":"{id}","name":"Bash","input":{{"command":{command:?}}}}}]"#
        );
        line("assistant", "2.1.300", &call, "")
    }

    /// A result for call `id`; `extra` is the rest of the envelope, such as
    /// its `toolUseResult`, which most Bash results lack.
    fn result(id: &str, content: &str, is_error: bool, extra: &str) -> String {
        let block = format!(
            r#"[{{"type":"tool_result","tool_use_id":"{id}","content":{content},"is_error":{is_error}}}]"#
        );
        line("user", "2.1.300", &block, extra)
    }

    #[test]
    fn results_pair_with_their_bash_call_say_how_it_ended_and_are_capped() {
        let long = format!(
            "Compiling é{}test result: FAILED. 1 passed; 1 failed",
            "x".repeat(OUTPUT_MAX)
        );
        let read = line(
            "assistant",
            "2.1.300",
            r#"[{"type":"tool_use","id":"r","name":"Read","input":{"file_path":"/f"}}]"#,
            "",
        );
        let input = [
            bash("a", "git rev-parse nope"),
            bash("b", "sleep 9"),
            read,
            bash("c", "cat big"),
            bash("d", "printf"),
            // Answered out of order, as parallel calls are.
            result(
                "b",
                r#""""#,
                false,
                r#","toolUseResult":{"stdout":"","interrupted":true}"#,
            ),
            result(
                "a",
                r#""Exit code 128\nfatal: bad ref""#,
                true,
                r#","toolUseResult":"Error: Exit code 128""#,
            ),
            result("r", r#""1\tfile body""#, false, ""),
            result("c", &format!("{long:?}"), false, ""),
            result(
                "d",
                r#"[{"type":"text","text":"one"},{"type":"text","text":"two"}]"#,
                false,
                "",
            ),
            // A second answer, or one to a call never made: no call to pair.
            result("a", r#""again""#, false, ""),
            result("zz", r#""stray""#, false, ""),
        ]
        .concat();
        let (records, _) = parse_all(input.as_bytes());
        let outputs: Vec<&RawRecord> = records
            .iter()
            .filter(|r| r.kind == ObservationKind::AgentOutput)
            .collect();
        let texts: Vec<&str> = outputs.iter().map(|r| r.command.as_str()).collect();
        assert_eq!(
            texts[..2],
            [
                "interrupted: (no output)",
                "failed: Exit code 128\nfatal: bad ref"
            ]
        );
        assert!(
            texts[2].len() <= OUTPUT_MAX && outputs[2].lossy && !outputs[1].lossy,
            "{}",
            texts[2].len()
        );
        assert_eq!(texts[3..], ["one\ntwo"]);
        assert!(
            texts[2].starts_with("Compiling é")
                && texts[2].contains("x\n[…]\nx")
                && texts[2].ends_with("xtest result: FAILED. 1 passed; 1 failed"),
            "{}",
            texts[2]
        );
    }

    #[test]
    fn a_resumed_parse_yields_exactly_the_tail_of_a_whole_one() {
        // The boundary falls between a call and its result.
        let head = bash("a", "git reflog");
        let input = [
            head.clone(),
            result("a", r#""abc123 HEAD@{0}""#, false, ""),
            bash("b", "ls"),
        ]
        .concat();
        let (whole, _) = parse_all(input.as_bytes());
        let mut resumed = Parser::resume(input.as_bytes(), head.len() as u64);
        let tail = resumed.by_ref().collect::<io::Result<Vec<_>>>().unwrap();
        assert_eq!(
            kinds_and_texts(&tail),
            [
                (ObservationKind::AgentOutput, "abc123 HEAD@{0}"),
                (ObservationKind::AgentCommand, "ls")
            ]
        );
        assert_eq!(tail, whole[1..]);
        assert_eq!(resumed.stable_len(), input.len() as u64);
    }

    #[test]
    fn only_tool_fields_are_kept_and_skills_keep_their_args() {
        let input = [
            line("user", "2.1.300", r#""a prompt naming git reflog""#, ""),
            line("assistant", "2.1.300", r#"[{"type":"text","text":"git reflog, said the reply"},{"type":"thinking","thinking":"git reflog"}]"#, ""),
            line("assistant", "2.1.300", r#"[{"type":"tool_use","name":"Skill","input":{"skill":"git-x","args":"stash"}}]"#, ""),
            line("assistant", "2.1.300", r#"[{"type":"tool_use","name":"mcp__x__y","input":{"q":"git reflog"}}]"#, ""),
            // The request Claude Code sent, duplicating calls already logged.
            line("api-request-blob", "2.1.300", r#"[{"type":"tool_use","name":"Bash","input":{"command":"git reflog"}}]"#, ""),
            bash("g", "git reflog"),
        ]
        .concat();
        let (records, parser) = parse_all(input.as_bytes());
        assert_eq!(
            kinds_and_texts(&records),
            [
                (ObservationKind::AgentTool, "Skill git-x stash"),
                (ObservationKind::AgentCommand, "git reflog")
            ]
        );
        assert_eq!(parser.unreadable(), 0);
    }

    #[test]
    fn every_version_is_read_and_only_unreadable_lines_are_skipped() {
        let call = |version: &str, id: &str, command: &str| {
            let block = format!(
                r#"[{{"type":"tool_use","id":"{id}","name":"Bash","input":{{"command":"{command}"}}}}]"#
            );
            line("assistant", version, &block, "")
        };
        let input = [
            bash("1", "one"),
            "{not json\n".to_string(),
            // A Bash call with no command is not a shape this parser knows.
            line(
                "assistant",
                "2.1.300",
                r#"[{"type":"tool_use","name":"Bash","input":{}}]"#,
                "",
            ),
            call("2.1.400", "2", "newer"),
            call("2.0.1", "3", "older"),
            call("3.0.0-beta", "4", "unparsed version"),
        ]
        .concat();
        let (records, parser) = parse_all(input.as_bytes());
        assert_eq!(
            kinds_and_texts(&records),
            [
                (ObservationKind::AgentCommand, "one"),
                (ObservationKind::AgentCommand, "newer"),
                (ObservationKind::AgentCommand, "older"),
                (ObservationKind::AgentCommand, "unparsed version"),
            ]
        );
        assert_eq!(parser.unreadable(), 2);
        assert_eq!(parser.stable_len() as usize, input.len());

        let partial = bash("4", "four");
        let (records, parser) = parse_all(partial.trim_end().as_bytes());
        assert!(records.is_empty() && parser.incomplete_tail() && parser.stable_len() == 0);
    }

    #[test]
    fn timestamps_are_read_only_in_the_form_claude_code_writes() {
        assert_eq!(iso_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_seconds("2000-03-01T00:00:01.5Z"), Some(951_868_801));
        assert_eq!(iso_seconds("2026-11-06T04:11:27.506Z"), Some(1_793_938_287));
        for bad in [
            "2026-11-06 04:11:27Z",
            "2026-11-06T04:11:27+08:00",
            "2026-13-06T04:11:27Z",
            "2026-11-06T04:11:27.Z",
            "",
        ] {
            assert_eq!(iso_seconds(bad), None, "{bad}");
        }
    }
}
