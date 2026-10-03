//! Terminal ownership: entering/leaving raw + alternate screen, the panic
//! restore path, and the one thread allowed to read terminal input.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::{cursor, execute, terminal};

use super::model::{Key, UiAction};

/// One flag per entry step that has taken effect. They are process-wide
/// because the terminal is: the guard and the panic hook both undo a step
/// only by swapping its flag off, so each step is undone exactly once.
/// (Undoing twice is not harmless: a second `?1049l` restores the saved
/// cursor again and the shell then overwrites the panic message.)
static RAW: AtomicBool = AtomicBool::new(false);
static ALT: AtomicBool = AtomicBool::new(false);
static HIDDEN: AtomicBool = AtomicBool::new(false);
static PASTE: AtomicBool = AtomicBool::new(false);
static PANICKED: AtomicBool = AtomicBool::new(false);

fn took(flag: &AtomicBool) -> bool {
    flag.swap(false, Ordering::SeqCst)
}

#[cfg(unix)]
static ORIGINAL_TERMIOS: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();

/// Prototype fault injection for the PTY restore checks: panics right after
/// the named entry stage (`raw`, `alt`, `cursor`, `paste`, `frame`). Removed
/// once #11 decides on a permanent PTY test harness.
pub fn fault_point(stage: &str) {
    if std::env::var("RELIC_UI_PANIC_AT").is_ok_and(|s| s == stage) {
        panic!("injected panic after {stage}");
    }
}

pub fn panicked() -> bool {
    PANICKED.load(Ordering::SeqCst)
}

/// Handle on the full-screen terminal state. Only one exists at a time.
pub struct TerminalGuard(());

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::other("relic ui needs an interactive terminal"));
        }
        install_panic_hook();
        let mut g = Self(());
        if let Err(e) = g.resume() {
            let _ = g.restore();
            return Err(e);
        }
        Ok(g)
    }

    /// (Re-)enter the full-screen state after `restore`, e.g. after fzf.
    /// Each flag is set only once its step took effect, so a failure part
    /// way leaves exactly the completed steps for `restore` to undo.
    pub fn resume(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        save_termios();
        let mut out = io::stdout();
        terminal::enable_raw_mode()?;
        RAW.store(true, Ordering::SeqCst);
        fault_point("raw");
        execute!(out, terminal::EnterAlternateScreen)?;
        ALT.store(true, Ordering::SeqCst);
        fault_point("alt");
        execute!(out, cursor::Hide)?;
        HIDDEN.store(true, Ordering::SeqCst);
        fault_point("cursor");
        execute!(out, EnableBracketedPaste)?;
        PASTE.store(true, Ordering::SeqCst);
        fault_point("paste");
        Ok(())
    }

    /// Idempotent, reverse order. Attempts every step even if one fails and
    /// reports the first failure, so one bad write cannot leave the tty raw.
    pub fn restore(&mut self) -> io::Result<()> {
        let mut out = io::stdout();
        let mut first: Option<io::Error> = None;
        let mut note = |r: io::Result<()>| {
            if let Err(e) = r {
                first.get_or_insert(e);
            }
        };
        if took(&PASTE) {
            note(execute!(out, DisableBracketedPaste));
        }
        if took(&HIDDEN) {
            note(execute!(out, cursor::Show));
        }
        if took(&ALT) {
            note(execute!(out, terminal::LeaveAlternateScreen));
        }
        if took(&RAW) {
            note(terminal::disable_raw_mode());
        }
        note(out.flush());
        first.map_or(Ok(()), Err)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(unix)]
fn save_termios() {
    if ORIGINAL_TERMIOS.get().is_some() {
        return;
    }
    // SAFETY: tcgetattr writes a termios for a valid fd; zeroed is a valid
    // initial bit pattern for this plain C struct.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut t) == 0 {
            let _ = ORIGINAL_TERMIOS.set(t);
        }
    }
}

fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANICKED.store(true, Ordering::SeqCst);
            emergency_restore();
            previous(info);
        }));
    });
}

/// Runs inside the panic hook, possibly while another frame holds stdout's
/// lock or crossterm's raw-mode mutex, so it takes neither: raw `write(2)`
/// of the reset sequences and `tcsetattr` with the termios saved at entry.
fn emergency_restore() {
    #[cfg(unix)]
    {
        let write = |bytes: &[u8]| {
            // SAFETY: write(2) on our own stdout fd with a static buffer.
            unsafe { libc::write(libc::STDOUT_FILENO, bytes.as_ptr().cast(), bytes.len()) };
        };
        if took(&PASTE) {
            write(b"\x1b[?2004l");
        }
        if took(&HIDDEN) {
            write(b"\x1b[?25h");
        }
        if took(&ALT) {
            write(b"\x1b[?1049l");
        }
        if took(&RAW)
            && let Some(t) = ORIGINAL_TERMIOS.get()
        {
            // SAFETY: restores a termios captured by tcgetattr on this fd.
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t) };
        }
    }
    #[cfg(not(unix))]
    {
        let _ = TerminalGuard(()).restore();
    }
}

/// How long the reader blocks in `poll` before rechecking its gate. Bounds
/// both the pause handshake and quit latency; idle cost is one syscall per
/// period, not a render.
const POLL: Duration = Duration::from_millis(100);

#[derive(Default)]
struct Gate {
    pause: bool,
    parked: bool,
    stop: bool,
}

/// The only terminal reader. It can be parked so an external program (fzf)
/// owns the tty without a second reader stealing its keystrokes.
pub struct InputReader {
    gate: Arc<(Mutex<Gate>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

impl InputReader {
    pub fn spawn(tx: SyncSender<UiAction>) -> Self {
        let gate = Arc::new((Mutex::new(Gate::default()), Condvar::new()));
        let g = Arc::clone(&gate);
        let handle = std::thread::Builder::new()
            .name("relic-input".into())
            .spawn(move || read_loop(&g, &tx))
            .expect("spawn input thread");
        Self {
            gate,
            handle: Some(handle),
        }
    }

    /// Returns once the reader is parked outside `poll`/`read`.
    pub fn pause(&self) {
        let (lock, cv) = &*self.gate;
        let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
        g.pause = true;
        while !g.parked {
            g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    pub fn resume(&self) {
        let (lock, cv) = &*self.gate;
        lock.lock().unwrap_or_else(|e| e.into_inner()).pause = false;
        cv.notify_all();
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        let (lock, cv) = &*self.gate;
        lock.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        cv.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn read_loop(gate: &(Mutex<Gate>, Condvar), tx: &SyncSender<UiAction>) {
    let (lock, cv) = gate;
    let park = |stop_too: bool| {
        let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
        g.parked = true;
        cv.notify_all();
        if stop_too {
            g.stop = true;
        }
    };
    loop {
        {
            let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
            while g.pause && !g.stop {
                g.parked = true;
                cv.notify_all();
                g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
            g.parked = false;
            if g.stop {
                g.parked = true;
                cv.notify_all();
                return;
            }
        }
        let ready = match event::poll(POLL) {
            Ok(r) => r,
            Err(_) => {
                park(true);
                let _ = tx.send(UiAction::InputClosed);
                return;
            }
        };
        if !ready {
            continue;
        }
        match event::read() {
            Ok(ev) => {
                if let Some(a) = translate(ev, Instant::now())
                    && tx.send(a).is_err()
                {
                    park(true);
                    return;
                }
            }
            Err(_) => {
                park(true);
                let _ = tx.send(UiAction::InputClosed);
                return;
            }
        }
    }
}

/// Crossterm event -> UI action. Pure, so the key map is testable.
pub fn translate(ev: Event, now: Instant) -> Option<UiAction> {
    match ev {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            ..
        }) => {
            if kind == KeyEventKind::Release {
                return None;
            }
            let ctrl = modifiers.contains(KeyModifiers::CONTROL);
            if modifiers.contains(KeyModifiers::ALT) {
                return None;
            }
            let key = match code {
                KeyCode::Char('c') if ctrl => return Some(UiAction::Interrupt(now)),
                KeyCode::Char(c) if ctrl => Key::Ctrl(c.to_ascii_lowercase()),
                KeyCode::Char(c) => Key::Char(c),
                KeyCode::Up => Key::Up,
                KeyCode::Down => Key::Down,
                KeyCode::PageUp => Key::PageUp,
                KeyCode::PageDown => Key::PageDown,
                KeyCode::Enter => Key::Enter,
                KeyCode::Esc => Key::Esc,
                KeyCode::Backspace => Key::Backspace,
                KeyCode::Tab => Key::Tab,
                KeyCode::BackTab => Key::BackTab,
                _ => return None,
            };
            Some(UiAction::Key(key))
        }
        Event::Paste(s) => Some(UiAction::Paste(s)),
        Event::Resize(..) => Some(UiAction::Resize),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn key_translation() {
        let now = Instant::now();
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let cases: &[(Event, Option<Key>)] = &[
            (key(KeyCode::Char('j'), none), Some(Key::Char('j'))),
            (
                key(KeyCode::Char('Q'), KeyModifiers::SHIFT),
                Some(Key::Char('Q')),
            ),
            (key(KeyCode::Char('n'), ctrl), Some(Key::Ctrl('n'))),
            (
                key(KeyCode::Char('F'), ctrl | KeyModifiers::SHIFT),
                Some(Key::Ctrl('f')),
            ),
            (key(KeyCode::Char('x'), KeyModifiers::ALT), None),
            (
                key(KeyCode::BackTab, KeyModifiers::SHIFT),
                Some(Key::BackTab),
            ),
            (key(KeyCode::F(5), none), None),
        ];
        for (ev, want) in cases {
            let got = match translate(ev.clone(), now) {
                Some(UiAction::Key(k)) => Some(k),
                None => None,
                other => panic!("{ev:?} -> {other:?}"),
            };
            assert_eq!(got, *want, "{ev:?}");
        }
        assert!(
            matches!(translate(key(KeyCode::Char('c'), ctrl), now), Some(UiAction::Interrupt(t)) if t == now)
        );
        let release = Event::Key(KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: none,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        });
        assert!(translate(release, now).is_none());
        assert!(
            matches!(translate(Event::Paste("a\nb".into()), now), Some(UiAction::Paste(s)) if s == "a\nb")
        );
        assert!(matches!(
            translate(Event::Resize(80, 24), now),
            Some(UiAction::Resize)
        ));
    }
}
