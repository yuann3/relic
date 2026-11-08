//! Terminal ownership: entering/leaving raw + alternate screen, the panic
//! restore path, and the one thread allowed to read terminal input.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, Once, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, KeyCode,
    KeyEvent, KeyEventKind, KeyModifiers,
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

/// What the panic hook records, without a real panic; one-way, like the hook.
#[cfg(test)]
pub fn mark_panicked() {
    PANICKED.store(true, Ordering::SeqCst);
}

/// Handle on the full-screen terminal state. Only one exists at a time.
pub struct TerminalGuard(());

impl TerminalGuard {
    /// A guard that has entered nothing, for driving the event loop on a
    /// test backend.
    #[cfg(test)]
    pub fn detached() -> Self {
        Self(())
    }

    pub fn enter() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::other("ui needs an interactive terminal"));
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
    ///
    /// A program that owned the tty and was killed (the picker SIGKILLs an
    /// fzf that overflows its output) leaves its modes behind, so this
    /// starts from the user's termios and turns mouse reporting off.
    /// crossterm records whatever mode it finds as the one to restore at
    /// exit; without the reset the shell would get fzf's raw, no-echo tty.
    pub fn resume(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            save_termios();
            if !RAW.load(Ordering::SeqCst)
                && let Some(t) = ORIGINAL_TERMIOS.get()
            {
                // SAFETY: restores a termios captured by tcgetattr on this fd.
                unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t) };
            }
        }
        let mut out = io::stdout();
        execute!(out, DisableMouseCapture)?;
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
pub(crate) fn emergency_restore() {
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
/// Retry period for a send into a full UI channel.
const SEND_RETRY: Duration = Duration::from_millis(10);

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
        Self::spawn_with(tx, || {
            if event::poll(POLL)? {
                event::read().map(Some)
            } else {
                Ok(None)
            }
        })
    }

    /// `next` is one bounded wait for terminal input: `Ok(None)` when the
    /// wait ended with nothing to read.
    pub fn spawn_with(
        tx: SyncSender<UiAction>,
        mut next: impl FnMut() -> io::Result<Option<Event>> + Send + 'static,
    ) -> Self {
        let gate = Arc::new((Mutex::new(Gate::default()), Condvar::new()));
        let g = Arc::clone(&gate);
        let builder = std::thread::Builder::new().name("relic-input".into());
        let handle = crate::signal::spawn(builder, move || read_loop(&g, &tx, &mut next))
            .expect("spawn input thread");
        Self {
            gate,
            handle: Some(handle),
        }
    }

    /// Returns once the reader is parked outside `poll`/`read`.
    pub fn pause(&self) {
        let (lock, cv) = &*self.gate;
        let mut g = lock.lock().unwrap_or_else(PoisonError::into_inner);
        g.pause = true;
        // A reader backing off on a full channel waits on this condvar.
        cv.notify_all();
        while !g.parked {
            g = cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
    }

    pub fn resume(&self) {
        let (lock, cv) = &*self.gate;
        lock.lock().unwrap_or_else(PoisonError::into_inner).pause = false;
        cv.notify_all();
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        let (lock, cv) = &*self.gate;
        lock.lock().unwrap_or_else(PoisonError::into_inner).stop = true;
        cv.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn read_loop(
    gate: &(Mutex<Gate>, Condvar),
    tx: &SyncSender<UiAction>,
    next: &mut impl FnMut() -> io::Result<Option<Event>>,
) {
    'read: while wait_while_paused(gate) {
        let actions = match next() {
            Ok(None) => continue,
            Ok(Some(ev)) => translate(ev, Instant::now()),
            Err(_) => {
                send(gate, tx, UiAction::InputClosed);
                break;
            }
        };
        for action in actions {
            if !send(gate, tx, action) {
                break 'read;
            }
        }
    }
    // However the loop ended, a pause in progress must return and later
    // ones must not wait.
    let (lock, cv) = gate;
    let mut g = lock.lock().unwrap_or_else(PoisonError::into_inner);
    g.parked = true;
    g.stop = true;
    cv.notify_all();
}

/// Parks while paused. False once stopped.
fn wait_while_paused((lock, cv): &(Mutex<Gate>, Condvar)) -> bool {
    let mut g = lock.lock().unwrap_or_else(PoisonError::into_inner);
    while g.pause && !g.stop {
        g.parked = true;
        cv.notify_all();
        g = cv.wait(g).unwrap_or_else(PoisonError::into_inner);
    }
    g.parked = false;
    !g.stop
}

/// Delivers `a`, waiting while the channel is full. A blocking `send` would
/// deadlock: the only thread that drains the channel may be the one waiting
/// in `pause` or joining this thread on drop, so every wait goes back
/// through the gate. False once stopped or the receiver is gone.
fn send(gate: &(Mutex<Gate>, Condvar), tx: &SyncSender<UiAction>, mut a: UiAction) -> bool {
    loop {
        match tx.try_send(a) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(back)) => a = back,
        }
        {
            let (lock, cv) = gate;
            let g = lock.lock().unwrap_or_else(PoisonError::into_inner);
            // pause() and drop notify, so a request is seen at once; the
            // timeout only paces retries while the main thread catches up.
            if !g.pause && !g.stop {
                drop(cv.wait_timeout(g, SEND_RETRY));
            }
        }
        if !wait_while_paused(gate) {
            return false;
        }
    }
}

/// Crossterm event -> UI actions. Pure, so the key map is testable.
///
/// crossterm reads Esc and the next byte that arrives in the same `read()`
/// as Alt+key (crossterm-rs/crossterm#993). Relic binds no Alt chords, so
/// Alt+key is split back into the Esc and the key that were typed.
pub fn translate(ev: Event, now: Instant) -> Vec<UiAction> {
    match ev {
        Event::Key(k)
            if k.kind != KeyEventKind::Release && k.modifiers.contains(KeyModifiers::ALT) =>
        {
            let key = KeyEvent {
                modifiers: k.modifiers - KeyModifiers::ALT,
                ..k
            };
            let mut actions = vec![UiAction::Key(Key::Esc)];
            actions.extend(translate_one(Event::Key(key), now));
            actions
        }
        ev => translate_one(ev, now).into_iter().collect(),
    }
}

fn translate_one(ev: Event, now: Instant) -> Option<UiAction> {
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
            let key = match code {
                KeyCode::Char('c') if ctrl => return Some(UiAction::Interrupt(now)),
                // ^H is the terminal's other Backspace (byte 0x08).
                KeyCode::Char('h' | 'H') if ctrl => Key::Backspace,
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
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{self, TryRecvError};

    #[test]
    fn pause_and_stop_return_while_the_ui_channel_is_full() {
        const CAP: usize = 4;
        let (tx, rx) = mpsc::sync_channel(CAP);
        let reads = Arc::new(AtomicUsize::new(0));
        let n = Arc::clone(&reads);
        // A terminal that always has another key ready: the reader fills
        // the channel and then holds one more key it cannot deliver.
        let reader = InputReader::spawn_with(tx, move || {
            n.fetch_add(1, Ordering::SeqCst);
            Ok(Some(key(KeyCode::Char('x'), KeyModifiers::NONE)))
        });
        let stalled = |reads: &AtomicUsize| {
            // Reads stop once the reader holds a key it cannot deliver.
            let mut last = usize::MAX;
            while reads.load(Ordering::SeqCst) != last {
                last = reads.load(Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        stalled(&reads);
        assert_eq!(reads.load(Ordering::SeqCst), CAP + 1);

        let reader = crate::ui::within("pause", move || {
            reader.pause();
            reader
        });
        let read_when_parked = reads.load(Ordering::SeqCst);
        for _ in 0..CAP {
            rx.try_recv().unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Empty)),
            "a parked reader must not deliver"
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            read_when_parked,
            "a parked reader must not read the terminal"
        );

        reader.resume();
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the held key is delivered after resume"
        );
        stalled(&reads);
        // The channel is full again and nobody drains it while the reader
        // is dropped, as on exit.
        crate::ui::within("drop", move || drop(reader));
        drop(rx);
    }

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
            (key(KeyCode::Char('h'), ctrl), Some(Key::Backspace)),
            (key(KeyCode::Backspace, none), Some(Key::Backspace)),
            (
                key(KeyCode::Char('F'), ctrl | KeyModifiers::SHIFT),
                Some(Key::Ctrl('f')),
            ),
            (
                key(KeyCode::BackTab, KeyModifiers::SHIFT),
                Some(Key::BackTab),
            ),
            (key(KeyCode::F(5), none), None),
        ];
        let keys = |ev: Event| -> Vec<Key> {
            translate(ev.clone(), now)
                .into_iter()
                .map(|a| match a {
                    UiAction::Key(k) => k,
                    other => panic!("{ev:?} -> {other:?}"),
                })
                .collect()
        };
        for (ev, want) in cases {
            assert_eq!(
                keys(ev.clone()),
                want.iter().cloned().collect::<Vec<_>>(),
                "{ev:?}"
            );
        }
        // Esc then a letter in one read arrives as Alt+letter: both keys,
        // in order, and never the letter alone.
        let alt = KeyModifiers::ALT;
        assert_eq!(
            keys(key(KeyCode::Char('j'), alt)),
            [Key::Esc, Key::Char('j')]
        );
        assert_eq!(
            keys(key(KeyCode::Char('n'), alt | ctrl)),
            [Key::Esc, Key::Ctrl('n')]
        );
        assert_eq!(keys(key(KeyCode::F(5), alt)), [Key::Esc]);
        assert!(matches!(
            &translate(key(KeyCode::Char('c'), ctrl), now)[..],
            [UiAction::Interrupt(t)] if *t == now
        ));
        let release = Event::Key(KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: none,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        });
        assert!(translate(release, now).is_empty());
        assert!(
            matches!(&translate(Event::Paste("a\nb".into()), now)[..], [UiAction::Paste(s)] if s == "a\nb")
        );
        assert!(matches!(
            &translate(Event::Resize(80, 24), now)[..],
            [UiAction::Resize]
        ));
    }
}
