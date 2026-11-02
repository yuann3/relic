mod schema;

pub use schema::SCHEMA_VERSION;

use crate::domain::{Observation, ObservationKind, Provenance, SearchHit, StoredObservation};
use crate::import::SourceKind;
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior};
use std::path::{Path, PathBuf};

/// SQLite VM steps between polls of `Store::interruptible`'s check. FTS5 reads
/// its index through statements of its own, so the count advances inside one
/// prefix merge or phrase scan too, only slowly: on the 1M fixture a stop
/// landed up to 10 ms late at 1000 steps and under 2 ms late at 100, with no
/// measurable cost to the query.
const PROGRESS_STEPS: i32 = 100;

/// A failed open of, or write to, the store.
#[derive(Debug)]
pub enum StoreError {
    /// Another connection held a lock past the 250 ms busy_timeout.
    Busy,
    /// Not SQLite, another program's database, or a store from a pre-release
    /// build, which ADR 0005 discards.
    NotRelic(PathBuf),
    /// A store written under another schema version.
    Version { path: PathBuf, found: i64 },
    /// An empty file: a store a first import has not finished creating, or
    /// nothing yet. Only a writer may fill it.
    NoStoreYet(PathBuf),
    /// A reader needs to create the WAL's side files, and this file or
    /// directory would not let it.
    Unwritable(PathBuf),
    Sqlite(rusqlite::Error),
    /// The OS random source failed while minting a source's logical id.
    Entropy(getrandom::Error),
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        match e.sqlite_error_code() {
            Some(rusqlite::ErrorCode::DatabaseBusy) => Self::Busy,
            _ => Self::Sqlite(e),
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(f, "busy: another relic is using the store; try again"),
            Self::NotRelic(path) => write!(
                f,
                "{} is not a store this relic can read: it is another program's file, or a \
                 pre-release relic wrote it. Move it away and run `relic import` again",
                path.display()
            ),
            Self::Version { path, found } => write!(
                f,
                "{} is store version {found} and this relic reads version {SCHEMA_VERSION}. Use the \
                 relic that wrote it, or move it away and run `relic import` again",
                path.display()
            ),
            Self::NoStoreYet(path) => write!(
                f,
                "no store yet at {}: run `relic import`, or try again if an import is creating it",
                path.display()
            ),
            Self::Unwritable(path) => write!(
                f,
                "{} is not writable: relic needs write access to a store and its directory even to read it",
                path.display()
            ),
            Self::Sqlite(e) => write!(f, "{e}"),
            Self::Entropy(e) => write!(f, "no random bytes for a source id: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// What `stat` said about a source file when it was imported. Equal hints
/// mean the file is unchanged; `(dev, ino)` is never identity (ADR 0002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatHint {
    pub dev: i64,
    pub ino: i64,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

/// Where the next import of a source resumes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cursor {
    pub stat: StatHint,
    /// Byte end of the last committed record.
    pub stable_len: i64,
    /// Keyed hash of the bytes just before `stable_len`. `None` proves
    /// nothing about the prefix, so the next import must not trust it.
    pub boundary_guard: Option<[u8; 32]>,
}

/// A source's newest revision and its cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewestRevision {
    pub revision_id: i64,
    pub cursor: Cursor,
}

/// The store's write lock, held for one source. The newest revision was read
/// after the lock was taken, so no other importer can change it before this
/// one has decided: drop the lock to store nothing, or turn it into the
/// revision transaction that stores the records.
pub struct SourceLock<'a> {
    tx: Transaction<'a>,
    locator: &'a [u8],
    stored_kind: Option<SourceKind>,
    newest: Option<NewestRevision>,
}

impl<'a> SourceLock<'a> {
    /// The source's newest revision, if it was ever imported.
    pub fn newest(&self) -> Option<&NewestRevision> {
        self.newest.as_ref()
    }

    /// The shell this source was first imported as, if it ever was.
    pub fn stored_kind(&self) -> Option<SourceKind> {
        self.stored_kind
    }

    /// Delete everything stored for this source and record it as `kind`, for
    /// a source that was imported as the wrong shell: its records were parsed
    /// by the wrong grammar, so none of them is evidence. Returns how many
    /// records went. Nothing is gone until the revision that follows commits.
    pub fn replace_kind(&mut self, kind: SourceKind) -> Result<usize, StoreError> {
        let revisions = "SELECT sr.id FROM source_revision sr JOIN source s ON s.id = sr.source_id
                         WHERE s.private_locator = ?1";
        // External-content FTS keeps its own index: each row has to be
        // removed from it by its text before the row itself goes.
        self.tx.execute(
            &format!(
                "INSERT INTO observation_fts (observation_fts, rowid, safe_text)
                 SELECT 'delete', id, safe_text FROM observation WHERE revision_id IN ({revisions})"
            ),
            [self.locator],
        )?;
        let removed = self.tx.execute(
            &format!("DELETE FROM observation WHERE revision_id IN ({revisions})"),
            [self.locator],
        )?;
        self.tx.execute(
            "DELETE FROM source_revision
             WHERE source_id IN (SELECT id FROM source WHERE private_locator = ?1)",
            [self.locator],
        )?;
        self.tx.execute(
            "UPDATE source SET kind = ?2, parser_contract = ?3 WHERE private_locator = ?1",
            rusqlite::params![self.locator, kind_int(kind), parser_contract(kind)],
        )?;
        self.stored_kind = Some(kind);
        self.newest = None;
        Ok(removed)
    }

    /// Open a new revision, creating the source as `kind` on first sight.
    pub fn new_revision(self, kind: SourceKind) -> Result<RevisionTx<'a>, StoreError> {
        let Self { tx, locator, .. } = self;
        let parser_contract = parser_contract(kind);
        let known: Option<i64> = tx
            .query_row("SELECT id FROM source WHERE private_locator = ?1", [locator], |r| r.get(0))
            .optional()?;
        let source_id = match known {
            Some(id) => id,
            None => {
                let mut logical_id = [0u8; 16];
                getrandom::fill(&mut logical_id).map_err(StoreError::Entropy)?;
                tx.execute(
                    "INSERT INTO source (logical_id, kind, private_locator, parser_contract, enabled)
                     VALUES (?1, ?2, ?3, ?4, 1)",
                    rusqlite::params![logical_id.as_slice(), kind_int(kind), locator, parser_contract],
                )?;
                tx.last_insert_rowid()
            }
        };

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        // The cursor is only known once the records are read; `commit` writes it.
        tx.execute(
            "INSERT INTO source_revision (source_id, dev, ino, size, mtime_ns, ctime_ns,
                stable_len, provenance, parser_contract, redaction_revision, imported_at_ms)
             VALUES (?1, 0, 0, 0, 0, 0, 0, 0, ?2, ?3, ?4)",
            rusqlite::params![source_id, parser_contract, crate::redact::REDACTION_REVISION, now_ms],
        )?;
        let revision_id = tx.last_insert_rowid();
        Ok(RevisionTx { tx, revision_id, next_ordinal: 0 })
    }

    /// The last `n` records of the newest revision, oldest first: the
    /// `(observed_at_ms, safe_text)` a rewritten file is aligned by. Read
    /// from the rows themselves, so no hash of them is stored (ADR 0008).
    pub fn anchor(&self, n: usize) -> Result<Vec<(Option<i64>, String)>, StoreError> {
        let Some(newest) = &self.newest else { return Ok(vec![]) };
        let mut records = self
            .tx
            .prepare(
                "SELECT observed_at_ms, safe_text FROM observation
                 WHERE revision_id = ?1 ORDER BY ordinal DESC LIMIT ?2",
            )?
            .query_map(rusqlite::params![newest.revision_id, n as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        records.reverse();
        Ok(records)
    }

    /// Every record stored for this source, in any revision, as
    /// `(kind, observed_at_ms, safe_text)`.
    pub fn records(&self) -> Result<Vec<(ObservationKind, Option<i64>, String)>, StoreError> {
        let mut stmt = self.tx.prepare(
            "SELECT o.kind, o.observed_at_ms, o.safe_text FROM observation o
             JOIN source_revision sr ON sr.id = o.revision_id JOIN source s ON s.id = sr.source_id
             WHERE s.private_locator = ?1",
        )?;
        let rows = stmt.query_map([self.locator], |r| Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (kind, at, text) = row?;
            let kind = ObservationKind::from_i64(kind).ok_or(rusqlite::Error::IntegralValueOutOfRange(0, kind))?;
            out.push((kind, at, text));
        }
        Ok(out)
    }

    /// Reopen the newest revision to append records after its last one. A
    /// source with no revision yet gets its first.
    pub fn append(self, kind: SourceKind) -> Result<RevisionTx<'a>, StoreError> {
        let Some(newest) = &self.newest else { return self.new_revision(kind) };
        let revision_id = newest.revision_id;
        let next_ordinal = self.tx.query_row(
            "SELECT COALESCE(MAX(ordinal) + 1, 0) FROM observation WHERE revision_id = ?1",
            [revision_id],
            |r| r.get(0),
        )?;
        Ok(RevisionTx { tx: self.tx, revision_id, next_ordinal })
    }
}

/// An open write transaction on one revision. Nothing inserted through it is
/// visible to any reader until `commit`; dropping it rolls everything back,
/// including a revision and source row it created (ADR 0005).
pub struct RevisionTx<'a> {
    tx: Transaction<'a>,
    revision_id: i64,
    next_ordinal: i64,
}

impl RevisionTx<'_> {
    pub fn revision_id(&self) -> i64 {
        self.revision_id
    }

    /// Append one observation and its FTS row. Returns the row ID.
    pub fn insert(&mut self, o: &Observation) -> Result<i64, StoreError> {
        self.tx
            .prepare_cached(
                "INSERT INTO observation (revision_id, ordinal, kind, observed_at_ms, safe_text,
                    flags, locator)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(rusqlite::params![
                self.revision_id,
                self.next_ordinal,
                o.kind.as_i64(),
                o.observed_at_ms,
                o.safe_text,
                o.flags,
                o.locator,
            ])?;
        let id = self.tx.last_insert_rowid();
        self.tx
            .prepare_cached("INSERT INTO observation_fts (rowid, safe_text) VALUES (?1, ?2)")?
            .execute(rusqlite::params![id, o.safe_text])?;
        self.next_ordinal += 1;
        Ok(id)
    }

    /// Record where the import stopped and make the whole revision visible.
    /// `provenance` is added to what the revision already claims.
    pub fn commit(self, cursor: &Cursor, provenance: Provenance) -> Result<(), StoreError> {
        self.tx.execute(
            "UPDATE source_revision
             SET dev = ?2, ino = ?3, size = ?4, mtime_ns = ?5, ctime_ns = ?6,
                 stable_len = ?7, boundary_guard = ?8, provenance = provenance | ?9
             WHERE id = ?1",
            rusqlite::params![
                self.revision_id,
                cursor.stat.dev,
                cursor.stat.ino,
                cursor.stat.size,
                cursor.stat.mtime_ns,
                cursor.stat.ctime_ns,
                cursor.stable_len,
                cursor.boundary_guard.as_ref().map(<[u8; 32]>::as_slice),
                provenance.as_i64(),
            ],
        )?;
        self.tx.commit()?;
        Ok(())
    }
}

/// Local observation store backed by SQLite + FTS5.
pub struct Store {
    conn: Connection,
    /// Where the installation key lives; an in-memory store has none.
    key_path: Option<std::path::PathBuf>,
}

impl Store {
    /// Open or create a store at the given path, to write to it.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        schema::initialize(&conn, path)?;
        Ok(Self { conn, key_path: Some(key_path(path)) })
    }

    /// Open an existing store to read it. Nothing here writes to the file:
    /// no DDL, no journal switch, no write lock, so it reads while an import
    /// is writing and never stamps Relic's schema into another file (ADR 0005).
    pub fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        // Even a reader creates the WAL's -shm beside the database. Where it
        // cannot, SQLite fails with "unable to open database file"; beside a
        // read-only database it leaves read-only side files that block the
        // next import. Refuse both before SQLite touches anything.
        // SQLite puts them beside what a symlink points at, not beside the link.
        let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let dir = real.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        for needed in [real.as_path(), dir] {
            if !writable(needed) {
                return Err(StoreError::Unwritable(needed.to_path_buf()));
            }
        }
        let conn =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(schema::BUSY_TIMEOUT)?;
        // The first read is where a reader meets a store that another relic
        // is recovering after a crashed write: measured on bundled SQLite
        // 3.49.1, that is SQLITE_BUSY_RECOVERY once busy_timeout runs out.
        // One retry, said out loud, and no loop (ADR 0005).
        let is_store = match schema::is_store(&conn, path) {
            Err(StoreError::Busy) => {
                eprintln!("relic: note: the store was busy, as it is while another relic recovers it; trying once more");
                schema::is_store(&conn, path)?
            }
            is_store => is_store?,
        };
        if !is_store {
            return Err(StoreError::NoStoreYet(path.to_path_buf()));
        }
        schema::configure_reader(&conn)?;
        Ok(Self { conn, key_path: Some(key_path(path)) })
    }

    /// Open an in-memory store (for tests).
    pub fn open_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        schema::initialize(&conn, Path::new(":memory:"))?;
        Ok(Self { conn, key_path: None })
    }

    /// Run `f` while polling `stop`. Once `stop` returns true the running
    /// statement fails with an error `is_interrupt` recognises. FTS5 does a
    /// query's work inside SQLite, so this is the only way to give up on one.
    pub fn interruptible<T>(&self, stop: impl FnMut() -> bool + Send + 'static, f: impl FnOnce(&Self) -> T) -> T {
        self.conn.progress_handler(PROGRESS_STEPS, Some(stop));
        let out = f(self);
        self.conn.progress_handler(0, None::<fn() -> bool>);
        out
    }

    /// The installation key's file, beside the database (see `crate::key`).
    pub fn key_path(&self) -> Option<&std::path::Path> {
        self.key_path.as_deref()
    }

    /// Check whether a table exists in the database.
    pub fn has_table(&self, name: &str) -> Result<bool, rusqlite::Error> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
            [name],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Return the stored schema version.
    pub fn schema_version(&self) -> Result<i64, rusqlite::Error> {
        self.conn.query_row("PRAGMA user_version", [], |row| row.get(0))
    }

    /// Take the write lock and read the newest revision of the source at
    /// `locator` under it. BEGIN IMMEDIATE takes the lock up front, so a
    /// second importer waits here, then fails with `Busy`, instead of deciding
    /// on a revision that another importer is about to replace.
    pub fn lock_source<'a>(&'a mut self, locator: &'a [u8]) -> Result<SourceLock<'a>, StoreError> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored_kind = tx
            .query_row("SELECT kind FROM source WHERE private_locator = ?1", [locator], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .map(|kind| {
                [SourceKind::Zsh, SourceKind::Bash, SourceKind::Git]
                    .into_iter()
                    .find(|k| kind_int(*k) == kind)
                    // Guessing a shell here could delete the source's records
                    // on the next `--shell`.
                    .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, kind))
            })
            .transpose()?;
        let newest = tx
            .query_row(
                "SELECT sr.id, sr.dev, sr.ino, sr.size, sr.mtime_ns, sr.ctime_ns,
                        sr.stable_len, sr.boundary_guard
                 FROM source s JOIN source_revision sr ON sr.source_id = s.id
                 WHERE s.private_locator = ?1
                 ORDER BY sr.id DESC LIMIT 1",
                [locator],
                |r| {
                    Ok(NewestRevision {
                        revision_id: r.get(0)?,
                        cursor: Cursor {
                            stat: StatHint {
                                dev: r.get(1)?,
                                ino: r.get(2)?,
                                size: r.get(3)?,
                                mtime_ns: r.get(4)?,
                                ctime_ns: r.get(5)?,
                            },
                            stable_len: r.get(6)?,
                            boundary_guard: r.get(7)?,
                        },
                    })
                },
            )
            .optional()?;
        Ok(SourceLock { tx, locator, stored_kind, newest })
    }

    /// Which fields the revision's source file encodes.
    pub fn revision_provenance(&self, revision_id: i64) -> Result<Provenance, rusqlite::Error> {
        self.conn.query_row(
            "SELECT provenance FROM source_revision WHERE id = ?1",
            [revision_id],
            |r| r.get(0).map(Provenance::from_i64),
        )
    }

    /// Count total observations in the store.
    pub fn observation_count(&self) -> Result<i64, rusqlite::Error> {
        self.conn.query_row("SELECT COUNT(*) FROM observation", [], |row| row.get(0))
    }

    /// Re-redact every revision stored under another redaction revision.
    /// Each becomes a new revision holding `redact` of the old safe text, in
    /// the same order, and the old one is deleted, so ids cited before this
    /// no longer resolve (ADR 0003). Returns how many records were rewritten.
    ///
    /// The rewrite is one transaction. Scrubbing the freed pages and the WAL
    /// comes after it; the second value is false when that could not finish,
    /// in which case the next rescrub or forget tries again.
    pub fn rescrub(&mut self, current: &str, redact: impl Fn(&str) -> String) -> Result<(usize, bool), StoreError> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stale: Vec<i64> = tx
            .prepare("SELECT id FROM source_revision WHERE redaction_revision <> ?1 ORDER BY id")?
            .query_map([current], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        if stale.is_empty() {
            let pending = scrub_pending(&tx)?;
            drop(tx);
            return Ok((0, !pending || self.scrub_free_space(false)?));
        }
        mark_scrub_pending(&tx)?;
        let mut rewritten = 0;
        for old in stale {
            tx.execute(
                "INSERT INTO source_revision (source_id, dev, ino, size, mtime_ns, ctime_ns, stable_len,
                    boundary_guard, provenance, parser_contract, redaction_revision, imported_at_ms)
                 SELECT source_id, dev, ino, size, mtime_ns, ctime_ns, stable_len,
                    boundary_guard, provenance, parser_contract, ?2, imported_at_ms
                 FROM source_revision WHERE id = ?1",
                rusqlite::params![old, current],
            )?;
            let new = tx.last_insert_rowid();
            // In pages by ordinal, so a million-row revision is never held in
            // memory and the cursor never reads a table it is inserting into.
            let mut after = -1i64;
            loop {
                // The row as stored, `kind` included: a rescrub must not
                // rewrite a kind this build does not know.
                struct Row(i64, i64, Option<i64>, String, i64, Vec<u8>);
                let page: Vec<Row> = tx
                    .prepare_cached(
                        "SELECT ordinal, kind, observed_at_ms, safe_text, flags, locator FROM observation
                         WHERE revision_id = ?1 AND ordinal > ?2 ORDER BY ordinal LIMIT 1000",
                    )?
                    .query_map([old, after], |r| {
                        Ok(Row(r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                    })?
                    .collect::<Result<_, _>>()?;
                let Some(last) = page.last() else { break };
                after = last.0;
                let mut insert = tx.prepare_cached(
                    "INSERT INTO observation (revision_id, ordinal, kind, observed_at_ms, safe_text, flags, locator)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                for Row(ordinal, kind, observed_at_ms, safe_text, flags, locator) in &page {
                    insert.execute(rusqlite::params![new, ordinal, kind, observed_at_ms, redact(safe_text), flags, locator])?;
                }
                rewritten += page.len();
            }
            tx.execute("DELETE FROM observation WHERE revision_id = ?1", [old])?;
            tx.execute("DELETE FROM source_revision WHERE id = ?1", [old])?;
        }
        // Every row changed, so the index is rebuilt whole from what is left.
        tx.execute("INSERT INTO observation_fts (observation_fts) VALUES ('rebuild')", [])?;
        tx.commit()?;
        // Every row was rewritten, so half the file is free pages: compact it.
        Ok((rewritten, self.scrub_free_space(true)?))
    }

    /// Delete these observations for good and say which of them existed, and
    /// whether their bytes are off the disk too (see `rescrub`).
    pub fn forget(&mut self, ids: &[i64]) -> Result<(Vec<i64>, bool), StoreError> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut forgotten = Vec::new();
        for id in ids {
            // The index entry goes first and by its text: see `replace_kind`.
            let indexed = tx.execute(
                "INSERT INTO observation_fts (observation_fts, rowid, safe_text)
                 SELECT 'delete', id, safe_text FROM observation WHERE id = ?1",
                [id],
            )?;
            if indexed == 1 {
                tx.execute("DELETE FROM observation WHERE id = ?1", [id])?;
                forgotten.push(*id);
            }
        }
        if !forgotten.is_empty() {
            mark_scrub_pending(&tx)?;
        }
        // A scrub an earlier run could not finish is finished by this one,
        // even when it has nothing left to forget.
        let pending = scrub_pending(&tx)?;
        tx.commit()?;
        Ok((forgotten, !pending || self.scrub_free_space(false)?))
    }

    /// Get deleted text off the disk. `secure_delete` already zeroed it in
    /// the database file; what is left are the page images in the WAL, which
    /// the checkpoint truncates. `false` when a reader's open snapshot kept
    /// the WAL. The delete itself recorded that a scrub is owed, in its own
    /// transaction, so a scrub that is blocked or fails is finished by the
    /// next rescrub or forget.
    ///
    /// `compact` also rewrites the file without its free pages. The copy is
    /// built on disk, not in memory as `temp_store` would have it, and with
    /// a 2 MiB page cache: VACUUM fills whatever cache it is given, and at
    /// the usual 16 MiB a 1M-record rescrub peaked at 72 MiB, over the 64 MiB
    /// budget. With these it peaks at 52 MiB, set by the rewrite before it.
    fn scrub_free_space(&self, compact: bool) -> Result<bool, StoreError> {
        if compact {
            self.conn.execute_batch("PRAGMA temp_store = FILE; PRAGMA cache_size = -2048; VACUUM; PRAGMA cache_size = -16384; PRAGMA temp_store = MEMORY;")?;
        }
        if !self.checkpoint()? {
            return Ok(false);
        }
        self.conn.execute("DELETE FROM schema_meta WHERE key = 'scrub_pending'", [])?;
        // Clearing the note went through the WAL as well.
        Ok(self.checkpoint()?)
    }

    /// Fold the WAL into the database file and truncate it. `false` when a
    /// reader's open snapshot blocked it: SQLite reports that in the pragma's
    /// result row, not as an error, after waiting out busy_timeout.
    pub fn checkpoint(&self) -> Result<bool, rusqlite::Error> {
        let busy: i64 = self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        Ok(busy == 0)
    }

    /// Matches where every word matches whole, most recently imported first
    /// (ADR 0006). Query terms are escaped (no raw FTS syntax). Rank must
    /// score every match before returning any, while rowid order lets FTS5
    /// stop after `limit` rows (260 ms vs 0.04 ms at 1M).
    /// Hits carry score 0.0. Rowid order is import order, not observed time.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, rusqlite::Error> {
        self.search_fts(&escape_fts_query(query, false), limit, false)
    }

    /// `search` where the last word is a prefix, so partially typed input
    /// ("gi") finds completed words ("git"). A prefix merges all its postings
    /// before the first row; see docs/research/interactive-fts-measurements.md.
    pub fn search_prefix(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, rusqlite::Error> {
        self.search_fts(&escape_fts_query(query, true), limit, false)
    }

    /// Matches by BM25 rank, the more recently imported first among equals.
    /// Costs about 0.5 µs per match, so run it under `interruptible`.
    pub fn search_ranked(
        &self,
        query: &str,
        last_prefix: bool,
        limit: usize,
    ) -> Result<Vec<SearchHit>, rusqlite::Error> {
        self.search_fts(&escape_fts_query(query, last_prefix), limit, true)
    }

    /// The newest observations, unfiltered.
    pub fn recent(&self, limit: usize) -> Result<Vec<SearchHit>, rusqlite::Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, 0.0 FROM observation ORDER BY id DESC LIMIT ?1",
        )?;
        let ids = stmt
            .query_map([limit as i64], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        self.hits_for(&ids)
    }

    fn search_fts(
        &self,
        escaped: &str,
        limit: usize,
        by_rank: bool,
    ) -> Result<Vec<SearchHit>, rusqlite::Error> {
        if escaped.is_empty() {
            return Ok(vec![]);
        }

        // Step 1: top-N rowids from FTS5, by rank or newest first.
        let sql = if by_rank {
            "SELECT rowid, rank FROM observation_fts
             WHERE observation_fts MATCH ?1
             ORDER BY rank, rowid DESC
             LIMIT ?2"
        } else {
            "SELECT rowid, 0.0 FROM observation_fts
             WHERE observation_fts MATCH ?1
             ORDER BY rowid DESC
             LIMIT ?2"
        };
        let mut fts_stmt = self.conn.prepare_cached(sql)?;
        let ranked: Vec<(i64, f64)> = fts_stmt
            .query_map(rusqlite::params![escaped, limit as i64], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        self.hits_for(&ranked)
    }

    /// Step 2: fetch observation data for ranked rowids, keeping their order.
    fn hits_for(&self, ranked: &[(i64, f64)]) -> Result<Vec<SearchHit>, rusqlite::Error> {
        if ranked.is_empty() {
            return Ok(vec![]);
        }

        let mut obs_stmt = self.conn.prepare_cached(
            "SELECT id, observed_at_ms, kind, safe_text, revision_id, ordinal
             FROM observation WHERE id = ?1",
        )?;

        let mut hits = Vec::with_capacity(ranked.len());
        for (rowid, score) in ranked {
            // Only a row deleted since step 1 is skipped: an interrupt or an
            // I/O error must fail the page, not shorten it.
            if let Some((mut hit, revision_id, ordinal)) = obs_stmt
                .query_row([rowid], |row| {
                    let kind_val: i64 = row.get(2)?;
                    let hit = SearchHit {
                        id: row.get(0)?,
                        observed_at_ms: row.get(1)?,
                        observed_before_ms: None,
                        kind: ObservationKind::from_i64(kind_val)
                            .unwrap_or(ObservationKind::ZshCommand),
                        safe_text: row.get(3)?,
                        score: *score,
                    };
                    Ok((hit, row.get::<_, i64>(4)?, row.get::<_, i64>(5)?))
                })
                .optional()?
            {
                if hit.observed_at_ms.is_none() {
                    hit.observed_before_ms = self.observed_no_later_than(revision_id, ordinal)?;
                }
                hits.push(hit);
            }
        }

        Ok(hits)
    }

    /// For an untimed observation, the time of the next timestamped one in the
    /// same revision. History files append in order, so the untimed command
    /// ran no later than that; zsh only stamps lines once EXTENDED_HISTORY or
    /// SHARE_HISTORY is on, leaving older lines untimed. None when nothing
    /// later is timed.
    pub fn observed_no_later_than(
        &self,
        revision_id: i64,
        ordinal: i64,
    ) -> Result<Option<i64>, rusqlite::Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT observed_at_ms FROM observation
             WHERE revision_id = ?1 AND ordinal > ?2 AND observed_at_ms IS NOT NULL
             ORDER BY ordinal LIMIT 1",
        )?;
        let mut rows = stmt.query(rusqlite::params![revision_id, ordinal])?;
        rows.next()?.map(|r| r.get(0)).transpose()
    }

    /// Get neighboring observations within the same revision, by ordinal range.
    pub fn get_neighbors(
        &self,
        revision_id: i64,
        center_ordinal: i64,
        before: u32,
        after: u32,
    ) -> Result<Vec<StoredObservation>, rusqlite::Error> {
        let lo = center_ordinal - before as i64;
        let hi = center_ordinal + after as i64;
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, revision_id, ordinal, kind, observed_at_ms, safe_text,
                    flags, locator
             FROM observation
             WHERE revision_id = ?1 AND ordinal >= ?2 AND ordinal <= ?3
             ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(rusqlite::params![revision_id, lo, hi], |row| {
            row_to_stored_observation(row)
        })?;
        rows.collect()
    }

    /// Read a single observation by row ID.
    pub fn get_observation(&self, id: i64) -> Result<Option<StoredObservation>, rusqlite::Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, revision_id, ordinal, kind, observed_at_ms, safe_text,
                    flags, locator
             FROM observation WHERE id = ?1",
        )?;
        let mut rows = stmt.query([id])?;
        match rows.next()? {
            Some(row) => Ok(Some(row_to_stored_observation(row)?)),
            None => Ok(None),
        }
    }
}

/// The error a statement fails with once `Store::interruptible`'s `stop`
/// returned true.
pub fn is_interrupt(e: &rusqlite::Error) -> bool {
    e.sqlite_error_code() == Some(ErrorCode::OperationInterrupted)
}

fn key_path(db: &Path) -> PathBuf {
    let mut path = db.as_os_str().to_owned();
    path.push(".key");
    path.into()
}

fn writable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

fn scrub_pending(tx: &Transaction<'_>) -> Result<bool, rusqlite::Error> {
    tx.query_row("SELECT EXISTS (SELECT 1 FROM schema_meta WHERE key = 'scrub_pending')", [], |r| r.get(0))
}

fn mark_scrub_pending(tx: &Transaction<'_>) -> Result<usize, rusqlite::Error> {
    tx.execute("INSERT OR REPLACE INTO schema_meta (key, value) VALUES ('scrub_pending', '1')", [])
}

fn kind_int(kind: SourceKind) -> i64 {
    match kind {
        SourceKind::Zsh => 1,
        SourceKind::Bash => 2,
        SourceKind::Git => 3,
    }
}

fn parser_contract(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Zsh => crate::source::zsh::PARSER_CONTRACT,
        SourceKind::Bash => crate::source::bash::PARSER_CONTRACT,
        SourceKind::Git => crate::source::git::PARSER_CONTRACT,
    }
}

/// Escape user/model text into a safe FTS5 query.
/// Each word becomes a quoted phrase token. No raw FTS operators pass through;
/// the only operator Relic itself adds is `*` on the last token when
/// `last_prefix` is set.
fn escape_fts_query(query: &str, last_prefix: bool) -> String {
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

fn row_to_stored_observation(row: &rusqlite::Row<'_>) -> Result<StoredObservation, rusqlite::Error> {
    let kind_val: i64 = row.get(3)?;
    Ok(StoredObservation {
        id: row.get(0)?,
        revision_id: row.get(1)?,
        ordinal: row.get(2)?,
        kind: ObservationKind::from_i64(kind_val).unwrap_or(ObservationKind::ZshCommand),
        observed_at_ms: row.get(4)?,
        safe_text: row.get(5)?,
        flags: row.get(6)?,
        locator: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_observation(ordinal: i64, text: &str) -> Observation {
        Observation {
            kind: ObservationKind::ZshCommand,
            observed_at_ms: Some(1_700_000_000_000 + ordinal),
            safe_text: text.to_string(),
            flags: 0,
            locator: vec![],
        }
    }

    fn begin<'a>(store: &'a mut Store, locator: &'a [u8]) -> RevisionTx<'a> {
        store.lock_source(locator).unwrap().new_revision(SourceKind::Zsh).unwrap()
    }

    fn newest(store: &mut Store) -> Option<NewestRevision> {
        store.lock_source(b"/h").unwrap().newest().cloned()
    }

    /// Commit `obs` as one new revision. Returns the row IDs.
    fn insert(store: &mut Store, obs: &[Observation]) -> Vec<i64> {
        let mut rev = begin(store, b"/h");
        let ids = obs.iter().map(|o| rev.insert(o).unwrap()).collect();
        rev.commit(&Cursor::default(), Provenance::COMMAND).unwrap();
        ids
    }

    #[test]
    fn open_creates_schema_tables() {
        let store = Store::open_memory().expect("open_memory failed");

        assert!(store.has_table("schema_meta").unwrap());
        assert!(store.has_table("source").unwrap());
        assert!(store.has_table("source_revision").unwrap());
        assert!(store.has_table("observation").unwrap());
        assert!(store.has_table("observation_fts").unwrap());
    }

    #[test]
    fn open_sets_schema_version() {
        let store = Store::open_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION as i64);
    }

    #[test]
    fn open_file_creates_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        {
            let store = Store::open(&db_path).unwrap();
            assert!(store.has_table("observation").unwrap());
        }

        let store = Store::open(&db_path).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION as i64);
    }

    #[test]
    fn pragmas_are_configured() {
        let store = Store::open_memory().unwrap();

        let fk: i64 = store
            .conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(fk, 1, "foreign_keys should be enabled");
    }

    // --- Slice 2: Insert + read observations ---

    #[test]
    fn insert_and_read_observation() {
        let mut store = Store::open_memory().unwrap();

        let obs = make_observation(0, "cargo build --release");
        let ids = insert(&mut store, &[obs]);
        assert_eq!(ids.len(), 1);

        let stored = store.get_observation(ids[0]).unwrap().expect("should exist");
        assert_eq!(stored.safe_text, "cargo build --release");
        assert_eq!(stored.kind, ObservationKind::ZshCommand);
        assert_eq!(stored.ordinal, 0);
        assert_eq!(stored.revision_id, 1);
    }

    #[test]
    fn insert_batch_assigns_sequential_ids() {
        let mut store = Store::open_memory().unwrap();

        let batch: Vec<Observation> = (0..5)
            .map(|i| make_observation(i, &format!("cmd_{i}")))
            .collect();
        let ids = insert(&mut store, &batch);

        assert_eq!(ids.len(), 5);
        for (i, &id) in ids.iter().enumerate() {
            let stored = store.get_observation(id).unwrap().unwrap();
            assert_eq!(stored.safe_text, format!("cmd_{i}"));
            assert_eq!(stored.ordinal, i as i64);
        }
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let store = Store::open_memory().unwrap();
        assert!(store.get_observation(999).unwrap().is_none());
    }

    // --- Slice 3: FTS search ---

    #[test]
    fn search_finds_matching_observations() {
        let mut store = Store::open_memory().unwrap();

        insert(&mut store, &[
            make_observation(0, "cargo build --release"),
            make_observation(1, "git push origin main"),
            make_observation(2, "cargo test --lib"),
        ]);

        let hits = store.search("cargo", 10).unwrap();
        assert_eq!(hits.len(), 2);
        // Both "cargo build" and "cargo test" should match.
        let texts: Vec<&str> = hits.iter().map(|h| h.safe_text.as_str()).collect();
        assert!(texts.iter().any(|t| t.contains("build")));
        assert!(texts.iter().any(|t| t.contains("test")));
    }

    #[test]
    fn search_respects_limit() {
        let mut store = Store::open_memory().unwrap();

        let batch: Vec<Observation> = (0..20)
            .map(|i| make_observation(i, &format!("cargo command_{i}")))
            .collect();
        insert(&mut store, &batch);

        let hits = store.search("cargo", 5).unwrap();
        assert_eq!(hits.len(), 5);
    }

    #[test]
    fn search_empty_query_returns_empty() {
        let store = Store::open_memory().unwrap();
        let hits = store.search("", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_no_match_returns_empty() {
        let mut store = Store::open_memory().unwrap();

        insert(&mut store, &[
            make_observation(0, "cargo build"),
        ]);

        let hits = store.search("nonexistent_term", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn recent_prefix_matches_partial_last_word() {
        let mut store = Store::open_memory().unwrap();
        insert(&mut store, &[
            make_observation(0, "git push origin main"),
            make_observation(1, "cargo build"),
        ]);

        assert!(store.search("gi", 10).unwrap().is_empty());
        let hits = store.search_prefix("gi", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].safe_text, "git push origin main");
        // Earlier words still match whole.
        assert!(store.search_prefix("gi pu", 10).unwrap().is_empty());
        assert_eq!(store.search_prefix("git pu", 10).unwrap().len(), 1);
    }

    #[test]
    fn recent_prefix_does_not_pass_fts_syntax_through() {
        let mut store = Store::open_memory().unwrap();
        insert(&mut store, &[make_observation(0, "echo NEAR(a b)")]);

        // A stray quote or operator in user input must not become FTS syntax.
        assert!(store.search_prefix("\"", 10).unwrap().is_empty());
        assert_eq!(store.search_prefix("NEAR(a", 10).unwrap().len(), 1);
    }

    #[test]
    fn recent_prefix_orders_newest_first() {
        let mut store = Store::open_memory().unwrap();
        // BM25 would put the shortest row first; recency must not.
        insert(&mut store, &[
            make_observation(0, "git"),
            make_observation(1, "git commit -m one two three four"),
            make_observation(2, "ls"),
            make_observation(3, "git push origin main --force"),
        ]);

        let texts = |hits: Vec<SearchHit>| hits.into_iter().map(|h| h.safe_text).collect::<Vec<_>>();
        assert_eq!(
            texts(store.search_prefix("gi", 10).unwrap()),
            ["git push origin main --force", "git commit -m one two three four", "git"]
        );
        assert_eq!(texts(store.search_ranked("git", false, 1).unwrap()), ["git"]);
        assert_eq!(texts(store.recent(2).unwrap()), ["git push origin main --force", "ls"]);
    }

    #[test]
    fn untimed_hits_are_bounded_by_the_next_timed_command() {
        let mut store = Store::open_memory().unwrap();
        let untimed = |o, t| Observation { observed_at_ms: None, ..make_observation(o, t) };
        insert(&mut store, &[
            untimed(0, "ssh old-host"),
            untimed(1, "brew install gh"),
            make_observation(2, "cd Developer"),
            make_observation(3, "ssh new-host"),
            untimed(4, "ssh trailing"),
        ]);

        let hits = store.search("ssh", 10).unwrap();
        let by_text = |t: &str| hits.iter().find(|h| h.safe_text == t).unwrap();
        let first_timed = 1_700_000_000_002;
        assert_eq!(by_text("ssh old-host").observed_before_ms, Some(first_timed));
        // A timed hit carries its own time, not a bound.
        assert_eq!(by_text("ssh new-host").observed_at_ms, Some(1_700_000_000_003));
        assert_eq!(by_text("ssh new-host").observed_before_ms, None);
        // Nothing timed follows the last line: no bound to infer.
        assert_eq!(by_text("ssh trailing").observed_before_ms, None);
    }

    #[test]
    fn bound_lookup_uses_the_partial_index() {
        let store = Store::open_memory().unwrap();
        let plan: String = store.conn.query_row(
            "EXPLAIN QUERY PLAN SELECT observed_at_ms FROM observation
             WHERE revision_id = 1 AND ordinal > 5 AND observed_at_ms IS NOT NULL
             ORDER BY ordinal LIMIT 1",
            [],
            |r| r.get(3),
        ).unwrap();
        // A scan over a fully untimed bash history would cost O(rows) per hit.
        assert!(plan.contains("observation_timed"), "{plan}");
    }

    #[test]
    fn search_escapes_fts_operators() {
        let mut store = Store::open_memory().unwrap();

        // Insert text that contains FTS operator-like chars.
        insert(&mut store, &[
            make_observation(0, "git log --oneline OR --graph"),
        ]);

        // Searching for "OR" as a literal word should not break.
        let hits = store.search("OR", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    fn count(store: &Store, table: &str) -> i64 {
        store.conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_dropped_revision_leaves_nothing_behind() {
        let mut store = Store::open_memory().unwrap();
        let mut rev = begin(&mut store, b"/h");
        rev.insert(&make_observation(0, "cargo build")).unwrap();
        drop(rev);

        for table in ["source", "source_revision", "observation", "observation_fts_docsize"] {
            assert_eq!(count(&store, table), 0, "{table}");
        }
        assert!(store.search("cargo", 10).unwrap().is_empty());
        assert_eq!(newest(&mut store), None);
    }

    #[test]
    fn append_continues_the_revision_or_changes_nothing() {
        let mut store = Store::open_memory().unwrap();
        // Nothing to continue yet: the append opens the first revision.
        let lock = store.lock_source(b"/h").unwrap();
        assert!(lock.anchor(16).unwrap().is_empty());
        drop(lock.append(SourceKind::Zsh).unwrap());
        assert_eq!(newest(&mut store), None);
        insert(&mut store, &[make_observation(0, "one"), make_observation(1, "two")]);
        let first = newest(&mut store).unwrap();

        let mut rev = store.lock_source(b"/h").unwrap().append(SourceKind::Zsh).unwrap();
        rev.insert(&make_observation(2, "abandoned")).unwrap();
        drop(rev);
        assert_eq!(newest(&mut store).unwrap(), first);
        assert!(store.search("abandoned", 10).unwrap().is_empty());

        let cursor = Cursor {
            stat: StatHint { dev: 1, ino: 2, size: 30, mtime_ns: 4, ctime_ns: 5 },
            stable_len: 30,
            boundary_guard: Some([7; 32]),
        };
        let mut rev = store.lock_source(b"/h").unwrap().append(SourceKind::Zsh).unwrap();
        let id = rev.insert(&make_observation(2, "three")).unwrap();
        rev.commit(&cursor, Provenance::OBSERVED_AT).unwrap();

        let lock = store.lock_source(b"/h").unwrap();
        let texts = |n| lock.anchor(n).unwrap().into_iter().map(|(_, text)| text).collect::<Vec<_>>();
        assert_eq!(texts(2), ["two", "three"]);
        assert_eq!(texts(16), ["one", "two", "three"]);
        assert_eq!(lock.anchor(1).unwrap(), [(Some(1_700_000_000_002), "three".to_string())]);
        drop(lock);

        let three = store.get_observation(id).unwrap().unwrap();
        assert_eq!((three.revision_id, three.ordinal), (first.revision_id, 2));
        assert_eq!(store.search("three", 10).unwrap().len(), 1);
        assert_eq!(
            newest(&mut store).unwrap(),
            NewestRevision { revision_id: first.revision_id, cursor }
        );
        // The append adds to what the revision already claimed.
        assert_eq!(
            store.revision_provenance(first.revision_id).unwrap(),
            Provenance::COMMAND.with(Provenance::OBSERVED_AT)
        );
    }

    #[test]
    fn a_locator_is_one_source_with_a_random_logical_id() {
        let mut store = Store::open_memory().unwrap();
        insert(&mut store, &[make_observation(0, "one")]);
        insert(&mut store, &[make_observation(0, "two")]);
        let mut other = store.lock_source(b"/other").unwrap().new_revision(SourceKind::Bash).unwrap();
        other.insert(&make_observation(0, "three")).unwrap();
        other.commit(&Cursor::default(), Provenance::COMMAND).unwrap();

        assert_eq!(count(&store, "source"), 2);
        assert_eq!(count(&store, "source_revision"), 3);
        let ids: Vec<Vec<u8>> = store
            .conn
            .prepare("SELECT logical_id FROM source")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(ids.iter().all(|id| id.len() == 16));
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn read_only_store_searches_while_an_import_transaction_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let mut store = Store::open(&db).unwrap();
        // Readers get past an open import only because of the WAL: a rollback
        // journal would block them once the import spills its page cache.
        let mode: String = store.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        insert(&mut store, &[make_observation(0, "needle committed")]);

        let mut writing = begin(&mut store, b"/other");
        writing.insert(&make_observation(0, "needle pending")).unwrap();

        // Opened while the write lock is held: a writing open would be Busy here.
        let reader = Store::open_read_only(&db).unwrap();
        let found = |reader: &Store| -> Vec<String> {
            reader.search("needle", 10).unwrap().into_iter().map(|h| h.safe_text).collect()
        };
        assert_eq!(found(&reader), ["needle committed"]);

        writing.commit(&Cursor::default(), Provenance::COMMAND).unwrap();
        assert_eq!(found(&reader), ["needle pending", "needle committed"]);
    }

    fn side_files(db: &Path) -> Vec<PathBuf> {
        ["-wal", "-shm", "-journal", ".key"]
            .iter()
            .map(|suffix| {
                let mut path = db.as_os_str().to_owned();
                path.push(suffix);
                PathBuf::from(path)
            })
            .filter(|path| path.exists())
            .collect()
    }

    /// Another program's database, an empty file, a file that is not SQLite,
    /// and a store a pre-release relic wrote (relic's tables, no version).
    fn not_relics(dir: &Path) -> Vec<PathBuf> {
        let foreign = dir.join("notes.db");
        Connection::open(&foreign).unwrap().execute_batch("CREATE TABLE notes(x)").unwrap();
        let empty = dir.join("empty.db");
        std::fs::write(&empty, b"").unwrap();
        let text = dir.join("text.db");
        std::fs::write(&text, "not sqlite at all\n".repeat(20)).unwrap();
        let prototype = dir.join("prototype.db");
        drop(Store::open(&prototype).unwrap());
        Connection::open(&prototype)
            .unwrap()
            .execute_batch("PRAGMA user_version = 0; PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        // A version is no identity: plenty of programs stamp 1.
        let stamped = dir.join("stamped.db");
        Connection::open(&stamped).unwrap().execute_batch("CREATE TABLE notes(x); PRAGMA user_version = 1").unwrap();
        vec![foreign, empty, text, prototype, stamped]
    }

    #[test]
    fn a_reader_refuses_a_file_that_is_not_a_store_and_leaves_it_alone() {
        let dir = tempfile::tempdir().unwrap();
        for path in not_relics(dir.path()) {
            let before = std::fs::read(&path).unwrap();
            let err = Store::open_read_only(&path).err().expect("must refuse");
            // An empty file may be a store an import is creating right now:
            // "move it away" would be the wrong advice.
            if path.ends_with("empty.db") {
                assert!(matches!(&err, StoreError::NoStoreYet(p) if *p == path), "{err}");
            } else {
                assert!(matches!(&err, StoreError::NotRelic(p) if *p == path), "{err}");
            }
            assert!(err.to_string().contains("relic import"), "no hint: {err}");
            assert!(std::fs::read(&path).unwrap() == before, "{} changed", path.display());
            // The pre-release store is in WAL mode, and reading even its
            // header creates the -wal and -shm.
            if !path.ends_with("prototype.db") {
                assert_eq!(side_files(&path), Vec::<PathBuf>::new());
            }
        }
    }

    #[test]
    fn a_writer_refuses_another_programs_file_and_leaves_it_alone() {
        let dir = tempfile::tempdir().unwrap();
        for path in not_relics(dir.path()) {
            // An empty file is nobody's: a writer may make it a store.
            if path.ends_with("empty.db") {
                continue;
            }
            let before = std::fs::read(&path).unwrap();
            let err = Store::open(&path).err().expect("must refuse");
            assert!(matches!(&err, StoreError::NotRelic(p) if *p == path), "{err}");
            assert!(std::fs::read(&path).unwrap() == before, "{} changed", path.display());
        }
    }

    #[test]
    fn both_opens_refuse_another_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        drop(Store::open(&db).unwrap());
        Connection::open(&db).unwrap().execute_batch("PRAGMA user_version = 99").unwrap();
        for open in [Store::open, Store::open_read_only] {
            let err = open(&db).err().expect("must refuse");
            assert!(matches!(err, StoreError::Version { found: 99, .. }), "{err}");
        }
    }

    #[test]
    fn a_reader_cannot_write_and_a_search_changes_no_byte_of_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let (store, _) = store_with(&db, &["cargo build"]);
        assert!(store.checkpoint().unwrap());
        drop(store);
        let before = std::fs::read(&db).unwrap();

        let reader = Store::open_read_only(&db).unwrap();
        assert_eq!(reader.search("cargo", 10).unwrap().len(), 1);
        let err = reader.conn.execute("DELETE FROM observation", []).unwrap_err();
        assert_eq!(err.sqlite_error_code(), Some(ErrorCode::ReadOnly), "{err}");
        drop(reader);
        assert!(std::fs::read(&db).unwrap() == before, "the search wrote to the database");
    }

    #[test]
    fn a_reader_refuses_what_it_cannot_write_beside_and_leaves_no_side_file() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path, m| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let db = sub.join("relic.db");
        let (store, _) = store_with(&db, &["cargo build"]);
        assert!(store.checkpoint().unwrap());
        // The last connection to close removes the -wal and -shm.
        drop(store);
        let clean = side_files(&db);

        mode(&db, 0o444);
        let err = Store::open_read_only(&db).err().expect("read-only file");
        assert!(matches!(&err, StoreError::Unwritable(p) if *p == db.canonicalize().unwrap()), "{err}");
        mode(&db, 0o644);

        mode(&sub, 0o555);
        let err = Store::open_read_only(&db).err().expect("read-only directory");
        mode(&sub, 0o755);
        let real_sub = sub.canonicalize().unwrap();
        assert!(matches!(&err, StoreError::Unwritable(p) if *p == real_sub), "{err}");
        assert_eq!(side_files(&db), clean);

        // Through a symlink it is the target's directory that counts.
        let link = dir.path().join("link.db");
        std::os::unix::fs::symlink(&db, &link).unwrap();
        mode(&sub, 0o555);
        let err = Store::open_read_only(&link).err().expect("link into a read-only directory");
        mode(&sub, 0o755);
        assert!(matches!(&err, StoreError::Unwritable(p) if *p == real_sub), "{err}");
        assert_eq!(Store::open_read_only(&link).unwrap().search("cargo", 10).unwrap().len(), 1);
    }

    #[test]
    fn interruptible_stops_a_running_search_and_only_that_one() {
        let mut store = Store::open_memory().unwrap();
        let batch: Vec<Observation> = (0..2_000).map(|i| make_observation(i, &format!("git commit {i}"))).collect();
        insert(&mut store, &batch);

        let err = store.interruptible(|| true, |s| s.search_ranked("git", false, 10)).unwrap_err();
        assert!(is_interrupt(&err), "{err}");
        // The handler goes away with the call.
        assert_eq!(store.search_ranked("git", false, 10).unwrap().len(), 10);

        // An interrupt while the rows are fetched fails the page: it used to
        // drop the rest and return a short page as if it were complete. Where
        // the n-th poll lands is SQLite's business, so try every one of them.
        let limit = 2_000;
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = |nth: usize| {
            let polls = polls.clone();
            polls.store(0, std::sync::atomic::Ordering::Relaxed);
            move || polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= nth
        };
        assert_eq!(store.interruptible(count(usize::MAX), |s| s.search("git", limit)).unwrap().len(), limit);
        let total = polls.load(std::sync::atomic::Ordering::Relaxed);
        for nth in 0..total {
            match store.interruptible(count(nth), |s| s.search("git", limit)) {
                Ok(hits) => assert_eq!(hits.len(), limit, "poll {nth} of {total}: a short page passed off as whole"),
                Err(e) => assert!(is_interrupt(&e), "{e}"),
            }
        }
    }

    #[test]
    fn rank_ties_come_most_recently_imported_first() {
        let mut store = Store::open_memory().unwrap();
        let ids = insert(&mut store, &[
            make_observation(0, "git push"),
            make_observation(1, "ls"),
            make_observation(2, "git push"),
            make_observation(3, "git push"),
        ]);
        let got: Vec<i64> = store.search_ranked("push", false, 10).unwrap().iter().map(|h| h.id).collect();
        assert_eq!(got, [ids[3], ids[2], ids[0]]);
    }

    /// Every byte SQLite keeps for `db`: the file, its WAL and its shared memory.
    fn on_disk(db: &std::path::Path) -> Vec<u8> {
        ["", "-wal", "-shm"]
            .iter()
            .flat_map(|suffix| {
                let mut path = db.as_os_str().to_owned();
                path.push(suffix);
                std::fs::read(path).unwrap_or_default()
            })
            .collect()
    }

    fn holds(bytes: &[u8], needle: &str) -> bool {
        bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    fn store_with(db: &std::path::Path, texts: &[&str]) -> (Store, Vec<i64>) {
        let mut store = Store::open(db).unwrap();
        let mut rev = store.lock_source(b"/h").unwrap().new_revision(SourceKind::Zsh).unwrap();
        let ids = texts.iter().map(|t| rev.insert(&make_observation(0, t)).unwrap()).collect();
        rev.commit(&Cursor::default(), Provenance::COMMAND).unwrap();
        (store, ids)
    }

    #[test]
    fn a_rescrub_leaves_no_byte_of_the_old_text_and_no_old_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let (mut store, old_ids) = store_with(&db, &["deploy LEAKEDCANARY7 now", "ls -la"]);
        assert!(holds(&on_disk(&db), "LEAKEDCANARY7"));

        // Stored under the current revision: nothing to do.
        assert_eq!(store.rescrub(crate::redact::REDACTION_REVISION, str::to_string).unwrap(), (0, true));
        assert!(store.get_observation(old_ids[0]).unwrap().is_some());

        let rewritten = store
            .rescrub("r-next", |text| text.replace("LEAKEDCANARY7", "[REDACTED:secret]"))
            .unwrap();
        assert_eq!(rewritten, (2, true));

        let on_disk = on_disk(&db);
        assert!(!holds(&on_disk, "LEAKEDCANARY7"), "old text survives in the db, WAL or shm");
        assert!(!holds(&on_disk, "leakedcanary7"), "old term survives in the search index");
        assert!(store.search("LEAKEDCANARY7", 10).unwrap().is_empty());
        let hits = store.search("deploy", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].safe_text, "deploy [REDACTED:secret] now");
        // A citation made before the rescrub is stale, never a different row.
        assert!(old_ids.iter().all(|id| store.get_observation(*id).unwrap().is_none()));
        assert!(hits[0].id > old_ids[1]);

        let revisions: Vec<String> = store
            .conn
            .prepare("SELECT redaction_revision FROM source_revision")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(revisions, ["r-next"]);
        store.conn.execute("INSERT INTO observation_fts (observation_fts) VALUES ('integrity-check')", []).unwrap();
        // The order within the revision is what it was.
        let kept = store.get_neighbors(store.get_observation(hits[0].id).unwrap().unwrap().revision_id, 0, 0, 5).unwrap();
        assert_eq!(kept.iter().map(|o| o.safe_text.as_str()).collect::<Vec<_>>(), ["deploy [REDACTED:secret] now", "ls -la"]);
    }

    #[test]
    fn a_forgotten_observation_is_gone_from_disk_and_its_id_is_never_reused() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let (mut store, ids) = store_with(&db, &["echo one", "echo two", "curl LEAKEDCANARY8"]);

        // An id that never existed is not reported as forgotten.
        assert_eq!(store.forget(&[ids[2], 999]).unwrap(), (vec![ids[2]], true));

        let on_disk = on_disk(&db);
        assert!(!holds(&on_disk, "LEAKEDCANARY8") && !holds(&on_disk, "leakedcanary8"));
        assert!(store.search("LEAKEDCANARY8", 10).unwrap().is_empty());
        assert!(store.get_observation(ids[2]).unwrap().is_none());
        assert_eq!(store.search("echo", 10).unwrap().len(), 2);
        store.conn.execute("INSERT INTO observation_fts (observation_fts) VALUES ('integrity-check')", []).unwrap();

        // The forgotten row had the highest id; the next row must not take it.
        let mut rev = store.lock_source(b"/h").unwrap().append(SourceKind::Zsh).unwrap();
        let next = rev.insert(&make_observation(0, "echo three")).unwrap();
        rev.commit(&Cursor::default(), Provenance::COMMAND).unwrap();
        assert!(next > ids[2]);
    }

    #[test]
    fn a_scrub_a_reader_blocked_is_reported_and_finished_by_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let (mut store, ids) = store_with(&db, &["echo one", "curl LEAKEDCANARY6"]);

        // A reader mid-query, as the TUI is while it is open.
        let reader = rusqlite::Connection::open(&db).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        reader.query_row("SELECT COUNT(*) FROM observation", [], |r| r.get::<_, i64>(0)).unwrap();

        assert_eq!(store.forget(&[ids[1]]).unwrap(), (vec![ids[1]], false));
        assert!(holds(&on_disk(&db), "LEAKEDCANARY6"), "the reader's snapshot should still pin the old pages");

        // Still blocked: the same forget again has nothing to delete and
        // must not claim the bytes are gone.
        assert_eq!(store.forget(&[ids[1]]).unwrap(), (vec![], false));

        reader.execute_batch("COMMIT").unwrap();
        // The same forget once more: nothing to delete, but it finishes the scrub.
        assert_eq!(store.forget(&[ids[1]]).unwrap(), (vec![], true));
        assert!(!holds(&on_disk(&db), "LEAKEDCANARY6"));
        // An import would have finished it too, and now finds nothing owed.
        assert_eq!(store.rescrub(crate::redact::REDACTION_REVISION, str::to_string).unwrap(), (0, true));
        // And only once.
        let pending: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM schema_meta WHERE key = 'scrub_pending'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending, 0);
    }

}
