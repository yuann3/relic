//! Hand-rolled argv parsing. A parser crate is a startup/size decision left to
//! the architecture lock; this keeps `--help` free of any dependency cost.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::import::SourceKind;

// `concat!` takes only literals, so the `ui` line is spliced in by a macro:
// a headless build must not advertise a command it cannot run.
macro_rules! usage {
    ($ui:literal) => {
        concat!(
            "\
relic — recall what you did in the terminal, with evidence

USAGE:
    relic search <QUERY>... [--db PATH] [--limit N] [--prefix] [--rank] [--json | --fzf]
    relic recall <QUESTION>... [--db PATH] [--json] [--allow-unknown-cost]
",
            $ui,
            "    relic import <HISTORY_FILE> [--shell zsh|bash] [--db PATH]
    relic forget <ID>... [--db PATH]
    relic model check
    relic --help | --version

OPTIONS:
    --db PATH     Database file. Default: $RELIC_DB, else
                  $XDG_DATA_HOME/relic/relic.db (~/.local/share/relic/relic.db)
    --limit N     Maximum results (default 20, or 5000 with --fzf; capped at 5000)
    --prefix      Let the last word match as a prefix, as the TUI does
    --rank        Order by relevance. Default: most recently imported first,
                  which is import order, not the time a command ran
    --json        Print one relic.search/v1 document on stdout
    --fzf         Pick one result with fzf and show its evidence
    recall        Answer a question from your history with a model, citing
                  the records it rests on. --json prints one relic.recall/v1
                  document. Ctrl-C cancels it
    --allow-unknown-cost
                  Let recall use a remote model with no prices set, whose
                  cost it then cannot cap. Calls, bytes, tokens and time
                  stay capped
    forget        Delete records for good, by the id search --json and the TUI
                  show. Remove the line from your history file too, or a later
                  import of a rewritten file can store it again
    --shell       History format for import. Default: what the file was imported
                  as before, else what its content or name shows. A --shell
                  that differs from the stored one replaces that file's records

MODEL:
    recall sends the model your question and the commands it searches for,
    redacted, and prints each request's size and destination on stderr.
    model check sends a synthetic request, never your history, and prints
    a relic.model-check/v1 receipt. Configure the model with:
    RELIC_MODEL_PROFILE   openai (default), vllm, llama_cpp or custom
    RELIC_MODEL_URL       Base URL up to the API version. Default for openai:
                          https://api.openai.com/v1. Plain http only to a
                          loopback address such as 127.0.0.1
    RELIC_MODEL_NAME      The model to ask for
    RELIC_MODEL_API_KEY   Bearer key; required for openai
    RELIC_MODEL_OUTPUT, RELIC_MODEL_TOKEN_FIELD
                          custom only: json_schema, flat_json_schema or
                          json_object; max_completion_tokens or max_tokens
    RELIC_MODEL_PRICE_IN, RELIC_MODEL_PRICE_OUT
                          US dollars per million tokens, to fence cost.
                          A plain-http loopback model costs nothing; a
                          remote one without prices needs
                          --allow-unknown-cost

EXIT STATUS:
    0             Success; search found a match; recall answered, fully or
                  in part
    1             Search found no match, nothing matched in fzf, or recall
                  found nothing it could cite
    2             Error, including bad arguments, a busy store, a query
                  with nothing to search for, and a recall that failed or
                  ran out of budget
    130           The fzf picker or a recall was cancelled
    128+n         Killed by signal n
"
        )
    };
}

#[cfg(feature = "tui")]
pub const USAGE: &str = usage!("    relic ui [--db PATH]\n");
#[cfg(not(feature = "tui"))]
pub const USAGE: &str = usage!("");

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Search(SearchArgs),
    Recall { question: String, db: Option<PathBuf>, json: bool, allow_unknown_cost: bool },
    Ui { db: Option<PathBuf> },
    Import { path: PathBuf, shell: Option<SourceKind>, db: Option<PathBuf> },
    Forget { ids: Vec<i64>, db: Option<PathBuf> },
    ModelCheck,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SearchArgs {
    pub query: String,
    pub db: Option<PathBuf>,
    /// None means the output's default page size.
    pub limit: Option<usize>,
    pub prefix: bool,
    pub rank: bool,
    pub output: Output,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Output {
    Text,
    Json,
    Fzf,
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.into_iter().map(|a| {
        a.into_string().map_err(|a| format!("argument is not valid UTF-8: {a:?}"))
    });
    let Some(sub) = args.next().transpose()? else {
        return Ok(Command::Help);
    };
    match sub.as_str() {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "-V" | "--version" => Ok(Command::Version),
        "search" => parse_search(args),
        "recall" => {
            let (mut words, mut db, mut json, mut options_done) = (Vec::new(), None, false, false);
            let mut allow_unknown_cost = false;
            while let Some(a) = args.next().transpose()? {
                match a.as_str() {
                    _ if options_done => words.push(a),
                    "--" => options_done = true,
                    "-h" | "--help" => return Ok(Command::Help),
                    "--db" => db = Some(value(&mut args, "--db")?.into()),
                    "--json" => json = true,
                    "--allow-unknown-cost" => allow_unknown_cost = true,
                    f if f.starts_with("--") => return Err(format!("unknown option: {f}")),
                    _ => words.push(a),
                }
            }
            if words.is_empty() {
                return Err("recall needs a question".into());
            }
            Ok(Command::Recall { question: words.join(" "), db, json, allow_unknown_cost })
        }
        "ui" => {
            let mut db = None;
            while let Some(a) = args.next().transpose()? {
                match a.as_str() {
                    "-h" | "--help" => return Ok(Command::Help),
                    "--db" => db = Some(value(&mut args, "--db")?.into()),
                    _ => return Err(format!("unexpected argument for ui: {a}")),
                }
            }
            Ok(Command::Ui { db })
        }
        "import" => {
            let (mut path, mut shell, mut db) = (None, None, None);
            while let Some(a) = args.next().transpose()? {
                match a.as_str() {
                    "-h" | "--help" => return Ok(Command::Help),
                    "--db" => db = Some(value(&mut args, "--db")?.into()),
                    "--shell" => {
                        shell = Some(match value(&mut args, "--shell")?.as_str() {
                            "zsh" => SourceKind::Zsh,
                            "bash" => SourceKind::Bash,
                            other => return Err(format!("unknown shell: {other}")),
                        })
                    }
                    f if f.starts_with("--") => return Err(format!("unknown option: {f}")),
                    _ if path.is_none() => path = Some(PathBuf::from(a)),
                    _ => return Err(format!("unexpected argument for import: {a}")),
                }
            }
            let path = path.ok_or("import needs a history file")?;
            Ok(Command::Import { path, shell, db })
        }
        "model" => match (args.next().transpose()?.as_deref(), args.next().transpose()?.as_deref()) {
            (Some("-h" | "--help"), _) | (Some("check"), Some("-h" | "--help")) => Ok(Command::Help),
            (Some("check"), None) => Ok(Command::ModelCheck),
            (Some("check"), Some(a)) => Err(format!("unexpected argument for model check: {a}")),
            (Some(sub), _) => Err(format!("unknown model command: {sub}; try model check")),
            (None, _) => Err("model needs a command: model check".into()),
        },
        "forget" => {
            let (mut ids, mut db) = (Vec::new(), None);
            while let Some(a) = args.next().transpose()? {
                match a.as_str() {
                    "-h" | "--help" => return Ok(Command::Help),
                    "--db" => db = Some(value(&mut args, "--db")?.into()),
                    f if f.starts_with("--") => return Err(format!("unknown option: {f}")),
                    // As the TUI shows it (`#42`) or bare.
                    id => ids.push(
                        id.trim_start_matches('#')
                            .parse()
                            .map_err(|_| format!("not a record id: {id}"))?,
                    ),
                }
            }
            if ids.is_empty() {
                return Err("forget needs at least one record id".into());
            }
            Ok(Command::Forget { ids, db })
        }
        other => Err(format!("unknown command: {other}")),
    }
}

fn parse_search(
    mut args: impl Iterator<Item = Result<String, String>>,
) -> Result<Command, String> {
    let (mut words, mut db, mut limit, mut prefix) = (Vec::new(), None, None, false);
    let (mut json, mut fzf, mut rank) = (false, false, false);
    let mut options_done = false;
    while let Some(a) = args.next().transpose()? {
        match a.as_str() {
            _ if options_done => words.push(a),
            "--" => options_done = true,
            "-h" | "--help" => return Ok(Command::Help),
            "--db" => db = Some(value(&mut args, "--db")?.into()),
            "--limit" => {
                let n: usize = value(&mut args, "--limit")?
                    .parse()
                    .map_err(|_| "--limit needs a positive number".to_string())?;
                if n == 0 {
                    return Err("--limit needs a positive number".into());
                }
                limit = Some(n);
            }
            "--prefix" => prefix = true,
            "--rank" => rank = true,
            "--json" => json = true,
            "--fzf" => fzf = true,
            f if f.starts_with("--") => return Err(format!("unknown option: {f}")),
            _ => words.push(a),
        }
    }
    if words.is_empty() {
        return Err("search needs a query".into());
    }
    let output = match (json, fzf) {
        (true, true) => return Err("--json and --fzf cannot be combined".into()),
        (true, false) => Output::Json,
        (false, true) => Output::Fzf,
        (false, false) => Output::Text,
    };
    Ok(Command::Search(SearchArgs { query: words.join(" "), db, limit, prefix, rank, output }))
}

fn value(
    args: &mut impl Iterator<Item = Result<String, String>>,
    flag: &str,
) -> Result<String, String> {
    args.next().transpose()?.ok_or_else(|| format!("{flag} needs a value"))
}

/// Whether the database sits in Relic's own data directory, which Relic may
/// then keep owner-only. A `--db` or `$RELIC_DB` path can be in any directory,
/// and that directory is not Relic's to change.
pub fn owns_data_dir(explicit: &Option<PathBuf>) -> bool {
    explicit.is_none() && std::env::var_os("RELIC_DB").is_none_or(|v| v.is_empty())
}

/// `--db`, else `$RELIC_DB`, else the XDG data directory.
pub fn resolve_db(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    resolve_db_from(explicit, |k| std::env::var_os(k))
}

fn resolve_db_from(
    explicit: Option<PathBuf>,
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, String> {
    let nonempty = |k| env(k).filter(|v: &OsString| !v.is_empty());
    if let Some(p) = explicit.or_else(|| nonempty("RELIC_DB").map(PathBuf::from)) {
        return Ok(p);
    }
    // The XDG spec says to ignore a relative value; honouring it would put the
    // database wherever the command happened to run.
    let data = nonempty("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| nonempty("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok_or("cannot locate a database: pass --db or set RELIC_DB")?;
    Ok(data.join("relic").join("relic.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Command, String> {
        parse(args.iter().map(OsString::from))
    }

    fn search(args: &[&str]) -> SearchArgs {
        match p(args).unwrap() {
            Command::Search(s) => s,
            other => panic!("expected search, got {other:?}"),
        }
    }

    #[test]
    fn no_args_and_help_flags_show_help() {
        assert_eq!(p(&[]).unwrap(), Command::Help);
        assert_eq!(p(&["-h"]).unwrap(), Command::Help);
        assert_eq!(p(&["--help"]).unwrap(), Command::Help);
        assert_eq!(p(&["--version"]).unwrap(), Command::Version);
    }

    #[test]
    fn usage_lists_ui_only_when_built_with_it() {
        assert_eq!(USAGE.contains("relic ui"), cfg!(feature = "tui"));
    }

    #[test]
    fn subcommands_accept_help_flags() {
        for args in [
            &["ui", "--help"][..],
            &["ui", "--db", "x.db", "-h"],
            &["import", "--help"],
            &["import", "h.txt", "-h"],
            &["forget", "--help"],
            &["forget", "7", "-h"],
            &["search", "--help"],
            &["search", "git", "-h", "--db", "x.db"],
            &["recall", "why", "--help"],
        ] {
            assert_eq!(p(args), Ok(Command::Help), "{args:?}");
        }
        // After `--` it is a query word like any other.
        assert_eq!(search(&["search", "--", "-h"]).query, "-h");
    }

    #[test]
    fn search_joins_words_and_reads_options() {
        let s = search(&["search", "certbot", "--limit", "5", "renew", "--json", "--db", "x.db"]);
        assert_eq!(s.query, "certbot renew");
        assert_eq!(s.limit, Some(5));
        assert_eq!(search(&["search", "x"]).limit, None);
        assert_eq!(s.output, Output::Json);
        assert_eq!(s.db, Some(PathBuf::from("x.db")));
        assert!(!s.prefix && !s.rank);
        assert!(search(&["search", "x", "--rank"]).rank);
    }

    #[test]
    fn double_dash_lets_a_query_start_with_dashes() {
        assert_eq!(search(&["search", "--", "--force", "push"]).query, "--force push");
    }

    #[test]
    fn search_rejects_bad_input() {
        assert!(p(&["search"]).is_err());
        assert!(p(&["search", "x", "--json", "--fzf"]).is_err());
        assert!(p(&["search", "x", "--limit", "0"]).is_err());
        assert!(p(&["search", "x", "--limit"]).is_err());
        assert!(p(&["search", "x", "--bogus"]).is_err());
        assert!(p(&["frobnicate"]).is_err());
    }

    #[test]
    fn import_defaults_to_zsh() {
        let cmd = p(&["import", "h.txt", "--shell", "bash"]).unwrap();
        assert_eq!(
            cmd,
            Command::Import { path: "h.txt".into(), shell: Some(SourceKind::Bash), db: None }
        );
        assert!(matches!(p(&["import", "h"]).unwrap(), Command::Import { shell: None, .. }));
        assert!(p(&["import"]).is_err());
        assert!(p(&["import", "h", "--shell", "fish"]).is_err());
        assert_eq!(p(&["forget", "#42", "7"]).unwrap(), Command::Forget { ids: vec![42, 7], db: None });
        assert!(p(&["forget"]).is_err());
        assert_eq!(p(&["model", "check"]).unwrap(), Command::ModelCheck);
        assert_eq!(p(&["model", "check", "--help"]).unwrap(), Command::Help);
        assert!(p(&["model"]).is_err() && p(&["model", "chek"]).is_err() && p(&["model", "check", "x"]).is_err());
        assert!(p(&["forget", "all"]).is_err());
        assert_eq!(
            p(&["recall", "how", "--json", "did", "--", "--db", "x"]).unwrap(),
            Command::Recall { question: "how did --db x".into(), db: None, json: true, allow_unknown_cost: false }
        );
        assert!(p(&["recall"]).is_err() && p(&["recall", "--json"]).is_err() && p(&["recall", "x", "--fzf"]).is_err());
    }

    #[test]
    fn db_resolution_order() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| OsString::from(v))
        };
        let all = env(&[("RELIC_DB", "/r.db"), ("XDG_DATA_HOME", "/x"), ("HOME", "/h")]);
        assert_eq!(resolve_db_from(Some("/e.db".into()), all).unwrap(), PathBuf::from("/e.db"));
        assert_eq!(resolve_db_from(None, all).unwrap(), PathBuf::from("/r.db"));
        let xdg = env(&[("RELIC_DB", ""), ("XDG_DATA_HOME", "/x"), ("HOME", "/h")]);
        assert_eq!(resolve_db_from(None, xdg).unwrap(), PathBuf::from("/x/relic/relic.db"));
        let home = env(&[("HOME", "/h")]);
        assert_eq!(
            resolve_db_from(None, home).unwrap(),
            PathBuf::from("/h/.local/share/relic/relic.db")
        );
        assert!(resolve_db_from(None, env(&[])).is_err());
        let relative = env(&[("XDG_DATA_HOME", "rel"), ("HOME", "/h")]);
        assert_eq!(resolve_db_from(None, relative).unwrap(), PathBuf::from("/h/.local/share/relic/relic.db"));
        assert!(resolve_db_from(None, env(&[("XDG_DATA_HOME", "rel")])).is_err());
    }
}
