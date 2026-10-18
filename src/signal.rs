//! Fatal signals while Relic owns the terminal or runs fzf (ADR 0011). TERM,
//! INT, HUP and QUIT stop fzf, put the terminal back, then kill Relic with
//! the same signal, so the exit status is still 128+n.
//!
//! Everything the handler does is async-signal-safe: atomics, `kill`,
//! `waitpid`, `nanosleep`, `tcsetattr`, `write`, `sigaction` and `raise`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

const FATAL: [libc::c_int; 4] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT];

/// How long fzf gets to restore the terminal itself before SIGKILL.
const GRACE_MS: u32 = 200;

/// The running fzf's pid, or 0.
static CHILD: AtomicI32 = AtomicI32::new(0);

/// `/dev/tty` and its termios before Relic or fzf changed it. fzf may die
/// by SIGKILL in raw mode, so the TUI's own restore is not enough.
static TTY: OnceLock<(libc::c_int, libc::termios)> = OnceLock::new();

/// Installs the handler for every fatal signal not inherited as ignored: a
/// `nohup` or `trap '' TERM` is the user's choice. Call before the terminal
/// changes, on the main thread, before [`block_in_this_thread`] threads exist.
pub fn install() {
    save_tty();
    for sig in FATAL {
        // SAFETY: sigaction with a null new action only reads the current one.
        let ignored = unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(sig, std::ptr::null(), &mut old);
            old.sa_sigaction == libc::SIG_IGN
        };
        if ignored {
            continue;
        }
        // SAFETY: installs an async-signal-safe handler with every fatal
        // signal masked while it runs, so a second one cannot interrupt it.
        // SIGTTOU too: after Ctrl-Z and `kill %1` the handler runs in the
        // background, and its tcsetattr would otherwise stop Relic again.
        unsafe {
            let mut act: libc::sigaction = std::mem::zeroed();
            act.sa_sigaction = on_fatal as *const () as libc::sighandler_t;
            libc::sigemptyset(&mut act.sa_mask);
            for s in FATAL.into_iter().chain([libc::SIGTTOU]) {
                libc::sigaddset(&mut act.sa_mask, s);
            }
            libc::sigaction(sig, &act, std::ptr::null_mut());
        }
    }
}

/// Keeps fatal signals off this thread, so the handler runs on the main
/// thread: there it cannot race the main thread taking the terminal back
/// after fzf, and a `waitpid` on fzf is interrupted rather than raced.
pub fn block_in_this_thread() {
    // SAFETY: builds a signal set on the stack and blocks it for this thread.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in FATAL {
            libc::sigaddset(&mut set, s);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// The fzf a fatal signal must stop first; 0 once it has been reaped.
pub fn set_child(pid: libc::pid_t) {
    CHILD.store(pid, Ordering::SeqCst);
}

fn save_tty() {
    if TTY.get().is_some() {
        return;
    }
    // SAFETY: open(2) of a C string literal; tcgetattr fills a zeroed termios
    // for that fd. The fd stays open for the handler: it is never closed.
    unsafe {
        let fd = libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if fd < 0 {
            return;
        }
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 || TTY.set((fd, t)).is_err() {
            libc::close(fd);
        }
    }
}

extern "C" fn on_fatal(sig: libc::c_int) {
    stop_child();
    #[cfg(feature = "tui")]
    crate::ui::emergency_restore();
    if let Some((fd, t)) = TTY.get() {
        // SAFETY: restores a termios tcgetattr captured on this fd.
        unsafe { libc::tcsetattr(*fd, libc::TCSANOW, t) };
    }
    // SAFETY: the default action for `sig` is to terminate; unblocking it
    // after the raise delivers it at once, so this never returns.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
    }
}

/// SIGTERM, a short grace to let fzf restore the terminal, then SIGKILL.
/// By pid, not group: fzf shares Relic's foreground group.
fn stop_child() {
    let pid = CHILD.swap(0, Ordering::SeqCst);
    if pid <= 0 {
        return;
    }
    // SAFETY: kill, waitpid, nanosleep and write are async-signal-safe;
    // `st` and `tick` live on this stack frame.
    unsafe {
        let mut st = 0;
        // Checked before any kill: once the main thread has reaped fzf the
        // pid may be another process's. Only the main thread reaps, and this
        // handler runs on it, so nothing reaps between check and kill.
        if libc::waitpid(pid, &mut st, libc::WNOHANG) != 0 {
            return;
        }
        libc::kill(pid, libc::SIGTERM);
        let tick = libc::timespec { tv_sec: 0, tv_nsec: 10_000_000 };
        for _ in 0..GRACE_MS / 10 {
            if libc::waitpid(pid, &mut st, libc::WNOHANG) != 0 {
                return;
            }
            libc::nanosleep(&tick, std::ptr::null_mut());
        }
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, &mut st, 0);
        // A killed fzf leaves its screen behind: mouse reporting, its
        // alternate screen and a hidden cursor. To the terminal fzf drew on:
        // `search --fzf` may have its stdout in the user's file or pipe.
        if let Some((fd, _)) = TTY.get() {
            let reset = b"\x1b[?1000l\x1b[?1006l\x1b[?1049l\x1b[?25h";
            libc::write(*fd, reset.as_ptr().cast(), reset.len());
        }
    }
}
