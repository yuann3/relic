//! Terminal lifecycle of the real `relic ui` binary on a pseudo-terminal
//! (docs/research/tui-fzf-integration.md, "Tests > PTY lifecycle"): every
//! way out of the workspace must leave the tty cooked and echoing, the
//! cursor shown and the alternate screen left, within a hard deadline.

#![cfg(unix)]

// The headless build runs only one case and leaves most helpers unused.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
mod pty_support;

// Shared with the picker tests; this binary uses only `FakeFzf`.
#[cfg(feature = "tui")]
#[allow(dead_code)]
mod fake_fzf;

#[cfg(not(feature = "tui"))]
mod headless {
    use super::pty_support::{self, ENTER_ALT, Fixture, Session, serial};

    #[test]
    fn ui_refuses_without_touching_the_terminal() {
        let _s = serial();
        let fx = Fixture::new();
        let mut s = Session::spawn(&fx.db, &[]);
        s.assert_exit_code(1);
        s.assert_restored();
        s.with(|o| {
            assert!(
                o.contains("built without the TUI"),
                "{}",
                pty_support::dump(o)
            );
            assert_eq!(o.count(ENTER_ALT), 0);
        });
    }
}

#[cfg(feature = "tui")]
mod tui {
    use std::ffi::OsStr;
    use std::os::unix::process::ExitStatusExt;
    use std::time::Duration;

    use super::pty_support::{
        self, COLS, DEADLINE, ENTER_ALT, Fixture, Hup, LEAVE_ALT, Output, ROWS, Session, path_with,
        serial, wait_gone,
    };

    use super::fake_fzf::FakeFzf;

    fn ready(fx: &Fixture, env: &[(&str, &OsStr)]) -> Session {
        let mut s = Session::spawn(&fx.db, env);
        s.wait_ready();
        s
    }

    fn quits_cleanly(mut s: Session, keys: &[u8]) {
        s.send(keys);
        s.assert_exit_code(0);
        s.assert_restored();
    }

    #[test]
    fn esc_from_the_query_bar_quits() {
        let _s = serial();
        let fx = Fixture::new();
        quits_cleanly(ready(&fx, &[]), b"\x1b");
    }

    #[test]
    fn q_quits_from_a_pane_but_is_text_in_the_query_bar() {
        let _s = serial();
        let fx = Fixture::new();
        let mut s = ready(&fx, &[]);
        s.send(b"q");
        s.wait_screen("> q ");
        s.assert_alive_for(Duration::from_millis(300));
        // Tab moves focus to the results pane, where q is a command.
        quits_cleanly(s, b"\tq");
    }

    #[test]
    fn double_ctrl_c_quits() {
        let _s = serial();
        let fx = Fixture::new();
        quits_cleanly(ready(&fx, &[]), b"\x03\x03");
    }

    #[test]
    fn single_ctrl_c_only_warns() {
        let _s = serial();
        let fx = Fixture::new();
        let mut s = ready(&fx, &[]);
        s.send(b"\x03");
        s.wait_screen("press Ctrl-C again within 1 s to quit");
        // Past the 1 s window a second press is a first press again.
        s.assert_alive_for(Duration::from_millis(1300));
        s.send(b"\x03");
        s.assert_alive_for(Duration::from_millis(300));
        quits_cleanly(s, b"\x1b");
    }

    /// `RELIC_UI_PANIC_AT` panics right after the named entry stage
    /// (src/ui/term.rs `fault_point`). The panic hook must undo exactly the
    /// steps taken, and the message must survive on the main screen.
    fn panic_after(stage: &str) {
        let _s = serial();
        let fx = Fixture::new();
        let mut s = Session::spawn(&fx.db, &[("RELIC_UI_PANIC_AT", stage.as_ref())]);
        s.assert_exit_code(101);
        s.assert_restored();
        let msg = format!("injected panic after {stage}");
        s.with(|o| {
            let shown = pty_support::dump(o);
            let entered_alt = stage != "raw";
            assert_eq!(o.count(ENTER_ALT) > 0, entered_alt, "{shown}");
            assert!(!o.screen.in_alt(), "{shown}");
            assert!(
                o.screen.text().contains(&msg),
                "panic message lost\n{shown}"
            );
        });
    }

    #[test]
    fn panic_after_raw_mode_restores() {
        panic_after("raw");
    }

    #[test]
    fn panic_after_alternate_screen_restores() {
        panic_after("alt");
    }

    #[test]
    fn panic_after_hiding_the_cursor_restores() {
        panic_after("cursor");
    }

    #[test]
    fn panic_after_bracketed_paste_restores() {
        panic_after("paste");
    }

    #[test]
    fn panic_after_the_first_frame_restores() {
        panic_after("frame");
    }

    /// 50 window-size changes, each with its SIGWINCH, as fast as possible.
    fn resize_storm(s: &Session) {
        for i in 0..50u16 {
            s.resize(12 + i % 25, 40 + (i * 7) % 90);
            s.signal(libc::SIGWINCH);
        }
        s.resize(24, 80);
        s.signal(libc::SIGWINCH);
    }

    /// The query box's top right corner sits in the last column of 80.
    fn drawn_for_80_columns(o: &Output) -> bool {
        let text = o.screen.text();
        let top = text.lines().next().unwrap_or("");
        o.screen.in_alt() && top.chars().count() == 80 && top.ends_with('┓')
    }

    #[test]
    fn resize_storm_then_quit() {
        let _s = serial();
        let fx = Fixture::new();
        let mut s = ready(&fx, &[]);
        resize_storm(&s);
        s.wait_for("a frame for the final 24x80 size", drawn_for_80_columns);
        s.send(b"cmd-07");
        s.wait_for("the query typed after the storm", |o| {
            drawn_for_80_columns(o)
                && o.screen.text().contains("> cmd-07")
                && o.screen.text().contains("echo fixture-cmd-07")
        });
        quits_cleanly(s, b"\x1b");
    }

    /// Keys typed while resizes are still being handled sit unread until
    /// the next keystroke. Observed on macOS in ~40% of single storms: the
    /// input thread waits in kevent with the bytes pending, and one more key
    /// delivers them all. Five storms fail almost every run there; on Linux
    /// (epoll) it is rarer, 2 of 8 runs of this test.
    #[test]
    #[ignore = "lost wakeup in the crossterm 0.29 event reader: keys typed \
                during a resize storm are not read until the next keystroke"]
    fn keys_typed_during_a_resize_storm_are_read() {
        let _s = serial();
        let fx = Fixture::new();
        for round in 0..5 {
            let mut s = ready(&fx, &[]);
            resize_storm(&s);
            s.send(b"cmd-07");
            s.wait_for(&format!("the query typed during storm {round}"), |o| {
                o.screen.text().contains("> cmd-07")
            });
            quits_cleanly(s, b"\x1b");
        }
    }

    /// The emulator agrees with the size relic was started at; guards the
    /// resize assertion above against a vacuous corner check.
    #[test]
    fn first_frame_fills_the_starting_size() {
        let _s = serial();
        let fx = Fixture::new();
        let mut s = ready(&fx, &[]);
        s.wait_for("a full-width first frame", |o| {
            let text = o.screen.text();
            text.lines().count() == ROWS as usize
                && text
                    .lines()
                    .next()
                    .is_some_and(|l| l.chars().count() == COLS as usize)
        });
        quits_cleanly(s, b"\x1b");
    }

    /// Closing the terminal window: the master closes and the shell passes
    /// SIGHUP on to the foreground job. Termios cannot be checked
    /// afterwards; the terminal no longer exists.
    #[test]
    fn hangup_kills_relic() {
        let _s = serial();
        let fx = Fixture::new();
        let mut s = ready(&fx, &[]);
        s.hang_up();
        let status = s.wait_exit();
        assert_eq!(
            status.signal(),
            Some(libc::SIGHUP),
            "{}",
            pty_support::describe(status)
        );
    }

    /// The same disconnect under `nohup`: no signal, so relic must notice
    /// the dead tty itself and exit.
    #[test]
    #[ignore = "relic never exits: crossterm 0.29's reader retries read(2) \
                == 0 on the hung-up tty forever, so the input thread spins at \
                ~100% CPU and never reports InputClosed"]
    fn hangup_without_sighup_exits() {
        let _s = serial();
        let fx = Fixture::new();
        let mut s = Session::spawn_with(&fx.db, &[], Hup::Ignore);
        s.wait_ready();
        s.hang_up();
        s.wait_exit();
    }

    /// Hangup while fzf owns the terminal: neither relic nor fzf survives.
    #[test]
    fn hangup_while_fzf_runs_leaves_no_process() {
        let _s = serial();
        let fx = Fixture::new();
        let fzf = FakeFzf::new(
            "cat > /dev/null\n\
             printf 'FAKE-FZF-READY\\r\\n' > /dev/tty\n\
             IFS= read -r line < /dev/tty\n\
             exit 130",
        );
        let path = path_with(fzf.dir.path());
        let mut s = ready(&fx, &[("PATH", &path)]);
        let pid = s.pid();
        s.send(b"\x06");
        s.wait_for("the fake fzf to wait on /dev/tty", |o| {
            o.contains("FAKE-FZF-READY")
        });
        s.hang_up();
        let status = s.wait_exit();
        assert_eq!(
            status.signal(),
            Some(libc::SIGHUP),
            "{}",
            pty_support::describe(status)
        );
        assert!(
            wait_gone(pid, Duration::from_secs(1)),
            "relic pid {pid} still exists"
        );
        let pids = String::from_utf8(fzf.read("pids")).unwrap();
        for p in pids.lines() {
            let p: libc::pid_t = p.parse().unwrap();
            assert!(
                wait_gone(p, DEADLINE / 2),
                "fake fzf pid {p} outlived the hangup"
            );
        }
    }

    /// Records how it was started, then picks the page's last record (the
    /// first is already selected, so it could not show that a pick landed).
    const RECORDING_PICKER: &str = "\
        if [ -p /dev/stdin ] && ! [ -t 0 ]; then echo pipe; else echo other; fi > \"$D/stdin_kind\"\n\
        if ( : < /dev/tty ) 2>/dev/null; then echo yes; else echo no; fi > \"$D/tty_ok\"\n\
        stty -a < /dev/tty > \"$D/stty\" 2>&1\n\
        tr '\\0' '\\n' > \"$D/records\"\n\
        printf 'FAKE-FZF-RAN\\r\\n' > /dev/tty\n\
        tail -n 1 \"$D/records\" > \"$D/picked\"\n\
        printf '%s\\t\\0' \"$(cut -f1 \"$D/picked\")\"";

    /// Back on the alternate screen (entered a second time) with a whole
    /// frame: the query title never changes, so it is only there again if
    /// every cell was redrawn after re-entry, not just the changed ones.
    fn resumed(o: &Output, showing: &str) -> bool {
        let text = o.screen.text();
        o.count(ENTER_ALT) == 2
            && o.screen.in_alt()
            && text.contains("Query · local index · offline")
            && text.contains(showing)
    }

    fn read_text(fzf: &FakeFzf, name: &str) -> String {
        String::from_utf8(fzf.read(name)).unwrap()
    }

    /// The suspend-run-resume sequence: leave the alternate screen and raw
    /// mode, let fzf own the tty, then re-enter and redraw with the pick.
    #[test]
    fn f_hands_the_terminal_to_fzf_and_resumes_with_the_pick() {
        let _s = serial();
        let fx = Fixture::new();
        let fzf = FakeFzf::new(RECORDING_PICKER);
        let path = path_with(fzf.dir.path());
        let mut s = ready(&fx, &[("PATH", &path)]);
        s.send(b"\tf");
        s.wait_for("fzf to run and the TUI to come back", |o| {
            resumed(o, "[ Evidence")
        });

        assert_eq!(read_text(&fzf, "stdin_kind").trim(), "pipe");
        assert_eq!(read_text(&fzf, "tty_ok").trim(), "yes");
        let stty = read_text(&fzf, "stty");
        let modes: Vec<&str> = stty
            .split(|c: char| c.is_whitespace() || c == ';')
            .collect();
        for mode in ["icanon", "echo", "isig"] {
            assert!(
                modes.contains(&mode) && !modes.contains(&format!("-{mode}").as_str()),
                "fzf saw the tty without {mode}:\n{stty}"
            );
        }

        let picked = read_text(&fzf, "picked");
        let fields: Vec<&str> = picked.trim_end().split('\t').collect();
        let (id, text) = (fields[0], fields[3]);
        assert_eq!(
            text, "echo fixture-cmd-01",
            "newest first, so the last is the oldest"
        );
        s.wait_screen(&format!("id: #{id} "));
        s.wait_screen(text);
        s.with(|o| {
            // fzf ran strictly between leaving and re-entering.
            let shown = pty_support::dump(o);
            let ran = o.last(b"FAKE-FZF-RAN").expect("fake fzf wrote to /dev/tty");
            let left = position(o, LEAVE_ALT, 0).expect("left the alternate screen");
            let first = position(o, ENTER_ALT, 0).unwrap();
            let again = position(o, ENTER_ALT, first + 1).unwrap();
            assert!(first < left && left < ran && ran < again, "{shown}");
        });
        fzf.assert_reaped();
        quits_cleanly(s, b"q");
    }

    fn position(o: &Output, needle: &[u8], from: usize) -> Option<usize> {
        o.raw[from..]
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|i| i + from)
    }

    /// While fzf runs, relic's input thread is parked: a line typed at the
    /// terminal reaches fzf, not the query. The fake reads only after a
    /// pause, so the line waits in the tty where a second reader would find
    /// it first; a reader racing an already blocked read(2) always loses.
    #[test]
    fn keystrokes_typed_during_fzf_reach_fzf_not_the_query() {
        let _s = serial();
        let fx = Fixture::new();
        let fzf = FakeFzf::new(
            "cat > /dev/null\n\
             printf 'FAKE-FZF-READY\\r\\n' > /dev/tty\n\
             sleep 0.5\n\
             IFS= read -r line < /dev/tty\n\
             printf '%s' \"$line\" > \"$D/typed\"\n\
             exit 130",
        );
        let path = path_with(fzf.dir.path());
        let mut s = ready(&fx, &[("PATH", &path)]);
        s.send(b"\x06");
        s.wait_for("the fake fzf to wait on /dev/tty", |o| {
            o.contains("FAKE-FZF-READY")
        });
        s.send(b"stolen-check\r");
        s.wait_for("the TUI to come back", |o| resumed(o, "fzf cancelled"));
        assert_eq!(read_text(&fzf, "typed"), "stolen-check");
        s.with(|o| {
            let text = o.screen.text();
            assert!(
                text.contains("> type to search your history"),
                "{}",
                pty_support::dump(o)
            );
            assert!(!text.contains("stolen"), "{}", pty_support::dump(o));
        });
        fzf.assert_reaped();
        quits_cleanly(s, b"\x1b");
    }

    fn fzf_outcome(body: &str, notice: &str) {
        let _s = serial();
        let fx = Fixture::new();
        let fzf = FakeFzf::new(body);
        let path = path_with(fzf.dir.path());
        let mut s = ready(&fx, &[("PATH", &path)]);
        s.send(b"\x06");
        s.wait_for(&format!("the TUI to come back with {notice:?}"), |o| {
            resumed(o, notice)
        });
        fzf.assert_reaped();
        quits_cleanly(s, b"\x1b");
    }

    #[test]
    fn fzf_cancel_resumes() {
        fzf_outcome("cat > /dev/null; exit 130", "fzf cancelled");
    }

    #[test]
    fn fzf_killed_resumes() {
        fzf_outcome(
            "cat > /dev/null; kill -9 $$",
            "fzf returned an invalid selection",
        );
    }

    #[test]
    fn fzf_missing_resumes() {
        let _s = serial();
        let fx = Fixture::new();
        let empty = tempfile::tempdir().unwrap();
        let mut s = ready(&fx, &[("PATH", empty.path().as_os_str())]);
        s.send(b"\x06");
        s.wait_for("the TUI to come back", |o| {
            resumed(o, "fzf unavailable; using indexed search")
        });
        quits_cleanly(s, b"\x1b");
    }
}
