# Choose the storage, search, and runtime architecture

Status: open  
Assignee: performance-architect  
Labels: wayfinder:research  
Parent: [Relic v0.1: ship the evidence-backed terminal experience compiler](../MAP.md)  
Blocked by: none

## Question

Which minimal Rust runtime, SQLite/FTS5 schema and pragmas, redaction pipeline, incremental import strategy, Git boundary, concurrency model, and dependency set can meet Relic's hard latency, throughput, memory, database-size, and binary-size gates? Compare plausible alternatives using primary documentation and source-level evidence, and identify what must be proven by prototype rather than assumed.

## Comments
