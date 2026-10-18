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
        s.assert_exit_code(2);
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
        self, COLS, DEADLINE, ENTER_ALT, Fixture, Hup, LEAVE_ALT, Output, RELIC, ROWS, Session,
        path_with, serial, wait_gone,
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

    /// Esc and a letter that reach relic in one read are still two keys:
    /// Esc leaves the results pane and the letter is query text.
    #[test]
    fn esc_then_a_letter_in_one_read_is_both_keys() {
        let _s = serial();
        let fx = Fixture::new();
        let mut s = ready(&fx, &[]);
        s.send(b"\t");
        s.send(b"\x1bw");
        s.wait_screen("> w ");
        quits_cleanly(s, b"\x1b");
    }

    /// Ctrl-Z gives the shell its terminal back and stops relic; `fg`
    /// (SIGCONT to the foreground job) brings the workspace back whole.
    #[test]
    fn ctrl_z_suspends_and_fg_redraws_the_whole_workspace() {
        let _s = serial();
        let fx = Fixture::new();
        let mut s = ready(&fx, &[]);
        s.send(b"\x1a");
        s.wait_stopped();
        s.assert_given_back();
        s.with(|o| assert!(!o.screen.in_alt(), "{}", pty_support::dump(o)));
        // Re-entering the alternate screen blanks it: only a full redraw
        // brings the rows back.
        s.fg();
        s.wait_screen("fixture-cmd-40");
        s.with(|o| assert!(o.screen.in_alt(), "{}", pty_support::dump(o)));
        // Typing works again: the reader was resumed.
        s.send(b"zz");
        s.wait_screen("> zz ");
        quits_cleanly(s, b"\x1b");
    }

    /// `kill %1` on a stopped relic sends TERM then CONT while the shell
    /// owns the terminal: relic dies of TERM, and putting the tty back from
    /// the background must not stop it again.
    #[test]
    fn a_fatal_signal_after_ctrl_z_kills_relic_from_the_background() {
        let _s = serial();
        let fx = Fixture::new();
        let mut s = ready(&fx, &[]);
        s.send(b"\x1a");
        s.wait_stopped();
        s.signal(libc::SIGTERM);
        s.signal(libc::SIGCONT);
        let status = s.wait_exit();
        assert_eq!(status.signal(), Some(libc::SIGTERM), "{}", pty_support::describe(status));
        s.assert_restored();
    }

    /// Ctrl-Z ignored by whoever started relic stays ignored: relic says so
    /// and keeps running.
    #[test]
    fn ctrl_z_under_an_inherited_ignored_sigtstp_does_not_suspend() {
        let _s = serial();
        let fx = Fixture::new();
        let script = "trap '' TSTP; exec \"$0\" ui --db \"$1\"";
        let args = ["-c".as_ref(), script.as_ref(), RELIC.as_ref(), fx.db.as_os_str()];
        let mut s = Session::spawn_program("/bin/sh".as_ref(), &args, &[]);
        s.wait_ready();
        s.send(b"\x1a");
        s.wait_screen("not suspending");
        s.send(b"zz");
        s.wait_screen("> zz ");
        quits_cleanly(s, b"\x1b");
    }

    /// A dumb terminal gets a message, not escape sequences it cannot draw.
    #[test]
    fn a_dumb_terminal_is_refused_without_touching_it() {
        let _s = serial();
        let fx = Fixture::new();
        let mut s = Session::spawn(&fx.db, &[("TERM", "dumb".as_ref())]);
        s.assert_exit_code(2);
        s.assert_restored();
        s.with(|o| {
            assert!(o.contains("TERM is dumb or unset"), "{}", pty_support::dump(o));
            assert_eq!(o.count(ENTER_ALT), 0);
        });
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
        // The notice goes when the window closes, with no key pressed.
        s.wait_for("the notice to expire", |o| !o.screen.text().contains("Ctrl-C again"));
        // Past the 1 s window a second press is a first press again.
        s.assert_alive_for(Duration::from_millis(300));
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

    /// While the workspace is replaced by "too small", keys do nothing to
    /// panes the user cannot see: q in the results pane does not quit, and
    /// typing does not reach the query.
    #[test]
    fn keys_do_nothing_while_the_terminal_is_too_small() {
        let _s = serial();
        let fx = Fixture::new();
        let mut s = ready(&fx, &[]);
        s.send(b"\t");
        s.resize(8, 30);
        s.signal(libc::SIGWINCH);
        s.wait_screen("terminal too small");
        s.send(b"qx");
        s.assert_alive_for(Duration::from_millis(300));
        s.resize(24, 80);
        s.signal(libc::SIGWINCH);
        s.wait_for("the workspace back at 80 columns", |o| {
            o.screen.text().lines().next().is_some_and(|top| top.chars().count() == 80)
        });
        s.with(|o| assert!(!o.screen.text().contains("> x"), "{}", pty_support::dump(o)));
        quits_cleanly(s, b"q");
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

    /// A fatal signal sent to relic alone (`kill`, `timeout`, a supervisor)
    /// leaves the terminal as it was and relic dead of that signal.
    #[test]
    fn a_fatal_signal_restores_the_terminal_and_exits_by_that_signal() {
        let _s = serial();
        let fx = Fixture::new();
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
            let mut s = ready(&fx, &[]);
            s.signal(sig);
            let status = s.wait_exit();
            assert_eq!(status.signal(), Some(sig), "{}", pty_support::describe(status));
            s.assert_restored();
        }
    }

    /// An fzf that ignores SIGTERM and leaves the tty raw: relic kills it
    /// and still gives the shell back the terminal it had.
    const STUBBORN_PICKER: &str = "cat > /dev/null\n\
        trap '' TERM\n\
        stty raw -echo < /dev/tty\n\
        printf 'FAKE-FZF-READY\\r\\n' > /dev/tty\n\
        IFS= read -r line < /dev/tty";

    fn signal_while_fzf_runs(mut s: Session, fzf: &FakeFzf) {
        s.wait_for("the fake fzf to wait on /dev/tty", |o| o.contains("FAKE-FZF-READY"));
        s.signal(libc::SIGTERM);
        let status = s.wait_exit();
        assert_eq!(status.signal(), Some(libc::SIGTERM), "{}", pty_support::describe(status));
        s.assert_restored();
        let pids = String::from_utf8(fzf.read("pids")).unwrap();
        for p in pids.lines() {
            let p: libc::pid_t = p.parse().unwrap();
            assert!(wait_gone(p, Duration::from_secs(1)), "fake fzf pid {p} outlived relic");
        }
    }

    #[test]
    fn a_fatal_signal_while_fzf_runs_in_the_tui_stops_fzf_and_restores() {
        let _s = serial();
        let fx = Fixture::new();
        let fzf = FakeFzf::new(STUBBORN_PICKER);
        let path = path_with(fzf.dir.path());
        let s = ready(&fx, &[("PATH", &path)]);
        s.send(b"\x06");
        signal_while_fzf_runs(s, &fzf);
    }

    #[test]
    fn a_fatal_signal_while_search_fzf_runs_stops_fzf_and_restores() {
        let _s = serial();
        let fx = Fixture::new();
        let fzf = FakeFzf::new(STUBBORN_PICKER);
        let path = path_with(fzf.dir.path());
        let args = ["search", "fixture", "--fzf", "--db"].map(OsStr::new);
        let s = Session::spawn_args(&[&args[..], &[fx.db.as_os_str()]].concat(), &[("PATH", &path)]);
        signal_while_fzf_runs(s, &fzf);
    }

    /// A signal the user's shell left ignored (`nohup`, `trap '' TERM`)
    /// stays ignored.
    #[test]
    fn an_inherited_ignored_signal_stays_ignored() {
        let _s = serial();
        let fx = Fixture::new();
        let script = "trap '' TERM; exec \"$0\" ui --db \"$1\"";
        let args = ["-c".as_ref(), script.as_ref(), RELIC.as_ref(), fx.db.as_os_str()];
        let mut s = Session::spawn_program("/bin/sh".as_ref(), &args, &[]);
        s.wait_ready();
        s.signal(libc::SIGTERM);
        s.assert_alive_for(Duration::from_millis(300));
        quits_cleanly(s, b"\x1b");
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
            // Neither startup nor the resume asked where the cursor is: a
            // terminal that does not answer would stall them 2 s.
            assert_eq!(o.count(b"\x1b[6n"), 0, "{shown}");
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

    /// `search --fzf` keeps fzf's own answers apart: a pick is 0, Enter
    /// with nothing matching is 1, and a cancel is 130 (ADR 0011).
    #[test]
    fn search_fzf_exits_0_on_a_pick_1_on_no_match_and_130_on_cancel() {
        let _s = serial();
        let fx = Fixture::new();
        for (body, code) in [
            ("cat > /dev/null; printf '1\\t\\0'", 0),
            ("cat > /dev/null; exit 1", 1),
            ("cat > /dev/null; exit 130", 130),
        ] {
            let fzf = FakeFzf::new(body);
            let path = path_with(fzf.dir.path());
            let args = ["search", "fixture", "--fzf", "--db"].map(OsStr::new);
            let mut s = Session::spawn_args(&[&args[..], &[fx.db.as_os_str()]].concat(), &[("PATH", &path)]);
            s.assert_exit_code(code);
            fzf.assert_reaped();
        }
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
