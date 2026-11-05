//! Validation: running a verifier the user wrote, and nothing else (ADR
//! 0012). The argv is the user's, fixed, with no shell; it runs in the cwd
//! the user chose, under the umask the user started Relic with, with its
//! time and output bounded. Relic never runs a recalled procedure.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::redact::Redactor;
use crate::subprocess::{Run, bounded};

/// How long a verifier may run.
pub const TIMEOUT: Duration = Duration::from_secs(60);
/// Most bytes of its stdout read; the output is not kept.
const OUTPUT_MAX: usize = 1 << 20;

/// What a verifier's workspace is: the user's own directory, isolated from
/// nothing. Never called a sandbox.
pub const UNISOLATED: &str = "unisolated";

/// The umask Relic started with, before it set its own `0o077`; `UNSET`
/// until `main` says, and then a verifier keeps the current one.
static ORIGINAL_UMASK: AtomicU32 = AtomicU32::new(UNSET);
const UNSET: u32 = u32::MAX;

/// Remember the umask Relic replaced, for verifiers to run under.
pub fn set_original_umask(mask: libc::mode_t) {
    ORIGINAL_UMASK.store(u32::from(mask), Ordering::Relaxed);
}

/// The umask the user started Relic with, if `main` recorded it.
pub fn original_umask() -> Option<libc::mode_t> {
    let mask = ORIGINAL_UMASK.load(Ordering::Relaxed);
    (mask != UNSET).then_some(mask as libc::mode_t)
}

/// How a verifier run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Passed,
    Failed,
    TimedOut,
    TooLarge,
}

/// One verifier run, as stored: its command line and cwd redacted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationRecord {
    /// The argv shell-quoted into one line, then redacted.
    pub command: String,
    pub cwd: String,
    pub outcome: Outcome,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub relic_version: String,
    pub assurance: String,
}

impl ValidationRecord {
    /// Only a passing verifier confirms anything (ADR 0012).
    pub fn confirmed(&self) -> bool {
        self.outcome == Outcome::Passed
    }
}

/// Run `argv` in `cwd`. Also the first line the verifier wrote to stderr,
/// redacted, for a run that did not pass.
pub fn run(argv: &[String], cwd: &Path) -> std::io::Result<(ValidationRecord, Option<String>)> {
    use std::os::unix::process::CommandExt;
    let (program, args) = argv.split_first().ok_or_else(|| std::io::Error::other("a verifier needs a program"))?;
    let redactor = Redactor::new();
    if !cwd.is_dir() {
        return Err(std::io::Error::other(format!("{}: not a directory", redactor.import(&cwd.display().to_string()))));
    }
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(cwd);
    if let Some(mask) = original_umask() {
        // SAFETY: umask(2) is async-signal-safe and touches no memory.
        unsafe {
            cmd.pre_exec(move || {
                libc::umask(mask);
                Ok(())
            })
        };
    }
    let started = Instant::now();
    let run = bounded(cmd, TIMEOUT, OUTPUT_MAX)
        .map_err(|e| std::io::Error::new(e.kind(), format!("could not run {}: {e}", redactor.import(program))))?;
    let (outcome, exit_code, said) = match run {
        Run::Done { status, stderr, .. } => {
            let first = String::from_utf8_lossy(&stderr).lines().find(|l| !l.trim().is_empty()).map(|l| redactor.redact(l.trim()));
            let outcome = if status.success() { Outcome::Passed } else { Outcome::Failed };
            (outcome, status.code(), first.filter(|_| outcome != Outcome::Passed))
        }
        Run::TimedOut => (Outcome::TimedOut, None, None),
        Run::TooLarge => (Outcome::TooLarge, None, None),
    };
    let record = ValidationRecord {
        command: redactor.import(&command_line(argv)),
        cwd: redactor.import(&cwd.display().to_string()),
        outcome,
        exit_code,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        relic_version: env!("CARGO_PKG_VERSION").into(),
        assurance: UNISOLATED.into(),
    };
    Ok((record, said))
}

/// `argv` as one shell-quoted line: redacting each argument alone would
/// never see `--password` beside its value.
fn command_line(argv: &[String]) -> String {
    let quote = |a: &String| {
        if !a.is_empty() && a.bytes().all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./_-".contains(&b)) {
            a.clone()
        } else {
            format!("'{}'", a.replace('\'', r"'\''"))
        }
    };
    argv.iter().map(quote).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_verifier_runs_its_argv_unshelled_in_the_chosen_cwd_under_the_users_umask() {
        let dir = tempfile::tempdir().unwrap();
        set_original_umask(0o027);
        // A shell metacharacter is an argument, not syntax.
        let (r, _) = run(&argv(&["touch", "made", "x; touch injected"]), dir.path()).unwrap();
        assert_eq!((r.outcome, r.exit_code, r.assurance.as_str()), (Outcome::Passed, Some(0), UNISOLATED));
        assert!(!dir.path().join("injected").exists());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("made")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "the user's umask, not Relic's 077");
    }

    #[test]
    fn a_failing_verifier_confirms_nothing_and_its_argv_is_stored_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let script = "echo 'no such ref' >&2; exit 3";
        let (r, said) = run(&argv(&["sh", "-c", script, "x", "--token=Zq9CanaryJoined1", "--password", "Zq9CanaryApart2"]), dir.path()).unwrap();
        assert_eq!((r.outcome, r.exit_code, r.confirmed()), (Outcome::Failed, Some(3), false));
        assert_eq!(said.as_deref(), Some("no such ref"));
        assert!(!r.command.contains("Zq9Canary"), "{}", r.command);
        assert!(r.command.starts_with("sh -c 'echo '\\''no such ref'\\'' >&2; exit 3' x --token="), "{}", r.command);

        let err = run(&argv(&["true"]), &dir.path().join("gone")).unwrap_err().to_string();
        assert!(err.ends_with("/gone: not a directory"), "{err}");
        let err = run(&argv(&["no-such-verifier-zq"]), dir.path()).unwrap_err().to_string();
        assert!(err.starts_with("could not run no-such-verifier-zq: "), "{err}");
    }
}
