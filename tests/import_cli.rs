//! `relic import` against a store another importer is writing.

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
