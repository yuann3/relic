# Choose the storage, search, and runtime architecture

Status: closed  
Assignee: performance-architect-2  
Labels: wayfinder:research  
Parent: [Relic v0.1: ship the evidence-backed terminal experience compiler](../MAP.md)  
Blocked by: none

## Question

Which minimal Rust runtime, SQLite/FTS5 schema and pragmas, redaction pipeline, incremental import strategy, Git boundary, concurrency model, and dependency set can meet Relic's hard latency, throughput, memory, database-size, and binary-size gates? Compare plausible alternatives using primary documentation and source-level evidence, and identify what must be proven by prototype rather than assumed.

## Comments

### Resolution — 2026-08-16

Research is captured in [Storage, search, and runtime architecture for Relic v0.1](../../../docs/research/storage-search-runtime-architecture.md). Use a synchronous one-process Rust core, bundled SQLite/FTS5 through rusqlite, compact integer rows plus an external-content sanitized-text index, streaming byte parsing and compiled redaction, one transactional writer, bounded fixed-argv Git subprocesses, and ordinary threads only for depth-one model children. No daemon, Tokio core, ORM, git2, embeddings, scripting VM, or provider framework. Pragmas, tokenizer behavior, batches, mmap/cache, dependency features, and all stated performance numbers remain hypotheses gated by the executable performance prototype.
