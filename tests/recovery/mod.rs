//! The Git-recovery acceptance fixture of ADR 0016: a store built through
//! relic's own importers, holding how a lost commit and a deleted branch in
//! `shop` were recovered, and on request a held-out sibling, `notes`, whose
//! stash was dropped.
//!
//! Every leg is deterministic: seeded noise history, hook frames and a
//! Claude Code session written here, and repositories built by the real
//! `git` with a fixed clock and identity, so the same seed gives the same
//! records and the same shas on every run and machine.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// When the story starts, after the noise history ends.
const S: i64 = 1_700_100_000;
/// Seeded noise commands before the story.
pub const NOISE: usize = 2000;
/// Where the commands ran, as the hook and Claude Code saw it.
const HOME: &str = "/home/dev";
const SHOP: &str = "/home/dev/src/shop";
const NOTES: &str = "/home/dev/src/notes";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// The lost commit was put on `rescue`, and the hook saw it succeed.
    Answerable,
    /// The rescue was typed outside the repository: history holds the
    /// command, the hook saw it fail, and Git has no `rescue`.
    Contradictory,
}

pub struct Recovery {
    _dir: tempfile::TempDir,
    pub db: PathBuf,
    /// A home for skill links, empty until something is activated.
    pub home: PathBuf,
    pub shop: PathBuf,
    /// The sibling repository, built by [`Recovery::import_sibling`].
    pub notes: PathBuf,
    /// The commit `git reset --hard HEAD~1` lost.
    pub lost: String,
    /// The tip of `feature/coupons` when Claude Code deleted it.
    pub deleted: String,
    /// The stash dropped in `notes`, once the sibling is imported.
    pub stash: Option<String>,
    input: PathBuf,
}

/// One command as the user typed it: when, how long, its status and where.
struct Typed {
    at: i64,
    took: i64,
    status: i32,
    cwd: &'static str,
    command: String,
}

fn typed(at: i64, took: i64, status: i32, cwd: &'static str, command: impl Into<String>) -> Typed {
    Typed { at, took, status, cwd, command: command.into() }
}

impl Recovery {
    pub fn build(variant: Variant, seed: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (input, home) = (dir.path().join("in"), dir.path().join("home"));
        for d in [&input, &home, &input.join("hooks")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let shop = dir.path().join("shop");
        std::fs::create_dir(&shop).unwrap();
        let g = |at: i64, args: &[&str]| git(&shop, at, args);
        g(S, &["init", "-q", "-b", "main"]);
        g(S, &["commit", "-q", "--allow-empty", "-m", "add cart"]);
        g(S + 60, &["commit", "-q", "--allow-empty", "-m", "add checkout"]);
        g(S + 120, &["switch", "-q", "-c", "feature/coupons"]);
        g(S + 120, &["commit", "-q", "--allow-empty", "-m", "coupons: apply discount codes"]);
        let deleted = g(S + 120, &["rev-parse", "HEAD"]);
        g(S + 180, &["switch", "-q", "main"]);
        g(S + 240, &["commit", "-q", "--allow-empty", "-m", "WIP: round prices to the cent"]);
        let lost = g(S + 240, &["rev-parse", "HEAD"]);
        g(S + 300, &["reset", "-q", "--hard", "HEAD~1"]);
        if variant == Variant::Answerable {
            g(S + 420, &["branch", "rescue", "HEAD@{1}"]);
        }
        g(S + 600, &["branch", "-D", "feature/coupons"]);
        let short = &deleted[..7];
        g(S + 960, &["branch", "feature/coupons", short]);
        g(S + 990, &["switch", "-q", "feature/coupons"]);

        // In the contradictory variant the rescue ran in ~, not the repo.
        let (rescued, rescue_cwd) = match variant {
            Variant::Answerable => (0, SHOP),
            Variant::Contradictory => (128, HOME),
        };
        let story = [
            typed(S + 230, 0, 0, HOME, "cd ~/src/shop"),
            typed(S + 240, 0, 0, SHOP, "git commit -m 'WIP: round prices to the cent'"),
            typed(S + 300, 0, 0, SHOP, "git reset --hard HEAD~1"),
            typed(S + 330, 0, 0, SHOP, "git log --oneline -3"),
            typed(S + 360, 0, 0, SHOP, "git reflog"),
            typed(S + 420, 0, rescued, rescue_cwd, "git branch rescue HEAD@{1}"),
            typed(S + 480, 0, rescued, rescue_cwd, "git log --oneline -1 rescue"),
            typed(S + 900, 0, 128, SHOP, "git switch feature/coupons"),
            typed(S + 930, 0, 0, SHOP, "git reflog --all | grep coupons"),
            typed(S + 960, 0, 0, SHOP, format!("git branch feature/coupons {short}")),
            typed(S + 990, 0, 0, SHOP, "git switch feature/coupons"),
        ];
        let mut history = relic::fixture::generate_diverse_zsh_history(NOISE, seed);
        history.extend(zsh_history(&story));
        std::fs::write(input.join("zsh_history"), history).unwrap();
        std::fs::write(input.join("hooks/zsh-1700100230-4242-17.frames"), frames(&story)).unwrap();

        // Asked to tidy merged branches, the agent deleted an unmerged one.
        let project = input.join("claude/projects/-home-dev-src-shop");
        std::fs::create_dir_all(&project).unwrap();
        let session = [
            claude(S + 590, "user", r#""tidy up the branches that are already merged""#),
            claude(S + 600, "assistant", &bash_call("t1", "git branch --merged main")),
            claude(S + 601, "user", &bash_result("t1", "* main")),
            claude(S + 610, "assistant", &bash_call("t2", "git branch -D feature/coupons")),
            claude(S + 611, "user", &bash_result("t2", &format!("Deleted branch feature/coupons (was {short})."))),
        ]
        .concat();
        std::fs::write(project.join("6b0e3c1d-0d6e-4c43-9a3e-2f0c3d7e9a11.jsonl"), session).unwrap();

        let r = Self {
            db: dir.path().join("relic.db"),
            notes: dir.path().join("notes"),
            _dir: dir,
            home,
            shop,
            lost,
            deleted,
            stash: None,
            input,
        };
        r.import(&[r.input.join("zsh_history").to_str().unwrap(), "--shell", "zsh"]);
        r.import(&["--hooks", r.input.join("hooks").to_str().unwrap()]);
        r.import(&["--claude", r.input.join("claude/projects").to_str().unwrap()]);
        r.import(&[r.shop.to_str().unwrap()]);
        r
    }

    /// The held-out sibling: `notes` is imported with a stash in it, the
    /// stash is dropped, and history, hook and repository are imported
    /// again. Git no longer has the stash; the store still does.
    pub fn import_sibling(&mut self) {
        std::fs::create_dir(&self.notes).unwrap();
        let g = |at: i64, args: &[&str]| git(&self.notes, at, args);
        g(S + 5000, &["init", "-q", "-b", "main"]);
        std::fs::write(self.notes.join("post.md"), "# Release notes\n").unwrap();
        g(S + 5000, &["add", "post.md"]);
        g(S + 5000, &["commit", "-q", "-m", "outline the release post"]);
        std::fs::write(self.notes.join("post.md"), "# Release notes\n\nRelic 0.1 recalls what you did.\n").unwrap();
        g(S + 5060, &["stash", "push", "-q", "-m", "release post draft"]);
        let stash = g(S + 5060, &["rev-parse", "refs/stash"]);
        let story = [
            typed(S + 4990, 0, 0, HOME, "cd ~/src/notes"),
            typed(S + 5060, 0, 0, NOTES, "git stash push -m 'release post draft'"),
        ];
        append(&self.input.join("zsh_history"), &zsh_history(&story));
        self.import(&[self.input.join("zsh_history").to_str().unwrap(), "--shell", "zsh"]);
        self.import(&[self.notes.to_str().unwrap()]);

        g(S + 5600, &["stash", "drop", "-q"]);
        let dropped = [typed(S + 5600, 0, 0, NOTES, "git stash drop"), typed(S + 5640, 0, 0, NOTES, "git stash list")];
        append(&self.input.join("zsh_history"), &zsh_history(&dropped));
        let hooks = self.input.join("hooks");
        let typed: Vec<Typed> = story.into_iter().chain(dropped).collect();
        std::fs::write(hooks.join("zsh-1700104990-5151-3.frames"), frames(&typed)).unwrap();
        self.import(&[self.input.join("zsh_history").to_str().unwrap(), "--shell", "zsh"]);
        self.import(&["--hooks", hooks.to_str().unwrap()]);
        self.import(&[self.notes.to_str().unwrap()]);
        self.stash = Some(stash);
    }

    /// `relic` on this fixture's store, with its home.
    pub fn relic(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_relic"))
            .args(args)
            .env("RELIC_DB", &self.db)
            .env("HOME", &self.home)
            .output()
            .unwrap()
    }

    fn import(&self, args: &[&str]) {
        let out = self.relic(&[&["import"][..], args].concat());
        assert!(out.status.success(), "relic import {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
}

/// `git -C <repo> <args>` at `at` seconds, as one fixed person, with no
/// config but the repository's: its trimmed stdout.
fn git(repo: &Path, at: i64, args: &[&str]) -> String {
    let date = format!("@{at} +0000");
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .envs([("GIT_AUTHOR_NAME", "dev"), ("GIT_AUTHOR_EMAIL", "dev@example.com")])
        .envs([("GIT_COMMITTER_NAME", "dev"), ("GIT_COMMITTER_EMAIL", "dev@example.com")])
        .envs([("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)])
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn append(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    std::fs::OpenOptions::new().append(true).open(path).unwrap().write_all(bytes).unwrap();
}

/// zsh extended history lines.
fn zsh_history(story: &[Typed]) -> Vec<u8> {
    story.iter().map(|t| format!(": {}:{};{}\n", t.at, t.took, t.command)).collect::<String>().into_bytes()
}

/// What `relic hook zsh` writes for these commands.
fn frames(story: &[Typed]) -> Vec<u8> {
    story
        .iter()
        .map(|t| format!("rh1\0zsh\0{}.250\0{}.750\0{}\0{}\0{}\0\n", t.at, t.at + t.took, t.status, t.cwd, t.command))
        .collect::<String>()
        .into_bytes()
}

/// One Claude Code 2.1.318 record in `shop`, at `at` seconds.
fn claude(at: i64, kind: &str, content: &str) -> String {
    let time = relic::app::format_utc(at * 1000).replace(' ', "T");
    format!(
        r#"{{"type":"{kind}","version":"2.1.318","timestamp":"{time}:{:02}.000Z","cwd":"{SHOP}","gitBranch":"main","message":{{"content":{content}}}}}"#,
        at % 60
    ) + "\n"
}

fn bash_call(id: &str, command: &str) -> String {
    serde_json::json!([{"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command}}]).to_string()
}

fn bash_result(id: &str, output: &str) -> String {
    serde_json::json!([{"type": "tool_result", "tool_use_id": id, "content": output}]).to_string()
}
