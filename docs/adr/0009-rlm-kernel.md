# 0009. The RLM kernel executes five operators, consumes child verdicts, checks child results strictly and owns batching and cancellation

Status: Accepted · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

[Specify the bounded RLM and model boundary](https://github.com/yuann3/relic/issues/5) fixed a seven-operator, depth-one kernel (`docs/research/rlm-model-boundary.md:13`, `:137-147`). [Prove bounded RLM recall with exact citations](https://github.com/yuann3/relic/issues/9) proved it with `FakeModel` only. The prototype leaves child calls without effect:

- `execute_assess` collects `all_assessments` and drops them; only the input observations become the AssessmentSet (`src/rlm/executor.rs:115`, `:153`, `:156-159`). Root state carries no assessment data (`src/rlm/coordinator.rs:321-366`). Select, Relate and Combine fall through to `Ok(None)` (`src/rlm/coordinator.rs:315-316`). A wrong verdict mapping changes no output, so no child-adapter test can fail.
- The child-result check covers only citations, against an allowlist built per Assess op rather than per packet, so a child can cite another batch's evidence. No `packet_id` check, no `item_id` membership/duplicate/coverage check, no `deny_unknown_fields`, no byte caps (`src/rlm/executor.rs:109-151`; `src/rlm/protocol.rs`). Research requires rejecting unknown `packet_id`/`item_id` and out-of-allowlist citations, byte caps, and fail-closed unknown fields (`rlm-model-boundary.md:207`, `:489`).
- `batch_size` is a model-chosen u32 cast to u16 for the reservation; 65536 truncates to 0 and `div_ceil` panics. Plan admission rejects 0 but sets no upper bound, so one packet can carry all 64 items and exceed the 24 KiB child cap (`src/rlm/coordinator.rs:256-257`; `src/rlm/validate.rs:229-246`). Research makes batching host policy: ≤4 items/call (`rlm-model-boundary.md:145`), "an evaluation result, not a protocol fact" (`:518`).
- `run_investigation` takes no cancel token and emits no events (`src/rlm/coordinator.rs:47-52`). The RLM boundary decision requires cancel checks before every reservation, operator batch, child admission and commit, with SQLite interruptible (`rlm-model-boundary.md:317`, `:469-480`). The TUI's `fake_recall` already proved an `Arc<AtomicBool>` cancel plus `RecallEvent` stream (`src/ui/fake_recall.rs`, `src/ui/model.rs:116-122`). `execute_op` already takes 8 arguments under an uncommented clippy allow (`src/rlm/coordinator.rs:198`).
- Relate requires relation indexes in the store (session, git_state) ([Prove bounded RLM recall with exact citations](https://github.com/yuann3/relic/issues/9) resolution), which would add DB size against [0008](0008-gate-change-database-size.md).

## Decision

- **Five executed operators.** v0.1 executes `search`, `expand`, `assess`, `select`, `inspect`. Plan admission (`src/rlm/validate.rs`) rejects `relate` and `combine` until they are built. Nothing is admitted but left unexecuted.
- **Verdicts persist and drive Select.** Each child `ItemAssessment` is persisted per evidence handle in the AssessmentSet. Select filters by verdict.
- **Child results are checked strictly.** The whole child result is rejected on any of:
  - `packet_id` differing from the packet sent;
  - an unknown or duplicate `item_id`;
  - a citation outside *that call's* packet allowlist (built from the packet's own items, not the whole Assess op);
  - an unknown field — `#[serde(deny_unknown_fields)]` on every model-facing struct.
- Child byte caps are enforced before parsing.
- A packet item the child omits is recorded as `Unassessed`. A verdict is never inferred. Select excludes `Unassessed` items; the root sees them as unassessed, never as support.
- **Host owns batching.** `batch_size` is removed from `AssessV1`. The host picks the batch: ≤4 items, shrunk until the child packet is ≤24 KiB (the child cap of [0004](0004-egress-seam-and-privacy-canary.md)). This deletes the u16 cast and its panic class.
- An item larger than the child cap on its own is truncated through the research's explicit `TruncationV1` (`rlm-model-boundary.md:99`, `:103`). Truncated evidence cannot support a claim about omitted content (`:219`).
- **Cancel and progress.** `run_investigation` takes `&AtomicBool` cancel and a progress sink. Both are checked/emitted at the RLM boundary decision's admission points: before every reservation, operator batch, child admission and commit.
- The progress event enum is owned by `src/rlm`. The UI maps it to `RecallEvent`.
- SQLite steps are interruptible through the store's SQLite progress handler seam (see [0006](0006-search-ordering-and-query-budget.md)).
- An investigation context struct may replace `execute_op`'s 8 arguments.

## Amends

- [Specify the bounded RLM and model boundary](https://github.com/yuann3/relic/issues/5): the seven-operator kernel becomes five executed operators. `relate` and `combine` remain only as names rejected at plan admission. The "seven-operator" decision line in [Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1) changes to match.
- [Specify the bounded RLM and model boundary](https://github.com/yuann3/relic/issues/5): the `AssessV1` plan schema loses `batch_size`. No compatibility path; the prototype schema breaks.

## Consequences

- Owner: S2 recall core, executor hardening step, which needs no network and lands before the adapter ([0010](0010-model-boundary.md)). It covers verdict persistence, Select, child-result checks, host batching and the Relate/Combine admission rejection. Cancel/progress lands with the S2 adapter step, which owns the Ctrl-C gate.
- Required proofs, each a committed test:
  - flipping one child verdict changes Select output;
  - each rejection case (wrong `packet_id`, unknown `item_id`, duplicate `item_id`, cross-packet citation, unknown field, oversize body) rejects the whole result;
  - an omitted item yields `Unassessed` and never passes Select;
  - an item over the child cap is sent truncated, marked with `TruncationV1`;
  - a plan containing `relate` or `combine` is refused at plan admission;
  - no packet exceeds 4 items or 24 KiB.
- Adapter tests ([0010](0010-model-boundary.md)) become honest: a wrong wire mapping now changes output.
- The empty-claims decision of [Resolve the claims-text gap in the Jev child contract](https://github.com/yuann3/relic/issues/16), justified by "Select operator filters by verdict only", gets a consumer.

## Rejected alternatives

- **Drop Select and pass assessment summaries in root state.** Spends root context and moves deterministic filtering into the model.
- **Keep prototype behaviour until the held-out evaluation.** Child calls cost money and change nothing, and every adapter test stays vacuous.
- **All-or-nothing on missing items too.** One skipped item wastes the whole batch. `Unassessed` keeps the useful work without inventing a verdict.
- **Lenient checking (silently drop unknown/duplicate items).** Violates fail-closed and hides model faults. It also weakens the forgery-resistant handles that [Prove bounded RLM recall with exact citations](https://github.com/yuann3/relic/issues/9) proved.
- **Keep `batch_size`, admitted only as 1..=4.** The model controls a host policy it has no basis to choose.
- **Clamp `batch_size` silently to 4.** Hides model mistakes and is a silent policy change.
- **Pull-based step machine (`next_step()`).** Larger refactor, and a blocking HTTP call inside one step still cannot be interrupted.
- **No cancel token or progress sink; kill the thread or process on cancel.** No progress UI and no durable `Cancelled` record, which fails the RLM boundary decision's cancellation contract (`rlm-model-boundary.md:469-480`).
- **Keep Relate/Combine admitted but unexecuted.** Accepted plans would silently do nothing.
- **Build Relate/Combine for v0.1.** Relate needs relation indexes that add DB size.
