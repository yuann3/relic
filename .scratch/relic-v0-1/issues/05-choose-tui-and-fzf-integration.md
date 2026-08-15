# Choose the TUI and fzf integration architecture

Status: open  
Assignee: tui-researcher  
Labels: wayfinder:research  
Parent: [Relic v0.1: ship the evidence-backed terminal experience compiler](../MAP.md)  
Blocked by: none

## Question

How should the CLI, Ratatui/Crossterm investigation workspace, SQLite FTS narrowing, optional external fzf selection, streaming model progress, and shared application core fit together without feeding the full corpus to fzf, harming headless startup, duplicating business logic, or making automated JSON use unreliable?

## Comments
