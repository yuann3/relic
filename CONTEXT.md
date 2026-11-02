# Relic context

Relic imports local experience artifacts: shell history, shell hooks, Git and agent traces. It stores them as redacted observations, searches them offline, and recalls an evidence-cited procedure through a bounded RLM. It can then validate that procedure with a user-authored verifier, compile it into an Agent Skill and activate it manually. This file maps the bounded contexts and fixes the ubiquitous language. The decisions themselves live in the ADRs under [docs/adr/](docs/adr/). Map: [Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1).

## Rules

- One Cargo package. Each context is a top-level module, with no workspace split ([0001](docs/adr/0001-one-synchronous-process.md)).
- No new traits beyond the two polymorphic seams: the source adapter and the model adapter.
- Code moves into its context's module only when a slice touches it. No drive-by moves.
- Reserved module names: `model/`, `validation/`, `catalog/`.
- Cross-cutting ADRs: [0001](docs/adr/0001-one-synchronous-process.md) (one process), [0014](docs/adr/0014-performance-contract.md) (performance), [0015](docs/adr/0015-dependency-envelope.md) (dependencies).

## Bounded contexts

| Context | Responsibility | Current modules | ADRs |
|---|---|---|---|
| Source Ingestion | Reads sources as byte streams. Owns source identity, change detection, revisions, anchor alignment, shell kind and atomic import. Source adapters are one of the two polymorphic seams. | `src/source/{mod,zsh,bash,git}.rs`, `src/import.rs` | [0002](docs/adr/0002-source-identity-and-revisions.md), [0005](docs/adr/0005-evidence-store-transactions-and-read-only-open.md) |
| Redaction & Identity | Owns the redaction policy, typed markers, `redaction_revision`, rescrub, file permissions and the installation key. | `src/redact.rs`, `src/key.rs` | [0003](docs/adr/0003-redaction-policy.md), [0002](docs/adr/0002-source-identity-and-revisions.md), [0004](docs/adr/0004-egress-seam-and-privacy-canary.md) |
| Evidence Store | Owns the SQLite schema, observations, the FTS5 index, revisions, transactions, read-only open, `user_version` and size. | `src/store/{mod,schema}.rs`, `src/domain.rs` | [0005](docs/adr/0005-evidence-store-transactions-and-read-only-open.md), [0008](docs/adr/0008-gate-change-database-size.md) |
| Search | Owns query compilation, recently-imported-first ordering, opt-in `--rank`, the query budget, hot prefixes and the fzf picker. | query methods in `src/store/mod.rs`; `search` and `SearchPage` in `src/app.rs`; `src/picker.rs` | [0006](docs/adr/0006-search-ordering-and-query-budget.md), [0007](docs/adr/0007-gate-change-warm-fts-hot-prefixes.md) |
| Recall | Owns the RLM kernel: plan admission, five executed operators, evidence handles, the budget ledger, the finish gate, cancellation and progress. | `src/rlm/{protocol,validate,evidence,budget,executor,coordinator,finish}.rs` | [0009](docs/adr/0009-rlm-kernel.md) |
| Model Boundary | Owns the one Chat Completions adapter, `EgressRequest`, retries, pinning, fences and `relic model check`. Model adapters are the second polymorphic seam. | `src/rlm/model/` (`ModelBoundary`, `FakeModel`; `egress.rs` the `EgressRequest`, `prompt.rs` instructions and schemas, `chat.rs` the ureq client and `model check`), `src/rlm/calls.rs` (admission, fences, retry, the child pool) | [0010](docs/adr/0010-model-boundary.md), [0004](docs/adr/0004-egress-seam-and-privacy-canary.md) |
| Validation | Owns verifier runs and validation records. | none yet (`validation/` reserved) | [0012](docs/adr/0012-validation-boundary.md) |
| Skill Catalog | Owns skill compilation, CAS bundles, `catalog.db`, activation and rollback. | none yet (`catalog/` reserved) | [0013](docs/adr/0013-skill-catalog-direction.md) |
| Reuse Evaluation | Owns the acceptance fixture, the seeded investigations, the reuse gate and the gate harness. | `src/fixture.rs`, `benches/{gates,fixture_db,interactive_fts}.rs` | [0016](docs/adr/0016-acceptance-fixture-and-reuse-gate.md), [0014](docs/adr/0014-performance-contract.md) |
| Presentation | Owns the CLI and versioned JSON, the TUI, the terminal, signals and exit codes. | `src/main.rs`, `src/cli.rs`, `src/app.rs` (the use-case layer shared by CLI and TUI), `src/signal.rs` (fatal signals for the TUI and `search --fzf`), `src/ui/{mod,model,view,term,recall}.rs` | [0011](docs/adr/0011-terminal-and-cli-contract.md) |

## Context map

| Context | Depends on |
|---|---|
| Source Ingestion | Redaction & Identity (redact before store), Evidence Store (one transaction per revision) |
| Redaction & Identity | nothing |
| Evidence Store | nothing |
| Search | Evidence Store (read-only open) |
| Recall | Evidence Store (read-only), Search (shared query budget and word cap), Model Boundary |
| Model Boundary | Redaction & Identity (an `EgressRequest` exists only after the redactor has run) |
| Validation | Recall (procedure and citations), Redaction & Identity (verifier argv redacted before storage) |
| Skill Catalog | Recall (procedure), Validation (validation records), Evidence Store (durable citations) |
| Reuse Evaluation | every context above it. It drives the full path and reads Skill Catalog metadata for the pre-recall host step. |
| Presentation | Source Ingestion, Search, Recall, Model Boundary, Validation, Skill Catalog, all through `src/app.rs` |

The pre-recall skill match is a host step that runs before Recall. It is not part of Recall's operator kernel ([0016](docs/adr/0016-acceptance-fixture-and-reuse-gate.md)).

## Glossary

One canonical term per concept. Use the term, not the words in the Avoid column.

| Term | Meaning | Avoid |
|---|---|---|
| Source | One history file, hook stream, Git repo or agent trace that Relic imports. Its stored identity is a random `logical_id`; it is looked up by its path with parent directories resolved, which stays in the private locator. | file (as identity) |
| Source revision | An ordered run of observations from one source. Appends and anchor-aligned rewrites extend it; a rewrite with no unique anchor, or a rescrub, starts a new one. | snapshot, version |
| Observation | A stored, redacted row taken from a source revision. | evidence (for the row), event, entry |
| Safe text | The redacted text that is stored for an observation. It is the only text that reaches SQLite, FTS, logs or a model. | raw text, command (for the stored form) |
| Redaction marker | `[REDACTED:<kind>]`. A well-formed marker is never redacted again. A user-typed look-alike is escaped at import. | `[REDACTED]` (untyped) |
| Forget | The explicit, logged purge of evidence (`relic forget`). Removing a line from a history file never removes it from Relic. | delete, remove, prune |
| Rescrub | Re-redacting stored safe text into a new revision when `redaction_revision` changes. The old revision is purged and citations to it go stale. | re-import, migration |
| Installation key | 32 random bytes in a `0600` file beside the DB. It keys only the prefix digest and its boundary-guard fallback; never source identity, a per-record hash or an evidence handle ([0002](docs/adr/0002-source-identity-and-revisions.md)). Losing it is harmless. | secret key, master key |
| Recently imported | Rowid order, the default order on every search surface. Label it as import order. | newest (unlabelled), chronological |
| Observed at | When the command ran, according to the source: either an exact time or an inferred bound (`When`). For a Git reflog entry, when Git recorded the ref move; for a Git ref, when Relic imported the repository and saw it there. | timestamp (unqualified), date |
| Investigation | One bounded RLM recall run. Evidence handles are scoped to it. | session, query |
| Recall log | `<db>.recall.log`, an owner-only JSONL file beside the store: each recall's start, every request sent (the payload audit without its preview), replies, retries and its finish, durably synced. Metadata and record ids only, so `forget` leaves no evidence text behind in it ([S12](https://github.com/yuann3/relic/issues/37)). | audit trail, trace |
| Plan admission | `src/rlm/validate.rs`: the check that a `PlanV1` is well-formed and within its fences before anything executes. Relate and Combine are rejected here. | plan validation, validate |
| Finish gate | `src/rlm/finish.rs`: every final claim and procedure step must cite evidence that resolves. | finish validation, verify |
| Handle integrity check | `verify_handle`: the check that an evidence handle is genuine, current and belongs to this investigation. | verify, verification, validation |
| Evidence handle | A random ID that can be cited, scoped to one investigation. Invalid in any other investigation. | evidence ID in skills, citation (for durable use) |
| Durable citation | Revision + ordinal + content hash. Skills cite evidence this way. | evidence ID, handle |
| Budget lease | An RLM budget reservation. Used only in Recall. | lease (anywhere else) |
| Validation | A run of a user-approved verifier, and nothing else. Relic never runs the procedure itself. | plan or finish validation, verify |
| Verifier | A fixed argv written by the user, run with no shell, in a cwd the user chooses, with bounded output and time. | test, check script, replay |
| Validation record | `{ redacted argv, cwd, exit status, duration, Relic version, workspace assurance label }`, stored as `ConfirmedByValidation`. A user's word alone is `UserAsserted`, a lower grade. | validation result, attestation |
| Workspace | The directory where a verifier runs, with an assurance label. | sandbox |
| Skill bundle | Immutable content-addressed skill content (provisional layout `<hash>/<name>/SKILL.md`, [0013](docs/adr/0013-skill-catalog-direction.md)). | candidate, skill version, artifact |
| Skill name | The stable pointer to the active bundle. | alias, slug |
| Activation | An explicit, confirmed pointer swap that makes a bundle live. Provisionally a symlink swap for Claude Code and catalog-only for any other consumer ([0013](docs/adr/0013-skill-catalog-direction.md)). | install, promote, deploy |
| Rollback | Swapping a skill name back to the bundle it pointed to before. | uninstall, revert |
| Shell hook | A metadata-only zsh or Bash hook that records command frames, including exit status. | hook (alone) |
| Agent hook | A hook in an agent tool such as Claude Code. | hook (alone) |
| SQLite progress handler | The rusqlite `hooks`-feature callback that enforces the query deadline and cancellation. | hook (alone) |

Dropped terms: "candidate" belongs to out-of-scope governance vocabulary. Relate and Combine are mentioned only as operators that plan admission rejects in v0.1.
