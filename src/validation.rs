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

/// The umask Relic started with, before it set its own `0o077`.
static ORIGINAL_UMASK: AtomicU32 = AtomicU32::new(0o022);

/// Remember the umask Relic replaced, for verifiers to run under.
pub fn set_original_umask(mask: libc::mode_t) {
    ORIGINAL_UMASK.store(u32::from(mask), Ordering::Relaxed);
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

/// One verifier run, as stored: argv and cwd redacted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationRecord {
    pub argv: Vec<String>,
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
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(cwd);
    let mask = ORIGINAL_UMASK.load(Ordering::Relaxed) as libc::mode_t;
    // SAFETY: umask(2) is async-signal-safe and touches no memory.
    unsafe {
        cmd.pre_exec(move || {
            libc::umask(mask);
            Ok(())
        })
    };
    let started = Instant::now();
    let run = bounded(cmd, TIMEOUT, OUTPUT_MAX)?;
    let redactor = Redactor::new();
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
        argv: argv.iter().map(|a| redactor.import(a)).collect(),
        cwd: redactor.import(&cwd.display().to_string()),
        outcome,
        exit_code,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        relic_version: env!("CARGO_PKG_VERSION").into(),
        assurance: UNISOLATED.into(),
    };
    Ok((record, said))
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
        let (r, said) = run(&argv(&["sh", "-c", "echo 'no such ref' >&2; exit 3", "--password=hunter2hunter2"]), dir.path()).unwrap();
        assert_eq!((r.outcome, r.exit_code, r.confirmed()), (Outcome::Failed, Some(3), false));
        assert_eq!(said.as_deref(), Some("no such ref"));
        assert!(!r.argv.concat().contains("hunter2"), "{:?}", r.argv);
    }
}
