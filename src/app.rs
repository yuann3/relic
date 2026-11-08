//! Use cases shared by the CLI and the TUI. Presentation adapters call these;
//! they never touch the store directly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::domain::{Provenance, StoredObservation, When};
use crate::redact::Redactor;
use crate::rlm::Progress;
use crate::rlm::coordinator::{
    InvestigationResult, InvestigationStatus, SkillSeed, run_investigation,
};
use crate::rlm::model::ModelBoundary;
use crate::rlm::model::egress::Role;
use crate::rlm::protocol::{AnswerStatus, BudgetV1, FinishV1};
use crate::store::Store;

/// An observation with the commands recorded around it in the same source.
#[derive(Debug, Clone)]
pub struct EvidenceView {
    pub observation: StoredObservation,
    pub timeline: Vec<StoredObservation>,
    /// Inferred upper bounds for the untimed rows above, by observation id.
    pub bounds: HashMap<i64, i64>,
    /// What the observation's revision says its source file encodes.
    pub provenance: Provenance,
}

impl EvidenceView {
    pub fn when(&self, o: &StoredObservation) -> When {
        When::from_parts(o.observed_at_ms, self.bounds.get(&o.id).copied())
    }
}

pub fn open_evidence(
    store: &Store,
    id: i64,
    context: u32,
) -> Result<Option<EvidenceView>, rusqlite::Error> {
    let Some(observation) = store.get_observation(id)? else {
        return Ok(None);
    };
    let timeline = store.get_neighbors(
        observation.revision_id,
        observation.ordinal,
        context,
        context,
    )?;
    let mut bounds = HashMap::new();
    for o in timeline.iter().chain([&observation]) {
        if o.observed_at_ms.is_none()
            && let Some(ms) = store.observed_no_later_than(o.revision_id, o.ordinal)?
        {
            bounds.insert(o.id, ms);
        }
    }
    let provenance = store.revision_provenance(observation.revision_id)?;
    Ok(Some(EvidenceView {
        observation,
        timeline,
        bounds,
        provenance,
    }))
}

/// History text is hostile input. Render it as one line that cannot drive the
/// terminal, break NUL/tab framing, or visually reorder itself: line-breaking
/// whitespace becomes a space; every other control and invisible format
/// character becomes a visible U+FFFD.
pub fn display_line(text: &str) -> String {
    text.chars().map(display_char).collect()
}

pub fn display_char(c: char) -> char {
    match c {
        '\t' | '\n' | '\r' | '\u{2028}' | '\u{2029}' => ' ',
        c if c.is_control() => '\u{FFFD}',
        // Bidi marks/overrides/isolates (Trojan Source), zero-width
        // characters, and interlinear annotation anchors.
        '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' => '\u{FFFD}',
        '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}' => '\u{FFFD}',
        // Soft hyphen, Mongolian vowel separator, prepended number marks,
        // and tag characters (invisible ASCII smuggling).
        '\u{00AD}' | '\u{180E}' | '\u{0600}'..='\u{0605}' | '\u{06DD}' | '\u{070F}' => '\u{FFFD}',
        '\u{E0001}' | '\u{E0020}'..='\u{E007F}' => '\u{FFFD}',
        c => c,
    }
}

pub const RECALL_SCHEMA: &str = "relic.recall/v1";

/// How a recall ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallOutcome {
    Answered,
    Partial,
    /// Nothing it found could be cited for an answer.
    Abstained,
    BudgetExhausted,
    Cancelled,
    Failed,
}

/// One recall, as `relic recall --json` prints it and the TUI shows it.
/// Every string in it is redacted; cites are record ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecallReport {
    pub schema: &'static str,
    /// As the model was asked it: redacted.
    pub question: String,
    pub outcome: RecallOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub answer: String,
    pub claims: Vec<CitedText>,
    pub procedure: Vec<CitedText>,
    pub contradictions: Vec<String>,
    pub missing: Vec<String>,
    /// Every cited record, in the order first cited.
    pub evidence: Vec<CitedRecord>,
    pub root_calls: u32,
    pub child_calls: u16,
    /// At most this many micro-US-dollars; null when the model has no
    /// prices, so the cost is unknown.
    pub cost_microusd: Option<u64>,
    /// What the provider says answered, in the order first seen: an alias
    /// may stand for any of them.
    pub models: Vec<String>,
    /// The active skill the host step picked for this question, before the
    /// investigation began (ADR 0016).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<SkillUsed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillUsed {
    pub name: String,
    /// The bundle's content hash: which version of the skill.
    pub bundle: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CitedText {
    pub text: String,
    pub supports: Vec<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contradicts: Vec<i64>,
}

/// A cited record, with where it sits in its source revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CitedRecord {
    pub id: i64,
    pub revision: i64,
    pub ordinal: i64,
    pub source: &'static str,
    pub observed_at_ms: Option<i64>,
    pub text: String,
}

impl CitedRecord {
    pub fn when(&self) -> When {
        When::from_parts(self.observed_at_ms, None)
    }
}

pub const RECORDS_SCHEMA: &str = "relic.records/v1";

/// Records by id, as `relic show` prints them: the exact redacted text a
/// skill points an agent to, rather than copy where agents rewrite it.
#[derive(Debug, Serialize)]
pub struct RecordsPage {
    pub schema: &'static str,
    pub records: Vec<CitedRecord>,
    /// Ids that name no stored record: forgotten, or never stored.
    pub missing: Vec<i64>,
}

pub fn show(store: &Store, ids: &[i64]) -> Result<RecordsPage, rusqlite::Error> {
    let mut page = RecordsPage {
        schema: RECORDS_SCHEMA,
        records: Vec::new(),
        missing: Vec::new(),
    };
    for &id in ids {
        match store.get_observation(id)? {
            Some(o) => page.records.push(CitedRecord {
                id: o.id,
                revision: o.revision_id,
                ordinal: o.ordinal,
                source: o.kind.source_label(),
                observed_at_ms: o.observed_at_ms,
                text: o.safe_text,
            }),
            None => page.missing.push(id),
        }
    }
    Ok(page)
}

pub const RECALL_LOG_SCHEMA: &str = "relic.recall-log/v1";

/// Where recalls against the store at `db` are logged: beside it, as its
/// key is.
pub fn recall_log_path(db: &Path) -> PathBuf {
    let mut path = db.as_os_str().to_owned();
    path.push(".recall.log");
    PathBuf::from(path)
}

/// The recall log: one JSON line per start, request sent, reply, retry and
/// finish, all under one random recall id. Metadata and record ids only: a
/// copy of evidence text here would outlive `relic forget`.
struct RecallLog {
    file: std::fs::File,
    recall: String,
    /// The first write that failed; later lines are not attempted.
    failed: Option<std::io::Error>,
}

impl RecallLog {
    fn open(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)?;
        // One made by another tool, or before Relic set its umask.
        if file.metadata()?.permissions().mode() & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let mut id = [0u8; 8];
        getrandom::fill(&mut id).map_err(std::io::Error::other)?;
        let recall = crate::rlm::evidence::hex_encode(&id);
        Ok(Self {
            file,
            recall,
            failed: None,
        })
    }

    fn line(&mut self, event: &str, fields: serde_json::Value) {
        use std::io::Write;
        if self.failed.is_some() {
            return;
        }
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let mut line = serde_json::json!({"schema": RECALL_LOG_SCHEMA, "recall": self.recall, "at_ms": at_ms, "event": event});
        if let (Some(line), serde_json::Value::Object(fields)) = (line.as_object_mut(), fields) {
            line.extend(fields);
        }
        let mut bytes = line.to_string().into_bytes();
        bytes.push(b'\n');
        // One write per line: concurrent recalls append whole lines.
        if let Err(e) = self.file.write_all(&bytes) {
            self.failed = Some(e);
        }
    }

    fn progress(&mut self, p: &Progress) {
        let role = |r: &Role| if *r == Role::Root { "root" } else { "child" };
        let redact = |s: &str| Redactor::new().redact(s);
        match p {
            Progress::Egress(a) => self.line("send", serde_json::json!({
                "role": role(&a.role), "bytes": a.bytes, "origin": a.origin, "profile": a.profile,
                "request_hash": a.request_hash, "redaction_revision": a.redaction_revision,
            })),
            Progress::Replied { role: r, returned_model, usage } => {
                let usage = usage.map(|u| serde_json::json!({"prompt_tokens": u.prompt_tokens, "completion_tokens": u.completion_tokens}));
                self.line("reply", serde_json::json!({"role": role(r), "model": returned_model, "usage": usage}));
            }
            Progress::Retry { role: r, attempt, reason } => {
                self.line("retry", serde_json::json!({"role": role(r), "attempt": attempt, "reason": redact(reason)}));
            }
            // Notes quote what the model wrote; the rest is in the finish line.
            Progress::RootCall { .. } | Progress::Op { .. } | Progress::ChildCall { .. } | Progress::Note(_) => {}
        }
    }

    /// The last line, synced: a Ctrl-C exits right after it. Without the
    /// report's error, which can quote evidence the endpoint or the model
    /// echoed.
    fn finish(&mut self, r: &RecallReport) -> Option<std::io::Error> {
        self.line(
            "finish",
            serde_json::json!({
                "outcome": r.outcome, "root_calls": r.root_calls, "child_calls": r.child_calls,
                "cost_microusd": r.cost_microusd, "models": r.models,
                "cited": r.evidence.iter().map(|e| e.id).collect::<Vec<_>>(),
            }),
        );
        if self.failed.is_none()
            && let Err(e) = self.file.sync_data()
        {
            self.failed = Some(e);
        }
        self.failed.take()
    }
}

/// Answer `question` from the evidence in `store` through a bounded
/// investigation, logging it to `log`. Of the active `skills`, the one the
/// question names is picked first and logged. Setting `cancel` from any thread or
/// a signal handler ends it with [`RecallOutcome::Cancelled`]. A log that
/// cannot be opened or written fails the recall, setting `cancel` to stop
/// it mid-way: nothing leaves unaudited.
pub fn recall(
    question: &str,
    store: &Store,
    skills: &[crate::catalog::ActiveSkill],
    model: Arc<dyn ModelBoundary>,
    log: &Path,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(Progress),
) -> RecallReport {
    // Once, here: the report shows the question as it was sent.
    let question = Redactor::new().import(question);
    let log_error =
        |e: std::io::Error| format!("could not write the recall log {}: {e}", log.display());
    let unlogged = |question, e| {
        let failed = InvestigationResult {
            status: InvestigationStatus::Failed(log_error(e)),
            finish: None,
            cited: HashMap::new(),
            cost_microusd: None,
            turns: 0,
            child_calls: 0,
        };
        report(question, failed, Vec::new())
    };
    let mut log = match RecallLog::open(log) {
        Ok(log) => log,
        Err(e) => return unlogged(question, e),
    };
    let target = model.target();
    log.line("start", serde_json::json!({"relic_version": env!("CARGO_PKG_VERSION"), "profile": target.profile, "origin": target.origin}));
    // Before any request leaves, so the log shows the pick preceding them.
    let picked = crate::catalog::select(&question, skills);
    if let Some(s) = picked {
        log.line(
            "skill",
            serde_json::json!({"name": s.name, "bundle": s.bundle}),
        );
    }
    if let Some(e) = log.failed.take() {
        return unlogged(question, e);
    }
    let mut models: Vec<String> = Vec::new();
    let mut progress = |p: Progress| {
        log.progress(&p);
        // A request whose audit line failed must not leave: stop before it
        // is sent. `finish` turns the stop into the log's failure.
        if log.failed.is_some() {
            cancel.store(true, Ordering::SeqCst);
        }
        if let Progress::Replied {
            returned_model: Some(m),
            ..
        } = &p
            && !models.contains(m)
        {
            models.push(m.clone());
        }
        progress(p);
    };
    let seed = picked.map(|s| SkillSeed {
        name: &s.name,
        description: &s.description,
        records: &s.cites,
    });
    let r = run_investigation(
        &question,
        seed.as_ref(),
        store,
        model,
        BudgetV1::prototype_default(),
        cancel,
        &mut progress,
    );
    let mut report = report(question, r, models);
    report.skill = picked.map(|s| SkillUsed {
        name: s.name.clone(),
        bundle: s.bundle.clone(),
    });
    if let Some(e) = log.finish(&report) {
        report.outcome = RecallOutcome::Failed;
        report.error = Some(log_error(e));
    }
    report
}

fn report(question: String, r: InvestigationResult, models: Vec<String>) -> RecallReport {
    let (outcome, error) = match r.status {
        InvestigationStatus::Completed => (RecallOutcome::Answered, None),
        InvestigationStatus::Partial => (RecallOutcome::Partial, None),
        InvestigationStatus::Abstained => (RecallOutcome::Abstained, None),
        InvestigationStatus::BudgetExhausted => (RecallOutcome::BudgetExhausted, None),
        InvestigationStatus::Cancelled => (RecallOutcome::Cancelled, None),
        // A refusal or a malformed reply quotes what the model wrote.
        InvestigationStatus::Failed(e) => (RecallOutcome::Failed, Some(Redactor::new().redact(&e))),
    };
    let finish = r.finish.unwrap_or(FinishV1 {
        status: AnswerStatus::Abstained,
        answer: String::new(),
        claims: vec![],
        procedure: vec![],
        contradictions: vec![],
        missing: vec![],
    });
    let mut evidence: Vec<CitedRecord> = Vec::new();
    let mut ids = |cites: &[String]| -> Vec<i64> {
        cites
            .iter()
            .filter_map(|c| r.cited.get(c))
            .map(|o| {
                if !evidence.iter().any(|e| e.id == o.id) {
                    evidence.push(CitedRecord {
                        id: o.id,
                        revision: o.revision_id,
                        ordinal: o.ordinal,
                        source: o.kind.source_label(),
                        observed_at_ms: o.observed_at_ms,
                        text: o.safe_text.clone(),
                    });
                }
                o.id
            })
            .collect()
    };
    let claims = finish
        .claims
        .iter()
        .map(|c| CitedText {
            text: c.text.clone(),
            supports: ids(&c.supports),
            contradicts: ids(&c.contradicts),
        })
        .collect();
    let procedure = finish
        .procedure
        .iter()
        .map(|s| CitedText {
            text: s.text.clone(),
            supports: ids(&s.supports),
            contradicts: vec![],
        })
        .collect();
    RecallReport {
        schema: RECALL_SCHEMA,
        question,
        outcome,
        error,
        answer: finish.answer,
        claims,
        procedure,
        contradictions: finish.contradictions,
        missing: finish.missing,
        evidence,
        root_calls: r.turns,
        child_calls: r.child_calls,
        cost_microusd: r.cost_microusd,
        models,
        skill: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SourceKind;
    use crate::import::import_file;
    use crate::redact::Redactor;
    use crate::search::MatchMode;
    use crate::search::tests::{req, search, store_with};

    #[test]
    fn evidence_includes_surrounding_commands() {
        let store = store_with(&["cd /etc/nginx", "certbot renew", "systemctl reload nginx"]);
        let id = search(&store, &req("certbot", 1, MatchMode::Words))
            .unwrap()
            .hits[0]
            .id;
        let view = open_evidence(&store, id, 1).unwrap().unwrap();
        assert_eq!(view.observation.safe_text, "certbot renew");
        let texts: Vec<&str> = view.timeline.iter().map(|o| o.safe_text.as_str()).collect();
        assert_eq!(
            texts,
            ["cd /etc/nginx", "certbot renew", "systemctl reload nginx"]
        );
    }

    #[test]
    fn display_line_neutralizes_controls_and_invisible_reordering() {
        assert_eq!(display_line("a\tb\nc\rd\u{2028}e"), "a b c d e");
        assert_eq!(display_line("\x1b[31mred"), "\u{FFFD}[31mred");
        assert_eq!(
            display_line("x\0y\u{7f}z\u{85}"),
            "x\u{FFFD}y\u{FFFD}z\u{FFFD}"
        );
        for c in [
            '\u{061C}',
            '\u{200B}',
            '\u{200E}',
            '\u{202E}',
            '\u{2066}',
            '\u{FEFF}',
            '\u{FFF9}',
            '\u{00AD}',
            '\u{180E}',
            '\u{0600}',
            '\u{E0041}',
        ] {
            assert_eq!(display_line(&format!("a{c}b")), "a\u{FFFD}b", "{c:?}");
        }
        assert_eq!(display_line("部署 café ✓"), "部署 café ✓");
    }

    /// The shape of a real history where share_history was turned on late:
    /// plain lines first, timestamped lines after.
    fn mixed_store() -> Store {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zsh_history");
        std::fs::write(
            &path,
            "ssh volty.exe.dev\nccd --resume\n: 1791017948:0;cd Developer\n: 1791018000:0;ls\n",
        )
        .unwrap();
        let mut store = Store::open_memory().unwrap();
        import_file(&mut store, &path, SourceKind::Zsh, &Redactor::new()).unwrap();
        store
    }

    #[test]
    fn untimed_history_gets_an_inferred_bound_everywhere() {
        let store = mixed_store();
        let first_timed = 1_791_017_948_000;

        let page = search(&store, &req("ccd", 10, MatchMode::Words)).unwrap();
        let hit = &page.hits[0];
        assert_eq!(hit.when(), When::NoLaterThan(first_timed));
        let v: serde_json::Value = serde_json::to_value(&page).unwrap();
        assert_eq!(v["hits"][0]["observed_at_ms"], serde_json::Value::Null);
        assert_eq!(v["hits"][0]["observed_before_ms"], first_timed);

        let view = open_evidence(&store, hit.id, 5).unwrap().unwrap();
        let whens: Vec<When> = view.timeline.iter().map(|o| view.when(o)).collect();
        assert_eq!(
            whens,
            [
                When::NoLaterThan(first_timed),
                When::NoLaterThan(first_timed),
                When::At(first_timed),
                When::At(1_791_018_000_000),
            ]
        );
        assert_eq!(view.when(&view.observation), When::NoLaterThan(first_timed));
    }

    #[test]
    fn missing_evidence_is_none() {
        let store = store_with(&["ls"]);
        assert!(open_evidence(&store, 999, 3).unwrap().is_none());
    }

    #[test]
    fn a_log_that_breaks_after_it_opened_stops_the_request_it_could_not_audit() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        // A pipe whose reader goes away after the start line: the send line
        // then fails with EPIPE.
        let log = dir.path().join("relic.db.recall.log");
        let c_path = std::ffi::CString::new(log.as_os_str().as_bytes()).unwrap();
        // SAFETY: mkfifo of a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let mut reader = Some(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&log)
                .unwrap(),
        );
        let model = Arc::new(crate::rlm::model::FakeModel::new());
        let r = recall(
            "q",
            &store_with(&["ls"]),
            &[],
            model.clone(),
            &log,
            &Arc::default(),
            &mut |p| {
                if matches!(p, Progress::RootCall { .. }) {
                    drop(reader.take());
                }
            },
        );
        assert_eq!(r.outcome, RecallOutcome::Failed);
        assert!(
            r.error
                .as_deref()
                .is_some_and(|e| e.contains("could not write the recall log")),
            "{:?}",
            r.error
        );
        assert!(
            model.requests().is_empty(),
            "a request left without its audit"
        );
    }
}
