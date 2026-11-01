use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use relic::app::{self, Budget, MatchMode, Order, SearchRequest};
use relic::cli::{self, Command, Output, SearchArgs};
use relic::import::{History, ImportError, ImportResult, Outcome, SourceKind};
use relic::picker::{Pick, PickError, PickMode};
use relic::store::Store;
use serde::Serialize;

fn main() -> ExitCode {
    // Before anything is created: the database, its WAL, the key and any file
    // a later command writes must never be readable by another user.
    // SAFETY: umask only swaps the process's file mode mask; no thread exists yet.
    unsafe { libc::umask(0o077) };
    let cmd = match cli::parse(std::env::args_os().skip(1)) {
        Ok(cmd) => cmd,
        Err(e) => {
            // Parse errors quote argv, which is as hostile as history text.
            eprintln!("relic: {}\n\n{}", app::display_line(&e), cli::USAGE);
            return ExitCode::from(2);
        }
    };
    match run(cmd) {
        Ok(code) => code,
        // `relic search … | head` closing the pipe early is not a failure.
        Err(e) if broken_pipe(e.as_ref()) => ExitCode::SUCCESS,
        // Exit 1 is "found nothing"; any error is "could not look" (ADR 0011).
        Err(e) => could_not_look(e.as_ref()),
    }
}

fn broken_pipe(e: &(dyn std::error::Error + 'static)) -> bool {
    let kind = e
        .downcast_ref::<std::io::Error>()
        .map(std::io::Error::kind)
        .or_else(|| e.downcast_ref::<serde_json::Error>().and_then(serde_json::Error::io_error_kind));
    kind == Some(ErrorKind::BrokenPipe)
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn run(cmd: Command) -> Result<ExitCode> {
    match cmd {
        // `print!` panics on a closed pipe; a write error reaches `broken_pipe`.
        Command::Help => std::io::stdout().lock().write_all(cli::USAGE.as_bytes())?,
        Command::Version => {
            // The gate harness reads the build from here: nothing else tells a
            // headless, full or debug binary apart from outside.
            let build = if cfg!(feature = "tui") { "tui" } else { "headless" };
            let debug = if cfg!(debug_assertions) { ", debug" } else { "" };
            writeln!(std::io::stdout().lock(), "relic {} ({build}{debug})", env!("CARGO_PKG_VERSION"))?;
        }
        Command::Search(args) => return search(args),
        Command::Recall { question, db, json, allow_unknown_cost } => {
            return recall(&question, db, json, allow_unknown_cost);
        }
        Command::Ui { db } => ui(&existing_db(db)?)?,
        Command::ModelCheck => {
            use relic::rlm::model::chat::{self, ChatCompletionsClient};
            let client = ChatCompletionsClient::from_env()?;
            let (receipt, verdict) = chat::check(&client);
            // Terminal-safe: the receipt quotes the endpoint's error text.
            let mut out = std::io::stdout().lock();
            receipt.serialize(&mut serde_json::Serializer::with_formatter(&mut out, TerminalSafe))?;
            writeln!(out)?;
            verdict.map_err(|e| format!("model check failed: {e}"))?;
        }
        Command::Forget { ids, db } => {
            let owns_dir = cli::owns_data_dir(&db);
            let db = existing_db(db)?;
            tighten(&db, owns_dir);
            let (forgotten, scrubbed) = Store::open(&db)?.forget(&ids)?;
            for id in &forgotten {
                eprintln!("relic: forgot record #{id}");
            }
            if !scrubbed {
                eprintln!("{SCRUB_BLOCKED}");
            }
            let unknown: Vec<String> =
                ids.iter().filter(|id| !forgotten.contains(id)).map(|id| format!("#{id}")).collect();
            if !unknown.is_empty() {
                return Err(format!("no such record: {}", unknown.join(" ")).into());
            }
        }
        Command::Import { path, shell, db } => {
            let owns_dir = cli::owns_data_dir(&db);
            let db = cli::resolve_db(db)?;
            // The file before the store: an import that fails on the file
            // alone must not leave a database behind.
            let history = History::open(&path)?;
            if shell.is_none() && !db.exists() && history.detected()?.is_none() {
                return Err(ImportError::UnknownShell(path).into());
            }
            if let Some(dir) = db.parent() {
                std::fs::create_dir_all(dir)?;
            }
            tighten(&db, owns_dir);
            let r = match import(&db, &history, shell) {
                Err(ImportError::Busy) => {
                    eprintln!("relic: busy: another import is writing {}; try again", shown(&db));
                    return Ok(ExitCode::from(2));
                }
                r => r?,
            };
            let (name, n) = (shown(&path), r.records_stored);
            if r.rescrubbed > 0 {
                eprintln!(
                    "relic: redaction rules changed since they were stored: re-redacted {} stored commands",
                    r.rescrubbed
                );
            }
            if let Some((was, removed)) = r.replaced {
                eprintln!(
                    "relic: {name}: was imported as {was}; replaced its {removed} stored commands with a {} import",
                    r.kind
                );
            }
            if r.scrub_blocked {
                eprintln!("{SCRUB_BLOCKED}");
            }
            if r.checkpoint_blocked {
                eprintln!("relic: note: WAL not checkpointed: a reader is active; the next large import retries");
            }
            if r.lossy_records > 0 {
                eprintln!("relic: warning: {name}: {} commands stored lossy (cut, or not decodable)", r.lossy_records);
            }
            if r.dangling_meta > 0 {
                eprintln!("relic: warning: {name}: {} dangling zsh Meta bytes", r.dangling_meta);
            }
            if r.incomplete_tail {
                eprintln!("relic: note: {name}: the last record is incomplete and was not stored");
            }
            if let Some(warning) = &r.key_warning {
                eprintln!("relic: note: {}", app::display_line(warning));
            }
            match r.outcome {
                Outcome::Unchanged => eprintln!("{name}: unchanged since last import"),
                Outcome::First => eprintln!("{name}: stored {n} {} commands in {}", r.kind, shown(&db)),
                Outcome::Appended => eprintln!("{name}: added {n} new commands"),
                Outcome::Aligned => {
                    eprintln!("{name}: the file was rewritten; added {n} new commands and kept the earlier ones")
                }
                Outcome::Superseded => eprintln!(
                    "{name}: the file was rewritten beyond what relic could align; stored it whole ({n} commands) as a new revision and kept the earlier ones"
                ),
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

const SCRUB_BLOCKED: &str = "relic: warning: another relic is reading the store, so the removed text is still \
    on disk in its WAL; close it and run any import or forget to finish";

/// Make what a write open touches owner-only: files created before Relic set
/// its umask, or by another tool, may be readable by others. Only warns: a
/// store Relic cannot fix is still the user's to use (ADR 0003).
fn tighten(db: &Path, owns_dir: bool) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let side = |suffix: &str| {
        let mut path = db.as_os_str().to_owned();
        path.push(suffix);
        PathBuf::from(path)
    };
    let dir = db.parent().filter(|_| owns_dir).map(|dir| (dir.to_path_buf(), 0o700));
    let files = ["", "-wal", "-shm", ".key"].map(|suffix| (side(suffix), 0o600));
    // SAFETY: getuid has no preconditions and cannot fail.
    let me = unsafe { libc::getuid() };
    for (path, mode) in dir.into_iter().chain(files) {
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if meta.uid() != me {
            eprintln!("relic: warning: {} is owned by another user", shown(&path));
        } else if meta.mode() & 0o077 != 0
            && let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
        {
            eprintln!("relic: warning: could not make {} owner-only: {e}", shown(&path));
        }
    }
}

fn import(
    db: &Path,
    history: &History,
    shell: Option<SourceKind>,
) -> std::result::Result<ImportResult, ImportError> {
    let mut store = Store::open(db)?;
    relic::import::import(&mut store, history, shell, &relic::redact::Redactor::new())
}

fn search(args: SearchArgs) -> Result<ExitCode> {
    let store = Store::open_read_only(&existing_db(args.db)?)?;
    let mode = if args.prefix { MatchMode::PrefixLast } else { MatchMode::Words };
    let order = if args.rank { Order::Rank } else { Order::RecentlyImported };
    // fzf is for narrowing a broad page; 20 rows would defeat it.
    let default_limit = if args.output == Output::Fzf { app::MAX_PAGE } else { 20 };
    let limit = args.limit.unwrap_or(default_limit);
    let req = SearchRequest { query: args.query, limit, mode, order };
    let page = app::search(&store, &req, &Budget::start())?;
    // Recent commands are what a typing surface shows; to a script they would
    // pass for matches. A query that searched nothing is a usage error.
    if !page.filtered {
        let why = match &page.deferred {
            Some(w) if w.chars().any(char::is_alphanumeric) => format!(
                "\"{}\" is too short; type {}+ characters",
                app::display_line(w),
                app::MIN_PREFIX_CHARS
            ),
            _ if page.words_dropped > 0 => format!(
                "every word was ignored: a query uses at most {} words of at most {} parts each",
                app::MAX_QUERY_WORDS,
                app::MAX_WORD_TOKENS
            ),
            _ => "search matches words of letters or digits".into(),
        };
        return Err(format!("nothing to search for in \"{}\": {why}", app::display_line(&page.query)).into());
    }
    let mut out = std::io::stdout().lock();
    match args.output {
        Output::Json => {
            page.serialize(&mut serde_json::Serializer::with_formatter(&mut out, TerminalSafe))?;
            writeln!(out)?;
        }
        Output::Text => print_page(&mut out, &page)?,
        Output::Fzf => {
            // fzf draws on /dev/tty itself; our stdio may be redirected.
            if std::fs::File::open("/dev/tty").is_err() {
                return Err("--fzf needs an interactive terminal".into());
            }
            if page.hits.is_empty() {
                eprintln!("no matches");
                return Ok(ExitCode::FAILURE);
            }
            page_notes(&page);
            relic::signal::install();
            let id = match relic::picker::pick(&page, PickMode::Inline) {
                Ok(Pick::Chosen(id)) => id,
                Ok(Pick::NoMatch) => return Ok(ExitCode::FAILURE),
                Ok(Pick::Cancelled) => return Ok(ExitCode::from(130)),
                // The picker is optional: indexed results are still complete.
                Err(e @ (PickError::Unavailable | PickError::TooOld { .. })) => {
                    eprintln!("relic: {e}");
                    print_page(&mut out, &page)?;
                    return Ok(ExitCode::SUCCESS);
                }
                Err(e) => return Err(e.into()),
            };
            let view = app::open_evidence(&store, id, 3)?.ok_or("selected evidence vanished")?;
            for o in &view.timeline {
                let mark = if o.id == id { ">" } else { " " };
                writeln!(out, "{mark} {} {}", view.when(o).label(), app::display_line(&o.safe_text))?;
            }
        }
    }
    if page.hits.is_empty() {
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

fn recall(question: &str, db: Option<PathBuf>, json: bool, allow_unknown_cost: bool) -> Result<ExitCode> {
    use relic::app::RecallOutcome;
    use relic::rlm::model::ModelBoundary;
    let db = existing_db(db)?;
    let store = Store::open_read_only(&db)?;
    let model = Arc::new(relic::rlm::model::chat::ChatCompletionsClient::from_env()?);
    // The $1 cap fences nothing without prices (research, budget and cost lease).
    if model.prices().is_none() && !allow_unknown_cost {
        return Err("this model has no prices, so recall cannot cap what it spends: set RELIC_MODEL_PRICE_IN \
                    and RELIC_MODEL_PRICE_OUT, or pass --allow-unknown-cost"
            .into());
    }
    let cancel = Arc::new(AtomicBool::new(false));
    relic::signal::interrupt_cancels(Arc::clone(&cancel));
    // Every request's size and destination is the payload audit (ADR 0004).
    let report = app::recall(question, &store, model, &app::recall_log_path(&db), &cancel, &mut |p| {
        eprintln!("relic: {}", app::display_line(&p.to_string()))
    });
    let mut out = std::io::stdout().lock();
    if json {
        report.serialize(&mut serde_json::Serializer::with_formatter(&mut out, TerminalSafe))?;
        writeln!(out)?;
    } else {
        print_recall(&mut out, &report)?;
    }
    Ok(match report.outcome {
        RecallOutcome::Answered | RecallOutcome::Partial => ExitCode::SUCCESS,
        RecallOutcome::Abstained => ExitCode::FAILURE,
        RecallOutcome::Cancelled => {
            eprintln!("relic: recall cancelled");
            ExitCode::from(130)
        }
        RecallOutcome::BudgetExhausted => {
            eprintln!("relic: recall ran out of budget before it could answer");
            ExitCode::from(2)
        }
        RecallOutcome::Failed => {
            let why = report.error.as_deref().unwrap_or("unknown");
            eprintln!("relic: recall failed: {}", app::display_line(why));
            ExitCode::from(2)
        }
    })
}

/// Model text is as hostile as history text: every line through
/// `display_line`.
fn print_recall(out: &mut impl Write, r: &app::RecallReport) -> Result<()> {
    let cites = |ids: &[i64]| ids.iter().map(|id| format!("#{id}")).collect::<Vec<_>>().join(", ");
    let outcome = match r.outcome {
        app::RecallOutcome::Answered => "answered",
        app::RecallOutcome::Partial => "partly answered",
        app::RecallOutcome::Abstained => "no answer",
        app::RecallOutcome::BudgetExhausted => "out of budget",
        app::RecallOutcome::Cancelled => "cancelled",
        app::RecallOutcome::Failed => "failed",
    };
    writeln!(out, "{outcome}: {}", app::display_line(&r.answer))?;
    if !r.claims.is_empty() {
        writeln!(out, "claims:")?;
    }
    for c in &r.claims {
        let against = if c.contradicts.is_empty() { String::new() } else { format!(", against {}", cites(&c.contradicts)) };
        writeln!(out, "  - {} [{}{against}]", app::display_line(&c.text), cites(&c.supports))?;
    }
    if !r.procedure.is_empty() {
        writeln!(out, "procedure:")?;
    }
    for (i, s) in r.procedure.iter().enumerate() {
        writeln!(out, "  {}. {} [{}]", i + 1, app::display_line(&s.text), cites(&s.supports))?;
    }
    for (label, items) in [("contradictions", &r.contradictions), ("missing", &r.missing)] {
        if !items.is_empty() {
            writeln!(out, "{label}:")?;
        }
        for m in items {
            writeln!(out, "  - {}", app::display_line(m))?;
        }
    }
    if !r.evidence.is_empty() {
        writeln!(out, "evidence:")?;
    }
    for e in &r.evidence {
        writeln!(out, "  #{:<6} {} {:<4}  {}", e.id, e.when().label(), e.source, app::display_line(&e.text))?;
    }
    let cost = r.cost_microusd.map_or("an unknown cost".into(), |c| format!("at most ${:.4}", c as f64 / 1e6));
    let by = if r.models.is_empty() { String::new() } else { format!(", answered by {}", r.models.join(", ")) };
    writeln!(out, "spent: {} root + {} child calls, {cost}{}", r.root_calls, r.child_calls, app::display_line(&by))?;
    Ok(())
}

/// "Could not look" is not "nothing found": exit 2, not 1 (ADR 0011).
fn could_not_look(e: &dyn std::error::Error) -> ExitCode {
    // Errors carry paths and argv; neither may drive the terminal.
    eprintln!("relic: {}", app::display_line(&e.to_string()));
    ExitCode::from(2)
}

/// A path as it may reach the terminal.
fn shown(path: &Path) -> String {
    app::display_line(&path.display().to_string())
}

/// serde_json escapes only what JSON requires, so C1 controls, bidi overrides
/// and line separators would reach a terminal that shows the output raw.
/// Escaping whatever `display_char` neutralizes keeps the decoded text exact.
struct TerminalSafe;

impl serde_json::ser::Formatter for TerminalSafe {
    fn write_string_fragment<W: ?Sized + Write>(&mut self, w: &mut W, fragment: &str) -> std::io::Result<()> {
        let mut start = 0;
        for (i, c) in fragment.char_indices() {
            if app::display_char(c) == c {
                continue;
            }
            w.write_all(&fragment.as_bytes()[start..i])?;
            for unit in c.encode_utf16(&mut [0; 2]) {
                write!(w, "\\u{unit:04x}")?;
            }
            start = i + c.len_utf8();
        }
        w.write_all(&fragment.as_bytes()[start..])
    }
}

fn print_page(out: &mut impl Write, page: &app::SearchPage) -> Result<()> {
    for h in &page.hits {
        writeln!(out, "{} {:<4}  {}", h.when().label(), h.source, app::display_line(&h.text))?;
    }
    page_notes(page);
    Ok(())
}

/// What the page left out, on stderr so stdout stays the results.
fn page_notes(page: &app::SearchPage) {
    if let Some(w) = &page.deferred {
        eprintln!(
            "(\"{}\" is too short to filter by; type {}+ characters)",
            app::display_line(w),
            app::MIN_PREFIX_CHARS
        );
    }
    if page.words_dropped > 0 {
        eprintln!(
            "({} word{} ignored: a query uses at most {} words of at most {} parts each)",
            page.words_dropped,
            if page.words_dropped == 1 { "" } else { "s" },
            app::MAX_QUERY_WORDS,
            app::MAX_WORD_TOKENS
        );
    }
    if page.truncated {
        eprintln!("(more matches past {}; narrow the query or raise --limit)", page.limit);
    }
}

/// Search and the UI must not create an empty database as a side effect.
fn existing_db(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let db = cli::resolve_db(explicit)?;
    if !db.exists() {
        return Err(format!(
            "no database at {}; run `relic import ~/.zsh_history` first or pass --db",
            db.display()
        )
        .into());
    }
    Ok(db)
}

#[cfg(feature = "tui")]
fn ui(db: &Path) -> Result<()> {
    relic::ui::run(db)
}

#[cfg(not(feature = "tui"))]
fn ui(_db: &Path) -> Result<()> {
    Err("this relic was built without the TUI (feature `tui`)".into())
}
