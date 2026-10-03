mod schema;

pub use schema::SCHEMA_VERSION;

use crate::domain::{Observation, ObservationKind, SearchHit, StoredObservation};
use rusqlite::Connection;

/// Local observation store backed by SQLite + FTS5.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open or create a store at the given path.
    pub fn open(path: &std::path::Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        schema::initialize(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory store (for tests).
    pub fn open_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        schema::initialize(&conn)?;
        Ok(Self { conn })
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
        self.conn
            .query_row("SELECT value FROM schema_meta WHERE key = 'version'", [], |row| {
                row.get::<_, String>(0)
            })
            .map(|v| v.parse::<i64>().unwrap_or(0))
    }

    /// Create a source (if not exists) and a new import revision. Returns revision_id.
    pub fn create_import_revision(
        &self,
        path: &std::path::Path,
        kind: crate::import::SourceKind,
        file_len: i64,
    ) -> Result<i64, rusqlite::Error> {
        let path_bytes = path.to_string_lossy();
        let kind_int: i64 = match kind {
            crate::import::SourceKind::Zsh => 1,
            crate::import::SourceKind::Bash => 2,
        };
        let logical_id = path_bytes.as_bytes();

        // Upsert source.
        self.conn.execute(
            "INSERT OR IGNORE INTO source (logical_id, kind, private_locator, parser_contract, enabled)
             VALUES (?1, ?2, ?3, ?4, 1)",
            rusqlite::params![logical_id, kind_int, path_bytes.as_bytes(), "v1"],
        )?;
        let source_id: i64 = self.conn.query_row(
            "SELECT id FROM source WHERE logical_id = ?1",
            [logical_id],
            |row| row.get(0),
        )?;

        // Create revision.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        self.conn.execute(
            "INSERT INTO source_revision (source_id, revision_key, opened_len, stable_len,
                boundary_guard, snapshot_fingerprint, parser_contract, redaction_revision,
                imported_at_ms, complete)
             VALUES (?1, ?2, ?3, ?3, X'00', X'00', 'v1', 'r1', ?4, 0)",
            rusqlite::params![source_id, now_ms.to_le_bytes().as_slice(), file_len, now_ms],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Mark a revision as complete.
    pub fn complete_revision(&self, revision_id: i64) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE source_revision SET complete = 1 WHERE id = ?1",
            [revision_id],
        )?;
        Ok(())
    }

    /// Check if a source already has a complete revision with the given file length.
    /// Returns true if the file is unchanged and can be skipped.
    pub fn has_unchanged_revision(
        &self,
        path: &std::path::Path,
        file_len: i64,
    ) -> Result<bool, rusqlite::Error> {
        let path_bytes = path.to_string_lossy();
        let logical_id = path_bytes.as_bytes();

        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM source s
             JOIN source_revision sr ON sr.source_id = s.id
             WHERE s.logical_id = ?1 AND sr.stable_len = ?2 AND sr.complete = 1",
            rusqlite::params![logical_id, file_len],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Count total observations in the store.
    pub fn observation_count(&self) -> Result<i64, rusqlite::Error> {
        self.conn.query_row("SELECT COUNT(*) FROM observation", [], |row| row.get(0))
    }

    /// Create a dummy source + revision for tests. Returns the revision_id.
    pub fn insert_raw_source_and_revision(&self) -> Result<i64, rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO source (logical_id, kind, private_locator, parser_contract, enabled)
             VALUES (X'01', 1, X'00', 'zsh/v1', 1)",
            [],
        )?;
        let source_id = self.conn.last_insert_rowid();
        self.conn.execute(
            "INSERT INTO source_revision (source_id, revision_key, opened_len, stable_len,
                boundary_guard, snapshot_fingerprint, parser_contract, redaction_revision,
                imported_at_ms, complete)
             VALUES (?1, X'AA', 1000, 1000, X'BB', X'CC', 'zsh/v1', 'r1', 0, 1)",
            [source_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Checkpoint the WAL. Call after large imports to compact the database.
    pub fn checkpoint(&self) -> Result<(), rusqlite::Error> {
        self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }

    /// Insert observations in a batch transaction. Returns assigned row IDs.
    pub fn insert_observations(&mut self, obs: &[Observation]) -> Result<Vec<i64>, rusqlite::Error> {
        let tx = self.conn.transaction()?;
        let mut ids = Vec::with_capacity(obs.len());

        {
            let mut insert_obs = tx.prepare_cached(
                "INSERT INTO observation (revision_id, ordinal, kind, observed_at_ms, safe_text,
                    safe_hash, keyed_fingerprint, flags, provenance, locator)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            let mut insert_fts = tx.prepare_cached(
                "INSERT INTO observation_fts (rowid, safe_text) VALUES (?1, ?2)",
            )?;

            for o in obs {
                insert_obs.execute(rusqlite::params![
                    o.revision_id,
                    o.ordinal,
                    o.kind.as_i64(),
                    o.observed_at_ms,
                    o.safe_text,
                    o.safe_hash,
                    o.keyed_fingerprint,
                    o.flags,
                    o.provenance,
                    o.locator,
                ])?;
                let id = tx.last_insert_rowid();
                insert_fts.execute(rusqlite::params![id, o.safe_text])?;
                ids.push(id);
            }
        }

        tx.commit()?;
        Ok(ids)
    }

    /// Search observations via FTS5. Query terms are escaped (no raw FTS syntax).
    /// Returns up to `limit` hits ranked by BM25.
    ///
    /// Two-step: (1) FTS5 narrowing with `ORDER BY rank` for the internal
    /// priority-queue optimization, (2) fetch observation metadata by rowid.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, rusqlite::Error> {
        self.search_fts(&escape_fts_query(query, false), limit)
    }

    /// Like [`Store::search`], but the last word matches as a prefix, so
    /// partially typed input ("gi") finds completed words ("git").
    pub fn search_prefix(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, rusqlite::Error> {
        self.search_fts(&escape_fts_query(query, true), limit)
    }

    fn search_fts(&self, escaped: &str, limit: usize) -> Result<Vec<SearchHit>, rusqlite::Error> {
        if escaped.is_empty() {
            return Ok(vec![]);
        }

        // Step 1: Get top-N rowids from FTS5 with rank optimization.
        let mut fts_stmt = self.conn.prepare_cached(
            "SELECT rowid, rank FROM observation_fts
             WHERE observation_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;
        let ranked: Vec<(i64, f64)> = fts_stmt
            .query_map(rusqlite::params![escaped, limit as i64], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        if ranked.is_empty() {
            return Ok(vec![]);
        }

        // Step 2: Fetch observation data for those rowids.
        let mut obs_stmt = self.conn.prepare_cached(
            "SELECT id, observed_at_ms, kind, safe_text, revision_id, ordinal
             FROM observation WHERE id = ?1",
        )?;

        let mut hits = Vec::with_capacity(ranked.len());
        for (rowid, score) in &ranked {
            if let Ok(row) = obs_stmt
                .query_row([rowid], |row| {
                    let kind_val: i64 = row.get(2)?;
                    Ok(SearchHit {
                        id: row.get(0)?,
                        revision_id: row.get(4)?,
                        ordinal: row.get(5)?,
                        observed_at_ms: row.get(1)?,
                        kind: ObservationKind::from_i64(kind_val)
                            .unwrap_or(ObservationKind::ZshCommand),
                        safe_text: row.get(3)?,
                        score: *score,
                    })
                })
            {
                hits.push(row);
            }
        }

        Ok(hits)
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
                    safe_hash, keyed_fingerprint, flags, provenance, locator
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
                    safe_hash, keyed_fingerprint, flags, provenance, locator
             FROM observation WHERE id = ?1",
        )?;
        let mut rows = stmt.query([id])?;
        match rows.next()? {
            Some(row) => Ok(Some(row_to_stored_observation(row)?)),
            None => Ok(None),
        }
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
        safe_hash: row.get(6)?,
        keyed_fingerprint: row.get(7)?,
        flags: row.get(8)?,
        provenance: row.get(9)?,
        locator: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Insert a dummy source + revision so FK constraints are satisfied.
    /// Returns the revision_id.
    fn setup_revision(store: &Store) -> i64 {
        store.conn.execute(
            "INSERT INTO source (logical_id, kind, private_locator, parser_contract, enabled)
             VALUES (X'01', 1, X'00', 'zsh/v1', 1)",
            [],
        ).unwrap();
        let source_id = store.conn.last_insert_rowid();
        store.conn.execute(
            "INSERT INTO source_revision (source_id, revision_key, opened_len, stable_len,
                boundary_guard, snapshot_fingerprint, parser_contract, redaction_revision,
                imported_at_ms, complete)
             VALUES (?1, X'AA', 1000, 1000, X'BB', X'CC', 'zsh/v1', 'r1', 0, 1)",
            [source_id],
        ).unwrap();
        store.conn.last_insert_rowid()
    }

    fn make_observation(revision_id: i64, ordinal: i64, text: &str) -> Observation {
        Observation {
            revision_id,
            ordinal,
            kind: ObservationKind::ZshCommand,
            observed_at_ms: Some(1_700_000_000_000 + ordinal),
            safe_text: text.to_string(),
            safe_hash: vec![0; 4],
            keyed_fingerprint: vec![0; 4],
            flags: 0,
            provenance: vec![],
            locator: vec![],
        }
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
        let rev_id = setup_revision(&store);

        let obs = make_observation(rev_id, 0, "cargo build --release");
        let ids = store.insert_observations(&[obs]).unwrap();
        assert_eq!(ids.len(), 1);

        let stored = store.get_observation(ids[0]).unwrap().expect("should exist");
        assert_eq!(stored.safe_text, "cargo build --release");
        assert_eq!(stored.kind, ObservationKind::ZshCommand);
        assert_eq!(stored.ordinal, 0);
        assert_eq!(stored.revision_id, rev_id);
    }

    #[test]
    fn insert_batch_assigns_sequential_ids() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = setup_revision(&store);

        let batch: Vec<Observation> = (0..5)
            .map(|i| make_observation(rev_id, i, &format!("cmd_{i}")))
            .collect();
        let ids = store.insert_observations(&batch).unwrap();

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
        let rev_id = setup_revision(&store);

        store.insert_observations(&[
            make_observation(rev_id, 0, "cargo build --release"),
            make_observation(rev_id, 1, "git push origin main"),
            make_observation(rev_id, 2, "cargo test --lib"),
        ]).unwrap();

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
        let rev_id = setup_revision(&store);

        let batch: Vec<Observation> = (0..20)
            .map(|i| make_observation(rev_id, i, &format!("cargo command_{i}")))
            .collect();
        store.insert_observations(&batch).unwrap();

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
        let rev_id = setup_revision(&store);

        store.insert_observations(&[
            make_observation(rev_id, 0, "cargo build"),
        ]).unwrap();

        let hits = store.search("nonexistent_term", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_prefix_matches_partial_last_word() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = setup_revision(&store);
        store.insert_observations(&[
            make_observation(rev_id, 0, "git push origin main"),
            make_observation(rev_id, 1, "cargo build"),
        ]).unwrap();

        assert!(store.search("gi", 10).unwrap().is_empty());
        let hits = store.search_prefix("gi", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].safe_text, "git push origin main");
        // Earlier words still match whole.
        assert!(store.search_prefix("gi pu", 10).unwrap().is_empty());
        assert_eq!(store.search_prefix("git pu", 10).unwrap().len(), 1);
    }

    #[test]
    fn search_prefix_does_not_pass_fts_syntax_through() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = setup_revision(&store);
        store.insert_observations(&[make_observation(rev_id, 0, "echo NEAR(a b)")]).unwrap();

        // A stray quote or operator in user input must not become FTS syntax.
        assert!(store.search_prefix("\"", 10).unwrap().is_empty());
        assert_eq!(store.search_prefix("NEAR(a", 10).unwrap().len(), 1);
    }

    #[test]
    fn search_hit_locates_its_timeline() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = setup_revision(&store);
        store.insert_observations(&[
            make_observation(rev_id, 0, "cd project"),
            make_observation(rev_id, 1, "certbot renew"),
            make_observation(rev_id, 2, "systemctl reload nginx"),
        ]).unwrap();

        let hit = &store.search("certbot", 10).unwrap()[0];
        assert_eq!((hit.revision_id, hit.ordinal), (rev_id, 1));
        let around = store.get_neighbors(hit.revision_id, hit.ordinal, 1, 1).unwrap();
        let texts: Vec<&str> = around.iter().map(|o| o.safe_text.as_str()).collect();
        assert_eq!(texts, ["cd project", "certbot renew", "systemctl reload nginx"]);
    }

    #[test]
    fn search_escapes_fts_operators() {
        let mut store = Store::open_memory().unwrap();
        let rev_id = setup_revision(&store);

        // Insert text that contains FTS operator-like chars.
        store.insert_observations(&[
            make_observation(rev_id, 0, "git log --oneline OR --graph"),
        ]).unwrap();

        // Searching for "OR" as a literal word should not break.
        let hits = store.search("OR", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }
}
