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
//! A signal skips cleanup and leaves the stripped copies in the temp directory.

use std::ffi::OsString;
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
        let bytes = std::fs::metadata(&stripped).map_err(|e| format!("stat: {e}"))?.len();
        binaries.push(json!({
            "build": build,
            "version": version,
            "source_path": source,
            "source_sha256": sha256(source)?,
            "stripped_bytes": bytes,
            "stripped_sha256": sha256(&stripped)?,
        }));

        gates.push(gate(format!("stripped {build} binary"), "MiB", bytes as f64 / MIB, limit_mib));

        let mut ms = sample(&stripped, &["--help"], samples)?;
        let raw = ms.clone();
        ms.sort_by(f64::total_cmp);
        let mut timed =
            gate(format!("warm --help, {build}"), "ms", nearest_rank(&ms, 95), HELP_P95_LIMIT_MS);
        timed["statistic"] = json!("nearest-rank p95");
        timed["min"] = json!(ms[0]);
        timed["p50"] = json!(nearest_rank(&ms, 50));
        timed["max"] = json!(ms[ms.len() - 1]);
        timed["warmups"] = json!(WARMUPS);
        timed["samples"] = json!(raw);
        gates.push(timed);
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
    writeln!(std::io::stdout(), "{}", serde_json::to_string_pretty(&report).unwrap())
        .map_err(|e| format!("stdout: {e}"))?;
    Ok(passed)
}

fn parse_samples(arg: &str) -> Result<usize, String> {
    match arg.parse() {
        Ok(n) if n >= MIN_SAMPLES => Ok(n),
        _ => Err(format!("SAMPLES must be a number >= {MIN_SAMPLES}, got {arg:?}")),
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
    let marker = if build == "headless" { "(headless)" } else { "(tui)" };
    if out.status.success() && version.starts_with("relic ") && version.ends_with(marker) {
        Ok(version)
    } else {
        Err(format!(
            "{} is not a release {build} relic: `--version` printed {version:?}, expected `relic <version> {marker}`",
            bin.display()
        ))
    }
}

/// Wall time in ms of `samples` fresh processes, spawn to exit, after
/// `WARMUPS` discarded runs.
fn sample(bin: &Path, args: &[&str], samples: usize) -> Result<Vec<f64>, String> {
    let run = || {
        let mut cmd = Command::new(bin);
        cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let t = Instant::now();
        // A failing command exits fast; timing it would pass the gate.
        succeed(&mut cmd)?;
        Ok(t.elapsed().as_secs_f64() * 1000.0)
    };
    for _ in 0..WARMUPS {
        run()?;
    }
    (0..samples).map(|_| run()).collect()
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
    let out = Command::new(cmd).args(args).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
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
    Some(std::fs::read_to_string("/proc/loadavg").ok()?.trim().to_string())
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
