use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use relic::app::{self, MatchMode, SearchRequest};
use relic::cli::{self, Command, Output, SearchArgs};
use relic::store::Store;

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
        Err(e) => {
            eprintln!("relic: {e}");
            ExitCode::FAILURE
        }
    }
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn run(cmd: Command) -> Result<ExitCode> {
    match cmd {
        Command::Help => print!("{}", cli::USAGE),
        Command::Version => println!("relic {}", env!("CARGO_PKG_VERSION")),
        Command::Search(args) => return search(args),
        Command::Ui { db } => ui(&existing_db(db)?)?,
        Command::Import { path, shell, db } => {
            let db = cli::resolve_db(db)?;
            if let Some(dir) = db.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut store = Store::open(&db)?;
            let r = relic::import::import_file(
                &mut store,
                &path,
                shell,
                &relic::redact::Redactor::new(),
            )?;
            if r.skipped {
                eprintln!("{}: unchanged since last import", path.display());
            } else {
                eprintln!("{}: stored {} commands in {}", path.display(), r.records_stored, db.display());
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn search(args: SearchArgs) -> Result<ExitCode> {
    let store = Store::open(&existing_db(args.db)?)?;
    let mode = if args.prefix { MatchMode::PrefixLast } else { MatchMode::Words };
    let req = SearchRequest { query: args.query, limit: args.limit, mode };
    let page = app::search(&store, &req)?;
    let mut out = std::io::stdout().lock();
    match args.output {
        Output::Json => {
            serde_json::to_writer(&mut out, &page)?;
            writeln!(out)?;
        }
        Output::Text => {
            for h in &page.hits {
                let when = h.observed_at_ms.map(app::format_utc).unwrap_or_else(|| "unknown time    ".into());
                writeln!(out, "{when}  {:<4}  {}", h.source, app::display_line(&h.text))?;
            }
            if page.truncated {
                eprintln!("(more matches past {}; narrow the query or raise --limit)", page.limit);
            }
        }
        Output::Fzf => {
            if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
                return Err("--fzf needs an interactive terminal".into());
            }
            if page.hits.is_empty() {
                eprintln!("no matches");
                return Ok(ExitCode::FAILURE);
            }
            let Some(id) = relic::picker::pick(&page)? else {
                return Ok(ExitCode::from(130));
            };
            let view = app::open_evidence(&store, id, 3)?.ok_or("selected evidence vanished")?;
            for o in &view.timeline {
                let mark = if o.id == id { ">" } else { " " };
                let when = o.observed_at_ms.map(app::format_utc).unwrap_or_default();
                writeln!(out, "{mark} {when}  {}", app::display_line(&o.safe_text))?;
            }
        }
    }
    if page.hits.is_empty() {
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
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
