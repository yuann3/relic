//! `relic import` and `relic forget` as processes: a busy store, relative path
//! spellings, the shell a file is read as, what a failed import leaves
//! behind, rescrub, forgetting, file modes, and how a Git repository's
//! import runs git.

use std::process::Command;

#[test]
fn import_into_a_busy_store_exits_2_with_a_busy_message() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("relic.db");
    let hist = dir.path().join("zsh_history");
    std::fs::write(&hist, ": 1700000000:0;echo one\n").unwrap();
    drop(relic::store::Store::open(&db).unwrap());

    let writer = rusqlite::Connection::open(&db).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let import = || {
        Command::new(env!("CARGO_BIN_EXE_relic"))
            .arg("import")
            .arg(&hist)
            .arg("--db")
            .arg(&db)
            .output()
            .unwrap()
    };

    let out = import();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.starts_with("relic: busy: "), "{stderr}");

    writer.execute_batch("ROLLBACK").unwrap();
    let out = import();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn three_spellings_of_one_file_are_one_source() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let db = dir.path().join("relic.db");
    std::fs::write(dir.path().join("h"), ": 1700000000:0;echo one\n: 1700000001:0;echo two\n").unwrap();

    let absolute = dir.path().join("h");
    for spelling in ["./h", absolute.to_str().unwrap(), "sub/../h", "h"] {
        let out = Command::new(env!("CARGO_BIN_EXE_relic"))
            .current_dir(dir.path())
            .args(["import", spelling, "--db"])
            .arg(&db)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{spelling}: {}", String::from_utf8_lossy(&out.stderr));
    }

    let conn = rusqlite::Connection::open(&db).unwrap();
    let count = |table: &str| -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    };
    assert_eq!((count("source"), count("source_revision"), count("observation")), (1, 1, 2));
}

fn import_in(dir: &std::path::Path, args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_relic"))
        .current_dir(dir)
        .arg("import")
        .args(args)
        .args(["--db", "data/relic.db"])
        .output()
        .unwrap();
    (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn a_differing_shell_replaces_the_source_and_says_so_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("history"), "#1700000000\necho alpha\n#1700000001\necho beta\n").unwrap();

    let (code, stderr) = import_in(dir.path(), &["history", "--shell", "zsh"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stderr.contains("history: stored 4 zsh commands"), "{stderr}");

    let (code, stderr) = import_in(dir.path(), &["history", "--shell", "bash"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        stderr.contains("relic: history: was imported as zsh; replaced its 4 stored commands with a bash import"),
        "{stderr}"
    );
    assert!(stderr.contains("history: stored 2 bash commands"), "{stderr}");
}

#[test]
fn each_import_says_what_it_did_with_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let hist = dir.path().join(".zsh_history");
    let lines = |range: std::ops::Range<u32>| -> String {
        range.map(|i| format!(": {}:0;echo line-{i}\n", 1_700_000_000 + i)).collect()
    };
    std::fs::write(&hist, lines(0..30)).unwrap();
    let import = || import_in(dir.path(), &[".zsh_history"]).1;

    assert!(import().contains(".zsh_history: stored 30 zsh commands in "));
    assert!(import().contains(".zsh_history: unchanged since last import"));
    std::fs::write(&hist, lines(0..32)).unwrap();
    assert!(import().contains(".zsh_history: added 2 new commands"));
    std::fs::write(&hist, lines(10..33)).unwrap();
    assert!(import().contains("the file was rewritten; added 1 new commands and kept the earlier ones"));
    std::fs::write(&hist, lines(100..103)).unwrap();
    assert!(import().contains("stored it whole (3 commands) as a new revision and kept the earlier ones"));
}

#[test]
fn an_import_that_fails_on_the_file_leaves_no_database_behind() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("history"), "echo one\n").unwrap();

    let (code, stderr) = import_in(dir.path(), &["history"]);
    assert_ne!(code, Some(0));
    assert!(stderr.contains("history: cannot tell whether this is a zsh or a bash history"), "{stderr}");
    assert!(stderr.contains("--shell"), "{stderr}");

    let (code, stderr) = import_in(dir.path(), &["missing"]);
    assert_ne!(code, Some(0));
    assert!(stderr.contains("missing: No such file"), "{stderr}");

    assert!(!dir.path().join("data").exists());
}

fn relic(dir: &std::path::Path, args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_relic")).current_dir(dir).args(args).output().unwrap();
    (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn on_disk(dir: &std::path::Path, needle: &str) -> bool {
    std::fs::read_dir(dir).unwrap().any(|entry| {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap_or_default();
        bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
    })
}

#[test]
fn an_import_rescrubs_what_older_redaction_rules_left_in_the_store() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("data")).unwrap();
    std::fs::write(dir.path().join(".zsh_history"), ": 1700000000:0;mysql -u root -pOldRulesMissedThis1 shop\n").unwrap();
    let (code, stderr) = import_in(dir.path(), &[".zsh_history"]);
    assert_eq!(code, Some(0), "{stderr}");

    // What a store written by rules that missed `-p<password>` looks like.
    let conn = rusqlite::Connection::open(dir.path().join("data/relic.db")).unwrap();
    conn.execute_batch(
        "UPDATE observation SET safe_text = 'mysql -u root -pOldRulesMissedThis1 shop';
         UPDATE source_revision SET redaction_revision = 'r1';
         INSERT INTO observation_fts (observation_fts) VALUES ('rebuild');",
    )
    .unwrap();
    drop(conn);
    assert!(on_disk(&dir.path().join("data"), "OldRulesMissedThis1"));

    // The file is unchanged, and the store is rescrubbed all the same.
    let (code, stderr) = import_in(dir.path(), &[".zsh_history"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stderr.contains("re-redacted 1 stored commands"), "{stderr}");
    assert!(stderr.contains("unchanged since last import"), "{stderr}");
    assert!(!on_disk(&dir.path().join("data"), "OldRulesMissedThis1"));
    assert!(!on_disk(&dir.path().join("data"), "oldrulesmissedthis1"));

    let conn = rusqlite::Connection::open(dir.path().join("data/relic.db")).unwrap();
    let (text, revision): (String, String) = conn
        .query_row(
            "SELECT o.safe_text, sr.redaction_revision FROM observation o
             JOIN source_revision sr ON sr.id = o.revision_id",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(text, "mysql -u root -p[REDACTED:password] shop");
    assert_eq!(revision, relic::redact::REDACTION_REVISION);

    // Once is enough.
    let (_, stderr) = import_in(dir.path(), &[".zsh_history"]);
    assert!(!stderr.contains("re-redacted"), "{stderr}");
}

#[test]
fn forget_deletes_a_record_for_good_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".zsh_history"), ": 1700000000:0;echo keep\n: 1700000001:0;curl LEAKEDCANARY9\n").unwrap();
    import_in(dir.path(), &[".zsh_history"]);
    let id: i64 = rusqlite::Connection::open(dir.path().join("data/relic.db"))
        .unwrap()
        .query_row("SELECT id FROM observation WHERE safe_text LIKE '%LEAKEDCANARY9'", [], |r| r.get(0))
        .unwrap();

    // Shown exactly as stored, which is what a skill sends an agent to read.
    let show = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_relic")).current_dir(dir.path()).args(args).args(["--db", "data/relic.db"]).output().unwrap();
        (out.status.code(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
    };
    let (code, stdout, _) = show(&["show", &format!("#{id}"), "--json"]);
    let page: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!((code, &page["schema"], &page["records"][0]["text"]), (Some(0), &"relic.records/v1".into(), &"curl LEAKEDCANARY9".into()));

    let (code, stderr) = relic(dir.path(), &["forget", &format!("#{id}"), "--db", "data/relic.db"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stderr.contains(&format!("relic: forgot record #{id}")), "{stderr}");
    let (code, stdout, stderr) = show(&["show", &id.to_string()]);
    assert!(code == Some(1) && stdout.is_empty() && stderr.contains(&format!("no such record: #{id}")), "{stderr}");
    assert!(!on_disk(&dir.path().join("data"), "LEAKEDCANARY9"));

    // Forgetting it again is an error that names it; so is an id never seen.
    let (code, stderr) = relic(dir.path(), &["forget", &id.to_string(), "--db", "data/relic.db"]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains(&format!("no such record: #{id}")), "{stderr}");

    // An unchanged or appended-to file does not bring it back.
    let (_, stderr) = import_in(dir.path(), &[".zsh_history"]);
    assert!(stderr.contains("unchanged"), "{stderr}");
    assert!(!on_disk(&dir.path().join("data"), "LEAKEDCANARY9"));
}

#[test]
fn a_write_open_leaves_relics_own_files_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let loosen = |path: &std::path::Path, mode| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
    };

    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(".zsh_history"), ": 1700000000:0;echo one\n").unwrap();
    let import = || {
        let out = Command::new(env!("CARGO_BIN_EXE_relic"))
            .current_dir(home.path())
            .env("XDG_DATA_HOME", home.path().join("share"))
            .env_remove("RELIC_DB")
            .args(["import", ".zsh_history"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    };
    // Created fresh: nothing is readable by anyone else, whatever the umask.
    import();
    let data = home.path().join("share/relic");
    let db = data.join("relic.db");
    assert_eq!((mode(&data), mode(&db), mode(&data.join("relic.db.key"))), (0o700, 0o600, 0o600));

    // A store from before the umask rule: everything world-readable, with a
    // reader holding the WAL and shm open so they exist to be fixed.
    let reader = rusqlite::Connection::open(&db).unwrap();
    reader.query_row("SELECT COUNT(*) FROM observation", [], |r| r.get::<_, i64>(0)).unwrap();
    loosen(&data, 0o755);
    for name in ["relic.db", "relic.db-wal", "relic.db-shm"] {
        loosen(&data.join(name), 0o644);
    }
    std::fs::write(home.path().join(".zsh_history"), ": 1700000000:0;echo one\n: 1700000001:0;echo two\n").unwrap();
    import();
    assert_eq!(mode(&data), 0o700);
    for name in ["relic.db", "relic.db-wal", "relic.db-shm"] {
        assert_eq!(mode(&data.join(name)), 0o600, "{name}");
    }

    // A database somewhere else: its files are tightened, its directory is not.
    let other = tempfile::tempdir().unwrap();
    loosen(other.path(), 0o755);
    let (code, stderr) = relic(home.path(), &["import", ".zsh_history", "--db", other.path().join("x.db").to_str().unwrap()]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!((mode(other.path()), mode(&other.path().join("x.db"))), (0o755, 0o600));
}

/// `git` in `dir`, with no config but the repository's own.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .envs([("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"), ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t")])
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn repo_in(dir: &std::path::Path) -> std::path::PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
    repo
}

#[test]
fn a_git_import_never_runs_a_git_left_in_the_current_directory() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo_in(dir.path());
    let trap = dir.path().join("trap");
    std::fs::create_dir(&trap).unwrap();
    std::fs::write(trap.join("git"), "#!/bin/sh\n/usr/bin/touch ran\n").unwrap();
    std::fs::set_permissions(trap.join("git"), std::fs::Permissions::from_mode(0o755)).unwrap();
    // No PATH at all: the default search path applies, not the current
    // directory.
    let out = Command::new(env!("CARGO_BIN_EXE_relic"))
        .current_dir(&trap)
        .env_clear()
        .arg("import")
        .arg(&repo)
        .arg("--db")
        .arg(dir.path().join("relic.db"))
        .output()
        .unwrap();
    assert!(!trap.join("ran").exists(), "relic ran ./git: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn a_repository_whose_own_config_names_a_file_in_home_imports() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo_in(dir.path());
    git(&repo, &["config", "include.path", "~/.gitconfig-extra"]);
    let out = Command::new(env!("CARGO_BIN_EXE_relic"))
        .env("HOME", dir.path())
        .arg("import")
        .arg(&repo)
        .arg("--db")
        .arg(dir.path().join("relic.db"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn a_directory_git_cannot_read_leaves_no_database_and_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo_in(dir.path());
    // A branch at an object that is not there: `git log` fails.
    std::fs::write(repo.join(".git/refs/heads/broken"), format!("{}\n", "1".repeat(40))).unwrap();
    let db = dir.path().join("relic.db");
    let out = Command::new(env!("CARGO_BIN_EXE_relic")).arg("import").arg(&repo).arg("--db").arg(&db).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains(&format!("{}: git log failed", repo.display())), "{stderr}");
    assert!(!db.exists(), "a database was left behind");

    let plain = dir.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_relic")).arg("import").arg(&plain).arg("--db").arg(&db).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.code() == Some(2) && stderr.contains("not a git repository"), "{stderr}");
    assert!(!db.exists(), "a database was left behind");
}
