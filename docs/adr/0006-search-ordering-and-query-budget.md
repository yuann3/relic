# 0006. Search is newest-first everywhere, and every query runs under one budget

Status: Accepted · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

- BM25 `rank` costs about 0.5 µs per match: 33k matches take 16 ms, 108k take 52 ms, 475k take 260 ms. Hot single terms took 42–74 ms at 1M in [Prove the ingestion and search performance envelope](https://github.com/yuann3/relic/issues/8). Under `ORDER BY rowid DESC LIMIT n`, FTS5 stops early: any whole word costs 0.03–0.06 ms p95 at 1M, including `task` at 93% of rows (docs/research/interactive-fts-measurements.md:16-18, 79-80).
- Only the TUI and `--prefix` are newest-first today. Default `relic search`, including `--json` and `--fzf`, orders by rank (src/app.rs:81, src/store/mod.rs:236-240), so `relic search git` at 1M still fails the map's warm-FTS gate.
- The storage contract SQL in [Choose the storage, search, and runtime architecture](https://github.com/yuann3/relic/issues/4) is `ORDER BY score, o.id DESC` (docs/research/storage-search-runtime-architecture.md:119-127). [Prove the interactive search workspace](https://github.com/yuann3/relic/issues/10) kept rank available for explicit searches.
- rowid order is import order, not command time. A backfilled older history sorts above newer commands. Ordering by `observed_at_ms` loses the FTS early exit and was never measured (docs/research/interactive-fts-measurements.md:312).
- [\[D1\] Fix the e2e dogfood findings from the workspace prototype](https://github.com/yuann3/relic/issues/22) found hot-token phrase queries up to 1.2 s that cannot be cancelled, and no word cap. Rank over repeated words grows superlinearly (100×`git` 0.58 s, 800× 8 s, per a comment on the unreviewed `fix/store-read-path` branch). Master has no word cap, no progress handler and no rusqlite `hooks` feature (src/store/mod.rs:356-369, Cargo.toml:15).
- One word can tokenize into a long phrase: `"task/"×50 + "ta"*` is a single 51-token word, so a word cap alone does not bound it.
- [Specify the bounded RLM and model boundary](https://github.com/yuann3/relic/issues/5) caps the RLM `search` operator at 12 terms (docs/research/rlm-model-boundary.md:141).
- Unproven: whether SQLite's progress handler fires inside a single FTS5 prefix merge or phrase scan. The WIP `examples/probe.rs` that would show it was never run into a committed record.

## Decision

Ordering:
- The default order on every search surface (TUI, `relic search` incl. `--fzf`, and `--json`) is `ORDER BY rowid DESC`.
- That order is labelled "recently imported" (rowid order), never "newest by time". *Recently imported* and *Observed at* (command time) are distinct glossary terms.
- Rank is opt-in through `--rank`, ordered `rank, rowid DESC`, and always runs under the query budget.
- True time ordering is deferred until measured.
- No reranker exists in v0.1, and reranking never enters `app::search` ([0010](0010-model-boundary.md)).

Query budget:
- One word cap, `12`, shared by the CLI query compiler and the RLM `search` operator.
- A tokens-per-word cap, which bounds a single word that tokenizes into a long phrase.
- Every query runs under a progress-handler deadline plus a cancel flag (rusqlite `hooks` feature). A stopped query returns `TooBroad` or `Cancelled`.
- Both caps are reported in the JSON page (`words_dropped`).

## Amends

- The storage contract's search SQL (docs/research/storage-search-runtime-architecture.md:119-127): the default changes from `ORDER BY score, o.id DESC` to `ORDER BY rowid DESC`. Rank becomes the opt-in `--rank` mode, ordered `rank, rowid DESC`, always under the budget.

## Consequences

- S1c (read path) owns all of this, together with the read-only open ([0005](0005-evidence-store-transactions-and-read-only-open.md)).
- CLI tests pin that default `relic search --json` returns hits in descending rowid order, that `--rank` orders by `rank, rowid DESC`, and that a query over 12 words reports `words_dropped`.
- Acceptance proof: a 1M-fixture bench in the gate harness ([0014](0014-performance-contract.md)) in which the single-word 51-token hot phrase and a 400×`git` `--rank` query each return `TooBroad` or `Cancelled` within the budget plus a stated epsilon.
- D1's 1.2 s finding stays open until that bench passes.
- If the interrupt does not land inside a single FTS5 merge, the worst case is the longest single merge (about 20–40 ms at 1M, linear in history size). S1c records which holds.
- The deadline is what bounds unindexed hot-prefix and sparse-AND keystrokes under the amended warm-FTS gate ([0007](0007-gate-change-warm-fts-hot-prefixes.md)).
- Exit codes for no match, error and cancel follow [0011](0011-terminal-and-cli-contract.md).

## Rejected alternatives

- Newest-first for interactive search only, with CLI and JSON keeping unbounded rank: `relic search git` at 1M keeps failing the hard warm-FTS gate.
- Rank inside a rowid recency window (V5, newest 10%): leaves `c*` at 60 ms and `git c*` at 41 ms, and the window means something different on a multi-year history (docs/research/interactive-fts-measurements.md:168). Rank over the newest K matches was never measured.
- Keep rank and narrow the gate to single words under about 35k matches: excludes ordinary keystrokes (`git c*` 47 ms, `cargo re*` 30 ms), so it cannot serve search-as-you-type, and it relaxes a hard gate.
- Order by `observed_at_ms`: loses the FTS early exit; unmeasured.
- Word cap only: leaves the single 51-token word, and so the 1.2 s case, unbounded.
- A worker that interrupts the connection on supersession: the same uncertainty about where interrupts land, plus thread coordination.
- The WIP's 16-word cap: a second limit beside the RLM operator's 12 for one query compiler.

## Open facts

- Whether the progress-handler interrupt lands inside a single FTS5 prefix merge or phrase scan at 1M (S1c).
- The tokens-per-word cap value, and the deadline value with its epsilon; the S1c bench sets and proves both.
