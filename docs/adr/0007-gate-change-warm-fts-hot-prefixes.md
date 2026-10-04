# 0007. Amend the warm-FTS gate: hot unindexed prefixes are deadline-bounded, not <20 ms

Status: Accepted · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

- A prefix with no prefix index of exactly its length merges every posting under it before `LIMIT` or a rowid bound applies: about 13 ns per posting for one large term, 19–30 ns when several merge. The worst keystroke the TUI sends on the 1M fixture is `git tas*` at 20.95 ms p95; 3 of 98 sent keystrokes exceed 20 ms. Cost is linear in history size, so about 42 ms at 2M. The research says the 20.8–21.1 ms margin "means nothing"; the cost model decides (docs/research/interactive-fts-measurements.md:20-43).
- Sparse AND (two tokens that never share a row) cannot stop early: `git task` costs 6.8 ms on every schema. Two 30%-frequency tokens that never co-occur are extrapolated, not measured, at about 60 ms at 1M (docs/research/interactive-fts-measurements.md:21, 69).
- Measured strategies (worst sent keystroke, DB size): baseline 20.95 ms, 3.04x; `prefix='2'` 20.83 ms, 3.41x; `prefix='2 3 4'` 6.82 ms, 4.02x, but 21.2 ms when the dominant token has 5+ characters; `prefix='2'` + OR-of-terms rewrite 6.69 ms, 3.41x plus a term list (docs/research/interactive-fts-measurements.md:29-37).
- The OR rewrite's 6.69 ms holds only for narrow expansions: the widest OR in that run had 2 terms. Behind a lead word it never co-occurs with, OR cost multiplies: 300 terms behind `git` 12.8 ms, 1,111 terms 73 ms, against 8.5 ms for the plain prefix (ratios only; SQLite 3.53.4). The term list takes 86 ms to load at 1M and must reload on every `data_version` change, which every command another shell records triggers once shell hooks are active. A stale list silently misses results (docs/research/interactive-fts-measurements.md:53, 59-66, 299).
- Storage contract: "Do not optimize with a second sidecar authority. SQLite remains the cursor/index truth" (docs/research/storage-search-runtime-architecture.md:180).
- No option meets <20 ms in general, so every choice is either a renegotiation or a bet on unmeasured behaviour. The lock's gate policy allows renegotiating one gate per ADR, with written justification and a new number, judged on user-perceptible impact.

## Decision

Old gate: warm FTS over 1M events, p95 <20 ms.

New gate, at 1M events, warm cache:
- Whole-word and indexed-prefix keystrokes: p95 <20 ms. Hard gate.
- Unindexed hot-prefix and sparse-AND keystrokes: bounded by the query deadline of [0006](0006-search-ordering-and-query-budget.md). On a deadline hit the TUI keeps the previous page.

Schema and query shape:
- No FTS prefix index. No in-memory term list. No OR-of-terms rewrite.
- Escalate to the OR-of-terms rewrite only if dogfooding shows deadline hits matter, and only after lead-word + wide-OR keystrokes (for example `git 1*`, `docker 1*`) are added to the bench.

Why this departs from the research recommendation (`prefix='2'` + OR rewrite, docs/research/interactive-fts-measurements.md:55-68):
- The rewrite is proven only for 2-term ORs. Real long tails produce wide expansions.
- Wide ORs multiply in cost behind a lead word (73 ms at 1,111 terms vs 8.5 ms for the plain prefix).
- A stale term list is a silent correctness bug (missed results), not a slowdown. The deadline's failure mode is a visible kept page.
- The term list is a sidecar authority, which the storage contract forbids.

User-perceptible impact:
- On the 1M fixture, 95 of 98 sent keystrokes stay under 20 ms; whole words cost 0.03–0.06 ms. Keystroke to rendered page measured p50 4 ms, p95 15 ms at 1M ([Prove the interactive search workspace](https://github.com/yuann3/relic/issues/10)).
- While typing a prefix of a very frequent token behind a lead word (`git tas`), the TUI can keep the previous keystroke's page until a keystroke finishes within the deadline. Nothing hangs.
- The affected keystrokes grow with history size (about 21 ms at 1M, about 42 ms at 2M), so larger histories see more kept pages.

## Amends

- [Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1), Notes, hard gate "`<20 ms` warm FTS over 1M events": replaced by the two-part gate above.
- [Choose the storage, search, and runtime architecture](https://github.com/yuann3/relic/issues/4), acceptance matrix row "FTS query, 1M events: warm p95 `<20 ms`" (docs/research/storage-search-runtime-architecture.md:313): same replacement. The cold half of that row (`<100 ms`) is unchanged.

## Consequences

- S0 records the amended gate on the map as an explicit amendment that links this ADR.
- S1c (read path) owns the proof in the gate harness ([0014](0014-performance-contract.md)): the 1M keystroke set (benches/interactive_fts.rs) split into whole-word/indexed-prefix keystrokes, asserted p95 <20 ms, and unindexed hot-prefix/sparse-AND keystrokes, asserted to end within the deadline plus the stated epsilon.
- `prefix='2'` would also have added 0.37x to the size gate being restated in [0008](0008-gate-change-database-size.md); the deadline adds no state and no size.

## Rejected alternatives

- `prefix='2'` + OR-of-terms rewrite (research recommendation): proven only for 2-term ORs; wide ORs multiply; a stale term list is a silent correctness bug; sidecar authority.
- `prefix='2 3 4'`: +0.98x size, and a dominant token of 5+ characters brings back about 21 ms.
- `prefix='2'` alone: +0.37x size and still 20.83 ms on `git tas*`.
- Keep the <20 ms gate and treat the 21 ms miss as noise: the research rejects that reading; the miss is linear and doubles at 2M.
- Run `interactive_fts --hot` on an anonymized real history before locking (docs/research/interactive-fts-measurements.md:71): not adopted; dogfooding decides whether deadline hits matter.

## Open facts

- How often real keystrokes hit the deadline on a real history (dogfooding; the fixture has 30 command prefixes and one command per second).
- The measured sparse-AND floor for two high-frequency tokens that never co-occur at 1M (extrapolated about 60 ms).
- Whether the interrupt lands inside a single merge ([0006](0006-search-ordering-and-query-budget.md)); if not, the bound is the longest single merge.
