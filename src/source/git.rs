//! A local Git repository as a source: its reflogs (HEAD, branches, stash)
//! and its refs, read through the installed `git` with fixed argv, no
//! shell, a sanitized environment, bounded output and a timeout (ADR 0001).
//! Never its files, diffs or commit bodies.
//!
//! Git is enrichment, not a command log: a reflog entry says a ref moved,
//! not which command moved it (research, source contracts).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const PARSER_CONTRACT: &str = "git-v1";

/// How long one `git` run may take.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Most bytes of one run's output. About 100k reflog entries.
const OUTPUT_MAX: usize = 16 << 20;
/// Most bytes of `git`'s stderr kept for an error message.
const STDERR_MAX: usize = 4096;
/// Most bytes of one record's text: a ref name and a subject, not a page.
pub const RECORD_MAX: usize = 4096;

/// What one record is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    /// A ref moved: when Git says it did.
    Reflog,
    /// Where a ref pointed when it was imported.
    Ref,
}

/// One raw record, before redaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRecord {
    pub kind: RecordKind,
    /// Seconds since the epoch, for a reflog entry.
    pub timestamp: Option<i64>,
    pub text: String,
    /// `text` was cut at [`RECORD_MAX`] or was not valid UTF-8.
    pub lossy: bool,
}

#[derive(Debug)]
pub enum GitError {
    /// No `git` on `PATH`.
    NotInstalled,
    TimedOut { args: String },
    TooLarge { args: String },
    /// Lines of `git`'s output that are no record: not the git Relic
    /// asked, or a format it does not know.
    Unreadable { args: String, lines: usize },
    /// `git` ran and failed; `stderr` is its first line.
    Failed { args: String, stderr: String },
    Io(std::io::Error),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => f.write_str("git is not installed, or not on PATH"),
            Self::TimedOut { args } => write!(f, "git {args} took over {} s; stopped it", TIMEOUT.as_secs()),
            Self::TooLarge { args } => write!(f, "git {args} printed over {} MiB; stopped it", OUTPUT_MAX >> 20),
            Self::Unreadable { args, lines } => write!(f, "git {args} printed {lines} lines that are no record; stored nothing"),
            Self::Failed { args, stderr } => write!(f, "git {args} failed: {stderr}"),
            Self::Io(e) => write!(f, "running git: {e}"),
        }
    }
}

impl std::error::Error for GitError {}

/// The repository's absolute common Git directory, which identifies it:
/// the same from its top level, any subdirectory, the `.git` directory
/// itself or a linked worktree. A worktree's own Git directory would make
/// it a second source holding every reflog entry again. As bytes: a path
/// need not be UTF-8, and a lossy one names no directory.
fn git_dir(repo: &Path) -> Result<PathBuf, GitError> {
    use std::os::unix::ffi::OsStrExt;
    let out = git(repo, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(out.strip_suffix(b"\n").unwrap_or(&out))))
}

/// A repository as read: what identifies it, and its records.
pub struct Repository {
    pub git_dir: PathBuf,
    /// Every reflog entry, oldest first, then every ref.
    pub records: Vec<RawRecord>,
}

/// Read the repository at `repo`: its top level, a subdirectory, its Git
/// directory or a linked worktree.
pub fn read(repo: &Path) -> Result<Repository, GitError> {
    let git_dir = git_dir(repo)?;
    let records = records(&git_dir)?;
    Ok(Repository { git_dir, records })
}

fn records(repo: &Path) -> Result<Vec<RawRecord>, GitError> {
    // `%gD` with unix dates is `<ref>@{<seconds>}`; a ref name cannot hold
    // `@{`. One entry per line: a reflog subject is one line.
    let reflog = git(repo, &["log", "--walk-reflogs", "--all", "--date=unix", "--format=%gD%x00%H%x00%gs"])?;
    // `git log -g --all` is newest first, every ref's entries interleaved
    // by when they were written: reversed, the order things moved in.
    let mut records = parse(&reflog, "log", reflog_entry)?;
    records.reverse();
    let refs = git(repo, &["for-each-ref", "--format=%(refname)%00%(objectname)%00%(contents:subject)"])?;
    records.extend(parse(&refs, "for-each-ref", ref_entry)?);
    Ok(records)
}

/// One record per line of `out`, or an error naming how many lines are
/// none: an import that skipped them would look complete.
fn parse(out: &[u8], args: &str, entry: fn(&[u8]) -> Option<RawRecord>) -> Result<Vec<RawRecord>, GitError> {
    let lines: Vec<&[u8]> = out.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect();
    let records: Vec<RawRecord> = lines.iter().filter_map(|l| entry(l)).collect();
    match lines.len() - records.len() {
        0 => Ok(records),
        lines => Err(GitError::Unreadable { args: args.into(), lines }),
    }
}

fn fields(line: &[u8]) -> Option<[&[u8]; 3]> {
    let mut it = line.splitn(3, |&b| b == 0);
    Some([it.next()?, it.next()?, it.next()?])
}

fn reflog_entry(line: &[u8]) -> Option<RawRecord> {
    let [selector, oid, subject] = fields(line)?;
    let selector = std::str::from_utf8(selector).ok()?;
    let (name, at) = selector.strip_suffix('}')?.rsplit_once("@{")?;
    let timestamp = at.parse().ok();
    Some(record(RecordKind::Reflog, timestamp, &[b"git reflog ", name.as_bytes(), b" ", oid, b" ", subject]))
}

fn ref_entry(line: &[u8]) -> Option<RawRecord> {
    let [name, oid, subject] = fields(line)?;
    Some(record(RecordKind::Ref, None, &[b"git ref ", name, b" ", oid, b" ", subject]))
}

fn record(kind: RecordKind, timestamp: Option<i64>, parts: &[&[u8]]) -> RawRecord {
    let bytes = parts.concat();
    let mut lossy = false;
    let mut text = String::from_utf8(bytes).unwrap_or_else(|e| {
        lossy = true;
        String::from_utf8_lossy(e.as_bytes()).into_owned()
    });
    if text.len() > RECORD_MAX {
        text.truncate(text.floor_char_boundary(RECORD_MAX));
        lossy = true;
    }
    RawRecord { kind, timestamp, text, lossy }
}

/// `git -C <repo> <args>`: its stdout, or why there is none.
fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let mut cmd = Command::new("git");
    // No system or global config, so no pager, alias or signature check
    // the user set elsewhere runs; the repository's own config still
    // applies, as Git needs it to read refs. These commands never touch the
    // index or the work tree, which is what fsmonitor and hooks act on.
    cmd.args(["--no-pager", "--no-optional-locks", "-c", "log.showSignature=false", "-C"])
        .arg(repo)
        .args(args)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    // Passed on only when set: an empty PATH searches the current
    // directory, where anyone could have left a `git`. HOME expands a `~` in
    // the repository's own config; GIT_CONFIG_GLOBAL still keeps
    // ~/.gitconfig out.
    for var in ["PATH", "HOME"] {
        if let Some(value) = std::env::var_os(var) {
            cmd.env(var, value);
        }
    }
    let shown = args.first().copied().unwrap_or_default().to_string();
    match bounded(cmd, TIMEOUT, OUTPUT_MAX) {
        Ok(Run::Done { status, stdout, .. }) if status.success() => Ok(stdout),
        Ok(Run::Done { stderr, .. }) => {
            let stderr = String::from_utf8_lossy(&stderr);
            let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("no message");
            Err(GitError::Failed { args: shown, stderr: crate::redact::Redactor::new().redact(first.trim()) })
        }
        Ok(Run::TimedOut) => Err(GitError::TimedOut { args: shown }),
        Ok(Run::TooLarge) => Err(GitError::TooLarge { args: shown }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(GitError::NotInstalled),
        Err(e) => Err(GitError::Io(e)),
    }
}

enum Run {
    Done { status: std::process::ExitStatus, stdout: Vec<u8>, stderr: Vec<u8> },
    TimedOut,
    TooLarge,
}

/// Run `cmd` with no stdin, stopping it once `timeout` passes or its stdout
/// passes `max` bytes. The timeout covers its output too: a process it
/// started can hold the pipes open after it exits.
fn bounded(mut cmd: Command, timeout: Duration, max: usize) -> std::io::Result<Run> {
    use std::os::unix::process::CommandExt;
    let deadline = Instant::now() + timeout;
    // Its own group, so a timeout stops what it started as well.
    let mut child = cmd.process_group(0).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let stdout = capped(child.stdout.take(), max, false);
    let stderr = capped(child.stderr.take(), STDERR_MAX, true);
    let stop = |child: &mut std::process::Child| {
        let group = libc::pid_t::try_from(child.id()).map_err(std::io::Error::other)?;
        // SAFETY: kill(2) of the group this child leads; at worst ESRCH.
        unsafe { libc::kill(-group, libc::SIGKILL) };
        child.wait()?;
        Ok(Run::TimedOut)
    };
    let read = |rx: &mpsc::Receiver<_>| match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(read) => Ok(Some(read)),
        Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::other("a git output reader panicked")),
    };
    let (Some(out), Some(err)) = (read(&stdout)?, read(&stderr)?) else {
        return stop(&mut child);
    };
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            return stop(&mut child);
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let stdout = out?;
    if stdout.len() > max {
        return Ok(Run::TooLarge);
    }
    Ok(Run::Done { status, stdout, stderr: err.unwrap_or_default() })
}

/// The first `keep` bytes of `pipe` (stdout: one more, to tell a cut), read
/// on a thread. Stdout is dropped at the cap, so a writer past it dies of
/// EPIPE; stderr is read to its end, so a run that only warns a lot still
/// succeeds.
fn capped<R: Read + Send + 'static>(pipe: Option<R>, keep: usize, drain: bool) -> mpsc::Receiver<std::io::Result<Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let read = pipe.map_or(Ok(()), |mut p| {
            let limit = if drain { keep } else { keep + 1 };
            p.by_ref().take(limit as u64).read_to_end(&mut out)?;
            if drain {
                std::io::copy(&mut p, &mut std::io::sink())?;
            }
            Ok(())
        });
        let _ = tx.send(read.map(|()| out));
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn a_run_is_stopped_at_its_timeout_and_at_its_output_cap() {
        let t = Instant::now();
        assert!(matches!(bounded(sh("sleep 5"), Duration::from_millis(100), 64).unwrap(), Run::TimedOut));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());

        let t = Instant::now();
        assert!(matches!(bounded(sh("yes"), Duration::from_secs(5), 1000).unwrap(), Run::TooLarge));
        assert!(t.elapsed() < Duration::from_secs(2), "the writer died at the cap: {:?}", t.elapsed());

        let Run::Done { status, stdout, stderr } = bounded(sh("printf out; echo why >&2; exit 3"), Duration::from_secs(5), 3).unwrap() else {
            panic!()
        };
        assert_eq!((status.code(), stdout.as_slice(), stderr.as_slice()), (Some(3), &b"out"[..], &b"why\n"[..]));

        // Exited, but what it started still holds stdout open.
        let t = Instant::now();
        assert!(matches!(bounded(sh("sleep 5 & exit 0"), Duration::from_millis(100), 64).unwrap(), Run::TimedOut));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());

        // Warnings past what is kept do not kill it.
        let warns = "i=0; while [ $i -lt 2000 ]; do echo warning >&2; i=$((i+1)); done; printf done";
        let Run::Done { status, stdout, stderr } = bounded(sh(warns), Duration::from_secs(5), 64).unwrap() else { panic!() };
        assert_eq!((status.code(), stdout.as_slice(), stderr.len()), (Some(0), &b"done"[..], STDERR_MAX));
    }

    #[test]
    fn records_are_one_line_each_and_cut_at_their_cap() {
        let r = reflog_entry(b"refs/heads/main@{1700000000}\0abc\0commit: fix").unwrap();
        assert_eq!((r.kind, r.timestamp, r.text.as_str(), r.lossy), (RecordKind::Reflog, Some(1_700_000_000), "git reflog refs/heads/main abc commit: fix", false));
        assert_eq!(reflog_entry(b"HEAD\0abc\0no time").map(|r| r.text), None);
        let out = b"HEAD@{1}\0abc\0one\n/some/path\nHEAD@{2}\0abc\0two\n";
        assert!(matches!(parse(out, "log", reflog_entry), Err(GitError::Unreadable { lines: 1, .. })));
        assert_eq!(parse(b"HEAD@{1}\0abc\0one\n", "log", reflog_entry).unwrap().len(), 1);
        let long = [b"refs/heads/x\0abc\0".as_slice(), &[b'y'; RECORD_MAX], b"\xff"].concat();
        let r = ref_entry(&long).unwrap();
        assert!(r.lossy && r.text.len() <= RECORD_MAX && r.text.starts_with("git ref refs/heads/x abc yyy"));
    }
}
