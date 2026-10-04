//! Build a deterministic fixture database for the interactive search prototype.
//!
//! Run with: cargo run --release --bin fixture_db -- <OUT.db> <RECORDS>

use std::time::Instant;

use relic::fixture;
use relic::import::{self, SourceKind};
use relic::redact::Redactor;
use relic::store::Store;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, records] = args.as_slice() else {
        eprintln!("usage: fixture_db <OUT.db> <RECORDS>");
        std::process::exit(2);
    };
    let records: usize = records.parse().expect("RECORDS must be a number");
    if std::path::Path::new(out).exists() {
        eprintln!("{out} already exists; refusing to append a second copy");
        std::process::exit(1);
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let history = dir.path().join("zsh_history");
    // Seed 77 matches the #8 diverse corpus, so latencies are comparable.
    std::fs::write(&history, fixture::generate_diverse_zsh_history(records, 77)).unwrap();

    let t = Instant::now();
    let mut store = Store::open(out.as_ref()).expect("open db");
    let r = import::import_file(&mut store, &history, SourceKind::Zsh, &Redactor::new())
        .expect("import");
    store.checkpoint().unwrap();
    println!(
        "{out}: {} observations in {:.1}s",
        store.observation_count().unwrap(),
        t.elapsed().as_secs_f64()
    );
    assert_eq!(r.records_stored, records);
}
