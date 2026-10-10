//! The gate harness (ADR 0014): every performance gate result comes from here.
//!
//! Each sample is a fresh process, each gate takes at least `MIN_SAMPLES`, and
//! p95 is nearest-rank. Raw JSON goes to stdout, a summary to stderr, and the
//! exit status is 1 when a gate fails.
//!
//! Headless and full builds both land at `<target>/release/relic`, so build
//! them into separate target directories:
//!
//!   cargo build --release --no-default-features --target-dir target/gates/headless
//!   cargo build --release --target-dir target/gates/full
//!   cargo run --release --example gates -- \
//!       target/gates/headless/release/relic target/gates/full/release/relic [SAMPLES]
//!
//! Run it on an idle machine. Straight after a release build the samples drift
//! down for tens of runs and p95 reads about twice its settled value.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use serde_json::{Value, json};

const MIN_SAMPLES: usize = 20;
/// Discarded runs before sampling, so "warm" means the stripped copy has been
/// paged in and has passed its first-exec checks.
const WARMUPS: usize = 3;
const MIB: f64 = 1024.0 * 1024.0;

const HELP_P95_LIMIT_MS: f64 = 15.0;
/// Upper bounds of the 8–12 MiB headless and 12–18 MiB full targets.
const HEADLESS_LIMIT_MIB: f64 = 12.0;
const FULL_LIMIT_MIB: f64 = 18.0;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (headless, full, samples) = match args.as_slice() {
        [headless, full] => (headless, full, MIN_SAMPLES),
        [headless, full, samples] => (headless, full, parse_samples(samples)),
        _ => usage("expected two binaries"),
    };

    let work = tempfile::tempdir().expect("temp dir");
    let mut gates = Vec::new();
    let mut binaries = Vec::new();
    for (build, source, limit_mib) in [
        ("headless", headless, HEADLESS_LIMIT_MIB),
        ("full", full, FULL_LIMIT_MIB),
    ] {
        let stripped = work.path().join(format!("relic-{build}"));
        std::fs::copy(source, &stripped).unwrap_or_else(|e| usage(&format!("{source}: {e}")));
        must(Command::new("strip").arg(&stripped), "strip");
        let bytes = std::fs::metadata(&stripped).expect("stat stripped binary").len();
        binaries.push(json!({
            "build": build,
            "source_path": source,
            "stripped_bytes": bytes,
            "stripped_sha256": sha256(&stripped),
        }));

        let mib = bytes as f64 / MIB;
        gates.push(json!({
            "gate": format!("stripped {build} binary"),
            "unit": "MiB",
            "value": mib,
            "limit": limit_mib,
            "pass": mib <= limit_mib,
        }));

        let ms = sample(&stripped, &["--help"], samples);
        let p95 = nearest_rank_p95(&ms);
        gates.push(json!({
            "gate": format!("warm --help, {build}"),
            "unit": "ms",
            "value": p95,
            "limit": HELP_P95_LIMIT_MS,
            "pass": p95 < HELP_P95_LIMIT_MS,
            "statistic": "nearest-rank p95",
            "warmups": WARMUPS,
            "samples": ms,
        }));
    }

    let mut failed = false;
    for g in &gates {
        let pass = g["pass"] == true;
        failed |= !pass;
        eprintln!(
            "{} {}: {:.2} {} (limit {})",
            if pass { "PASS" } else { "FAIL" },
            g["gate"].as_str().unwrap(),
            g["value"].as_f64().unwrap(),
            g["unit"].as_str().unwrap(),
            g["limit"],
        );
    }

    let report = json!({
        "schema_version": 1,
        "machine": machine(),
        "binaries": binaries,
        "gates": gates,
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    if failed {
        std::process::exit(1);
    }
}

fn usage(problem: &str) -> ! {
    eprintln!("gates: {problem}");
    eprintln!("usage: gates <HEADLESS_BIN> <FULL_BIN> [SAMPLES >= {MIN_SAMPLES}]");
    std::process::exit(2);
}

fn parse_samples(arg: &str) -> usize {
    match arg.parse() {
        Ok(n) if n >= MIN_SAMPLES => n,
        _ => usage(&format!("SAMPLES must be a number >= {MIN_SAMPLES}, got {arg:?}")),
    }
}

/// Wall time in ms of `samples` fresh processes, spawn to exit, after
/// `WARMUPS` discarded runs.
fn sample(bin: &Path, args: &[&str], samples: usize) -> Vec<f64> {
    let run = || {
        let mut cmd = Command::new(bin);
        cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let t = Instant::now();
        // A failing command exits fast; timing it would pass the gate.
        must(&mut cmd, "sample");
        t.elapsed().as_secs_f64() * 1000.0
    };
    for _ in 0..WARMUPS {
        run();
    }
    (0..samples).map(|_| run()).collect()
}

/// The ⌈0.95·N⌉-th smallest sample, with no interpolation.
fn nearest_rank_p95(samples: &[f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[(sorted.len() * 95).div_ceil(100) - 1]
}

fn must(cmd: &mut Command, what: &str) {
    match cmd.status() {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("{what}: {cmd:?} exited with {status}"),
        Err(e) => panic!("{what}: {cmd:?}: {e}"),
    }
}

fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn sha256(path: &Path) -> Option<String> {
    let path = path.to_str()?;
    let line = output("shasum", &["-a", "256", path]).or_else(|| output("sha256sum", &[path]))?;
    line.split_whitespace().next().map(str::to_string)
}

fn cpu() -> Option<String> {
    if cfg!(target_os = "macos") {
        return output("sysctl", &["-n", "machdep.cpu.brand_string"]);
    }
    let info = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let line = info.lines().find(|l| l.starts_with("model name"))?;
    Some(line.split_once(':')?.1.trim().to_string())
}

fn machine() -> Value {
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "kernel": output("uname", &["-sr"]),
        "cpu": cpu(),
        "rustc": output("rustc", &["-V"]),
        // The harness and the measured binaries share one Cargo.lock, so this
        // is their bundled SQLite as long as both were built from this tree.
        "sqlite": rusqlite::version(),
        "git_rev": output("git", &["rev-parse", "HEAD"]),
        "git_dirty": output("git", &["status", "--porcelain"]).map(|s| !s.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p95_of_1_to(n: usize) -> f64 {
        // Descending, so a missing sort is caught.
        let samples: Vec<f64> = (1..=n).rev().map(|i| i as f64).collect();
        nearest_rank_p95(&samples)
    }

    #[test]
    fn p95_is_nearest_rank() {
        assert_eq!(p95_of_1_to(20), 19.0);
        assert_eq!(p95_of_1_to(21), 20.0);
        assert_eq!(p95_of_1_to(100), 95.0);
    }
}
