//! `relic import` as a process: a busy store, relative path spellings, the
//! shell it reads a file as, and what a failed import leaves behind.

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
