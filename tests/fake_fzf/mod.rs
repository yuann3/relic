//! Fake fzf executables for picker tests. Each one records its pid, argv and
//! environment in its own temp directory (`$D`) before running a test body.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use relic::app::{PageHit, SEARCH_SCHEMA, SearchPage};

pub struct FakeFzf {
    pub dir: tempfile::TempDir,
    pub path: PathBuf,
}

impl FakeFzf {
    /// Reports a supported version, then runs `body` as the picker.
    pub fn new(body: &str) -> Self {
        Self::with_version("0.74.4 (fake)", body)
    }

    pub fn with_version(version: &str, body: &str) -> Self {
        Self::new_raw(&format!(
            "if [ \"$1\" = --version ]; then echo '{version}'; exit 0; fi\n\
             for a in \"$@\"; do printf '%s\\0' \"$a\"; done > \"$D/argv\"\n\
             env > \"$D/env\"\n\
             {body}\n"
        ))
    }

    /// `script` handles `--version` itself.
    pub fn new_raw(script: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fzf");
        let d = dir.path().display();
        std::fs::write(&path, format!("#!/bin/sh\nD='{d}'\necho $$ >> \"$D/pids\"\n{script}"))
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir, path }
    }

    pub fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.dir.path().join(name)).unwrap()
    }

    pub fn argv(&self) -> Vec<String> {
        let raw = String::from_utf8(self.read("argv")).unwrap();
        raw.split_terminator('\0').map(String::from).collect()
    }

    /// Every process this fake ran has been waited for: a zombie still
    /// answers `kill(pid, 0)`, a reaped pid does not.
    pub fn assert_reaped(&self) {
        let pids = String::from_utf8(self.read("pids")).unwrap();
        for pid in pids.lines() {
            let pid: libc::pid_t = pid.parse().unwrap();
            // SAFETY: signal 0 only checks that the pid exists.
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            assert!(!alive, "fake fzf pid {pid} was not reaped");
        }
    }
}

pub fn page(ids: &[i64]) -> SearchPage {
    SearchPage {
        schema: SEARCH_SCHEMA,
        query: "q".into(),
        limit: ids.len(),
        truncated: false,
        deferred: None,
        words_dropped: 0,
        order: relic::app::Order::RecentlyImported,
        hits: ids
            .iter()
            .map(|&id| PageHit {
                id,
                observed_at_ms: Some(1_700_000_000_000),
                observed_before_ms: None,
                source: "zsh",
                text: format!("cmd {id}"),
            })
            .collect(),
    }
}

/// Runs `f` on its own thread so a deadlock fails the test instead of hanging it.
pub fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || tx.send(f()));
    rx.recv_timeout(limit).expect("timed out: picker deadlocked")
}
