//! Pure rendering of [`UiModel`]. Every status is a word; color only repeats
//! what the text already says, and `NO_COLOR` drops it entirely.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::model::{
    Focus, Modal, RECALL_CHILD_MAX, RECALL_ROOT_MAX, RECALL_SPEND_CAP, RECALL_WALL_MAX,
    RecallStatus, StepState, UiModel,
};
use crate::app::{display_line, format_utc, source_label};

/// Below this the panes cannot show a usable row, so say so instead.
pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 10;
/// Under this width the three columns become named panes switched with Tab.
pub const NARROW_WIDTH: u16 = 100;
const MAX_STEPS_SHOWN: usize = 8;

/// What the importer writes for a redacted secret (see `redact.rs`).
const REDACTED: &str = "[REDACTED]";

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
    let t = Theme::new(m.mono);
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
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
        Some(Modal::ConfirmRecall) => confirm_recall(frame, &t, area),
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
    let mut spans = vec![
        Span::styled("> ", t.title),
        Span::raw(display_line(&m.query)),
    ];
    if m.query.is_empty() {
        spans.push(Span::styled("type to search your history", t.dim));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
    if focused && m.modal.is_none() {
        let x = inner.x + 2 + ratatui::text::Line::raw(display_line(&m.query)).width() as u16;
        frame.set_cursor_position((x.min(inner.right().saturating_sub(1)), inner.y));
    }
}

fn results(frame: &mut Frame, m: &UiModel, t: &Theme, area: Rect) {
    let s = &m.search;
    let title = match &s.page {
        Some(p) if p.truncated => format!("Results {} (page full; narrow the query)", p.hits.len()),
        Some(p) => format!("Results {}", p.hits.len()),
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
                Span::styled(when(h.observed_at_ms), t.dim),
                Span::raw(format!("  {:<5} ", h.source)),
            ];
            spans.extend(safe_spans(&display_line(&h.text), t));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(t.selected)
        .highlight_symbol("> ");
    let mut state = ListState::default().with_selected(Some(s.selected));
    frame.render_stateful_widget(list, area, &mut state);
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
            let short = o
                .observed_at_ms
                .map_or_else(|| "unknown    ".into(), |ms| format_utc(ms)[5..].to_string());
            let mut spans = vec![mark, Span::styled(format!("{short} "), t.dim)];
            spans.extend(safe_spans(&display_line(&o.safe_text), t));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .block(pane(t, title, focused))
        .highlight_style(t.selected)
        .highlight_symbol("> ");
    let mut state = ListState::default().with_selected(Some(m.evidence.cursor));
    frame.render_stateful_widget(list, area, &mut state);
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
    let field = |name: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{name}: "), t.title),
            Span::raw(value),
        ])
    };
    // The importer stores an empty provenance blob today. Say so rather than
    // imply a source location we do not have.
    let provenance = if o.provenance.is_empty() {
        "not recorded (store keeps none yet)".to_string()
    } else {
        format!("{} bytes, not decoded by the prototype", o.provenance.len())
    };
    let mut lines = vec![
        field("id", format!("#{}", o.id)),
        field("source", source_label(o.kind).to_string()),
        field(
            "time",
            match o.observed_at_ms {
                Some(ms) => format!("{} UTC", format_utc(ms)),
                None => "unknown".into(),
            },
        ),
        field(
            "revision",
            format!("{} · ordinal {}", o.revision_id, o.ordinal),
        ),
        field("provenance", provenance),
        Line::raw(""),
        Line::styled("exact text, redacted at import:", t.title),
    ];
    for l in o.safe_text.split('\n') {
        lines.push(Line::from(safe_spans(&display_line(l), t)));
    }
    let p = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((m.evidence.scroll, 0));
    frame.render_widget(p, area);
}

fn recall(frame: &mut Frame, m: &UiModel, t: &Theme, area: Rect) {
    let r = &m.recall;
    let (word, style) = match &r.status {
        RecallStatus::Idle => ("idle", t.dim),
        RecallStatus::Running => ("running", t.busy),
        RecallStatus::Cancelled => ("cancelled", t.bad),
        RecallStatus::Finished => ("finished", t.ok),
    };
    let block = pane(t, "Recall".into(), m.focus == Focus::Recall);
    let mut lines = vec![Line::from(vec![
        Span::raw("status: "),
        Span::styled(word, style),
        Span::styled(" (fake, no model)", t.dim),
    ])];
    if r.status == RecallStatus::Idle {
        lines.push(Line::styled("r asks before starting a recall", t.dim));
    } else {
        lines.push(Line::raw(format!(
            "budget: wall {:.1}s/{}s · cap {RECALL_SPEND_CAP}",
            r.elapsed().as_secs_f32(),
            RECALL_WALL_MAX.as_secs(),
        )));
        lines.push(Line::raw(format!(
            "calls: root {}/{RECALL_ROOT_MAX} · child {}/{RECALL_CHILD_MAX}",
            r.root_calls, r.child_calls,
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
        if let Some(f) = &r.finding {
            let cites: Vec<String> = f.cites.iter().map(|c| format!("#{c}")).collect();
            lines.push(Line::styled(
                format!("finding (cites {}):", cites.join(", ")),
                t.title,
            ));
            lines.push(Line::from(safe_spans(&display_line(&f.claim), t)));
            lines.push(Line::styled("Enter here opens the cited evidence", t.dim));
        }
    }
    let p = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

fn status_bar(frame: &mut Frame, m: &UiModel, t: &Theme, area: Rect) {
    let s = &m.search;
    let search = match (&s.error, s.pending, &s.page, s.latency) {
        (Some(_), _, _, _) => Span::styled("search: failed", t.bad),
        (None, true, _, _) => Span::styled("search: running", t.busy),
        (None, false, Some(p), Some(lat)) => Span::raw(format!(
            "search: {} hits in {:.1} ms{}",
            p.hits.len(),
            lat.as_secs_f64() * 1000.0,
            if p.truncated { " (more exist)" } else { "" }
        )),
        _ => Span::styled("search: idle", t.dim),
    };
    let recall = match m.recall.status {
        RecallStatus::Running => Span::styled("recall: running", t.busy),
        RecallStatus::Idle => Span::styled("recall: idle", t.dim),
        RecallStatus::Cancelled => Span::raw("recall: cancelled"),
        RecallStatus::Finished => Span::raw("recall: finished"),
    };
    let mut first = vec![search, Span::raw(" · "), recall];
    if let Some(n) = &m.notice {
        first.push(Span::raw(" · "));
        first.push(Span::styled(display_line(n), t.busy));
    }
    let hints = match m.focus {
        Focus::Query => {
            "type to search · ↑/↓ select · Enter open · Tab pane · ^F fzf · ^R recall · ? help · Esc quit"
        }
        _ => {
            "j/k move · Enter open · Tab pane · / search · f fzf · r recall · ? help · Esc back · q quit"
        }
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

fn confirm_recall(frame: &mut Frame, t: &Theme, area: Rect) {
    let lines = vec![
        Line::raw(format!(
            "Recall uses a model and may spend up to {RECALL_SPEND_CAP} / {RECALL_ROOT_MAX} root + {RECALL_CHILD_MAX} child calls. Start? y/n"
        )),
        Line::raw(""),
        Line::styled(
            "Prototype: this recall is scripted. No model is called and nothing leaves this machine.",
            t.dim,
        ),
    ];
    let box_ = centered(area, 64, 7);
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

fn when(ms: Option<i64>) -> String {
    ms.map(format_utc)
        .unwrap_or_else(|| "unknown time    ".into())
}

/// Splits out redaction markers so they read differently from the command
/// around them while still showing (and copying as) the literal marker.
fn safe_spans<'a>(text: &str, t: &Theme) -> Vec<Span<'a>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(REDACTED) {
        if i > 0 {
            out.push(Span::raw(rest[..i].to_string()));
        }
        out.push(Span::styled(REDACTED, t.redacted));
        rest = &rest[i + REDACTED.len()..];
    }
    if !rest.is_empty() {
        out.push(Span::raw(rest.to_string()));
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
                        };
                        let result = app::search(store, &req).map_err(|e| e.to_string());
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
    fn timeline_labels_untimed_history_as_unknown() {
        // Plain bash history has no timestamps.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bash_history");
        std::fs::write(&path, "cd /etc/nginx\ncertbot renew\nls\n").unwrap();
        let mut store = Store::open_memory().unwrap();
        import_file(&mut store, &path, SourceKind::Bash, &Redactor::new()).unwrap();

        let s = text(&render(&typed(&store, "certb", false), 120, 40));
        assert!(s.contains("hit unknown     certbot renew"), "{s}");
        assert!(!s.contains("hit wn time"), "{s}");
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
            "2023-11-14 22:13  zsh   certbot renew",
            "Timeline · zsh",
            "cd /etc/nginx",
            "hit 11-14 22:13 certbot renew",
            "11-14 22:13 systemctl reload",
            "provenance: not recorded",
            "revision: 1 · ordinal 1",
            "exact text, redacted at import:",
            "status: idle",
            "search: 1 hits in 1.5 ms",
            "Esc quit",
        ] {
            assert!(s.contains(want), "missing {want:?} in\n{s}");
        }
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
            s.contains("[Evidence]") && s.contains("provenance: not recorded"),
            "{s}"
        );
        tab(&mut m);
        assert!(text(&render(&m, 80, 24)).contains("status: idle"));
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
            .filter(|(_, l)| l.contains("API_KEY=[REDACTED]"))
            .last()
            .unwrap();
        let col = Span::raw(&line[..line.rfind("[REDACTED]").unwrap()]).width() as u16;
        let row = row as u16;
        for x in col..col + REDACTED.len() as u16 {
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
            source: "zsh",
            text: text.into(),
            revision_id: 1,
            ordinal: 0,
        };
        let page = SearchPage {
            schema: SEARCH_SCHEMA,
            query: "x".into(),
            limit: 200,
            truncated: false,
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
            safe_hash: vec![],
            keyed_fingerprint: vec![],
            flags: 0,
            provenance: vec![],
            locator: vec![],
        };
        let view = EvidenceView {
            observation: o.clone(),
            timeline: vec![o],
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
        drive(&mut m, &store, UiAction::Key(Key::Ctrl('r')));
        let s = text(&render(&m, 120, 40));
        assert!(
            s.contains("Recall uses a model and may spend up to $1 / 8 root + 16"),
            "{s}"
        );
        assert!(s.contains("Start? y/n"), "{s}");

        drive(&mut m, &store, UiAction::Key(Key::Esc));
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
            "wall 2.3s/60s",
            "root 2/8",
            "child 3/16",
            "done plan → 2 searches",
            "run  assess batch 1/3",
            "recall: running",
        ] {
            assert!(s.contains(want), "missing {want:?} in\n{s}");
        }

        // Longer than the pane is wide: the claim must wrap, not be cut.
        let claim =
            "you renewed the certificate with certbot after the reload; claim-end".to_string();
        let event = RecallEvent::Finished {
            claim,
            cites: vec![cite],
        };
        drive(&mut m, &store, UiAction::Recall { op, event });
        let s = text(&render(&m, 120, 40));
        for want in [
            &format!("finding (cites #{cite})"),
            "status: finished",
            "claim-end",
        ] {
            assert!(s.contains(want), "missing {want:?} in\n{s}");
        }
    }

    #[test]
    fn a_long_recall_keeps_its_newest_steps_on_screen() {
        let store = store_with(NGINX);
        let mut m = typed(&store, "certb", false);
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
