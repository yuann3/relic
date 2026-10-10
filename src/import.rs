use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::domain::{Observation, ObservationKind, Provenance};
use crate::redact::Redactor;
use crate::source::{self, RawRecord};
use crate::store::{Cursor, RevisionTx, StatHint, Store, StoreError};

/// Which shell format to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Zsh,
    Bash,
}

/// Result of an import operation.
#[derive(Debug)]
pub struct ImportResult {
    pub records_stored: usize,
    pub revision_id: i64,
    pub skipped: bool,
    /// The records are committed, but a reader's open snapshot kept the WAL
    /// from being folded into the database file.
    pub checkpoint_blocked: bool,
}

/// A single-transaction import leaves the whole import in the WAL, and
/// SQLite's own autocheckpoint is passive: it never shrinks the file, which
/// ADR 0008 counts in the size gate. So fold it in explicitly, but only for a
/// bulk import: 10k records is a few MiB of WAL, about the 1000-page mark
/// where autocheckpoint starts anyway, while the 100-record append has a
/// 25 ms budget that cannot pay for a truncating checkpoint's fsyncs.
const CHECKPOINT_MIN_RECORDS: usize = 10_000;

/// Import a history file into the store as one new revision, or skip it when
/// `stat` says it has not changed since the last import.
pub fn import_file(
    store: &mut Store,
    path: &Path,
    kind: SourceKind,
    redactor: &Redactor,
) -> Result<ImportResult, ImportError> {
    // Lock before looking: an importer that decided first and then waited for
    // another import of this file would store the file a second time.
    let source = store.lock_source(path.as_os_str().as_bytes(), kind)?;
    let stat = stat_hint(&fs::metadata(path).map_err(ImportError::Io)?);
    if source.newest().is_some_and(|r| r.cursor.stat == stat) {
        return Ok(ImportResult {
            records_stored: 0,
            revision_id: 0,
            skipped: true,
            checkpoint_blocked: false,
        });
    }

    let data = fs::read(path).map_err(ImportError::Io)?;
    let raw_records = match kind {
        SourceKind::Zsh => source::zsh::parse(&data),
        SourceKind::Bash => source::bash::parse(&data),
    };
    // The parser cannot say where the last complete record ends yet, so the
    // cursor claims no stable prefix and carries no guard: nothing may resume
    // from it until change detection lands (#24).
    let cursor = Cursor { stat, stable_len: 0, boundary_guard: None };
    let (revision_id, stored) =
        write_records(source.new_revision()?, kind, &cursor, raw_records.into_iter().map(Ok), redactor)?;
    committed(store, revision_id, stored)
}

fn stat_hint(metadata: &fs::Metadata) -> StatHint {
    StatHint {
        dev: metadata.dev() as i64,
        ino: metadata.ino() as i64,
        size: metadata.size() as i64,
        mtime_ns: metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
        ctime_ns: metadata.ctime() * 1_000_000_000 + metadata.ctime_nsec(),
    }
}

/// Stream `records` into one revision transaction and commit it. Any error,
/// from the records or the store, drops the transaction: nothing of this
/// import becomes visible and a retry starts clean. Returns the revision and
/// how many records it gained.
fn write_records(
    mut revision: RevisionTx<'_>,
    kind: SourceKind,
    cursor: &Cursor,
    records: impl Iterator<Item = io::Result<RawRecord>>,
    redactor: &Redactor,
) -> Result<(i64, usize), ImportError> {
    let obs_kind = match kind {
        SourceKind::Zsh => ObservationKind::ZshCommand,
        SourceKind::Bash => ObservationKind::BashCommand,
    };

    let mut provenance = Provenance::COMMAND;
    let mut stored = 0;
    for raw in records {
        let raw = raw.map_err(ImportError::Io)?;
        if raw.timestamp.is_some() {
            provenance = provenance.with(Provenance::OBSERVED_AT);
        }
        revision.insert(&raw_to_observation(&raw, obs_kind, redactor))?;
        stored += 1;
    }
    let revision_id = revision.revision_id();
    revision.commit(cursor, provenance)?;
    Ok((revision_id, stored))
}

/// Checkpoint after a bulk commit. The revision is already durable here, so a
/// checkpoint error is reported as a store error, never as `Busy`: "try
/// again" would be wrong advice for an import that succeeded.
fn committed(store: &Store, revision_id: i64, stored: usize) -> Result<ImportResult, ImportError> {
    let checkpoint_blocked = stored >= CHECKPOINT_MIN_RECORDS
        && !store.checkpoint().map_err(|e| ImportError::Store(StoreError::Sqlite(e)))?;
    Ok(ImportResult { records_stored: stored, revision_id, skipped: false, checkpoint_blocked })
}

fn raw_to_observation(raw: &RawRecord, kind: ObservationKind, redactor: &Redactor) -> Observation {
    Observation {
        kind,
        observed_at_ms: raw.timestamp.map(|t| t * 1000),
        safe_text: redactor.redact(&raw.command),
        flags: 0,
        locator: raw.byte_offset.to_le_bytes().to_vec(),
    }
}

#[derive(Debug)]
pub enum ImportError {
    Io(io::Error),
    /// Another import holds the store's write lock.
    Busy,
    Store(StoreError),
}

impl From<StoreError> for ImportError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Busy => Self::Busy,
            e => Self::Store(e),
        }
    }
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::Busy => write!(f, "{}", StoreError::Busy),
            Self::Store(e) => write!(f, "store error: {e}"),
        }
    }
}

impl std::error::Error for ImportError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn import_zsh_file_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let history_path = dir.path().join("zsh_history");
        {
            let mut f = fs::File::create(&history_path).unwrap();
            write!(
                f,
                ": 1700000000:0;cargo build --release\n\
                 : 1700000001:5;cargo test\n\
                 : 1700000002:0;git push origin main\n"
            )
            .unwrap();
        }

        let mut store = Store::open_memory().unwrap();
        let redactor = Redactor::new();

        let result = import_file(&mut store, &history_path, SourceKind::Zsh, &redactor).unwrap();

        assert_eq!(result.records_stored, 3);

        // Verify search finds imported records.
        let hits = store.search("cargo", 10).unwrap();
        assert_eq!(hits.len(), 2);

        let hits = store.search("git push", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].safe_text.contains("git push"));
    }

    #[test]
    fn import_bash_file_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let history_path = dir.path().join("bash_history");
        {
            let mut f = fs::File::create(&history_path).unwrap();
            write!(
                f,
                "#1700000000\ncargo build\n#1700000005\ngit status\n"
            )
            .unwrap();
        }

        let mut store = Store::open_memory().unwrap();
        let redactor = Redactor::new();

        let result = import_file(&mut store, &history_path, SourceKind::Bash, &redactor).unwrap();

        assert_eq!(result.records_stored, 2);

        let hits = store.search("cargo", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn import_redacts_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let history_path = dir.path().join("zsh_history");
        {
            let mut f = fs::File::create(&history_path).unwrap();
            writeln!(f, ": 1700000000:0;API_KEY=sk12345 cargo deploy").unwrap();
        }

        let mut store = Store::open_memory().unwrap();
        let redactor = Redactor::new();

        import_file(&mut store, &history_path, SourceKind::Zsh, &redactor).unwrap();

        let hits = store.search("cargo", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].safe_text.contains("[REDACTED]"));
        assert!(!hits[0].safe_text.contains("sk12345"));
    }

    #[test]
    fn import_nonexistent_file_returns_error() {
        let mut store = Store::open_memory().unwrap();
        let redactor = Redactor::new();

        let result = import_file(
            &mut store,
            Path::new("/nonexistent/history"),
            SourceKind::Zsh,
            &redactor,
        );
        assert!(result.is_err());
    }

    // --- Slice 8: Incremental no-change detection ---

    #[test]
    fn import_unchanged_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let history_path = dir.path().join("zsh_history");
        {
            let mut f = fs::File::create(&history_path).unwrap();
            write!(f, ": 1700000000:0;cargo build\n: 1700000001:0;cargo test\n").unwrap();
        }

        let mut store = Store::open_memory().unwrap();
        let redactor = Redactor::new();

        // First import — should process.
        let r1 = import_file(&mut store, &history_path, SourceKind::Zsh, &redactor).unwrap();
        assert!(!r1.skipped);
        assert_eq!(r1.records_stored, 2);

        // Second import of the same unchanged file — should skip.
        let r2 = import_file(&mut store, &history_path, SourceKind::Zsh, &redactor).unwrap();
        assert!(r2.skipped);
        assert_eq!(r2.records_stored, 0);

        // Observations should still be 2, not doubled.
        assert_eq!(store.observation_count().unwrap(), 2);
    }

    #[test]
    fn import_changed_file_reimports() {
        let dir = tempfile::tempdir().unwrap();
        let history_path = dir.path().join("zsh_history");
        {
            let mut f = fs::File::create(&history_path).unwrap();
            writeln!(f, ": 1700000000:0;cargo build").unwrap();
        }

        let mut store = Store::open_memory().unwrap();
        let redactor = Redactor::new();

        let r1 = import_file(&mut store, &history_path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!(r1.records_stored, 1);

        // Append to the file: its stat hint no longer matches the stored one.
        {
            let mut f = fs::OpenOptions::new().append(true).open(&history_path).unwrap();
            writeln!(f, ": 1700000001:0;cargo test").unwrap();
        }

        // A changed hint reimports the whole file as a new revision.
        let r2 = import_file(&mut store, &history_path, SourceKind::Zsh, &redactor).unwrap();
        assert!(!r2.skipped);
        assert_eq!(r2.records_stored, 2);
    }

    /// One new revision of `locator` holding `records`, as `import_file`
    /// stores a file it has already read.
    fn import_records(
        store: &mut Store,
        locator: &[u8],
        records: impl Iterator<Item = io::Result<RawRecord>>,
    ) -> Result<ImportResult, ImportError> {
        let revision = store.lock_source(locator, SourceKind::Zsh)?.new_revision()?;
        let (revision_id, stored) =
            write_records(revision, SourceKind::Zsh, &Cursor::default(), records, &Redactor::new())?;
        committed(store, revision_id, stored)
    }

    fn records(n: usize, fail_at: Option<usize>) -> impl Iterator<Item = io::Result<RawRecord>> {
        (0..n).map(move |i| {
            if fail_at == Some(i) {
                return Err(io::Error::other("source went away"));
            }
            Ok(RawRecord {
                timestamp: Some(1_700_000_000 + i as i64),
                duration: None,
                command: format!("echo needle {i}"),
                byte_offset: i,
            })
        })
    }

    /// Rows per table and FTS matches, seen by a connection of its own.
    fn visible(db: &Path) -> [i64; 5] {
        let conn = rusqlite::Connection::open_with_flags(
            db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        [
            "SELECT COUNT(*) FROM source",
            "SELECT COUNT(*) FROM source_revision",
            "SELECT COUNT(*) FROM observation",
            // Counting observation_fts itself would only re-read observation.
            "SELECT COUNT(*) FROM observation_fts_docsize",
            "SELECT COUNT(*) FROM observation_fts WHERE observation_fts MATCH 'needle'",
        ]
        .map(|sql| conn.query_row(sql, [], |r| r.get(0)).unwrap())
    }

    #[test]
    fn interrupted_import_is_invisible_and_a_retry_stores_everything_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let mut store = Store::open(&db).unwrap();

        let err = import_records(&mut store, b"/h", records(50, Some(30))).unwrap_err();
        assert!(matches!(err, ImportError::Io(_)), "{err}");
        assert_eq!(visible(&db), [0, 0, 0, 0, 0]);
        assert!(store.search("needle", 100).unwrap().is_empty());

        let r = import_records(&mut store, b"/h", records(50, None)).unwrap();
        assert_eq!(r.records_stored, 50);
        assert_eq!(visible(&db), [1, 1, 50, 50, 50]);
    }

    #[test]
    fn second_importer_gets_busy_and_succeeds_once_the_first_is_done() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let history_path = dir.path().join("zsh_history");
        fs::write(&history_path, ": 1700000000:0;echo needle\n").unwrap();
        let mut first = Store::open(&db).unwrap();
        let mut second = Store::open(&db).unwrap();
        let redactor = Redactor::new();

        let writing = first.lock_source(b"/other", SourceKind::Zsh).unwrap();
        let started = std::time::Instant::now();
        let err = import_file(&mut second, &history_path, SourceKind::Zsh, &redactor).unwrap_err();
        assert!(matches!(err, ImportError::Busy), "{err}");
        // It waited out busy_timeout rather than failing on first contact.
        assert!(started.elapsed() >= std::time::Duration::from_millis(200), "{:?}", started.elapsed());
        assert_eq!(visible(&db), [0, 0, 0, 0, 0]);

        drop(writing);
        let r = import_file(&mut second, &history_path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!(r.records_stored, 1);
        assert_eq!(visible(&db), [1, 1, 1, 1, 1]);
    }

    #[test]
    fn an_importer_that_waited_for_the_same_file_does_not_store_it_again() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let history_path = dir.path().join("zsh_history");
        fs::write(&history_path, ": 1700000000:0;echo needle\n").unwrap();
        let mut first = Store::open(&db).unwrap();
        let mut second = Store::open(&db).unwrap();
        let locator = history_path.as_os_str().as_bytes();

        let mut writing = first.lock_source(locator, SourceKind::Zsh).unwrap().new_revision().unwrap();
        let raw = RawRecord { timestamp: Some(1_700_000_000), duration: None, command: "echo needle".into(), byte_offset: 0 };
        writing.insert(&raw_to_observation(&raw, ObservationKind::ZshCommand, &Redactor::new())).unwrap();
        let stat = stat_hint(&fs::metadata(&history_path).unwrap());

        let waiter = std::thread::scope(|s| {
            let waiter = s.spawn(|| import_file(&mut second, &history_path, SourceKind::Zsh, &Redactor::new()));
            // Well inside the 250 ms busy_timeout: the waiter gets the lock, not Busy.
            std::thread::sleep(std::time::Duration::from_millis(100));
            writing.commit(&Cursor { stat, ..Cursor::default() }, Provenance::COMMAND).unwrap();
            waiter.join().unwrap()
        });

        assert!(waiter.unwrap().skipped);
        assert_eq!(visible(&db), [1, 1, 1, 1, 1]);
    }

    #[test]
    fn only_a_bulk_import_truncates_the_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let wal_len = || fs::metadata(dir.path().join("relic.db-wal")).unwrap().len();
        let mut store = Store::open(&db).unwrap();

        let small = import_records(&mut store, b"/small", records(100, None)).unwrap();
        assert!(!small.checkpoint_blocked);
        assert!(wal_len() > 0);

        let bulk = import_records(&mut store, b"/bulk", records(CHECKPOINT_MIN_RECORDS, None)).unwrap();
        assert!(!bulk.checkpoint_blocked);
        assert_eq!(wal_len(), 0);
        assert_eq!(visible(&db)[2], 100 + CHECKPOINT_MIN_RECORDS as i64);
    }

    #[test]
    fn a_reader_blocking_the_checkpoint_is_reported_and_the_import_still_commits() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let wal_len = || fs::metadata(dir.path().join("relic.db-wal")).unwrap().len();
        let mut store = Store::open(&db).unwrap();
        let reader = rusqlite::Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        // An open read transaction pins the reader's snapshot in the WAL.
        reader.execute_batch("BEGIN").unwrap();
        let seen: i64 = reader.query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0)).unwrap();
        assert_eq!(seen, 0);

        let r = import_records(&mut store, b"/bulk", records(CHECKPOINT_MIN_RECORDS, None)).unwrap();
        assert!(r.checkpoint_blocked);
        assert!(wal_len() > 0);
        assert_eq!(visible(&db)[2], CHECKPOINT_MIN_RECORDS as i64);

        reader.execute_batch("COMMIT").unwrap();
        assert!(store.checkpoint().unwrap());
        assert_eq!(wal_len(), 0);
    }

    #[test]
    fn provenance_claims_times_only_when_the_file_has_them() {
        let dir = tempfile::tempdir().unwrap();
        let timed = dir.path().join("timed");
        let untimed = dir.path().join("untimed");
        fs::write(&timed, "#1700000000\ncargo build\n").unwrap();
        fs::write(&untimed, "cargo build\n").unwrap();
        let mut store = Store::open_memory().unwrap();
        let redactor = Redactor::new();

        let t = import_file(&mut store, &timed, SourceKind::Bash, &redactor).unwrap();
        let u = import_file(&mut store, &untimed, SourceKind::Bash, &redactor).unwrap();
        assert_eq!(
            store.revision_provenance(t.revision_id).unwrap(),
            Provenance::COMMAND.with(Provenance::OBSERVED_AT)
        );
        assert_eq!(store.revision_provenance(u.revision_id).unwrap(), Provenance::COMMAND);
    }
}
