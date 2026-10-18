//! `relic search` as a process: the order of a page, the word cap, and what a
//! read-only open does with an import in progress, a locked store and a file
//! that is not a store.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn relic(args: &[&str], db: &Path) -> Output {
    // `ui` refuses a dumb or missing TERM before it opens the store.
    Command::new(env!("CARGO_BIN_EXE_relic")).args(args).arg("--db").arg(db).env("TERM", "xterm").output().unwrap()
}

fn store_with(dir: &Path, commands: &[&str]) -> PathBuf {
    let hist = dir.join("zsh_history");
    let lines: String =
        commands.iter().enumerate().map(|(i, c)| format!(": {}:0;{c}\n", 1_700_000_000 + i)).collect();
    std::fs::write(&hist, lines).unwrap();
    let db = dir.join("relic.db");
    let out = relic(&["import", hist.to_str().unwrap()], &db);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    db
}

fn page(out: &Output) -> serde_json::Value {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn texts(page: &serde_json::Value) -> Vec<&str> {
    page["hits"].as_array().unwrap().iter().map(|h| h["text"].as_str().unwrap()).collect()
}

#[test]
fn a_page_is_most_recently_imported_first_and_rank_is_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    // BM25 scores the shortest row highest; it was imported first.
    let db = store_with(dir.path(), &["git", "git commit -m a long message here", "ls", "git push"]);

    let default = page(&relic(&["search", "git", "--json"], &db));
    assert_eq!(texts(&default), ["git push", "git commit -m a long message here", "git"]);
    assert_eq!(default["order"], "recently_imported");
    let ids: Vec<i64> = default["hits"].as_array().unwrap().iter().map(|h| h["id"].as_i64().unwrap()).collect();
    assert!(ids.is_sorted_by(|a, b| a > b), "{ids:?}");

    let ranked = page(&relic(&["search", "git", "--json", "--rank"], &db));
    assert_eq!(texts(&ranked), ["git", "git push", "git commit -m a long message here"]);
    assert_eq!(ranked["order"], "rank");

    // The text page is the same page.
    let text = relic(&["search", "git"], &db);
    let lines: Vec<String> = String::from_utf8(text.stdout).unwrap().lines().map(String::from).collect();
    assert!(lines[0].ends_with("git push") && lines[2].ends_with("git"), "{lines:?}");
}

#[test]
fn words_past_the_cap_are_ignored_and_the_page_says_how_many() {
    let dir = tempfile::tempdir().unwrap();
    let db = store_with(dir.path(), &["git push", "ls"]);
    let mut args = vec!["search", "--json"];
    args.extend(["git"; 12]);
    args.extend(["nomatch"; 2]);

    let capped = page(&relic(&args, &db));
    assert_eq!((texts(&capped), &capped["words_dropped"]), (vec!["git push"], &serde_json::json!(2)));
    // The text page says it on stderr, where it cannot be taken for a result.
    args.remove(1);
    let text = relic(&args, &db);
    assert!(String::from_utf8_lossy(&text.stderr).contains("2 words ignored"), "the note is missing");
    assert!(String::from_utf8_lossy(&text.stdout).ends_with("git push\n"));

    let whole = page(&relic(&["search", "--json", "git", "push"], &db));
    assert_eq!(whole["words_dropped"], 0);
}

/// JSON escapes only what it must, so `--json` escapes what a terminal would
/// act on as well; the decoded text is still exactly what was stored.
#[test]
fn json_escapes_controls_a_terminal_would_act_on_and_keeps_the_text_exact() {
    let dir = tempfile::tempdir().unwrap();
    let hostile = "echo a\u{85}b\u{202e}c\u{2028}d\u{e0041}e café";
    let db = store_with(dir.path(), &[hostile]);
    let out = relic(&["search", "--json", "echo"], &db);
    let raw = String::from_utf8(out.stdout.clone()).unwrap();
    assert!(!raw.contains(['\u{85}', '\u{202e}', '\u{2028}', '\u{e0041}']), "{raw}");
    assert!(raw.contains(r"\u0085") && raw.contains(r"\udb40\udc41") && raw.contains("café"), "{raw}");
    assert_eq!(texts(&page(&out)), [hostile]);
}

/// Exit 0 is a match, 1 is no match, and a query that searches no word is a
/// usage error: never the recent commands passed off as matches (ADR 0011).
#[test]
fn exit_status_tells_a_match_from_no_match_from_a_query_with_nothing_to_search() {
    let dir = tempfile::tempdir().unwrap();
    let db = store_with(dir.path(), &["git push", "gh pr list"]);
    let long_word = ["a"; 17].join("/");
    let letters = "search matches words of letters or digits";
    let cases: &[(&[&str], i32, &str)] = &[
        (&["search", "git"], 0, ""),
        (&["search", "--prefix", "git", "p"], 0, ""),
        (&["search", "nomatch"], 1, ""),
        (&["search", "--json", "nomatch"], 1, ""),
        (&["search", "&&"], 2, letters),
        (&["search", "--json", "&&"], 2, letters),
        (&["search", "--prefix", "&&"], 2, letters),
        (&["search", "--prefix", "g"], 2, "\"g\" is too short; type 2+ characters"),
        (&["search", &long_word], 2, "every word was ignored"),
    ];
    for &(args, code, why) in cases {
        let out = relic(args, &db);
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert_eq!(out.status.code(), Some(code), "{args:?}: {stderr}");
        if code == 2 {
            assert!(stdout.is_empty(), "{args:?} printed rows: {stdout}");
            assert!(stderr.contains("nothing to search for") && stderr.contains(why), "{args:?}: {stderr}");
        }
    }
}

#[test]
fn search_reads_while_an_import_holds_the_write_lock() {
    let dir = tempfile::tempdir().unwrap();
    let db = store_with(dir.path(), &["cargo build"]);
    let writer = rusqlite::Connection::open(&db).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE; DELETE FROM observation;").unwrap();

    let found = page(&relic(&["search", "cargo", "--json"], &db));
    assert_eq!(texts(&found), ["cargo build"]);
}

#[test]
fn search_refuses_a_file_that_is_not_a_store_and_does_not_touch_it() {
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("notes.db");
    rusqlite::Connection::open(&notes).unwrap().execute_batch("CREATE TABLE notes(x)").unwrap();
    let before = std::fs::read(&notes).unwrap();

    let out = relic(&["search", "x"], &notes);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("is not a store this relic can read") && stderr.contains("relic import"), "{stderr}");
    assert!(std::fs::read(&notes).unwrap() == before, "search wrote to another program's database");
    let tables: i64 = rusqlite::Connection::open(&notes)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tables, 1);
}

#[test]
fn a_store_that_stays_locked_gets_one_traced_retry_then_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let db = store_with(dir.path(), &["cargo build"]);
    // A connection in exclusive locking mode shuts every reader out, as a
    // recovery in progress does.
    let holder = rusqlite::Connection::open(&db).unwrap();
    holder
        .execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN IMMEDIATE; DELETE FROM observation WHERE 0;")
        .unwrap();

    let out = relic(&["search", "cargo"], &db);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert_eq!(stderr.matches("trying once more").count(), 1, "{stderr}");
    assert!(stderr.contains("relic: busy: "), "{stderr}");
    // Busy is exit 2 from every command that meets it. A headless `ui`
    // refuses before it opens the store.
    let ui: &[&[&str]] = if cfg!(feature = "tui") { &[&["ui"]] } else { &[] };
    for other in ui.iter().copied().chain([&["forget", "1"][..]]) {
        let out = relic(other, &db);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{other:?}: {stderr}");
        assert!(stderr.contains("relic: busy: "), "{other:?}: {stderr}");
    }

    drop(holder);
    assert_eq!(texts(&page(&relic(&["search", "cargo", "--json"], &db))), ["cargo build"]);
}
