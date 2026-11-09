//! What Relic's own shell hooks wrote ([`ZSH`], [`BASH`]): one file per
//! shell session, appended by that shell alone, one frame per command.
//!
//! A frame is seven fields, each ended by a NUL, then a newline:
//! `rh1`, the shell, start, end, exit status, directory, command. No field
//! holds a NUL (bash cannot store one, and the zsh hook strips them), so a
//! line ending in the seventh NUL ends a frame however many newlines the
//! command or directory held.
//!
//! The two shells see different things, and a record says only what its
//! shell saw: zsh hooks run before and after a command, so a zsh frame has
//! its start, duration and starting directory; bash's prompt hook runs only
//! after one, so a bash frame has when the prompt came back and the
//! directory the command left. Nothing missing is filled in.

use std::io::{self, BufRead};

use super::{Lines, RawRecord, decode};
use crate::domain::ObservationKind;

/// v1: the zsh and bash frames of `relic hook`'s first version.
pub const PARSER_CONTRACT: &str = "hook-v1";

/// What `relic hook zsh` prints.
pub const ZSH: &str = include_str!("hook.zsh");
/// What `relic hook bash` prints.
pub const BASH: &str = include_str!("hook.bash");

const MAGIC: &[u8] = b"rh1\0";
const FIELDS: usize = 7;

/// Whether the first bytes of a file are a hook frame.
pub fn looks_like(head: &[u8]) -> bool {
    head.starts_with(MAGIC)
}

/// Streams records out of one session's frames.
pub struct Parser<R> {
    lines: Lines<R>,
    frame: Vec<u8>,
    stable_len: u64,
    incomplete_tail: bool,
    unreadable: usize,
    /// Frames starting before this offset are read for the history number
    /// they leave behind, but not yielded.
    from: u64,
    /// Bash only: the history entry of the last frame, once one was read.
    /// The first frame is the prompt before any command, naming the last
    /// line of the history file the shell loaded. So is the first frame of
    /// a file the hook made again after it was deleted: without the frame
    /// before, a command cannot be told from a prompt without one, and
    /// dropping a command beats inventing one.
    last_history: Option<Option<(u64, Vec<u8>)>>,
}

impl<R: BufRead> Parser<R> {
    /// Parse a session from its first byte, yielding only the frames that
    /// start at `from` or later: whether a bash frame is a new command
    /// depends on the frame before it.
    pub fn resume(reader: R, from: u64) -> Self {
        Self {
            lines: Lines::new(reader, false),
            frame: Vec::new(),
            stable_len: 0,
            incomplete_tail: false,
            unreadable: 0,
            from,
            last_history: None,
        }
    }

    /// Byte end of the last frame read, kept or not.
    pub fn stable_len(&self) -> u64 {
        self.stable_len
    }

    /// The input ended inside a frame: the shell may still be writing it.
    pub fn incomplete_tail(&self) -> bool {
        self.incomplete_tail
    }

    /// Frames that were not what `relic hook` writes, or were cut at the
    /// record cap. Skipped.
    pub fn unreadable(&self) -> usize {
        self.unreadable
    }

    fn next_record(&mut self) -> io::Result<Option<RawRecord>> {
        loop {
            let start = self.lines.offset;
            self.frame.clear();
            let (mut cut, mut nuls) = (false, 0);
            loop {
                let before = self.frame.len();
                let Some(line) = self.lines.read_into(&mut self.frame)? else {
                    self.incomplete_tail |= !self.frame.is_empty();
                    return Ok(None);
                };
                if !line.terminated {
                    self.incomplete_tail = true;
                    return Ok(None);
                }
                cut |= line.cut;
                // Past the cap, NULs are dropped and cannot be counted.
                nuls += self.frame[before..].iter().filter(|&&b| b == 0).count();
                if line.last == Some(0) && (cut || nuls >= FIELDS) {
                    break;
                }
                if !cut {
                    self.frame.push(b'\n');
                }
            }
            self.stable_len = self.lines.offset;
            let end = self.lines.offset;
            let parsed = if cut { None } else { parse(&self.frame) };
            let Some(frame) = parsed else {
                self.unreadable += 1;
                continue;
            };
            // The number and the text: under HISTCONTROL=erasedups a new
            // command can take the number the last one had.
            let new = match frame.history {
                None => true,
                Some(number) => {
                    let entry = number.map(|n| (n, frame.command.to_vec()));
                    let new = self
                        .last_history
                        .as_ref()
                        .is_some_and(|seen| *seen != entry);
                    self.last_history = Some(entry);
                    new
                }
            };
            if !new
                || start < self.from
                || frame.command.is_empty()
                || frame.command.starts_with(b" ")
            {
                continue;
            }
            return Ok(Some(frame.into_record(start, end)));
        }
    }
}

impl<R: BufRead> Iterator for Parser<R> {
    type Item = io::Result<RawRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_record().transpose()
    }
}

struct Frame<'a> {
    /// zsh: when the command started and ended, in ms.
    span: Option<(i64, i64)>,
    /// bash: when the prompt came back, in seconds.
    prompt_at: Option<i64>,
    status: i32,
    cwd: &'a [u8],
    command: &'a [u8],
    /// bash: the history number, absent when the history was empty.
    history: Option<Option<u64>>,
}

impl Frame<'_> {
    fn into_record(self, start: u64, end: u64) -> RawRecord {
        let mut lossy = false;
        let command = decode(self.command.to_vec(), &mut lossy);
        // Shown on one line after the command, so a newline in a directory
        // name is escaped there: the line is a label, not the name's bytes.
        let cwd = decode(self.cwd.to_vec(), &mut lossy);
        lossy |= cwd.contains('\n');
        let cwd = cwd.replace('\n', "\\n");
        let facts = match self.span {
            Some((started, ended)) => {
                let took = (ended - started).max(0);
                format!(
                    "exit {} · took {}.{} s · cwd: {cwd}",
                    self.status,
                    took / 1000,
                    took % 1000 / 100
                )
            }
            None => format!("exit {} · cwd after: {cwd}", self.status),
        };
        RawRecord {
            kind: ObservationKind::HookFrame,
            timestamp: self
                .span
                .map(|(started, _)| started.div_euclid(1000))
                .or(self.prompt_at),
            duration: self
                .span
                .map(|(started, ended)| (ended - started).max(0) / 1000),
            command: format!("{command}\n{facts}"),
            start,
            end,
            lossy,
        }
    }
}

/// A hook record's text as its command and the line of what the hook saw.
/// No line when redaction took it, cut across it, into a marker: the whole
/// text is then the command, and nothing passes for what the hook saw.
pub fn split_facts(text: &str) -> (&str, Option<&str>) {
    if let Some((command, facts)) = text.rsplit_once('\n')
        && let Some(rest) = facts.strip_prefix("exit ")
    {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && rest[digits..].starts_with(" · ") {
            return (command, Some(facts));
        }
    }
    (text, None)
}

fn parse(frame: &[u8]) -> Option<Frame<'_>> {
    let fields: Vec<&[u8]> = frame.strip_suffix(b"\0")?.split(|&b| b == 0).collect();
    let [b"rh1", shell, started, ended, status, cwd, command] = fields[..] else {
        return None;
    };
    let status = std::str::from_utf8(status).ok()?.parse().ok()?;
    match shell {
        b"zsh" => Some(Frame {
            span: Some((millis(started)?, millis(ended)?)),
            prompt_at: None,
            status,
            cwd,
            command,
            history: None,
        }),
        b"bash" if started.is_empty() => {
            let prompt_at = std::str::from_utf8(ended).ok()?.parse().ok()?;
            let (history, command) = history_line(command)?;
            Some(Frame {
                span: None,
                prompt_at: Some(prompt_at),
                status,
                cwd,
                command,
                history: Some(history),
            })
        }
        _ => None,
    }
}

/// `$EPOCHREALTIME`, seconds with a fraction, as milliseconds.
fn millis(field: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(field).ok()?;
    let (secs, frac) = text.split_once('.').unwrap_or((text, ""));
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ms: i64 = format!("{frac:0<3}")[..3].parse().ok()?;
    secs.parse::<i64>().ok()?.checked_mul(1000)?.checked_add(ms)
}

/// Bash's `history 1`: the entry's number right-aligned, a `*` if it was
/// edited, two spaces in all, the entry, a newline. Nothing at all when the
/// history is empty.
fn history_line(line: &[u8]) -> Option<(Option<u64>, &[u8])> {
    if line.is_empty() {
        return Some((None, line));
    }
    let line = line.strip_suffix(b"\n")?.trim_ascii_start();
    let digits = line.iter().take_while(|b| b.is_ascii_digit()).count();
    let number = std::str::from_utf8(&line[..digits]).ok()?.parse().ok()?;
    let command = line[digits..]
        .strip_prefix(b"  ")
        .or_else(|| line[digits..].strip_prefix(b"* "))?;
    Some((Some(number), command))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zsh(start: &str, end: &str, status: &str, cwd: &str, cmd: &str) -> String {
        format!("rh1\0zsh\0{start}\0{end}\0{status}\0{cwd}\0{cmd}\0\n")
    }

    fn bash(at: &str, status: &str, cwd: &str, history: &str) -> String {
        format!("rh1\0bash\0\0{at}\0{status}\0{cwd}\0{history}\0\n")
    }

    fn records(input: &str, from: u64) -> (Vec<RawRecord>, Parser<&[u8]>) {
        let mut parser = Parser::resume(input.as_bytes(), from);
        let records = parser.by_ref().collect::<io::Result<_>>().unwrap();
        (records, parser)
    }

    #[test]
    fn a_zsh_frame_is_its_command_then_what_the_hook_saw() {
        let input = zsh(
            "1700000000.9871",
            "1700000003.2",
            "1",
            "/srv/app",
            "make deploy\n  --force",
        );
        let (records, parser) = records(&input, 0);
        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!(
            r.command,
            "make deploy\n  --force\nexit 1 · took 2.2 s · cwd: /srv/app"
        );
        // The start, not the end: zsh history times a command by its start.
        assert_eq!(
            (r.timestamp, r.duration, r.kind),
            (Some(1_700_000_000), Some(2), ObservationKind::HookFrame)
        );
        assert_eq!(
            (r.start, r.end, parser.stable_len(), r.lossy),
            (0, input.len() as u64, input.len() as u64, false)
        );
    }

    #[test]
    fn a_bash_frame_is_a_command_only_when_its_history_number_moved() {
        let input = [
            // The prompt before the first command: the history file's last line.
            bash("1700000000", "0", "/home/u", "  812  ssh old-box\n"),
            bash("1700000005", "2", "/srv", "  813  cd /srv && make\n"),
            // Enter on an empty line, or a command HISTCONTROL left out.
            bash("1700000009", "2", "/srv", "  813  cd /srv && make\n"),
            bash("1700000012", "0", "/srv", "  814* git status\n"),
            bash("1700000013", "0", "/srv", "  815   export TOKEN=x\n"),
            // erasedups: `make` moved to the end, so 815 again.
            bash("1700000014", "127", "/srv", "  815  make\n"),
        ]
        .concat();
        let (records, _) = records(&input, 0);
        let got: Vec<_> = records
            .iter()
            .map(|r| (r.timestamp, r.duration, r.command.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    Some(1_700_000_005),
                    None,
                    "cd /srv && make\nexit 2 · cwd after: /srv"
                ),
                (
                    Some(1_700_000_012),
                    None,
                    "git status\nexit 0 · cwd after: /srv"
                ),
                (
                    Some(1_700_000_014),
                    None,
                    "make\nexit 127 · cwd after: /srv"
                ),
            ]
        );
    }

    #[test]
    fn a_resumed_parse_still_knows_the_history_number_before_it() {
        let first = [
            bash("1", "0", "/", "    1  a\n"),
            bash("2", "0", "/", "    2  b\n"),
        ]
        .concat();
        let input = [
            first.as_str(),
            &bash("3", "0", "/", "    2  b\n"),
            &bash("4", "1", "/", "    3  c\n"),
        ]
        .concat();
        let (records, _) = records(&input, first.len() as u64);
        assert_eq!(
            records
                .iter()
                .map(|r| r.command.as_str())
                .collect::<Vec<_>>(),
            ["c\nexit 1 · cwd after: /"]
        );
        assert_eq!(
            records[0].start,
            input.len() as u64 - bash("4", "1", "/", "    3  c\n").len() as u64
        );
    }

    #[test]
    fn a_frame_still_being_written_is_withheld() {
        let done = zsh("1.0", "2.0", "0", "/", "a");
        let next = zsh("3.0", "4.0", "0", "/", "two\nlines");
        // 7 leaves `two` and its newline: a whole line, not a whole frame.
        for cut in [1, 5, 7] {
            let partial = &next[..next.len() - cut];
            let input = format!("{done}{partial}");
            let (records, parser) = records(&input, 0);
            assert_eq!(records.len(), 1, "cut {cut}");
            assert!(parser.incomplete_tail(), "cut {cut}");
            assert_eq!(parser.stable_len(), done.len() as u64, "cut {cut}");
        }
    }

    #[test]
    fn a_broken_frame_is_counted_and_the_next_one_read() {
        let input = [
            "rh1\0zsh\x001.0\0\n",
            &zsh("1.0", "2.0", "0", "/", "a"),
            &zsh("1.0", "2.0", "x", "/", "status not a number"),
            "rh1\0fish\x001.0\x002.0\x000\0/\0b\0\n",
            &zsh("5.0", "6.0", "0", "/", "kept"),
        ]
        .concat();
        let (records, parser) = records(&input, 0);
        // The fragment and the frame after it read as one frame of too many fields.
        assert_eq!(
            records
                .iter()
                .map(|r| r.command.lines().next().unwrap())
                .collect::<Vec<_>>(),
            ["kept"]
        );
        assert_eq!(parser.unreadable(), 3);
    }

    #[test]
    fn a_newline_in_the_directory_cannot_pass_for_the_facts_line() {
        let (records, _) = records(&zsh("1.0", "1.0", "0", "/tmp/a\nexit 0", "ls"), 0);
        assert_eq!(
            records[0].command,
            "ls\nexit 0 · took 0.0 s · cwd: /tmp/a\\nexit 0"
        );
        assert!(records[0].lossy);
    }

    #[test]
    fn zsh_times_keep_their_milliseconds() {
        assert_eq!(millis(b"1700000000.0509"), Some(1_700_000_000_050));
        assert_eq!(millis(b"1700000000.5"), Some(1_700_000_000_500));
        assert_eq!(millis(b"1700000000"), Some(1_700_000_000_000));
        assert_eq!(millis(b"1700000000.-5"), None);
    }
}
