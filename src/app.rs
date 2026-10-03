//! Use cases shared by the CLI and the TUI. Presentation adapters call these;
//! they never touch the store directly.

use serde::Serialize;

use crate::domain::{ObservationKind, StoredObservation};
use crate::store::Store;

pub const SEARCH_SCHEMA: &str = "relic.search/v1";

/// Hard cap on one page. Broad queries past it must be narrowed rather than
/// handed to a picker as a stand-in for the index.
pub const MAX_PAGE: usize = 5_000;

/// A shorter trailing word is not sent to FTS: one-character prefixes merge
/// the doclists of every term they start and cost ~20 ms at 1M, while
/// two characters stay under 8 ms (docs/research/interactive-fts-measurements.md).
pub const MIN_PREFIX_CHARS: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchMode {
    /// Every word must match whole. Used by scripts and recall.
    Words,
    /// Newest first; the last word matches as a prefix once it has
    /// [`MIN_PREFIX_CHARS`]. Used while the user is typing.
    PrefixLast,
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub limit: usize,
    pub mode: MatchMode,
}

/// One immutable page of ranked results. A picker may only return ids that
/// are members of the page it was given.
#[derive(Debug, Clone, Serialize)]
pub struct SearchPage {
    pub schema: &'static str,
    pub query: String,
    pub limit: usize,
    /// More matches exist beyond `limit`.
    pub truncated: bool,
    /// A trailing word too short to filter by yet; the page ignores it.
    pub deferred: Option<String>,
    pub hits: Vec<PageHit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageHit {
    pub id: i64,
    pub observed_at_ms: Option<i64>,
    pub source: &'static str,
    pub text: String,
    #[serde(skip)]
    pub revision_id: i64,
    #[serde(skip)]
    pub ordinal: i64,
}

impl SearchPage {
    pub fn contains(&self, id: i64) -> bool {
        self.hits.iter().any(|h| h.id == id)
    }
}

pub fn search(store: &Store, req: &SearchRequest) -> Result<SearchPage, rusqlite::Error> {
    let limit = req.limit.clamp(1, MAX_PAGE);
    // Ask for one extra row so the page can say whether it was cut off.
    let mut deferred = None;
    let mut hits = match req.mode {
        MatchMode::Words => store.search(&req.query, limit + 1)?,
        MatchMode::PrefixLast => {
            let mut words: Vec<&str> = req.query.split_whitespace().collect();
            if words.last().is_some_and(|w| w.chars().count() < MIN_PREFIX_CHARS) {
                deferred = words.pop().map(String::from);
            }
            if words.is_empty() {
                store.recent(limit + 1)?
            } else {
                store.search_recent_prefix(&words.join(" "), limit + 1)?
            }
        }
    };
    let truncated = hits.len() > limit;
    hits.truncate(limit);
    Ok(SearchPage {
        schema: SEARCH_SCHEMA,
        query: req.query.clone(),
        limit,
        truncated,
        deferred,
        hits: hits
            .into_iter()
            .map(|h| PageHit {
                id: h.id,
                observed_at_ms: h.observed_at_ms,
                source: source_label(h.kind),
                text: h.safe_text,
                revision_id: h.revision_id,
                ordinal: h.ordinal,
            })
            .collect(),
    })
}

/// An observation with the commands recorded around it in the same source.
#[derive(Debug, Clone)]
pub struct EvidenceView {
    pub observation: StoredObservation,
    pub timeline: Vec<StoredObservation>,
}

pub fn open_evidence(
    store: &Store,
    id: i64,
    context: u32,
) -> Result<Option<EvidenceView>, rusqlite::Error> {
    let Some(observation) = store.get_observation(id)? else {
        return Ok(None);
    };
    let timeline =
        store.get_neighbors(observation.revision_id, observation.ordinal, context, context)?;
    Ok(Some(EvidenceView { observation, timeline }))
}

pub fn source_label(kind: ObservationKind) -> &'static str {
    match kind {
        ObservationKind::ZshCommand => "zsh",
        ObservationKind::BashCommand => "bash",
        ObservationKind::HookFrame => "hook",
        ObservationKind::AgentMessage => "agent",
    }
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
        c => c,
    }
}

/// `YYYY-MM-DD HH:MM` in UTC. Local time needs a tz database, which the
/// prototype does not carry.
pub fn format_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{import_file, SourceKind};
    use crate::redact::Redactor;
    use std::io::Write;

    fn store_with(lines: &[&str]) -> Store {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zsh_history");
        let mut f = std::fs::File::create(&path).unwrap();
        for (i, l) in lines.iter().enumerate() {
            writeln!(f, ": {}:0;{l}", 1_700_000_000 + i).unwrap();
        }
        let mut store = Store::open_memory().unwrap();
        import_file(&mut store, &path, SourceKind::Zsh, &Redactor::new()).unwrap();
        store
    }

    fn req(query: &str, limit: usize, mode: MatchMode) -> SearchRequest {
        SearchRequest { query: query.into(), limit, mode }
    }

    #[test]
    fn page_reports_truncation() {
        let store = store_with(&["git a", "git b", "git c"]);
        let page = search(&store, &req("git", 2, MatchMode::Words)).unwrap();
        assert_eq!(page.hits.len(), 2);
        assert!(page.truncated);

        let page = search(&store, &req("git", 3, MatchMode::Words)).unwrap();
        assert!(!page.truncated);
    }

    #[test]
    fn prefix_mode_finds_partial_input() {
        let store = store_with(&["kubectl get pods", "ls"]);
        assert!(search(&store, &req("kube", 10, MatchMode::Words)).unwrap().hits.is_empty());
        let page = search(&store, &req("kube", 10, MatchMode::PrefixLast)).unwrap();
        assert_eq!(page.hits[0].text, "kubectl get pods");
        assert_eq!(page.hits[0].source, "zsh");
    }

    #[test]
    fn prefix_mode_is_newest_first() {
        let store = store_with(&["git", "git commit -m a long message here", "git push"]);
        let page = search(&store, &req("gi", 10, MatchMode::PrefixLast)).unwrap();
        let texts: Vec<&str> = page.hits.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, ["git push", "git commit -m a long message here", "git"]);
    }

    #[test]
    fn short_trailing_word_is_deferred_not_searched() {
        let store = store_with(&["git commit", "git push", "ls -la", "cargo build"]);

        // "git c": filter by "git" only, newest first, and say "c" is pending.
        let page = search(&store, &req("git c", 10, MatchMode::PrefixLast)).unwrap();
        let texts: Vec<&str> = page.hits.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, ["git push", "git commit"]);
        assert_eq!(page.deferred.as_deref(), Some("c"));

        // A lone short word shows the newest commands unfiltered.
        let page = search(&store, &req("l", 2, MatchMode::PrefixLast)).unwrap();
        let texts: Vec<&str> = page.hits.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, ["cargo build", "ls -la"]);
        assert!(page.truncated);

        // Two characters filter. Words mode never defers.
        let page = search(&store, &req("git co", 10, MatchMode::PrefixLast)).unwrap();
        assert_eq!((page.hits.len(), page.deferred), (1, None));
        let page = search(&store, &req("git c", 10, MatchMode::Words)).unwrap();
        assert_eq!((page.hits.len(), page.deferred), (0, None));
    }

    #[test]
    fn limit_is_capped_at_max_page() {
        let store = store_with(&["git a"]);
        let page = search(&store, &req("git", usize::MAX, MatchMode::Words)).unwrap();
        assert_eq!(page.limit, MAX_PAGE);
    }

    #[test]
    fn search_json_is_versioned_and_hides_locators() {
        let store = store_with(&["certbot renew"]);
        let page = search(&store, &req("certbot", 10, MatchMode::Words)).unwrap();
        let v: serde_json::Value = serde_json::to_value(&page).unwrap();
        assert_eq!(v["schema"], "relic.search/v1");
        assert_eq!(v["hits"][0]["text"], "certbot renew");
        assert_eq!(v["hits"][0]["observed_at_ms"], 1_700_000_000_000i64);
        assert!(v["hits"][0].get("revision_id").is_none());
    }

    #[test]
    fn redacted_secrets_never_reach_the_page() {
        let store = store_with(&["API_KEY=sk12345 cargo deploy"]);
        let page = search(&store, &req("cargo", 10, MatchMode::Words)).unwrap();
        assert!(!page.hits[0].text.contains("sk12345"));
    }

    #[test]
    fn evidence_includes_surrounding_commands() {
        let store = store_with(&["cd /etc/nginx", "certbot renew", "systemctl reload nginx"]);
        let id = search(&store, &req("certbot", 1, MatchMode::Words)).unwrap().hits[0].id;
        let view = open_evidence(&store, id, 1).unwrap().unwrap();
        assert_eq!(view.observation.safe_text, "certbot renew");
        let texts: Vec<&str> = view.timeline.iter().map(|o| o.safe_text.as_str()).collect();
        assert_eq!(texts, ["cd /etc/nginx", "certbot renew", "systemctl reload nginx"]);
    }

    #[test]
    fn display_line_neutralizes_controls_and_invisible_reordering() {
        assert_eq!(display_line("a\tb\nc\rd\u{2028}e"), "a b c d e");
        assert_eq!(display_line("\x1b[31mred"), "\u{FFFD}[31mred");
        assert_eq!(display_line("x\0y\u{7f}z\u{85}"), "x\u{FFFD}y\u{FFFD}z\u{FFFD}");
        for c in ['\u{061C}', '\u{200B}', '\u{200E}', '\u{202E}', '\u{2066}', '\u{FEFF}', '\u{FFF9}'] {
            assert_eq!(display_line(&format!("a{c}b")), "a\u{FFFD}b", "{:?}", c);
        }
        assert_eq!(display_line("部署 café ✓"), "部署 café ✓");
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01 00:00");
        assert_eq!(format_utc(1_700_000_000_000), "2023-11-14 22:13");
        assert_eq!(format_utc(951_782_400_000), "2000-02-29 00:00");
        assert_eq!(format_utc(-60_000), "1969-12-31 23:59");
    }

    #[test]
    fn missing_evidence_is_none() {
        let store = store_with(&["ls"]);
        assert!(open_evidence(&store, 999, 3).unwrap().is_none());
    }
}
