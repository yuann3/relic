use std::path::Path;

use rusqlite::{Connection, ErrorCode};

use super::StoreError;

/// Kept in `PRAGMA user_version`: a reader can check it without a table to
/// read it from, and a file that is not Relic's says 0 (ADR 0005).
pub const SCHEMA_VERSION: u32 = 1;

/// How long a connection waits on another's lock before it is `Busy`. Set
/// through the API, which reads nothing: the version check comes first.
pub const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Whether the file at `path` is this build's store: `Ok(false)` when it
/// holds nothing at all, which only a writer may turn into one. A version
/// alone is no identity (other programs stamp 1 too), so Relic's own index
/// table has to be there. One statement, so one snapshot: a first import
/// committing between two reads would look like a foreign file.
pub fn is_store(conn: &Connection, path: &Path) -> Result<bool, StoreError> {
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
        Ok((_, 0, _)) => Ok(false),
        Ok((_, _, 0)) | Ok((0, _, _)) => Err(not_relic()),
        Ok((v, _, _)) if v == i64::from(SCHEMA_VERSION) => Ok(true),
        Ok((found, _, _)) => Err(StoreError::Version {
            path: path.to_path_buf(),
            found,
        }),
    }
}

/// Set the writer's pragmas, and create the schema in a file that has none.
pub fn initialize(conn: &Connection, path: &Path) -> Result<(), StoreError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // Decided before anything writes: `journal_mode = WAL` alone would
    // convert a file that is another program's.
    let empty = !is_store(conn, path)?;

    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -16384;
         PRAGMA secure_delete = ON;",
    )?;

    if empty {
        // One transaction, so a reader never finds half a schema, and the
        // version goes in last. Without 'secure-delete' FTS5 only tombstones
        // a deleted row and its terms stay in the index until some later
        // merge: a forgotten secret would still be on disk. The setting is
        // stored in the index, so once is enough.
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             {SCHEMA_SQL}
             INSERT INTO observation_fts (observation_fts, rank) VALUES ('secure-delete', 1);
             PRAGMA user_version = {SCHEMA_VERSION};
             COMMIT;"
        ))?;
    }
    Ok(())
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
