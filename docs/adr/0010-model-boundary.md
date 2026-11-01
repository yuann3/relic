# 0010. One Chat Completions adapter over ureq is the v0.1 model boundary; Jev is a conditional child-only slice and Codex is out

Status: Accepted · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

- [Specify the bounded RLM and model boundary](https://github.com/yuann3/relic/issues/5) chose one blocking Chat Completions adapter (base URL + `/chat/completions`, `stream: false`) with no provider SDK (`docs/research/rlm-model-boundary.md:15`). [Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1) lists "provider framework" as out of scope and leaves it to this lock to decide whether Jev amends the single adapter.
- No adapter exists. `FakeModel` is the only `ModelBoundary` (`src/rlm/model.rs:17-29`). `ModelError` has only `Refused` and `InvalidResponse` (`src/rlm/model.rs:31-35`), so it cannot express the retry classes. Cargo.toml has no HTTP client. ureq came in only for the Jev spike and left with it (added in 7fa9b14, removed in 17d7550). The spike's ureq config kept gzip and had no timeout, proxy/redirect override or body cap, which violates `rlm-model-boundary.md:385-397`.
- The ledger has no counters for provider attempts, cost or wall time. `wall_ms` is passed through untracked, and the coordinator never reserves `model_egress_bytes` (`src/rlm/budget.rs:9-18`, `:106-125`). Children run sequentially, with no pool and `max_concurrency` unenforced (`src/rlm/executor.rs:117-154`).
- Jev evidence ([[P1] Prove Jev across all three integration surfaces](https://github.com/yuann3/relic/issues/18)):
  - The 15 inputs were hand-written and judged by eye against `jev-latest` (resolved to jev-1.13.0). There is no same-packet comparison with a Chat Completions child.
  - Measured latency is 294–433 ms per batch, against the 130 ms estimate of [Research: Jev latency and cost at Relic's budget scale](https://github.com/yuann3/relic/issues/15). That is ~1.6–2.4× faster than the ~700 ms Haiku figure, not 5.4× (`docs/research/jev-prototype-findings.md:23`, `:42`; `docs/research/jev-cost-latency-analysis.md:100-104`, `:128`).
  - Pricing comes from third-party early-access sources (`jev-cost-latency-analysis.md:13-17`). No Jev or TypeSafe document covers retention, logging or training use.
  - The spike interpolated evidence into `instructions`, against the RLM boundary decision's evidence-as-JSON-data rule (`rlm-model-boundary.md:488`). The API does not force this.
- Rerank ([[T2] Wire Jev reranking into the search query path](https://github.com/yuann3/relic/issues/20)): one call took 294 ms for 6 items, against the <20 ms warm-FTS gate and the map's offline-search requirement. Its "BM25" input was hand-written, not FTS output. The ticket wires into the RLM `execute_search`, but plain search runs through `app::search` (`src/app.rs:76-98`). Intent routing ([[T3] Wire Jev intent routing into RLM planning](https://github.com/yuann3/relic/issues/21)) costs 286–376 ms, sends the question to a second provider, and has no strategy table to route to.
- Retry: the RLM boundary decision's rule follows RFC 9110 for non-idempotent POST (`rlm-model-boundary.md:455-467`). TypeSafe docs prescribe backoff on 429 and 529 (`docs/research/typesafe-http-api-contract.md:132-139`). CODING_STANDARD requires that a retried error leave a visible trace.
- Transport: the RLM boundary decision picked ureq 3 with reqwest/Tokio contingent on a never-run Ctrl-C test (`rlm-model-boundary.md:385-399`, `:469-480`, `:544`). The headless binary is 2.2 MiB against an 8–12 MiB target ([Prove the interactive search workspace](https://github.com/yuann3/relic/issues/10)). The ureq/rustls delta is unmeasured.
- Codex: [Determine whether Codex subscription authentication is officially usable](https://github.com/yuann3/relic/issues/6) gave a qualified go, conditional on a pinned-CLI prototype on macOS and Linux (`docs/research/codex-subscription-authentication.md:109-120`). No such ticket exists. The map calls Codex support "desirable but non-blocking".

## Decision

- **One adapter.** One `ChatCompletionsClient` serves root and child calls. It ships with built-in profiles `openai`, `llama_cpp`, `custom` (configuration presets, not separate clients; `vllm` was removed after S7: a vLLM server is reached through `custom` with `json_schema` and `max_tokens`), `relic model check`, and mock-server tests. It lands after executor hardening ([0009](0009-rlm-kernel.md)), which needs no network. Adapters accept only an `EgressRequest` ([0004](0004-egress-seam-and-privacy-canary.md)).
- **Fences.** Every outbound model call, whatever the provider, reserves:
  - one logical call (root or child);
  - `provider_attempts`;
  - `model_egress_bytes`;
  - cost;
  - output tokens.

  `wall_ms` is enforced as a deadline. `max_concurrency` (default 4) caps concurrent child calls. Enforcement lands in the adapter slice.
- **Retry, for every adapter.** At most one retry, and only on a failure proven to occur before send or on a 429 with a bounded `Retry-After`. Never retry on 5xx, 529, in-flight or ambiguous failures. Each retry writes a trace event (attempt ordinal, reason) and consumes `provider_attempts`.
- **Pinning (as the RLM boundary decision wrote it).** Model aliases are allowed. The returned model is always recorded. A pinned revision is required only for a persistent model cache. Release evidence names exact server and model versions.
- **Transport.** ureq 3 with `default-features = false`, `rustls` + `json`, no proxy, no redirects, timeouts derived from the deadline, and a 1 MiB body cap.
- **Transport failure rule.** Switch to reqwest/Tokio only if this Ctrl-C gate fails: Ctrl-C during a root call, and during a four-child batch, exits 130 within 250 ms with `Cancelled` written and no late commits.
- **Jev.** Jev is a conditional v0.1 slice limited to child Assess calls. It lands only if, before its slice starts, both of these exist:
  - a same-packet comparison against a Chat Completions child model;
  - a clean TypeSafe retention/logging answer.

  Until then the `ModelBoundary` child seam only keeps Jev possible, and the single adapter stands.
- **No reranker anywhere in v0.1.** Reranking never enters `app::search`. Jev reranking and Jev intent routing are rejected for v0.1.
- **Codex subscription delegation is out of scope for v0.1.** Its re-entry contract is the go/no-go rule of [Determine whether Codex subscription authentication is officially usable](https://github.com/yuann3/relic/issues/6) (`codex-subscription-authentication.md:109-120`).

## Amends

- [Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1):
  - Out of scope gains Codex subscription delegation (the go/no-go rule as re-entry contract), Jev reranking and Jev intent routing.
  - The Notes question (does Jev amend the single adapter?) is answered: not unless the conditional Jev slice lands.
  - The decision line for the Codex ticket ("optionally delegate") is deferred past v0.1.
- [Embed Jev across Relic's pipeline](https://github.com/yuann3/relic/issues/12) and [Scope the wide Jev integration surfaces for v0.1](https://github.com/yuann3/relic/issues/17): three Jev surfaces become one conditional surface (child Assess). [[T2] Wire Jev reranking into the search query path](https://github.com/yuann3/relic/issues/20) and [[T3] Wire Jev intent routing into RLM planning](https://github.com/yuann3/relic/issues/21) close as out of scope.
- [Specify the bounded RLM and model boundary](https://github.com/yuann3/relic/issues/5): the single adapter is amended only if the Jev slice lands, and then only for child Assess calls.
- TypeSafe research (`typesafe-http-api-contract.md:132-139`): its exponential backoff on 429/529 does not apply. The shared retry rule governs.

## Consequences

- Owner: S2 recall core, in this order:
  1. executor hardening ([0009](0009-rlm-kernel.md));
  2. the Chat Completions adapter (egress seam, budget enforcement, ureq Ctrl-C gate, cancel/progress);
  3. `relic recall --json` and the TUI replacing `fake_recall`;
  4. the privacy canary gate ([0004](0004-egress-seam-and-privacy-canary.md)).
- `ModelError` must distinguish every class the retry rule names. The ledger gains attempt, cost, output-token and deadline enforcement, and the coordinator reserves egress bytes.
- The mock-server matrix covers:
  - connect failure before send;
  - 429 with `Retry-After`;
  - 500;
  - 529;
  - disconnect after body send;
  - timeout;
  - oversized body;
  - invalid JSON;
  - unknown fields;
  - refusal;
  - `finish_reason: length`.

  Only the first two cases retry. No logical call commits twice (`rlm-model-boundary.md:543`).
- The Ctrl-C gate is a hanging-server process test. Its four-child case needs a concurrent child pool, which does not exist yet. If the gate fails, the reqwest/Tokio switch overrides [0001](0001-one-synchronous-process.md) and [0015](0015-dependency-envelope.md) for that dependency.
- `relic model check` must pass against one real remote and one real local llama.cpp endpoint as release evidence, with exact versions recorded.
- S2 measures and reports, per [0014](0014-performance-contract.md): the stripped ureq/rustls delta and RSS with the four-child pool. RSS is reported, not gated.
- The Jev slice (S2b) is subject to the same fences, retry rule and egress seam. S0 creates the Jev evidence ticket; S2b runs only if it passes. [[T1] Implement JevModelBoundary adapter](https://github.com/yuann3/relic/issues/19) stays blocked on the recall core and that ticket.
- Users without API billing get recall through the `llama_cpp` profile, or `custom` for another local server such as vLLM.

## Rejected alternatives

- **Jev leaves v0.1 outright.** Decides on missing evidence; the conditional slice lets the same-packet comparison and retention answer decide instead.
- **Jev on all three surfaces.** A reranker conflicts with offline search and the <20 ms gate. An intent pre-call sends the question out and needs non-child fences. It has the least evidence.
- **Rerank only inside RLM recall.** A non-child call that overlaps Assess + Select.
- **Opt-in networked `relic search --rerank`.** Adds egress and a model dependency to plain search.
- **Optional intent pre-call.** There is no strategy table to route to, and the root planner already does the job.
- **`aux_calls` fence.** A dimension only rerank and intent need, and both are rejected.
- **Leave fences as implemented.** The bounded-cost claim would rest on fences that do not run.
- **Treat 529 like 429.** Needs per-status provider knowledge and proof that a 529 was not applied.
- **Per-adapter retry policies.** Two policies to test, and inconsistent budget consumption.
- **Pinned revision in every profile.** Excludes local servers and providers without immutable IDs.
- **Aliases everywhere, no persistent cache.** Gives up cache reuse for no gain over the standing pinning rule.
- **Adopt reqwest/Tokio now.** Breaks the sync runtime and grows the binary without evidence.
- **Run the Ctrl-C prototype before the lock.** Delays the lock. The failure rule decides it instead.
- **Build the adapter in parallel with hardening on `FakeModel`.** `FakeModel` tests cannot catch wire-mapping errors, and integration risk moves to the merge.
- **Build the adapter after the CLI/TUI recall surfaces.** Leaves the transport gate, which can change the dependency envelope, unproven until late.
- **Codex prototype ticket now.** It is a second adapter with subprocess and sandbox verification on two OSes, and it delays the MVP.
- **Drop Codex entirely.** Discards resolved research for a feature the map calls desirable.

## Open facts

- The Ctrl-C gate with ureq; the stripped ureq/rustls delta; RSS with four concurrent children (S2).
- Whether any real remote or local Chat Completions model reliably emits a valid multi-turn `PlanV1` (S2, `relic model check`).
- For the Jev evidence ticket:
  - same-packet verdict agreement and effect on the answer against a Chat Completions child;
  - TypeSafe retention, logging and training-use terms;
  - primary-source pricing;
  - whether an exact model ID is accepted in place of `jev-latest`;
  - whether a TypeSafe 429 guarantees the request was not applied or billed.
