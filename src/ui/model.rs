//! Workspace state and the pure `update` step. Nothing here does I/O: every
//! side effect leaves as a [`UiEffect`] and every result comes back as a
//! [`UiAction`], so the whole interaction model is testable without a
//! terminal, a database, or threads.

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::app::{EvidenceView, RecallReport};
use crate::search::{SearchError, SearchPage};

/// Rows asked of the index per keystroke. The picker and the list both work
/// on this page; broad queries say "narrow it" instead of growing it.
pub const PAGE_LIMIT: usize = 200;
/// Bounds the query so a huge paste cannot become a huge FTS expression.
pub const QUERY_MAX_CHARS: usize = 256;
pub const DOUBLE_INTERRUPT: Duration = Duration::from_secs(1);
const INTERRUPT_HINT: &str = "press Ctrl-C again within 1 s to quit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Query,
    Results,
    Timeline,
    Evidence,
    Recall,
}

impl Focus {
    const ORDER: [Focus; 5] = [
        Focus::Query,
        Focus::Results,
        Focus::Timeline,
        Focus::Evidence,
        Focus::Recall,
    ];

    fn step(self, forward: bool) -> Focus {
        let i = Self::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        let n = Self::ORDER.len();
        Self::ORDER[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }
}

/// Keys after the terminal adapter has stripped crossterm's types away.
/// Ctrl-C is not a key: it arrives as [`UiAction::Interrupt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Up,
    Down,
    PageUp,
    PageDown,
    Enter,
    Esc,
    Backspace,
    Tab,
    BackTab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modal {
    Help,
    ConfirmRecall,
}

#[derive(Debug)]
pub enum UiAction {
    /// Sent once before the first frame: show the newest commands.
    Start,
    Key(Key),
    /// Ctrl-C, stamped by the input thread so double-press timing stays pure.
    Interrupt(Instant),
    Paste(String),
    /// The view reads the size from the frame; this only forces a redraw.
    Resize,
    /// Low-rate deadline while an operation runs; never sent when idle.
    Tick(Instant),
    SearchDone {
        generation: u64,
        result: Result<SearchPage, SearchError>,
        elapsed: Duration,
    },
    EvidenceLoaded {
        id: i64,
        result: Result<Option<EvidenceView>, String>,
    },
    PickerReturned(PickOutcome),
    Recall {
        op: u64,
        event: RecallEvent,
    },
    InputClosed,
    /// Ctrl-Z, but SIGTSTP was ignored when Relic started.
    SuspendRefused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickOutcome {
    Picked(i64),
    Cancelled,
    Failed(String),
}

/// Typed progress from a recall operation. The model boundary is
/// non-streaming, so progress is steps, counters and budget, never tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecallEvent {
    Started {
        at: Instant,
    },
    StepStarted(String),
    /// Ends the newest running step; an empty outcome shows none.
    StepDone(String),
    Calls {
        root: u32,
        child: u32,
    },
    Finished(Box<RecallReport>),
}

/// Everything `update` may ask the runtime to do. There is deliberately no
/// variant that runs a command: the workspace never executes history.
#[derive(Debug, Clone)]
pub enum UiEffect {
    /// The worker debounces and keeps only the newest generation.
    Search {
        generation: u64,
        query: String,
    },
    OpenEvidence {
        id: i64,
    },
    /// fzf over exactly this immutable page.
    OpenPicker(SearchPage),
    StartRecall {
        op: u64,
        question: String,
    },
    CancelRecall {
        op: u64,
    },
    /// Ctrl-Z: give the terminal back and stop until the shell's `fg`.
    Suspend,
    Exit,
}

#[derive(Debug, Default)]
pub struct SearchState {
    /// Newest generation asked for. Results from any other are stale.
    pub generation: u64,
    pub page: Option<SearchPage>,
    pub selected: usize,
    pub pending: bool,
    /// Query time measured inside the worker, around `search::search` only.
    pub latency: Option<Duration>,
    pub error: Option<String>,
    /// The newest query ran out of its budget, and `page` is still the one
    /// for an earlier query (ADR 0007).
    pub too_broad: bool,
    /// First list row on screen, kept by the view between frames so the
    /// highlight moves inside the pane and the list scrolls only at its
    /// edges. Back to the top for each new page.
    pub list_offset: Cell<usize>,
}

impl SearchState {
    pub fn selected_hit(&self) -> Option<&crate::search::PageHit> {
        self.page.as_ref()?.hits.get(self.selected)
    }

    fn hit_count(&self) -> usize {
        self.page.as_ref().map_or(0, |p| p.hits.len())
    }
}

#[derive(Debug, Default)]
pub struct EvidenceState {
    /// Only a load for this id is accepted; earlier ones are stale.
    pub wanted: Option<i64>,
    pub view: Option<EvidenceView>,
    /// Row of `view.timeline` the detail pane shows.
    pub cursor: usize,
    pub scroll: u16,
    /// Furthest useful scroll, recorded by the view on each draw. Only the
    /// renderer knows how the pane's width wraps the text, so it reports
    /// back rather than `update` estimating and stopping short of the end.
    pub scroll_max: Cell<u16>,
    pub error: Option<String>,
    /// Like [`SearchState::list_offset`], for the timeline.
    pub list_offset: Cell<usize>,
}

impl EvidenceState {
    pub fn shown(&self) -> Option<&crate::domain::StoredObservation> {
        let view = self.view.as_ref()?;
        view.timeline.get(self.cursor).or(Some(&view.observation))
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Running,
    Done,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub label: String,
    pub outcome: Option<String>,
    pub state: StepState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecallStatus {
    Idle,
    Running,
    Cancelled,
    Finished,
}

#[derive(Debug)]
pub struct RecallState {
    next_op: u64,
    /// The operation whose events are accepted. Cleared on cancel/finish,
    /// which fences every late event from that operation.
    pub active: Option<u64>,
    pub status: RecallStatus,
    pub steps: Vec<Step>,
    pub root_calls: u32,
    pub child_calls: u32,
    pub started_at: Option<Instant>,
    pub now: Option<Instant>,
    pub report: Option<RecallReport>,
    /// `None` follows the newest line, so progress and then the report stay
    /// in view; scrolling up pins a row until it is back at the end.
    pub scroll: Option<u16>,
    /// Like [`EvidenceState::scroll_max`]: only the view knows the wrap.
    pub scroll_max: Cell<u16>,
}

impl RecallState {
    pub fn running(&self) -> bool {
        self.active.is_some()
    }

    pub fn elapsed(&self) -> Duration {
        match (self.started_at, self.now) {
            (Some(s), Some(n)) => n.saturating_duration_since(s),
            _ => Duration::ZERO,
        }
    }
}

impl Default for RecallState {
    fn default() -> Self {
        Self {
            next_op: 1,
            active: None,
            status: RecallStatus::Idle,
            steps: Vec::new(),
            root_calls: 0,
            child_calls: 0,
            started_at: None,
            now: None,
            report: None,
            scroll: None,
            scroll_max: Cell::new(0),
        }
    }
}

#[derive(Debug)]
pub struct UiModel {
    pub focus: Focus,
    pub query: String,
    pub search: SearchState,
    pub evidence: EvidenceState,
    pub recall: RecallState,
    /// Where a recall's requests go, or why there is no model to ask.
    pub recall_model: Result<String, String>,
    /// The model has prices, so the cost cap holds. Confirming a recall
    /// without them is the opt-in to an unknown cost.
    pub recall_priced: bool,
    pub modal: Option<Modal>,
    /// One transient line for the status bar: picker errors, hints.
    pub notice: Option<String>,
    /// `NO_COLOR`: styles use modifiers only.
    pub mono: bool,
    /// The locale is not UTF-8: draw in ASCII.
    pub ascii: bool,
    /// Set by the view on each draw. While the workspace is replaced by the
    /// "too small" message, keys would act on panes the user cannot see.
    pub too_small: Cell<bool>,
    pub dirty: bool,
    last_interrupt: Option<Instant>,
}

impl UiModel {
    pub fn new(mono: bool) -> Self {
        Self {
            focus: Focus::Query,
            query: String::new(),
            search: SearchState::default(),
            evidence: EvidenceState::default(),
            recall: RecallState::default(),
            recall_model: Err("no model is configured".into()),
            recall_priced: false,
            modal: None,
            notice: None,
            mono,
            ascii: false,
            too_small: Cell::new(false),
            dirty: true,
            last_interrupt: None,
        }
    }

    /// When the double-Ctrl-C window closes, so the runtime wakes to take
    /// its notice down.
    pub fn interrupt_expires(&self) -> Option<Instant> {
        self.last_interrupt.map(|t| t + DOUBLE_INTERRUPT)
    }
}

pub fn update(m: &mut UiModel, action: UiAction) -> Vec<UiEffect> {
    match action {
        UiAction::Start => query_changed(m),
        UiAction::Key(key) => {
            m.dirty = true;
            if m.too_small.get() && key != Key::Ctrl('z') {
                return vec![];
            }
            on_key(m, key)
        }
        UiAction::Interrupt(at) => {
            m.dirty = true;
            on_interrupt(m, at)
        }
        UiAction::Paste(text) => {
            m.dirty = true;
            if m.modal.is_some() || m.too_small.get() {
                return vec![];
            }
            m.focus = Focus::Query;
            let room = QUERY_MAX_CHARS.saturating_sub(m.query.chars().count());
            m.query.extend(
                text.chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .take(room),
            );
            query_changed(m)
        }
        UiAction::Resize => {
            m.dirty = true;
            vec![]
        }
        UiAction::Tick(now) => {
            if m.recall.running() {
                m.recall.now = Some(now);
                m.dirty = true;
            }
            // The double-Ctrl-C window closed: its notice would now lie.
            if m.interrupt_expires().is_some_and(|end| now >= end) {
                m.last_interrupt = None;
                if m.notice
                    .as_deref()
                    .is_some_and(|n| n.ends_with(INTERRUPT_HINT))
                {
                    m.notice = None;
                    m.dirty = true;
                }
            }
            vec![]
        }
        UiAction::SearchDone {
            generation,
            result,
            elapsed,
        } => {
            if generation != m.search.generation {
                return vec![];
            }
            m.dirty = true;
            m.search.pending = false;
            m.search.latency = Some(elapsed);
            m.search.too_broad = false;
            match result {
                // Typing through a hot prefix must not blank the pane: the
                // page stays, and the view says whose page it is.
                Err(SearchError::TooBroad) if m.search.page.is_some() => {
                    m.search.too_broad = true;
                    vec![]
                }
                Ok(page) => {
                    m.search.error = None;
                    m.search.selected = 0;
                    m.search.list_offset.set(0);
                    let first = page.hits.first().map(|h| h.id);
                    m.search.page = Some(page);
                    // Evidence the user opened while this search ran stays put.
                    if matches!(m.focus, Focus::Evidence | Focus::Timeline) {
                        return vec![];
                    }
                    match first {
                        Some(id) => want_evidence(m, id),
                        None => {
                            m.evidence.clear();
                            vec![]
                        }
                    }
                }
                Err(e) => {
                    // The old page belongs to a different query; showing it
                    // under "search: failed" would pass it off as this one's.
                    m.search.error = Some(e.to_string());
                    m.search.page = None;
                    m.search.selected = 0;
                    m.evidence.clear();
                    vec![]
                }
            }
        }
        UiAction::EvidenceLoaded { id, result } => {
            if m.evidence.wanted != Some(id) {
                return vec![];
            }
            m.dirty = true;
            match result {
                Ok(Some(view)) => {
                    m.evidence.cursor = view.timeline.iter().position(|o| o.id == id).unwrap_or(0);
                    m.evidence.list_offset.set(0);
                    m.evidence.view = Some(view);
                    m.evidence.error = None;
                }
                Ok(None) => {
                    m.evidence.view = None;
                    m.evidence.error = Some(format!("evidence #{id} is no longer in the store"));
                }
                Err(e) => {
                    m.evidence.view = None;
                    m.evidence.error = Some(e);
                }
            }
            m.evidence.scroll = 0;
            vec![]
        }
        UiAction::PickerReturned(outcome) => {
            m.dirty = true;
            match outcome {
                PickOutcome::Picked(id) => {
                    let pos = m
                        .search
                        .page
                        .as_ref()
                        .and_then(|p| p.hits.iter().position(|h| h.id == id));
                    let Some(pos) = pos else {
                        m.notice =
                            Some("fzf returned a result outside the current page; ignored".into());
                        return vec![];
                    };
                    m.search.selected = pos;
                    m.focus = Focus::Evidence;
                    m.notice = None;
                    want_evidence(m, id)
                }
                PickOutcome::Cancelled => {
                    m.notice = Some("fzf cancelled".into());
                    vec![]
                }
                PickOutcome::Failed(msg) => {
                    m.notice = Some(msg);
                    vec![]
                }
            }
        }
        UiAction::Recall { op, event } => on_recall_event(m, op, event),
        UiAction::InputClosed => vec![UiEffect::Exit],
        UiAction::SuspendRefused => {
            m.dirty = true;
            m.notice = Some("not suspending: Ctrl-Z was ignored when relic started".into());
            vec![]
        }
    }
}

fn on_key(m: &mut UiModel, key: Key) -> Vec<UiEffect> {
    // Raw mode turns off the tty's own Ctrl-Z, so it arrives as a key and
    // must suspend from anywhere, as it would without the TUI.
    if key == Key::Ctrl('z') {
        return vec![UiEffect::Suspend];
    }
    match m.modal {
        Some(Modal::Help) => {
            if matches!(key, Key::Esc | Key::Char('?' | 'q')) {
                m.modal = None;
            }
            return vec![];
        }
        Some(Modal::ConfirmRecall) => {
            // Anything but y is a no: a dialog that ignored other keys stayed
            // open while the user typed on, so the 'y' of "ruby", whose 'r'
            // opened it, gave consent.
            if matches!(key, Key::Char('y' | 'Y')) {
                return start_recall(m);
            }
            m.modal = None;
            m.notice = Some("recall not started".into());
            return vec![];
        }
        None => {}
    }

    let typing = m.focus == Focus::Query;
    match key {
        // While typing, '?' is query text (`$?`, globs); Tab away for help.
        Key::Char('?') if !typing => {
            m.modal = Some(Modal::Help);
            vec![]
        }
        Key::Ctrl('f') => open_picker(m),
        Key::Ctrl('r') => toggle_recall(m),
        Key::Ctrl('u') => {
            m.focus = Focus::Query;
            m.query.clear();
            query_changed(m)
        }
        Key::Ctrl('n') | Key::Down => move_by(m, 1),
        Key::Ctrl('p') | Key::Up => move_by(m, -1),
        Key::PageDown => move_by(m, 10),
        Key::PageUp => move_by(m, -10),
        Key::Tab | Key::BackTab => {
            m.focus = m.focus.step(key == Key::Tab);
            vec![]
        }
        Key::Enter => enter(m),
        Key::Esc => {
            if let Some(op) = m.recall.active {
                cancel_recall(m, op)
            } else if !typing {
                m.focus = Focus::Query;
                vec![]
            } else {
                vec![UiEffect::Exit]
            }
        }
        Key::Char('j') if !typing => move_by(m, 1),
        Key::Char('k') if !typing => move_by(m, -1),
        Key::Char('q') if !typing => vec![UiEffect::Exit],
        Key::Char('f') if !typing => open_picker(m),
        Key::Char('r') if !typing => toggle_recall(m),
        Key::Char('/') if !typing => {
            m.focus = Focus::Query;
            vec![]
        }
        // Any other printable key goes to the query, wherever focus was.
        Key::Char(c) if !c.is_control() => {
            m.focus = Focus::Query;
            if m.query.chars().count() >= QUERY_MAX_CHARS {
                return vec![];
            }
            m.query.push(c);
            query_changed(m)
        }
        Key::Backspace => {
            m.focus = Focus::Query;
            if m.query.pop().is_none() {
                return vec![];
            }
            query_changed(m)
        }
        Key::Char(_) | Key::Ctrl(_) => vec![],
    }
}

fn on_interrupt(m: &mut UiModel, at: Instant) -> Vec<UiEffect> {
    if m.last_interrupt
        .is_some_and(|prev| at.saturating_duration_since(prev) <= DOUBLE_INTERRUPT)
    {
        return vec![UiEffect::Exit];
    }
    m.last_interrupt = Some(at);
    let hint = INTERRUPT_HINT;
    let effects = if let Some(op) = m.recall.active {
        let e = cancel_recall(m, op);
        m.notice = Some(format!("recall cancelled; {hint}"));
        return e;
    } else if m.modal.take().is_some() {
        vec![]
    } else if !m.query.is_empty() {
        m.query.clear();
        m.focus = Focus::Query;
        query_changed(m)
    } else {
        vec![]
    };
    m.notice = Some(hint.into());
    effects
}

/// Every query edit gets a new generation, even ones that do not search, so
/// an in-flight result for the old text can never land on the new text.
fn query_changed(m: &mut UiModel) -> Vec<UiEffect> {
    m.search.generation += 1;
    m.search.error = None;
    m.notice = None;
    // An empty query is still a search: it pages the newest commands.
    m.search.pending = true;
    vec![UiEffect::Search {
        generation: m.search.generation,
        query: m.query.clone(),
    }]
}

fn want_evidence(m: &mut UiModel, id: i64) -> Vec<UiEffect> {
    m.evidence.wanted = Some(id);
    vec![UiEffect::OpenEvidence { id }]
}

fn move_by(m: &mut UiModel, delta: isize) -> Vec<UiEffect> {
    match m.focus {
        Focus::Query | Focus::Results => {
            let n = m.search.hit_count();
            if n == 0 {
                return vec![];
            }
            let next = m.search.selected.saturating_add_signed(delta).min(n - 1);
            if next == m.search.selected {
                return vec![];
            }
            m.search.selected = next;
            let id = m.search.selected_hit().map(|h| h.id);
            id.map_or_else(Vec::new, |id| want_evidence(m, id))
        }
        Focus::Timeline => {
            let n = m.evidence.view.as_ref().map_or(0, |v| v.timeline.len());
            if n > 0 {
                m.evidence.cursor = m.evidence.cursor.saturating_add_signed(delta).min(n - 1);
                m.evidence.scroll = 0;
            }
            vec![]
        }
        Focus::Evidence => {
            m.evidence.scroll = m
                .evidence
                .scroll
                .saturating_add_signed(delta as i16)
                .min(m.evidence.scroll_max.get());
            vec![]
        }
        Focus::Recall => {
            let max = m.recall.scroll_max.get();
            let to = m
                .recall
                .scroll
                .unwrap_or(max)
                .saturating_add_signed(delta as i16)
                .min(max);
            m.recall.scroll = (to < max).then_some(to);
            vec![]
        }
    }
}

fn enter(m: &mut UiModel) -> Vec<UiEffect> {
    let id = match m.focus {
        Focus::Query | Focus::Results => m.search.selected_hit().map(|h| h.id),
        Focus::Timeline => m.evidence.shown().map(|o| o.id),
        Focus::Recall => m
            .recall
            .report
            .as_ref()
            .and_then(|r| r.evidence.first())
            .map(|e| e.id),
        Focus::Evidence => return vec![],
    };
    let Some(id) = id else {
        m.notice = Some("nothing selected to open".into());
        return vec![];
    };
    if let Some(pos) = m
        .search
        .page
        .as_ref()
        .and_then(|p| p.hits.iter().position(|h| h.id == id))
    {
        m.search.selected = pos;
    }
    m.focus = Focus::Evidence;
    want_evidence(m, id)
}

fn open_picker(m: &mut UiModel) -> Vec<UiEffect> {
    // The shown page belongs to an older query until the search lands, and
    // a pick from it would then be overwritten by the new page.
    if m.search.pending {
        m.notice = Some("still searching; press f again when results update".into());
        return vec![];
    }
    match &m.search.page {
        Some(page) if !page.hits.is_empty() => vec![UiEffect::OpenPicker(page.clone())],
        _ => {
            m.notice = Some("nothing to pick from; type a query first".into());
            vec![]
        }
    }
}

fn toggle_recall(m: &mut UiModel) -> Vec<UiEffect> {
    if let Some(op) = m.recall.active {
        return cancel_recall(m, op);
    }
    if let Err(why) = &m.recall_model {
        m.notice = Some(format!("recall needs a model: {why}; see relic --help"));
        return vec![];
    }
    if m.query.trim().is_empty() {
        m.notice = Some("recall answers the query; type a question first".into());
        return vec![];
    }
    m.modal = Some(Modal::ConfirmRecall);
    vec![]
}

fn start_recall(m: &mut UiModel) -> Vec<UiEffect> {
    m.modal = None;
    let op = m.recall.next_op;
    m.recall = RecallState {
        next_op: op + 1,
        active: Some(op),
        status: RecallStatus::Running,
        ..RecallState::default()
    };
    m.focus = Focus::Recall;
    m.notice = None;
    vec![UiEffect::StartRecall {
        op,
        question: m.query.clone(),
    }]
}

fn cancel_recall(m: &mut UiModel, op: u64) -> Vec<UiEffect> {
    m.recall.active = None;
    m.recall.status = RecallStatus::Cancelled;
    for s in &mut m.recall.steps {
        if s.state == StepState::Running {
            s.state = StepState::Cancelled;
        }
    }
    vec![UiEffect::CancelRecall { op }]
}

fn on_recall_event(m: &mut UiModel, op: u64, event: RecallEvent) -> Vec<UiEffect> {
    if m.recall.active != Some(op) {
        return vec![];
    }
    m.dirty = true;
    let r = &mut m.recall;
    match event {
        RecallEvent::Started { at } => {
            r.started_at = Some(at);
            r.now = Some(at);
        }
        RecallEvent::StepStarted(label) => {
            r.steps.push(Step {
                label,
                outcome: None,
                state: StepState::Running,
            });
        }
        RecallEvent::StepDone(outcome) => {
            if let Some(s) = r
                .steps
                .iter_mut()
                .rev()
                .find(|s| s.state == StepState::Running)
            {
                s.state = StepState::Done;
                s.outcome = (!outcome.is_empty()).then_some(outcome);
            }
        }
        RecallEvent::Calls { root, child } => {
            r.root_calls = root;
            r.child_calls = child;
        }
        RecallEvent::Finished(report) => {
            r.active = None;
            r.status = RecallStatus::Finished;
            // The last operation has no event of its own that ends it.
            for s in &mut r.steps {
                if s.state == StepState::Running {
                    s.state = StepState::Done;
                }
            }
            r.report = Some(*report);
        }
    }
    vec![]
}

/// An answered report citing `cites`, for the UI's tests.
#[cfg(test)]
pub(crate) fn answered(cites: &[i64]) -> RecallReport {
    use crate::app::{CitedRecord, CitedText, RECALL_SCHEMA, RecallOutcome};
    RecallReport {
        schema: RECALL_SCHEMA,
        question: "how did I renew".into(),
        outcome: RecallOutcome::Answered,
        error: None,
        answer: "with certbot".into(),
        claims: vec![CitedText {
            text: "certbot renew ran".into(),
            supports: cites.to_vec(),
            contradicts: vec![],
        }],
        procedure: vec![],
        contradictions: vec![],
        missing: vec![],
        evidence: cites
            .iter()
            .map(|&id| CitedRecord {
                id,
                revision: 1,
                ordinal: id,
                source: "zsh",
                observed_at_ms: None,
                text: format!("cmd {id}"),
            })
            .collect(),
        root_calls: 2,
        child_calls: 3,
        cost_microusd: Some(1200),
        models: vec!["m-1".into()],
        skill: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ObservationKind, Provenance, StoredObservation};
    use crate::search::{PageHit, SEARCH_SCHEMA};

    fn hit(id: i64, text: &str) -> PageHit {
        PageHit {
            id,
            observed_at_ms: Some(1_700_000_000_000 + id * 1000),
            observed_before_ms: None,
            source: "zsh",
            text: text.into(),
        }
    }

    pub(crate) fn page(query: &str, ids: &[i64]) -> SearchPage {
        SearchPage {
            schema: SEARCH_SCHEMA,
            query: query.into(),
            limit: PAGE_LIMIT,
            truncated: false,
            deferred: None,
            words_dropped: 0,
            order: crate::search::Order::RecentlyImported,
            filtered: true,
            hits: ids
                .iter()
                .map(|&id| hit(id, &format!("{query} cmd {id}")))
                .collect(),
        }
    }

    fn obs(id: i64, text: &str) -> StoredObservation {
        StoredObservation {
            id,
            revision_id: 1,
            ordinal: id,
            kind: ObservationKind::ZshCommand,
            observed_at_ms: Some(1_700_000_000_000),
            safe_text: text.into(),
            flags: 0,
            locator: vec![],
        }
    }

    fn view(center: i64) -> EvidenceView {
        EvidenceView {
            observation: obs(center, "center"),
            timeline: (center - 1..=center + 1)
                .map(|i| obs(i, &format!("row {i}")))
                .collect(),
            bounds: Default::default(),
            provenance: Provenance::COMMAND.with(Provenance::OBSERVED_AT),
        }
    }

    fn keys(m: &mut UiModel, s: &str) -> Vec<UiEffect> {
        s.chars()
            .flat_map(|c| update(m, UiAction::Key(Key::Char(c))))
            .collect()
    }

    fn done(generation: u64, page: SearchPage) -> UiAction {
        UiAction::SearchDone {
            generation,
            result: Ok(page),
            elapsed: Duration::from_millis(3),
        }
    }

    /// A model with "git" typed and its page of ids 10, 11, 12 accepted.
    fn with_results() -> UiModel {
        let mut m = UiModel::new(false);
        m.recall_model = Ok("http://127.0.0.1:8080 (llama_cpp)".into());
        keys(&mut m, "git");
        let g = m.search.generation;
        update(&mut m, done(g, page("git", &[10, 11, 12])));
        m
    }

    fn searches(effects: &[UiEffect]) -> Vec<(u64, String)> {
        effects
            .iter()
            .filter_map(|e| match e {
                UiEffect::Search { generation, query } => Some((*generation, query.clone())),
                _ => None,
            })
            .collect()
    }

    fn is_exit(effects: &[UiEffect]) -> bool {
        effects.iter().any(|e| matches!(e, UiEffect::Exit))
    }

    #[test]
    fn typing_emits_one_search_per_edit_with_rising_generations() {
        let cases: &[(&str, &[(u64, &str)])] = &[
            ("g", &[(1, "g")]),
            ("gi", &[(1, "g"), (2, "gi")]),
            ("git", &[(1, "g"), (2, "gi"), (3, "git")]),
            // j/k/q/f/r are text while the query bar has focus.
            (
                "jkqfr",
                &[(1, "j"), (2, "jk"), (3, "jkq"), (4, "jkqf"), (5, "jkqfr")],
            ),
        ];
        for (typed, want) in cases {
            let mut m = UiModel::new(false);
            let got = searches(&keys(&mut m, typed));
            let want: Vec<(u64, String)> = want.iter().map(|(g, q)| (*g, q.to_string())).collect();
            assert_eq!(got, want, "typing {typed:?}");
            assert!(m.search.pending);
        }
    }

    #[test]
    fn start_pages_the_newest_commands() {
        let mut m = UiModel::new(false);
        let e = update(&mut m, UiAction::Start);
        assert_eq!(searches(&e), [(1, String::new())]);
        assert!(m.search.pending);
    }

    #[test]
    fn picker_waits_for_the_pending_search() {
        let mut m = with_results();
        keys(&mut m, "x"); // "gitx" is now searching; the "git" page is stale
        assert!(update(&mut m, UiAction::Key(Key::Ctrl('f'))).is_empty());
        assert!(
            m.notice
                .as_deref()
                .unwrap_or_default()
                .contains("still searching")
        );
        let g = m.search.generation;
        update(&mut m, done(g, page("gitx", &[20])));
        let e = update(&mut m, UiAction::Key(Key::Ctrl('f')));
        assert!(matches!(&e[..], [UiEffect::OpenPicker(p)] if p.query == "gitx"));
    }

    #[test]
    fn a_landing_search_keeps_evidence_the_user_opened() {
        let mut m = with_results();
        keys(&mut m, "x");
        // Enter on the stale page while "gitx" is still searching.
        update(&mut m, UiAction::Key(Key::Enter));
        assert_eq!(m.focus, Focus::Evidence);
        let g = m.search.generation;
        let e = update(&mut m, done(g, page("gitx", &[20])));
        assert!(
            !e.iter().any(|e| matches!(e, UiEffect::OpenEvidence { .. })),
            "{e:?}"
        );
        // From the query bar the first hit still opens as before.
        m.focus = Focus::Query;
        keys(&mut m, "y");
        let g = m.search.generation;
        let e = update(&mut m, done(g, page("gitxy", &[30])));
        assert!(
            matches!(&e[..], [UiEffect::OpenEvidence { id: 30 }]),
            "{e:?}"
        );
    }

    #[test]
    fn backspace_and_ctrl_u_edit_the_query() {
        let mut m = UiModel::new(false);
        keys(&mut m, "git");
        let e = update(&mut m, UiAction::Key(Key::Backspace));
        assert_eq!(searches(&e), [(4, "gi".to_string())]);

        // Clearing asks for the newest commands under a new generation, so
        // the in-flight "gi" result cannot land on the empty query.
        let e = update(&mut m, UiAction::Key(Key::Ctrl('u')));
        assert_eq!(searches(&e), [(5, String::new())]);
        assert_eq!(m.query, "");
        update(&mut m, done(4, page("gi", &[1])));
        assert!(m.search.page.is_none());

        // Backspace on an empty query is a no-op, not an empty search.
        assert!(update(&mut m, UiAction::Key(Key::Backspace)).is_empty());
    }

    #[test]
    fn stale_search_results_are_discarded() {
        let mut m = UiModel::new(false);
        keys(&mut m, "car");
        m.dirty = false;
        // Generation 2 ("ca") finishes after "car" (3) was requested.
        let e = update(&mut m, done(2, page("ca", &[1, 2])));
        assert!(e.is_empty());
        assert!(m.search.page.is_none());
        assert!(m.search.pending);
        assert!(!m.dirty, "a discarded result must not cause a redraw");

        let e = update(&mut m, done(3, page("car", &[7])));
        assert_eq!(m.search.page.as_ref().unwrap().query, "car");
        assert!(!m.search.pending);
        assert_eq!(m.search.latency, Some(Duration::from_millis(3)));
        // The first hit's timeline loads right away.
        assert!(matches!(e[..], [UiEffect::OpenEvidence { id: 7 }]));
    }

    #[test]
    fn a_too_broad_search_keeps_the_previous_page_until_one_finishes() {
        let too_broad = |m: &mut UiModel| {
            let generation = m.search.generation;
            let result = Err(SearchError::TooBroad);
            update(
                m,
                UiAction::SearchDone {
                    generation,
                    result,
                    elapsed: Duration::from_millis(100),
                },
            )
        };
        let mut m = with_results();
        let shown = m.search.page.as_ref().unwrap().query.clone();
        m.search.selected = 1;
        keys(&mut m, "x");
        assert!(too_broad(&mut m).is_empty());
        assert_eq!(
            m.search.page.as_ref().map(|p| p.query.as_str()),
            Some(shown.as_str())
        );
        assert!(m.search.too_broad && !m.search.pending && m.search.error.is_none());
        assert_eq!(
            m.search.selected, 1,
            "the selection on the kept page must not move"
        );

        // The next keystroke's page replaces it and the mark goes.
        keys(&mut m, "y");
        let g = m.search.generation;
        update(&mut m, done(g, page("xy", &[3])));
        assert!(!m.search.too_broad);
        assert_eq!(m.search.page.as_ref().unwrap().query, "xy");

        // With no page to keep, it is an error like any other.
        let mut m = UiModel::new(false);
        keys(&mut m, "x");
        too_broad(&mut m);
        assert!(m.search.page.is_none() && !m.search.too_broad);
        assert!(
            m.search
                .error
                .as_deref()
                .is_some_and(|e| e.contains("too broad")),
            "{:?}",
            m.search.error
        );
    }

    #[test]
    fn a_failed_search_drops_the_old_page_and_the_next_edit_clears_the_error() {
        let mut m = with_results();
        update(
            &mut m,
            UiAction::EvidenceLoaded {
                id: 10,
                result: Ok(Some(view(10))),
            },
        );
        keys(&mut m, "x");
        let g = m.search.generation;
        update(
            &mut m,
            UiAction::SearchDone {
                generation: g,
                result: Err(SearchError::Store(rusqlite::Error::InvalidQuery)),
                elapsed: Duration::from_millis(3),
            },
        );
        assert!(
            m.search.page.is_none(),
            "the old query's page must not show under the failed one"
        );
        assert!(m.search.selected_hit().is_none());
        assert!(m.evidence.view.is_none() && m.evidence.wanted.is_none());
        assert_eq!(m.search.error.as_deref(), Some("Query is not read-only"));

        let e = keys(&mut m, "y");
        assert_eq!(searches(&e).len(), 1);
        assert_eq!(m.search.error, None, "a new query is not failed yet");
        assert!(m.search.pending);
    }

    #[test]
    fn evidence_scroll_stops_at_the_end_of_the_content() {
        let mut m = with_results();
        update(
            &mut m,
            UiAction::EvidenceLoaded {
                id: 10,
                result: Ok(Some(view(10))),
            },
        );
        m.focus = Focus::Evidence;
        // The view records the limit when it draws; emulate one draw.
        m.evidence.scroll_max.set(5);
        for _ in 0..1000 {
            update(&mut m, UiAction::Key(Key::Down));
        }
        assert_eq!(m.evidence.scroll, 5);
        update(&mut m, UiAction::Key(Key::Up));
        assert_eq!(m.evidence.scroll, 4, "one key back from the end");
    }

    #[test]
    fn selection_moves_load_the_timeline_of_the_new_hit() {
        let table: &[(Focus, Key, Option<i64>, usize)] = &[
            (Focus::Query, Key::Down, Some(11), 1),
            (Focus::Query, Key::Ctrl('n'), Some(11), 1),
            (Focus::Results, Key::Char('j'), Some(11), 1),
            (Focus::Query, Key::Up, None, 0),
            (Focus::Results, Key::Char('k'), None, 0),
            (Focus::Results, Key::PageDown, Some(12), 2),
        ];
        for &(focus, key, want, selected) in table {
            let mut m = with_results();
            m.focus = focus;
            let e = update(&mut m, UiAction::Key(key));
            let got = e.iter().find_map(|e| match e {
                UiEffect::OpenEvidence { id } => Some(*id),
                _ => None,
            });
            assert_eq!(got, want, "{focus:?} {key:?}");
            assert_eq!(m.search.selected, selected, "{focus:?} {key:?}");
            assert_eq!(m.query, "git", "{key:?} must not edit the query");
        }
    }

    #[test]
    fn enter_opens_evidence_and_focuses_it() {
        let mut m = with_results();
        update(&mut m, UiAction::Key(Key::Down));
        let e = update(&mut m, UiAction::Key(Key::Enter));
        assert!(matches!(e[..], [UiEffect::OpenEvidence { id: 11 }]));
        assert_eq!(m.focus, Focus::Evidence);

        // Only the load for the wanted id is accepted.
        update(
            &mut m,
            UiAction::EvidenceLoaded {
                id: 10,
                result: Ok(Some(view(10))),
            },
        );
        assert!(m.evidence.view.is_none());
        update(
            &mut m,
            UiAction::EvidenceLoaded {
                id: 11,
                result: Ok(Some(view(11))),
            },
        );
        assert_eq!(m.evidence.shown().unwrap().id, 11);
        assert_eq!(
            m.evidence.cursor, 1,
            "cursor starts on the hit, not the first neighbor"
        );
    }

    #[test]
    fn enter_on_a_timeline_row_recenters_on_it() {
        let mut m = with_results();
        update(
            &mut m,
            UiAction::EvidenceLoaded {
                id: 10,
                result: Ok(Some(view(10))),
            },
        );
        m.focus = Focus::Timeline;
        update(&mut m, UiAction::Key(Key::Char('j')));
        assert_eq!(
            m.evidence.shown().unwrap().id,
            11,
            "j moves the timeline cursor"
        );
        assert_eq!(
            m.search.selected, 0,
            "and leaves the result selection alone"
        );
        let e = update(&mut m, UiAction::Key(Key::Enter));
        assert!(matches!(e[..], [UiEffect::OpenEvidence { id: 11 }]));
        assert_eq!(
            m.search.selected, 1,
            "the opened row is also a hit, so it is selected"
        );
    }

    #[test]
    fn picker_needs_a_page_and_hands_over_exactly_that_page() {
        let mut m = UiModel::new(false);
        assert!(update(&mut m, UiAction::Key(Key::Ctrl('f'))).is_empty());
        assert!(m.notice.is_some());

        let mut m = with_results();
        // 'f' is text while typing; Ctrl-F works from the query bar.
        assert!(searches(&update(&mut m, UiAction::Key(Key::Char('f')))).len() == 1);
        let mut m = with_results();
        let e = update(&mut m, UiAction::Key(Key::Ctrl('f')));
        let [UiEffect::OpenPicker(p)] = &e[..] else {
            panic!("{e:?}")
        };
        assert_eq!(
            p.hits.iter().map(|h| h.id).collect::<Vec<_>>(),
            [10, 11, 12]
        );

        m.focus = Focus::Results;
        assert!(matches!(
            update(&mut m, UiAction::Key(Key::Char('f')))[..],
            [UiEffect::OpenPicker(_)]
        ));
    }

    #[test]
    fn picker_outcomes() {
        let mut m = with_results();
        let e = update(&mut m, UiAction::PickerReturned(PickOutcome::Picked(12)));
        assert!(matches!(e[..], [UiEffect::OpenEvidence { id: 12 }]));
        assert_eq!((m.search.selected, m.focus), (2, Focus::Evidence));

        let mut m = with_results();
        let e = update(&mut m, UiAction::PickerReturned(PickOutcome::Picked(99)));
        assert!(e.is_empty(), "an id outside the page is never opened");
        assert_eq!(m.search.selected, 0);

        let mut m = with_results();
        update(&mut m, UiAction::PickerReturned(PickOutcome::Cancelled));
        assert_eq!(m.notice.as_deref(), Some("fzf cancelled"));

        let mut m = with_results();
        let msg = crate::picker::PickError::Unavailable.to_string();
        update(
            &mut m,
            UiAction::PickerReturned(PickOutcome::Failed(msg.clone())),
        );
        assert_eq!(m.notice, Some(msg));
    }

    fn running_recall() -> (UiModel, u64) {
        let mut m = with_results();
        m.focus = Focus::Results;
        update(&mut m, UiAction::Key(Key::Char('r')));
        let e = update(&mut m, UiAction::Key(Key::Char('y')));
        let [UiEffect::StartRecall { op, .. }] = e[..] else {
            panic!("{e:?}")
        };
        (m, op)
    }

    #[test]
    fn recall_requires_confirmation() {
        let mut m = with_results();
        m.focus = Focus::Results;
        let e = update(&mut m, UiAction::Key(Key::Char('r')));
        assert!(e.is_empty());
        assert_eq!(m.modal, Some(Modal::ConfirmRecall));
        // Any key but y declines, and is spent on declining: typing "ruby"
        // in a pane never starts a recall.
        for k in [
            Key::Esc,
            Key::Char('n'),
            Key::Enter,
            Key::Down,
            Key::Char('u'),
        ] {
            if m.modal.is_none() {
                update(&mut m, UiAction::Key(Key::Char('r')));
            }
            assert_eq!(m.modal, Some(Modal::ConfirmRecall), "{k:?}");
            assert!(update(&mut m, UiAction::Key(k)).is_empty(), "{k:?}");
            assert_eq!(m.modal, None, "{k:?}");
            assert_eq!(m.notice.as_deref(), Some("recall not started"), "{k:?}");
        }
        for c in "ruby".chars() {
            m.focus = Focus::Results;
            let e = update(&mut m, UiAction::Key(Key::Char(c)));
            assert!(
                !e.iter().any(|e| matches!(e, UiEffect::StartRecall { .. })),
                "{c}"
            );
        }
        assert!(!m.recall.running());
        m.focus = Focus::Results;

        update(&mut m, UiAction::Key(Key::Ctrl('r')));
        let e = update(&mut m, UiAction::Key(Key::Char('y')));
        let [UiEffect::StartRecall { op, ref question }] = e[..] else {
            panic!("{e:?}")
        };
        // The keys after the "r" were query text: "git" became "gitby".
        assert_eq!((op, question.as_str()), (1, "gitby"));
        assert_eq!(m.recall.status, RecallStatus::Running);
    }

    #[test]
    fn recall_without_a_question_or_a_model_does_not_offer_to_spend() {
        let mut m = with_results();
        update(&mut m, UiAction::Key(Key::Ctrl('u')));
        update(&mut m, UiAction::Key(Key::Ctrl('r')));
        assert_eq!(m.modal, None);
        assert_eq!(
            m.notice.as_deref(),
            Some("recall answers the query; type a question first")
        );

        let mut m = with_results();
        m.recall_model = Err("RELIC_MODEL_NAME is required".into());
        update(&mut m, UiAction::Key(Key::Ctrl('r')));
        assert_eq!(m.modal, None);
        assert_eq!(
            m.notice.as_deref(),
            Some("recall needs a model: RELIC_MODEL_NAME is required; see relic --help")
        );
    }

    #[test]
    fn recall_progress_then_finding() {
        let (mut m, op) = running_recall();
        let t0 = Instant::now();
        for ev in [
            RecallEvent::Started { at: t0 },
            RecallEvent::StepStarted("plan".into()),
            RecallEvent::StepDone("2 searches".into()),
            RecallEvent::Calls { root: 2, child: 3 },
            RecallEvent::StepStarted("run i1".into()),
        ] {
            update(&mut m, UiAction::Recall { op, event: ev });
        }
        update(&mut m, UiAction::Tick(t0 + Duration::from_millis(1500)));
        assert_eq!(m.recall.elapsed(), Duration::from_millis(1500));
        assert_eq!((m.recall.root_calls, m.recall.child_calls), (2, 3));
        assert_eq!(m.recall.steps[0].state, StepState::Done);

        let finished = RecallEvent::Finished(Box::new(answered(&[11, 10])));
        update(
            &mut m,
            UiAction::Recall {
                op,
                event: finished,
            },
        );
        assert_eq!(m.recall.status, RecallStatus::Finished);
        assert_eq!(
            m.recall.steps[1].state,
            StepState::Done,
            "the last operation ends with the recall"
        );
        m.focus = Focus::Recall;
        let e = update(&mut m, UiAction::Key(Key::Enter));
        assert!(
            matches!(e[..], [UiEffect::OpenEvidence { id: 11 }]),
            "the first cited record opens"
        );
    }

    #[test]
    fn cancelled_recall_fences_late_events() {
        for cancel in [
            UiAction::Key(Key::Esc),
            UiAction::Key(Key::Ctrl('r')),
            UiAction::Interrupt(Instant::now()),
        ] {
            let (mut m, op) = running_recall();
            update(
                &mut m,
                UiAction::Recall {
                    op,
                    event: RecallEvent::StepStarted("plan".into()),
                },
            );
            let e = update(&mut m, cancel);
            assert!(matches!(e[..], [UiEffect::CancelRecall { op: o }] if o == op));
            assert_eq!(m.recall.status, RecallStatus::Cancelled);
            assert_eq!(m.recall.steps[0].state, StepState::Cancelled);

            m.dirty = false;
            for late in [
                RecallEvent::StepDone("late".into()),
                RecallEvent::Calls { root: 8, child: 16 },
                RecallEvent::Finished(Box::new(answered(&[10]))),
            ] {
                assert!(update(&mut m, UiAction::Recall { op, event: late }).is_empty());
            }
            assert_eq!(m.recall.status, RecallStatus::Cancelled);
            assert_eq!((m.recall.root_calls, m.recall.report.is_none()), (0, true));
            assert!(!m.dirty);
        }
    }

    #[test]
    fn a_new_recall_fences_the_previous_one() {
        let (mut m, first) = running_recall();
        update(&mut m, UiAction::Key(Key::Esc));
        m.focus = Focus::Results;
        update(&mut m, UiAction::Key(Key::Char('r')));
        let e = update(&mut m, UiAction::Key(Key::Char('y')));
        let [UiEffect::StartRecall { op: second, .. }] = e[..] else {
            panic!()
        };
        assert_ne!(first, second);
        update(
            &mut m,
            UiAction::Recall {
                op: first,
                event: RecallEvent::StepStarted("old".into()),
            },
        );
        assert!(m.recall.steps.is_empty());
    }

    #[test]
    fn esc_unwinds_one_layer_at_a_time() {
        let (mut m, _) = running_recall();
        m.focus = Focus::Evidence;
        // recall -> focus -> quit
        assert!(matches!(
            update(&mut m, UiAction::Key(Key::Esc))[..],
            [UiEffect::CancelRecall { .. }]
        ));
        assert!(update(&mut m, UiAction::Key(Key::Esc)).is_empty());
        assert_eq!(m.focus, Focus::Query);
        assert!(is_exit(&update(&mut m, UiAction::Key(Key::Esc))));

        let mut m = UiModel::new(false);
        // In the query bar '?' is text; off it, '?' opens help.
        update(&mut m, UiAction::Key(Key::Char('?')));
        assert_eq!((m.modal, m.query.as_str()), (None, "?"));
        update(&mut m, UiAction::Key(Key::Tab));
        update(&mut m, UiAction::Key(Key::Char('?')));
        assert_eq!(m.modal, Some(Modal::Help));
        assert!(!is_exit(&update(&mut m, UiAction::Key(Key::Esc))));
        assert_eq!(m.modal, None);
    }

    #[test]
    fn q_quits_only_when_not_typing() {
        let mut m = UiModel::new(false);
        assert!(!is_exit(&update(&mut m, UiAction::Key(Key::Char('q')))));
        assert_eq!(m.query, "q");
        m.focus = Focus::Results;
        assert!(is_exit(&update(&mut m, UiAction::Key(Key::Char('q')))));
    }

    #[test]
    fn ctrl_c_cancels_first_and_quits_on_a_quick_second_press() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;

        let mut m = UiModel::new(false);
        keys(&mut m, "git");
        assert!(!is_exit(&update(&mut m, UiAction::Interrupt(t0))));
        assert_eq!(m.query, "", "first press clears the query");
        assert!(is_exit(&update(&mut m, UiAction::Interrupt(t0 + ms(600)))));

        let mut m = UiModel::new(false);
        assert!(!is_exit(&update(&mut m, UiAction::Interrupt(t0))));
        assert!(
            !is_exit(&update(&mut m, UiAction::Interrupt(t0 + ms(1500)))),
            "too slow: re-arms"
        );
        assert!(is_exit(&update(&mut m, UiAction::Interrupt(t0 + ms(2000)))));
    }

    #[test]
    fn paste_is_bounded_single_line_data() {
        let mut m = UiModel::new(false);
        m.focus = Focus::Results;
        let e = update(&mut m, UiAction::Paste("git\nlog\t-p\x1b[31m".into()));
        assert_eq!(m.query, "git log -p [31m");
        assert_eq!(m.focus, Focus::Query);
        assert_eq!(
            searches(&e).len(),
            1,
            "a paste is one edit, not one per char"
        );

        let mut m = UiModel::new(false);
        update(&mut m, UiAction::Paste("x".repeat(10_000)));
        assert_eq!(m.query.chars().count(), QUERY_MAX_CHARS);
    }

    #[test]
    fn idle_ticks_do_not_redraw() {
        let mut m = UiModel::new(false);
        m.dirty = false;
        update(&mut m, UiAction::Tick(Instant::now()));
        assert!(!m.dirty);
        update(&mut m, UiAction::Resize);
        assert!(m.dirty);
    }

    /// Small deterministic generator so the fuzz below needs no new crate.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }

    #[test]
    fn no_action_sequence_starts_recall_or_picker_without_the_user_asking() {
        let keyset = [
            Key::Char('g'),
            Key::Char('y'),
            Key::Char('r'),
            Key::Char('f'),
            Key::Char('q'),
            Key::Char('j'),
            Key::Char('?'),
            Key::Char('/'),
            Key::Ctrl('r'),
            Key::Ctrl('f'),
            Key::Ctrl('u'),
            Key::Ctrl('z'),
            Key::Up,
            Key::Down,
            Key::Enter,
            Key::Esc,
            Key::Tab,
            Key::BackTab,
            Key::Backspace,
            Key::PageDown,
        ];
        let mut rng = Lcg(42);
        let t0 = Instant::now();
        let mut starts = 0;
        for run in 0..300 {
            let mut m = UiModel::new(false);
            m.recall_model = Ok("http://127.0.0.1:8080 (llama_cpp)".into());
            let mut ops = Vec::new();
            for step in 0..60u64 {
                let action = match rng.next() % 10 {
                    0 => UiAction::Interrupt(t0 + Duration::from_secs(step * 2)),
                    1 => done(m.search.generation, page("g", &[1, 2, 3])),
                    2 => UiAction::PickerReturned(PickOutcome::Picked((rng.next() % 5) as i64)),
                    3 => match ops.last() {
                        Some(&op) => UiAction::Recall {
                            op,
                            event: RecallEvent::Calls { root: 1, child: 1 },
                        },
                        None => UiAction::Paste("cargo\nbuild".into()),
                    },
                    _ => UiAction::Key(keyset[(rng.next() % keyset.len() as u64) as usize]),
                };
                let confirming = m.modal == Some(Modal::ConfirmRecall);
                let is_y = matches!(action, UiAction::Key(Key::Char('y' | 'Y')));
                let picker_key = matches!(action, UiAction::Key(Key::Ctrl('f')))
                    || (matches!(action, UiAction::Key(Key::Char('f'))) && m.focus != Focus::Query);
                let suspend_key = matches!(action, UiAction::Key(Key::Ctrl('z')));
                for e in update(&mut m, action) {
                    match e {
                        UiEffect::StartRecall { op, .. } => {
                            assert!(
                                confirming && is_y,
                                "run {run} step {step}: recall without confirmation"
                            );
                            starts += 1;
                            ops.push(op);
                        }
                        UiEffect::OpenPicker(_) => assert!(picker_key, "run {run} step {step}"),
                        UiEffect::Suspend => assert!(suspend_key, "run {run} step {step}"),
                        UiEffect::Search { .. }
                        | UiEffect::OpenEvidence { .. }
                        | UiEffect::CancelRecall { .. }
                        | UiEffect::Exit => {}
                    }
                }
            }
        }
        assert!(
            starts > 0,
            "the fuzz must actually reach the confirmed path"
        );
    }
}
