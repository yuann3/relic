# 0001. Relic is one synchronous Rust process with a single-crate library core

Status: Accepted · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

[Choose the storage, search, and runtime architecture](https://github.com/yuann3/relic/issues/4) ([resolution](https://github.com/yuann3/relic/issues/4#issuecomment-5306831579)) chose this runtime. The research is in `docs/research/storage-search-runtime-architecture.md`. Its argument (:28-41) is that Relic's operations are short, local and serial at their authority boundary. There is one SQLite writer, search is a blocking local call, and the source check is a blocking stat. Git runs as a bounded subprocess after narrowing, and model latency dwarfs any executor choice. Tokio adds dependencies, binary size and startup cost without lifting SQLite's single-writer limit. A daemon adds lifecycle, recovery and idle footprint, and it makes `search` depend on service health.

[Prove the ingestion and search performance envelope](https://github.com/yuann3/relic/issues/8) measured the one-process rusqlite/FTS5 core as viable. The import, incremental, unchanged-check, selective-FTS and binary-size gates passed. The stripped headless binary is 2.5 MiB against the 8–12 MiB target (2.2 MiB in [Prove the interactive search workspace](https://github.com/yuann3/relic/issues/10)). The gates that failed (high-frequency-term FTS, DB size, RSS) are handled in [0007](0007-gate-change-warm-fts-hot-prefixes.md), [0008](0008-gate-change-database-size.md) and [0002](0002-source-identity-and-revisions.md). None of them calls for a different runtime.

This ADR restates that decision; it does not reopen it.

## Decision

- Each invocation is one synchronous process. There is no daemon, index service or async runtime in the core.
- One Cargo package: a library core, one CLI binary, and the TUI behind an optional `tui` feature (`storage-search-runtime-architecture.md:212-235`). No workspace split. The bounded contexts are top-level modules of this crate, mapped in `CONTEXT.md`. No new traits beyond the two polymorphic seams, source adapter and model adapter (:235).
- Ordinary threads appear only where work is independent and blocking: depth-one child model calls and the TUI's terminal and search workers. Every queue is bounded (:39).
- Storage is bundled SQLite/FTS5 through `rusqlite`, with one transactional writer, WAL, and FTS rows written explicitly in the same transaction as base rows (no triggers). Transaction and open rules are in [0005](0005-evidence-store-transactions-and-read-only-open.md).
- Git is the installed `git` executable, run with fixed argv, no shell, bounded output and a timeout. Never libgit2.
- Forbidden in v0.1: daemon, Tokio core, ORM/SQLx/async SQL/connection pool, embeddings or vector store, `git2`, scripting VM (Rhai, Python, JS, WASM), plugin system, provider framework. The crate allowlist is in [0015](0015-dependency-envelope.md).
- Tokio and `reqwest` can enter only through the failure rule in [0010](0010-model-boundary.md): if `ureq` fails the Ctrl-C gate.

## Consequences

- No slice may add a background service, an async executor or a second crate. Changing that requires a superseding ADR.
- Cancellation is cooperative and synchronous: an `AtomicBool` plus the SQLite progress handler ([0006](0006-search-ordering-and-query-budget.md), [0009](0009-rlm-kernel.md)).
- Startup, RSS and binary-size gates are measured against this single binary by the harness in [0014](0014-performance-contract.md).

## Rejected alternatives

- Tokio/async core: there is no asynchronous local bottleneck, and the cost comes before any evidence of need.
- Daemon or index service: brings service lifecycle and idle footprint, and the one-shot gates pass without it.
- SQLx, ORM or pool: an abstraction over one local connection, with extra compile and runtime surface.
- Tantivy or another index: duplicates SQLite's metadata and transaction boundary. FTS5 meets the selective gates.
- Embeddings or vector DB: privacy, download and index cost, and weak deterministic evidence. Lexical narrowing plus the RLM suffices.
- `git2`/libgit2: a large native surface whose behaviour and config diverge from the installed Git.
- Scripting VM: arbitrary execution and dependency cost. Typed `PlanV1` suffices.
- A Cargo workspace with one crate per context: contradicts the decision for a single package and adds build and abstraction overhead before any second caller exists.
