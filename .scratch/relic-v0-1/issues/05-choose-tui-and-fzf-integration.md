# Choose the TUI and fzf integration architecture

Status: closed  
Assignee: tui-researcher-2  
Labels: wayfinder:research  
Parent: [Relic v0.1: ship the evidence-backed terminal experience compiler](../MAP.md)  
Blocked by: none

## Question

How should the CLI, Ratatui/Crossterm investigation workspace, SQLite FTS narrowing, optional external fzf selection, streaming model progress, and shared application core fit together without feeding the full corpus to fzf, harming headless startup, duplicating business logic, or making automated JSON use unreliable?

## Comments

### Resolution — 2026-08-16

Research is captured in [TUI and fzf integration architecture for Relic v0.1](../../../docs/research/tui-fzf-integration.md). One typed application core serves complete CLI/JSON and a feature-gated Ratatui TUI using a pure state/update/effect boundary and bounded blocking workers. SQLite FTS5 always narrows first; optional external fzf receives at most a bounded immutable search page over a NUL-delimited fixed-argv protocol, with no preview/execute shell commands. Running fzf from the TUI requires fully restoring/pausing terminal ownership and must pass PTY tests; CLI-only fzf is the honest fallback. No Tokio, agent graph, chat UI, token-partial JSON, or unconditional render loop.
