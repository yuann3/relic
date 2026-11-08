//! Pure rendering of [`UiModel`]. Every status is a word; color only repeats
//! what the text already says, and `NO_COLOR` drops it entirely.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::model::{Focus, Modal, RecallState, RecallStatus, StepState, UiModel};
use crate::app::{MIN_PREFIX_CHARS, RecallOutcome, When, display_line, source_label};
use crate::domain::{ObservationKind, Provenance};
use crate::rlm::protocol::BudgetV1;

/// Below this the panes cannot show a usable row, so say so instead.
pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 10;
/// Under this width the three columns become named panes switched with Tab.
pub const NARROW_WIDTH: u16 = 100;
const MAX_STEPS_SHOWN: usize = 8;
/// Characters of a command one list row looks at; a row shows at most a
/// pane's width. The whole of a 1 MiB command took ~40 ms every frame.
const ROW_CHARS: usize = 1024;
/// Bytes of a command the evidence pane wraps, for the same reason. The
/// rest is counted on screen, not silently dropped.
const EVIDENCE_BYTES: usize = 16 * 1024;

struct Theme {
    title: Style,
    dim: Style,
    selected: Style,
    redacted: Style,
    ok: Style,
    busy: Style,
    bad: Style,
}

impl Theme {
    fn new(mono: bool) -> Self {
        let m = |modifier| Style::new().add_modifier(modifier);
        if mono {
            return Self {
                title: m(Modifier::BOLD),
                dim: Style::new(),
                selected: m(Modifier::REVERSED),
                redacted: m(Modifier::BOLD | Modifier::UNDERLINED),
                ok: Style::new(),
                busy: m(Modifier::BOLD),
                bad: m(Modifier::BOLD),
            };
        }
        // Foreground colors only: the terminal's own background stays.
        Self {
            title: m(Modifier::BOLD).fg(Color::Cyan),
            dim: Style::new().fg(Color::DarkGray),
            selected: m(Modifier::REVERSED),
            redacted: m(Modifier::BOLD | Modifier::UNDERLINED).fg(Color::Magenta),
            ok: Style::new().fg(Color::Green),
            busy: m(Modifier::BOLD).fg(Color::Yellow),
            bad: m(Modifier::BOLD).fg(Color::Red),
        }
    }
}

pub fn view(frame: &mut Frame, m: &UiModel) {
    workspace(frame, m);
    if m.ascii {
        for cell in frame.buffer_mut().content.iter_mut() {
            if !cell.symbol().is_ascii() {
                let c = ascii_for(cell.symbol());
                cell.set_char(c);
            }
        }
    }
}

/// What a terminal in a non-UTF-8 locale gets for a symbol it cannot show.
fn ascii_for(symbol: &str) -> char {
    match symbol {
        "─" | "━" => '-',
        "│" | "┃" => '|',
        "┌" | "┐" | "└" | "┘" | "┏" | "┓" | "┗" | "┛" => '+',
        "…" => '.',
        "·" => '-',
        "→" => '>',
        "↑" => '^',
        "↓" => 'v',
        _ => '?',
    }
}

fn workspace(frame: &mut Frame, m: &UiModel) {
    let t = Theme::new(m.mono);
    let area = frame.area();
    m.too_small
        .set(area.width < MIN_WIDTH || area.height < MIN_HEIGHT);
    if m.too_small.get() {
        let msg = format!(
            "terminal too small ({}x{}); relic needs at least {MIN_WIDTH}x{MIN_HEIGHT}. Ctrl-C twice quits.",
            area.width, area.height
        );
        frame.render_widget(Paragraph::new(msg).wrap(Wrap { trim: true }), area);
        return;
    }

    let narrow = area.width < NARROW_WIDTH;
    let [query, body, status] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(area);
    query_bar(frame, m, &t, query);
    status_bar(frame, m, &t, status);

    if narrow {
        let [tabs, pane] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(body);
        let shown = if m.focus == Focus::Query {
            Focus::Results
        } else {
            m.focus
        };
        let names = [
            (Focus::Results, "Results"),
            (Focus::Timeline, "Timeline"),
            (Focus::Evidence, "Evidence"),
            (Focus::Recall, "Recall"),
        ];
        let mut spans = vec![Span::raw(" ")];
        for (f, name) in names {
            if f == shown {
                spans.push(Span::styled(format!("[{name}]"), t.selected));
            } else {
                spans.push(Span::raw(format!(" {name} ")));
            }
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled("Tab switches", t.dim));
        frame.render_widget(Line::from(spans), tabs);
        match shown {
            Focus::Timeline => timeline(frame, m, &t, pane),
            Focus::Evidence => evidence(frame, m, &t, pane),
            Focus::Recall => recall(frame, m, &t, pane),
            Focus::Query | Focus::Results => results(frame, m, &t, pane),
        }
    } else {
        let [left, mid, right] = Layout::horizontal([
            Constraint::Percentage(40),
            Constraint::Percentage(30),
            Constraint::Percentage(30),
        ])
        .areas(body);
        let [tl, rc] =
            Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(mid);
        results(frame, m, &t, left);
        timeline(frame, m, &t, tl);
        recall(frame, m, &t, rc);
        evidence(frame, m, &t, right);
    }

    match m.modal {
        Some(Modal::Help) => help(frame, &t, area),
        Some(Modal::ConfirmRecall) => confirm_recall(frame, m, &t, area),
        None => {}
    }
}

/// Focus is shown by shape (thick border, bracketed title), not by color.
fn pane<'a>(t: &Theme, title: String, focused: bool) -> Block<'a> {
    let (title, border) = if focused {
        (format!("[ {title} ]"), BorderType::Thick)
    } else {
        (format!(" {title} "), BorderType::Plain)
    };
    Block::bordered()
        .border_type(border)
        .title(Span::styled(title, t.title))
}

fn query_bar(frame: &mut Frame, m: &UiModel, t: &Theme, area: Rect) {
    let focused = m.focus == Focus::Query;
    let block = pane(t, "Query · local index · offline".into(), focused);
    let inner = block.inner(area);
    // Room after the prompt, keeping a cell for the cursor.
    let shown = tail(
        &display_line(&m.query),
        usize::from(inner.width).saturating_sub(3),
    );
    let mut spans = vec![Span::styled("> ", t.title), Span::raw(shown.clone())];
    if m.query.is_empty() {
        spans.push(Span::styled("type to search your history", t.dim));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
    if focused && m.modal.is_none() {
        let x = inner.x + 2 + Line::raw(shown).width() as u16;
        frame.set_cursor_position((x.min(inner.right().saturating_sub(1)), inner.y));
    }
}

/// The start of `text` a list row can show, neutralized.
fn row_text(text: &str) -> String {
    text.chars()
        .take(ROW_CHARS)
        .map(crate::app::display_char)
        .collect()
}

/// The end of `text` that fits in `room` columns, behind '…' when cut:
/// typing and Backspace act at the end, so the end must stay in view.
fn tail(text: &str, room: usize) -> String {
    if Line::raw(text).width() <= room {
        return text.to_string();
    }
    let (mut width, mut start) = (1, text.len());
    for (i, c) in text.char_indices().rev() {
        let w = Span::raw(&text[i..i + c.len_utf8()]).width();
        if width + w > room {
            break;
        }
        width += w;
        start = i;
    }
    format!("…{}", &text[start..])
}

fn results(frame: &mut Frame, m: &UiModel, t: &Theme, area: Rect) {
    let s = &m.search;
    let title = match &s.page {
        Some(p) => {
            // Titles truncate in narrow panes, so the deferred-word hint
            // replaces the other notes; the status line still says
            // "(more exist)" for a full page.
            match p.deferred.as_deref() {
                _ if s.too_broad => format!(
                    "Results {} · still for \"{}\"",
                    p.hits.len(),
                    display_line(&p.query)
                ),
                Some(w) => format!(
                    "Results {} · \"{}\" needs {MIN_PREFIX_CHARS}+ chars",
                    p.hits.len(),
                    display_line(w)
                ),
                None if p.words_dropped > 0 => {
                    let s = if p.words_dropped == 1 { "" } else { "s" };
                    format!(
                        "Results {} · {} word{s} ignored",
                        p.hits.len(),
                        p.words_dropped
                    )
                }
                // "+" for a full page: the label must survive a narrow pane,
                // and the status line spells out "(more exist)".
                None => format!(
                    "Results {}{} · recently imported",
                    p.hits.len(),
                    if p.truncated { "+" } else { "" }
                ),
            }
        }
        None => "Results".into(),
    };
    let block = pane(t, title, matches!(m.focus, Focus::Query | Focus::Results));
    let Some(page) = &s.page else {
        let msg = match (&s.error, s.pending) {
            (Some(e), _) => Line::styled(format!("search failed: {e}"), t.bad),
            (None, true) => Line::styled("searching…", t.busy),
            (None, false) => Line::styled("no query yet", t.dim),
        };
        frame.render_widget(Paragraph::new(msg).block(block), area);
        return;
    };
    if page.hits.is_empty() {
        let msg = Line::styled(
            format!("no matches for \"{}\"", display_line(&page.query)),
            t.dim,
        );
        frame.render_widget(Paragraph::new(msg).block(block), area);
        return;
    }
    let items: Vec<ListItem> = page
        .hits
        .iter()
        .map(|h| {
            let mut spans = vec![
                Span::styled(h.when().label(), t.dim),
                Span::raw(format!(" {:<5} ", h.source)),
            ];
            spans.extend(safe_spans(&row_text(&h.text), t));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(t.selected)
        .highlight_symbol("> ");
    let mut state = ListState::default()
        .with_offset(s.list_offset.get())
        .with_selected(Some(s.selected));
    frame.render_stateful_widget(list, area, &mut state);
    s.list_offset.set(state.offset());
}

fn timeline(frame: &mut Frame, m: &UiModel, t: &Theme, area: Rect) {
    let focused = m.focus == Focus::Timeline;
    let Some(v) = &m.evidence.view else {
        let block = pane(t, "Timeline".into(), focused);
        let msg = match &m.evidence.error {
            Some(e) => Line::styled(e.clone(), t.bad),
            None => Line::styled("select a result to see what ran around it", t.dim),
        };
        frame.render_widget(Paragraph::new(msg).block(block), area);
        return;
    };
    let title = format!(
        "Timeline · {} · revision {}",
        source_label(v.observation.kind),
        v.observation.revision_id
    );
    let items: Vec<ListItem> = v
        .timeline
        .iter()
        .map(|o| {
            let is_hit = o.id == v.observation.id;
            let mark = if is_hit {
                Span::styled("hit ", t.title)
            } else {
                Span::raw("    ")
            };
            // Month-day is enough beside neighbors; the detail pane has the year.
            let mut spans = vec![mark, Span::styled(v.when(o).short(), t.dim), Span::raw(" ")];
            spans.extend(safe_spans(&row_text(&o.safe_text), t));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .block(pane(t, title, focused))
        .highlight_style(t.selected)
        .highlight_symbol("> ");
    let mut state = ListState::default()
        .with_offset(m.evidence.list_offset.get())
        .with_selected(Some(m.evidence.cursor));
    frame.render_stateful_widget(list, area, &mut state);
    m.evidence.list_offset.set(state.offset());
}

fn evidence(frame: &mut Frame, m: &UiModel, t: &Theme, area: Rect) {
    let block = pane(t, "Evidence".into(), m.focus == Focus::Evidence);
    let Some(o) = m.evidence.shown() else {
        let msg = match &m.evidence.error {
            Some(e) => Line::styled(e.clone(), t.bad),
            None => Line::styled("Enter opens the selected result", t.dim),
        };
        frame.render_widget(Paragraph::new(msg).block(block), area);
        return;
    };
    let when = m
        .evidence
        .view
        .as_ref()
        .map_or(When::Unknown, |v| v.when(o));
    let field = |name: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{name}: "), t.title),
            Span::raw(value),
        ])
    };
    let from = if matches!(o.kind, ObservationKind::GitReflog | ObservationKind::GitRef) {
        "git"
    } else {
        "file"
    };
    let provenance = match (o.kind, m.evidence.view.as_ref().map(|v| v.provenance)) {
        // Git keeps no time for where a ref points; Relic's clock is no
        // source's.
        (ObservationKind::GitRef, Some(_)) => "text from git, time of import".to_string(),
        (_, Some(p)) if p.has(Provenance::OBSERVED_AT) => format!("text, time from {from}"),
        (_, Some(_)) => format!("text only from {from}"),
        (_, None) => "unknown".to_string(),
    };
    let mut lines = vec![
        field("id", format!("#{}", o.id)),
        field("source", source_label(o.kind).to_string()),
        field("time", when.detail()),
        field(
            "revision",
            format!("{} · ordinal {}", o.revision_id, o.ordinal),
        ),
        field("provenance", provenance),
        Line::raw(""),
        Line::styled("exact text, redacted at import:", t.title),
    ];
    let shown = o.safe_text.floor_char_boundary(EVIDENCE_BYTES);
    for l in o.safe_text[..shown].split('\n') {
        lines.push(Line::from(safe_spans(&display_line(l), t)));
    }
    if shown < o.safe_text.len() {
        lines.push(Line::styled(
            format!(
                "… {} more bytes not shown; `relic search --json` prints all of it",
                o.safe_text.len() - shown
            ),
            t.dim,
        ));
    }
    let inner = block.inner(area);
    let p = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let rows = p.line_count(inner.width);
    let max = u16::try_from(rows.saturating_sub(usize::from(inner.height))).unwrap_or(u16::MAX);
    m.evidence.scroll_max.set(max);
    let p = p.block(block).scroll((m.evidence.scroll.min(max), 0));
    frame.render_widget(p, area);
}

/// What a recall is doing or how it ended, as a word and its style.
fn recall_word(r: &RecallState, t: &Theme) -> (&'static str, Style) {
    match (&r.status, r.report.as_ref().map(|r| r.outcome)) {
        (RecallStatus::Idle, _) => ("idle", t.dim),
        (RecallStatus::Running, _) => ("running", t.busy),
        (RecallStatus::Cancelled, _) | (_, Some(RecallOutcome::Cancelled)) => ("cancelled", t.bad),
        (_, Some(RecallOutcome::Answered)) => ("answered", t.ok),
        (_, Some(RecallOutcome::Partial)) => ("partly answered", t.ok),
        (_, Some(RecallOutcome::Abstained)) => ("no answer", t.busy),
        (_, Some(RecallOutcome::BudgetExhausted)) => ("out of budget", t.bad),
        (_, Some(RecallOutcome::Failed)) => ("failed", t.bad),
        (RecallStatus::Finished, None) => ("finished", t.ok),
    }
}

/// The spend a recall may reach, as the confirmation promises it.
fn recall_cap(b: &BudgetV1, priced: bool) -> String {
    let cost = match b.max_cost_microusd {
        Some(c) if priced => format!("${:.2}", c as f64 / 1e6),
        _ => "cost unknown".into(),
    };
    format!("{cost} · {}s", b.wall_ms / 1000)
}

fn recall(frame: &mut Frame, m: &UiModel, t: &Theme, area: Rect) {
    let r = &m.recall;
    let (word, style) = recall_word(r, t);
    let budget = BudgetV1::prototype_default();
    let block = pane(t, "Recall".into(), m.focus == Focus::Recall);
    let mut lines = vec![Line::from(vec![
        Span::raw("status: "),
        Span::styled(word, style),
    ])];
    if r.status == RecallStatus::Idle {
        lines.push(Line::styled("r asks before starting a recall", t.dim));
    } else {
        lines.push(Line::raw(format!(
            "budget: wall {:.1}s of {}",
            r.elapsed().as_secs_f32(),
            recall_cap(&budget, m.recall_priced),
        )));
        lines.push(Line::raw(format!(
            "calls: root {}/{} · child {}/{}",
            r.root_calls, budget.root_calls, r.child_calls, budget.child_calls,
        )));
        // A long recall keeps its newest steps on screen.
        let hidden = r.steps.len().saturating_sub(MAX_STEPS_SHOWN);
        if hidden > 0 {
            lines.push(Line::styled(format!("({hidden} earlier steps)"), t.dim));
        }
        for s in &r.steps[hidden..] {
            let (w, st) = match s.state {
                StepState::Running => ("run ", t.busy),
                StepState::Done => ("done", t.ok),
                StepState::Cancelled => ("stop", t.bad),
            };
            let mut spans = vec![
                Span::styled(w, st),
                Span::raw(" "),
                Span::raw(display_line(&s.label)),
            ];
            if let Some(out) = &s.outcome {
                spans.push(Span::styled(format!(" → {}", display_line(out)), t.dim));
            }
            lines.push(Line::from(spans));
        }
        if let Some(report) = &r.report {
            report_lines(&mut lines, report, t);
        }
    }
    let inner = block.inner(area);
    let p = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let rows = p.line_count(inner.width);
    let max = u16::try_from(rows.saturating_sub(usize::from(inner.height))).unwrap_or(u16::MAX);
    r.scroll_max.set(max);
    let p = p.block(block).scroll((r.scroll.unwrap_or(max).min(max), 0));
    frame.render_widget(p, area);
}

/// A finished recall: its answer, then each claim and step with its cites.
fn report_lines(lines: &mut Vec<Line<'static>>, r: &crate::app::RecallReport, t: &Theme) {
    let cites = |ids: &[i64]| {
        ids.iter()
            .map(|c| format!("#{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(e) = &r.error {
        lines.push(Line::styled(format!("error: {}", display_line(e)), t.bad));
    }
    if !r.answer.is_empty() {
        lines.push(Line::styled("answer:", t.title));
        lines.push(Line::from(safe_spans(&display_line(&r.answer), t)));
    }
    for c in &r.claims {
        let mut spans = vec![Span::raw("- ")];
        spans.extend(safe_spans(&display_line(&c.text), t));
        spans.push(Span::styled(format!(" [{}]", cites(&c.supports)), t.dim));
        if !c.contradicts.is_empty() {
            spans.push(Span::styled(
                format!(" against [{}]", cites(&c.contradicts)),
                t.bad,
            ));
        }
        lines.push(Line::from(spans));
    }
    if !r.procedure.is_empty() {
        lines.push(Line::styled("procedure:", t.title));
    }
    for (i, s) in r.procedure.iter().enumerate() {
        let mut spans = vec![Span::raw(format!("{}. ", i + 1))];
        spans.extend(safe_spans(&display_line(&s.text), t));
        spans.push(Span::styled(format!(" [{}]", cites(&s.supports)), t.dim));
        lines.push(Line::from(spans));
    }
    for (label, items) in [
        ("contradictions:", &r.contradictions),
        ("missing:", &r.missing),
    ] {
        if !items.is_empty() {
            lines.push(Line::styled(label, t.title));
        }
        for m in items {
            lines.push(Line::from(
                [vec![Span::raw("- ")], safe_spans(&display_line(m), t)].concat(),
            ));
        }
    }
    if !r.evidence.is_empty() {
        lines.push(Line::styled(
            "Enter here opens the first cited record",
            t.dim,
        ));
    }
}

fn status_bar(frame: &mut Frame, m: &UiModel, t: &Theme, area: Rect) {
    let s = &m.search;
    let search = match (&s.error, s.pending, &s.page, s.latency) {
        (Some(_), _, _, _) => Span::styled("search: failed", t.bad),
        (None, true, _, _) => Span::styled("search: running", t.busy),
        (None, false, Some(_), _) if s.too_broad => Span::styled(
            format!(
                "search: too broad, stopped after {} ms",
                crate::app::QUERY_DEADLINE.as_millis()
            ),
            t.bad,
        ),
        (None, false, Some(p), Some(lat)) => Span::raw(format!(
            "search: {} hits in {:.1} ms{}",
            p.hits.len(),
            lat.as_secs_f64() * 1000.0,
            if p.truncated { " (more exist)" } else { "" }
        )),
        _ => Span::styled("search: idle", t.dim),
    };
    let (word, style) = recall_word(&m.recall, t);
    let recall = match m.recall.status {
        RecallStatus::Running | RecallStatus::Idle => {
            Span::styled(format!("recall: {word}"), style)
        }
        RecallStatus::Cancelled | RecallStatus::Finished => Span::raw(format!("recall: {word}")),
    };
    let mut first = vec![search, Span::raw(" · "), recall];
    if let Some(n) = &m.notice {
        first.push(Span::raw(" · "));
        first.push(Span::styled(display_line(n), t.busy));
    }
    let hints = match m.focus {
        Focus::Query => {
            "↑/↓ select · Enter open · Tab pane, then ? help · ^F fzf · ^R recall · Esc quit"
        }
        _ => "j/k move · Enter open · Tab pane · / search · f fzf · r recall · ? help · q quit",
    };
    let [a, b] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    frame.render_widget(Line::from(first), a);
    frame.render_widget(Line::styled(hints, t.dim), b);
}

fn help(frame: &mut Frame, t: &Theme, area: Rect) {
    let rows = [
        ("typing", "edit the query; results update as you type"),
        ("Backspace / ^U", "delete a character / clear the query"),
        ("↑ ↓  ^N ^P  j k", "move (j/k when focus is not the query)"),
        ("Enter", "open evidence for the selection or citation"),
        ("Tab / Shift-Tab", "next / previous pane"),
        ("/", "back to the query"),
        ("f  or ^F", "pick from this page with fzf"),
        ("r  or ^R", "start (asks first) or cancel a recall"),
        ("Esc", "close dialog, cancel recall, back, then quit"),
        ("^C ^C", "cancel or clear; twice within 1 s quits"),
        ("^Z", "suspend to the shell; fg comes back"),
        ("q", "quit when not typing"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!("{k:<16}"), t.title),
                Span::raw(*d),
            ])
        })
        .collect();
    let box_ = centered(area, 66, lines.len() as u16 + 2);
    frame.render_widget(Clear, box_);
    let block = Block::bordered().title(Span::styled(" Keys · ? or Esc closes ", t.title));
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), box_);
}

fn confirm_recall(frame: &mut Frame, m: &UiModel, t: &Theme, area: Rect) {
    let b = BudgetV1::prototype_default();
    let to = m.recall_model.as_deref().unwrap_or("the model");
    let lines = vec![
        Line::raw(format!(
            "Recall asks a model: up to {} root + {} child calls, {}.",
            b.root_calls,
            b.child_calls,
            recall_cap(&b, m.recall_priced)
        )),
        Line::styled(
            "It sends the query and the commands it finds, redacted, to",
            t.dim,
        ),
        Line::styled(display_line(to), t.dim),
        Line::raw(""),
        Line::raw("Start? y/n"),
    ];
    let box_ = centered(area, 72, 7);
    frame.render_widget(Clear, box_);
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .title(Span::styled(" Confirm model use ", t.busy));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        box_,
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let (w, h) = (w.min(area.width), h.min(area.height));
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Splits out redaction markers so they read differently from the command
/// around them while still showing (and copying as) the literal marker.
fn safe_spans<'a>(text: &str, t: &Theme) -> Vec<Span<'a>> {
    let mut out = Vec::new();
    let mut pos = 0;
    for marker in crate::redact::markers(text) {
        if marker.start > pos {
            out.push(Span::raw(text[pos..marker.start].to_string()));
        }
        pos = marker.end;
        out.push(Span::styled(text[marker].to_string(), t.redacted));
    }
    if pos < text.len() {
        out.push(Span::raw(text[pos..].to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Modifier};

    use super::*;
    use crate::app::{self, MatchMode, SearchRequest};
    use crate::import::{SourceKind, import_file};
    use crate::redact::Redactor;
    use crate::store::Store;
    use crate::ui::model::{Key, RecallEvent, UiAction, UiEffect, update};

    fn store_with(lines: &[&str]) -> Store {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zsh_history");
        let mut f = std::fs::File::create(&path).unwrap();
        for (i, l) in lines.iter().enumerate() {
            writeln!(f, ": {}:0;{l}", 1_700_000_000 + i).unwrap();
        }
        let mut store = Store::open_memory().unwrap();
        import_file(&mut store, &path, SourceKind::Zsh, &Redactor::new()).unwrap();
        store
    }

    /// Plays the runtime's part: each effect is answered from a real store.
    fn drive(m: &mut UiModel, store: &Store, action: UiAction) {
        let mut queue = vec![action];
        while let Some(a) = queue.pop() {
            for e in update(m, a) {
                match e {
                    UiEffect::Search { generation, query } => {
                        let req = SearchRequest {
                            query,
                            limit: 200,
                            mode: MatchMode::PrefixLast,
                            order: app::Order::RecentlyImported,
                        };
                        let result = app::search(store, &req, &app::Budget::start());
                        let elapsed = Duration::from_micros(1500);
                        queue.push(UiAction::SearchDone {
                            generation,
                            result,
                            elapsed,
                        });
                    }
                    UiEffect::OpenEvidence { id } => {
                        let result = app::open_evidence(store, id, 8).map_err(|e| e.to_string());
                        queue.push(UiAction::EvidenceLoaded { id, result });
                    }
                    _ => {}
                }
            }
        }
    }

    fn typed(store: &Store, query: &str, mono: bool) -> UiModel {
        let mut m = UiModel::new(mono);
        for c in query.chars() {
            drive(&mut m, store, UiAction::Key(Key::Char(c)));
        }
        m
    }

    const NGINX: &[&str] = &[
        "cd /etc/nginx",
        "certbot renew",
        "systemctl reload nginx",
        "ls",
    ];

    fn render(m: &UiModel, w: u16, h: u16) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| view(f, m)).unwrap();
        term.backend().buffer().clone()
    }

    /// Rows as the terminal shows them: the cell after a wide glyph is a
    /// placeholder the backend never prints.
    fn text(b: &Buffer) -> String {
        let w = b.area.width as usize;
        b.content()
            .chunks(w)
            .map(|row| {
                let mut out = String::new();
                let mut skip = 0;
                for c in row {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    out.push_str(c.symbol());
                    skip = Span::raw(c.symbol()).width().saturating_sub(1);
                }
                out
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Cell at display column `x` of row `y`.
    fn cell(b: &Buffer, x: u16, y: u16) -> &ratatui::buffer::Cell {
        &b[(x, y)]
    }

    #[test]
    fn untimed_rows_show_the_inferred_bound() {
        // Plain lines written before share_history began stamping them.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zsh_history");
        std::fs::write(
            &path,
            "ccd --resume\nbrew install gh\n: 1791017948:0;cd Developer\n",
        )
        .unwrap();
        let mut store = Store::open_memory().unwrap();
        import_file(&mut store, &path, SourceKind::Zsh, &Redactor::new()).unwrap();

        let s = text(&render(&typed(&store, "ccd", false), 160, 40));
        assert!(s.contains("> ≤2026-10-03 08:59 zsh   ccd --resume"), "{s}");
        // Bounded and exact dates share a column; the marker sits left of it.
        assert!(s.contains("hit ≤10-03 08:59 ccd --resume"), "{s}");
        assert!(s.contains("     10-03 08:59 cd Developer"), "{s}");
        assert!(
            s.contains("no later than 2026-10-03") && s.contains("(inferred from history order)"),
            "{s}"
        );
        assert!(!s.contains("unknown time"), "{s}");
    }

    #[test]
    fn timeline_labels_untimed_history_as_unknown() {
        // Plain bash history has no timestamps.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bash_history");
        std::fs::write(&path, "cd /etc/nginx\ncertbot renew\nls\n").unwrap();
        let mut store = Store::open_memory().unwrap();
        import_file(&mut store, &path, SourceKind::Bash, &Redactor::new()).unwrap();

        let s = text(&render(&typed(&store, "certb", false), 120, 40));
        assert!(s.contains("hit  unknown     certbot renew"), "{s}");
        assert!(!s.contains("hit wn time"), "{s}");
        assert!(s.contains("provenance: text only from file"), "{s}");
    }

    #[test]
    fn git_evidence_says_its_text_is_from_git_and_a_refs_time_is_of_import() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .envs([
                    ("GIT_AUTHOR_NAME", "t"),
                    ("GIT_AUTHOR_EMAIL", "t@t"),
                    ("GIT_COMMITTER_NAME", "t"),
                    ("GIT_COMMITTER_EMAIL", "t@t"),
                ])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "wired"]);
        let mut store = Store::open_memory().unwrap();
        crate::import::import_git(
            &mut store,
            crate::source::git::read(dir.path()).unwrap(),
            &Redactor::new(),
        )
        .unwrap();
        // Refs are stored after the reflog, so the newest hit is the ref.
        let s = text(&render(&typed(&store, "wired", false), 160, 40));
        assert!(
            s.contains("provenance: text from git, time of import"),
            "{s}"
        );
        let s = text(&render(&typed(&store, "reflog wired", false), 160, 40));
        assert!(s.contains("provenance: text, time from git"), "{s}");
    }

    #[test]
    fn results_say_when_a_short_word_is_not_filtering_yet() {
        let store = store_with(NGINX);
        let s = text(&render(&typed(&store, "certbot r", false), 120, 40));
        assert!(s.contains("Results 1 · \"r\" needs 2+ chars"), "{s}");
        let s = text(&render(&typed(&store, "certbot re", false), 120, 40));
        assert!(s.contains("[ Results 1 · recently imported ]"), "{s}");
    }

    #[test]
    fn results_say_when_words_were_ignored_and_keep_the_order_label_on_a_full_page() {
        let store = store_with(NGINX);
        let long = vec!["x"; crate::app::MAX_WORD_TOKENS + 1].join("/");
        let s = text(&render(
            &typed(&store, &format!("certbot {long} "), false),
            120,
            40,
        ));
        assert!(s.contains("[ Results 1 · 1 word ignored ]"), "{s}");

        let mut m = typed(&store, "certbot re", false);
        m.search.page.as_mut().unwrap().truncated = true;
        // The narrowest three-pane layout: the label is not cut off.
        let s = text(&render(&m, 100, 40));
        assert!(s.contains("[ Results 1+ · recently imported ]"), "{s}");
    }

    #[test]
    fn a_too_broad_query_says_whose_page_is_still_shown() {
        let store = store_with(NGINX);
        let mut m = typed(&store, "certbot re", false);
        update(&mut m, UiAction::Key(Key::Char('n')));
        let generation = m.search.generation;
        let result = Err(crate::app::SearchError::TooBroad);
        update(
            &mut m,
            UiAction::SearchDone {
                generation,
                result,
                elapsed: Duration::from_millis(100),
            },
        );
        let s = text(&render(&m, 120, 40));
        assert!(s.contains("> certbot ren"), "{s}");
        assert!(
            s.contains("[ Results 1 · still for \"certbot re\" ]"),
            "{s}"
        );
        assert!(s.contains("search: too broad, stopped after 100 ms"), "{s}");
    }

    #[test]
    fn wide_layout_shows_every_pane() {
        let store = store_with(NGINX);
        let m = typed(&store, "certb", false);
        let s = text(&render(&m, 120, 40));
        for want in [
            "Query · local index · offline",
            "> certb",
            "Results 1",
            "2023-11-14 22:13 zsh   certbot renew",
            "Timeline · zsh",
            "cd /etc/nginx",
            "hit  11-14 22:13 certbot renew",
            "11-14 22:13 systemctl relo",
            "provenance: text, time from file",
            "revision: 1 · ordinal 1",
            "exact text, redacted at import:",
            "status: idle",
            "search: 1 hits in 1.5 ms",
            "Esc quit",
        ] {
            assert!(s.contains(want), "missing {want:?} in\n{s}");
        }
    }

    /// The list scrolls only when the highlight reaches an edge: moving
    /// back up from the bottom moves the highlight, not the rows.
    #[test]
    fn the_highlight_moves_inside_a_scrolled_list() {
        let rows: Vec<String> = (0..60).map(|i| format!("echo row-{i:02}")).collect();
        let store = store_with(&rows.iter().map(String::as_str).collect::<Vec<_>>());
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut m = typed(&store, "", false);
        drive(&mut m, &store, UiAction::Start);
        drive(&mut m, &store, UiAction::Key(Key::Tab));
        let mut screen = |m: &UiModel| {
            term.draw(|f| view(f, m)).unwrap();
            text(term.backend().buffer())
        };
        let row_of = |s: &str, needle: &str| s.lines().position(|l| l.contains(needle));
        for _ in 0..25 {
            drive(&mut m, &store, UiAction::Key(Key::Char('j')));
            screen(&m);
        }
        let s = screen(&m);
        let bottom = row_of(&s, ">  2023").unwrap();
        for _ in 0..3 {
            drive(&mut m, &store, UiAction::Key(Key::Char('k')));
        }
        let s = screen(&m);
        assert_eq!(row_of(&s, ">  2023"), Some(bottom - 3), "{s}");
        assert_eq!(
            row_of(&s, "row-34"),
            Some(bottom),
            "the rows stayed put:\n{s}"
        );
    }

    /// At 80 columns the hints end in how to quit, and a long query keeps
    /// its end, where typing happens, in view.
    #[test]
    fn hints_fit_80_columns_and_a_long_query_shows_its_end() {
        let store = store_with(NGINX);
        let long =
            "certbot renew --deploy-hook 'systemctl reload nginx' --cert-name example.com zz";
        let mut m = typed(&store, long, false);
        let s = text(&render(&m, 80, 24));
        let lines: Vec<&str> = s.lines().collect();
        assert!(
            lines[1].contains("…") && lines[1].contains("example.com zz"),
            "{s}"
        );
        assert!(lines[23].trim_end().ends_with("Esc quit"), "{s}");
        drive(&mut m, &store, UiAction::Key(Key::Tab));
        let s = text(&render(&m, 80, 24));
        assert!(
            s.lines().last().unwrap().trim_end().ends_with("q quit"),
            "{s}"
        );
        // A query that fits is shown whole, with no '…'.
        let s = text(&render(&typed(&store, "certb", false), 80, 24));
        assert!(s.contains("> certb") && !s.contains('…'), "{s}");
    }

    #[test]
    fn narrow_layout_switches_named_panes_with_tab() {
        let store = store_with(NGINX);
        let mut m = typed(&store, "certb", false);
        let s = text(&render(&m, 80, 24));
        assert!(
            s.contains("[Results]") && s.contains("certbot renew"),
            "{s}"
        );
        assert!(
            !s.contains("systemctl reload nginx"),
            "timeline must not be crushed in:\n{s}"
        );
        assert!(!s.contains("provenance:"));

        let tab = |m: &mut UiModel| drive(m, &store, UiAction::Key(Key::Tab));
        tab(&mut m); // Query -> Results: same pane
        assert!(text(&render(&m, 80, 24)).contains("[Results]"));
        tab(&mut m);
        let s = text(&render(&m, 80, 24));
        assert!(
            s.contains("[Timeline]") && s.contains("systemctl reload nginx"),
            "{s}"
        );
        tab(&mut m);
        let s = text(&render(&m, 80, 24));
        assert!(
            s.contains("[Evidence]") && s.contains("provenance: text, time from file"),
            "{s}"
        );
        tab(&mut m);
        assert!(text(&render(&m, 80, 24)).contains("status: idle"));
    }

    #[test]
    fn evidence_scroll_reaches_the_end_of_a_wrapped_command_and_stops() {
        let words: Vec<String> = (0..40).map(|i| format!("arg{i:02}")).collect();
        let long = format!("{} TAILWORD", words.join(" "));
        let store = store_with(&["ls", &long]);
        let mut m = typed(&store, "arg00", false);
        drive(&mut m, &store, UiAction::Key(Key::Enter));
        assert_eq!(m.focus, Focus::Evidence);
        // The event loop draws before the next key, which records the limit.
        render(&m, 100, 14);
        for _ in 0..1000 {
            drive(&mut m, &store, UiAction::Key(Key::Down));
        }
        let end = m.evidence.scroll;
        assert!(end > 0 && end < 100, "scrolled to {end}");
        // The narrowest evidence pane (wide layout at 100 columns) and a
        // short one: one line of text wraps to many rows here.
        let s = text(&render(&m, 100, 14));
        assert!(s.contains("TAILWORD"), "{s}");
    }

    /// A command longer than the pane wraps is cut at a stated point, and
    /// the pane says how much it left out.
    #[test]
    fn evidence_counts_the_bytes_of_a_huge_command_it_does_not_show() {
        let long = format!("huge {}", "x".repeat(EVIDENCE_BYTES * 3));
        let store = store_with(&["ls", &long]);
        let mut m = typed(&store, "huge", false);
        drive(&mut m, &store, UiAction::Key(Key::Enter));
        render(&m, 120, 40);
        for _ in 0..2000 {
            drive(&mut m, &store, UiAction::Key(Key::PageDown));
        }
        let s = text(&render(&m, 120, 40));
        let left_out = long.len() - EVIDENCE_BYTES;
        assert!(
            s.contains(&format!("… {left_out} more bytes not shown")),
            "{s}"
        );
    }

    /// In a non-UTF-8 locale every cell is ASCII, and the frame keeps its
    /// shape: corners, edges and separators have ASCII stand-ins.
    #[test]
    fn a_non_utf8_locale_draws_ascii() {
        let store = store_with(&["echo 部署 café", "ls"]);
        let mut m = typed(&store, "echo", false);
        m.ascii = true;
        let s = text(&render(&m, 120, 30));
        assert!(s.is_ascii(), "{s}");
        let first = s.lines().next().unwrap();
        assert!(
            first.starts_with("+[ Query - local index - offline ]---") && first.ends_with('+'),
            "{s}"
        );
        // A wide glyph keeps its two columns, so the rows stay aligned.
        assert!(s.contains("echo ??  caf?"), "{s}");
    }

    #[test]
    fn tiny_terminal_says_so() {
        let store = store_with(NGINX);
        let m = typed(&store, "certb", false);
        let s = text(&render(&m, 30, 8));
        assert!(s.contains("terminal too") && s.contains("(30x8)"), "{s}");
        assert!(!s.contains("certbot"));
        // Exactly at the minimum the workspace renders.
        assert!(text(&render(&m, MIN_WIDTH, MIN_HEIGHT)).contains("Query"));
    }

    fn colored_cells(b: &Buffer) -> usize {
        b.content()
            .iter()
            .filter(|c| c.fg != Color::Reset || c.bg != Color::Reset)
            .count()
    }

    #[test]
    fn no_color_renders_monochrome_but_keeps_meaning() {
        let store = store_with(&["deploy API_KEY=abc123 --prod"]);
        let mut m = typed(&store, "deploy", true);
        drive(&mut m, &store, UiAction::Key(Key::Enter));
        let b = render(&m, 120, 40);
        assert_eq!(colored_cells(&b), 0, "NO_COLOR must not paint any cell");
        let s = text(&b);
        assert!(
            s.contains("[ Evidence ]"),
            "focus shown by shape and brackets:\n{s}"
        );
        assert!(s.contains("status: idle"));

        let mut colored = typed(&store, "deploy", false);
        drive(&mut colored, &store, UiAction::Key(Key::Enter));
        assert!(colored_cells(&render(&colored, 120, 40)) > 0);
    }

    #[test]
    fn redaction_markers_are_distinct_and_literal() {
        let store = store_with(&["deploy API_KEY=abc123 --prod"]);
        let mut m = typed(&store, "deploy", true);
        drive(&mut m, &store, UiAction::Key(Key::Enter));
        let b = render(&m, 120, 40);
        let s = text(&b);
        assert!(!s.contains("abc123"));
        // The last occurrence is the evidence pane's exact-text line.
        let (row, line) = s
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains("API_KEY=[REDACTED:key]"))
            .last()
            .unwrap();
        let col = Span::raw(&line[..line.rfind("[REDACTED:key]").unwrap()]).width() as u16;
        let row = row as u16;
        for x in col..col + "[REDACTED:key]".len() as u16 {
            assert!(
                cell(&b, x, row).modifier.contains(Modifier::UNDERLINED),
                "marker cell {x} not distinct"
            );
        }
        assert!(
            !cell(&b, col - 2, row)
                .modifier
                .contains(Modifier::UNDERLINED),
            "surrounding text styled as marker"
        );
    }

    /// Bypasses the importer: it currently mangles non-ASCII (redact.rs
    /// pushes bytes as chars), and this test is about rendering.
    fn showing(text: &str) -> UiModel {
        use crate::app::{EvidenceView, PageHit, SEARCH_SCHEMA, SearchPage};
        use crate::domain::{ObservationKind, StoredObservation};
        let mut m = UiModel::new(false);
        update(&mut m, UiAction::Key(Key::Char('x')));
        let hit = PageHit {
            id: 5,
            observed_at_ms: Some(0),
            observed_before_ms: None,
            source: "zsh",
            text: text.into(),
        };
        let page = SearchPage {
            schema: SEARCH_SCHEMA,
            query: "x".into(),
            limit: 200,
            truncated: false,
            deferred: None,
            words_dropped: 0,
            order: crate::app::Order::RecentlyImported,
            filtered: true,
            hits: vec![hit],
        };
        let generation = m.search.generation;
        update(
            &mut m,
            UiAction::SearchDone {
                generation,
                result: Ok(page),
                elapsed: Duration::ZERO,
            },
        );
        let o = StoredObservation {
            id: 5,
            revision_id: 1,
            ordinal: 0,
            kind: ObservationKind::ZshCommand,
            observed_at_ms: Some(0),
            safe_text: text.into(),
            flags: 0,
            locator: vec![],
        };
        let view = EvidenceView {
            observation: o.clone(),
            timeline: vec![o],
            bounds: Default::default(),
            provenance: Provenance::COMMAND.with(Provenance::OBSERVED_AT),
        };
        update(
            &mut m,
            UiAction::EvidenceLoaded {
                id: 5,
                result: Ok(Some(view)),
            },
        );
        m
    }

    #[test]
    fn long_unicode_and_hostile_text_stay_inside_their_panes() {
        let long = format!("echo 部署完成 🚀 {} tail-marker", "word ".repeat(80));
        let hostile = "printf '\x1b]0;pwned\x07' \x1b[2J\u{9b}31m";
        for (q, input) in [("echo", long.as_str()), ("printf", hostile)] {
            let m = showing(input);
            let b = render(&m, 120, 40);
            let s = text(&b);
            assert!(
                !s.chars().any(|c| c != '\n' && c.is_control()),
                "control chars reached the buffer"
            );
            // The results pane is 40% of 120 columns; its right border must
            // survive on every row, wide glyphs or not.
            for (i, line) in s.lines().enumerate().skip(4).take(30) {
                let c = cell(&b, 47, i as u16);
                assert!(
                    matches!(c.symbol(), "│" | "┃"),
                    "row {i} border broken: {line}"
                );
            }
            if q == "echo" {
                assert!(s.contains("部署完成 🚀"), "{s}");
                assert!(
                    s.contains("tail-marker"),
                    "evidence must wrap long text, not cut it:\n{s}"
                );
            } else {
                assert!(
                    s.contains("printf '\u{FFFD}]0;pwned\u{FFFD}' \u{FFFD}[2J\u{FFFD}31m"),
                    "{s}"
                );
            }
        }
    }

    #[test]
    fn confirmation_and_help_overlays() {
        let store = store_with(NGINX);
        let mut m = typed(&store, "certb", false);
        m.recall_model = Ok("https://api.example.com (openai)".into());
        drive(&mut m, &store, UiAction::Key(Key::Ctrl('r')));
        let s = text(&render(&m, 120, 40));
        assert!(s.contains("16 child calls, cost unknown"), "{s}");
        m.recall_priced = true;
        let s = text(&render(&m, 120, 40));
        assert!(
            s.contains("Recall asks a model: up to 8 root + 16 child calls, $1.00 · 180s."),
            "{s}"
        );
        assert!(
            s.contains("Start? y/n") && s.contains("https://api.example.com (openai)"),
            "{s}"
        );

        drive(&mut m, &store, UiAction::Key(Key::Esc));
        drive(&mut m, &store, UiAction::Key(Key::Tab));
        drive(&mut m, &store, UiAction::Key(Key::Char('?')));
        let s = text(&render(&m, 80, 24));
        assert!(
            s.contains("Keys · ? or Esc closes") && s.contains("twice within 1 s quits"),
            "{s}"
        );
    }

    #[test]
    fn recall_progress_and_finding_render_as_words() {
        let store = store_with(NGINX);
        let mut m = typed(&store, "certb", true);
        m.recall_model = Ok("http://127.0.0.1:8080 (llama_cpp)".into());
        m.recall_priced = true;
        let cite = m.search.selected_hit().unwrap().id;
        drive(&mut m, &store, UiAction::Key(Key::Ctrl('r')));
        drive(&mut m, &store, UiAction::Key(Key::Char('y')));
        let op = m.recall.active.unwrap();
        let t0 = Instant::now();
        for event in [
            RecallEvent::Started { at: t0 },
            RecallEvent::StepStarted("plan".into()),
            RecallEvent::StepDone("2 searches".into()),
            RecallEvent::StepStarted("assess batch 1/3".into()),
            RecallEvent::Calls { root: 2, child: 3 },
        ] {
            drive(&mut m, &store, UiAction::Recall { op, event });
        }
        drive(
            &mut m,
            &store,
            UiAction::Tick(t0 + Duration::from_millis(2300)),
        );
        let s = text(&render(&m, 120, 40));
        for want in [
            "status: running",
            "wall 2.3s of $1.00 · 180s",
            "root 2/8",
            "child 3/16",
            "done plan → 2 searches",
            "run  assess batch 1/3",
            "recall: running",
        ] {
            assert!(s.contains(want), "missing {want:?} in\n{s}");
        }

        // Longer than the pane is wide: the claim must wrap, not be cut.
        let mut report = crate::ui::model::answered(&[cite]);
        report.claims[0].text =
            "you renewed the certificate with certbot after the reload; claim-end".into();
        report.procedure = vec![crate::app::CitedText {
            text: "certbot renew".into(),
            supports: vec![cite],
            contradicts: vec![],
        }];
        report.missing = vec!["when it expires".into()];
        drive(
            &mut m,
            &store,
            UiAction::Recall {
                op,
                event: RecallEvent::Finished(Box::new(report)),
            },
        );
        let s = text(&render(&m, 120, 40));
        for want in [
            "with certbot",
            &format!("claim-end [#{cite}]"),
            &format!("1. certbot renew [#{cite}]"),
            "- when it expires",
            "recall: answered",
        ] {
            assert!(s.contains(want), "missing {want:?} in\n{s}");
        }

        // At 120x26 the pane is shorter than its text: it follows the end,
        // so the finding is in view, and k scrolls back to the start.
        let s = text(&render(&m, 120, 26));
        assert!(
            s.contains("claim-end") && !s.contains("status: answered"),
            "{s}"
        );
        for _ in 0..20 {
            drive(&mut m, &store, UiAction::Key(Key::Char('k')));
        }
        let s = text(&render(&m, 120, 26));
        assert!(
            s.contains("status: answered") && !s.contains("claim-end"),
            "{s}"
        );
    }

    #[test]
    fn a_long_recall_keeps_its_newest_steps_on_screen() {
        let store = store_with(NGINX);
        let mut m = typed(&store, "certb", false);
        m.recall_model = Ok("http://127.0.0.1:8080 (llama_cpp)".into());
        drive(&mut m, &store, UiAction::Key(Key::Ctrl('r')));
        drive(&mut m, &store, UiAction::Key(Key::Char('y')));
        let op = m.recall.active.unwrap();
        for i in 0..30 {
            let event = RecallEvent::StepStarted(format!("step-{i:02}"));
            drive(&mut m, &store, UiAction::Recall { op, event });
        }
        let s = text(&render(&m, 120, 40));
        assert!(
            s.contains("step-29") && s.contains("(22 earlier steps)"),
            "{s}"
        );
        assert!(!s.contains("step-21"));
    }
}
