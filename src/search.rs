//! Search: one page of observations for a query, under the word caps and
//! the query budget every surface shares (ADR 0006).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::domain::When;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// What to search for. It borrows the query: the page owns its copy, and
/// a caller can run one request again.
#[derive(Debug, Clone, Copy)]
pub struct SearchRequest<'q> {
    pub query: &'q str,
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
        Self {
            deadline: Instant::now() + QUERY_DEADLINE,
            cancel: None,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
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
    /// A word was searched. When none was, the hits are not matches: they
    /// are the most recently imported commands a typing surface shows.
    #[serde(skip)]
    pub filtered: bool,
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

/// What the unicode61 tokenizer keeps inside one token: letters, digits, and
/// the combining diacritics it folds away, so a decomposed "é" is one
/// character of its word and not a break in it.
fn in_token(c: char) -> bool {
    c.is_alphanumeric() || ('\u{0300}'..='\u{036F}').contains(&c)
}

/// The tokens FTS5 makes of `word`: "-m" has one, "src/main.rs" three.
fn tokens(word: &str) -> impl Iterator<Item = &str> {
    word.split(|c| !in_token(c))
        .filter(|t| t.chars().any(char::is_alphanumeric))
}

/// Splits `word` before its last token: "src/main.r" is ("src/main.", "r").
fn split_last_token(word: &str) -> (&str, &str) {
    let after = |(i, c): (usize, char)| i + c.len_utf8();
    let end = word
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphanumeric())
        .map_or(0, after);
    let start = word[..end]
        .char_indices()
        .rev()
        .find(|(_, c)| !in_token(*c))
        .map_or(0, after);
    word.split_at(start)
}

/// One page for `req`, or [`SearchError::TooBroad`] / [`SearchError::Cancelled`]
/// once `budget` runs out. Every search surface comes through here, so the
/// caps and the budget hold for all of them.
pub fn search(
    store: &Store,
    req: &SearchRequest,
    budget: &Budget,
) -> Result<SearchPage, SearchError> {
    let limit = req.limit.clamp(1, MAX_PAGE);
    // A word with no letters or digits indexes to nothing: it cannot filter,
    // so it neither counts toward the cap nor is taken for the typed prefix.
    let typed: Vec<&str> = req
        .query
        .split_whitespace()
        .filter(|w| tokens(w).next().is_some())
        .collect();
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
    // Symbols alone index to nothing. The page still shows the most recent
    // commands as the user types them, so it must say they are not matches.
    if req.mode == MatchMode::PrefixLast
        && words.is_empty()
        && words_dropped == 0
        && deferred.is_none()
    {
        deferred = req.query.split_whitespace().last().map(String::from);
    }
    let prefix = typing && deferred.is_none();

    let query = words.join(" ");
    // Ask for one extra row so the page can say whether it was cut off.
    // With nothing to filter by yet, a typing surface shows the most recent
    // commands. Not when words were typed and dropped: those rows would pass
    // for matches of a query that was never run.
    let unfiltered = words.is_empty() && req.mode == MatchMode::PrefixLast && words_dropped == 0;
    let order = if unfiltered {
        Order::RecentlyImported
    } else {
        req.order
    };
    let run = |store: &Store| match (words.is_empty(), order) {
        (true, _) if unfiltered => store.recent(limit + 1),
        (true, _) => Ok(vec![]),
        (false, Order::Rank) => store.search_ranked(&query, prefix, limit + 1),
        (false, Order::RecentlyImported) if prefix => store.search_prefix(&query, limit + 1),
        (false, Order::RecentlyImported) => store.search(&query, limit + 1),
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
        query: req.query.to_owned(),
        limit,
        truncated,
        deferred,
        words_dropped,
        order,
        filtered: !words.is_empty(),
        hits: hits
            .into_iter()
            .map(|h| PageHit {
                id: h.id,
                observed_at_ms: h.observed_at_ms,
                observed_before_ms: h.observed_before_ms,
                source: h.kind.source_label(),
                text: h.safe_text,
            })
            .collect(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::SourceKind;
    use crate::import::import_file;
    use crate::redact::Redactor;
    use std::io::Write;

    pub(crate) fn store_with(lines: &[&str]) -> Store {
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

    pub(crate) fn req(query: &str, limit: usize, mode: MatchMode) -> SearchRequest<'_> {
        SearchRequest {
            query,
            limit,
            mode,
            order: Order::RecentlyImported,
        }
    }

    pub(crate) fn search(store: &Store, req: &SearchRequest) -> Result<SearchPage, SearchError> {
        // Far enough away that a loaded machine never turns a test's query TooBroad.
        let budget = Budget {
            deadline: Instant::now() + Duration::from_secs(60),
            cancel: None,
        };
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
        assert!(
            search(&store, &req("kube", 10, MatchMode::Words))
                .unwrap()
                .hits
                .is_empty()
        );
        let page = search(&store, &req("kube", 10, MatchMode::PrefixLast)).unwrap();
        assert_eq!(page.hits[0].text, "kubectl get pods");
        assert_eq!(page.hits[0].source, "zsh");
    }

    #[test]
    fn prefix_mode_is_newest_first() {
        let store = store_with(&["git", "git commit -m a long message here", "git push"]);
        let page = search(&store, &req("gi", 10, MatchMode::PrefixLast)).unwrap();
        let texts: Vec<&str> = page.hits.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(
            texts,
            ["git push", "git commit -m a long message here", "git"]
        );
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
        assert_eq!(
            texts(&page),
            ["git push", "git commit -m a long message here", "git"]
        );
        assert_eq!(
            serde_json::to_value(&page).unwrap()["order"],
            "recently_imported"
        );

        for mode in [MatchMode::Words, MatchMode::PrefixLast] {
            let ranked = SearchRequest {
                order: Order::Rank,
                ..req("git", 10, mode)
            };
            let page = search(&store, &ranked).unwrap();
            assert_eq!(
                texts(&page),
                ["git", "git push", "git commit -m a long message here"]
            );
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
        assert_eq!(
            (texts(&page), page.deferred.as_deref()),
            (vec!["vim src/main.rs"], None)
        );

        // A decomposed accent is inside its word, as FTS5 has it: "cafés"
        // typed as e + U+0301 does not end in a one-letter "s".
        let store = store_with(&["echo cafe\u{301}s", "echo cafe"]);
        let page = search(&store, &req("cafe\u{301}s", 10, MatchMode::PrefixLast)).unwrap();
        assert_eq!(
            (texts(&page), page.deferred.as_deref()),
            (vec!["echo cafe\u{301}s"], None)
        );

        // Nor does a word of symbols: it filters nothing, and the page says so.
        let page = search(&store, &req("&&", 10, MatchMode::PrefixLast)).unwrap();
        assert_eq!((page.hits.len(), page.deferred.as_deref()), (2, Some("&&")));

        // The unfiltered page a short word gets is never a ranked one.
        let ranked = SearchRequest {
            order: Order::Rank,
            ..req("g", 10, MatchMode::PrefixLast)
        };
        assert_eq!(
            search(&store, &ranked).unwrap().order,
            Order::RecentlyImported
        );

        // A trailing symbol neither filters nor makes "git" a prefix.
        let store = store_with(&["gitk --all", "git log"]);
        let page = search(&store, &req("git 🚀", 10, MatchMode::PrefixLast)).unwrap();
        assert_eq!(
            (texts(&page), page.deferred.as_deref()),
            (vec!["git log"], None)
        );
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
        assert_eq!(
            (texts(&page), page.words_dropped, page.deferred.as_deref()),
            (vec!["git push"], 3, None)
        );

        // One word, but a 17-token phrase to FTS5.
        let phrase = vec!["nomatch"; MAX_WORD_TOKENS + 1].join("/");
        let page = search(&store, &req(&format!("git {phrase}"), 10, MatchMode::Words)).unwrap();
        assert_eq!((texts(&page), page.words_dropped), (vec!["git push"], 1));
        // A word typed after a dropped one still matches as it is typed.
        let page = search(
            &store,
            &req(&format!("git {phrase} pu"), 10, MatchMode::PrefixLast),
        )
        .unwrap();
        assert_eq!((texts(&page), page.words_dropped), (vec!["git push"], 1));
        let path = vec!["nomatch"; MAX_WORD_TOKENS].join("/");
        let page = search(&store, &req(&format!("git {path}"), 10, MatchMode::Words)).unwrap();
        assert_eq!((page.hits.len(), page.words_dropped), (0, 0));

        // When every word was dropped, the most recent commands are not its matches.
        let page = search(&store, &req(&phrase, 10, MatchMode::PrefixLast)).unwrap();
        assert_eq!((page.hits.len(), page.words_dropped), (0, 1));

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
        let git = SearchRequest {
            order: Order::Rank,
            ..req("git", 10, MatchMode::Words)
        };
        let far = Instant::now() + Duration::from_secs(60);

        let cancel = Arc::new(AtomicBool::new(true));
        let err = super::search(
            &store,
            &git,
            &Budget {
                deadline: far,
                cancel: Some(cancel.clone()),
            },
        )
        .unwrap_err();
        assert!(matches!(err, SearchError::Cancelled), "{err}");

        let err = super::search(
            &store,
            &git,
            &Budget {
                deadline: Instant::now(),
                cancel: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, SearchError::TooBroad), "{err}");
        assert!(err.to_string().contains("too broad"), "{err}");

        cancel.store(false, Ordering::Relaxed);
        let page = super::search(
            &store,
            &git,
            &Budget {
                deadline: far,
                cancel: Some(cancel),
            },
        )
        .unwrap();
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
}
