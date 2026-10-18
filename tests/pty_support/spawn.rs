//! Starts a program on a pty under a stand-in login shell.
//!
//! The program cannot simply be the session leader: on macOS the leader's
//! exit revokes the controlling tty, after which `tcgetattr` on every slave
//! fd fails with ENOTTY, and reopening the slave resets termios to defaults
//! (which would make a still-raw terminal look restored). So a forked
//! supervisor plays an interactive shell: it leads the session, owns the
//! controlling tty, starts the program as a foreground job in its own
//! process group, reports its pid and wait status over a pipe, and stays
//! alive until the test lets go.
//!
//! A terminal hangup signals only the session leader. [`Hup::Forward`]
//! passes SIGHUP on to the job, as zsh and bash do when a terminal window
//! closes; [`Hup::Ignore`] is `nohup`, where the program has to notice the
//! dead terminal by itself.
//!
//! Like a shell with job control, the supervisor takes the terminal back
//! when the job stops, and on [`Supervised::fg`] hands it back and sends
//! SIGCONT.

use std::ffi::{CString, OsStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hup {
    Forward,
    Ignore,
}

/// The job's process group, for the supervisor's SIGHUP handler.
static JOB: AtomicI32 = AtomicI32::new(0);

/// Set by SIGUSR1: the test asked for `fg`.
static FG: AtomicBool = AtomicBool::new(false);

extern "C" fn request_fg(_: libc::c_int) {
    FG.store(true, Ordering::SeqCst);
}

extern "C" fn forward_hup(_: libc::c_int) {
    let job = JOB.load(Ordering::SeqCst);
    if job > 0 {
        // SAFETY: kill(2) is async-signal-safe.
        unsafe { libc::kill(-job, libc::SIGHUP) };
    }
}

pub struct Supervised {
    sup: libc::pid_t,
    pub pid: libc::pid_t,
    report: OwnedFd,
    control: Option<OwnedFd>,
}

impl Supervised {
    /// `env` is the program's whole environment.
    pub fn start(
        program: &OsStr,
        args: &[&OsStr],
        env: &[(&OsStr, &OsStr)],
        slave: RawFd,
        master: RawFd,
        hup: Hup,
    ) -> Self {
        let cstr = |b: &[u8]| CString::new(b).expect("no NUL in argv/env");
        let path = cstr(program.as_bytes());
        let argv: Vec<CString> = std::iter::once(program)
            .chain(args.iter().copied())
            .map(|a| cstr(a.as_bytes()))
            .collect();
        let envp: Vec<CString> = env
            .iter()
            .map(|(k, v)| cstr(&[k.as_bytes(), b"=", v.as_bytes()].concat()))
            .collect();
        let argv_p = null_terminated(&argv);
        let envp_p = null_terminated(&envp);
        let (report_r, report_w) = pipe();
        let (control_r, control_w) = pipe();
        // SAFETY: everything below the fork in the child is async-signal-safe
        // libc on memory prepared before the fork; it never returns to Rust.
        let sup = unsafe { libc::fork() };
        assert!(sup >= 0, "fork: {}", std::io::Error::last_os_error());
        if sup == 0 {
            let fds = Fds {
                slave,
                parent_only: [master, report_r.as_raw_fd(), control_w.as_raw_fd()],
                report: report_w.as_raw_fd(),
                control: control_r.as_raw_fd(),
            };
            unsafe { supervise(&path, &argv_p, &envp_p, &fds, hup) }
        }
        // The supervisor's copies are the only ones that may stay open, so
        // its death reads as EOF here and our close reads as EOF there.
        drop((report_w, control_r));
        let mut s = Self {
            sup,
            pid: 0,
            report: report_r,
            control: Some(control_w),
        };
        s.pid = s
            .read_i32(Duration::from_secs(5))
            .expect("supervisor did not report the program's pid");
        assert!(s.pid > 0, "supervisor failed to start: code {}", -s.pid);
        s
    }

    /// The program's wait status once it has exited, waiting up to `limit`.
    pub fn status(&self, limit: Duration) -> Option<ExitStatus> {
        self.read_i32(limit).map(ExitStatus::from_raw)
    }

    fn read_i32(&self, limit: Duration) -> Option<i32> {
        let fd = self.report.as_raw_fd();
        let end = Instant::now() + limit;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll on one live pollfd.
            let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis() as libc::c_int) };
            if n > 0 {
                break;
            }
            if left.is_zero() {
                return None;
            }
        }
        let mut b = [0u8; 4];
        // SAFETY: read(2) into a live 4-byte buffer. The supervisor writes
        // each value with one 4-byte write, which a pipe delivers whole.
        let n = unsafe { libc::read(fd, b.as_mut_ptr().cast(), 4) };
        (n == 4).then(|| i32::from_ne_bytes(b))
    }

    /// The shell's `fg`: the job gets the terminal back and SIGCONT.
    pub fn fg(&self) {
        // SAFETY: kill(2) to the supervisor, which handles SIGUSR1.
        unsafe { libc::kill(self.sup, libc::SIGUSR1) };
    }

    pub fn kill(&self, sig: libc::c_int) {
        // SAFETY: kill(2); the supervisor has not reaped the pid until it
        // reports a status, so it cannot have been reused before then.
        unsafe { libc::kill(self.pid, sig) };
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        // Closing the control pipe releases the supervisor; a program still
        // running is killed first so nothing outlives the test.
        drop(self.control.take());
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            let mut st = 0;
            // SAFETY: waitpid on our own child.
            let r = unsafe { libc::waitpid(self.sup, &mut st, libc::WNOHANG) };
            if r != 0 {
                return;
            }
            if Instant::now() >= end {
                // SAFETY: as above; SIGKILL then a blocking reap.
                unsafe {
                    libc::kill(self.pid, libc::SIGKILL);
                    libc::kill(self.sup, libc::SIGKILL);
                    libc::waitpid(self.sup, &mut st, 0);
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[derive(Clone, Copy)]
struct Fds {
    slave: RawFd,
    /// Copies fork() made of fds that belong to the test process.
    parent_only: [RawFd; 3],
    report: RawFd,
    control: RawFd,
}

/// The forked supervisor. Only async-signal-safe calls.
unsafe fn supervise(
    path: &CString,
    argv: &[*const libc::c_char],
    envp: &[*const libc::c_char],
    fds: &Fds,
    hup: Hup,
) -> ! {
    let Fds {
        slave,
        parent_only,
        report,
        control,
    } = *fds;
    unsafe {
        let say = |v: i32| {
            libc::write(report, (&raw const v).cast(), 4);
        };
        // fork() copied every fd. Holding the master here would keep the
        // terminal from ever hanging up, and holding the parent's pipe ends
        // would keep either side from seeing the other close.
        for fd in parent_only {
            libc::close(fd);
        }
        if libc::setsid() < 0 {
            say(-1);
            libc::_exit(1);
        }
        if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) < 0 {
            say(-2);
            libc::_exit(1);
        }
        // Handing the terminal to the job from outside the foreground
        // group raises SIGTTOU; shells ignore it for the same reason.
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        let pid = libc::fork();
        if pid == 0 {
            // Both sides set the job's group and hand it the terminal, so
            // neither order of the race leaves the program in the background.
            libc::setpgid(0, 0);
            libc::tcsetpgrp(slave, libc::getpid());
            // The test process ignores SIGPIPE (Rust runtime) and this one
            // SIGHUP and SIGTTOU; a shell starts a job with defaults.
            for sig in [libc::SIGHUP, libc::SIGPIPE, libc::SIGTTOU] {
                libc::signal(sig, libc::SIG_DFL);
            }
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            for fd in 0..3 {
                libc::dup2(slave, fd);
            }
            libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }
        if pid < 0 {
            say(-3);
            libc::_exit(1);
        }
        libc::setpgid(pid, pid);
        libc::tcsetpgrp(slave, pid);
        if hup == Hup::Forward {
            JOB.store(pid, Ordering::SeqCst);
            libc::signal(libc::SIGHUP, forward_hup as *const () as libc::sighandler_t);
        }
        libc::signal(libc::SIGUSR1, request_fg as *const () as libc::sighandler_t);
        say(pid);
        let mut st = 0;
        // Polled, so an `fg` request is seen whenever it arrives.
        let tick = libc::timespec { tv_sec: 0, tv_nsec: 5_000_000 };
        loop {
            if FG.swap(false, Ordering::SeqCst) {
                libc::tcsetpgrp(slave, pid);
                libc::kill(-pid, libc::SIGCONT);
            }
            match libc::waitpid(pid, &mut st, libc::WNOHANG | libc::WUNTRACED) {
                0 => {}
                // The job is gone without a status to report: no `say`, so
                // the test sees the supervisor's EOF, not a stale exit 0.
                -1 => libc::_exit(1),
                _ if libc::WIFSTOPPED(st) => {
                    libc::tcsetpgrp(slave, libc::getpgrp());
                }
                _ => break,
            }
            libc::nanosleep(&tick, std::ptr::null_mut());
        }
        say(st);
        let mut b = 0u8;
        libc::read(control, (&raw mut b).cast(), 1);
        libc::_exit(0);
    }
}

fn null_terminated(v: &[CString]) -> Vec<*const libc::c_char> {
    v.iter()
        .map(|c| c.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: pipe(2) fills two fds. CLOEXEC so the program does not
    // inherit them; the supervisor uses its copies without exec.
    unsafe {
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
    }
}
