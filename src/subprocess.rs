//! Running another program the way Relic always does: fixed argv, no shell,
//! no stdin, its own process group, and a deadline over both its run and its
//! output (ADR 0001). Git reads and user verifiers both go through here.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Most bytes of a run's stderr kept for an error message.
pub const STDERR_MAX: usize = 4096;

pub enum Run {
    Done {
        status: std::process::ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    TimedOut,
    TooLarge,
}

/// Run `cmd` with no stdin, stopping it once `timeout` passes or its stdout
/// passes `max` bytes. The timeout covers its output too: a process it
/// started can hold the pipes open after it exits. Nothing it started
/// outlives the run, or Relic: a fatal signal takes its group down too.
pub fn bounded(mut cmd: Command, timeout: Duration, max: usize) -> std::io::Result<Run> {
    use std::os::unix::process::CommandExt;
    let deadline = Instant::now() + timeout;
    // Its own group, so a timeout stops what it started as well. That keeps
    // it out of the terminal's foreground group, so Ctrl-C reaches only
    // Relic, which passes it on.
    let mut child = cmd
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let group = libc::pid_t::try_from(child.id()).map_err(std::io::Error::other)?;
    crate::signal::set_child_group(group);
    // Before the leader is reaped, while the group id is still its own.
    let end = |child: &mut std::process::Child| {
        // SAFETY: kill(2) of the group this unreaped child leads; at worst ESRCH.
        unsafe { libc::kill(-group, libc::SIGKILL) };
        crate::signal::set_child_group(0);
        child.wait()
    };
    let stdout = capped(child.stdout.take(), max, false)?;
    let stderr = capped(child.stderr.take(), STDERR_MAX, true)?;
    let stop = |child: &mut std::process::Child| {
        end(child)?;
        Ok(Run::TimedOut)
    };
    let read = |rx: &mpsc::Receiver<_>| match rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        Ok(read) => Ok(Some(read)),
        Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(std::io::Error::other("an output reader panicked"))
        }
    };
    let (Some(out), Some(err)) = (read(&stdout)?, read(&stderr)?) else {
        return stop(&mut child);
    };
    loop {
        // SAFETY: siginfo_t is plain data, valid all zero.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid fills `info`; WNOWAIT leaves the child unreaped.
        if unsafe {
            libc::waitid(
                libc::P_PID,
                group as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: waitid set si_pid, or left it zeroed if the child still runs.
        if unsafe { info.si_pid() } != 0 {
            break;
        }
        if Instant::now() >= deadline {
            return stop(&mut child);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let status = end(&mut child)?;
    let stdout = out?;
    if stdout.len() > max {
        return Ok(Run::TooLarge);
    }
    Ok(Run::Done {
        status,
        stdout,
        stderr: err.unwrap_or_default(),
    })
}

/// The first `keep` bytes of `pipe` (stdout: one more, to tell a cut), read
/// on a thread. Stdout is dropped at the cap, so a writer past it dies of
/// EPIPE; stderr is read to its end, so a run that only warns a lot still
/// succeeds.
/// Signals stay with the main thread, which owns the child.
fn capped<R: Read + Send + 'static>(
    pipe: Option<R>,
    keep: usize,
    drain: bool,
) -> std::io::Result<mpsc::Receiver<std::io::Result<Vec<u8>>>> {
    let (tx, rx) = mpsc::channel();
    crate::signal::spawn(std::thread::Builder::new(), move || {
        let mut out = Vec::new();
        let read = pipe.map_or(Ok(()), |mut p| {
            let limit = if drain { keep } else { keep + 1 };
            p.by_ref().take(limit as u64).read_to_end(&mut out)?;
            if drain {
                std::io::copy(&mut p, &mut std::io::sink())?;
            }
            Ok(())
        });
        let _ = tx.send(read.map(|()| out));
    })?;
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn a_run_is_stopped_at_its_timeout_and_at_its_output_cap() {
        let t = Instant::now();
        assert!(matches!(
            bounded(sh("sleep 5"), Duration::from_millis(100), 64).unwrap(),
            Run::TimedOut
        ));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());

        let t = Instant::now();
        assert!(matches!(
            bounded(sh("yes"), Duration::from_secs(5), 1000).unwrap(),
            Run::TooLarge
        ));
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "the writer died at the cap: {:?}",
            t.elapsed()
        );

        let Run::Done {
            status,
            stdout,
            stderr,
        } = bounded(
            sh("printf out; echo why >&2; exit 3"),
            Duration::from_secs(5),
            3,
        )
        .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (status.code(), stdout.as_slice(), stderr.as_slice()),
            (Some(3), &b"out"[..], &b"why\n"[..])
        );

        // Exited, but what it started still holds stdout open.
        let t = Instant::now();
        assert!(matches!(
            bounded(sh("sleep 5 & exit 0"), Duration::from_millis(100), 64).unwrap(),
            Run::TimedOut
        ));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());

        // Warnings past what is kept do not kill it.
        let warns =
            "i=0; while [ $i -lt 2000 ]; do echo warning >&2; i=$((i+1)); done; printf done";
        let Run::Done {
            status,
            stdout,
            stderr,
        } = bounded(sh(warns), Duration::from_secs(5), 64).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (status.code(), stdout.as_slice(), stderr.len()),
            (Some(0), &b"done"[..], STDERR_MAX)
        );

        // What a passing run left in the background does not outlive it.
        let Run::Done { stdout, .. } = bounded(
            sh("sleep 30 >/dev/null 2>&1 & echo $!"),
            Duration::from_secs(5),
            64,
        )
        .unwrap() else {
            panic!()
        };
        let left: libc::pid_t = String::from_utf8(stdout).unwrap().trim().parse().unwrap();
        let t = Instant::now();
        // SAFETY: kill(2) with signal 0 only checks that the process exists.
        while unsafe { libc::kill(left, 0) } == 0 {
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "the background sleep is still running"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
