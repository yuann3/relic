# 0015. Hold v0.1 to explicit allowed and forbidden crate lists; every addition shows cargo bloat and startup numbers

Status: Accepted · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

- Current runtime dependencies (`Cargo.toml`):
  - rusqlite 0.35 (`bundled`, `modern_sqlite`), serde, serde_json, libc and tempfile;
  - ratatui 0.30 and crossterm 0.29, optional behind the default `tui` feature.
- In the library, tempfile appears only in tests and libc only in `src/ui/term.rs`. They are regular dependencies because bench `[[bin]]` targets use them outside tests: `perf_spike` (tempfile, `libc::getrusage`) and `fixture_db` (tempfile).
- Footprint ([Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1) gates, measured on macOS):
  - Stripped headless binary is 2.2 MiB ([Prove the interactive search workspace](https://github.com/yuann3/relic/issues/10)) or 2.5 MiB ([Prove the ingestion and search performance envelope](https://github.com/yuann3/relic/issues/8)), against an 8–12 MiB target. The full TUI build is 2.7 MiB.
  - Warm `--help` p95 is 1.7 ms against <15 ms. No committed harness reproduces the `--help` or TUI-size figures ([0014](0014-performance-contract.md)).
  - The ureq/rustls, blake3, aho-corasick and rusqlite `hooks` deltas have never been measured.
- The research envelope (`docs/research/storage-search-runtime-architecture.md:237-265`) lists possible crates: `toml`, `blake3`, `memchr`, `aho-corasick`/`regex-automata`, `getrandom`, `lexopt` or minimal `clap`, minimal-TLS `ureq`, and `thiserror` or manual enums. It requires `cargo tree`, `cargo bloat` and before/after startup numbers per dependency.
- Rejected already:
  - [Choose the storage, search, and runtime architecture](https://github.com/yuann3/relic/issues/4) rejected a daemon, a Tokio core, ORMs, git2, embeddings and scripting VMs.
  - [Specify the bounded RLM and model boundary](https://github.com/yuann3/relic/issues/5) deferred async reqwest unless blocking ureq fails the Ctrl-C gate (`docs/research/rlm-model-boundary.md:506`).
  - The map puts a provider framework out of scope.
- The Jev prototype found that a thin ureq+serde client (~80 LOC) suffices and the typesafe-ai crate is unnecessary (`docs/research/jev-prototype-findings.md:62-82`). ureq left `Cargo.toml` with the spike (17d7550).
- Agent Skill frontmatter is a few constrained strings plus a string→string `metadata` map.

## Decision

Allowed:
- `rusqlite` with `bundled`, `modern_sqlite` and `hooks`
- `serde`, `serde_json`, `libc`, `blake3`, `getrandom`
- `ureq` with `default-features = false` and `rustls` + `json`, for model commands only
- `ratatui` and `crossterm`, optional behind the `tui` feature

Allowed once measured: `memchr`, `aho-corasick` / `regex-automata`, `lexopt`, `toml`.

Forbidden:
- `tokio`, and `reqwest` unless ureq fails the Ctrl-C gate ([0010](0010-model-boundary.md))
- `git2`, ORMs, and `anyhow` in the domain
- embeddings and scripting VMs
- provider and agent SDKs, including `typesafe-ai`
- `clap`
- YAML crates; skill frontmatter uses a hand-written emitter

Every addition shows `cargo bloat` and startup numbers.

Making `tempfile` dev-only requires moving the bench bins. That happens with the gate-harness replacement ([0014](0014-performance-contract.md)), not as a drive-by.

`libc` stays an unconditional runtime dependency: `umask` in `main` ([0003](0003-redaction-policy.md)) and the `search --fzf` signal handler ([0011](0011-terminal-and-cli-contract.md)) run in the headless build.

## Amends

- `docs/research/storage-search-runtime-architecture.md:237-265` (under [Choose the storage, search, and runtime architecture](https://github.com/yuann3/relic/issues/4)): the dependency table becomes the lists above.
  - `clap` is forbidden instead of compared with `lexopt`.
  - `toml`, `memchr` and `aho-corasick` move to "allowed once measured".
  - `thiserror` is not listed, so adding it is an addition under the measurement rule.
  - The rusqlite `hooks` feature is added for the query budget's progress handler ([0006](0006-search-ordering-and-query-budget.md)).

## Consequences

- A reviewer checks a dependency change against the lists above. Every crate added to `Cargo.toml`, allowed ones included, lands with `cargo bloat` and startup numbers in the same change.
- Measurement ownership ([0014](0014-performance-contract.md)):
  - S1b measures the redaction binary delta.
  - S2 measures the stripped ureq/rustls delta.
- ureq is initialized only on model-command paths (recall, `relic model check`). Import, search and `--help` never touch it.
- Frontmatter is emitted by hand; the skill compiler ([0013](0013-skill-catalog-direction.md)) needs no YAML crate.
- The current CLI parser stays hand-rolled unless `lexopt` earns its place with numbers.

## Rejected alternatives

- Principles only, with no fixed list: dependencies would arrive slice by slice with no shared reference, and reviewers would have nothing concrete to check.
- The allowlist plus `clap` and a YAML crate for convenience: adds startup and size cost against the <15 ms help gate, for frontmatter that is only a few strings.
- `typesafe-ai` or another provider SDK: a thin ureq client is enough.
- `reqwest`/Tokio by default: only the fallback if ureq fails the Ctrl-C gate.
- `libc` behind a feature: the headless build needs it for `umask` in `main` and the `search --fzf` signal handler.

## Open facts

- Binary-size and startup deltas for `blake3`, `getrandom`, ureq+rustls and the rusqlite `hooks` feature.
