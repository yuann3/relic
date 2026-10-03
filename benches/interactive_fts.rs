//! Search-as-you-type FTS measurements for #10, feeding the #11 architecture
//! lock. Rebuilds the 1M diverse fixture under several FTS5 schema variants
//! and times every keystroke-shaped query with each ordering.
//!
//! Run with:
//!   cargo run --release --bin fixture_db -- target/fixtures/1m.db 1000000
//!   cargo run --release --bin interactive_fts -- target/fixtures/1m.db target/fts-variants [RUNS]

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
    Query { text, prefix, class }
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
    Schema { name: "V0 baseline", file: "baseline", options: "" },
    // Only 1-char prefixes are slow under rowid order on the baseline, so the
    // cheapest prefix index is worth its own row.
    Schema { name: "V3 prefix='1'", file: "prefix1", options: "prefix='1'" },
    Schema { name: "V3 prefix='1 2 3'", file: "prefix123", options: "prefix='1 2 3'" },
    Schema { name: "V3 prefix='2 3'", file: "prefix23", options: "prefix='2 3'" },
    Schema { name: "V4 detail=column", file: "detail_column", options: "detail=column" },
    Schema { name: "V4 detail=none", file: "detail_none", options: "detail=none" },
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
        Order::Rank => "rank",
        Order::RowidDesc => "rowid DESC",
    };
    format!(
        "SELECT rowid FROM observation_fts WHERE observation_fts MATCH ?1{range} \
         ORDER BY {order} LIMIT ?2"
    )
}

fn fts_params(escaped: &str, limit: usize, lower: Option<i64>) -> Vec<Value> {
    let mut p = vec![Value::Text(escaped.to_owned()), Value::Integer(limit as i64)];
    if let Some(lo) = lower {
        p.push(Value::Integer(lo));
    }
    p
}

fn fts_rowids(
    conn: &Connection,
    sql: &str,
    params: &[Value],
) -> Result<Vec<i64>, rusqlite::Error> {
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
    Stats { p50: rank(0.50), p95: rank(0.95), max: ms[ms.len() - 1] }
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
        conn.execute_batch(&format!(
            "DROP TABLE observation_fts;
             CREATE VIRTUAL TABLE observation_fts USING fts5(
                 safe_text,
                 content='observation',
                 content_rowid='id',
                 tokenize='unicode61 remove_diacritics 2',
                 {options}
             );
             INSERT INTO observation_fts(observation_fts) VALUES('rebuild');"
        ))?;
    }
    let rebuild = t.elapsed().as_secs_f64();
    conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(rebuild)
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
        .map(|ext| fs::metadata(path.with_extension(ext)).map(|m| m.len()).unwrap_or(0))
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
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().replace('\n', " | "))
        .unwrap_or_else(|e| format!("({cmd} failed: {e})"))
}

/// Length of the token that carries `*`; whole-word queries never get gated.
fn prefix_len(q: &Query) -> Option<usize> {
    q.prefix.then(|| q.text.split_whitespace().last().map_or(0, |t| t.chars().count()))
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (src, work, runs) = match args.as_slice() {
        [s, w] => (s, w, 50),
        [s, w, r] => (s, w, r.parse().expect("RUNS must be a number")),
        _ => {
            eprintln!("usage: interactive_fts <SOURCE.db> <WORK_DIR> [RUNS]");
            std::process::exit(2);
        }
    };
    let src = Path::new(src);
    let work = Path::new(work);
    fs::create_dir_all(work).unwrap();

    println!("=== interactive FTS measurements ===");
    println!("host: {}", shell("sysctl", &["-n", "hw.model", "machdep.cpu.brand_string", "hw.ncpu", "hw.memsize"]));
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

    // --- Build and size every schema variant. ---
    println!("## Size per schema (after rebuild + VACUUM)\n");
    println!("| variant | DB MiB | ratio to source | observation MiB | obs indexes MiB | fts MiB | rebuild s |");
    println!("|---|---|---|---|---|---|---|");
    let mut paths = Vec::new();
    let mut ratios = Vec::new();
    for schema in SCHEMAS {
        let (path, rebuild) = build_variant(src, work, schema);
        let bytes = db_bytes(&path) as f64;
        let parts = size_breakdown(&open_read(&path));
        let part = |name: &str| {
            parts.iter().find(|(n, _)| n == name).map_or(0.0, |(_, b)| *b as f64)
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
        .query_map([], |r| Ok(format!("{} {}", r.get::<_, String>(0)?, mib(r.get::<_, i64>(1)? as f64))))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    println!("\nbaseline objects (MiB): {}", objects.join(", "));
    let avg: String = base
        .query_row(
            "SELECT printf('safe_text %.1f, safe_hash %.1f, keyed_fingerprint %.1f, provenance %.1f, locator %.1f',
                 avg(length(safe_text)), avg(length(safe_hash)), avg(length(keyed_fingerprint)),
                 avg(length(provenance)), avg(length(locator)))
             FROM observation",
            [],
            |r| r.get(0),
        )
        .unwrap();
    println!("avg observation column bytes: {avg}");
    let max_id: i64 = base.query_row("SELECT max(id) FROM observation", [], |r| r.get(0)).unwrap();
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
    for (order, ranged) in [(Order::Rank, false), (Order::RowidDesc, false), (Order::Rank, true), (Order::RowidDesc, true)] {
        let sql = format!("EXPLAIN QUERY PLAN {}", fts_sql(order, ranged));
        let mut stmt = base.prepare(&sql).unwrap();
        let params = fts_params("\"git\"", 201, ranged.then_some(lower));
        let plan: Vec<String> = stmt
            .query_map(params_from_iter(&params), |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        println!("- {} {}: {}", order.label(), if ranged { "+ rowid range" } else { "" }, plan.join("; "));
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
                        let params = fts_params(&escape(q.text, q.prefix), page + 1, ranged.then_some(lower));
                        let mut rows = Vec::new();
                        let (stats, _) = measure(runs, || {
                            rows = fts_rowids(&conn, &sql, &params).expect("fts query");
                            rows.len()
                        });
                        results.push(Measured { schema: si, order, ranged, query: qi, page, stats, rows });
                    }
                }
            }
        }
        eprintln!("timed {}", SCHEMAS[si].name);
    }

    let find = |si: usize, order: Order, ranged: bool, qi: usize, page: usize| {
        results
            .iter()
            .find(|m| m.schema == si && m.order == order && m.ranged == ranged && m.query == qi && m.page == page)
            .unwrap()
    };

    // --- Per-config tables. ---
    for (si, schema) in SCHEMAS.iter().enumerate() {
        for ranged in [false, true] {
            println!(
                "\n## {}{}\n",
                schema.name,
                if ranged { " + V5 newest-10% rowid range" } else { "" }
            );
            println!("| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |");
            println!("|---|---|---|---|---|---|---|---|");
            for (qi, q) in QUERIES.iter().enumerate() {
                let same = [Order::Rank, Order::RowidDesc]
                    .iter()
                    .flat_map(|&o| PAGES.iter().map(move |&p| (o, p)))
                    .all(|(o, p)| find(si, o, ranged, qi, p).rows == find(0, o, ranged, qi, p).rows);
                println!(
                    "| `{}` | {} | {} | {} | {} | {} | {} | {} |",
                    escape(q.text, q.prefix),
                    q.class,
                    if ranged { recent_matches[qi] } else { matches[qi] },
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
    println!("| variant | order | V5 range | ratio | all queries | prefix >= 2 chars | prefix >= 3 chars | queries > 20 ms p95 (all) |");
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
                    if slow.is_empty() { "none".to_owned() } else { slow.join(" ") }
                );
            }
        }
    }

    // --- V1: what a minimum prefix length removes. ---
    println!("\n## V1: queries skipped by a minimum prefix length (baseline, rank, page 200)\n");
    println!("| query | prefix chars | matches | baseline rank p95 | runs at min 2 | runs at min 3 |");
    println!("|---|---|---|---|---|---|");
    for (qi, q) in QUERIES.iter().enumerate() {
        let n = prefix_len(q);
        let runs_at = |min: usize| if n.is_none_or(|n| n >= min) { "yes" } else { "skipped" };
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
        ("NEAR(\"git\" \"commit\", 1)", "NEAR (not user-reachable; Relic escapes it)"),
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

    println!("\nload after: {}", shell("sysctl", &["-n", "vm.loadavg"]));
}

/// Store-like N+1 (FTS rowids, then one lookup per row) against a single
/// statement that joins the FTS page to `observation`.
fn end_to_end(path: &Path, name: &str, with_store: bool, runs: usize) {
    let conn = open_read(path);
    let page = 200;
    let e2e_queries = [("g", true), ("gi", true), ("git", true), ("cargo", false), ("git com", true)];

    println!("### {name}\n");
    let mut header = "| query | order | FTS only | FTS + N+1 lookups | JOIN on FTS subquery | naive JOIN |".to_owned();
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
            assert!(n == n2 && n == n3 && n == n4, "e2e shapes disagree on row count");

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
                    mode: if prefix { MatchMode::PrefixLast } else { MatchMode::Words },
                };
                let (s, _) = measure(runs, || app::search(store, &req).unwrap().hits.len());
                line.push_str(&format!(" {} |", cell(s)));
            } else if store.is_some() {
                line.push_str(" n/a |");
            }
            println!("{line}");
        }
    }
    println!();
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

    fn rowid_desc(conn: &Connection, query: &str, prefix: bool) -> Result<Vec<i64>, rusqlite::Error> {
        fts_rowids(conn, &fts_sql(Order::RowidDesc, false), &fts_params(&escape(query, prefix), 100, None))
    }

    /// The bench's baseline query must return exactly what Store returns, in
    /// the same order, or its numbers describe some other search.
    #[test]
    fn baseline_query_matches_store_search_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture_db(dir.path());
        let store = Store::open(&db).unwrap();
        let conn = open_read(&db);
        for query in ["g", "gi", "gi pu", "git c", "git pu", "src/main.rs", "\""] {
            let expected: Vec<i64> =
                store.search_prefix(query, 100).unwrap().iter().map(|h| h.id).collect();
            let got = fts_rowids(&conn, &fts_sql(Order::Rank, false), &fts_params(&escape(query, true), 100, None))
                .unwrap_or_default();
            assert_eq!(got, expected, "query {query:?}");
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
            .query_row("SELECT sql FROM sqlite_master WHERE name = 'observation_fts'", [], |r| r.get(0))
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
        let recent = fts_rowids(&conn, &fts_sql(Order::RowidDesc, true), &fts_params("\"git\"", 100, Some(all[1])))
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
}
