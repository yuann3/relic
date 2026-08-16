# Specify source and evidence contracts

Status: closed  
Assignee: source-researcher-2  
Labels: wayfinder:research  
Parent: [Relic v0.1: ship the evidence-backed terminal experience compiler](../MAP.md)  
Blocked by: none

## Question

What exact byte-level parsers, incremental identities/cursors, normalization rules, hook protocols, and evidence-quality semantics should Relic v0.1 use for native zsh/Bash history, metadata hooks, Git correlation, Prime Agent traces, and Claude Code traces on macOS/Linux—without persisting raw artifacts or inventing missing facts?

## Comments

### Resolution — 2026-08-16

Research is captured in [Source and evidence contracts for Relic v0.1](../../../docs/research/source-evidence-contracts.md). Relic will stream approved external artifacts through versioned byte parsers, redact before all persistence/egress, store immutable sanitized observations with per-field provenance, and use guarded revision/cursor contracts without inventing cwd, success, output, chronology, or session facts. Zsh and Bash retain different evidence guarantees; hooks are metadata-only and asymmetric; Git is enrichment; Prime/Claude adapters are version-pinned implementation formats with fail-closed compatibility tests.
