//! `relic import` as a process: a busy store, and relative path spellings.

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
