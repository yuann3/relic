//! The skill catalog (ADR 0013): validation records, immutable
//! content-addressed bundles, and activation by symlink swap into the
//! cross-client `.agents/skills/` directories most harnesses read
//! (docs/research/validation-skill-packaging.md).
//!
//! It lives beside the store at `<db>.catalog/`: `catalog.db`, synced in
//! full on every commit, and one `<hash>/<name>/SKILL.md` per bundle.

pub mod compile;

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use crate::store::Store;
use crate::validation::ValidationRecord;

#[derive(Debug)]
pub enum CatalogError {
    Sqlite(rusqlite::Error),
    Io(io::Error),
    Report(String),
    Compile(compile::CompileError),
    /// No verifier has passed for this report.
    Unconfirmed,
    /// Something Relic did not put there is at a link's path.
    NotOurs(PathBuf),
    /// Every activation of this link is already rolled back.
    NothingToRollBack(PathBuf),
    NoSuchSkill(String),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "skill catalog: {e}"),
            Self::Io(e) => write!(f, "skill catalog: {e}"),
            Self::Report(why) => write!(f, "not a recall report Relic can use: {why}"),
            Self::Compile(e) => e.fmt(f),
            Self::Unconfirmed => f.write_str("no verifier has passed for this report; run relic validate first"),
            Self::NotOurs(p) => write!(f, "{} exists and is not a skill Relic activated; leaving it alone", p.display()),
            Self::NothingToRollBack(p) => write!(f, "{}: no activation left to roll back", p.display()),
            Self::NoSuchSkill(name) => write!(f, "no compiled skill named {name}"),
        }
    }
}

impl std::error::Error for CatalogError {}

impl From<rusqlite::Error> for CatalogError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<io::Error> for CatalogError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Where the catalog for the store at `db` lives.
pub fn dir_for(db: &Path) -> PathBuf {
    let mut dir = db.as_os_str().to_owned();
    dir.push(".catalog");
    PathBuf::from(dir)
}

/// The parts of a `relic.recall/v1` report a skill is built from.
#[derive(Debug, Deserialize)]
pub struct Report {
    pub schema: String,
    pub outcome: String,
    pub procedure: Vec<ReportStep>,
}

#[derive(Debug, Deserialize)]
pub struct ReportStep {
    pub text: String,
    pub supports: Vec<i64>,
}

/// A report that holds a procedure, and the digest a validation is filed
/// under: edit the report and it needs validating again.
pub fn read_report(bytes: &[u8]) -> Result<(Report, String), CatalogError> {
    let report: Report = serde_json::from_slice(bytes).map_err(|e| CatalogError::Report(e.to_string()))?;
    if report.schema != crate::app::RECALL_SCHEMA {
        return Err(CatalogError::Report(format!("schema {}, not {}", report.schema, crate::app::RECALL_SCHEMA)));
    }
    if !matches!(report.outcome.as_str(), "answered" | "partial") || report.procedure.is_empty() {
        return Err(CatalogError::Report(format!("it {} with no procedure", report.outcome)));
    }
    Ok((report, blake3::hash(bytes).to_hex().to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub hash: String,
    pub name: String,
    pub description: String,
    /// `<catalog>/<hash>/<name>`, the directory a link points at.
    pub path: PathBuf,
}

/// A bundle some link points at now: what the pre-recall host step matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSkill {
    pub name: String,
    pub description: String,
    pub bundle: String,
}

pub struct Catalog {
    conn: Connection,
    dir: PathBuf,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS validation (
    id            INTEGER PRIMARY KEY,
    report_digest TEXT NOT NULL,
    record        TEXT NOT NULL,
    confirmed     INTEGER NOT NULL CHECK (confirmed IN (0,1)),
    at_ms         INTEGER NOT NULL
);
-- cites: [[record id, keyed hash of its safe text], ...], the durable form.
CREATE TABLE IF NOT EXISTS bundle (
    hash          TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    description   TEXT NOT NULL,
    validation_id INTEGER NOT NULL REFERENCES validation(id),
    cites         TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
-- Append-only: each swap of a link, and what it pointed at before. A
-- rollback's row names the activation it undid.
CREATE TABLE IF NOT EXISTS activation (
    id       INTEGER PRIMARY KEY,
    link     BLOB NOT NULL,
    bundle   TEXT REFERENCES bundle(hash),
    previous TEXT REFERENCES bundle(hash),
    undoes   INTEGER REFERENCES activation(id),
    at_ms    INTEGER NOT NULL
);
";

fn now_ms() -> i64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
}

/// Durable on macOS too: there `fsync` stops at the drive's cache, and Rust's
/// `sync_all` is `F_FULLFSYNC`.
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

impl Catalog {
    /// Open the catalog, creating it on first use.
    pub fn open(dir: &Path) -> Result<Self, CatalogError> {
        fs::create_dir_all(dir)?;
        // Symlinks into it must not depend on the cwd.
        let dir = fs::canonicalize(dir)?;
        let conn = Connection::open(dir.join("catalog.db"))?;
        // FULL alone is not durable on macOS: SQLite's fullfsync is off there
        // by default.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA fullfsync = ON;
             PRAGMA checkpoint_fullfsync = ON; PRAGMA foreign_keys = ON;",
        )?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn, dir })
    }

    /// The catalog if one exists: recall must not create one.
    pub fn open_existing(dir: &Path) -> Result<Option<Self>, CatalogError> {
        if !dir.join("catalog.db").exists() {
            return Ok(None);
        }
        Self::open(dir).map(Some)
    }

    pub fn add_validation(&self, report_digest: &str, r: &ValidationRecord) -> Result<i64, CatalogError> {
        let record = serde_json::to_string(r).map_err(|e| CatalogError::Report(e.to_string()))?;
        self.conn.execute(
            "INSERT INTO validation (report_digest, record, confirmed, at_ms) VALUES (?1, ?2, ?3, ?4)",
            params![report_digest, record, r.confirmed(), now_ms()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The newest passing validation of this report: its id and time.
    fn confirmed(&self, report_digest: &str) -> Result<Option<(i64, i64)>, CatalogError> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, at_ms FROM validation WHERE report_digest = ?1 AND confirmed = 1 ORDER BY id DESC LIMIT 1",
                [report_digest],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Compile a validated report into a bundle, install it, and record it.
    /// Step prose comes from the report; every recorded command from the
    /// store, so a hand-edited report cannot put commands in a skill.
    pub fn compile(&self, store: &Store, report: &[u8], name: &str, description: &str) -> Result<Bundle, CatalogError> {
        let (report, digest) = read_report(report)?;
        let (validation_id, confirmed_at) = self.confirmed(&digest)?.ok_or(CatalogError::Unconfirmed)?;
        let key = match store.key_path() {
            Some(path) => crate::key::load_or_create(path)?.0,
            None => [0; 32],
        };
        let mut cites = Vec::new();
        let mut observations = Vec::new();
        for step in &report.procedure {
            let mut cited = Vec::new();
            for &id in &step.supports {
                let o = store.get_observation(id)?.ok_or_else(|| CatalogError::Report(format!("record #{id} is gone")))?;
                cites.push((id, blake3::keyed_hash(&key, o.safe_text.as_bytes()).to_hex()[..32].to_string()));
                cited.push(o);
            }
            observations.push(cited);
        }
        let steps: Vec<compile::Step> = report
            .procedure
            .iter()
            .zip(&observations)
            .map(|(step, cited)| compile::Step {
                text: &step.text,
                records: cited
                    .iter()
                    .map(|o| compile::Record {
                        id: o.id,
                        source: crate::app::source_label(o.kind),
                        observed_at_ms: o.observed_at_ms,
                        text: &o.safe_text,
                    })
                    .collect(),
            })
            .collect();
        let md = compile::render(name, description, confirmed_at, &steps).map_err(CatalogError::Compile)?;
        let bundle = self.install(name, description, &md)?;
        let cites = serde_json::to_string(&cites).map_err(|e| CatalogError::Report(e.to_string()))?;
        self.conn.execute(
            "INSERT OR IGNORE INTO bundle (hash, name, description, validation_id, cites, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![bundle.hash, name, description, validation_id, cites, now_ms()],
        )?;
        Ok(bundle)
    }

    /// Write `<hash>/<name>/SKILL.md` once: a temp directory, each file and
    /// directory synced, then a rename that never replaces.
    fn install(&self, name: &str, description: &str, md: &str) -> Result<Bundle, CatalogError> {
        let hash = blake3::hash(md.as_bytes()).to_hex().to_string();
        let bundle = Bundle { hash: hash.clone(), name: name.into(), description: description.into(), path: self.dir.join(&hash).join(name) };
        if bundle.path.join("SKILL.md").exists() {
            return Ok(bundle);
        }
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).map_err(io::Error::other)?;
        let tmp = self.dir.join(format!(".tmp-{}", blake3::hash(&nonce).to_hex()));
        fs::create_dir_all(tmp.join(name))?;
        let file = tmp.join(name).join("SKILL.md");
        fs::write(&file, md)?;
        fs::File::open(&file)?.sync_all()?;
        sync_dir(&tmp.join(name))?;
        sync_dir(&tmp)?;
        match fs::rename(&tmp, self.dir.join(&hash)) {
            Ok(()) => {}
            // Another compile of the same content got there first.
            Err(_) if bundle.path.join("SKILL.md").exists() => fs::remove_dir_all(&tmp)?,
            Err(e) => return Err(e.into()),
        }
        sync_dir(&self.dir)?;
        Ok(bundle)
    }

    /// The newest bundle compiled under `name`.
    pub fn bundle(&self, name: &str) -> Result<Bundle, CatalogError> {
        let row = self
            .conn
            .query_row(
                "SELECT hash, description FROM bundle WHERE name = ?1 ORDER BY created_at_ms DESC, rowid DESC LIMIT 1",
                [name],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        let (hash, description) = row.ok_or_else(|| CatalogError::NoSuchSkill(name.into()))?;
        Ok(Bundle { path: self.dir.join(&hash).join(name), hash, name: name.into(), description })
    }

    /// Which bundle `link` points at now, if Relic put it there; an error
    /// for anything else at that path.
    fn current(&self, link: &Path) -> Result<Option<String>, CatalogError> {
        let meta = match fs::symlink_metadata(link) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            meta => meta?,
        };
        if !meta.file_type().is_symlink() {
            return Err(CatalogError::NotOurs(link.into()));
        }
        let target = fs::read_link(link)?;
        let hash = target.strip_prefix(&self.dir).ok().and_then(|rest| rest.components().next());
        match hash {
            Some(hash) => Ok(Some(hash.as_os_str().to_string_lossy().into_owned())),
            None => Err(CatalogError::NotOurs(link.into())),
        }
    }

    /// Point `link` at `bundle`, or remove it, by an atomic rename.
    fn point(&self, link: &Path, bundle: Option<&str>) -> Result<(), CatalogError> {
        let parent = link.parent().ok_or_else(|| CatalogError::NotOurs(link.into()))?;
        match bundle {
            Some(hash) => {
                let name = link.file_name().ok_or_else(|| CatalogError::NotOurs(link.into()))?;
                fs::create_dir_all(parent)?;
                // Dot-named and not `.md`: no harness takes it for a skill.
                let mut tmp = std::ffi::OsString::from(".");
                tmp.push(name);
                tmp.push(".relic-swap");
                let tmp = parent.join(tmp);
                let _ = fs::remove_file(&tmp);
                std::os::unix::fs::symlink(self.dir.join(hash).join(name), &tmp)?;
                fs::rename(&tmp, link)?;
            }
            None => fs::remove_file(link)?,
        }
        Ok(sync_dir(parent)?)
    }

    /// Point every link at `bundle`, or none of them: links are checked first,
    /// and a failed swap puts back those already made.
    pub fn activate(&self, bundle: &Bundle, links: &[PathBuf]) -> Result<(), CatalogError> {
        let previous: Vec<Option<String>> = links.iter().map(|l| self.current(l)).collect::<Result<_, _>>()?;
        let tx = self.conn.unchecked_transaction()?;
        for (link, previous) in links.iter().zip(&previous) {
            tx.execute(
                "INSERT INTO activation (link, bundle, previous, at_ms) VALUES (?1, ?2, ?3, ?4)",
                params![link.as_os_str().as_bytes(), bundle.hash, previous, now_ms()],
            )?;
        }
        for (i, link) in links.iter().enumerate() {
            if let Err(e) = self.point(link, Some(&bundle.hash)) {
                for (link, previous) in links.iter().zip(&previous).take(i) {
                    let _ = self.point(link, previous.as_deref());
                }
                return Err(e);
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Undo each link's newest activation not yet undone: point it back at
    /// what was there before, or remove it if nothing was. Rolling back again
    /// steps further back. Returns where each now points.
    pub fn rollback(&self, links: &[PathBuf]) -> Result<Vec<Option<String>>, CatalogError> {
        let mut back = Vec::new();
        for link in links {
            let last: Option<(i64, Option<String>)> = self
                .conn
                .query_row(
                    "SELECT id, previous FROM activation WHERE link = ?1 AND undoes IS NULL
                     AND id NOT IN (SELECT undoes FROM activation WHERE undoes IS NOT NULL)
                     ORDER BY id DESC LIMIT 1",
                    [link.as_os_str().as_bytes()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let (undoes, previous) = last.ok_or_else(|| CatalogError::NothingToRollBack(link.clone()))?;
            // Refuses a link someone replaced since.
            let now = self.current(link)?;
            back.push((link, undoes, now, previous));
        }
        let tx = self.conn.unchecked_transaction()?;
        for (link, undoes, now, previous) in &back {
            tx.execute(
                "INSERT INTO activation (link, bundle, previous, undoes, at_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![link.as_os_str().as_bytes(), previous, now, undoes, now_ms()],
            )?;
        }
        for (link, _, _, previous) in &back {
            self.point(link, previous.as_deref())?;
        }
        tx.commit()?;
        Ok(back.into_iter().map(|(_, _, _, previous)| previous).collect())
    }

    /// Every bundle a link points at now, once each.
    pub fn active(&self) -> Result<Vec<ActiveSkill>, CatalogError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT b.name, b.description, b.hash FROM activation a JOIN bundle b ON b.hash = a.bundle
             WHERE a.id IN (SELECT MAX(id) FROM activation GROUP BY link) ORDER BY b.name",
        )?;
        let rows = stmt.query_map([], |r| Ok(ActiveSkill { name: r.get(0)?, description: r.get(1)?, bundle: r.get(2)? }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

/// Words that say nothing about which skill a question needs.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "from", "that", "this", "how", "did", "does", "what", "when", "where", "can", "could",
    "would", "should", "get", "got", "use", "using", "into", "out", "was", "were", "are", "you", "your", "our", "its",
    "has", "have", "had", "not", "but", "all", "any", "some", "one", "back", "again", "after", "before", "then", "now",
];

fn terms(text: &str) -> std::collections::BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 3 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// The pre-recall host step (ADR 0016): the active skill whose name and
/// description share the most words with the question, if it shares at
/// least two and no other skill shares as many. Deterministic, and outside
/// the RLM kernel.
pub fn select<'a>(question: &str, skills: &'a [ActiveSkill]) -> Option<&'a ActiveSkill> {
    let asked = terms(question);
    let mut scored: Vec<(usize, &ActiveSkill)> = skills
        .iter()
        .map(|s| (terms(&format!("{} {}", s.name.replace('-', " "), s.description)).intersection(&asked).count(), s))
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    match scored.as_slice() {
        [(best, skill), rest @ ..] if *best >= 2 && rest.first().is_none_or(|(next, _)| next < best) => Some(skill),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, description: &str) -> ActiveSkill {
        ActiveSkill { name: name.into(), description: description.into(), bundle: name.into() }
    }

    #[test]
    fn the_host_step_picks_the_one_skill_a_question_names_and_otherwise_none() {
        let reflog = skill("git-reflog-recovery", "Recover lost Git work, such as commits, branches or stashes, from the reflog");
        let certs = skill("certbot-renewal", "Renew a TLS certificate with certbot and reload nginx");
        let skills = [certs.clone(), reflog.clone()];
        assert_eq!(select("I dropped a git stash by mistake; how do I recover it?", &skills), Some(&reflog));
        assert_eq!(select("how did I renew the nginx certificate?", &skills), Some(&certs));
        // One shared word is not enough, and a tie names no skill.
        assert_eq!(select("what is git?", &skills), None);
        let twin = skill("git-lost-work", "Recover lost Git work from the reflog");
        assert_eq!(select("recover lost git work", &[reflog, twin]), None);
    }

    #[test]
    fn activation_swaps_links_atomically_refuses_what_is_not_ours_and_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(&dir.path().join("relic.db.catalog")).unwrap();
        let v = ValidationRecord {
            argv: vec!["true".into()],
            cwd: "/".into(),
            outcome: crate::validation::Outcome::Passed,
            exit_code: Some(0),
            duration_ms: 1,
            relic_version: "0".into(),
            assurance: crate::validation::UNISOLATED.into(),
        };
        let vid = catalog.add_validation("d", &v).unwrap();
        let bundle = |md: &str| {
            let b = catalog.install("git-reflog-recovery", "d", md).unwrap();
            catalog
                .conn
                .execute("INSERT OR IGNORE INTO bundle VALUES (?1, ?2, 'd', ?3, '[]', ?4)", params![b.hash, b.name, vid, now_ms()])
                .unwrap();
            b
        };
        let (one, two) = (bundle("one"), bundle("two"));
        let skills = dir.path().join("home/.agents/skills");
        let link = skills.join("git-reflog-recovery");
        let read = |link: &Path| fs::read_to_string(link.join("SKILL.md")).unwrap();

        catalog.activate(&one, std::slice::from_ref(&link)).unwrap();
        assert_eq!(read(&link), "one");
        catalog.activate(&two, std::slice::from_ref(&link)).unwrap();
        assert_eq!(read(&link), "two");
        assert_eq!(catalog.active().unwrap().iter().map(|s| s.bundle.as_str()).collect::<Vec<_>>(), [two.hash.as_str()]);
        // Nothing but the link itself is left in the skills directory.
        assert_eq!(fs::read_dir(&skills).unwrap().count(), 1);

        assert_eq!(catalog.rollback(std::slice::from_ref(&link)).unwrap(), [Some(one.hash.clone())]);
        assert_eq!(read(&link), "one");
        // Back past the first activation: no link at all.
        assert_eq!(catalog.rollback(std::slice::from_ref(&link)).unwrap(), [None]);
        assert!(fs::symlink_metadata(&link).is_err(), "rolled back to before the first activation");
        assert!(catalog.active().unwrap().is_empty());
        assert!(matches!(catalog.rollback(std::slice::from_ref(&link)), Err(CatalogError::NothingToRollBack(_))));

        // A skill the user made themselves is never replaced, and an all-or-
        // nothing activation touches no other link.
        let theirs = skills.join("theirs");
        fs::create_dir_all(&theirs).unwrap();
        let mine = dir.path().join("home/.claude/skills/git-reflog-recovery");
        let err = catalog.activate(&one, &[mine.clone(), theirs.clone()]).unwrap_err();
        assert!(matches!(err, CatalogError::NotOurs(p) if p == theirs));
        assert!(fs::symlink_metadata(&mine).is_err());

        // A swap that fails part-way puts back the links already swapped.
        use std::os::unix::fs::PermissionsExt;
        catalog.activate(&one, std::slice::from_ref(&link)).unwrap();
        let locked = dir.path().join("locked/.agents/skills");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(catalog.activate(&two, &[link.clone(), locked.join("git-reflog-recovery")]).is_err());
        assert_eq!(read(&link), "one");
        assert_eq!(catalog.active().unwrap().iter().map(|s| s.bundle.as_str()).collect::<Vec<_>>(), [one.hash.as_str()]);
    }
}
