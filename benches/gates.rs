//! The gate harness (ADR 0014): every performance gate result comes from here.
//!
//! Each sample is a fresh process, each gate takes at least `MIN_SAMPLES`, and
//! p95 is nearest-rank. Raw JSON goes to stdout and a summary to stderr. Exit
//! status: 0 when every gate passes, 1 when one fails, 2 when nothing could be
//! measured.
//!
//! Headless and full builds both land at `<target>/release/relic`, so build
//! them into separate target directories:
//!
//!   cargo build --release --no-default-features --target-dir target/gates/headless
//!   cargo build --release --target-dir target/gates/full
//!   cargo run --release --example gates -- \
//!       target/gates/headless/release/relic target/gates/full/release/relic [SAMPLES] \
//!       > target/gates/report.json
//!
//! Write the report under `target/`: a new file in the work tree makes
//! `git_dirty` true. `git_rev` is the commit of the directory the harness runs
//! in, so run it from the tree the binaries were built from.
//!
//! Run it on an idle machine. Straight after a release build the samples drift
//! down for tens of runs and p95 reads about twice its settled value; `loadavg`
//! and the min/p50/max beside each p95 are there to show that.
//!
//! The import gates (ADR 0002) build 1M- and 2M-record histories in the temp
//! directory and import each `SAMPLES` times, so a run takes several minutes
//! and about 1 GiB of temp space at its peak.
//!
//! The search gates (ADR 0006, ADR 0007) query the 1M store those imports
//! leave behind, a fresh process per sample.
//!
//! A signal skips cleanup and leaves the stripped copies in the temp directory.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use serde_json::{Value, json};

const USAGE: &str = "usage: gates <HEADLESS_BIN> <FULL_BIN> [SAMPLES >= 20]";
const MIN_SAMPLES: usize = 20;
/// Discarded runs before sampling, so "warm" means the stripped copy has been
/// paged in and has passed its first-exec checks.
const WARMUPS: usize = 3;
const MIB: f64 = 1024.0 * 1024.0;

const HELP_P95_LIMIT_MS: f64 = 15.0;
/// Upper bounds of the 8–12 MiB headless and 12–18 MiB full targets.
const HEADLESS_LIMIT_MIB: f64 = 12.0;
const FULL_LIMIT_MIB: f64 = 18.0;
const IMPORT_RSS_LIMIT_MIB: f64 = 64.0;
/// "No material growth" from 1M to 2M records (ADR 0002), as a ratio: memory
/// that scaled with the file would read 2.0.
const RSS_GROWTH_LIMIT: f64 = 1.25;
const APPEND_P95_LIMIT_MS: f64 = 25.0;
const IMPORT_100K_P95_LIMIT_MS: f64 = 1000.0;
const IMPORT_1M_P95_LIMIT_MS: f64 = 10_000.0;
/// Store bytes over raw source bytes on the 1M fixture, as restated by
/// ADR 0008 from this harness's own measurement.
const STORE_RATIO_LIMIT: f64 = 3.0;
const WHOLE_WORD_P95_LIMIT_MS: f64 = 20.0;
/// What a search that the deadline stops may take past the deadline, as a
/// fresh process: start-up and the read-only open (3 to 5 ms measured), the
/// stop itself (under 2 ms after the deadline, measured in-process at 1M) and
/// scheduling noise.
const DEADLINE_EPSILON_MS: f64 = 20.0;

fn main() -> ExitCode {
    // `run` returns instead of exiting so its temp directory is always removed.
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(problem) => {
            eprintln!("gates: {problem}\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// `Ok(false)` when a gate failed.
fn run() -> Result<bool, String> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(true);
    }
    let (headless, full, samples) = match args.as_slice() {
        [headless, full] => (headless, full, MIN_SAMPLES),
        [headless, full, samples] => (headless, full, parse_samples(&samples.to_string_lossy())?),
        _ => return Err("expected two binaries".into()),
    };

    let machine = machine();
    let work = tempfile::tempdir().map_err(|e| format!("temp dir: {e}"))?;
    let mut gates = Vec::new();
    let mut binaries = Vec::new();
    for (build, source, limit_mib) in [
        ("headless", Path::new(headless), HEADLESS_LIMIT_MIB),
        ("full", Path::new(full), FULL_LIMIT_MIB),
    ] {
        let version = identify(source, build)?;
        let stripped = work.path().join(format!("relic-{build}"));
        std::fs::copy(source, &stripped).map_err(|e| format!("{}: {e}", source.display()))?;
        succeed(Command::new("strip").arg(&stripped))?;
        // strip exits 0 on a file it could not process, and can break a signature.
        identify(&stripped, build)?;
        let bytes = std::fs::metadata(&stripped)
            .map_err(|e| format!("stat: {e}"))?
            .len();
        binaries.push(json!({
            "build": build,
            "version": version,
            "source_path": source,
            "source_sha256": sha256(source)?,
            "stripped_bytes": bytes,
            "stripped_sha256": sha256(&stripped)?,
        }));

        gates.push(gate(
            format!("stripped {build} binary"),
            "MiB",
            bytes as f64 / MIB,
            limit_mib,
        ));

        let ms = repeat(WARMUPS, samples, || {
            let mut cmd = Command::new(&stripped);
            timed(cmd.arg("--help"))
        })?;
        gates.push(p95_gate(
            format!("warm --help, {build}"),
            ms,
            HELP_P95_LIMIT_MS,
        ));
        if build == "headless" {
            import_gates(&stripped, work.path(), samples, &mut gates)?;
        }
    }

    for g in &gates {
        eprintln!(
            "{} {}: {:.2} {} (limit {})",
            if g["pass"] == true { "PASS" } else { "FAIL" },
            g["gate"].as_str().unwrap(),
            g["value"].as_f64().unwrap(),
            g["unit"].as_str().unwrap(),
            g["limit"],
        );
    }
    let passed = all_pass(&gates);

    let report = json!({
        "schema_version": 1,
        "machine": machine,
        "binaries": binaries,
        "gates": gates,
    });
    writeln!(
        std::io::stdout(),
        "{}",
        serde_json::to_string_pretty(&report).unwrap()
    )
    .map_err(|e| format!("stdout: {e}"))?;
    Ok(passed)
}

fn parse_samples(arg: &str) -> Result<usize, String> {
    match arg.parse() {
        Ok(n) if n >= MIN_SAMPLES => Ok(n),
        _ => Err(format!(
            "SAMPLES must be a number >= {MIN_SAMPLES}, got {arg:?}"
        )),
    }
}

fn gate(name: String, unit: &str, value: f64, limit: f64) -> Value {
    json!({ "gate": name, "unit": unit, "value": value, "limit": limit, "pass": value < limit })
}

fn all_pass(gates: &[Value]) -> bool {
    gates.iter().all(|g| g["pass"] == true)
}

/// The `--version` line of `bin`, which must be a release relic of `build`.
/// Without this the harness would time and pass whatever it was handed.
fn identify(bin: &Path, build: &str) -> Result<String, String> {
    let out = Command::new(bin)
        .arg("--version")
        .output()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let marker = if build == "headless" {
        "(headless)"
    } else {
        "(tui)"
    };
    if out.status.success() && version.starts_with("relic ") && version.ends_with(marker) {
        Ok(version)
    } else {
        Err(format!(
            "{} is not a release {build} relic: `--version` printed {version:?}, expected `relic <version> {marker}`",
            bin.display()
        ))
    }
}

/// The import gates of ADR 0002, on the headless binary: peak RSS of a whole
/// import at 1M and 2M records, and 100 records appended to a 1M store.
fn import_gates(
    relic: &Path,
    work: &Path,
    samples: usize,
    gates: &mut Vec<Value>,
) -> Result<(), String> {
    let io = |e: std::io::Error| format!("import fixture: {e}");
    let history = work.join("zsh_history");
    let db = work.join("import.db");
    let import = || {
        let mut cmd = Command::new(relic);
        cmd.arg("import")
            .arg(&history)
            .args(["--shell", "zsh", "--db"])
            .arg(&db);
        cmd
    };
    let fresh_db = || {
        for suffix in ["", "-wal", "-shm", ".key"] {
            let mut path = db.clone().into_os_string();
            path.push(suffix);
            let _ = fs::remove_file(path);
        }
    };

    // Seed 77 is the corpus every earlier 1M measurement used.
    fs::write(
        &history,
        relic::fixture::generate_diverse_zsh_history(100_000, 77),
    )
    .map_err(io)?;
    let ms = repeat(WARMUPS, samples, || {
        fresh_db();
        timed(&mut import())
    })?;
    gates.push(p95_gate(
        "import 100k records".into(),
        ms,
        IMPORT_100K_P95_LIMIT_MS,
    ));

    let mut peaks = Vec::new();
    // 1M last, so its store is the one measured for size and grown by the append gate.
    for (label, records) in [("2M", 2_000_000), ("1M", 1_000_000)] {
        fs::write(
            &history,
            relic::fixture::generate_diverse_zsh_history(records, 77),
        )
        .map_err(io)?;
        let mut ms = Vec::new();
        let mib = repeat(0, samples, || {
            fresh_db();
            let t = Instant::now();
            let mib = peak_rss_mib(&mut import())?;
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
            Ok(mib)
        })?;
        let peak = mib.iter().copied().fold(0.0, f64::max);
        let mut g = gate(
            format!("import peak RSS, {label} records"),
            "MiB",
            peak,
            IMPORT_RSS_LIMIT_MIB,
        );
        g["statistic"] = json!("max");
        g["samples"] = json!(mib);
        gates.push(g);
        peaks.push(peak);
        if records == 1_000_000 {
            // Timed through /usr/bin/time, whose own cost is noise at this scale.
            gates.push(p95_gate(
                "import 1M records".into(),
                ms,
                IMPORT_1M_P95_LIMIT_MS,
            ));
        }
    }
    gates.push(gate(
        "import peak RSS, 2M over 1M".into(),
        "ratio",
        peaks[0] / peaks[1],
        RSS_GROWTH_LIMIT,
    ));

    // The import checkpointed, so the WAL is folded in; count it anyway.
    let store_bytes: u64 = ["", "-wal", "-shm"]
        .iter()
        .map(|suffix| {
            let mut path = db.clone().into_os_string();
            path.push(suffix);
            fs::metadata(path).map_or(0, |m| m.len())
        })
        .sum();
    let source_bytes = fs::metadata(&history).map_err(io)?.len();
    gates.push(gate(
        "store bytes over source bytes, 1M records".into(),
        "ratio",
        store_bytes as f64 / source_bytes as f64,
        STORE_RATIO_LIMIT,
    ));

    let mut round = 0;
    let ms = repeat(WARMUPS, samples, || {
        round += 1;
        let more = relic::fixture::generate_zsh_history(100, round);
        fs::OpenOptions::new()
            .append(true)
            .open(&history)
            .and_then(|mut f| f.write_all(&more))
            .map_err(io)?;
        timed(&mut import())
    })?;
    gates.push(p95_gate(
        "append 100 records to a 1M store".into(),
        ms,
        APPEND_P95_LIMIT_MS,
    ));
    search_gates(relic, &db, samples, gates)
}

/// The read-path gates on a 1M store. Whole words must answer in under 20 ms
/// (ADR 0007). Everything else, whatever it costs unbounded, must end within
/// the query deadline plus `DEADLINE_EPSILON_MS` (ADR 0006): with a page,
/// with no match, stopped as too broad, or with every word ignored. Which of
/// those is recorded, not gated; a deadline that did not hold shows as time.
fn search_gates(
    relic: &Path,
    db: &Path,
    samples: usize,
    gates: &mut Vec<Value>,
) -> Result<(), String> {
    let hot = |n: usize| format!("{}ta", "task/".repeat(n - 1));
    let git_400 = vec!["git"; 400];
    let (phrase_51, phrase_16) = (hot(51), hot(relic::search::MAX_WORD_TOKENS));
    // In the fixture `task` is in 93% of rows and never beside `git`.
    let whole_words: [&[&str]; 5] = [
        &["git"],
        &["task"],
        &["cargo"],
        &["git", "commit"],
        &["cargo", "task"],
    ];
    let bounded: [(&str, Vec<&str>); 9] = [
        ("hot prefix `ta*`", vec!["--prefix", "ta"]),
        ("hot prefix `task*`", vec!["--prefix", "task"]),
        (
            "lead word and hot prefix `git tas*`",
            vec!["--prefix", "git", "tas"],
        ),
        ("sparse AND `git task`", vec!["git", "task"]),
        (
            "sparse AND `docker git*`",
            vec!["--prefix", "docker", "git"],
        ),
        ("`task` by rank", vec!["--rank", "task"]),
        ("400 x `git` by rank", [vec!["--rank"], git_400].concat()),
        ("one word of 51 hot tokens", vec!["--prefix", &phrase_51]),
        ("one word of 16 hot tokens", vec!["--prefix", &phrase_16]),
    ];
    let deadline_ms = relic::search::QUERY_DEADLINE.as_secs_f64() * 1000.0;

    let mut measure = |name: String, args: &[&str], limit: f64| -> Result<(), String> {
        let mut outcomes = std::collections::BTreeSet::new();
        let ms = repeat(WARMUPS, samples, || {
            let mut cmd = Command::new(relic);
            cmd.arg("search")
                .arg("--json")
                .arg("--db")
                .arg(db)
                .args(args);
            let (ms, how) = timed_search(&mut cmd)?;
            outcomes.insert(how);
            Ok(ms)
        })?;
        let mut g = p95_gate(name, ms, limit);
        g["outcomes"] = json!(outcomes);
        gates.push(g);
        Ok(())
    };
    for words in whole_words {
        measure(
            format!("search 1M, whole words `{}`", words.join(" ")),
            words,
            WHOLE_WORD_P95_LIMIT_MS,
        )?;
    }
    for (name, args) in &bounded {
        measure(
            format!("search 1M within the deadline, {name}"),
            args,
            deadline_ms + DEADLINE_EPSILON_MS,
        )?;
    }
    Ok(())
}

/// Wall time in ms of one `relic search`, and how it ended. Anything but a
/// page, no match, a stop at the deadline or an ignored query is a harness
/// failure.
fn timed_search(cmd: &mut Command) -> Result<(f64, String), String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let t = Instant::now();
    let out = cmd.output().map_err(|e| format!("{cmd:?}: {e}"))?;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let outcome = match out.status.code() {
        Some(0) => "page",
        Some(1) => "no match",
        Some(2) if stderr.contains("too broad") => "too broad",
        // A word past the token cap is dropped, and a query left empty is a
        // usage error (#22); it still had to end within the deadline.
        Some(2) if stderr.contains("every word was ignored") => "ignored",
        _ => return Err(format!("{cmd:?} exited with {}: {stderr}", out.status)),
    };
    Ok((ms, outcome.into()))
}

/// `warmups` discarded runs, then `samples` measured ones.
fn repeat(
    warmups: usize,
    samples: usize,
    mut run: impl FnMut() -> Result<f64, String>,
) -> Result<Vec<f64>, String> {
    for _ in 0..warmups {
        run()?;
    }
    (0..samples).map(|_| run()).collect()
}

/// Wall time in ms of one fresh process, spawn to exit.
fn timed(cmd: &mut Command) -> Result<f64, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let t = Instant::now();
    // A failing command exits fast; timing it would pass the gate.
    succeed(cmd)?;
    Ok(t.elapsed().as_secs_f64() * 1000.0)
}

/// Peak resident set of one fresh process, as `/usr/bin/time` reports it
/// (the measurement ADR 0014 names).
fn peak_rss_mib(cmd: &mut Command) -> Result<f64, String> {
    let (flag, label, unit) = if cfg!(target_os = "macos") {
        ("-l", "maximum resident set size", 1.0)
    } else {
        ("-v", "Maximum resident set size (kbytes):", 1024.0)
    };
    let out = Command::new("/usr/bin/time")
        .arg(flag)
        .arg(cmd.get_program())
        .args(cmd.get_args())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("/usr/bin/time: {e}"))?;
    let report = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(format!("{cmd:?} exited with {}: {report}", out.status));
    }
    report
        .lines()
        .find(|l| l.contains(label))
        .and_then(|l| l.replace(label, "").trim().parse::<f64>().ok())
        .map(|n| n * unit / MIB)
        .ok_or_else(|| format!("no peak RSS in /usr/bin/time output: {report}"))
}

fn p95_gate(name: String, mut ms: Vec<f64>, limit: f64) -> Value {
    let raw = ms.clone();
    ms.sort_by(f64::total_cmp);
    let mut g = gate(name, "ms", nearest_rank(&ms, 95), limit);
    g["statistic"] = json!("nearest-rank p95");
    g["min"] = json!(ms[0]);
    g["p50"] = json!(nearest_rank(&ms, 50));
    g["max"] = json!(ms[ms.len() - 1]);
    g["warmups"] = json!(WARMUPS);
    g["samples"] = json!(raw);
    g
}

/// The ⌈pct/100·N⌉-th smallest of `sorted`, with no interpolation.
fn nearest_rank(sorted: &[f64], pct: usize) -> f64 {
    sorted[(sorted.len() * pct).div_ceil(100) - 1]
}

fn succeed(cmd: &mut Command) -> Result<(), String> {
    match cmd.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("{cmd:?} exited with {status}")),
        Err(e) => Err(format!("{cmd:?}: {e}")),
    }
}

fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn sha256(path: &Path) -> Result<String, String> {
    let arg = path.to_str().ok_or("binary path is not UTF-8")?;
    output("shasum", &["-a", "256", arg])
        .or_else(|| output("sha256sum", &[arg]))
        .and_then(|line| line.split_whitespace().next().map(str::to_string))
        .ok_or_else(|| format!("cannot hash {arg}: neither shasum nor sha256sum worked"))
}

fn cpu() -> Option<String> {
    if cfg!(target_os = "macos") {
        return output("sysctl", &["-n", "machdep.cpu.brand_string"]);
    }
    let info = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let line = info.lines().find(|l| l.starts_with("model name"))?;
    Some(line.split_once(':')?.1.trim().to_string())
}

fn loadavg() -> Option<String> {
    if cfg!(target_os = "macos") {
        return output("sysctl", &["-n", "vm.loadavg"]);
    }
    Some(
        std::fs::read_to_string("/proc/loadavg")
            .ok()?
            .trim()
            .to_string(),
    )
}

/// A field the report records as null still leaves a trace on stderr.
fn known<T>(field: &str, value: Option<T>) -> Option<T> {
    if value.is_none() {
        eprintln!("gates: warning: could not determine {field}; recording null");
    }
    value
}

fn machine() -> Value {
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "kernel": known("kernel", output("uname", &["-sr"])),
        "cpu": known("cpu", cpu()),
        "loadavg": known("loadavg", loadavg()),
        "rustc": known("rustc", output("rustc", &["-V"])),
        // The harness and the measured binaries share one Cargo.lock, so this
        // is their bundled SQLite as long as both were built from this tree.
        "sqlite": rusqlite::version(),
        "git_rev": known("git_rev", output("git", &["rev-parse", "HEAD"])),
        "git_dirty": known(
            "git_dirty",
            output("git", &["status", "--porcelain"]).map(|s| !s.is_empty()),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_nearest_rank() {
        let one_to = |n: usize| (1..=n).map(|i| i as f64).collect::<Vec<_>>();
        assert_eq!(nearest_rank(&one_to(20), 95), 19.0);
        assert_eq!(nearest_rank(&one_to(21), 95), 20.0);
        assert_eq!(nearest_rank(&one_to(100), 95), 95.0);
        assert_eq!(nearest_rank(&one_to(20), 50), 10.0);
    }

    #[test]
    fn fewer_than_twenty_samples_is_refused() {
        assert!(parse_samples("19").is_err());
        assert_eq!(parse_samples("20"), Ok(20));
        assert_eq!(parse_samples("21"), Ok(21));
    }

    #[test]
    fn a_gate_passes_only_below_its_limit_and_one_failure_fails_the_run() {
        let under = gate("under".into(), "ms", 14.9, 15.0);
        let at = gate("at".into(), "ms", 15.0, 15.0);
        assert_eq!(under["pass"], true);
        assert_eq!(at["pass"], false);
        assert!(all_pass(std::slice::from_ref(&under)));
        assert!(!all_pass(&[under, at]));
    }
}
