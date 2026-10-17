//! Runs the real `relic ui` binary on a pseudo-terminal it owns as its
//! controlling tty, types into the master and reads everything it draws.
//!
//! The parent keeps its own slave fd open for the whole session. That keeps
//! the tty's termios alive after the child exits, so restoration can be read
//! back with `tcgetattr`, and it keeps the master readable without EIO.
//!
//! Nothing here may hang a test: every wait is bounded by one per-session
//! deadline, and dropping a [`Session`] kills and reaps the child.

pub mod screen;
mod spawn;

use std::ffi::{OsStr, OsString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use screen::Screen;
pub use spawn::Hup;
use spawn::Supervised;

/// Hard limit on one whole session, from spawn to reaped exit.
pub const DEADLINE: Duration = Duration::from_secs(10);
pub const ROWS: u16 = 30;
pub const COLS: u16 = 100;

pub const ENTER_ALT: &[u8] = b"\x1b[?1049h";
pub const LEAVE_ALT: &[u8] = b"\x1b[?1049l";
pub const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
pub const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
pub const PASTE_ON: &[u8] = b"\x1b[?2004h";
pub const PASTE_OFF: &[u8] = b"\x1b[?2004l";

/// The sessions run one at a time. Concurrent sessions would race on CPU
/// against the deadline, and a fake fzf written by one test thread while
/// another thread forks can fail to exec with ETXTBSY on Linux.
pub fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A small history imported through relic's own importer, newest last:
/// `fixture-cmd-01` .. `fixture-cmd-40`.
pub struct Fixture {
    _dir: tempfile::TempDir,
    pub db: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let hist = dir.path().join("zsh_history");
        let lines: String = (1..=40)
            .map(|i| format!(": {}:0;echo fixture-cmd-{i:02}\n", 1_700_000_000 + i * 60))
            .collect();
        std::fs::write(&hist, lines).unwrap();
        let db = dir.path().join("relic.db");
        let mut store = relic::store::Store::open(&db).unwrap();
        let r = relic::import::import_file(
            &mut store,
            &hist,
            relic::import::SourceKind::Zsh,
            &relic::redact::Redactor::new(),
        )
        .unwrap();
        assert_eq!(r.records_stored, 40);
        Self { _dir: dir, db }
    }
}

pub struct Output {
    pub raw: Vec<u8>,
    pub screen: Screen,
}

impl Output {
    pub fn count(&self, needle: &[u8]) -> usize {
        self.raw
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count()
    }

    pub fn last(&self, needle: &[u8]) -> Option<usize> {
        self.raw.windows(needle.len()).rposition(|w| w == needle)
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.last(needle.as_bytes()).is_some()
    }
}

struct Shared {
    out: Mutex<Output>,
    cv: Condvar,
}

pub struct Session {
    proc: Supervised,
    exited: Option<ExitStatus>,
    master: Option<Arc<OwnedFd>>,
    slave: OwnedFd,
    before: libc::termios,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    deadline: Instant,
}

impl Session {
    /// `relic ui --db <db>` with only `env` (plus `TERM`, a scratch `HOME`
    /// and, unless given, a system-only `PATH`) in its environment.
    pub fn spawn(db: &Path, env: &[(&str, &OsStr)]) -> Self {
        Self::spawn_with(db, env, Hup::Forward)
    }

    pub fn spawn_with(db: &Path, env: &[(&str, &OsStr)], hup: Hup) -> Self {
        Self::start(&["ui".as_ref(), "--db".as_ref(), db.as_os_str()], env, hup)
    }

    /// `relic <args>` on the terminal, with the environment of [`Self::spawn`].
    pub fn spawn_args(args: &[&OsStr], env: &[(&str, &OsStr)]) -> Self {
        Self::start(args, env, Hup::Forward)
    }

    fn start(args: &[&OsStr], env: &[(&str, &OsStr)], hup: Hup) -> Self {
        let (master, slave) = open_pty(ROWS, COLS);
        let before = termios(slave.as_raw_fd()).expect("tcgetattr before launch");
        let home = std::env::temp_dir();
        let mut vars: Vec<(&OsStr, &OsStr)> = vec![
            ("PATH".as_ref(), "/usr/bin:/bin".as_ref()),
            ("TERM".as_ref(), "xterm-256color".as_ref()),
            ("HOME".as_ref(), home.as_os_str()),
        ];
        for (k, v) in env {
            vars.retain(|(old, _)| *old != OsStr::new(k));
            vars.push((k.as_ref(), v));
        }
        let proc = Supervised::start(
            env!("CARGO_BIN_EXE_relic").as_ref(),
            args,
            &vars,
            slave.as_raw_fd(),
            master.as_raw_fd(),
            hup,
        );

        let shared = Arc::new(Shared {
            out: Mutex::new(Output {
                raw: Vec::new(),
                screen: Screen::new(ROWS, COLS),
            }),
            cv: Condvar::new(),
        });
        let master = Arc::new(master);
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let (m, sh, st) = (Arc::clone(&master), Arc::clone(&shared), Arc::clone(&stop));
            std::thread::spawn(move || read_master(m.as_raw_fd(), &sh, &st))
        };
        Self {
            proc,
            exited: None,
            master: Some(master),
            slave,
            before,
            shared,
            stop,
            reader: Some(reader),
            deadline: Instant::now() + DEADLINE,
        }
    }

    pub fn pid(&self) -> libc::pid_t {
        self.proc.pid
    }

    pub fn send(&self, bytes: &[u8]) {
        write_all(
            self.master.as_ref().expect("master is open").as_raw_fd(),
            bytes,
        );
    }

    /// Reads the output under the lock.
    pub fn with<T>(&self, f: impl FnOnce(&Output) -> T) -> T {
        f(&self.shared.out.lock().unwrap())
    }

    /// Blocks until `pred` holds, failing the test with a dump at the
    /// session deadline or as soon as relic exits without it holding.
    pub fn wait_for(&mut self, what: &str, pred: impl Fn(&Output) -> bool) {
        loop {
            let exited = self.try_exit();
            let out = self.shared.out.lock().unwrap();
            if pred(&out) {
                return;
            }
            if let Some(s) = exited {
                panic!(
                    "relic ui exited ({}) before {what}\n{}",
                    describe(s),
                    dump(&out)
                );
            }
            let left = self.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                panic!("timed out waiting for {what}\n{}", dump(&out));
            }
            // Short slices so an exit is noticed promptly.
            drop(
                self.shared
                    .cv
                    .wait_timeout(out, left.min(Duration::from_millis(50)))
                    .unwrap(),
            );
        }
    }

    fn try_exit(&mut self) -> Option<ExitStatus> {
        if self.exited.is_none() {
            self.exited = self.proc.status(Duration::ZERO);
        }
        self.exited
    }

    /// The full-screen UI is up and showing `text`.
    pub fn wait_screen(&mut self, text: &str) {
        self.wait_for(&format!("{text:?} on the alternate screen"), |o| {
            o.screen.in_alt() && o.screen.text().contains(text)
        });
    }

    /// First frame drawn with the newest fixture command listed.
    pub fn wait_ready(&mut self) {
        self.wait_screen("fixture-cmd-40");
    }

    /// Reaps the child, killing it and failing if the deadline passes.
    pub fn wait_exit(&mut self) -> ExitStatus {
        loop {
            if let Some(s) = self.try_exit() {
                return s;
            }
            if Instant::now() >= self.deadline {
                self.proc.kill(libc::SIGKILL);
                let dump = self.with(dump);
                panic!("relic ui did not exit before the deadline\n{dump}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The child is still running after `period`.
    pub fn assert_alive_for(&mut self, period: Duration) {
        let end = Instant::now() + period;
        while Instant::now() < end {
            if let Some(s) = self.try_exit() {
                let dump = self.with(dump);
                panic!("relic ui exited early with {s}\n{dump}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn assert_exit_code(&mut self, code: i32) {
        let s = self.wait_exit();
        let dump = self.with(dump);
        assert_eq!(s.code(), Some(code), "exit status {s}\n{dump}");
    }

    /// Termios back to the pre-launch state, alternate screen left after
    /// it was last entered, cursor shown after it was last hidden, and
    /// bracketed paste off after it was last enabled. Call after exit.
    pub fn assert_restored(&self) {
        assert!(self.exited.is_some(), "check restoration after exit");
        let now = termios(self.slave.as_raw_fd()).expect("tcgetattr after exit");
        let b = &self.before;
        let lost: Vec<&str> = [
            (libc::ICANON, "ICANON"),
            (libc::ECHO, "ECHO"),
            (libc::ISIG, "ISIG"),
        ]
        .into_iter()
        .filter(|(f, _)| b.c_lflag & f != now.c_lflag & f)
        .map(|(_, n)| n)
        .collect();
        let shown = self.with(dump);
        assert!(lost.is_empty(), "termios {lost:?} not restored\n{shown}");
        // The rest of raw mode (ICRNL, OPOST, VMIN, ...) must come back too.
        // PENDIN and FLUSHO are kernel state, not settings: macOS sets
        // PENDIN whenever ICANON is switched back on with tcsetattr.
        let lflag = |t: &libc::termios| t.c_lflag & !(libc::PENDIN | libc::FLUSHO);
        assert_eq!(
            (now.c_iflag, now.c_oflag, now.c_cflag, lflag(&now), now.c_cc),
            (b.c_iflag, b.c_oflag, b.c_cflag, lflag(b), b.c_cc),
            "termios differs from before launch\n{shown}"
        );
        self.with(|o| {
            for (on, off, name) in [
                (ENTER_ALT, LEAVE_ALT, "alternate screen"),
                (HIDE_CURSOR, SHOW_CURSOR, "hidden cursor"),
                (PASTE_ON, PASTE_OFF, "bracketed paste"),
            ] {
                if let Some(set) = o.last(on) {
                    assert!(
                        o.last(off).is_some_and(|unset| unset > set),
                        "{name} not undone after it was last set\n{shown}"
                    );
                }
            }
        });
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let fd = self.master.as_ref().expect("master is open").as_raw_fd();
        // SAFETY: TIOCSWINSZ reads a winsize from a live reference.
        let r = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) };
        assert_eq!(r, 0, "TIOCSWINSZ: {}", std::io::Error::last_os_error());
        self.shared.out.lock().unwrap().screen.resize(rows, cols);
    }

    pub fn signal(&self, sig: libc::c_int) {
        self.proc.kill(sig);
    }

    /// Closes every master fd: the terminal disconnects.
    pub fn hang_up(&mut self) {
        self.stop_reader();
        let master = self.master.take().expect("master is open");
        assert!(
            Arc::into_inner(master).is_some(),
            "reader still holds the master"
        );
    }

    fn stop_reader(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.reader.take() {
            h.join().unwrap();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.exited.is_none() {
            self.proc.kill(libc::SIGKILL);
        }
        self.stop_reader();
    }
}

pub fn pid_alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the pid exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Polls until `pid` is gone, up to `limit`.
pub fn wait_gone(pid: libc::pid_t, limit: Duration) -> bool {
    let end = Instant::now() + limit;
    while pid_alive(pid) {
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

pub fn path_with(dir: &Path) -> OsString {
    std::env::join_paths([dir, Path::new("/usr/bin"), Path::new("/bin")]).unwrap()
}

fn open_pty(rows: u16, cols: u16) -> (OwnedFd, OwnedFd) {
    let mut ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (mut m, mut s) = (-1, -1);
    // SAFETY: openpty writes two fds; null name/termios mean defaults.
    let r = unsafe {
        libc::openpty(
            &mut m,
            &mut s,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    assert_eq!(r, 0, "openpty: {}", std::io::Error::last_os_error());
    for fd in [m, s] {
        // relic must not inherit either: a master it held open could never
        // hang up, and the slave reaches it as fds 0-2 anyway.
        // SAFETY: fcntl on fds we just opened.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // SAFETY: openpty returned two fresh fds that nothing else owns.
    unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) }
}

fn termios(fd: RawFd) -> std::io::Result<libc::termios> {
    // SAFETY: zeroed is a valid termios; tcgetattr fills it for a tty fd.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(t)
    }
}

fn write_all(fd: RawFd, mut rest: &[u8]) {
    while !rest.is_empty() {
        // SAFETY: write(2) from a live slice to our own open fd.
        let n = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            panic!("write to pty master: {e}");
        }
        rest = &rest[n as usize..];
    }
}

fn read_master(fd: RawFd, shared: &Shared, stop: &AtomicBool) {
    let mut buf = [0u8; 8192];
    while !stop.load(Ordering::SeqCst) {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll on one live pollfd. The timeout bounds how long a
        // stop request waits.
        if unsafe { libc::poll(&mut pfd, 1, 50) } <= 0 {
            continue;
        }
        // SAFETY: read(2) into a live stack buffer.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        let mut out = shared.out.lock().unwrap();
        out.raw.extend_from_slice(&buf[..n as usize]);
        out.screen.feed(&buf[..n as usize]);
        shared.cv.notify_all();
    }
}

/// Screen text plus the escaped tail of the raw stream, for failures.
pub fn dump(o: &Output) -> String {
    let tail = &o.raw[o.raw.len().saturating_sub(1500)..];
    format!(
        "--- screen ({}) ---\n{}\n--- last {} raw bytes ---\n{}",
        if o.screen.in_alt() {
            "alternate"
        } else {
            "main"
        },
        o.screen.text(),
        tail.len(),
        tail.escape_ascii()
    )
}

/// How a session ended, for messages.
pub fn describe(s: ExitStatus) -> String {
    match (s.code(), s.signal()) {
        (Some(c), _) => format!("exit {c}"),
        (_, Some(sig)) => format!("signal {sig}"),
        _ => format!("{s}"),
    }
}
