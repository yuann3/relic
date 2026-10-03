//! Ratatui workspace. Compiled only with the `tui` feature; nothing outside
//! this module may import ratatui or crossterm.
//!
//! Threads (docs/research/tui-fzf-integration.md, "Thread shape"): the main
//! thread owns the terminal, the model and the effect runner; one input
//! thread reads the terminal; one search worker owns its own `Store`; a
//! recall runs on its own worker. Everything talks over bounded channels.

mod fake_recall;
mod model;
mod term;
mod view;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter, Stdout, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::{self, MatchMode, SearchRequest};
use crate::picker::{self, PickError};
use crate::store::Store;
use model::{PAGE_LIMIT, PickOutcome, UiAction, UiEffect, UiModel, update};
use term::{InputReader, TerminalGuard};

/// Quiet period before a keystroke becomes a query (prototype variable; the
/// research range is 50-100 ms).
const DEBOUNCE: Duration = Duration::from_millis(75);
/// Redraw rate while an operation runs, so the wall-clock meter moves.
/// Idle draws only on input.
const PROGRESS_TICK: Duration = Duration::from_millis(250);
/// Commands shown on each side of the opened one.
const TIMELINE_CONTEXT: u32 = 8;
const ACTION_QUEUE: usize = 256;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type Term = Terminal<CrosstermBackend<Stdout>>;

pub fn run(db: &Path) -> Result<()> {
    let started = Instant::now();
    // Evidence loads are single indexed lookups, cheap enough for the main
    // thread; searches get their own connection and thread.
    let store = Store::open(db)?;
    let mono = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let (tx, rx) = mpsc::sync_channel(ACTION_QUEUE);
    let mailbox = Arc::new(Mailbox::default());
    let worker = {
        let (db, mb, tx) = (db.to_path_buf(), Arc::clone(&mailbox), tx.clone());
        std::thread::Builder::new()
            .name("relic-search".into())
            .spawn(move || search_worker(&db, &mb, &tx, debounce()))?
    };

    let mut guard = TerminalGuard::enter()?;
    let outcome = (|| {
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        terminal.clear()?;
        let input = InputReader::spawn(tx.clone());
        let mut rt = Runtime {
            store,
            mailbox: Arc::clone(&mailbox),
            tx,
            input,
            terminal,
            recall: None,
            trace: Trace::from_env(),
            issued: None,
            awaiting_frame: None,
            started,
        };
        let r = rt.event_loop(&mut guard, &rx, UiModel::new(mono));
        if let Some((_, cancel)) = rt.recall.take() {
            cancel.store(true, Ordering::SeqCst);
        }
        // Dropping the runtime joins the input reader before the terminal
        // goes back to canonical mode.
        drop(rt);
        r
    })();
    mailbox.close();
    let restored = guard.restore();
    let _ = worker.join();
    outcome?;
    restored?;
    Ok(())
}

struct Runtime {
    store: Store,
    mailbox: Arc<Mailbox>,
    tx: SyncSender<UiAction>,
    input: InputReader,
    terminal: Term,
    recall: Option<(u64, Arc<AtomicBool>)>,
    trace: Trace,
    /// Newest search sent to the worker and when, for key-to-frame timing.
    issued: Option<(u64, Instant)>,
    /// A search page accepted but not yet drawn.
    awaiting_frame: Option<(u64, Instant, Duration)>,
    started: Instant,
}

impl Runtime {
    fn event_loop(
        &mut self,
        guard: &mut TerminalGuard,
        rx: &Receiver<UiAction>,
        mut m: UiModel,
    ) -> Result<()> {
        let mut queue = VecDeque::new();
        let mut next_tick = Instant::now() + PROGRESS_TICK;
        let mut first_frame = true;
        loop {
            if m.dirty {
                let t = Instant::now();
                self.terminal.draw(|f| view::view(f, &m))?;
                m.dirty = false;
                let drawn = Instant::now();
                self.trace
                    .line(format_args!("draw_ms={:.3}", ms(drawn - t)));
                if first_frame {
                    first_frame = false;
                    self.trace.line(format_args!(
                        "first_frame_ms={:.3}",
                        ms(drawn - self.started)
                    ));
                    term::fault_point("frame");
                }
                if let Some((g, issued, worker)) = self.awaiting_frame.take() {
                    self.trace.line(format_args!(
                        "search gen={g} worker_ms={:.3} key_to_frame_ms={:.3}",
                        ms(worker),
                        ms(drawn - issued)
                    ));
                }
            }
            if term::panicked() {
                return Err("a relic thread panicked; see the message above".into());
            }

            if queue.is_empty() {
                let action = if m.recall.running() {
                    let now = Instant::now();
                    if now >= next_tick {
                        next_tick = now + PROGRESS_TICK;
                        UiAction::Tick(now)
                    } else {
                        match rx.recv_timeout(next_tick - now) {
                            Ok(a) => a,
                            Err(RecvTimeoutError::Timeout) => continue,
                            Err(RecvTimeoutError::Disconnected) => return Ok(()),
                        }
                    }
                } else {
                    // Idle: block until something happens. No frame timer.
                    match rx.recv() {
                        Ok(a) => a,
                        Err(_) => return Ok(()),
                    }
                };
                queue.push_back(action);
                // Coalesce bursts (paste, key repeat) into one frame.
                while queue.len() < 64 {
                    match rx.try_recv() {
                        Ok(a) => queue.push_back(a),
                        Err(_) => break,
                    }
                }
            }

            while let Some(action) = queue.pop_front() {
                if let UiAction::SearchDone {
                    generation,
                    elapsed,
                    ..
                } = &action
                    && let Some((g, issued)) = self.issued
                    && g == *generation
                {
                    self.awaiting_frame = Some((g, issued, *elapsed));
                }
                for effect in update(&mut m, action) {
                    match effect {
                        UiEffect::Exit => return Ok(()),
                        e => queue.extend(self.run_effect(e, guard)?),
                    }
                }
            }
        }
    }

    /// Performs one effect. Synchronous results come back as actions; the
    /// rest arrive later on the channel.
    fn run_effect(&mut self, effect: UiEffect, guard: &mut TerminalGuard) -> Result<Vec<UiAction>> {
        Ok(match effect {
            UiEffect::Search { generation, query } => {
                let issued = Instant::now();
                self.issued = Some((generation, issued));
                self.mailbox.put(SearchJob {
                    generation,
                    query,
                    issued,
                });
                vec![]
            }
            UiEffect::OpenEvidence { id } => {
                let t = Instant::now();
                let result = app::open_evidence(&self.store, id, TIMELINE_CONTEXT)
                    .map_err(|e| e.to_string());
                self.trace
                    .line(format_args!("evidence_ms={:.3}", ms(t.elapsed())));
                vec![UiAction::EvidenceLoaded { id, result }]
            }
            UiEffect::OpenPicker(page) => {
                // Never two terminal readers: park ours before fzf starts.
                self.input.pause();
                guard.restore()?;
                let picked = picker::pick(&page);
                guard.resume()?;
                // The size may have changed while fzf ran; autoresize picks
                // it up, and clear forces every cell to be redrawn.
                self.terminal.autoresize()?;
                self.terminal.clear()?;
                self.input.resume();
                vec![UiAction::PickerReturned(pick_outcome(picked))]
            }
            UiEffect::StartRecall { op, query, cite } => {
                if let Some((_, old)) = self.recall.take() {
                    old.store(true, Ordering::SeqCst);
                }
                let cancel = Arc::new(AtomicBool::new(false));
                fake_recall::spawn(op, query, cite, Arc::clone(&cancel), self.tx.clone())?;
                self.recall = Some((op, cancel));
                vec![]
            }
            UiEffect::CancelRecall { op } => {
                if let Some((running, cancel)) = &self.recall
                    && *running == op
                {
                    cancel.store(true, Ordering::SeqCst);
                    self.recall = None;
                }
                vec![]
            }
            UiEffect::Exit => unreachable!("handled by the loop"),
        })
    }
}

fn pick_outcome(r: std::result::Result<Option<i64>, PickError>) -> PickOutcome {
    match r {
        Ok(Some(id)) => PickOutcome::Picked(id),
        Ok(None) => PickOutcome::Cancelled,
        Err(e @ PickError::Unavailable) => PickOutcome::Failed(e.to_string()),
        Err(e @ PickError::TooOld { .. }) => {
            PickOutcome::Failed(format!("{e}; using indexed search"))
        }
        Err(e @ (PickError::Io(_) | PickError::Protocol(_))) => PickOutcome::Failed(e.to_string()),
    }
}

struct SearchJob {
    generation: u64,
    query: String,
    issued: Instant,
}

/// A one-slot, latest-wins request box: a newer query replaces an unsent
/// older one, so the queue between UI and worker can never grow.
#[derive(Default)]
struct Mailbox {
    slot: Mutex<(Option<SearchJob>, bool)>,
    cv: Condvar,
}

impl Mailbox {
    fn put(&self, job: SearchJob) {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).0 = Some(job);
        self.cv.notify_one();
    }

    fn close(&self) {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
        self.cv.notify_one();
    }

    /// Next job, waiting at most `timeout` (forever if `None`). `None` on
    /// timeout or close.
    fn take(&self, timeout: Option<Duration>) -> Option<SearchJob> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut g = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if g.1 {
                return None;
            }
            if let Some(job) = g.0.take() {
                return Some(job);
            }
            g = match deadline {
                None => self.cv.wait(g).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    self.cv
                        .wait_timeout(g, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
            };
        }
    }

    fn closed(&self) -> bool {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).1
    }
}

/// `RELIC_UI_DEBOUNCE_MS` overrides the debounce so the prototype can
/// measure it; #11 picks the value and drops the knob.
fn debounce() -> Duration {
    std::env::var("RELIC_UI_DEBOUNCE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(DEBOUNCE, Duration::from_millis)
}

fn search_worker(db: &Path, mailbox: &Mailbox, tx: &SyncSender<UiAction>, debounce: Duration) {
    let store = Store::open(db).map_err(|e| e.to_string());
    while let Some(mut job) = mailbox.take(None) {
        // The quiet period runs from the keystroke, not from when this
        // worker got free: time a job spent queued behind a running search
        // already counts. Restarting the timer here measured ~130 ms p50
        // key-to-frame for 18 ms queries on the 1M fixture.
        loop {
            let left = (job.issued + debounce).saturating_duration_since(Instant::now());
            match mailbox.take(Some(left)) {
                Some(newer) => job = newer,
                None => break,
            }
        }
        if mailbox.closed() {
            return;
        }
        let req = SearchRequest {
            query: job.query,
            limit: PAGE_LIMIT,
            mode: MatchMode::PrefixLast,
        };
        let t = Instant::now();
        let result = match &store {
            Ok(s) => app::search(s, &req).map_err(|e| e.to_string()),
            Err(e) => Err(format!("cannot open the database: {e}")),
        };
        let elapsed = t.elapsed();
        if tx
            .send(UiAction::SearchDone {
                generation: job.generation,
                result,
                elapsed,
            })
            .is_err()
        {
            return;
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// `RELIC_UI_TRACE=<file>` appends timing lines (no query text) so the
/// prototype's latency can be measured from a real terminal session.
struct Trace(Option<BufWriter<File>>);

impl Trace {
    fn from_env() -> Self {
        let file = std::env::var_os("RELIC_UI_TRACE")
            .and_then(|p| File::options().create(true).append(true).open(p).ok());
        Self(file.map(BufWriter::new))
    }

    fn line(&mut self, args: std::fmt::Arguments) {
        if let Some(w) = &mut self.0 {
            let _ = writeln!(w, "{args}");
            let _ = w.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(generation: u64, query: &str, issued: Instant) -> SearchJob {
        SearchJob {
            generation,
            query: query.into(),
            issued,
        }
    }

    #[test]
    fn mailbox_keeps_only_the_newest_job() {
        let mb = Mailbox::default();
        for g in 1..=5 {
            mb.put(job(g, &format!("q{g}"), Instant::now()));
        }
        let job = mb.take(Some(Duration::ZERO)).unwrap();
        assert_eq!((job.generation, job.query.as_str()), (5, "q5"));
        assert!(mb.take(Some(Duration::from_millis(5))).is_none());
        mb.close();
        assert!(
            mb.take(None).is_none(),
            "close wakes and ends a blocked take"
        );
    }

    /// A worker over a real two-command database.
    fn worker(
        debounce: Duration,
    ) -> (
        Arc<Mailbox>,
        Receiver<UiAction>,
        std::thread::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        let hist = dir.path().join("h");
        std::fs::write(
            &hist,
            ": 1700000000:0;git status\n: 1700000001:0;cargo build\n",
        )
        .unwrap();
        let mut s = Store::open(&db).unwrap();
        let redactor = crate::redact::Redactor::new();
        crate::import::import_file(&mut s, &hist, crate::import::SourceKind::Zsh, &redactor)
            .unwrap();
        let mb = Arc::new(Mailbox::default());
        let (tx, rx) = mpsc::sync_channel(8);
        let h = {
            let mb = Arc::clone(&mb);
            std::thread::spawn(move || search_worker(&db, &mb, &tx, debounce))
        };
        (mb, rx, h, dir)
    }

    fn done(rx: &Receiver<UiAction>) -> (u64, SearchPage) {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            UiAction::SearchDone {
                generation, result, ..
            } => (generation, result.unwrap()),
            other => panic!("{other:?}"),
        }
    }

    use crate::app::SearchPage;

    #[test]
    fn worker_debounces_bursts_and_answers_the_latest_generation() {
        let (mb, rx, h, _dir) = worker(DEBOUNCE);
        // Typed faster than the debounce: only the last one runs.
        for (g, q) in [(1, "g"), (2, "gi"), (3, "git"), (4, "c"), (5, "car")] {
            mb.put(job(g, q, Instant::now()));
            std::thread::sleep(Duration::from_millis(10));
        }
        let (generation, page) = done(&rx);
        assert_eq!(generation, 5);
        assert_eq!(page.hits[0].text, "cargo build");
        assert!(
            rx.recv_timeout(DEBOUNCE * 3).is_err(),
            "superseded generations must not run"
        );
        mb.close();
        h.join().unwrap();
    }

    #[test]
    fn time_spent_queued_counts_toward_the_debounce() {
        let debounce = Duration::from_millis(300);
        let (mb, rx, h, _dir) = worker(debounce);
        // Pressed 400 ms ago (it waited behind a slow search): run now, do
        // not start a fresh quiet period.
        let t = Instant::now();
        mb.put(job(1, "git", t - Duration::from_millis(400)));
        assert_eq!(done(&rx).0, 1);
        assert!(
            t.elapsed() < debounce / 2,
            "re-debounced: {:?}",
            t.elapsed()
        );
        mb.close();
        h.join().unwrap();
    }

    #[test]
    fn every_picker_error_becomes_a_status_word_not_a_crash() {
        let cases = [
            (
                PickError::Unavailable,
                "fzf unavailable; using indexed search",
            ),
            (
                PickError::TooOld {
                    found: "0.20".into(),
                    minimum: "0.44",
                },
                "older than the supported 0.44",
            ),
            (PickError::Io(io::Error::other("boom")), "fzf failed: boom"),
            (
                PickError::Protocol("two records".into()),
                "invalid selection: two records",
            ),
        ];
        for (err, want) in cases {
            let PickOutcome::Failed(msg) = pick_outcome(Err(err)) else {
                panic!()
            };
            assert!(msg.contains(want), "{msg}");
        }
        assert_eq!(pick_outcome(Ok(None)), PickOutcome::Cancelled);
        assert_eq!(pick_outcome(Ok(Some(9))), PickOutcome::Picked(9));
    }
}
