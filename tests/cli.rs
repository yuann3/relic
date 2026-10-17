//! The argv contract of the `relic` binary: help, usage and exit statuses
//! that are not specific to one command (ADR 0011).

use std::process::{Command, Stdio};

fn relic() -> Command {
    Command::new(env!("CARGO_BIN_EXE_relic"))
}

#[test]
fn help_and_version_into_a_closed_pipe_exit_0_without_a_panic() {
    for flag in ["--help", "--version"] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let out = relic().arg(flag).stdout(writer).stderr(Stdio::piped()).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{flag}: {stderr}");
        assert!(stderr.is_empty(), "{flag}: {stderr}");
    }
}

/// File names and argv are as hostile as history text: no message may carry
/// a control or bidi character to the terminal.
#[test]
fn messages_neutralize_controls_in_paths_and_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let hostile = "h\x1b]0;pwned\x07\u{202e}\u{85}";
    let hist = dir.path().join(hostile);
    std::fs::write(&hist, ": 1700000000:0;echo one\n").unwrap();
    let db = dir.path().join(format!("{hostile}.db"));
    let missing = dir.path().join(format!("missing{hostile}"));
    let option = format!("--{hostile}");
    let runs: [&[&std::ffi::OsStr]; 4] = [
        &["import".as_ref(), hist.as_os_str(), "--db".as_ref(), db.as_os_str()],
        &["import".as_ref(), missing.as_os_str(), "--db".as_ref(), db.as_os_str()],
        &["search".as_ref(), "x".as_ref(), "--db".as_ref(), missing.as_os_str()],
        &["search".as_ref(), option.as_ref()],
    ];
    for args in runs {
        let out = relic().args(args).output().unwrap();
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.contains("h\u{FFFD}]0;pwned\u{FFFD}\u{FFFD}\u{FFFD}"), "{args:?}: {stderr:?}");
        assert!(!stderr.contains(['\x1b', '\x07', '\u{202e}', '\u{85}']), "{args:?}: {stderr:?}");
    }
}
