# Architecture decision records

The Relic v0.1 architecture lock, one decision per file; terms follow [CONTEXT.md](../../CONTEXT.md).

| ADR | Decision | Status |
|---|---|---|
| [0001](0001-one-synchronous-process.md) | Relic is one synchronous Rust process with a single-crate library core: no daemon, no Tokio | Accepted |
| [0002](0002-source-identity-and-revisions.md) | A source is its resolved path; rewrites anchor-align or supersede, and only forget, shell-kind correction and rescrub delete evidence | Accepted |
| [0003](0003-redaction-policy.md) | Precision-leaning structural denylist, typed idempotent markers, rescrub on revision change, owner-only files | Accepted |
| [0004](0004-egress-seam-and-privacy-canary.md) | Model adapters accept only a redacted, capped, ledgered `EgressRequest`; a privacy canary gates every egress path and stored field | Accepted |
| [0005](0005-evidence-store-transactions-and-read-only-open.md) | One transaction per import revision; readers open read-only and check `user_version` | Accepted |
| [0006](0006-search-ordering-and-query-budget.md) | Search is newest-first (rowid order) everywhere, `--rank` is opt-in, and every query runs under one budget (word cap 12) | Accepted |
| [0007](0007-gate-change-warm-fts-hot-prefixes.md) | Gate change: whole-word and indexed-prefix keystrokes p95 <20 ms; hot unindexed prefixes and sparse AND are deadline-bounded | Accepted |
| [0008](0008-gate-change-database-size.md) | Gate change: shrink the store, then restate the size gate as the measured ratio to raw source bytes | Accepted |
| [0009](0009-rlm-kernel.md) | The RLM kernel executes five operators, persists child verdicts for Select, checks child results strictly, and owns batching and cancellation | Accepted |
| [0010](0010-model-boundary.md) | One Chat Completions adapter over ureq; one retry rule; Jev is a conditional child-only slice; Codex is out | Accepted |
| [0011](0011-terminal-and-cli-contract.md) | The terminal is restored on every exit path, and exit codes follow grep's convention | Accepted |
| [0012](0012-validation-boundary.md) | Validation runs only a user-authored verifier; Relic never executes a recalled procedure | Accepted — provisional internals |
| [0013](0013-skill-catalog-direction.md) | Skills live in a separate content-addressed catalog and activate by symlink swap | Accepted — provisional internals |
| [0014](0014-performance-contract.md) | One committed gate harness measures every performance gate, and each gate has one owning slice | Accepted |
| [0015](0015-dependency-envelope.md) | Explicit allowed and forbidden crate lists; every addition shows `cargo bloat` and startup numbers | Accepted |
| [0016](0016-acceptance-fixture-and-reuse-gate.md) | Git reflog recovery is the acceptance fixture; reuse is gated on fewer model calls and egress bytes | Accepted |
