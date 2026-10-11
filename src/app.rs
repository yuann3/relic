//! Use cases shared by the CLI and the TUI. Presentation adapters call these;
//! they never touch the store directly.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::domain::{ObservationKind, Provenance, StoredObservation};
use crate::store::{Store, is_interrupt};

pub const SEARCH_SCHEMA: &str = "relic.search/v1";

/// Hard cap on one page. Broad queries past it must be narrowed rather than
/// handed to a picker as a stand-in for the index.
pub const MAX_PAGE: usize = 5_000;

/// A shorter trailing word is not sent to FTS. This removes only one-character
/// prefixes and does not bound cost: a prefix with no index of exactly its
/// length merges every posting under it before LIMIT applies (~13 ns per
/// posting, so `tas*` costs 12.6 ms on the 1M fixture, where `task` is in 93%
/// of rows). See docs/research/interactive-fts-measurements.md.
pub const MIN_PREFIX_CHARS: usize = 2;

/// Words past this many are dropped. One cap for the CLI, the TUI and the
/// RLM `search` operator (ADR 0006): rank cost grows faster than the word
/// count, and nobody types a thirteenth word to narrow a search.
pub const MAX_QUERY_WORDS: usize = 12;

/// A word that tokenizes into more tokens than this is dropped. One word can
/// be a long phrase to FTS5 ("task/" fifty times is one word and a 50-token
/// phrase), which the word cap does not see. 16 keeps real paths and URLs.
pub const MAX_WORD_TOKENS: usize = 16;

/// How long one query may run before it is stopped as too broad.
pub const QUERY_DEADLINE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchMode {
    /// Every word must match whole. Used by scripts and recall.
    Words,
    /// Newest first; the last word matches as a prefix once it has
    /// [`MIN_PREFIX_CHARS`]. Used while the user is typing.
    PrefixLast,
}

/// The order of a page. Rowid order is import order, not command time, and
/// is never called "newest" (ADR 0006).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    RecentlyImported,
    Rank,
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub limit: usize,
    pub mode: MatchMode,
    pub order: Order,
}

/// When a search gives up. Both are polled from SQLite's progress handler.
#[derive(Debug, Clone)]
pub struct Budget {
    pub deadline: Instant,
    /// Set from any thread to abandon the search.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Budget {
    /// [`QUERY_DEADLINE`] from now.
    pub fn start() -> Self {
        Self { deadline: Instant::now() + QUERY_DEADLINE, cancel: None }
    }

    fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
    }

    fn spent(&self) -> bool {
        self.cancelled() || Instant::now() >= self.deadline
    }
}

#[derive(Debug)]
pub enum SearchError {
    /// [`Budget::cancel`] was set.
    Cancelled,
    /// The search ran past [`Budget::deadline`].
    TooBroad,
    Store(rusqlite::Error),
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("search cancelled"),
            Self::TooBroad => write!(
                f,
                "query too broad: stopped after {} ms; add a rarer word",
                QUERY_DEADLINE.as_millis()
            ),
            Self::Store(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for SearchError {}

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
    /// Words the page ignored: past [`MAX_QUERY_WORDS`], or longer than
    /// [`MAX_WORD_TOKENS`] tokens.
    pub words_dropped: usize,
    pub order: Order,
    pub hits: Vec<PageHit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageHit {
    pub id: i64,
    pub observed_at_ms: Option<i64>,
    /// Untimed hits only: inferred upper bound from history order.
    pub observed_before_ms: Option<i64>,
    pub source: &'static str,
    pub text: String,
}

impl PageHit {
    pub fn when(&self) -> When {
        When::from_parts(self.observed_at_ms, self.observed_before_ms)
    }
}

impl SearchPage {
    pub fn contains(&self, id: i64) -> bool {
        self.hits.iter().any(|h| h.id == id)
    }
}

/// Runs of letters and digits are what the unicode61 tokenizer indexes: "-m"
/// has one, and "é" plus a combining mark is still one character of one.
fn tokens(word: &str) -> impl Iterator<Item = &str> {
    word.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty())
}

/// Splits `word` before its last token: "src/main.r" is ("src/main.", "r").
fn split_last_token(word: &str) -> (&str, &str) {
    let after = |(i, c): (usize, char)| i + c.len_utf8();
    let end = word.char_indices().rev().find(|(_, c)| c.is_alphanumeric()).map_or(0, after);
    let start = word[..end].char_indices().rev().find(|(_, c)| !c.is_alphanumeric()).map_or(0, after);
    word.split_at(start)
}

/// One page for `req`, or [`SearchError::TooBroad`] / [`SearchError::Cancelled`]
/// once `budget` runs out. Every search surface comes through here, so the
/// caps and the budget hold for all of them.
pub fn search(store: &Store, req: &SearchRequest, budget: &Budget) -> Result<SearchPage, SearchError> {
    let limit = req.limit.clamp(1, MAX_PAGE);
    // A word with no letters or digits indexes to nothing: it cannot filter,
    // so it neither counts toward the cap nor is taken for the typed prefix.
    let typed: Vec<&str> = req.query.split_whitespace().filter(|w| tokens(w).next().is_some()).collect();
    let mut words: Vec<&str> = typed
        .iter()
        .copied()
        .filter(|w| tokens(w).count() <= MAX_WORD_TOKENS)
        .take(MAX_QUERY_WORDS)
        .collect();
    let words_dropped = typed.len() - words.len();

    // The last thing typed is still being typed only if it is a word the
    // page searches: in "git 🚀", "git" was finished. The same slice of the
    // query, not an equal word: twelve "git" and a thirteenth are not it.
    let typing = req.mode == MatchMode::PrefixLast
        && req.query.split_whitespace().last().map(str::as_ptr) == words.last().map(|w| w.as_ptr());
    let mut deferred = None;
    if typing && let Some(last) = words.pop() {
        // Gate on the last token, not the last word: "main.r" ends in the
        // one-character prefix `r*`, which costs as much as a bare `r*`.
        let (head, tail) = split_last_token(last);
        if tail.chars().filter(|c| c.is_alphanumeric()).count() >= MIN_PREFIX_CHARS {
            words.push(last);
        } else if tokens(head).next().is_some() {
            // What came before the short token was finished: "main", not "main*".
            words.push(head);
            deferred = Some(tail.to_string());
        } else {
            deferred = Some(last.to_string());
        }
    }
    let prefix = typing && deferred.is_none();

    let query = words.join(" ");
    // Ask for one extra row so the page can say whether it was cut off.
    let run = |store: &Store| match (words.is_empty(), &req.mode, req.order) {
        (true, MatchMode::Words, _) => Ok(vec![]),
        (true, MatchMode::PrefixLast, _) => store.recent(limit + 1),
        (false, _, Order::Rank) => store.search_ranked(&query, prefix, limit + 1),
        (false, _, Order::RecentlyImported) if prefix => store.search_prefix(&query, limit + 1),
        (false, _, Order::RecentlyImported) => store.search(&query, limit + 1),
    };
    let stop = {
        let budget = budget.clone();
        move || budget.spent()
    };
    let mut hits = store.interruptible(stop, run).map_err(|e| match e {
        e if !is_interrupt(&e) => SearchError::Store(e),
        _ if budget.cancelled() => SearchError::Cancelled,
        _ => SearchError::TooBroad,
    })?;
    let truncated = hits.len() > limit;
    hits.truncate(limit);
    Ok(SearchPage {
        schema: SEARCH_SCHEMA,
        query: req.query.clone(),
        limit,
        truncated,
        deferred,
        words_dropped,
        order: req.order,
        hits: hits
            .into_iter()
            .map(|h| PageHit {
                id: h.id,
                observed_at_ms: h.observed_at_ms,
                observed_before_ms: h.observed_before_ms,
                source: source_label(h.kind),
                text: h.safe_text,
            })
            .collect(),
    })
}

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
    let timeline =
        store.get_neighbors(observation.revision_id, observation.ordinal, context, context)?;
    let mut bounds = HashMap::new();
    for o in timeline.iter().chain([&observation]) {
        if o.observed_at_ms.is_none()
            && let Some(ms) = store.observed_no_later_than(o.revision_id, o.ordinal)?
        {
            bounds.insert(o.id, ms);
        }
    }
    let provenance = store.revision_provenance(observation.revision_id)?;
    Ok(Some(EvidenceView { observation, timeline, bounds, provenance }))
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
        // Soft hyphen, Mongolian vowel separator, prepended number marks,
        // and tag characters (invisible ASCII smuggling).
        '\u{00AD}' | '\u{180E}' | '\u{0600}'..='\u{0605}' | '\u{06DD}' | '\u{070F}' => '\u{FFFD}',
        '\u{E0001}' | '\u{E0020}'..='\u{E007F}' => '\u{FFFD}',
        c => c,
    }
}

/// When a command ran, as far as the source can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    At(i64),
    /// The source has no time for it, but a later timestamped command in the
    /// same file bounds it.
    NoLaterThan(i64),
    Unknown,
}

impl When {
    fn from_parts(at: Option<i64>, before: Option<i64>) -> Self {
        match (at, before) {
            (Some(ms), _) => Self::At(ms),
            (None, Some(ms)) => Self::NoLaterThan(ms),
            (None, None) => Self::Unknown,
        }
    }

    /// Fixed 17 columns for result lists. The first column marks an
    /// inferred bound, so exact and bounded dates line up.
    pub fn label(self) -> String {
        match self {
            Self::At(ms) => format!(" {}", format_utc(ms)),
            Self::NoLaterThan(ms) => format!("≤{}", format_utc(ms)),
            Self::Unknown => " unknown time    ".into(),
        }
    }

    /// Fixed 12 columns, month-day only, for the timeline beside the year.
    pub fn short(self) -> String {
        match self {
            Self::At(ms) => format!(" {}", &format_utc(ms)[5..]),
            Self::NoLaterThan(ms) => format!("≤{}", &format_utc(ms)[5..]),
            Self::Unknown => " unknown    ".into(),
        }
    }

    /// Spelled out, saying which part is inferred.
    pub fn detail(self) -> String {
        match self {
            Self::At(ms) => format!("{} UTC", format_utc(ms)),
            Self::NoLaterThan(ms) => format!(
                "not recorded; no later than {} UTC (inferred from history order)",
                format_utc(ms)
            ),
            Self::Unknown => "not recorded".into(),
        }
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
        SearchRequest { query: query.into(), limit, mode, order: Order::RecentlyImported }
    }

    fn search(store: &Store, req: &SearchRequest) -> Result<SearchPage, SearchError> {
        // Far enough away that a loaded machine never turns a test's query TooBroad.
        let budget = Budget { deadline: Instant::now() + Duration::from_secs(60), cancel: None };
        super::search(store, req, &budget)
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

        // Words before a deferred one match whole: "git" is not "gitk".
        let store2 = store_with(&["gitk --all", "git log"]);
        let page = search(&store2, &req("git c", 10, MatchMode::PrefixLast)).unwrap();
        let texts: Vec<&str> = page.hits.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, ["git log"]);
        // Punctuation does not count toward the prefix length.
        let page = search(&store2, &req("git -m", 10, MatchMode::PrefixLast)).unwrap();
        assert_eq!(page.deferred.as_deref(), Some("-m"));

        // Two characters filter. Words mode never defers.
        let page = search(&store, &req("git co", 10, MatchMode::PrefixLast)).unwrap();
        assert_eq!((page.hits.len(), page.deferred), (1, None));
        let page = search(&store, &req("git c", 10, MatchMode::Words)).unwrap();
        assert_eq!((page.hits.len(), page.deferred), (0, None));
    }

    fn texts(page: &SearchPage) -> Vec<&str> {
        page.hits.iter().map(|h| h.text.as_str()).collect()
    }

    #[test]
    fn every_mode_is_most_recently_imported_first_unless_rank_is_asked_for() {
        // BM25 would put the shortest row first; import order must not.
        let store = store_with(&["git", "git commit -m a long message here", "git push"]);
        let page = search(&store, &req("git", 10, MatchMode::Words)).unwrap();
        assert_eq!(texts(&page), ["git push", "git commit -m a long message here", "git"]);
        assert_eq!(serde_json::to_value(&page).unwrap()["order"], "recently_imported");

        for mode in [MatchMode::Words, MatchMode::PrefixLast] {
            let ranked = SearchRequest { order: Order::Rank, ..req("git", 10, mode) };
            let page = search(&store, &ranked).unwrap();
            assert_eq!(texts(&page), ["git", "git push", "git commit -m a long message here"]);
            assert_eq!(serde_json::to_value(&page).unwrap()["order"], "rank");
        }
    }

    #[test]
    fn prefix_gate_counts_the_last_token_not_the_last_word() {
        let store = store_with(&["vim src/main.rs", "vim mainline.txt", "ls"]);
        for (query, deferred) in [("main.r", "r"), ("src/main.r", "r"), ("main.r-", "r-")] {
            let page = search(&store, &req(query, 10, MatchMode::PrefixLast)).unwrap();
            assert_eq!(page.deferred.as_deref(), Some(deferred), "{query}");
            // What came before the short token was finished: "main", not "main*".
            assert_eq!(texts(&page), ["vim src/main.rs"], "{query}");
        }
        let page = search(&store, &req("main.rs", 10, MatchMode::PrefixLast)).unwrap();
        assert_eq!((texts(&page), page.deferred.as_deref()), (vec!["vim src/main.rs"], None));

        // A trailing symbol neither filters nor makes "git" a prefix.
        let store = store_with(&["gitk --all", "git log"]);
        let page = search(&store, &req("git 🚀", 10, MatchMode::PrefixLast)).unwrap();
        assert_eq!((texts(&page), page.deferred.as_deref()), (vec!["git log"], None));
    }

    #[test]
    fn words_past_either_cap_are_dropped_and_reported() {
        let store = store_with(&["git push", "ls"]);
        let mut words = vec!["git"; MAX_QUERY_WORDS];
        words.extend(["nomatch"; 3]);
        let page = search(&store, &req(&words.join(" "), 10, MatchMode::Words)).unwrap();
        assert_eq!((texts(&page), page.words_dropped), (vec!["git push"], 3));
        // A dropped last word was not the one being typed: "git" stays whole.
        let page = search(&store, &req(&words.join(" "), 10, MatchMode::PrefixLast)).unwrap();
        assert_eq!((texts(&page), page.words_dropped, page.deferred.as_deref()), (vec!["git push"], 3, None));

        // One word, but a 17-token phrase to FTS5.
        let phrase = vec!["nomatch"; MAX_WORD_TOKENS + 1].join("/");
        let page = search(&store, &req(&format!("git {phrase}"), 10, MatchMode::Words)).unwrap();
        assert_eq!((texts(&page), page.words_dropped), (vec!["git push"], 1));
        // A word typed after a dropped one still matches as it is typed.
        let page = search(&store, &req(&format!("git {phrase} pu"), 10, MatchMode::PrefixLast)).unwrap();
        assert_eq!((texts(&page), page.words_dropped), (vec!["git push"], 1));
        let path = vec!["nomatch"; MAX_WORD_TOKENS].join("/");
        let page = search(&store, &req(&format!("git {path}"), 10, MatchMode::Words)).unwrap();
        assert_eq!((page.hits.len(), page.words_dropped), (0, 0));

        // Symbols index to nothing, so they are no word to drop.
        let page = search(&store, &req("git -- 🚀", 10, MatchMode::Words)).unwrap();
        assert_eq!((texts(&page), page.words_dropped), (vec!["git push"], 0));
        assert_eq!(serde_json::to_value(&page).unwrap()["words_dropped"], 0);
    }

    #[test]
    fn a_spent_budget_stops_the_search_and_says_which_way() {
        // Enough rows that ranking them takes SQLite past its first poll.
        let lines: Vec<String> = (0..2_000).map(|i| format!("git commit {i}")).collect();
        let store = store_with(&lines.iter().map(String::as_str).collect::<Vec<_>>());
        let git = SearchRequest { order: Order::Rank, ..req("git", 10, MatchMode::Words) };
        let far = Instant::now() + Duration::from_secs(60);

        let cancel = Arc::new(AtomicBool::new(true));
        let err = super::search(&store, &git, &Budget { deadline: far, cancel: Some(cancel.clone()) }).unwrap_err();
        assert!(matches!(err, SearchError::Cancelled), "{err}");

        let err = super::search(&store, &git, &Budget { deadline: Instant::now(), cancel: None }).unwrap_err();
        assert!(matches!(err, SearchError::TooBroad), "{err}");
        assert!(err.to_string().contains("too broad"), "{err}");

        cancel.store(false, Ordering::Relaxed);
        let page = super::search(&store, &git, &Budget { deadline: far, cancel: Some(cancel.clone()) }).unwrap();
        assert_eq!(page.hits.len(), 10);

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
        for c in [
            '\u{061C}', '\u{200B}', '\u{200E}', '\u{202E}', '\u{2066}', '\u{FEFF}', '\u{FFF9}',
            '\u{00AD}', '\u{180E}', '\u{0600}', '\u{E0041}',
        ] {
            assert_eq!(display_line(&format!("a{c}b")), "a\u{FFFD}b", "{:?}", c);
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
    fn time_labels_keep_their_columns() {
        let ms = 1_791_017_948_000;
        for w in [When::At(ms), When::NoLaterThan(ms), When::Unknown] {
            assert_eq!(w.label().chars().count(), 17, "{w:?}");
            assert_eq!(w.short().chars().count(), 12, "{w:?}");
        }
        assert_eq!(When::NoLaterThan(ms).label(), "≤2026-10-03 08:59");
        assert_eq!(When::NoLaterThan(ms).short(), "≤10-03 08:59");
        assert!(When::NoLaterThan(ms).detail().contains("inferred"));
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
