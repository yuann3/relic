use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use relic::app::{self, MatchMode, SearchRequest};
use relic::cli::{self, Command, Output, SearchArgs};
use relic::import::{ImportError, ImportResult, SourceKind, import_file};
use relic::picker::PickError;
use relic::store::{Store, StoreError};

fn main() -> ExitCode {
    let cmd = match cli::parse(std::env::args_os().skip(1)) {
        Ok(cmd) => cmd,
        Err(e) => {
            eprintln!("relic: {e}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    match run(cmd) {
        Ok(code) => code,
        // `relic search … | head` closing the pipe early is not a failure.
        Err(e) if broken_pipe(e.as_ref()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("relic: {e}");
            ExitCode::FAILURE
        }
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
        Command::Help => print!("{}", cli::USAGE),
        Command::Version => {
            // The gate harness reads the build from here: nothing else tells a
            // headless, full or debug binary apart from outside.
            let build = if cfg!(feature = "tui") { "tui" } else { "headless" };
            let debug = if cfg!(debug_assertions) { ", debug" } else { "" };
            println!("relic {} ({build}{debug})", env!("CARGO_PKG_VERSION"));
        }
        Command::Search(args) => return search(args),
        Command::Ui { db } => ui(&existing_db(db)?)?,
        Command::Import { path, shell, db } => {
            let db = cli::resolve_db(db)?;
            if let Some(dir) = db.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let r = match import(&db, &path, shell) {
                Err(ImportError::Busy) => {
                    eprintln!("relic: busy: another import is writing {}; try again", db.display());
                    return Ok(ExitCode::from(2));
                }
                r => r?,
            };
            if r.skipped {
                eprintln!("{}: unchanged since last import", path.display());
            } else {
                eprintln!("{}: stored {} commands in {}", path.display(), r.records_stored, db.display());
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn import(db: &Path, path: &Path, shell: SourceKind) -> std::result::Result<ImportResult, ImportError> {
    // Opening writes too, so a busy store can already refuse here.
    let mut store = Store::open(db).map_err(StoreError::from)?;
    import_file(&mut store, path, shell, &relic::redact::Redactor::new())
}

fn search(args: SearchArgs) -> Result<ExitCode> {
    let store = Store::open(&existing_db(args.db)?)?;
    let mode = if args.prefix { MatchMode::PrefixLast } else { MatchMode::Words };
    // fzf is for narrowing a broad page; 20 rows would defeat it.
    let default_limit = if args.output == Output::Fzf { app::MAX_PAGE } else { 20 };
    let limit = args.limit.unwrap_or(default_limit);
    let req = SearchRequest { query: args.query, limit, mode };
    let page = app::search(&store, &req)?;
    let mut out = std::io::stdout().lock();
    match args.output {
        Output::Json => {
            serde_json::to_writer(&mut out, &page)?;
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
            let id = match relic::picker::pick(&page) {
                Ok(Some(id)) => id,
                Ok(None) => return Ok(ExitCode::from(130)),
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
