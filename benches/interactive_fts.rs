//! Search-as-you-type FTS measurements for #10, feeding the #11 architecture
//! lock. Rebuilds the 1M diverse fixture under several FTS5 schema variants
//! and times every keystroke-shaped query with each ordering.
//!
//! Run with:
//!   cargo run --release --example fixture_db -- target/fixtures/1m.db 1000000
//!   cargo run --release --example interactive_fts -- target/fixtures/1m.db target/fts-variants [RUNS] [--hot]
//!
//! `--hot` runs only the high-frequency-token prefix study (`hot_study`),
//! which takes minutes instead of the full run's ~40.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use rusqlite::types::Value;
use rusqlite::{Connection, params_from_iter};

use relic::app::{self, MatchMode, SearchRequest};
use relic::fixture;
use relic::store::Store;

/// The TUI's page, and the page #8 measured.
const PAGES: [usize; 2] = [200, 50];

struct Query {
    text: &'static str,
    /// `MatchMode::PrefixLast` (what the TUI sends) versus `MatchMode::Words`.
    prefix: bool,
    class: &'static str,
}

const fn q(text: &'static str, prefix: bool, class: &'static str) -> Query {
    Query {
        text,
        prefix,
        class,
    }
}

/// Keystrokes on the way to "git com" and "cargo bu", plus the exact words #8
/// measured so the numbers line up with its 42-74 ms finding.
const QUERIES: &[Query] = &[
    q("g", true, "1-char"),
    q("c", true, "1-char"),
    q("d", true, "1-char"),
    q("gi", true, "2-char"),
    q("ca", true, "2-char"),
    q("do", true, "2-char"),
    q("ku", true, "2-char"),
    q("git", true, "3-char"),
    q("car", true, "3-char"),
    q("doc", true, "3-char"),
    q("cargo", true, "word*"),
    q("docker", true, "word*"),
    q("git", false, "word"),
    q("cargo", false, "word"),
    q("docker", false, "word"),
    q("git c", true, "two-word"),
    q("git co", true, "two-word"),
    q("git com", true, "two-word"),
    q("cargo bu", true, "two-word"),
    q("cargo re", true, "two-word"),
    // Two frequent words that rarely or never share a row: a rowid-ordered
    // walk cannot stop early, so this is its worst case.
    q("cargo com", true, "sparse AND"),
    q("docker git", true, "sparse AND"),
];

struct Schema {
    name: &'static str,
    file: &'static str,
    /// Extra fts5() arguments. Empty means the shipped schema, untouched.
    options: &'static str,
}

const SCHEMAS: &[Schema] = &[
    Schema {
        name: "V0 baseline",
        file: "baseline",
        options: "",
    },
    // Only 1-char prefixes are slow under rowid order on the baseline, so the
    // cheapest prefix index is worth its own row.
    Schema {
        name: "V3 prefix='1'",
        file: "prefix1",
        options: "prefix='1'",
    },
    Schema {
        name: "V3 prefix='1 2 3'",
        file: "prefix123",
        options: "prefix='1 2 3'",
    },
    Schema {
        name: "V3 prefix='2 3'",
        file: "prefix23",
        options: "prefix='2 3'",
    },
    Schema {
        name: "V4 detail=column",
        file: "detail_column",
        options: "detail=column",
    },
    Schema {
        name: "V4 detail=none",
        file: "detail_none",
        options: "detail=none",
    },
    Schema {
        name: "V3+V4 prefix='1' detail=none",
        file: "prefix1_none",
        options: "prefix='1', detail=none",
    },
    Schema {
        name: "V3+V4 prefix='1 2 3' detail=none",
        file: "prefix123_none",
        options: "prefix='1 2 3', detail=none",
    },
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    Rank,
    RowidDesc,
}

impl Order {
    fn label(self) -> &'static str {
        match self {
            Order::Rank => "rank",
            Order::RowidDesc => "rowid DESC",
        }
    }
}

/// Mirror of the private `store::escape_fts_query`: each word becomes a quoted
/// token and, in prefix mode, the last one gets `*`.
fn escape(query: &str, last_prefix: bool) -> String {
    let tokens: Vec<&str> = query.split_whitespace().collect();
    let last = tokens.len().saturating_sub(1);
    tokens
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let escaped = t.replace('"', "\"\"");
            let star = if last_prefix && i == last { "*" } else { "" };
            format!("\"{escaped}\"{star}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `?1` = escaped MATCH, `?2` = limit, `?3` = exclusive rowid lower bound.
fn fts_sql(order: Order, ranged: bool) -> String {
    let range = if ranged { " AND rowid > ?3" } else { "" };
    let order = match order {
        Order::Rank => "rank, rowid DESC",
        Order::RowidDesc => "rowid DESC",
    };
    format!(
        "SELECT rowid FROM observation_fts WHERE observation_fts MATCH ?1{range} \
         ORDER BY {order} LIMIT ?2"
    )
}

fn fts_params(escaped: &str, limit: usize, lower: Option<i64>) -> Vec<Value> {
    let mut p = vec![
        Value::Text(escaped.to_owned()),
        Value::Integer(limit as i64),
    ];
    if let Some(lo) = lower {
        p.push(Value::Integer(lo));
    }
    p
}

fn fts_rowids(conn: &Connection, sql: &str, params: &[Value]) -> Result<Vec<i64>, rusqlite::Error> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params_from_iter(params), |r| r.get(0))?;
    rows.collect()
}

#[derive(Clone, Copy)]
struct Stats {
    p50: f64,
    p95: f64,
    max: f64,
}

/// Nearest-rank percentiles over milliseconds.
fn stats(mut ms: Vec<f64>) -> Stats {
    assert!(!ms.is_empty());
    ms.sort_by(f64::total_cmp);
    let rank = |q: f64| ms[((q * ms.len() as f64).ceil() as usize).max(1) - 1];
    Stats {
        p50: rank(0.50),
        p95: rank(0.95),
        max: ms[ms.len() - 1],
    }
}

/// One discarded warm-up call, then `runs` timed calls. Every call must
/// return the same row count, or the timing is of a different query.
fn measure(runs: usize, mut f: impl FnMut() -> usize) -> (Stats, usize) {
    let rows = f();
    let ms = (0..runs)
        .map(|_| {
            let t = Instant::now();
            let n = f();
            let elapsed = t.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(n, rows, "row count changed between runs");
            elapsed
        })
        .collect();
    (stats(ms), rows)
}

/// Same read-side pragmas as `store::schema::initialize`.
fn open_read(path: &Path) -> Connection {
    let conn = Connection::open(path).expect("open variant db");
    conn.execute_batch(
        "PRAGMA temp_store = MEMORY;
         PRAGMA busy_timeout = 250;
         PRAGMA cache_size = -16384;",
    )
    .unwrap();
    conn
}

/// Replace `observation_fts` with one built from `options`, then VACUUM so
/// every variant is measured compacted. Returns seconds spent rebuilding.
fn apply_schema(conn: &Connection, options: &str) -> Result<f64, rusqlite::Error> {
    let t = Instant::now();
    if !options.is_empty() {
        recreate_fts(conn, options)?;
    }
    let rebuild = t.elapsed().as_secs_f64();
    conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(rebuild)
}

/// Drop `observation_fts` and rebuild it from `observation` with the shipped
/// arguments plus `options` (empty = the shipped schema exactly).
fn recreate_fts(conn: &Connection, options: &str) -> Result<(), rusqlite::Error> {
    let extra = if options.is_empty() {
        String::new()
    } else {
        format!(",\n{options}")
    };
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS observation_fts;
         CREATE VIRTUAL TABLE observation_fts USING fts5(
             safe_text,
             content='observation',
             content_rowid='id',
             tokenize='unicode61 remove_diacritics 2'{extra}
         );
         INSERT INTO observation_fts(observation_fts) VALUES('rebuild');"
    ))
}

fn build_variant(src: &Path, dir: &Path, schema: &Schema) -> (PathBuf, f64) {
    let dst = dir.join(format!("{}.db", schema.file));
    for ext in ["db-wal", "db-shm"] {
        let _ = fs::remove_file(dst.with_extension(ext));
    }
    fs::copy(src, &dst).expect("copy source db");
    let conn = Connection::open(&dst).unwrap();
    let rebuild = apply_schema(&conn, schema.options).expect("apply schema variant");
    drop(conn);
    (dst, rebuild)
}

fn db_bytes(path: &Path) -> u64 {
    ["db", "db-wal"]
        .iter()
        .map(|ext| {
            fs::metadata(path.with_extension(ext))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .sum()
}

/// Bytes per storage area, from the dbstat virtual table.
fn size_breakdown(conn: &Connection) -> Vec<(String, i64)> {
    let mut stmt = conn
        .prepare(
            "SELECT CASE
                 WHEN name LIKE 'observation_fts%' THEN 'fts'
                 WHEN name = 'observation' THEN 'observation'
                 WHEN name LIKE 'observation%' OR name LIKE 'sqlite_autoindex_observation%'
                     THEN 'obs-indexes'
                 ELSE 'other' END AS area,
                 SUM(pgsize)
             FROM dbstat GROUP BY area ORDER BY area",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn mib(bytes: f64) -> String {
    format!("{:.1}", bytes / (1024.0 * 1024.0))
}

fn cell(s: Stats) -> String {
    format!("{:.2} / {:.2} / {:.2}", s.p50, s.p95, s.max)
}

fn shell(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .replace('\n', " | ")
        })
        .unwrap_or_else(|e| format!("({cmd} failed: {e})"))
}

/// Length of the token that carries `*`; whole-word queries never get gated.
fn prefix_len(q: &Query) -> Option<usize> {
    q.prefix.then(|| {
        q.text
            .split_whitespace()
            .last()
            .map_or(0, |t| t.chars().count())
    })
}

struct Measured {
    schema: usize,
    order: Order,
    ranged: bool,
    query: usize,
    page: usize,
    stats: Stats,
    rows: Vec<i64>,
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let hot_only = args.iter().any(|a| a == "--hot");
    args.retain(|a| a != "--hot");
    let (src, work, runs) = match args.as_slice() {
        [s, w] => (s, w, 50),
        [s, w, r] => (s, w, r.parse().expect("RUNS must be a number")),
        _ => {
            eprintln!("usage: interactive_fts <SOURCE.db> <WORK_DIR> [RUNS] [--hot]");
            std::process::exit(2);
        }
    };
    let src = Path::new(src);
    let work = Path::new(work);
    fs::create_dir_all(work).unwrap();

    println!(
        "=== interactive FTS measurements{} ===",
        if hot_only { " (--hot)" } else { "" }
    );
    println!(
        "host: {}",
        shell(
            "sysctl",
            &[
                "-n",
                "hw.model",
                "machdep.cpu.brand_string",
                "hw.ncpu",
                "hw.memsize"
            ]
        )
    );
    println!("os: {}", shell("sw_vers", &["-productVersion"]));
    println!("load before: {}", shell("sysctl", &["-n", "vm.loadavg"]));
    let sqlite: String = Connection::open_in_memory()
        .unwrap()
        .query_row("SELECT sqlite_version()", [], |r| r.get(0))
        .unwrap();
    println!("sqlite (bundled): {sqlite}");
    println!("timed runs per cell: {runs} (+1 discarded warm-up); cells are p50 / p95 / max ms");

    let records: i64 = Connection::open(src)
        .unwrap()
        .query_row("SELECT count(*) FROM observation", [], |r| r.get(0))
        .unwrap();
    // Seed 77 is what fixture_db uses; regenerating gives the exact source bytes.
    let source_bytes = fixture::generate_diverse_zsh_history(records as usize, 77).len() as f64;
    println!(
        "records: {records}, source history: {} bytes ({} MiB), unvacuumed fixture db: {} bytes\n",
        source_bytes,
        mib(source_bytes),
        db_bytes(src)
    );

    if hot_only {
        hot_study(src, work, runs, source_bytes);
        println!("\nload after: {}", shell("sysctl", &["-n", "vm.loadavg"]));
        return;
    }

    // --- Build and size every schema variant. ---
    println!("## Size per schema (after rebuild + VACUUM)\n");
    println!(
        "| variant | DB MiB | ratio to source | observation MiB | obs indexes MiB | fts MiB | rebuild s |"
    );
    println!("|---|---|---|---|---|---|---|");
    let mut paths = Vec::new();
    let mut ratios = Vec::new();
    for schema in SCHEMAS {
        let (path, rebuild) = build_variant(src, work, schema);
        let bytes = db_bytes(&path) as f64;
        let parts = size_breakdown(&open_read(&path));
        let part = |name: &str| {
            parts
                .iter()
                .find(|(n, _)| n == name)
                .map_or(0.0, |(_, b)| *b as f64)
        };
        println!(
            "| {} | {} | {:.2}x | {} | {} | {} | {:.1} |",
            schema.name,
            mib(bytes),
            bytes / source_bytes,
            mib(part("observation")),
            mib(part("obs-indexes")),
            mib(part("fts")),
            rebuild
        );
        paths.push(path);
        ratios.push(bytes / source_bytes);
    }

    let base = open_read(&paths[0]);
    let objects: Vec<String> = base
        .prepare("SELECT name, SUM(pgsize) FROM dbstat GROUP BY name ORDER BY 2 DESC LIMIT 8")
        .unwrap()
        .query_map([], |r| {
            Ok(format!(
                "{} {}",
                r.get::<_, String>(0)?,
                mib(r.get::<_, i64>(1)? as f64)
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    println!("\nbaseline objects (MiB): {}", objects.join(", "));
    let avg: String = base
        .query_row(
            "SELECT printf('safe_text %.1f, locator %.1f',
                 avg(length(safe_text)), avg(length(locator)))
             FROM observation",
            [],
            |r| r.get(0),
        )
        .unwrap();
    println!("avg observation column bytes: {avg}");
    let max_id: i64 = base
        .query_row("SELECT max(id) FROM observation", [], |r| r.get(0))
        .unwrap();
    // The fixture writes one command per second, so 1M rows span only ~11.6
    // days and "last 30 days" would be everything. The newest 10% of rowids
    // stands in for a recency window.
    let lower = max_id - records / 10;
    let window_ms: (i64, i64) = base
        .query_row(
            "SELECT min(observed_at_ms), max(observed_at_ms) FROM observation WHERE id > ?1",
            [lower],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    println!(
        "\nrecency window (V5): rowid > {lower} = newest {} rows, {:.1} h of fixture time",
        max_id - lower,
        (window_ms.1 - window_ms.0) as f64 / 3_600_000.0
    );

    let matches: Vec<i64> = QUERIES
        .iter()
        .map(|q| {
            base.query_row(
                "SELECT count(*) FROM observation_fts WHERE observation_fts MATCH ?1",
                [escape(q.text, q.prefix)],
                |r| r.get(0),
            )
            .unwrap()
        })
        .collect();
    let recent_matches: Vec<i64> = QUERIES
        .iter()
        .map(|q| {
            base.query_row(
                "SELECT count(*) FROM observation_fts WHERE observation_fts MATCH ?1 AND rowid > ?2",
                rusqlite::params![escape(q.text, q.prefix), lower],
                |r| r.get(0),
            )
            .unwrap()
        })
        .collect();

    println!("\n## Query plans on the baseline schema\n");
    for (order, ranged) in [
        (Order::Rank, false),
        (Order::RowidDesc, false),
        (Order::Rank, true),
        (Order::RowidDesc, true),
    ] {
        let sql = format!("EXPLAIN QUERY PLAN {}", fts_sql(order, ranged));
        let mut stmt = base.prepare(&sql).unwrap();
        let params = fts_params("\"git\"", 201, ranged.then_some(lower));
        let plan: Vec<String> = stmt
            .query_map(params_from_iter(&params), |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        println!(
            "- {} {}: {}",
            order.label(),
            if ranged { "+ rowid range" } else { "" },
            plan.join("; ")
        );
    }
    drop(base);

    // --- Time every (schema, order, range, query, page). ---
    let mut results: Vec<Measured> = Vec::new();
    for (si, path) in paths.iter().enumerate() {
        let conn = open_read(path);
        for order in [Order::Rank, Order::RowidDesc] {
            for ranged in [false, true] {
                let sql = fts_sql(order, ranged);
                for (qi, q) in QUERIES.iter().enumerate() {
                    for page in PAGES {
                        // The TUI asks for one extra row to detect truncation.
                        let params = fts_params(
                            &escape(q.text, q.prefix),
                            page + 1,
                            ranged.then_some(lower),
                        );
                        let mut rows = Vec::new();
                        let (stats, _) = measure(runs, || {
                            rows = fts_rowids(&conn, &sql, &params).expect("fts query");
                            rows.len()
                        });
                        results.push(Measured {
                            schema: si,
                            order,
                            ranged,
                            query: qi,
                            page,
                            stats,
                            rows,
                        });
                    }
                }
            }
        }
        eprintln!("timed {}", SCHEMAS[si].name);
    }

    let find = |si: usize, order: Order, ranged: bool, qi: usize, page: usize| {
        results
            .iter()
            .find(|m| {
                m.schema == si
                    && m.order == order
                    && m.ranged == ranged
                    && m.query == qi
                    && m.page == page
            })
            .unwrap()
    };

    // --- Per-config tables. ---
    for (si, schema) in SCHEMAS.iter().enumerate() {
        for ranged in [false, true] {
            println!(
                "\n## {}{}\n",
                schema.name,
                if ranged {
                    " + V5 newest-10% rowid range"
                } else {
                    ""
                }
            );
            println!(
                "| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |"
            );
            println!("|---|---|---|---|---|---|---|---|");
            for (qi, q) in QUERIES.iter().enumerate() {
                let same = [Order::Rank, Order::RowidDesc]
                    .iter()
                    .flat_map(|&o| PAGES.iter().map(move |&p| (o, p)))
                    .all(|(o, p)| {
                        find(si, o, ranged, qi, p).rows == find(0, o, ranged, qi, p).rows
                    });
                println!(
                    "| `{}` | {} | {} | {} | {} | {} | {} | {} |",
                    escape(q.text, q.prefix),
                    q.class,
                    if ranged {
                        recent_matches[qi]
                    } else {
                        matches[qi]
                    },
                    cell(find(si, Order::Rank, ranged, qi, 200).stats),
                    cell(find(si, Order::Rank, ranged, qi, 50).stats),
                    cell(find(si, Order::RowidDesc, ranged, qi, 200).stats),
                    cell(find(si, Order::RowidDesc, ranged, qi, 50).stats),
                    if same { "yes" } else { "NO" },
                );
            }
        }
    }

    // --- Summary: worst p95 per config at page 200, under each V1 gate. ---
    println!("\n## Summary: worst p95 (ms) at page 200 over the query set\n");
    println!(
        "| variant | order | V5 range | ratio | all queries | prefix >= 2 chars | prefix >= 3 chars | queries > 20 ms p95 (all) |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    for (si, schema) in SCHEMAS.iter().enumerate() {
        for order in [Order::Rank, Order::RowidDesc] {
            for ranged in [false, true] {
                let worst = |min: usize| {
                    QUERIES
                        .iter()
                        .enumerate()
                        .filter(|(_, q)| prefix_len(q).is_none_or(|n| n >= min))
                        .map(|(qi, _)| find(si, order, ranged, qi, 200).stats.p95)
                        .fold(0.0, f64::max)
                };
                let slow: Vec<String> = QUERIES
                    .iter()
                    .enumerate()
                    .filter(|&(qi, _)| find(si, order, ranged, qi, 200).stats.p95 > 20.0)
                    .map(|(_, q)| format!("`{}`", escape(q.text, q.prefix)))
                    .collect();
                println!(
                    "| {} | {} | {} | {:.2}x | {:.2} | {:.2} | {:.2} | {} |",
                    schema.name,
                    order.label(),
                    if ranged { "newest 10%" } else { "-" },
                    ratios[si],
                    worst(1),
                    worst(2),
                    worst(3),
                    if slow.is_empty() {
                        "none".to_owned()
                    } else {
                        slow.join(" ")
                    }
                );
            }
        }
    }

    // --- V1: what a minimum prefix length removes. ---
    println!("\n## V1: queries skipped by a minimum prefix length (baseline, rank, page 200)\n");
    println!(
        "| query | prefix chars | matches | baseline rank p95 | runs at min 2 | runs at min 3 |"
    );
    println!("|---|---|---|---|---|---|");
    for (qi, q) in QUERIES.iter().enumerate() {
        let n = prefix_len(q);
        let runs_at = |min: usize| {
            if n.is_none_or(|n| n >= min) {
                "yes"
            } else {
                "skipped"
            }
        };
        println!(
            "| `{}` | {} | {} | {:.2} | {} | {} |",
            escape(q.text, q.prefix),
            n.map_or("whole word".to_owned(), |n| n.to_string()),
            matches[qi],
            find(0, Order::Rank, false, qi, 200).stats.p95,
            runs_at(2),
            runs_at(3),
        );
    }

    // --- V4: which query features each schema still supports. ---
    println!("\n## Feature probes (rowid DESC, page 200, rows compared to baseline)\n");
    let probes = [
        ("\"src/main.rs\"", "phrase (tokenizer splits a word)"),
        ("\"task-12\"*", "phrase with prefix last token"),
        ("\"git\" \"commit\"", "two-token AND"),
        (
            "NEAR(\"git\" \"commit\", 1)",
            "NEAR (not user-reachable; Relic escapes it)",
        ),
        ("safe_text : \"git\"", "column filter (not user-reachable)"),
    ];
    print!("| probe | kind |");
    for s in SCHEMAS {
        print!(" {} |", s.name);
    }
    println!();
    println!("|---|---|{}", "---|".repeat(SCHEMAS.len()));
    let sql = fts_sql(Order::RowidDesc, false);
    let conns: Vec<Connection> = paths.iter().map(|p| open_read(p)).collect();
    for (probe, kind) in probes {
        let params = fts_params(probe, 201, None);
        let reference = fts_rowids(&conns[0], &sql, &params);
        print!("| `{probe}` | {kind} |");
        for c in &conns {
            let out = match (fts_rowids(c, &sql, &params), &reference) {
                (Err(e), _) => format!("error: {e}"),
                (Ok(r), Ok(b)) if &r == b => format!("ok ({} rows)", r.len()),
                (Ok(r), _) => format!("DIFFERENT ({} rows)", r.len()),
            };
            print!(" {out} |");
        }
        println!();
    }
    drop(conns);

    // --- End-to-end: FTS step plus metadata fetch for a TUI page. ---
    println!("\n## End-to-end page=200 cost: FTS + metadata (p50 / p95 / max ms)\n");
    let e2e_schemas = [0, 1];
    for &si in &e2e_schemas {
        end_to_end(&paths[si], SCHEMAS[si].name, si == 0, runs);
    }

    hot_study(src, work, runs, source_bytes);

    println!("\nload after: {}", shell("sysctl", &["-n", "vm.loadavg"]));
}

/// Store-like N+1 (FTS rowids, then one lookup per row) against a single
/// statement that joins the FTS page to `observation`.
fn end_to_end(path: &Path, name: &str, with_store: bool, runs: usize) {
    let conn = open_read(path);
    let page = 200;
    let e2e_queries = [
        ("g", true),
        ("gi", true),
        ("git", true),
        ("cargo", false),
        ("git com", true),
    ];

    println!("### {name}\n");
    let mut header =
        "| query | order | FTS only | FTS + N+1 lookups | JOIN on FTS subquery | naive JOIN |"
            .to_owned();
    let mut sep = "|---|---|---|---|---|---|".to_owned();
    if with_store {
        header.push_str(" app::search (real Store) |");
        sep.push_str("---|");
    }
    println!("{header}\n{sep}");

    let mut lookup = conn
        .prepare("SELECT id, observed_at_ms, kind, safe_text, revision_id, ordinal FROM observation WHERE id = ?1")
        .unwrap();
    let store = with_store.then(|| Store::open(path).unwrap());

    for (text, prefix) in e2e_queries {
        let escaped = escape(text, prefix);
        for order in [Order::Rank, Order::RowidDesc] {
            let by = match order {
                Order::Rank => "rank",
                Order::RowidDesc => "rowid DESC",
            };
            let fts = fts_sql(order, false);
            let params = fts_params(&escaped, page + 1, None);
            let (fts_only, n) = measure(runs, || fts_rowids(&conn, &fts, &params).unwrap().len());

            let (n_plus_1, n2) = measure(runs, || {
                let ids = fts_rowids(&conn, &fts, &params).unwrap();
                ids.iter()
                    .map(|id| {
                        lookup
                            .query_row([id], |r| {
                                Ok((
                                    r.get::<_, i64>(0)?,
                                    r.get::<_, Option<i64>>(1)?,
                                    r.get::<_, i64>(2)?,
                                    r.get::<_, String>(3)?,
                                    r.get::<_, i64>(4)?,
                                    r.get::<_, i64>(5)?,
                                ))
                            })
                            .unwrap()
                    })
                    .count()
            });

            let outer = match order {
                Order::Rank => "f.rank",
                Order::RowidDesc => "o.id DESC",
            };
            let sub_join = format!(
                "SELECT o.id, o.observed_at_ms, o.kind, o.safe_text, o.revision_id, o.ordinal
                 FROM (SELECT rowid AS id, rank FROM observation_fts
                       WHERE observation_fts MATCH ?1 ORDER BY {by} LIMIT ?2) f
                 JOIN observation o ON o.id = f.id
                 ORDER BY {outer}"
            );
            let naive_by = match order {
                Order::Rank => "f.rank",
                Order::RowidDesc => "f.rowid DESC",
            };
            let naive_join = format!(
                "SELECT o.id, o.observed_at_ms, o.kind, o.safe_text, o.revision_id, o.ordinal
                 FROM observation_fts f JOIN observation o ON o.id = f.rowid
                 WHERE observation_fts MATCH ?1 ORDER BY {naive_by} LIMIT ?2"
            );
            let run_join = |sql: &str| {
                let mut stmt = conn.prepare_cached(sql).unwrap();
                stmt.query_map(params_from_iter(&params), |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(3)?))
                })
                .unwrap()
                .count()
            };
            let (joined, n3) = measure(runs, || run_join(&sub_join));
            let (naive, n4) = measure(runs, || run_join(&naive_join));
            assert!(
                n == n2 && n == n3 && n == n4,
                "e2e shapes disagree on row count"
            );

            let mut line = format!(
                "| `{escaped}` | {} | {} | {} | {} | {} |",
                order.label(),
                cell(fts_only),
                cell(n_plus_1),
                cell(joined),
                cell(naive)
            );
            if let (Some(store), Order::Rank) = (&store, order) {
                let req = SearchRequest {
                    query: text.to_owned(),
                    limit: page,
                    mode: if prefix {
                        MatchMode::PrefixLast
                    } else {
                        MatchMode::Words
                    },
                    order: app::Order::Rank,
                };
                // Measured to the end: the bench wants the cost, not the cut-off.
                let far = || app::Budget {
                    deadline: Instant::now() + std::time::Duration::from_secs(600),
                    cancel: None,
                };
                let (s, _) = measure(runs, || {
                    app::search(store, &req, &far()).unwrap().hits.len()
                });
                line.push_str(&format!(" {} |", cell(s)));
            } else if store.is_some() {
                line.push_str(" n/a |");
            }
            println!("{line}");
        }
    }
    println!();
}

// ---------------------------------------------------------------------------
// Hot-token prefixes.
//
// FTS5 answers a prefix token that has no prefix index of exactly its length
// by merging the full doclist of every term under the prefix before the first
// row comes out, so `ORDER BY rowid DESC LIMIT n` cannot exit early. The cost
// follows the postings under the prefix, not its length: on the fixture
// `"task"*` (one term, 93% of rows) costs far more than the whole word
// `"task"`. This study finds the cheapest schema whose worst keystroke stays
// under the gate when the corpus has dominant tokens.
// ---------------------------------------------------------------------------

/// How many of the most frequent tokens get every prefix length. Eight
/// reaches the two most frequent tokens longer than four characters on the
/// fixture (`config`, `cargo`).
const HOT_TOKENS: usize = 8;
/// Leading words for two-word keystrokes. On the fixture `git` never shares
/// a row with `# task-N`; `cargo` always does.
const HOT_LEADS: [&str; 2] = ["git", "cargo"];
const HOT_PAGE: usize = 200;
const GATE_MS: f64 = 20.0;
/// Extra prefixes for the rewrite and cost-model tables: wide expansions
/// (many small terms) rather than one dominant term.
const WIDE_PROBES: [&str; 4] = ["c", "t", "1", "12"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Corpus {
    /// The 1M fixture as built.
    Fixture,
    /// The fixture with `# task-N` renamed `# taskid-N`: the dominant token
    /// becomes six characters, longer than any indexed prefix length.
    LongHot,
}

impl Corpus {
    fn label(self) -> &'static str {
        match self {
            Corpus::Fixture => "fixture",
            Corpus::LongHot => "taskid corpus (fixture with `# task-N` renamed `# taskid-N`)",
        }
    }

    fn hot_tokens(self) -> usize {
        match self {
            Corpus::Fixture => HOT_TOKENS,
            Corpus::LongHot => 1,
        }
    }
}

struct HotSchema {
    name: &'static str,
    file: &'static str,
    options: &'static str,
    corpus: Corpus,
}

const fn hs(
    name: &'static str,
    file: &'static str,
    options: &'static str,
    corpus: Corpus,
) -> HotSchema {
    HotSchema {
        name,
        file,
        options,
        corpus,
    }
}

/// The first schema of each corpus is that corpus's reference.
const HOT_SCHEMAS: &[HotSchema] = &[
    hs("baseline", "hot_baseline", "", Corpus::Fixture),
    hs("prefix='2'", "hot_prefix2", "prefix='2'", Corpus::Fixture),
    hs(
        "prefix='2 3'",
        "hot_prefix23",
        "prefix='2 3'",
        Corpus::Fixture,
    ),
    hs(
        "prefix='2 3 4'",
        "hot_prefix234",
        "prefix='2 3 4'",
        Corpus::Fixture,
    ),
    hs(
        "prefix='1 2 3 4'",
        "hot_prefix1234",
        "prefix='1 2 3 4'",
        Corpus::Fixture,
    ),
    hs(
        "taskid baseline",
        "hot_taskid_baseline",
        "",
        Corpus::LongHot,
    ),
    hs(
        "taskid prefix='2'",
        "hot_taskid_prefix2",
        "prefix='2'",
        Corpus::LongHot,
    ),
    hs(
        "taskid prefix='2 3 4'",
        "hot_taskid_prefix234",
        "prefix='2 3 4'",
        Corpus::LongHot,
    ),
];

/// A keystroke-shaped query with an owned text, so it can be generated from
/// the corpus's own vocabulary.
struct Keystroke {
    text: String,
    prefix: bool,
    class: &'static str,
}

impl Keystroke {
    fn new(text: impl Into<String>, prefix: bool, class: &'static str) -> Self {
        Keystroke {
            text: text.into(),
            prefix,
            class,
        }
    }

    fn escaped(&self) -> String {
        escape(&self.text, self.prefix)
    }

    fn last(&self) -> &str {
        self.text.split_whitespace().last().unwrap_or("")
    }

    fn head(&self) -> Vec<&str> {
        let words: Vec<&str> = self.text.split_whitespace().collect();
        words[..words.len().saturating_sub(1)].to_vec()
    }

    /// Characters in the `*` token; `None` for a whole-word query.
    fn prefix_chars(&self) -> Option<usize> {
        self.prefix.then(|| self.last().chars().count())
    }

    /// Whether the TUI sends it to FTS under its `MIN_PREFIX_CHARS` gate.
    fn sent_by_tui(&self) -> bool {
        self.prefix_chars()
            .is_none_or(|n| n >= app::MIN_PREFIX_CHARS)
    }
}

/// Every term with its document count, sorted by term. Read through a temp
/// fts5vocab table, so nothing is written to the database file.
fn vocabulary(conn: &Connection) -> Vec<(String, i64)> {
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS temp.hot_vocab
             USING fts5vocab(main, observation_fts, 'row');",
    )
    .unwrap();
    let mut stmt = conn
        .prepare("SELECT term, doc FROM temp.hot_vocab ORDER BY term")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// The terms a prefix token expands to (byte-wise, like FTS5).
fn expansion<'a>(vocab: &'a [(String, i64)], prefix: &str) -> &'a [(String, i64)] {
    let start = vocab.partition_point(|(t, _)| t.as_str() < prefix);
    let len = vocab[start..]
        .iter()
        .take_while(|(t, _)| t.starts_with(prefix))
        .count();
    &vocab[start..start + len]
}

/// Terms and postings the last token touches: every expanded term for a
/// prefix, the exact term for a whole word.
fn touched(vocab: &[(String, i64)], k: &Keystroke) -> (usize, i64) {
    let last = k.last().to_lowercase();
    let terms: Vec<&(String, i64)> = if k.prefix {
        expansion(vocab, &last).iter().collect()
    } else {
        vocab.iter().filter(|(t, _)| *t == last).collect()
    };
    (terms.len(), terms.iter().map(|(_, d)| d).sum())
}

/// Each prefix from two characters up to the full token, the whole word, and
/// the same behind each lead word.
fn hot_keystrokes(hot: &[(String, i64)]) -> Vec<Keystroke> {
    let mut out = Vec::new();
    for (token, _) in hot {
        let chars: Vec<char> = token.chars().collect();
        let prefixes: Vec<String> = (2..=chars.len())
            .map(|n| chars[..n].iter().collect())
            .collect();
        for p in &prefixes {
            out.push(Keystroke::new(p.as_str(), true, "hot prefix"));
        }
        out.push(Keystroke::new(token.as_str(), false, "hot word"));
        for lead in HOT_LEADS.iter().filter(|&&l| l != token) {
            for p in &prefixes {
                out.push(Keystroke::new(
                    format!("{lead} {p}"),
                    true,
                    "lead + hot prefix",
                ));
            }
            out.push(Keystroke::new(
                format!("{lead} {token}"),
                false,
                "lead + hot word",
            ));
        }
    }
    out
}

/// `head AND (last_expr)`, with each head word quoted whole.
fn with_last(k: &Keystroke, last_expr: &str) -> String {
    let head = escape(&k.head().join(" "), false);
    if head.is_empty() {
        format!("({last_expr})")
    } else {
        format!("{head} AND ({last_expr})")
    }
}

fn quote(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// Rewrite: the last token as a whole word OR as a prefix.
fn word_or_prefix(k: &Keystroke) -> String {
    let w = quote(k.last());
    with_last(k, &format!("{w} OR {w}*"))
}

/// Rewrite: the prefix replaced by an OR of the terms it expands to. `None`
/// when nothing matches the prefix (the result is then empty).
fn or_of_terms(k: &Keystroke, terms: &[String]) -> Option<String> {
    (!terms.is_empty()).then(|| {
        with_last(
            k,
            &terms
                .iter()
                .map(|t| quote(t))
                .collect::<Vec<_>>()
                .join(" OR "),
        )
    })
}

/// Exclusive upper bound for a term range scan over `prefix*`.
fn prefix_upper(prefix: &str) -> String {
    format!("{prefix}\u{10FFFF}")
}

/// Terms under `prefix`, read from the temp fts5vocab table per keystroke.
fn scan_terms(conn: &Connection, prefix: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare_cached("SELECT term FROM temp.hot_vocab WHERE term >= ?1 AND term < ?2")
        .unwrap();
    stmt.query_map([prefix, &prefix_upper(prefix)], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Rows for the rowid-DESC page of an FTS expression; `None` means empty.
fn page_rows(conn: &Connection, expr: Option<&str>, lower: Option<i64>) -> Vec<i64> {
    match expr {
        None => Vec::new(),
        Some(e) => fts_rowids(
            conn,
            &fts_sql(Order::RowidDesc, lower.is_some()),
            &fts_params(e, HOT_PAGE + 1, lower),
        )
        .expect("fts query"),
    }
}

/// Rewrite: run the last token as a whole word first. A full page of whole
/// word hits proves the prefix page lies at or above its oldest rowid, so the
/// prefix query is bounded there. Same rows as the plain prefix query.
fn exact_first(conn: &Connection, k: &Keystroke) -> Vec<i64> {
    let words = page_rows(conn, Some(&escape(&k.text, false)), None);
    let lower = (words.len() == HOT_PAGE + 1).then(|| words[words.len() - 1] - 1);
    page_rows(conn, Some(&k.escaped()), lower)
}

fn build_hot_variant(src: &Path, dir: &Path, schema: &HotSchema) -> PathBuf {
    let dst = dir.join(format!("{}.db", schema.file));
    for ext in ["db-wal", "db-shm"] {
        let _ = fs::remove_file(dst.with_extension(ext));
    }
    fs::copy(src, &dst).expect("copy source db");
    let conn = Connection::open(&dst).unwrap();
    match schema.corpus {
        Corpus::Fixture => {
            apply_schema(&conn, schema.options).expect("apply schema variant");
        }
        Corpus::LongHot => {
            conn.execute_batch(
                "DROP TABLE observation_fts;
                 UPDATE observation SET safe_text = replace(safe_text, '# task-', '# taskid-');",
            )
            .unwrap();
            recreate_fts(&conn, schema.options).unwrap();
            conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
                .unwrap();
        }
    }
    dst
}

fn p95(s: Stats) -> String {
    format!("{:.2}", s.p95)
}

fn hot_study(src: &Path, work: &Path, runs: usize, source_bytes: f64) {
    println!(
        "\n# Hot-token prefixes: rowid DESC, page {HOT_PAGE} (limit {}), p95 ms\n",
        HOT_PAGE + 1
    );
    println!("## Size per hot-study schema (after rebuild + VACUUM)\n");
    println!("| variant | corpus | DB MiB | ratio to source | fts MiB |");
    println!("|---|---|---|---|---|");
    let mut paths = Vec::new();
    let mut ratios = Vec::new();
    for schema in HOT_SCHEMAS {
        let path = build_hot_variant(src, work, schema);
        let bytes = db_bytes(&path) as f64;
        let fts = size_breakdown(&open_read(&path))
            .iter()
            .find(|(n, _)| n == "fts")
            .map_or(0.0, |(_, b)| *b as f64);
        println!(
            "| {} | {} | {} | {:.2}x | {} |",
            schema.name,
            if schema.corpus == Corpus::Fixture {
                "fixture"
            } else {
                "taskid"
            },
            mib(bytes),
            bytes / source_bytes,
            mib(fts)
        );
        paths.push(path);
        ratios.push(bytes / source_bytes);
    }
    eprintln!("built hot variants");

    for corpus in [Corpus::Fixture, Corpus::LongHot] {
        let variants: Vec<usize> = (0..HOT_SCHEMAS.len())
            .filter(|&i| HOT_SCHEMAS[i].corpus == corpus)
            .collect();
        let base = open_read(&paths[variants[0]]);
        let mut vocab = Vec::new();
        let (vocab_load, _) = measure(runs.min(10), || {
            vocab = vocabulary(&base);
            vocab.len()
        });
        let records: i64 = base
            .query_row("SELECT count(*) FROM observation", [], |r| r.get(0))
            .unwrap();

        println!("\n## {}\n", corpus.label());
        println!(
            "Full term list through fts5vocab (what an in-memory term cache would load): {} terms, {} ms (p50 / p95 / max, {} runs)\n",
            vocab.len(),
            cell(vocab_load),
            runs.min(10)
        );
        println!(
            "### Top 10 tokens by document frequency (fts5vocab 'row', {} distinct terms)\n",
            vocab.len()
        );
        println!("| # | token | chars | rows containing it | share of rows |");
        println!("|---|---|---|---|---|");
        let mut by_doc: Vec<(String, i64)> = vocab.clone();
        by_doc.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (i, (t, d)) in by_doc.iter().take(10).enumerate() {
            println!(
                "| {} | `{t}` | {} | {d} | {:.1}% |",
                i + 1,
                t.chars().count(),
                100.0 * *d as f64 / records as f64
            );
        }

        // Keystroke set: the #10 set (fixture only), then the hot set.
        let mut keys: Vec<Keystroke> = Vec::new();
        if corpus == Corpus::Fixture {
            keys.extend(
                QUERIES
                    .iter()
                    .map(|q| Keystroke::new(q.text, q.prefix, q.class)),
            );
        }
        keys.extend(hot_keystrokes(
            &by_doc[..corpus.hot_tokens().min(by_doc.len())],
        ));
        let mut seen = HashSet::new();
        keys.retain(|k| seen.insert(k.escaped()));

        // Time every keystroke on every schema of this corpus.
        let mut timed: Vec<Vec<(Stats, Vec<i64>)>> = Vec::new();
        for &vi in &variants {
            let conn = open_read(&paths[vi]);
            let row = keys
                .iter()
                .map(|k| {
                    let mut rows = Vec::new();
                    let (stats, _) = measure(runs, || {
                        rows = page_rows(&conn, Some(&k.escaped()), None);
                        rows.len()
                    });
                    (stats, rows)
                })
                .collect();
            timed.push(row);
            eprintln!("timed hot set on {}", HOT_SCHEMAS[vi].name);
        }

        println!(
            "\n### Every keystroke (p95 ms; `!` marks rows that differ from {})\n",
            HOT_SCHEMAS[variants[0]].name
        );
        let mut header =
            "| query | class | `*` chars | terms under last token | postings under last token | matches |".to_owned();
        for &vi in &variants {
            header.push_str(&format!(" {} |", HOT_SCHEMAS[vi].name));
        }
        println!("{header}");
        println!("|---|---|---|---|---|---|{}", "---|".repeat(variants.len()));
        for (ki, k) in keys.iter().enumerate() {
            let (terms, postings) = touched(&vocab, k);
            let matches: i64 = base
                .query_row(
                    "SELECT count(*) FROM observation_fts WHERE observation_fts MATCH ?1",
                    [k.escaped()],
                    |r| r.get(0),
                )
                .unwrap();
            let mut line = format!(
                "| `{}` | {} | {} | {terms} | {postings} | {matches} |",
                k.escaped(),
                k.class,
                k.prefix_chars()
                    .map_or("word".to_owned(), |n| n.to_string()),
            );
            for row in &timed {
                let (s, rows) = &row[ki];
                let same = rows == &timed[0][ki].1;
                line.push_str(&format!(" {}{} |", p95(*s), if same { "" } else { " !" }));
            }
            println!("{line}");
        }

        println!("\n### Summary: worst keystroke per schema\n");
        println!(
            "| variant | ratio to source | worst p95, keystrokes the TUI sends (last token >= {} chars or whole) | that query | worst p95, 1-char included | sent keystrokes > {GATE_MS} ms | sent keystrokes > 10 ms | rows differ |",
            app::MIN_PREFIX_CHARS
        );
        println!("|---|---|---|---|---|---|---|---|");
        for (row, &vi) in timed.iter().zip(&variants) {
            let sent = || keys.iter().enumerate().filter(|(_, k)| k.sent_by_tui());
            let (worst_i, worst) = sent()
                .map(|(ki, _)| (ki, row[ki].0.p95))
                .fold((0, 0.0), |a, b| if b.1 > a.1 { b } else { a });
            let all = row.iter().map(|(s, _)| s.p95).fold(0.0, f64::max);
            let over = |ms: f64| sent().filter(|&(ki, _)| row[ki].0.p95 > ms).count();
            let differ = (0..keys.len())
                .filter(|&ki| row[ki].1 != timed[0][ki].1)
                .count();
            println!(
                "| {} | {:.2}x | {:.2} | `{}` | {:.2} | {} of {} | {} | {differ} |",
                HOT_SCHEMAS[vi].name,
                ratios[vi],
                worst,
                keys[worst_i].escaped(),
                all,
                over(GATE_MS),
                sent().count(),
                over(10.0),
            );
        }

        // Hybrid: a prefix whose length has no prefix index becomes an OR of
        // its expanded whole terms from the in-memory term list.
        println!(
            "\n### Hybrid: prefix index for short prefixes, OR of expanded terms above it (p95 ms)\n"
        );
        println!(
            "| schema | OR rewrite for `*` tokens of | worst p95, keystrokes the TUI sends | that query | 2nd worst | most terms in one OR | sent keystrokes > {GATE_MS} ms | rows differ from plain |"
        );
        println!("|---|---|---|---|---|---|---|---|");
        for (vj, &vi) in variants.iter().enumerate() {
            let min_or = match HOT_SCHEMAS[vi].options {
                "" => 2,
                "prefix='2'" => 3,
                _ => continue,
            };
            let conn = open_read(&paths[vi]);
            let mut worst: Vec<(f64, usize)> = Vec::new();
            let mut widest = 0;
            let mut differ = 0;
            for (ki, k) in keys.iter().enumerate().filter(|(_, k)| k.sent_by_tui()) {
                let expr = if k.prefix_chars().is_some_and(|n| n >= min_or) {
                    let terms: Vec<String> = expansion(&vocab, &k.last().to_lowercase())
                        .iter()
                        .map(|(t, _)| t.clone())
                        .collect();
                    widest = widest.max(terms.len());
                    or_of_terms(k, &terms)
                } else {
                    Some(k.escaped())
                };
                let mut rows = Vec::new();
                let (s, _) = measure(runs, || {
                    rows = page_rows(&conn, expr.as_deref(), None);
                    rows.len()
                });
                differ += usize::from(rows != timed[vj][ki].1);
                worst.push((s.p95, ki));
            }
            worst.sort_by(|a, b| b.0.total_cmp(&a.0));
            println!(
                "| {} | >= {min_or} chars | {:.2} | `{}` | {:.2} (`{}`) | {widest} | {} | {differ} |",
                HOT_SCHEMAS[vi].name,
                worst[0].0,
                keys[worst[0].1].escaped(),
                worst[1].0,
                keys[worst[1].1].escaped(),
                worst.iter().filter(|w| w.0 > GATE_MS).count(),
            );
        }

        if corpus == Corpus::Fixture {
            rewrites(&base, &vocab, &keys, runs);
        }
    }
}

/// Index-free rewrites of a prefix keystroke on the baseline schema, against
/// the plain prefix query they would replace.
fn rewrites(conn: &Connection, vocab: &[(String, i64)], keys: &[Keystroke], runs: usize) {
    let mut probes: Vec<Keystroke> = keys
        .iter()
        .filter(|k| k.prefix && (k.class.starts_with("hot") || k.class.starts_with("lead")))
        .map(|k| Keystroke::new(k.text.as_str(), true, k.class))
        .collect();
    probes.extend(
        WIDE_PROBES
            .iter()
            .map(|p| Keystroke::new(*p, true, "wide probe")),
    );

    println!("\n### Query rewrites on the baseline schema (p95 ms)\n");
    println!(
        "Columns: plain prefix (what Store sends); whole word only (changes results); word OR prefix; \
         exact-first (whole-word page bounds a `rowid >=` range on the prefix query; same rows); \
         per-keystroke fts5vocab term scan; OR of the expanded whole terms given a term list; \
         scan + OR together (same rows).\n"
    );
    println!(
        "| query | terms | postings | plain | whole word only | rows kept | word OR prefix | exact-first | rows same | vocab scan | OR of terms | scan + OR | rows same |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    let mut model = Vec::new();
    for k in &probes {
        let (terms, postings) = touched(vocab, k);
        let last = k.last().to_lowercase();
        let expanded: Vec<String> = expansion(vocab, &last)
            .iter()
            .map(|(t, _)| t.clone())
            .collect();
        let plain_rows = page_rows(conn, Some(&k.escaped()), None);

        let (plain, _) = measure(runs, || page_rows(conn, Some(&k.escaped()), None).len());
        let word_expr = escape(&k.text, false);
        let word_rows = page_rows(conn, Some(&word_expr), None);
        let (word, _) = measure(runs, || page_rows(conn, Some(&word_expr), None).len());
        let kept = word_rows.iter().filter(|r| plain_rows.contains(r)).count();
        let wop = word_or_prefix(k);
        let (wop_s, _) = measure(runs, || page_rows(conn, Some(&wop), None).len());
        let (exact, _) = measure(runs, || exact_first(conn, k).len());
        let exact_same = exact_first(conn, k) == plain_rows;
        let (scan, n) = measure(runs, || scan_terms(conn, &last).len());
        assert_eq!(
            n, terms,
            "vocab scan disagrees with the in-memory vocabulary for {last:?}"
        );
        let or_expr = or_of_terms(k, &expanded);
        let (or_s, _) = measure(runs, || page_rows(conn, or_expr.as_deref(), None).len());
        let (both, _) = measure(runs, || {
            let terms = scan_terms(conn, &last);
            page_rows(conn, or_of_terms(k, &terms).as_deref(), None).len()
        });
        let or_same = page_rows(conn, or_expr.as_deref(), None) == plain_rows;
        println!(
            "| `{}` | {terms} | {postings} | {} | {} | {kept} of {} | {} | {} | {} | {} | {} | {} | {} |",
            k.escaped(),
            p95(plain),
            p95(word),
            plain_rows.len(),
            p95(wop_s),
            p95(exact),
            if exact_same { "yes" } else { "NO" },
            p95(scan),
            p95(or_s),
            p95(both),
            if or_same { "yes" } else { "NO" },
        );
        if k.head().is_empty() {
            model.push((k.escaped(), terms, postings, plain));
        }
    }

    println!("\n### Cost model: single-token prefix on the baseline schema\n");
    println!("| query | terms | postings | p50 ms | p95 ms | ns per posting (p50) |");
    println!("|---|---|---|---|---|---|");
    model.sort_by_key(|m| m.2);
    for (q, terms, postings, s) in &model {
        println!(
            "| `{q}` | {terms} | {postings} | {:.2} | {:.2} | {:.1} |",
            s.p50,
            s.p95,
            s.p50 * 1e6 / (*postings).max(1) as f64
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relic::import::{SourceKind, import_file};
    use relic::redact::Redactor;
    use std::io::Write;

    const LINES: &[&str] = &[
        "git push origin main",
        "git commit -m wip",
        "go build ./...",
        "cargo build --release",
        "grep -r TODO src/main.rs",
        "docker compose up",
        "git checkout src/main.rs",
    ];

    fn fixture_db(dir: &Path) -> PathBuf {
        let history = dir.join("zsh_history");
        let mut f = fs::File::create(&history).unwrap();
        for (i, l) in LINES.iter().enumerate() {
            writeln!(f, ": {}:0;{l}", 1_700_000_000 + i).unwrap();
        }
        let db = dir.join("base.db");
        let mut store = Store::open(&db).unwrap();
        import_file(&mut store, &history, SourceKind::Zsh, &Redactor::new()).unwrap();
        store.checkpoint().unwrap();
        db
    }

    fn rowid_desc(
        conn: &Connection,
        query: &str,
        prefix: bool,
    ) -> Result<Vec<i64>, rusqlite::Error> {
        fts_rowids(
            conn,
            &fts_sql(Order::RowidDesc, false),
            &fts_params(&escape(query, prefix), 100, None),
        )
    }

    /// The bench's queries must return exactly what Store returns, in the
    /// same order, or its numbers describe some other search.
    #[test]
    fn bench_queries_match_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture_db(dir.path());
        let store = Store::open(&db).unwrap();
        let conn = open_read(&db);
        let ids =
            |hits: Vec<relic::domain::SearchHit>| hits.iter().map(|h| h.id).collect::<Vec<_>>();
        for query in ["g", "gi", "gi pu", "git c", "git pu", "src/main.rs", "\""] {
            let recent = ids(store.search_prefix(query, 100).unwrap());
            assert_eq!(
                rowid_desc(&conn, query, true).unwrap_or_default(),
                recent,
                "{query:?}"
            );
            let ranked = ids(store.search_ranked(query, false, 100).unwrap());
            let got = fts_rowids(
                &conn,
                &fts_sql(Order::Rank, false),
                &fts_params(&escape(query, false), 100, None),
            )
            .unwrap_or_default();
            assert_eq!(got, ranked, "{query:?}");
        }
    }

    #[test]
    fn prefix_index_variant_keeps_baseline_results() {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture_db(dir.path());
        let conn = Connection::open(&db).unwrap();
        let before: Vec<_> = ["g", "gi", "git c", "src/main.rs"]
            .iter()
            .map(|q| rowid_desc(&conn, q, true).unwrap())
            .collect();

        apply_schema(&conn, "prefix='1 2 3'").unwrap();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'observation_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.contains("prefix='1 2 3'"), "{sql}");
        let after: Vec<_> = ["g", "gi", "git c", "src/main.rs"]
            .iter()
            .map(|q| rowid_desc(&conn, q, true).unwrap())
            .collect();
        assert_eq!(after, before);
        assert!(after.iter().all(|r| !r.is_empty()));
    }

    #[test]
    fn detail_none_variant_rejects_multi_token_phrases() {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture_db(dir.path());
        let conn = Connection::open(&db).unwrap();
        assert_eq!(rowid_desc(&conn, "src/main.rs", false).unwrap().len(), 2);

        apply_schema(&conn, "detail=none").unwrap();
        assert_eq!(rowid_desc(&conn, "git c", true).unwrap().len(), 2);
        assert!(rowid_desc(&conn, "src/main.rs", false).is_err());
    }

    #[test]
    fn rowid_range_keeps_only_newer_rows() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_read(&fixture_db(dir.path()));
        let all = rowid_desc(&conn, "git", false).unwrap();
        let recent = fts_rowids(
            &conn,
            &fts_sql(Order::RowidDesc, true),
            &fts_params("\"git\"", 100, Some(all[1])),
        )
        .unwrap();
        assert_eq!(recent, all[..1]);
    }

    #[test]
    fn nearest_rank_percentiles() {
        let s = stats((1..=100).rev().map(f64::from).collect());
        assert_eq!((s.p50, s.p95, s.max), (50.0, 95.0, 100.0));
        let one = stats(vec![7.0]);
        assert_eq!((one.p50, one.p95, one.max), (7.0, 7.0, 7.0));
    }

    #[test]
    fn hot_keystrokes_cover_every_prefix_length() {
        let texts = |hot: &[(String, i64)]| -> Vec<(String, bool)> {
            hot_keystrokes(hot)
                .into_iter()
                .map(|k| (k.text, k.prefix))
                .collect()
        };
        let task = texts(&[("task".to_owned(), 9)]);
        for want in [
            ("ta", true),
            ("tas", true),
            ("task", true),
            ("task", false),
            ("git ta", true),
            ("cargo tas", true),
            ("cargo task", false),
        ] {
            assert!(
                task.contains(&(want.0.to_owned(), want.1)),
                "{want:?} missing from {task:?}"
            );
        }
        assert!(
            !task.iter().any(|(t, _)| t == "t"),
            "1-char prefixes are gated, not hot keystrokes"
        );
        // A hot token that is itself a lead word is not paired with itself.
        assert!(
            !texts(&[("git".to_owned(), 9)])
                .iter()
                .any(|(t, _)| t.starts_with("git gi"))
        );
    }

    #[test]
    fn expansion_is_the_bytewise_term_range() {
        let vocab: Vec<(String, i64)> = ["ta", "table", "tag", "task", "tasks", "tb", "z"]
            .iter()
            .map(|t| (t.to_string(), 1))
            .collect();
        let terms = |p: &str| {
            expansion(&vocab, p)
                .iter()
                .map(|(t, _)| t.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(terms("ta"), ["ta", "table", "tag", "task", "tasks"]);
        assert_eq!(terms("task"), ["task", "tasks"]);
        assert!(terms("q").is_empty());
        assert!(prefix_upper("task").as_str() > "tasks~");
    }

    /// More than a page of `task` rows, with newer `tasks`/`tar` rows mixed
    /// in, so the exact-first bound and the term expansion both matter.
    fn hot_fixture_db(dir: &Path) -> PathBuf {
        let history = dir.join("zsh_history");
        let mut f = fs::File::create(&history).unwrap();
        for i in 0..600 {
            let cmd = match i % 5 {
                0 => format!("tasks list {i}"),
                1 => format!("tar xf a{i}.tar"),
                2 => format!("git push # task-{i}"),
                _ => format!("cargo build # task-{i}"),
            };
            writeln!(f, ": {}:0;{cmd}", 1_700_000_000 + i).unwrap();
        }
        let db = dir.join("hot.db");
        let mut store = Store::open(&db).unwrap();
        import_file(&mut store, &history, SourceKind::Zsh, &Redactor::new()).unwrap();
        store.checkpoint().unwrap();
        db
    }

    #[test]
    fn prefix_preserving_rewrites_return_the_plain_prefix_rows() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_read(&hot_fixture_db(dir.path()));
        let vocab = vocabulary(&conn);
        for text in ["ta", "tas", "task", "cargo ta", "cargo task", "git tas"] {
            let k = Keystroke::new(text, true, "test");
            let plain = page_rows(&conn, Some(&k.escaped()), None);
            assert!(!plain.is_empty(), "{text}");
            assert_eq!(exact_first(&conn, &k), plain, "exact-first {text}");
            let terms = scan_terms(&conn, k.last());
            assert_eq!(terms.len(), touched(&vocab, &k).0, "{text}");
            assert_eq!(
                page_rows(&conn, or_of_terms(&k, &terms).as_deref(), None),
                plain,
                "terms {text}"
            );
            assert_eq!(
                page_rows(&conn, Some(&word_or_prefix(&k)), None),
                plain,
                "word OR prefix {text}"
            );
        }
        // The exact-first bound is exercised: `task` alone fills a page.
        let words = page_rows(&conn, Some("\"task\""), None);
        assert_eq!(words.len(), HOT_PAGE + 1);
        // Whole word only drops newer `tasks` rows: it changes the results.
        let k = Keystroke::new("task", true, "test");
        assert_ne!(words, page_rows(&conn, Some(&k.escaped()), None));
        assert_eq!(or_of_terms(&Keystroke::new("zz", true, "test"), &[]), None);
    }

    #[test]
    fn long_hot_variant_renames_the_dominant_token() {
        let dir = tempfile::tempdir().unwrap();
        let src = hot_fixture_db(dir.path());
        let schema = hs("t", "long", "prefix='2 3 4'", Corpus::LongHot);
        let conn = open_read(&build_hot_variant(&src, dir.path(), &schema));
        let vocab = vocabulary(&conn);
        assert!(vocab.iter().any(|(t, _)| t == "taskid"));
        assert!(!vocab.iter().any(|(t, _)| t == "task"));
        assert!(!page_rows(&conn, Some("\"cargo\" \"taski\"*"), None).is_empty());
    }
}
