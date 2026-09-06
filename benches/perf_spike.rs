//! Performance spike benchmark for issue #8.
//!
//! Measures import throughput, FTS search latency, no-change detection,
//! memory, and database size against the acceptance gates.
//!
//! Run with: cargo run --release --bin perf_spike

use std::fs;
use std::path::Path;
use std::time::Instant;

use relic::fixture;
use relic::import::{self, SourceKind};
use relic::redact::Redactor;
use relic::store::Store;

fn main() {
    println!("=== Relic Performance Spike ===\n");

    let dir = tempfile::tempdir().expect("create temp dir");
    let db_path = dir.path().join("bench.db");

    // Generate fixtures.
    println!("Generating fixtures...");
    let zsh_100k = fixture::generate_zsh_history(80_000, 42);
    let bash_100k = fixture::generate_bash_history(20_000, 43);
    let zsh_1m = fixture::generate_zsh_history(800_000, 44);
    let bash_200k = fixture::generate_bash_history(200_000, 45);

    let zsh_100k_path = dir.path().join("zsh_100k");
    let bash_100k_path = dir.path().join("bash_100k");
    let zsh_1m_path = dir.path().join("zsh_1m");
    let bash_200k_path = dir.path().join("bash_200k");

    fs::write(&zsh_100k_path, &zsh_100k).unwrap();
    fs::write(&bash_100k_path, &bash_100k).unwrap();
    fs::write(&zsh_1m_path, &zsh_1m).unwrap();
    fs::write(&bash_200k_path, &bash_200k).unwrap();

    println!(
        "  zsh 80k: {} bytes, bash 20k: {} bytes",
        zsh_100k.len(),
        bash_100k.len()
    );
    println!(
        "  zsh 800k: {} bytes, bash 200k: {} bytes\n",
        zsh_1m.len(),
        bash_200k.len()
    );

    let redactor = Redactor::new();

    // --- Gate: import 100k mixed native records < 1s ---
    println!("--- Import 100k mixed records ---");
    {
        let mut store = Store::open(&db_path).unwrap();
        let t = Instant::now();
        let r1 = import::import_file(&mut store, &zsh_100k_path, SourceKind::Zsh, &redactor).unwrap();
        let r2 = import::import_file(&mut store, &bash_100k_path, SourceKind::Bash, &redactor).unwrap();
        let elapsed = t.elapsed();
        let total = r1.records_stored + r2.records_stored;
        println!(
            "  {total} records in {:.1}ms  (gate: <1000ms) {}",
            elapsed.as_secs_f64() * 1000.0,
            gate(elapsed.as_millis() < 1000)
        );
        store.checkpoint().unwrap();
    }

    // Check DB size after checkpoint.
    let db_size = file_size(&db_path);
    let source_size = zsh_100k.len() + bash_100k.len();
    let ratio = db_size as f64 / source_size as f64;
    println!(
        "  DB size: {} bytes ({:.1}x source)  (gate: <2x) {}\n",
        db_size,
        ratio,
        gate(ratio < 2.0)
    );

    // --- Gate: FTS query over 100k, warm < 20ms ---
    println!("--- FTS search (100k records) ---");
    {
        let store = Store::open(&db_path).unwrap();

        // Warm up.
        let _ = store.search("cargo", 10);

        let queries = ["cargo", "git push", "docker", "kubectl", "echo hello"];
        for q in &queries {
            let t = Instant::now();
            let hits = store.search(q, 50).unwrap();
            let elapsed = t.elapsed();
            println!(
                "  \"{q}\": {} hits in {:.2}ms  (gate: <20ms) {}",
                hits.len(),
                elapsed.as_secs_f64() * 1000.0,
                gate(elapsed.as_millis() < 20)
            );
        }
    }
    println!();

    // --- Gate: unchanged-source check < 20ms ---
    println!("--- Unchanged-source check ---");
    {
        let mut store = Store::open(&db_path).unwrap();
        let t = Instant::now();
        let r = import::import_file(&mut store, &zsh_100k_path, SourceKind::Zsh, &redactor).unwrap();
        let elapsed = t.elapsed();
        println!(
            "  skipped={}, {:.2}ms  (gate: <20ms) {}",
            r.skipped,
            elapsed.as_secs_f64() * 1000.0,
            gate(r.skipped && elapsed.as_millis() < 20)
        );
    }
    println!();

    // --- Gate: import 1M mixed native records < 10s ---
    println!("--- Import 1M mixed records ---");
    {
        let db_1m_path = dir.path().join("bench_1m.db");
        let mut store = Store::open(&db_1m_path).unwrap();
        let t = Instant::now();
        let r1 = import::import_file(&mut store, &zsh_1m_path, SourceKind::Zsh, &redactor).unwrap();
        let r2 = import::import_file(&mut store, &bash_200k_path, SourceKind::Bash, &redactor).unwrap();
        let elapsed = t.elapsed();
        let total = r1.records_stored + r2.records_stored;
        println!(
            "  {total} records in {:.1}ms  (gate: <10000ms) {}",
            elapsed.as_secs_f64() * 1000.0,
            gate(elapsed.as_millis() < 10000)
        );

        store.checkpoint().unwrap();

        let db_1m_size = file_size(&db_1m_path);
        let source_1m_size = zsh_1m.len() + bash_200k.len();
        let ratio_1m = db_1m_size as f64 / source_1m_size as f64;
        println!(
            "  DB size: {} bytes ({:.1}x source)  (gate: <2x) {}",
            db_1m_size,
            ratio_1m,
            gate(ratio_1m < 2.0)
        );

        // --- Gate: FTS query over 1M, warm < 20ms ---
        println!();
        println!("--- FTS search (1M records) ---");

        // Warm up.
        let _ = store.search("cargo", 10);

        let queries = ["cargo", "git", "docker", "kubectl", "echo"];
        for q in &queries {
            let t = Instant::now();
            let hits = store.search(q, 50).unwrap();
            let elapsed = t.elapsed();
            println!(
                "  \"{q}\": {} hits in {:.2}ms  (gate: <20ms) {}",
                hits.len(),
                elapsed.as_secs_f64() * 1000.0,
                gate(elapsed.as_millis() < 20)
            );
        }
    }
    println!();

    // --- Gate: incremental import 100 records < 25ms ---
    println!("--- Incremental import (100 records) ---");
    {
        let incr_path = dir.path().join("incr_history");
        let data = fixture::generate_zsh_history(100, 99);
        fs::write(&incr_path, &data).unwrap();

        let incr_db = dir.path().join("incr.db");
        let mut store = Store::open(&incr_db).unwrap();
        let t = Instant::now();
        let r = import::import_file(&mut store, &incr_path, SourceKind::Zsh, &redactor).unwrap();
        let elapsed = t.elapsed();
        println!(
            "  {} records in {:.2}ms  (gate: <25ms) {}",
            r.records_stored,
            elapsed.as_secs_f64() * 1000.0,
            gate(elapsed.as_millis() < 25)
        );
    }
    println!();

    // --- Realistic diverse corpus: 1M with varied term frequencies ---
    println!("--- FTS search (1M diverse records) ---");
    {
        let diverse_path = dir.path().join("diverse_1m");
        let diverse_data = fixture::generate_diverse_zsh_history(1_000_000, 77);
        let diverse_source_size = diverse_data.len();
        fs::write(&diverse_path, &diverse_data).unwrap();

        let diverse_db_path = dir.path().join("diverse.db");
        let mut store = Store::open(&diverse_db_path).unwrap();
        let t = Instant::now();
        let r = import::import_file(&mut store, &diverse_path, SourceKind::Zsh, &redactor).unwrap();
        let elapsed = t.elapsed();
        println!(
            "  Import: {} records in {:.1}ms",
            r.records_stored,
            elapsed.as_secs_f64() * 1000.0
        );

        store.checkpoint().unwrap();
        let db_size = file_size(&diverse_db_path);
        let ratio = db_size as f64 / diverse_source_size as f64;
        println!(
            "  DB size: {} bytes ({:.1}x source)  (gate: <2x) {}",
            db_size, ratio, gate(ratio < 2.0)
        );

        // Warm up.
        let _ = store.search("cargo", 10);

        let queries = ["cargo", "git commit", "docker", "kubectl", "python3"];
        for q in &queries {
            let t = Instant::now();
            let hits = store.search(q, 50).unwrap();
            let elapsed = t.elapsed();
            println!(
                "  \"{q}\": {} hits in {:.2}ms  (gate: <20ms) {}",
                hits.len(),
                elapsed.as_secs_f64() * 1000.0,
                gate(elapsed.as_millis() < 20)
            );
        }
    }
    println!();

    // --- RSS measurement ---
    println!("--- Peak RSS ---");
    let rss = get_rss_bytes();
    let rss_mib = rss as f64 / (1024.0 * 1024.0);
    println!(
        "  Current RSS: {:.1} MiB  (gate: <64 MiB) {}",
        rss_mib,
        gate(rss_mib < 64.0)
    );
    println!();

    println!("=== Done ===");
}

fn gate(pass: bool) -> &'static str {
    if pass { "PASS" } else { "FAIL" }
}

fn file_size(path: &Path) -> u64 {
    // Include WAL and SHM files.
    let mut total = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let wal = path.with_extension("db-wal");
    let shm = path.with_extension("db-shm");
    total += fs::metadata(wal).map(|m| m.len()).unwrap_or(0);
    total += fs::metadata(shm).map(|m| m.len()).unwrap_or(0);
    total
}

#[cfg(target_os = "macos")]
fn get_rss_bytes() -> u64 {
    use std::mem;
    unsafe {
        let mut info: libc::rusage = mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut info) == 0 {
            // macOS reports in bytes
            info.ru_maxrss as u64
        } else {
            0
        }
    }
}

#[cfg(target_os = "linux")]
fn get_rss_bytes() -> u64 {
    use std::mem;
    unsafe {
        let mut info: libc::rusage = mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut info) == 0 {
            // Linux reports in kilobytes
            (info.ru_maxrss as u64) * 1024
        } else {
            0
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn get_rss_bytes() -> u64 {
    0
}
