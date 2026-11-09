use std::path::Path;

use rusqlite::{Connection, ErrorCode, Transaction, TransactionBehavior};

use super::StoreError;

/// Kept in `PRAGMA user_version`: a reader can check it without a table to
/// read it from, and a file that is not Relic's says 0 (ADR 0005).
pub const SCHEMA_VERSION: u32 = 2;

/// Forward-only migrations, run by writers only (ADR 0005): the n-th takes
/// a store from version n to n + 1. A new store is [`SCHEMA_SQL`] with all
/// of them applied.
const MIGRATIONS: [&str; 1] = [
    // A Git ref the user forgot, so the next import of its repository does
    // not store it again. A keyed hash of its safe text, never the text.
    "CREATE TABLE forgotten_ref (
         source_id INTEGER NOT NULL REFERENCES source(id),
         mark      BLOB NOT NULL,
         PRIMARY KEY (source_id, mark)
     ) WITHOUT ROWID;",
];

/// How long a connection waits on another's lock before it is `Busy`. Set
/// through the API, which reads nothing: the version check comes first.
pub const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// The schema version of the store at `path`: `None` when the file holds
/// nothing at all, which only a writer may turn into one, and an error for
/// a version newer than this build's. A version alone is no identity (other
/// programs stamp 1 too), so Relic's own index table has to be there. One
/// statement, so one snapshot: a first import committing between two reads
/// would look like a foreign file.
pub fn store_version(conn: &Connection, path: &Path) -> Result<Option<u32>, StoreError> {
    let read = conn.query_row(
        "SELECT user_version,
                (SELECT COUNT(*) FROM sqlite_master),
                (SELECT COUNT(*) FROM sqlite_master WHERE name = 'observation_fts')
         FROM pragma_user_version",
        [],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    );
    let not_relic = || StoreError::NotRelic(path.to_path_buf());
    match read {
        Err(e) if e.sqlite_error_code() == Some(ErrorCode::NotADatabase) => Err(not_relic()),
        Err(e) => Err(e.into()),
        Ok((_, 0, _)) => Ok(None),
        Ok((_, _, 0)) | Ok((0, _, _)) => Err(not_relic()),
        Ok((found, _, _)) => match u32::try_from(found) {
            Ok(v) if v <= SCHEMA_VERSION => Ok(Some(v)),
            _ => Err(StoreError::Version {
                path: path.to_path_buf(),
                found,
            }),
        },
    }
}

/// Set the writer's pragmas, and create the schema in a file that has none.
pub fn initialize(conn: &Connection, path: &Path) -> Result<(), StoreError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // Decided before anything writes: `journal_mode = WAL` alone would
    // convert a file that is another program's.
    let found = store_version(conn, path)?;

    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -16384;
         PRAGMA secure_delete = ON;",
    )?;

    if found.is_none() {
        // One transaction, so a reader never finds half a schema, and the
        // version goes in last. Without 'secure-delete' FTS5 only tombstones
        // a deleted row and its terms stay in the index until some later
        // merge: a forgotten secret would still be on disk. The setting is
        // stored in the index, so once is enough.
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             {SCHEMA_SQL}
             {}
             INSERT INTO observation_fts (observation_fts, rank) VALUES ('secure-delete', 1);
             PRAGMA user_version = {SCHEMA_VERSION};
             COMMIT;",
            MIGRATIONS.concat()
        ))?;
    } else if found < Some(SCHEMA_VERSION) {
        migrate(conn)?;
    }
    Ok(())
}

/// Bring a store an older relic wrote up to [`SCHEMA_VERSION`], one
/// migration per exclusive transaction. The version is read again under the
/// lock, so two writers opening it at once never run one twice.
fn migrate(conn: &Connection) -> Result<(), StoreError> {
    loop {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Exclusive)?;
        let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let Some(sql) = MIGRATIONS.get(version as usize - 1) else {
            return Ok(());
        };
        tx.execute_batch(&format!("{sql} PRAGMA user_version = {};", version + 1))?;
        tx.commit()?;
    }
}

/// What a reader may set: nothing here writes to the file.
pub fn configure_reader(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -16384;",
    )
}

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS schema_meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS source (
    id              INTEGER PRIMARY KEY,
    -- Random, assigned at first import. The path lives only in private_locator.
    logical_id      BLOB NOT NULL UNIQUE,
    kind            INTEGER NOT NULL,
    private_locator BLOB NOT NULL UNIQUE,
    parser_contract TEXT NOT NULL,
    enabled         INTEGER NOT NULL CHECK (enabled IN (0,1))
);

-- A source's newest revision carries its import cursor: the stat hint
-- (dev..ctime_ns), stable_len and boundary_guard. provenance is the
-- revision-wide domain::Provenance bit set.
CREATE TABLE IF NOT EXISTS source_revision (
    id                   INTEGER PRIMARY KEY,
    source_id            INTEGER NOT NULL REFERENCES source(id),
    dev                  INTEGER NOT NULL,
    ino                  INTEGER NOT NULL,
    size                 INTEGER NOT NULL,
    mtime_ns             INTEGER NOT NULL,
    ctime_ns             INTEGER NOT NULL,
    stable_len           INTEGER NOT NULL,
    boundary_guard       BLOB,
    provenance           INTEGER NOT NULL,
    parser_contract      TEXT NOT NULL,
    redaction_revision   TEXT NOT NULL,
    imported_at_ms       INTEGER NOT NULL
);

-- AUTOINCREMENT: forget and rescrub delete rows, and an id handed out as a
-- citation must never come back as a different command.
CREATE TABLE IF NOT EXISTS observation (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    revision_id        INTEGER NOT NULL REFERENCES source_revision(id),
    ordinal            INTEGER NOT NULL,
    kind               INTEGER NOT NULL,
    observed_at_ms     INTEGER,
    safe_text          TEXT NOT NULL,
    flags              INTEGER NOT NULL DEFAULT 0,
    locator            BLOB NOT NULL,
    UNIQUE(revision_id, ordinal)
);

CREATE VIRTUAL TABLE IF NOT EXISTS observation_fts USING fts5(
    safe_text,
    content='observation',
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE INDEX IF NOT EXISTS observation_time
    ON observation(observed_at_ms, id);

-- Finds the next timestamped command after an untimed one. Partial, so it
-- costs nothing for untimed sources; it replaces an index on
-- (revision_id, ordinal) that duplicated the UNIQUE constraint's.
CREATE INDEX IF NOT EXISTS observation_timed
    ON observation(revision_id, ordinal) WHERE observed_at_ms IS NOT NULL;
";
