use rusqlite::Connection;

pub const SCHEMA_VERSION: u32 = 1;

/// Initialize pragmas and create tables if they don't exist.
pub fn initialize(conn: &Connection) -> Result<(), rusqlite::Error> {
    // Pragmas — applied on every connection open.
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA busy_timeout = 250;
         PRAGMA cache_size = -16384;",
    )?;

    // Schema creation — idempotent via IF NOT EXISTS.
    conn.execute_batch(SCHEMA_SQL)?;

    // Set version if not present.
    conn.execute(
        "INSERT OR IGNORE INTO schema_meta (key, value) VALUES ('version', ?1)",
        [SCHEMA_VERSION.to_string()],
    )?;

    Ok(())
}

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS schema_meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS source (
    id              INTEGER PRIMARY KEY,
    logical_id      BLOB NOT NULL UNIQUE,
    kind            INTEGER NOT NULL,
    private_locator BLOB NOT NULL,
    parser_contract TEXT NOT NULL,
    enabled         INTEGER NOT NULL CHECK (enabled IN (0,1))
);

CREATE TABLE IF NOT EXISTS source_revision (
    id                   INTEGER PRIMARY KEY,
    source_id            INTEGER NOT NULL REFERENCES source(id),
    revision_key         BLOB NOT NULL UNIQUE,
    file_identity        BLOB,
    opened_len           INTEGER NOT NULL,
    stable_len           INTEGER NOT NULL,
    boundary_guard       BLOB NOT NULL,
    snapshot_fingerprint BLOB NOT NULL,
    parser_contract      TEXT NOT NULL,
    redaction_revision   TEXT NOT NULL,
    imported_at_ms       INTEGER NOT NULL,
    complete             INTEGER NOT NULL CHECK (complete IN (0,1))
);

CREATE TABLE IF NOT EXISTS observation (
    id                 INTEGER PRIMARY KEY,
    revision_id        INTEGER NOT NULL REFERENCES source_revision(id),
    ordinal            INTEGER NOT NULL,
    kind               INTEGER NOT NULL,
    observed_at_ms     INTEGER,
    safe_text          TEXT NOT NULL,
    safe_hash          BLOB NOT NULL,
    keyed_fingerprint  BLOB NOT NULL,
    flags              INTEGER NOT NULL DEFAULT 0,
    provenance         BLOB NOT NULL,
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
