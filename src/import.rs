use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufReader, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::domain::{FLAG_LOSSY, Observation, ObservationKind, Provenance, SourceKind};
use crate::key::{self, InstallationKey};
use crate::redact::{REDACTION_REVISION, Redactor};
use crate::source::{self, RawRecord};
use crate::store::{Cursor, RevisionTx, StatHint, Store, StoreError};

/// How an import related the file to what the store already held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Outcome {
    /// The stat hint matched the stored one: the file was not read.
    #[default]
    Unchanged,
    /// The source's first revision.
    First,
    /// The boundary guard vouched for the stored prefix: only the bytes past
    /// it were parsed.
    Appended,
    /// The file was rewritten and the stored records' end found in it: the
    /// records after that were added to the existing revision.
    Aligned,
    /// The file was rewritten and the stored records' end could not be placed
    /// uniquely: the whole file is a new revision and the old one is kept.
    Superseded,
}

/// Result of an import operation.
#[derive(Debug)]
pub struct ImportResult {
    /// The shell the file was read as: asked for, else stored, else detected.
    pub kind: SourceKind,
    /// The source was stored as this other shell: that many records, parsed
    /// by the wrong grammar, were deleted before this import.
    pub replaced: Option<(SourceKind, usize)>,
    /// Records, in any source, that were stored under older redaction rules
    /// and were re-redacted before this import looked at the file.
    pub rescrubbed: usize,
    /// A reader kept the WAL from being truncated after a rescrub or forget,
    /// so text removed from the store is still on disk; the next import or
    /// forget tries again.
    pub scrub_blocked: bool,
    pub records_stored: usize,
    /// Records the parser yielded, over every pass this import made.
    pub records_parsed: usize,
    pub revision_id: i64,
    pub outcome: Outcome,
    /// The installation key was replaced or could not be written.
    pub key_warning: Option<String>,
    /// The records are committed, but a reader's open snapshot kept the WAL
    /// from being folded into the database file.
    pub checkpoint_blocked: bool,
    /// Stored records flagged `FLAG_LOSSY`.
    pub lossy_records: usize,
    /// zsh Meta bytes with nothing to decode, in stored and withheld records.
    pub dangling_meta: u32,
    /// The file ended in a partial record, which was not stored.
    pub incomplete_tail: bool,
    /// Claude Code lines that were not the JSON the parser knows, or hook
    /// frames that were not what `relic hook` writes. Skipped.
    pub unreadable: usize,
    /// The Claude Code version parsing stopped at, unread with everything
    /// after it until a Relic that knows it imports the file.
    pub unsupported: Option<String>,
}

impl ImportResult {
    fn unchanged(kind: SourceKind) -> Self {
        Self {
            kind,
            replaced: None,
            rescrubbed: 0,
            scrub_blocked: false,
            records_stored: 0,
            records_parsed: 0,
            revision_id: 0,
            outcome: Outcome::Unchanged,
            key_warning: None,
            checkpoint_blocked: false,
            lossy_records: 0,
            dangling_meta: 0,
            incomplete_tail: false,
            unreadable: 0,
            unsupported: None,
        }
    }
}

/// A single-transaction import leaves the whole import in the WAL, and
/// SQLite's own autocheckpoint is passive: it never shrinks the file, which
/// ADR 0008 counts in the size gate. So fold it in explicitly, but only for a
/// bulk import: 10k records is a few MiB of WAL, about the 1000-page mark
/// where autocheckpoint starts anyway, while the 100-record append has a
/// 25 ms budget that cannot pay for a truncating checkpoint's fsyncs.
const CHECKPOINT_MIN_RECORDS: usize = 10_000;

/// The boundary guard covers this many bytes before `stable_len`. A shell
/// rewrite shifts or drops bytes, so it changes this window; an edit that
/// keeps every byte in it still gets past (ADR 0002 accepts that for the
/// 25 ms append gate, which a whole-prefix digest misses at 55 MB).
const GUARD_WINDOW: u64 = 65536;

/// How many of a revision's last records anchor a rewritten file.
const ANCHOR_RECORDS: usize = 16;

/// How many consecutive records of the new file must fall inside the anchor.
/// Half the anchor, so the match survives the shell dropping some anchor
/// records (zsh's HIST_IGNORE_ALL_DUPS) while one repeated command cannot
/// place the boundary on its own.
const ALIGN_WINDOW: usize = 8;

/// A history file opened for import, before any store exists: everything
/// that can be wrong with the file alone is refused here, so a failed first
/// import need not create a database.
pub struct History {
    path: PathBuf,
    resolved: PathBuf,
    file: fs::File,
    stat: StatHint,
}

impl History {
    pub fn open(path: &Path) -> Result<Self, ImportError> {
        // An OS error alone does not say which file it was about.
        let named = |e: ImportError| match e {
            ImportError::Io(e) => {
                ImportError::Io(io::Error::new(e.kind(), format!("{}: {e}", path.display())))
            }
            e => e,
        };
        let resolved = resolve(path).map_err(named)?;
        let file = open_no_follow(&resolved).map_err(named)?;
        // fstat on the open file: the hint describes the bytes this import
        // reads, even when the name is renamed over meanwhile.
        let metadata = file.metadata().map_err(ImportError::Io)?;
        // Before reading: a device never ends.
        if !metadata.is_file() {
            return Err(ImportError::NotRegularFile(path.to_path_buf()));
        }
        Ok(Self {
            path: path.to_path_buf(),
            resolved,
            file,
            stat: stat_hint(&metadata),
        })
    }

    /// The source the file gives away: a `.jsonl` file of JSON records is a
    /// Claude Code session; a history by its first line (zsh extended
    /// history, or a bash `#<epoch>` marker), else by its name. Only the
    /// first line, because a `#123` comment or a pasted `: 1:0;x` further
    /// down an untimed history says nothing about who wrote the file.
    pub fn detected(&self) -> io::Result<Option<SourceKind>> {
        let first = first_line(&self.file)?;
        let name = self
            .resolved
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        Ok(if source::hook::looks_like(&first) {
            Some(SourceKind::Hook)
        } else if name.ends_with(".jsonl") && source::claude::looks_like(&first) {
            Some(SourceKind::Claude)
        } else if source::zsh::is_extended(&first) {
            Some(SourceKind::Zsh)
        } else if source::bash::is_marker(&first) {
            Some(SourceKind::Bash)
        } else if name.contains("zsh") || name.contains("zhistory") {
            Some(SourceKind::Zsh)
        } else if name.contains("bash") {
            Some(SourceKind::Bash)
        } else {
            None
        })
    }
}

/// The start of `file` up to its first line ending, or as much of it as
/// either shell's timestamp prefix can occupy.
fn first_line(file: &fs::File) -> io::Result<Vec<u8>> {
    let mut head = vec![0; 64];
    let read = file.read_at(&mut head, 0)?;
    head.truncate(read);
    let end = head
        .iter()
        .position(|b| matches!(b, b'\n' | b'\r'))
        .unwrap_or(head.len());
    head.truncate(end);
    Ok(head)
}

/// Import a history file. Unchanged since the last import: nothing is read.
/// Grown: only the new tail is parsed. Rewritten: the records after the
/// stored ones are added, or the whole file becomes a new revision when the
/// stored ones cannot be found. Nothing already stored is ever removed,
/// except by a `shell` that differs from the one the source was stored as.
pub fn import_file(
    store: &mut Store,
    path: &Path,
    shell: impl Into<Option<SourceKind>>,
    redactor: &Redactor,
) -> Result<ImportResult, ImportError> {
    import(store, &History::open(path)?, shell.into(), redactor)
}

pub fn import(
    store: &mut Store,
    history: &History,
    shell: Option<SourceKind>,
    redactor: &Redactor,
) -> Result<ImportResult, ImportError> {
    let History { file, stat, .. } = history;
    let stat = *stat;
    // First, and for every source: alignment compares stored safe text with
    // what the current rules make of the file. A rescrub brings the stored
    // side as close as it can, working from safe text: where older rules
    // redacted something the current ones keep, the two still differ and
    // that source's next rewrite supersedes instead of aligning.
    let (rescrubbed, scrubbed) = store.rescrub(REDACTION_REVISION, |safe| redactor.redact(safe))?;
    let key_path = store.key_path().map(Path::to_path_buf);
    // Lock before looking: an importer that decided first and then waited for
    // another import of this file would store the file a second time.
    let mut source = store.lock_source(history.resolved.as_os_str().as_bytes())?;
    // Only through `import_git`; a path once a repository is not a history.
    if shell == Some(SourceKind::Git) || source.stored_kind() == Some(SourceKind::Git) {
        return Err(ImportError::GitAsHistory);
    }
    // Asked beats stored beats detected: a stored kind was either asked for
    // once or detected once, and detection must not flip it later.
    let (kind, replaced) = match (shell, source.stored_kind()) {
        (Some(asked), Some(stored)) if asked != stored => {
            (asked, Some((stored, source.replace_kind(asked)?)))
        }
        (Some(kind), _) | (None, Some(kind)) => (kind, None),
        (None, None) => match history.detected().map_err(ImportError::Io)? {
            Some(kind) => (kind, None),
            None => return Err(ImportError::UnknownShell(history.path.clone())),
        },
    };
    let newest = source.newest().cloned();
    if newest.as_ref().is_some_and(|r| r.cursor.stat == stat) {
        return Ok(ImportResult {
            replaced,
            rescrubbed,
            scrub_blocked: !scrubbed,
            ..ImportResult::unchanged(kind)
        });
    }

    // Under the write lock, so two first imports cannot each write a key.
    let (key, key_warning) = match key_path
        .as_deref()
        .map(|path| (path, key::load_or_create(path)))
    {
        None => (None, None),
        Some((path, Ok((key, created)))) => {
            let lost = created
                && newest
                    .as_ref()
                    .is_some_and(|r| r.cursor.boundary_guard.is_some());
            let warning = lost.then(|| {
                format!("installation key {} was missing or unreadable; wrote a new one and aligned this import by content", path.display())
            });
            (Some(key), warning)
        }
        Some((path, Err(e))) => (
            None,
            Some(format!(
                "installation key {}: {e}; every import aligns by content until it can be written",
                path.display()
            )),
        ),
    };

    let mut aligned = 0;
    let (mut revision, from, outcome) = match &newest {
        None => (source.new_revision(kind)?, 0, Outcome::First),
        Some(newest) if prefix_intact(file, key.as_ref(), &newest.cursor, stat.size)? => (
            source.append(kind)?,
            newest.cursor.stable_len as u64,
            Outcome::Appended,
        ),
        Some(_) => {
            let anchor: Vec<RecordKey> = source
                .anchor(ANCHOR_RECORDS)?
                .iter()
                .map(|(observed_at_ms, safe_text)| record_key(*observed_at_ms, safe_text))
                .collect();
            let (boundary, records) = align(file, kind, redactor, &anchor)?;
            aligned = records;
            match boundary {
                Some(from) => (source.append(kind)?, from, Outcome::Aligned),
                None => (source.new_revision(kind)?, 0, Outcome::Superseded),
            }
        }
    };

    let (stored, scan) = scan(file, from, kind, |records| {
        store_records(&mut revision, records, redactor)
    })?;
    // The hint was taken before reading. If the file changed under the read,
    // the offsets this import decided on may have pointed into other bytes
    // by the time it parsed from them. Dropping the revision stores nothing.
    if stat_hint(&file.metadata().map_err(ImportError::Io)?) != stat {
        return Err(ImportError::ChangedDuringImport(history.path.clone()));
    }
    let boundary_guard = match &key {
        Some(key) => Some(boundary_guard(file, key, scan.stable_len).map_err(ImportError::Io)?),
        None => None,
    };
    let cursor = Cursor {
        stat,
        stable_len: scan.stable_len as i64,
        boundary_guard,
    };
    let revision_id = revision.revision_id();
    revision.commit(&cursor, stored.provenance)?;
    Ok(ImportResult {
        kind,
        replaced,
        rescrubbed,
        scrub_blocked: !scrubbed,
        records_stored: stored.count,
        records_parsed: aligned + stored.count,
        revision_id,
        outcome,
        key_warning,
        checkpoint_blocked: checkpoint_blocked(store, stored.count)?,
        lossy_records: stored.lossy,
        dangling_meta: scan.dangling_meta,
        incomplete_tail: scan.incomplete_tail,
        unreadable: scan.unreadable,
        unsupported: scan.unsupported,
    })
}

/// What a Git import stored.
#[derive(Debug, PartialEq, Eq)]
pub struct GitImportResult {
    /// `First`, `Appended`, or `Unchanged` when Git held nothing new.
    pub outcome: Outcome,
    pub reflog_stored: usize,
    pub refs_stored: usize,
    pub lossy_records: usize,
    pub rescrubbed: usize,
    pub scrub_blocked: bool,
    pub checkpoint_blocked: bool,
}

/// Import a repository's reflog entries and refs, read before the store is
/// opened, storing only what the store does not hold yet. Nothing is ever
/// removed: a branch deleted or a reflog expired in Git stays evidence
/// here, which is what recovering it needs.
pub fn import_git(
    store: &mut Store,
    repo: source::git::Repository,
    redactor: &Redactor,
) -> Result<GitImportResult, ImportError> {
    let source::git::Repository {
        git_dir,
        records: raw,
    } = repo;
    let (rescrubbed, scrubbed) = store.rescrub(REDACTION_REVISION, |safe| redactor.redact(safe))?;
    let source = store.lock_source(git_dir.as_os_str().as_bytes())?;
    if let Some(kind) = source.stored_kind().filter(|k| *k != SourceKind::Git) {
        return Err(ImportError::Io(io::Error::other(format!(
            "{} was imported as a {kind} history, not a Git repository",
            git_dir.display()
        ))));
    }
    // A ref's time is when it was seen, so it matches by text alone. Counted,
    // not a set: one second can hold the same reflog entry twice.
    let key = |kind: ObservationKind, at: Option<i64>, text: String| {
        (
            kind,
            at.filter(|_| kind == ObservationKind::GitReflog),
            text,
        )
    };
    let mut known: std::collections::HashMap<_, usize> = std::collections::HashMap::new();
    for (kind, at, text) in source.records()? {
        *known.entry(key(kind, at, text)).or_default() += 1;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let mut fresh = Vec::new();
    for r in raw {
        let kind = match r.kind {
            source::git::RecordKind::Reflog => ObservationKind::GitReflog,
            source::git::RecordKind::Ref => ObservationKind::GitRef,
        };
        let observed_at_ms = match r.kind {
            source::git::RecordKind::Reflog => r.timestamp.and_then(|t| t.checked_mul(1000)),
            source::git::RecordKind::Ref => Some(now_ms),
        };
        let safe_text = redactor.import(&r.text);
        if let Some(n) = known
            .get_mut(&key(kind, observed_at_ms, safe_text.clone()))
            .filter(|n| **n > 0)
        {
            *n -= 1;
            continue;
        }
        let flags = if r.lossy { FLAG_LOSSY } else { 0 };
        fresh.push(Observation {
            kind,
            observed_at_ms,
            safe_text,
            flags,
            locator: Vec::new(),
        });
    }
    let count = |kind| fresh.iter().filter(|o| o.kind == kind).count();
    let mut result = GitImportResult {
        outcome: Outcome::Unchanged,
        reflog_stored: count(ObservationKind::GitReflog),
        refs_stored: count(ObservationKind::GitRef),
        lossy_records: fresh.iter().filter(|o| o.flags & FLAG_LOSSY != 0).count(),
        rescrubbed,
        scrub_blocked: !scrubbed,
        checkpoint_blocked: false,
    };
    if fresh.is_empty() {
        return Ok(result);
    }
    result.outcome = if source.newest().is_some() {
        Outcome::Appended
    } else {
        Outcome::First
    };
    let mut revision = source.append(SourceKind::Git)?;
    for o in &fresh {
        revision.insert(o)?;
    }
    // No cursor: Git is asked again on every import, never stat-skipped.
    revision.commit(
        &Cursor::default(),
        Provenance::COMMAND.with(Provenance::OBSERVED_AT),
    )?;
    result.checkpoint_blocked = checkpoint_blocked(store, fresh.len())?;
    Ok(result)
}

/// What a pass over a file's records learned besides the records.
struct Scan {
    /// Byte end of the last complete record, from the start of the file.
    stable_len: u64,
    dangling_meta: u32,
    incomplete_tail: bool,
    unreadable: usize,
    unsupported: Option<String>,
}

/// Parse `file` from byte `from`, which must be where a record starts, and
/// hand `run` the records with their offsets counted from the start of the
/// file.
fn scan<T>(
    mut file: &fs::File,
    from: u64,
    kind: SourceKind,
    run: impl FnOnce(&mut dyn Iterator<Item = io::Result<RawRecord>>) -> Result<T, ImportError>,
) -> Result<(T, Scan), ImportError> {
    // A Claude Code result names only the call it answers, and a bash hook
    // frame is new only against the frame before it, either of which may
    // sit before `from`: their parsers read from the start and skip to it.
    let read_from = if matches!(kind, SourceKind::Claude | SourceKind::Hook) {
        0
    } else {
        from
    };
    file.seek(SeekFrom::Start(read_from))
        .map_err(ImportError::Io)?;
    let reader = BufReader::new(file);
    let absolute = |raw: io::Result<RawRecord>| {
        raw.map(|mut raw| {
            raw.start += read_from;
            raw.end += read_from;
            raw
        })
    };
    let (mut unreadable, mut unsupported) = (0, None);
    let (out, stable_len, dangling_meta, incomplete_tail) = match kind {
        SourceKind::Zsh => {
            let mut parser = source::zsh::Parser::new(reader);
            let out = run(&mut parser.by_ref().map(absolute))?;
            (
                out,
                parser.stable_len(),
                parser.dangling_meta(),
                parser.incomplete_tail().is_some(),
            )
        }
        SourceKind::Bash => {
            let multiline = source::bash::is_marker(&first_line(file).map_err(ImportError::Io)?);
            let mut parser = source::bash::Parser::new(reader, multiline);
            let out = run(&mut parser.by_ref().map(absolute))?;
            (
                out,
                parser.stable_len(),
                0,
                parser.incomplete_tail().is_some(),
            )
        }
        SourceKind::Claude => {
            let mut parser = source::claude::Parser::resume(reader, from);
            let out = run(&mut parser.by_ref().map(absolute))?;
            (unreadable, unsupported) = (
                parser.unreadable(),
                parser.unsupported().map(str::to_string),
            );
            (out, parser.stable_len(), 0, parser.incomplete_tail())
        }
        SourceKind::Hook => {
            let mut parser = source::hook::Parser::resume(reader, from);
            let out = run(&mut parser.by_ref().map(absolute))?;
            unreadable = parser.unreadable();
            (out, parser.stable_len(), 0, parser.incomplete_tail())
        }
        SourceKind::Git => return Err(ImportError::GitAsHistory),
    };
    Ok((
        out,
        Scan {
            stable_len: read_from + stable_len,
            dangling_meta,
            incomplete_tail,
            unreadable,
            unsupported,
        },
    ))
}

/// Keyed hash of the bytes just before `stable_len`.
fn boundary_guard(file: &fs::File, key: &InstallationKey, stable_len: u64) -> io::Result<[u8; 32]> {
    let window = stable_len.min(GUARD_WINDOW);
    let mut bytes = vec![0; window as usize];
    file.read_exact_at(&mut bytes, stable_len - window)?;
    Ok(*blake3::keyed_hash(key, &bytes).as_bytes())
}

/// Whether the file still holds, where the cursor left them, the bytes the
/// stored guard was taken over. Without a key or a stored guard nothing is
/// proven.
fn prefix_intact(
    file: &fs::File,
    key: Option<&InstallationKey>,
    cursor: &Cursor,
    size: i64,
) -> Result<bool, ImportError> {
    match (key, cursor.boundary_guard) {
        (Some(key), Some(guard)) if (0..=size).contains(&cursor.stable_len) => {
            Ok(
                boundary_guard(file, key, cursor.stable_len as u64).map_err(ImportError::Io)?
                    == guard,
            )
        }
        _ => Ok(false),
    }
}

/// What two records must share to be the same record across a rewrite.
/// Unkeyed and never stored: both sides are recomputed from safe text.
type RecordKey = [u8; 32];

fn record_key(observed_at_ms: Option<i64>, safe_text: &str) -> RecordKey {
    let mut hasher = blake3::Hasher::new();
    // Tagged, so an untimed record cannot collide with a timed one whose
    // time bytes happen to start its text.
    match observed_at_ms {
        Some(ms) => hasher.update(&[1]).update(&ms.to_le_bytes()),
        None => hasher.update(&[0]),
    };
    *hasher.update(safe_text.as_bytes()).finalize().as_bytes()
}

/// Find where the stored records end in a rewritten file: the byte offset
/// just past the last record the store already holds, or `None` when that
/// cannot be placed uniquely. Also the number of records read.
///
/// A record is a candidate for "last stored" when it and the records before
/// it, `ALIGN_WINDOW` in all, appear in that order in the anchor, with gaps
/// allowed for records the shell dropped. Around the true boundary the
/// candidates are one unbroken run ending at it; a second run means the same
/// commands repeat elsewhere and the boundary is a guess.
fn align(
    file: &fs::File,
    kind: SourceKind,
    redactor: &Redactor,
    anchor: &[RecordKey],
) -> Result<(Option<u64>, usize), ImportError> {
    // The revision holds no records, so every record in the file is new.
    if anchor.is_empty() {
        return Ok((Some(0), 0));
    }
    let width = anchor.len().min(ALIGN_WINDOW);
    let mut window = VecDeque::with_capacity(width);
    let (mut records, mut runs, mut in_run, mut boundary) = (0, 0usize, false, 0);
    scan(file, 0, kind, |parsed| {
        for raw in parsed {
            let raw = raw.map_err(ImportError::Io)?;
            // Redacted first: the anchor's keys were taken over safe text.
            let observation = raw_to_observation(&raw, redactor);
            if window.len() == width {
                window.pop_front();
            }
            window.push_back(record_key(
                observation.observed_at_ms,
                &observation.safe_text,
            ));
            let mut later = anchor.iter();
            let candidate =
                window.len() == width && window.iter().all(|key| later.any(|a| a == key));
            if candidate {
                runs += usize::from(!in_run);
                boundary = raw.end;
            }
            in_run = candidate;
            records += 1;
        }
        Ok(())
    })?;
    Ok(((runs == 1).then_some(boundary), records))
}

/// The path a source is known by: its directory with every symlink and `..`
/// resolved, and the file name as given. The name is not resolved, because
/// zsh's save-by-copy turns a symlinked history into a regular file and a
/// followed link would then change identity (ADR 0002).
pub fn resolve(path: &Path) -> Result<PathBuf, ImportError> {
    // No name to keep: `/`, or a path ending in `..`.
    let name = path
        .file_name()
        .ok_or_else(|| ImportError::NotRegularFile(path.to_path_buf()))?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    Ok(fs::canonicalize(parent)
        .map_err(ImportError::Io)?
        .join(name))
}

fn open_no_follow(resolved: &Path) -> Result<fs::File, ImportError> {
    fs::OpenOptions::new()
        .read(true)
        // Non-blocking, or opening a FIFO nobody writes to never returns.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(resolved)
        .map_err(|e| match fs::read_link(resolved) {
            Ok(target) => ImportError::FinalSymlink {
                path: resolved.to_path_buf(),
                target,
            },
            Err(_) => ImportError::Io(e),
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
    records: impl Iterator<Item = io::Result<RawRecord>>,
    redactor: &Redactor,
) -> Result<Stored, ImportError> {
    let mut stored = Stored {
        count: 0,
        lossy: 0,
        provenance: Provenance::COMMAND,
    };
    for raw in records {
        let observation = raw_to_observation(&raw.map_err(ImportError::Io)?, redactor);
        if observation.observed_at_ms.is_some() {
            stored.provenance = stored.provenance.with(Provenance::OBSERVED_AT);
        }
        revision.insert(&observation)?;
        stored.count += 1;
        stored.lossy += usize::from(observation.flags & FLAG_LOSSY != 0);
    }
    Ok(stored)
}

/// Checkpoint after a bulk commit; true when a reader blocked it. The
/// revision is already durable here, so a checkpoint error is reported as a
/// store error, never as `Busy`: "try again" would be wrong advice for an
/// import that succeeded.
fn checkpoint_blocked(store: &Store, stored: usize) -> Result<bool, ImportError> {
    Ok(stored >= CHECKPOINT_MIN_RECORDS
        && !store
            .checkpoint()
            .map_err(|e| ImportError::Store(StoreError::Sqlite(e)))?)
}

fn raw_to_observation(raw: &RawRecord, redactor: &Redactor) -> Observation {
    Observation {
        kind: raw.kind,
        // A time whose milliseconds overflow is absent, not wrapped.
        observed_at_ms: raw.timestamp.and_then(|t| t.checked_mul(1000)),
        safe_text: redactor.import(&raw.command),
        flags: if raw.lossy { FLAG_LOSSY } else { 0 },
        locator: raw.start.to_le_bytes().to_vec(),
    }
}

#[derive(Debug)]
pub enum ImportError {
    Io(io::Error),
    /// A directory, device, FIFO or socket: refused before any read.
    NotRegularFile(PathBuf),
    /// The path's last component is a symlink, which is never followed.
    FinalSymlink {
        path: PathBuf,
        target: PathBuf,
    },
    /// Neither `--shell`, an earlier import, the content nor the name says
    /// which shell wrote the file.
    UnknownShell(PathBuf),
    /// The file was written to while it was being read; nothing was stored.
    ChangedDuringImport(PathBuf),
    /// Another import holds the store's write lock.
    Busy,
    /// A Git source asked to be read as a history file.
    GitAsHistory,
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
            Self::Io(e) => write!(f, "{e}"),
            Self::NotRegularFile(path) => write!(f, "{}: not a regular file", path.display()),
            Self::GitAsHistory => f.write_str(
                "a Git repository is imported by its directory, not read as a history file",
            ),
            Self::FinalSymlink { path, target } => write!(
                f,
                "{}: is a symlink to {}; relic does not follow it, import the target instead",
                path.display(),
                target.display()
            ),
            Self::UnknownShell(path) => write!(
                f,
                "{}: cannot tell whether this is a zsh or a bash history; pass --shell zsh or --shell bash",
                path.display()
            ),
            Self::ChangedDuringImport(path) => {
                write!(
                    f,
                    "{}: changed while it was being imported; nothing was stored, try again",
                    path.display()
                )
            }
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
            write!(f, "#1700000000\ncargo build\n#1700000005\ngit status\n").unwrap();
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
        assert!(hits[0].safe_text.contains("API_KEY=[REDACTED:key]"));
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
        assert_eq!((r1.outcome, r1.records_stored), (Outcome::First, 2));

        // Second import of the same unchanged file — should skip.
        let r2 = import_file(&mut store, &history_path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!(
            (r2.outcome, r2.records_stored, r2.records_parsed),
            (Outcome::Unchanged, 0, 0)
        );

        // Observations should still be 2, not doubled.
        assert_eq!(store.observation_count().unwrap(), 2);
    }

    #[test]
    fn without_a_key_an_append_is_aligned_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let history_path = dir.path().join("zsh_history");
        fs::write(&history_path, zsh_history(0..1)).unwrap();
        // In memory: no key file, so no guard is stored or checked.
        let mut store = Store::open_memory().unwrap();
        let redactor = Redactor::new();

        let r1 = import_file(&mut store, &history_path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!((r1.outcome, r1.records_stored), (Outcome::First, 1));

        append(&history_path, &zsh_history(1..2));
        let r2 = import_file(&mut store, &history_path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!(
            (r2.outcome, r2.records_stored, r2.records_parsed),
            (Outcome::Aligned, 1, 3)
        );
        assert_eq!(r2.revision_id, r1.revision_id);
        assert_eq!(texts(&store), ["echo needle 0", "echo needle 1"]);
    }

    /// Extended-format zsh history: one `echo needle <i>` per `i`.
    fn zsh_history(range: std::ops::Range<usize>) -> String {
        range
            .map(|i| format!(": {}:0;echo needle {i}\n", 1_700_000_000 + i))
            .collect()
    }

    fn append(path: &Path, content: &str) {
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(content.as_bytes())
            .unwrap();
    }

    /// Replace a history the way zsh saves one: a new file renamed over it.
    fn rewrite(path: &Path, content: &str) {
        let tmp = path.with_extension("new");
        fs::write(&tmp, content).unwrap();
        fs::rename(&tmp, path).unwrap();
    }

    fn texts(store: &Store) -> Vec<String> {
        stored(store).into_iter().map(|o| o.safe_text).collect()
    }

    fn needles(range: std::ops::Range<usize>) -> Vec<String> {
        range.map(|i| format!("echo needle {i}")).collect()
    }

    #[test]
    fn an_append_parses_only_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let path = dir.path().join("zsh_history");
        // Longer than the guard window, so the guard is taken mid-file.
        fs::write(&path, zsh_history(0..3000)).unwrap();
        let old_len = fs::metadata(&path).unwrap().len();
        assert!(old_len > GUARD_WINDOW);
        let mut store = Store::open(&db).unwrap();
        let redactor = Redactor::new();

        let first = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!(
            (first.outcome, first.records_parsed),
            (Outcome::First, 3000)
        );

        append(&path, &zsh_history(3000..3100));
        let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!(
            (r.outcome, r.records_parsed, r.records_stored),
            (Outcome::Appended, 100, 100)
        );
        assert_eq!(r.revision_id, first.revision_id);
        assert_eq!(visible(&db), [1, 1, 3100, 3100, 3100]);
        let appended = store.get_observation(3001).unwrap().unwrap();
        assert_eq!(
            (appended.safe_text.as_str(), appended.ordinal),
            ("echo needle 3000", 3000)
        );
        // Offsets count from the start of the file, not of the tail.
        assert_eq!(appended.locator, old_len.to_le_bytes());
        assert_eq!(
            stable_len(&mut store, &path),
            fs::metadata(&path).unwrap().len() as i64
        );

        // The cursor an append leaves is as good as a first import's.
        append(&path, &zsh_history(3100..3101));
        let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!(
            (r.outcome, r.records_parsed, r.records_stored),
            (Outcome::Appended, 1, 1)
        );
        assert_eq!(visible(&db), [1, 1, 3101, 3101, 3101]);
    }

    #[test]
    fn a_bash_append_resumes_after_the_last_timed_record() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let path = dir.path().join("bash_history");
        fs::write(&path, "#1700000000\necho one\n").unwrap();
        let mut store = Store::open(&db).unwrap();
        let redactor = Redactor::new();
        import_file(&mut store, &path, SourceKind::Bash, &redactor).unwrap();

        append(&path, "#1700000001\necho two\nmore\n");
        let r = import_file(&mut store, &path, SourceKind::Bash, &redactor).unwrap();
        assert_eq!((r.outcome, r.records_parsed), (Outcome::Appended, 1));
        let rows = stored(&store);
        assert_eq!(
            (rows[1].safe_text.as_str(), rows[1].observed_at_ms),
            ("echo two\nmore", Some(1_700_000_001_000))
        );
        assert_eq!(rows[1].locator, 21u64.to_le_bytes());
    }

    #[test]
    fn a_record_completed_after_an_import_is_stored_by_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let path = dir.path().join("zsh_history");
        fs::write(&path, "echo done\necho parti").unwrap();
        let mut store = Store::open(&db).unwrap();
        let redactor = Redactor::new();
        let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!((r.records_stored, r.incomplete_tail), (1, true));

        append(&path, "al\necho next\n");
        let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!(
            (r.outcome, r.records_stored, r.incomplete_tail),
            (Outcome::Appended, 2, false)
        );
        assert_eq!(texts(&store), ["echo done", "echo partial", "echo next"]);
    }

    #[test]
    fn a_source_first_seen_empty_fills_its_first_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zsh_history");
        let redactor = Redactor::new();
        for (mut store, outcome) in [
            (
                Store::open(&dir.path().join("relic.db")).unwrap(),
                Outcome::Appended,
            ),
            (Store::open_memory().unwrap(), Outcome::Aligned),
        ] {
            fs::write(&path, "").unwrap();
            let empty = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
            assert_eq!((empty.outcome, empty.records_stored), (Outcome::First, 0));

            append(&path, &zsh_history(0..3));
            let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
            assert_eq!((r.outcome, r.records_stored), (outcome, 3));
            assert_eq!(r.revision_id, empty.revision_id);
        }
    }

    #[test]
    fn a_same_length_rewrite_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let path = dir.path().join("zsh_history");
        fs::write(&path, "echo needle one\necho needle aaa\necho needle two\n").unwrap();
        let mut store = Store::open(&db).unwrap();
        let redactor = Redactor::new();
        let first = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();

        // In place, as bash writes: same inode, same length.
        fs::write(&path, "echo needle one\necho needle bbb\necho needle two\n").unwrap();
        // Not left to the clock: a coarse one could stamp both writes alike.
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(mtime + std::time::Duration::from_secs(1))
            .unwrap();

        let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        // The edited record breaks the anchor, so nothing can be placed.
        assert_eq!((r.outcome, r.records_stored), (Outcome::Superseded, 3));
        assert_ne!(r.revision_id, first.revision_id);
        assert_eq!(visible(&db), [1, 2, 6, 6, 6]);
        assert_eq!(store.search("bbb", 10).unwrap().len(), 1);
        assert_eq!(store.search("aaa", 10).unwrap().len(), 1);
    }

    #[test]
    fn a_same_length_edit_tens_of_kilobytes_before_the_end_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let path = dir.path().join("zsh_history");
        let content = zsh_history(0..3000);
        fs::write(&path, &content).unwrap();
        let mut store = Store::open(&db).unwrap();
        let redactor = Redactor::new();
        import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();

        let edited = content.replace("echo needle 1500\n", "echo needle 15oo\n");
        let from_end = content.len() - content.find("echo needle 1500\n").unwrap();
        assert!(
            (32_768..GUARD_WINDOW as usize).contains(&from_end),
            "{from_end}"
        );
        fs::write(&path, &edited).unwrap();
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(mtime + std::time::Duration::from_secs(1))
            .unwrap();

        // Not trusted as an append: the whole file was read again.
        let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!(
            (r.outcome, r.records_parsed, r.records_stored),
            (Outcome::Aligned, 3000, 0)
        );
    }

    #[test]
    fn a_head_trim_keeps_old_rows_and_adds_only_the_new_ones() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let path = dir.path().join("zsh_history");
        fs::write(&path, zsh_history(0..30)).unwrap();
        let mut store = Store::open(&db).unwrap();
        let redactor = Redactor::new();
        let first = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();

        rewrite(&path, &zsh_history(10..33));
        let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!(
            (r.outcome, r.records_stored, r.records_parsed),
            (Outcome::Aligned, 3, 23 + 3)
        );
        assert_eq!(r.revision_id, first.revision_id);
        assert_eq!(visible(&db), [1, 1, 33, 33, 33]);
        assert_eq!(texts(&store), needles(0..33));
        let rows = stored(&store);
        assert_eq!(
            rows.iter().map(|o| o.ordinal).collect::<Vec<_>>(),
            (0..33).collect::<Vec<_>>()
        );

        // A trim with nothing new adds nothing, and leaves a usable cursor.
        rewrite(&path, &zsh_history(20..33));
        let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!((r.outcome, r.records_stored), (Outcome::Aligned, 0));
        append(&path, &zsh_history(33..34));
        let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!(
            (r.outcome, r.records_stored, r.records_parsed),
            (Outcome::Appended, 1, 1)
        );
        assert_eq!(texts(&store), needles(0..34));
        assert_eq!(visible(&db)[..2], [1, 1]);
    }

    #[test]
    fn an_interior_removal_keeps_old_rows_and_adds_only_the_new_ones() {
        let dir = tempfile::tempdir().unwrap();
        let redactor = Redactor::new();
        // HIST_IGNORE_ALL_DUPS: rerunning command 25 drops its old line and
        // appends a new one, with a new time when the history is timed.
        let plain: String = (0..30).map(|i| format!("echo needle {i}\n")).collect();
        for (name, before, after, rerun_at) in [
            (
                "timed",
                zsh_history(0..30),
                zsh_history(0..25) + &zsh_history(26..30) + ": 1800000000:0;echo needle 25\n",
                Some(1_800_000_000_000),
            ),
            (
                "plain",
                plain.clone(),
                plain.replace("echo needle 25\n", "") + "echo needle 25\n",
                None,
            ),
        ] {
            let db = dir.path().join(format!("{name}.db"));
            let path = dir.path().join(name);
            fs::write(&path, before).unwrap();
            let mut store = Store::open(&db).unwrap();
            let first = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();

            rewrite(&path, &after);
            let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
            assert_eq!(
                (r.outcome, r.records_stored),
                (Outcome::Aligned, 1),
                "{name}"
            );
            assert_eq!(r.revision_id, first.revision_id, "{name}");
            assert_eq!(visible(&db), [1, 1, 31, 31, 31], "{name}");
            let rows = stored(&store);
            assert_eq!(
                rows[..30]
                    .iter()
                    .map(|o| o.safe_text.clone())
                    .collect::<Vec<_>>(),
                needles(0..30),
                "{name}"
            );
            assert_eq!(
                (rows[30].safe_text.as_str(), rows[30].observed_at_ms),
                ("echo needle 25", rerun_at),
                "{name}"
            );
        }
    }

    #[test]
    fn an_ambiguous_anchor_supersedes_and_keeps_the_old_revision() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let path = dir.path().join("zsh_history");
        fs::write(&path, "make needle\nmake test\nmake install\n").unwrap();
        let mut store = Store::open(&db).unwrap();
        let redactor = Redactor::new();
        let first = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();

        // The stored three appear twice: either could be where they ended.
        rewrite(
            &path,
            "cd a\nmake needle\nmake test\nmake install\ncd b\nmake needle\nmake test\nmake install\n",
        );
        let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!((r.outcome, r.records_stored), (Outcome::Superseded, 8));
        assert_ne!(r.revision_id, first.revision_id);
        assert_eq!(visible(&db), [1, 2, 11, 11, 3]);
        let rows = stored(&store);
        assert!(rows[..3].iter().all(|o| o.revision_id == first.revision_id));
        assert!(rows[3..].iter().all(|o| o.revision_id == r.revision_id));

        // The new revision is the one later imports continue.
        append(&path, "cd c\n");
        let next = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!(
            (next.outcome, next.records_stored, next.revision_id),
            (Outcome::Appended, 1, r.revision_id)
        );
    }

    #[test]
    fn removing_a_line_from_the_file_never_removes_it_from_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let path = dir.path().join("zsh_history");
        fs::write(&path, zsh_history(0..12)).unwrap();
        let mut store = Store::open(&db).unwrap();
        let redactor = Redactor::new();
        import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();

        // The last line goes, then one from the middle while a new one arrives.
        rewrite(&path, &zsh_history(0..11));
        let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!((r.outcome, r.records_stored), (Outcome::Aligned, 0));
        assert_eq!(texts(&store), needles(0..12));

        rewrite(
            &path,
            &(zsh_history(0..5) + &zsh_history(6..11) + &zsh_history(12..13)),
        );
        let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!((r.outcome, r.records_stored), (Outcome::Aligned, 1));
        assert_eq!(texts(&store), needles(0..13));
        assert_eq!(visible(&db), [1, 1, 13, 13, 13]);
        for removed in ["5", "11"] {
            assert_eq!(
                store
                    .search(&format!("needle {removed}"), 10)
                    .unwrap()
                    .len(),
                1,
                "{removed}"
            );
        }
    }

    #[test]
    fn a_lost_key_creates_no_duplicate_source_and_the_next_import_aligns() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let redactor = Redactor::new();
        for name in ["deleted", "garbled"] {
            let db = dir.path().join(format!("{name}.db"));
            let key = dir.path().join(format!("{name}.db.key"));
            let path = dir.path().join(name);
            fs::write(&path, zsh_history(0..20)).unwrap();
            let mut store = Store::open(&db).unwrap();
            let first = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
            assert!(first.key_warning.is_none(), "{name}");
            assert_eq!(
                fs::metadata(&key).unwrap().permissions().mode() & 0o777,
                0o600,
                "{name}"
            );
            let old_key = fs::read(&key).unwrap();

            match name {
                "deleted" => fs::remove_file(&key).unwrap(),
                _ => fs::write(&key, b"not a key").unwrap(),
            }
            append(&path, &zsh_history(20..25));
            let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
            // The whole file was read to find the anchor, then the tail again.
            assert_eq!(
                (r.outcome, r.records_stored, r.records_parsed),
                (Outcome::Aligned, 5, 25 + 5),
                "{name}"
            );
            assert!(r.key_warning.is_some(), "{name}");
            assert_eq!(visible(&db), [1, 1, 25, 25, 25], "{name}");
            assert_eq!(texts(&store), needles(0..25), "{name}");
            let new_key = fs::read(&key).unwrap();
            assert_eq!(new_key.len(), 32, "{name}");
            assert_ne!(new_key, old_key, "{name}");
            assert_eq!(
                fs::metadata(&key).unwrap().permissions().mode() & 0o777,
                0o600,
                "{name}"
            );

            // The fresh key guards the next append.
            append(&path, &zsh_history(25..30));
            let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
            assert_eq!(
                (r.outcome, r.records_parsed),
                (Outcome::Appended, 5),
                "{name}"
            );
            assert!(r.key_warning.is_none(), "{name}");
            assert_eq!(visible(&db), [1, 1, 30, 30, 30], "{name}");
        }
    }

    #[test]
    fn a_key_that_cannot_be_written_is_reported_and_imports_still_align() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let path = dir.path().join("zsh_history");
        // A directory where the key file belongs: unreadable and unwritable.
        fs::create_dir_all(dir.path().join("relic.db.key/occupied")).unwrap();
        fs::write(&path, zsh_history(0..5)).unwrap();
        let mut store = Store::open(&db).unwrap();
        let redactor = Redactor::new();

        let first = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!(first.records_stored, 5);
        assert!(first.key_warning.is_some());

        append(&path, &zsh_history(5..7));
        let r = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!((r.outcome, r.records_stored), (Outcome::Aligned, 2));
        assert!(r.key_warning.is_some());
        assert_eq!(visible(&db), [1, 1, 7, 7, 7]);
    }

    /// One new revision of `locator` holding `records`, as `import_file`
    /// stores a file it has already read.
    fn import_records(
        store: &mut Store,
        locator: &[u8],
        records: impl Iterator<Item = io::Result<RawRecord>>,
    ) -> Result<ImportResult, ImportError> {
        let mut revision = store.lock_source(locator)?.new_revision(SourceKind::Zsh)?;
        let stored = store_records(&mut revision, records, &Redactor::new())?;
        revision.commit(&Cursor::default(), stored.provenance)?;
        Ok(ImportResult {
            records_stored: stored.count,
            checkpoint_blocked: checkpoint_blocked(store, stored.count)?,
            ..ImportResult::unchanged(SourceKind::Zsh)
        })
    }

    fn records(n: usize, fail_at: Option<usize>) -> impl Iterator<Item = io::Result<RawRecord>> {
        (0..n).map(move |i| {
            if fail_at == Some(i) {
                return Err(io::Error::other("source went away"));
            }
            Ok(RawRecord {
                kind: ObservationKind::ZshCommand,
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
        let conn =
            rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
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

        let writing = first.lock_source(b"/other").unwrap();
        let started = std::time::Instant::now();
        let err = import_file(&mut second, &history_path, SourceKind::Zsh, &redactor).unwrap_err();
        assert!(matches!(err, ImportError::Busy), "{err}");
        // It waited out busy_timeout rather than failing on first contact.
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(200),
            "{:?}",
            started.elapsed()
        );
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
        let resolved = resolve(&history_path).unwrap();
        let locator = resolved.as_os_str().as_bytes();

        let mut writing = first
            .lock_source(locator)
            .unwrap()
            .new_revision(SourceKind::Zsh)
            .unwrap();
        let raw = RawRecord {
            kind: ObservationKind::ZshCommand,
            timestamp: Some(1_700_000_000),
            duration: None,
            command: "echo needle".into(),
            start: 0,
            end: 27,
            lossy: false,
        };
        writing
            .insert(&raw_to_observation(&raw, &Redactor::new()))
            .unwrap();
        let stat = stat_hint(&fs::metadata(&history_path).unwrap());

        let waiter = std::thread::scope(|s| {
            let waiter = s.spawn(|| {
                import_file(
                    &mut second,
                    &history_path,
                    SourceKind::Zsh,
                    &Redactor::new(),
                )
            });
            // Well inside the 250 ms busy_timeout: the waiter gets the lock, not Busy.
            std::thread::sleep(std::time::Duration::from_millis(100));
            writing
                .commit(
                    &Cursor {
                        stat,
                        ..Cursor::default()
                    },
                    Provenance::COMMAND,
                )
                .unwrap();
            waiter.join().unwrap()
        });

        assert_eq!(waiter.unwrap().outcome, Outcome::Unchanged);
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

        let bulk =
            import_records(&mut store, b"/bulk", records(CHECKPOINT_MIN_RECORDS, None)).unwrap();
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
        let reader =
            rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        // An open read transaction pins the reader's snapshot in the WAL.
        reader.execute_batch("BEGIN").unwrap();
        let seen: i64 = reader
            .query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0))
            .unwrap();
        assert_eq!(seen, 0);

        let r =
            import_records(&mut store, b"/bulk", records(CHECKPOINT_MIN_RECORDS, None)).unwrap();
        assert!(r.checkpoint_blocked);
        assert!(wal_len() > 0);
        assert_eq!(visible(&db)[2], CHECKPOINT_MIN_RECORDS as i64);

        reader.execute_batch("COMMIT").unwrap();
        assert!(store.checkpoint().unwrap());
        assert_eq!(wal_len(), 0);
    }

    fn stable_len(store: &mut Store, path: &Path) -> i64 {
        let resolved = resolve(path).unwrap();
        let lock = store.lock_source(resolved.as_os_str().as_bytes()).unwrap();
        lock.newest().unwrap().cursor.stable_len
    }

    fn stored(store: &Store) -> Vec<StoredObservation> {
        // Ids are never reused, so after a replacement they do not start at 1.
        let mut rows: Vec<_> = store
            .recent(usize::MAX >> 1)
            .unwrap()
            .iter()
            .map(|hit| store.get_observation(hit.id).unwrap().unwrap())
            .collect();
        rows.reverse();
        rows
    }

    #[test]
    fn a_partial_trailing_record_is_not_committed() {
        let dir = tempfile::tempdir().unwrap();
        let redactor = Redactor::new();
        for (name, kind, content, complete_len) in [
            (
                "no_newline",
                SourceKind::Zsh,
                ": 1700000000:0;echo done\n: 1700000001:0;echo parti",
                25,
            ),
            (
                "continuation",
                SourceKind::Zsh,
                ": 1700000000:0;echo done\n: 1700000001:0;echo parti\\\n",
                25,
            ),
            (
                "bash",
                SourceKind::Bash,
                "#1700000000\necho done\n#1700000001\necho parti",
                22,
            ),
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
        fs::write(
            &path,
            b"echo \xE9\x83\xA3\xA8\necho broken\x83\necho tail\x83",
        )
        .unwrap();
        let mut store = Store::open_memory().unwrap();

        let r = import_file(&mut store, &path, SourceKind::Zsh, &Redactor::new()).unwrap();
        assert_eq!(
            (r.records_stored, r.lossy_records, r.dangling_meta),
            (2, 1, 2)
        );
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
        // Undecodable bytes each become a three-byte U+FFFD: still capped.
        content.resize(content.len() + source::RECORD_CAP, 0xFF);
        content.push(b'\n');
        fs::write(&path, &content).unwrap();
        let mut store = Store::open_memory().unwrap();

        let r = import_file(&mut store, &path, SourceKind::Zsh, &Redactor::new()).unwrap();
        assert_eq!((r.records_stored, r.lossy_records), (3, 2));
        let rows = stored(&store);
        assert!(rows[0].safe_text.len() <= source::RECORD_CAP);
        assert_eq!(rows[0].flags, FLAG_LOSSY);
        assert_eq!(
            (rows[1].safe_text.as_str(), rows[1].flags),
            ("echo after", 0)
        );
        assert!(rows[2].safe_text.len() <= source::RECORD_CAP);
        assert_eq!(rows[2].flags, FLAG_LOSSY);
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
        fs::write(
            &path,
            ": 1700000000:0;   \n \t \n: 1700000001:0;echo kept\n",
        )
        .unwrap();
        let mut store = Store::open_memory().unwrap();

        let r = import_file(&mut store, &path, SourceKind::Zsh, &Redactor::new()).unwrap();
        assert_eq!(r.records_stored, 1);
        assert_eq!(stored(&store)[0].safe_text, "echo kept");
    }

    #[test]
    fn a_timestamp_whose_milliseconds_overflow_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        // i64::MAX seconds parses, but times 1000 does not fit.
        for (name, kind, content) in [
            (
                "zsh",
                SourceKind::Zsh,
                format!(": {}:0;echo far\n: 1700000000:0;echo near\n", i64::MAX),
            ),
            (
                "bash",
                SourceKind::Bash,
                format!("#{}\necho far\n#1700000000\necho near\n", i64::MAX),
            ),
        ] {
            let path = dir.path().join(name);
            fs::write(&path, content).unwrap();
            let mut store = Store::open_memory().unwrap();

            import_file(&mut store, &path, kind, &Redactor::new()).unwrap();
            let rows = stored(&store);
            assert_eq!(
                (rows[0].safe_text.as_str(), rows[0].observed_at_ms),
                ("echo far", None),
                "{name}"
            );
            assert_eq!(rows[1].observed_at_ms, Some(1_700_000_000_000), "{name}");
        }
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
        assert_eq!(
            (rows[12].safe_text.as_str(), rows[12].observed_at_ms),
            ("echo timed", Some(1_700_000_000_000))
        );
    }

    #[test]
    fn non_regular_files_are_refused_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        for path in [fifo, PathBuf::from("/dev/null"), dir.path().to_path_buf()] {
            // On a thread: a FIFO nobody writes to blocks in open forever, and
            // a wrong check must fail this test, not hang the suite.
            let (done, refused) = std::sync::mpsc::channel();
            let import_path = path.clone();
            std::thread::spawn(move || {
                let mut store = Store::open_memory().unwrap();
                let r = import_file(&mut store, &import_path, SourceKind::Zsh, &Redactor::new());
                let refused =
                    matches!(&r, Err(ImportError::NotRegularFile(p)) if *p == import_path);
                let _ = done.send((refused, store.observation_count().unwrap()));
            });
            let outcome = refused.recv_timeout(std::time::Duration::from_secs(10));
            assert_eq!(outcome, Ok((true, 0)), "{}", path.display());
        }
    }

    #[test]
    fn a_final_symlink_is_refused_naming_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real_history");
        let link = dir.path().join("zsh_history");
        fs::write(&target, "echo needle\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut store = Store::open_memory().unwrap();

        let err = import_file(&mut store, &link, SourceKind::Zsh, &Redactor::new()).unwrap_err();
        assert!(
            matches!(&err, ImportError::FinalSymlink { target: t, .. } if *t == target),
            "{err}"
        );
        assert!(err.to_string().contains(target.to_str().unwrap()), "{err}");
        assert_eq!(store.observation_count().unwrap(), 0);
        assert!(
            store
                .lock_source(resolve(&link).unwrap().as_os_str().as_bytes())
                .unwrap()
                .newest()
                .is_none()
        );

        // A symlinked directory on the way there is resolved, not refused.
        let via = dir.path().join("via");
        std::os::unix::fs::symlink(dir.path(), &via).unwrap();
        let r = import_file(
            &mut store,
            &via.join("real_history"),
            SourceKind::Zsh,
            &Redactor::new(),
        )
        .unwrap();
        assert_eq!(r.records_stored, 1);
        let again = import_file(&mut store, &target, SourceKind::Zsh, &Redactor::new()).unwrap();
        assert_eq!(again.outcome, Outcome::Unchanged);
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
        let contract: String = conn
            .query_row("SELECT parser_contract FROM source_revision", [], |r| {
                r.get(0)
            })
            .unwrap();
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
        assert_eq!(
            store.revision_provenance(u.revision_id).unwrap(),
            Provenance::COMMAND
        );
    }
    fn import_as(
        name: &str,
        content: &str,
        shell: Option<SourceKind>,
    ) -> Result<ImportResult, ImportError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        fs::write(&path, content).unwrap();
        import_file(
            &mut Store::open_memory().unwrap(),
            &path,
            shell,
            &Redactor::new(),
        )
    }

    #[test]
    fn the_shell_is_told_from_content_before_the_file_name() {
        let zsh = ": 1700000000:0;echo one\n";
        let bash = "#1700000000\necho one\n";
        // Misleading names: the content decides.
        assert_eq!(
            import_as("bash_history", zsh, None).unwrap().kind,
            SourceKind::Zsh
        );
        assert_eq!(
            import_as(".zsh_history", bash, None).unwrap().kind,
            SourceKind::Bash
        );
        // Untimed content says nothing, so the name decides.
        assert_eq!(
            import_as(".zsh_history", "echo one\n", None).unwrap().kind,
            SourceKind::Zsh
        );
        assert_eq!(
            import_as(".zhistory", "echo one\n", None).unwrap().kind,
            SourceKind::Zsh
        );
        assert_eq!(
            import_as(".bash_history", "echo one\n", None).unwrap().kind,
            SourceKind::Bash
        );
    }

    #[test]
    fn a_file_that_gives_no_shell_away_is_refused_unless_one_is_given() {
        let plain = "echo one\n";
        assert!(matches!(
            import_as("history", plain, None),
            Err(ImportError::UnknownShell(_))
        ));
        // Only the first line counts: a marker-looking comment further down an
        // untimed history is not evidence of bash.
        let late = "echo one\n#1700000001\necho two\n";
        assert!(matches!(
            import_as("history", late, None),
            Err(ImportError::UnknownShell(_))
        ));
        assert_eq!(
            import_as(".zsh_history", late, None).unwrap().kind,
            SourceKind::Zsh
        );
        assert_eq!(
            import_as("history", plain, Some(SourceKind::Bash))
                .unwrap()
                .kind,
            SourceKind::Bash
        );
    }

    /// A bash frame is a new command only against the frame before it, which
    /// an appended import has already stored.
    #[test]
    fn a_hook_session_is_known_by_its_frames_and_grows_frame_by_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bash-1.frames");
        let frame = |n: u32, cmd: &str| {
            format!(
                "rh1\0bash\0\0{}\0{n}\0/srv\0    {n}  {cmd}\n\0\n",
                1_700_000_000 + n
            )
        };
        fs::write(&path, [frame(1, "old"), frame(2, "make")].concat()).unwrap();
        let mut store = Store::open(&dir.path().join("relic.db")).unwrap();
        let redactor = Redactor::new();
        let r = import_file(&mut store, &path, None, &redactor).unwrap();
        assert_eq!((r.kind, r.records_stored), (SourceKind::Hook, 1));
        // Enter on an empty line, then a command.
        append(&path, &[frame(2, "make"), frame(3, "make test")].concat());
        let r = import_file(&mut store, &path, None, &redactor).unwrap();
        assert_eq!((r.outcome, r.records_stored), (Outcome::Appended, 1));
        assert_eq!(
            texts(&store),
            [
                "make\nexit 2 · cwd after: /srv",
                "make test\nexit 3 · cwd after: /srv"
            ]
        );
    }

    #[test]
    fn a_stored_shell_outlasts_what_later_content_suggests() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        fs::write(&path, "echo one\n").unwrap();
        let mut store = Store::open(&dir.path().join("relic.db")).unwrap();
        let redactor = Redactor::new();
        import_file(&mut store, &path, SourceKind::Bash, &redactor).unwrap();

        // A zsh-looking line typed into bash must not flip the source to zsh.
        append(&path, ": 1700000000:0;echo two\n");
        let r = import_file(&mut store, &path, None, &redactor).unwrap();
        assert_eq!(
            (r.kind, r.replaced, r.records_stored),
            (SourceKind::Bash, None, 1)
        );
        assert_eq!(texts(&store), ["echo one", ": 1700000000:0;echo two"]);
    }

    #[test]
    fn a_differing_shell_replaces_the_source_and_its_search_index() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("relic.db");
        let path = dir.path().join("history");
        // Read as zsh these are four plain commands; as bash, two timed ones.
        fs::write(&path, "#1700000000\necho alpha\n#1700000001\necho beta\n").unwrap();
        let mut store = Store::open(&db).unwrap();
        let redactor = Redactor::new();
        let wrong = import_file(&mut store, &path, SourceKind::Zsh, &redactor).unwrap();
        assert_eq!(wrong.records_stored, 4);
        assert_eq!(store.search("1700000000", 10).unwrap().len(), 1);

        let r = import_file(&mut store, &path, SourceKind::Bash, &redactor).unwrap();
        assert_eq!(
            (r.kind, r.replaced, r.outcome),
            (SourceKind::Bash, Some((SourceKind::Zsh, 4)), Outcome::First)
        );
        assert_eq!(texts(&store), ["echo alpha", "echo beta"]);
        // One source, one revision, and nothing of the zsh reading left to find.
        assert_eq!(visible(&db)[..4], [1, 1, 2, 2]);
        assert!(store.search("1700000000", 10).unwrap().is_empty());
        assert_eq!(store.search("alpha", 10).unwrap().len(), 1);
        // The index holds exactly the rows that are left, not just fewer hits.
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO observation_fts (observation_fts) VALUES ('integrity-check')",
                [],
            )
            .unwrap();

        // The corrected kind is the stored one: no --shell, no second replace.
        let r = import_file(&mut store, &path, None, &redactor).unwrap();
        assert_eq!(
            (r.kind, r.replaced, r.outcome),
            (SourceKind::Bash, None, Outcome::Unchanged)
        );
    }

    /// `git` in `dir` with a fixed identity and clock, so reflog times are
    /// known and repeated entries fall in one second.
    fn git(dir: &Path, args: &[&str]) -> String {
        git_at(dir, 1_700_000_000, args)
    }

    /// [`git`] with the clock at `at` seconds.
    fn git_at(dir: &Path, at: i64, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .envs([
                ("GIT_AUTHOR_NAME", "t"),
                ("GIT_AUTHOR_EMAIL", "t@t"),
                ("GIT_COMMITTER_NAME", "t"),
                ("GIT_COMMITTER_EMAIL", "t@t"),
            ])
            .env("GIT_COMMITTER_DATE", format!("{at} +0000"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn read_git(repo: &Path) -> source::git::Repository {
        source::git::read(repo).unwrap()
    }

    fn git_texts(store: &Store) -> Vec<(ObservationKind, Option<i64>, String)> {
        let mut rows = store.recent(1000).unwrap();
        rows.reverse();
        rows.into_iter()
            .map(|h| (h.kind, h.observed_at_ms, h.safe_text))
            .collect()
    }

    #[test]
    fn a_git_import_keeps_what_git_later_deletes_and_adds_only_what_moved() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git(
            repo,
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "two password=hunter2",
            ],
        );
        let lost = git(repo, &["rev-parse", "HEAD"]);
        git(repo, &["branch", "feat"]);
        fs::write(repo.join("f"), "x").unwrap();
        git(repo, &["add", "f"]);
        git(repo, &["stash", "-q", "-m", "wip"]);
        // Twice in one second: two entries alike, both evidence.
        git(repo, &["reset", "-q", "--hard", "HEAD"]);
        git(repo, &["reset", "-q", "--hard", "HEAD"]);
        git(repo, &["reset", "-q", "--hard", "HEAD~1"]);
        let entries = git(repo, &["log", "-g", "--all", "--format=%H"])
            .lines()
            .count();

        let mut store = Store::open_memory().unwrap();
        let redactor = Redactor::new();
        let r = import_git(&mut store, read_git(&repo.join(".git")), &redactor).unwrap();
        assert_eq!(
            (r.outcome, r.reflog_stored, r.refs_stored),
            (Outcome::First, entries, 3)
        );
        let rows = git_texts(&store);
        let at = Some(1_700_000_000_000);
        let resets = rows
            .iter()
            .filter(|(k, t, s)| {
                *k == ObservationKind::GitReflog && *t == at && s.ends_with("reset: moving to HEAD")
            })
            .count();
        assert_eq!(resets, 3, "one from the stash, two typed: {rows:#?}");
        let commit = format!("git reflog HEAD {lost} commit: two password=[REDACTED:password]");
        assert!(
            rows.iter().any(|(_, t, s)| *s == commit && *t == at),
            "{rows:#?}"
        );
        assert!(
            rows.iter().any(|(k, _, s)| *k == ObservationKind::GitRef
                && s.starts_with("git ref refs/heads/feat ")),
            "{rows:#?}"
        );
        // The lost commit the evidence names is one Git can still give back.
        assert_eq!(
            git(
                repo,
                &["rev-parse", "--verify", &format!("{lost}^{{commit}}")]
            ),
            lost
        );

        // Git forgets the branch, the stash and every reflog entry; relic does not.
        git(repo, &["branch", "-q", "-D", "feat"]);
        git(repo, &["stash", "drop", "-q"]);
        git(repo, &["reflog", "expire", "--expire=now", "--all"]);
        let again = import_git(&mut store, read_git(repo), &redactor).unwrap();
        assert_eq!(
            (again.outcome, again.reflog_stored, again.refs_stored),
            (Outcome::Unchanged, 0, 0)
        );
        assert_eq!(git_texts(&store), rows);

        // A new commit moves HEAD and main: two entries, and main's new place.
        git(repo, &["commit", "-q", "--allow-empty", "-m", "three"]);
        let next = import_git(&mut store, read_git(repo), &redactor).unwrap();
        assert_eq!(
            (next.outcome, next.reflog_stored, next.refs_stored),
            (Outcome::Appended, 2, 1)
        );
        let after = git_texts(&store);
        assert_eq!(&after[..rows.len()], &rows[..]);
        assert!(
            after[rows.len()..]
                .iter()
                .all(|(_, _, s)| s.contains("three")),
            "{after:#?}"
        );
    }

    #[test]
    fn a_history_file_is_never_read_as_git() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zsh_history");
        fs::write(&path, ": 1700000000:0;ls\n").unwrap();
        let mut store = Store::open_memory().unwrap();
        let r = import_file(&mut store, &path, SourceKind::Git, &Redactor::new());
        assert!(matches!(r, Err(ImportError::GitAsHistory)), "{r:?}");
        assert_eq!(store.observation_count().unwrap(), 0);
    }

    #[test]
    fn entries_are_stored_in_gits_order_and_a_repeat_of_a_stored_one_is_added() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q", "-b", "main"]);
        // Two in one second, then one later: HEAD's and main's entries
        // interleave by time, and within a second keep Git's order.
        git(repo, &["commit", "-q", "--allow-empty", "-m", "a"]);
        git(repo, &["commit", "-q", "--allow-empty", "-m", "b"]);
        git_at(
            repo,
            1_700_000_100,
            &["commit", "-q", "--allow-empty", "-m", "c"],
        );
        git_at(repo, 1_700_000_100, &["reset", "-q", "--hard", "HEAD"]);
        // Long after the commit it points at: stored when the ref moved, not
        // by commit date.
        git_at(repo, 1_700_000_200, &["branch", "late", "HEAD~2"]);
        let mut store = Store::open_memory().unwrap();
        import_git(&mut store, read_git(repo), &Redactor::new()).unwrap();
        let rows = git_texts(&store);
        let reflog: Vec<_> = rows
            .iter()
            .filter(|(k, _, _)| *k == ObservationKind::GitReflog)
            .collect();
        assert!(
            reflog.windows(2).all(|w| w[0].1 <= w[1].1),
            "oldest first: {reflog:#?}"
        );
        let head: Vec<&str> = reflog
            .iter()
            .filter(|(_, _, s)| s.starts_with("git reflog HEAD "))
            .map(|(_, _, s)| s.rsplit_once(' ').unwrap().1)
            .collect();
        assert_eq!(head, ["a", "b", "c", "HEAD"], "{reflog:#?}");

        // The same move again, in the same second: a second entry alike.
        git_at(repo, 1_700_000_100, &["reset", "-q", "--hard", "HEAD"]);
        let again = import_git(&mut store, read_git(repo), &Redactor::new()).unwrap();
        assert_eq!((again.reflog_stored, again.refs_stored), (1, 0));
    }

    #[test]
    fn a_linked_worktree_is_the_repository_it_belongs_to() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git(&repo, &["worktree", "add", "-q", "-b", "side", "../wt"]);
        git(
            &dir.path().join("wt"),
            &["commit", "-q", "--allow-empty", "-m", "two"],
        );
        let mut store = Store::open_memory().unwrap();
        let first = import_git(&mut store, read_git(&repo), &Redactor::new()).unwrap();
        let rows = git_texts(&store);
        assert!(
            rows.iter().any(|(_, _, s)| s.ends_with(" commit: two")),
            "the worktree's moves: {rows:#?}"
        );
        let again = import_git(
            &mut store,
            read_git(&dir.path().join("wt")),
            &Redactor::new(),
        )
        .unwrap();
        assert_eq!(
            (first.outcome, again.outcome),
            (Outcome::First, Outcome::Unchanged),
            "{:#?}",
            git_texts(&store)
        );
    }
}
