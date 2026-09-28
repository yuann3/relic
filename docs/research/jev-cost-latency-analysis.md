# Jev latency and cost at Relic's budget scale

**Status:** recommended analysis
**Decision scope:** budget model compatibility with Jev-backed child calls
**Research date:** 2026-09-28

## Decision

Jev fits comfortably within Relic's existing budget fences. No new fence types are needed for the prototype. The `max_cost_microusd` fence already covers pay-per-decision cost, and the `child_calls` fence already limits the number of Jev requests. The main change is that child calls become far cheaper and faster than LLM-backed equivalents, so the budget defaults are generous rather than tight.

## Jev pricing model

TypeSafe charges $0.042 per million input tokens. Output tokens are free. There is no per-decision surcharge; cost scales linearly with the token count of the state + questions sent in each request. Each request can contain multiple questions evaluated independently and in parallel; the cost is one token charge for the whole request, not per question.

Source: [TypeSafe pricing](https://www.eesel.ai/blog/typesafe-jev-pricing), [Crescent AI analysis](https://www.ai-crescent.com/blog/jev-typesafe-pricing-2026), [AY Automate breakdown](https://www.ayautomate.com/blog/jev-pricing-cost-per-decision)

**Important caveat:** Jev launched September 16, 2026 and remains in early access. TypeSafe has not ruled out that current pricing is subsidized. Rate cards may change at general availability. The analysis below uses current published prices and should be revisited at GA.

## Jev latency profile

| Source | p50 | p95 | p99 | Notes |
|---|---|---|---|---|
| liteLLM classifier benchmark (240 calls) | 127 ms | 231 ms | -- | vs. Haiku p50 688 ms |
| lowspecbot latency-probe, US-West | 26 ms | 41 ms | 82 ms | Minimal payload, sfo1 |
| lowspecbot latency-probe, US-East | 71 ms | 83 ms | 209 ms | GitHub Actions runner |
| lowspecbot latency-probe, Tokyo | 141 ms | 179 ms | 323 ms | Cross-Pacific RTT |
| Banking77 benchmark (385 cases) | 188 ms | 320 ms | 481 ms | 77-way classification |
| TypeSafe stated range | -- | -- | -- | 70-500 ms end-to-end |

Sources: [liteLLM benchmark](https://docs.litellm.ai/blog/jev-auto-router-benchmark), [lowspecbot/latency-probe](https://github.com/lowspecbot-GH/latency-probe), [jev.pro latency explainer](https://jev.pro/explainers/jev-decision-latency/)

For planning purposes, we use **p50 = 130 ms, p95 = 250 ms** as representative figures for US-based deployments with realistic payloads.

## Cost calculation for a typical Relic investigation

### Relic's current budget defaults (from `BudgetV1::prototype_default()`)

| Fence | Default | Relevant to Jev |
|---|---|---|
| `wall_ms` | 180,000 (3 min) | Yes -- bounds total investigation time |
| `root_calls` | 8 | No -- root calls stay LLM-backed |
| `child_calls` | 16 | Yes -- each Assess batch = 1 child call |
| `max_concurrency` | 4 | Yes -- parallel Jev requests |
| `max_cost_microusd` | 1,000,000 ($1.00) | Yes -- total cost cap |
| `estimated_input_tokens` | 96,000 | Informational |
| `model_egress_bytes` | 256 KiB | Root model egress, not Jev |

### Assess call structure (from `executor.rs`)

Each Assess op selects up to `max_items` observations and batches them into child calls of `batch_size` items each. The coordinator reserves `ceil(max_items / batch_size)` child calls before execution.

Typical investigation from the end-to-end test: `max_items = 8, batch_size = 4` produces 2 child calls. The spec example plan uses `max_items = 32, batch_size = 4` producing 8 child calls.

Worst case under budget: 16 child calls (the fence maximum).

### Token estimate per Jev child call

A child call sends one `EvidencePacket` containing `batch_size` items plus the objective string. Each item has an `item_id`, `evidence_id`, and `safe_text` (typically a shell command or short observation, ~50-200 bytes). With 4 items per batch:

| Component | Estimated tokens |
|---|---|
| Packet envelope + objective | ~80 tokens |
| 4 items x ~60 tokens each | ~240 tokens |
| Total per child call | ~320 tokens |

With Jev's multi-question capability, we would send one request per batch with the assessment questions (supports/contradicts/irrelevant per item). The questions and state go in one request, so the token count is the combined payload.

For a generous upper bound, assume ~500 tokens per child call.

### Cost per investigation

| Scenario | Child calls | Tokens per call | Total tokens | Cost at $0.042/MTok |
|---|---|---|---|---|
| Typical (8 items, batch 4) | 2 | 500 | 1,000 | $0.000042 |
| Spec example (32 items, batch 4) | 8 | 500 | 4,000 | $0.000168 |
| Budget maximum (64 items, batch 4) | 16 | 500 | 8,000 | $0.000336 |

**Maximum cost of Jev child calls per investigation: $0.00034** -- 0.034% of the $1.00 budget cap.

### Comparison with LLM child calls

For the same Assess work using GPT-4o-mini ($0.15/MTok input, $0.60/MTok output):

| Scenario | Jev cost | GPT-4o-mini cost (input+output) | Ratio |
|---|---|---|---|
| Typical (2 calls) | $0.000042 | ~$0.0005 | Jev ~12x cheaper |
| Spec example (8 calls) | $0.000168 | ~$0.002 | Jev ~12x cheaper |
| Budget max (16 calls) | $0.000336 | ~$0.004 | Jev ~12x cheaper |

GPT-4o-mini estimates assume ~500 input tokens + ~200 output tokens per call. The ratio shifts further in Jev's favor because Jev charges nothing for output.

At both price points, child call cost is negligible relative to root call cost. A single root call to a frontier model (GPT-4o, Claude Sonnet) at ~$3-5/MTok input with ~2,000 input tokens costs ~$0.006-0.01 -- more than all 16 Jev child calls combined.

## Latency calculation for a typical investigation

### Sequential execution (current prototype)

The coordinator in `coordinator.rs` executes plan ops sequentially within a turn. Assess child calls within a single Assess op are also sequential (the `for chunk_start in ...` loop in `executor.rs`).

| Scenario | Child calls | p50 per call | Total Jev wall time | Fraction of 180s budget |
|---|---|---|---|---|
| Typical (2 calls) | 2 | 130 ms | 260 ms | 0.14% |
| Spec example (8 calls) | 8 | 130 ms | 1,040 ms | 0.58% |
| Budget max (16 calls) | 16 | 130 ms | 2,080 ms | 1.16% |

At p95 (250 ms per call):

| Scenario | Child calls | p95 per call | Total Jev wall time | Fraction of 180s budget |
|---|---|---|---|---|
| Typical (2 calls) | 2 | 250 ms | 500 ms | 0.28% |
| Spec example (8 calls) | 8 | 250 ms | 2,000 ms | 1.11% |
| Budget max (16 calls) | 16 | 250 ms | 4,000 ms | 2.22% |

### Batched/concurrent execution (future optimization)

With `max_concurrency = 4`, batches within a single Assess op could run in parallel. This is not implemented in the prototype but is a straightforward optimization.

| Scenario | Child calls | Concurrency | Waves | p50 wall time | p95 wall time |
|---|---|---|---|---|---|
| Typical (2 calls) | 2 | 2 | 1 | 130 ms | 250 ms |
| Spec example (8 calls) | 8 | 4 | 2 | 260 ms | 500 ms |
| Budget max (16 calls) | 16 | 4 | 4 | 520 ms | 1,000 ms |

Additionally, Jev evaluates multiple questions within a single request in parallel. If the Assess protocol is refactored to send all per-item questions in one Jev request (rather than one child call per batch), latency drops further. TypeSafe's docs report "12.2x cheaper and 10.0x faster" for 13 questions batched vs. 13 sequential calls.

### Comparison with LLM child calls

LLM child calls (e.g., Claude Haiku) have p50 latency of ~700 ms per call. For the spec example (8 calls, sequential): 5,600 ms vs. Jev's 1,040 ms -- a 5.4x improvement, consistent with the liteLLM benchmark finding of 5.43x.

## Budget model compatibility analysis

### Existing fences that cover Jev

| Fence | How it applies to Jev | Adequate? |
|---|---|---|
| `child_calls: 16` | Each Assess batch = 1 child call reservation. Jev or LLM, the count is the same. | Yes |
| `max_cost_microusd: 1_000_000` | Jev's maximum child cost ($0.00034) is 0.034% of cap. Works, but is not meaningfully constraining. | Yes, overprovisions |
| `wall_ms: 180_000` | Even 16 sequential Jev calls at p95 = 4s. Well within 180s. | Yes |
| `max_concurrency: 4` | Applies if we parallelize Assess batches. | Yes |
| `model_egress_bytes: 256 KiB` | Jev responses are small typed judgments, not generated text. No pressure on this fence. | Yes |
| `estimated_input_tokens: 96_000` | Jev child calls use ~8K tokens at max. Root LLM calls dominate. | Yes |

### Do we need a new `decisions` or `jev_calls` fence?

No. The existing `child_calls` fence already limits the number of Assess batches, which maps directly to Jev API requests. Adding a separate `decisions` fence would duplicate the constraint without adding safety. If we later refactor to send multiple questions per Jev request (collapsing batches), then `child_calls` should track Jev requests, not individual per-item judgments -- the budget reservation in the coordinator already does this correctly.

### Does `max_cost_microusd` need adjustment?

The $1.00 cap was set with LLM costs in mind. Root calls dominate cost regardless of child backend. With Jev children, the budget is even more dominated by root calls. No adjustment needed for Jev; the cap remains appropriate for root-call cost control.

## Recommendations

1. **No new fence types needed.** The existing budget model accommodates Jev without structural changes. `child_calls` limits request count; `max_cost_microusd` caps total spend; `wall_ms` bounds total time.

2. **Parallelize Assess batches.** The prototype executes Assess child calls sequentially. Given Jev's low latency, this is tolerable (worst case 4s). But parallelizing within `max_concurrency` is an easy win that cuts worst-case Jev wall time from 4s to 1s with the current concurrency limit of 4.

3. **Consider multi-question Jev requests.** TypeSafe's API accepts multiple questions per request, evaluated in parallel. Instead of sending one Jev request per batch (each with a single classification question), we could send one request per Assess op with all item questions. This would collapse 4-16 child calls into 1-2 Jev requests, reducing both latency and token overhead (shared state context).

4. **Revisit at GA.** Current pricing is early-access and may be subsidized. Re-evaluate when TypeSafe publishes GA pricing. The budget model's `max_cost_microusd` fence provides a hard cap regardless.

5. **Track Jev cost separately in telemetry.** Even though no new fence is needed, the investigation trace should distinguish Jev child costs from LLM root costs for observability. This is a telemetry concern, not a budget model change.

## Sources

- [TypeSafe API documentation](https://docs.typesafe.ai/api.md)
- [Building with System One](https://docs.typesafe.ai/concepts/how-to-build-with-system-one.md)
- [eesel AI: Jev pricing breakdown](https://www.eesel.ai/blog/typesafe-jev-pricing)
- [Crescent AI: Jev pricing analysis](https://www.ai-crescent.com/blog/jev-typesafe-pricing-2026)
- [AY Automate: cost per decision](https://www.ayautomate.com/blog/jev-pricing-cost-per-decision)
- [liteLLM: Jev classifier benchmark](https://docs.litellm.ai/blog/jev-auto-router-benchmark)
- [lowspecbot/latency-probe](https://github.com/lowspecbot-GH/latency-probe)
- [jev.pro: decision latency explainer](https://jev.pro/explainers/jev-decision-latency/)
- Relic source: `src/rlm/budget.rs`, `src/rlm/coordinator.rs`, `src/rlm/executor.rs`, `src/rlm/protocol.rs`
