use std::fs;
use std::path::Path;

use crate::domain::{Observation, ObservationKind};
use crate::redact::Redactor;
use crate::source::{self, RawRecord};
use crate::store::Store;

/// Which shell format to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Zsh,
    Bash,
}

/// Result of an import operation.
#[derive(Debug)]
pub struct ImportResult {
    pub records_parsed: usize,
    pub records_stored: usize,
    pub revision_id: i64,
    pub skipped: bool,
}

/// Import a history file into the store.
///
/// Reads the file, parses it, redacts sensitive content, and inserts
/// observations in a single transaction. Skips import if the file
/// is unchanged since the last complete revision.
pub fn import_file(
    store: &mut Store,
    path: &Path,
    kind: SourceKind,
    redactor: &Redactor,
) -> Result<ImportResult, ImportError> {
    let metadata = fs::metadata(path).map_err(ImportError::Io)?;
    let file_len = metadata.len() as i64;

    // No-change detection: skip if same file length with a complete revision.
    if store
        .has_unchanged_revision(path, file_len)
        .map_err(ImportError::Store)?
    {
        return Ok(ImportResult {
            records_parsed: 0,
            records_stored: 0,
            revision_id: 0,
            skipped: true,
        });
    }

    let data = fs::read(path).map_err(ImportError::Io)?;

    let raw_records = match kind {
        SourceKind::Zsh => source::zsh::parse(&data),
        SourceKind::Bash => source::bash::parse(&data),
    };
    let records_parsed = raw_records.len();

    // Register source and revision in the store.
    let file_len = data.len() as i64;
    let revision_id = store
        .create_import_revision(path, kind, file_len)
        .map_err(ImportError::Store)?;

    // Convert and insert in batches to bound memory.
    let obs_kind = match kind {
        SourceKind::Zsh => ObservationKind::ZshCommand,
        SourceKind::Bash => ObservationKind::BashCommand,
    };

    const BATCH_SIZE: usize = 10_000;
    let mut total_stored = 0;

    for chunk in raw_records.chunks(BATCH_SIZE) {
        let observations: Vec<Observation> = chunk
            .iter()
            .enumerate()
            .map(|(i, raw)| {
                let ordinal = (total_stored + i) as i64;
                raw_to_observation(raw, revision_id, ordinal, obs_kind, redactor)
            })
            .collect();

        let ids = store
            .insert_observations(&observations)
            .map_err(ImportError::Store)?;
        total_stored += ids.len();
    }

    // Mark revision complete.
    store
        .complete_revision(revision_id)
        .map_err(ImportError::Store)?;

    Ok(ImportResult {
        records_parsed,
        records_stored: total_stored,
        revision_id,
        skipped: false,
    })
}

fn raw_to_observation(
    raw: &RawRecord,
    revision_id: i64,
    ordinal: i64,
    kind: ObservationKind,
    redactor: &Redactor,
) -> Observation {
    let safe_text = redactor.redact(&raw.command);
    // For the spike, use simple hash placeholders.
    let safe_hash = simple_hash(safe_text.as_bytes());
    let keyed_fingerprint = simple_hash(raw.command.as_bytes());

    Observation {
        revision_id,
        ordinal,
        kind,
        observed_at_ms: raw.timestamp.map(|t| t * 1000),
        safe_text,
        safe_hash,
        keyed_fingerprint,
        flags: 0,
        provenance: Vec::new(),
        locator: raw.byte_offset.to_le_bytes().to_vec(),
    }
}

/// Placeholder hash for the spike (FNV-1a, 8 bytes). Will be replaced with BLAKE3.
fn simple_hash(data: &[u8]) -> Vec<u8> {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash.to_le_bytes().to_vec()
}

#[derive(Debug)]
pub enum ImportError {
    Io(std::io::Error),
    Store(rusqlite::Error),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO error: {e}"),
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

        assert_eq!(result.records_parsed, 3);
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

        assert_eq!(result.records_parsed, 2);
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

        // Append to file (different length).
        {
            let mut f = fs::OpenOptions::new().append(true).open(&history_path).unwrap();
            writeln!(f, ": 1700000001:0;cargo test").unwrap();
        }

        // File is now longer — should reimport.
        let r2 = import_file(&mut store, &history_path, SourceKind::Zsh, &redactor).unwrap();
        assert!(!r2.skipped);
        assert_eq!(r2.records_stored, 2);
    }
}
