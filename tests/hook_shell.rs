//! The hooks `relic hook` prints, run in a real interactive zsh and bash on
//! a terminal, beside a prompt hook of the user's own, then imported: what
//! each command left is its command line, exit status, directory and the
//! times its shell can know, and nothing for a prompt with no command.

#![cfg(unix)]

#[expect(dead_code, reason = "the relic ui helpers are unused here")]
mod pty_support;

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use pty_support::{RELIC, Session, serial};

const PROMPT: &str = "<P>";

struct Shell {
    dir: tempfile::TempDir,
    work: PathBuf,
}

impl Shell {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // Resolved: the shells report $PWD with the links in TMPDIR followed.
        let work = std::fs::canonicalize(dir.path()).unwrap().join("work");
        std::fs::create_dir(&work).unwrap();
        Self { dir, work }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Types each line at a prompt and waits for the next one, then exits.
    /// What the terminal showed.
    fn run(
        &self,
        program: &str,
        args: &[&OsStr],
        env: &[(&str, &OsStr)],
        lines: &[&str],
    ) -> String {
        let state = self.path("state");
        let mut env = env.to_vec();
        env.push(("XDG_STATE_HOME", state.as_os_str()));
        let mut s = Session::spawn_program(program.as_ref(), args, &env);
        s.wait_for("the first prompt", |o| o.count(PROMPT.as_bytes()) >= 1);
        for (prompts, line) in (2..).zip(lines) {
            s.send(format!("{line}\n").as_bytes());
            s.wait_for(&format!("a prompt after {line:?}"), |o| {
                o.count(PROMPT.as_bytes()) >= prompts
            });
        }
        s.send(b"exit\n");
        s.wait_exit();
        s.with(|o| String::from_utf8_lossy(&o.raw).into_owned())
    }

    fn hooks(&self) -> PathBuf {
        self.path("state/relic/hooks")
    }

    /// `relic import --hooks`, then every stored record's text and time.
    fn import(&self) -> Vec<(String, Option<i64>)> {
        let db = self.path("relic.db");
        let out = Command::new(RELIC)
            .args(["import", "--hooks"])
            .arg(self.hooks())
            .arg("--db")
            .arg(&db)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let conn = rusqlite::Connection::open(&db).unwrap();
        let mut stmt = conn
            .prepare("SELECT safe_text, observed_at_ms FROM observation ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// The one session file, owner-only in an owner-only directory.
    fn assert_private(&self) {
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let files: Vec<PathBuf> = std::fs::read_dir(self.hooks())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(
            (
                mode(&self.hooks()),
                mode(&self.path("state/relic")),
                mode(&files[0])
            ),
            (0o700, 0o700, 0o600)
        );
    }
}

fn hook(shell: &str) -> String {
    let out = Command::new(RELIC).args(["hook", shell]).output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

fn found(program: &str) -> bool {
    if Path::new(program).exists() {
        return true;
    }
    #[expect(
        clippy::explicit_write,
        reason = "libtest captures the print macros of a passing test, and a skip nobody sees reads as a proof"
    )]
    writeln!(std::io::stderr(), "SKIPPED: no {program}").unwrap();
    false
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[test]
fn zsh_records_status_start_duration_and_starting_directory() {
    let _s = serial();
    if !found("/bin/zsh") {
        return;
    }
    let sh = Shell::new();
    std::fs::write(sh.path("hook.zsh"), hook("zsh")).unwrap();
    // The user's own precmd runs after relic's and still sees the status,
    // and their TRAPZERR runs once per failing command.
    let rc = format!(
        "PS1='{PROMPT}' PS2='{PROMPT}'\nprecmd() {{ print -r -- \"mine:$?\" }}\nTRAPZERR() {{ print -r -- zerr }}\nsource {}\n",
        sh.path("hook.zsh").display()
    );
    std::fs::write(sh.path(".zshrc"), rc).unwrap();
    let w = sh.work.display().to_string();
    let before = now();
    let shown = sh.run(
        "/bin/zsh",
        &["-i".as_ref()],
        &[("ZDOTDIR", sh.path("").as_os_str())],
        &[
            // Made again owner-only, not under the user's umask.
            "rm $_relic_hook_file",
            &format!("cd {w}"),
            "false",
            "",
            " echo hidden",
            "sleep 1.3; (exit 3)",
            "cd /",
            "print -r -- \"two",
            "lines\"",
            // zsh lets a NUL into a command line, which would end a field.
            "print -z $'print -r -- a\\0b'",
            "",
        ],
    );
    let after = now();
    assert!(
        shown.contains("mine:1") && shown.contains("mine:3") && !shown.contains("rh1"),
        "{shown}"
    );
    assert_eq!(shown.matches("zerr").count(), 2, "{shown}");
    sh.assert_private();
    // Left out before it reaches the disk, not at import.
    let file = std::fs::read_dir(sh.hooks())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(!String::from_utf8_lossy(&std::fs::read(file).unwrap()).contains("hidden"));
    let records = sh.import();
    let shown: Vec<(&str, &str)> = records
        .iter()
        .map(|(t, _)| t.rsplit_once('\n').unwrap())
        .collect();
    let commands: Vec<&str> = shown.iter().map(|(c, _)| *c).collect();
    assert_eq!(
        commands,
        [
            "rm $_relic_hook_file",
            &format!("cd {w}"),
            "false",
            "sleep 1.3; (exit 3)",
            "cd /",
            "print -r -- \"two\nlines\"",
            "print -z $'print -r -- a\\0b'",
            "print -r -- ab",
        ]
    );
    let facts: Vec<&str> = shown.iter().map(|(_, f)| *f).collect();
    // A cd is recorded where it started, not where it left.
    let started = std::env::current_dir().unwrap().canonicalize().unwrap();
    assert!(
        facts[1].starts_with("exit 0 · took 0.")
            && facts[1].ends_with(&format!(" · cwd: {}", started.display())),
        "{facts:?}"
    );
    assert!(
        facts[2].starts_with("exit 1 · took 0.") && facts[2].ends_with(&format!(" · cwd: {w}")),
        "{facts:?}"
    );
    let took: f64 = facts[3]
        .strip_prefix("exit 3 · took ")
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    // No upper bound: the wall clock can step forward meanwhile.
    assert!(took >= 1.3, "{facts:?}");
    assert!(
        facts[4].ends_with(&format!(" · cwd: {w}")) && facts[5].ends_with(" · cwd: /"),
        "{facts:?}"
    );
    let at: Vec<i64> = records.iter().map(|(_, t)| t.unwrap()).collect();
    // To the second: the start, so the sleep's record is a second before the next one's.
    assert!(
        at.windows(2).all(|w| w[0] <= w[1]) && at[0] >= before / 1000 * 1000 && at[7] <= after,
        "{at:?}"
    );
    assert!(at[4] - at[3] >= 1000, "{at:?}");
}

#[test]
fn bash_records_status_and_the_directory_a_command_left_once_each() {
    let _s = serial();
    if !found("/bin/bash") {
        return;
    }
    let sh = Shell::new();
    std::fs::write(sh.path("history"), "old one\nold two\n").unwrap();
    std::fs::write(sh.path("hook.bash"), hook("bash")).unwrap();
    // The user's own PROMPT_COMMAND still runs, after relic's, and still
    // sees the status, and their ERR trap runs once per failing command.
    let rc = format!(
        "PS1='{PROMPT}' PS2='{PROMPT}'\nHISTFILE={}\nPROMPT_COMMAND='echo mine:$?'\ntrap 'echo trapped' ERR\neval \"$(cat {})\"\n",
        sh.path("history").display(),
        sh.path("hook.bash").display()
    );
    std::fs::write(sh.path("rc"), rc).unwrap();
    let w = sh.work.display().to_string();
    let before = now();
    let shown = sh.run(
        "/bin/bash",
        &[
            "--noprofile".as_ref(),
            "--rcfile".as_ref(),
            sh.path("rc").as_os_str(),
            "-i".as_ref(),
        ],
        &[],
        &[
            "rm $_relic_hook_file",
            &format!("cd {w}"),
            "false",
            "",
            " echo hidden",
            "sh -c 'exit 3'",
            "for i in 1 2",
            "do :; done",
        ],
    );
    let after = now();
    assert!(
        shown.contains("mine:1") && shown.contains("mine:3") && !shown.contains("rh1"),
        "{shown}"
    );
    assert_eq!(shown.matches("trapped").count(), 2, "{shown}");
    sh.assert_private();
    let records = sh.import();
    let texts: Vec<&str> = records.iter().map(|(t, _)| t.as_str()).collect();
    // Neither the history file's last line, read at the first prompt, nor
    // the empty line, nor the line starting with a space. Nor the rm, whose
    // frame is the first in the file it made again: with no frame before
    // it, it cannot be told from a prompt without a command.
    assert_eq!(
        texts,
        [
            format!("cd {w}\nexit 0 · cwd after: {w}"),
            format!("false\nexit 1 · cwd after: {w}"),
            format!("sh -c 'exit 3'\nexit 3 · cwd after: {w}"),
            format!("for i in 1 2; do :; done\nexit 0 · cwd after: {w}"),
        ]
    );
    let at: Vec<i64> = records.iter().map(|(_, t)| t.unwrap()).collect();
    assert!(
        at.iter()
            .all(|t| (before / 1000 * 1000..=after).contains(t)),
        "{at:?} not in {before}..{after}"
    );
}
