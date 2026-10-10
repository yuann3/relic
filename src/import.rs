use std::fs;
use std::io::{self, BufReader};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::domain::{FLAG_LOSSY, Observation, ObservationKind, Provenance};
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
#[derive(Debug, Default)]
pub struct ImportResult {
    pub records_stored: usize,
    pub revision_id: i64,
    pub skipped: bool,
    /// The records are committed, but a reader's open snapshot kept the WAL
    /// from being folded into the database file.
    pub checkpoint_blocked: bool,
    /// Stored records flagged `FLAG_LOSSY`.
    pub lossy_records: usize,
    /// zsh Meta bytes with nothing to decode, in stored and withheld records.
    pub dangling_meta: u32,
    /// The file ended in a partial record, which was not stored.
    pub incomplete_tail: bool,
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
    let metadata = fs::metadata(path).map_err(ImportError::Io)?;
    // Before opening: a FIFO blocks in open, a device never ends.
    if !metadata.is_file() {
        return Err(ImportError::NotRegularFile(path.to_path_buf()));
    }
    let stat = stat_hint(&metadata);
    if source.newest().is_some_and(|r| r.cursor.stat == stat) {
        return Ok(ImportResult { skipped: true, ..ImportResult::default() });
    }

    let reader = BufReader::new(fs::File::open(path).map_err(ImportError::Io)?);
    let mut revision = source.new_revision()?;
    let (stored, stable_len, dangling_meta, incomplete_tail) = match kind {
        SourceKind::Zsh => {
            let mut parser = source::zsh::Parser::new(reader);
            let stored = store_records(&mut revision, kind, &mut parser, redactor)?;
            (stored, parser.stable_len(), parser.dangling_meta(), parser.incomplete_tail().is_some())
        }
        SourceKind::Bash => {
            let mut parser = source::bash::Parser::new(reader);
            let stored = store_records(&mut revision, kind, &mut parser, redactor)?;
            (stored, parser.stable_len(), 0, parser.incomplete_tail().is_some())
        }
    };
    // No guard yet, so nothing may resume from stable_len until change
    // detection lands (#24).
    let cursor = Cursor { stat, stable_len: stable_len as i64, boundary_guard: None };
    let revision_id = revision.revision_id();
    revision.commit(&cursor, stored.provenance)?;
    Ok(ImportResult {
        records_stored: stored.count,
        revision_id,
        skipped: false,
        checkpoint_blocked: checkpoint_blocked(store, stored.count)?,
        lossy_records: stored.lossy,
        dangling_meta,
        incomplete_tail,
    })
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

/// What `store_records` put into a revision.
struct Stored {
    count: usize,
    lossy: usize,
    provenance: Provenance,
}

/// Stream `records` into an open revision. Any error, from the records or the
/// store, must drop the revision uncommitted: nothing of this import becomes
/// visible and a retry starts clean.
fn store_records(
    revision: &mut RevisionTx<'_>,
    kind: SourceKind,
    records: impl Iterator<Item = io::Result<RawRecord>>,
    redactor: &Redactor,
) -> Result<Stored, ImportError> {
    let obs_kind = match kind {
        SourceKind::Zsh => ObservationKind::ZshCommand,
        SourceKind::Bash => ObservationKind::BashCommand,
    };

    let mut stored = Stored { count: 0, lossy: 0, provenance: Provenance::COMMAND };
    for raw in records {
        let observation = raw_to_observation(&raw.map_err(ImportError::Io)?, obs_kind, redactor);
        if observation.observed_at_ms.is_some() {
            stored.provenance = stored.provenance.with(Provenance::OBSERVED_AT);
        }
        revision.insert(&observation)?;
        stored.count += 1;
        stored.lossy += (observation.flags & FLAG_LOSSY != 0) as usize;
    }
    Ok(stored)
}

/// Checkpoint after a bulk commit; true when a reader blocked it. The
/// revision is already durable here, so a checkpoint error is reported as a
/// store error, never as `Busy`: "try again" would be wrong advice for an
/// import that succeeded.
fn checkpoint_blocked(store: &Store, stored: usize) -> Result<bool, ImportError> {
    Ok(stored >= CHECKPOINT_MIN_RECORDS
        && !store.checkpoint().map_err(|e| ImportError::Store(StoreError::Sqlite(e)))?)
}

fn raw_to_observation(raw: &RawRecord, kind: ObservationKind, redactor: &Redactor) -> Observation {
    Observation {
        kind,
        // A time whose milliseconds overflow is absent, not wrapped.
        observed_at_ms: raw.timestamp.and_then(|t| t.checked_mul(1000)),
        safe_text: redactor.redact(&raw.command),
        flags: if raw.lossy { FLAG_LOSSY } else { 0 },
        locator: raw.start.to_le_bytes().to_vec(),
    }
}

#[derive(Debug)]
pub enum ImportError {
    Io(io::Error),
    /// A directory, device, FIFO or socket: refused before any read.
    NotRegularFile(PathBuf),
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
            Self::NotRegularFile(path) => write!(f, "{}: not a regular file", path.display()),
            Self::Busy => write!(f, "{}", StoreError::Busy),
            Self::Store(e) => write!(f, "store error: {e}"),
        }
    }
}

impl std::error::Error for ImportError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::StoredObservation;
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
        let mut revision = store.lock_source(locator, SourceKind::Zsh)?.new_revision()?;
        let stored = store_records(&mut revision, SourceKind::Zsh, records, &Redactor::new())?;
        revision.commit(&Cursor::default(), stored.provenance)?;
        Ok(ImportResult {
            records_stored: stored.count,
            checkpoint_blocked: checkpoint_blocked(store, stored.count)?,
            ..ImportResult::default()
        })
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
                start: i as u64,
                end: i as u64 + 1,
                lossy: false,
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
        let raw = RawRecord { timestamp: Some(1_700_000_000), duration: None, command: "echo needle".into(), start: 0, end: 27, lossy: false };
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

    fn stable_len(store: &mut Store, path: &Path) -> i64 {
        let lock = store.lock_source(path.as_os_str().as_bytes(), SourceKind::Zsh).unwrap();
        lock.newest().unwrap().cursor.stable_len
    }

    fn stored(store: &Store) -> Vec<StoredObservation> {
        (1..=store.observation_count().unwrap()).map(|id| store.get_observation(id).unwrap().unwrap()).collect()
    }

    #[test]
    fn a_partial_trailing_record_is_not_committed() {
        let dir = tempfile::tempdir().unwrap();
        let redactor = Redactor::new();
        for (name, kind, content, complete_len) in [
            ("no_newline", SourceKind::Zsh, ": 1700000000:0;echo done\n: 1700000001:0;echo parti", 25),
            ("continuation", SourceKind::Zsh, ": 1700000000:0;echo done\n: 1700000001:0;echo parti\\\n", 25),
            ("bash", SourceKind::Bash, "#1700000000\necho done\n#1700000001\necho parti", 22),
        ] {
            let path = dir.path().join(name);
            fs::write(&path, content).unwrap();
            let mut store = Store::open_memory().unwrap();

            let r = import_file(&mut store, &path, kind, &redactor).unwrap();
            assert_eq!(r.records_stored, 1, "{name}");
            assert!(r.incomplete_tail, "{name}");
            assert!(store.search("parti", 10).unwrap().is_empty(), "{name}");
            assert_eq!(store.search("done", 10).unwrap().len(), 1, "{name}");
            // The cursor stops at the last complete record, not the file length.
            assert_eq!(stable_len(&mut store, &path), complete_len, "{name}");
        }
    }

    #[test]
    fn a_dangling_meta_is_counted_and_stored_lossy_without_a_replacement_char() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zsh_history");
        // A decodable pair, a Meta cut off by the newline, and a Meta at EOF.
        fs::write(&path, b"echo \xE9\x83\xA3\xA8\necho broken\x83\necho tail\x83").unwrap();
        let mut store = Store::open_memory().unwrap();

        let r = import_file(&mut store, &path, SourceKind::Zsh, &Redactor::new()).unwrap();
        assert_eq!((r.records_stored, r.lossy_records, r.dangling_meta), (2, 1, 2));
        assert!(r.incomplete_tail);
        let rows = stored(&store);
        assert_eq!(rows[0].safe_text, "echo \u{90E8}");
        assert_eq!(rows[0].flags & FLAG_LOSSY, 0);
        assert_eq!(rows[1].safe_text, "echo broken");
        assert_eq!(rows[1].flags & FLAG_LOSSY, FLAG_LOSSY);
    }

    #[test]
    fn a_record_past_the_cap_is_stored_cut_and_lossy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zsh_history");
        let mut content = b": 1700000000:0;echo ".to_vec();
        content.resize(source::RECORD_CAP * 2, b'a');
        content.extend_from_slice(b"\n: 1700000001:0;echo after\n");
        fs::write(&path, &content).unwrap();
        let mut store = Store::open_memory().unwrap();

        let r = import_file(&mut store, &path, SourceKind::Zsh, &Redactor::new()).unwrap();
        assert_eq!((r.records_stored, r.lossy_records), (2, 1));
        let rows = stored(&store);
        assert!(rows[0].safe_text.len() <= source::RECORD_CAP);
        assert_eq!(rows[0].flags, FLAG_LOSSY);
        assert_eq!((rows[1].safe_text.as_str(), rows[1].flags), ("echo after", 0));
        assert_eq!(stable_len(&mut store, &path), content.len() as i64);
    }

    #[test]
    fn crlf_history_stores_commands_without_the_cr() {
        let dir = tempfile::tempdir().unwrap();
        let zsh = dir.path().join("zsh");
        let bash = dir.path().join("bash");
        fs::write(&zsh, ": 1700000000:0;echo zed\r\n").unwrap();
        fs::write(&bash, "#1700000000\r\necho bee\r\n").unwrap();
        let mut store = Store::open_memory().unwrap();
        let redactor = Redactor::new();

        import_file(&mut store, &zsh, SourceKind::Zsh, &redactor).unwrap();
        import_file(&mut store, &bash, SourceKind::Bash, &redactor).unwrap();
        let rows = stored(&store);
        assert_eq!(rows[0].safe_text, "echo zed");
        assert_eq!(rows[1].safe_text, "echo bee");
        // The CR did not hide the bash marker either.
        assert_eq!(rows[1].observed_at_ms, Some(1_700_000_000_000));
    }

    #[test]
    fn whitespace_only_commands_are_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zsh_history");
        fs::write(&path, ": 1700000000:0;   \n \t \n: 1700000001:0;echo kept\n").unwrap();
        let mut store = Store::open_memory().unwrap();

        let r = import_file(&mut store, &path, SourceKind::Zsh, &Redactor::new()).unwrap();
        assert_eq!(r.records_stored, 1);
        assert_eq!(stored(&store)[0].safe_text, "echo kept");
    }

    #[test]
    fn a_timestamp_whose_milliseconds_overflow_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zsh_history");
        // i64::MAX seconds parses, but times 1000 does not fit.
        fs::write(&path, format!(": {}:0;echo far\n: 1700000000:0;echo near\n", i64::MAX)).unwrap();
        let mut store = Store::open_memory().unwrap();

        import_file(&mut store, &path, SourceKind::Zsh, &Redactor::new()).unwrap();
        let rows = stored(&store);
        assert_eq!((rows[0].safe_text.as_str(), rows[0].observed_at_ms), ("echo far", None));
        assert_eq!(rows[1].observed_at_ms, Some(1_700_000_000_000));
    }

    #[test]
    fn a_bash_marker_past_the_first_ten_lines_still_times_its_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bash_history");
        let mut content: String = (0..12).map(|i| format!("untimed {i}\n")).collect();
        content.push_str("#1700000000\necho timed\n");
        fs::write(&path, content).unwrap();
        let mut store = Store::open_memory().unwrap();

        let r = import_file(&mut store, &path, SourceKind::Bash, &Redactor::new()).unwrap();
        assert_eq!(r.records_stored, 13);
        let rows = stored(&store);
        assert_eq!((rows[12].safe_text.as_str(), rows[12].observed_at_ms), ("echo timed", Some(1_700_000_000_000)));
    }

    #[test]
    fn non_regular_files_are_refused_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let mut store = Store::open_memory().unwrap();

        // Opening the FIFO would block forever: nothing ever writes to it.
        for path in [fifo.as_path(), Path::new("/dev/null"), dir.path()] {
            let err = import_file(&mut store, path, SourceKind::Zsh, &Redactor::new()).unwrap_err();
            assert!(matches!(&err, ImportError::NotRegularFile(p) if p == path), "{err}");
        }
        assert_eq!(store.observation_count().unwrap(), 0);
    }

    #[test]
    fn revisions_record_their_parser_contract() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let path = dir.path().join("zsh_history");
        fs::write(&path, "ls\n").unwrap();
        let mut store = Store::open(&db).unwrap();
        import_file(&mut store, &path, SourceKind::Zsh, &Redactor::new()).unwrap();

        let conn = rusqlite::Connection::open(&db).unwrap();
        let contract: String =
            conn.query_row("SELECT parser_contract FROM source_revision", [], |r| r.get(0)).unwrap();
        assert_eq!(contract, source::zsh::PARSER_CONTRACT);
        assert_ne!(source::zsh::PARSER_CONTRACT, source::bash::PARSER_CONTRACT);
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
