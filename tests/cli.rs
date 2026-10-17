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
