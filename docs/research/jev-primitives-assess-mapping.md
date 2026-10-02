# Mapping Jev primitives to the Assess child model contract

**Status:** research findings
**Decision scope:** Jev as the child model in Relic's bounded RLM
**Research date:** 2026-09-28

## Decision

Jev Choice(4) faithfully expresses the Verdict contract. Jev confidence maps to Relic's Confidence with threshold binning. The claims-text gap is real but non-blocking: child claims are informational, not structurally required by any downstream operator. Recommended approach: empty child claims for Jev assessments, with optional probability fields added to `ItemAssessmentV1`.

## The child model contract today

Each `child_call` (src/rlm/model.rs:19-24) receives:
- `EvidencePacket`: a batch of `EvidenceItem { item_id, evidence_id, safe_text }`
- `objective: &str`: the Assess operator's objective string

And returns `ChildResultV1` containing `Vec<ItemAssessmentV1>`:

```rust
pub struct ItemAssessmentV1 {
    pub item_id: String,
    pub verdict: Verdict,          // supports | contradicts | unclear | irrelevant
    pub confidence: Confidence,    // low | medium | high
    pub claims: Vec<ClaimV1>,      // text + supporting/contradicting evidence IDs
    pub missing: Vec<String>,      // missing evidence descriptions
}
```

## Field-by-field mapping

| Contract field | Jev primitive | Mapping | Gap? |
|---|---|---|---|
| `verdict: Verdict` | Choice(4 options) | `criteria: { supports, contradicts, unclear, irrelevant }` → `response.choice` maps 1:1 to `Verdict` enum | **None** |
| `confidence: Confidence` | Choice confidence | `response.confidence` (0.0-1.0) → threshold bin: >0.7=High, 0.4-0.7=Medium, <0.4=Low | **Semantic shift** (see below) |
| `claims: Vec<ClaimV1>` | — | Jev cannot generate text. No primitive produces free-text output. | **GAP** |
| `claims[].supports` | — | Evidence IDs are host-owned; the child model echoes them back. Jev doesn't know evidence IDs. | **GAP** (but bridgeable in code) |
| `claims[].contradicts` | — | Same as above | **GAP** (bridgeable) |
| `missing: Vec<String>` | — | Free-text descriptions of missing evidence. Jev can't produce these. | **GAP** |

### Jev request for one evidence item

```json
{
  "state": {
    "objective": "find observed renewal steps and contrary outcomes",
    "evidence": "certbot renew --nginx\nSystemd timer activated."
  },
  "model": "jev-latest",
  "questions": {
    "verdict": {
      "type": "choice",
      "instructions": "Does this evidence support or contradict the stated objective?",
      "criteria": {
        "supports": "The evidence directly supports the objective with an observed action, command, or outcome",
        "contradicts": "The evidence shows a failure, rollback, or outcome contrary to the objective",
        "unclear": "The evidence is ambiguous, truncated, or insufficient to judge relevance",
        "irrelevant": "The evidence is entirely unrelated to the objective"
      }
    }
  }
}
```

### Batching: multiple items per HTTP call

Jev evaluates all questions against the same state. For a batch of N evidence items, two approaches:

**Option A — N items as N questions in one call:**
```json
{
  "state": { "objective": "..." },
  "questions": {
    "item_0": { "type": "choice", "instructions": "Evidence: 'certbot renew'. Does this...", "criteria": { ... } },
    "item_1": { "type": "choice", "instructions": "Evidence: 'apt update'. Does this...", "criteria": { ... } }
  }
}
```
One HTTP call per batch. Each item's evidence goes into its question instructions (not state, since state is shared). This is the natural fit for Relic's Assess batches (≤4 items/batch).

**Option B — one call per item:**
Simpler but more HTTP calls. At ~150ms each, a 4-item batch = 600ms vs ~150ms for Option A.

**Recommendation: Option A.** Pack evidence into per-question instructions; put the objective in shared state.

## The confidence semantic shift

Jev confidence = distribution concentration. A Choice with `{supports: 0.85, contradicts: 0.05, unclear: 0.07, irrelevant: 0.03}` has high confidence because one option dominates.

Relic's `Confidence` = advisory quality signal (low/medium/high). The root model uses it to weight assessments.

These are close enough. Threshold mapping:
- Jev confidence > 0.7 → `Confidence::High`
- Jev confidence 0.4–0.7 → `Confidence::Medium`
- Jev confidence < 0.4 → `Confidence::Low`

The thresholds should be tunable (config, not hardcoded) since Jev's calibration may differ from LLM confidence patterns.

## The claims-text gap: analysis

### What depends on child claims?

Tracing downstream from `ItemAssessmentV1.claims`:

1. **Select operator** (src/rlm/validate.rs:170, executor) — filters by `verdict` only. **Ignores claims.**
2. **Root model** — receives assessment summaries when generating `FinishV1`. Child claims provide reasoning text the root can incorporate into final claims. **Informational, not structural.**
3. **Finish validation** (src/rlm/finish.rs:32-75) — validates `FinalClaimV1` and `ProcedureStepV1` (from the root model), not child `ClaimV1`. **Does not check child claims.**
4. **Evidence handles** — `ClaimV1.supports` and `.contradicts` reference evidence IDs. These are used for citation tracking. **Bridgeable in code** (the host knows which evidence was sent to Jev).

### Key insight

Child `ClaimV1` is the LLM child model's scratchpad — its explanation of *why* it chose a verdict. With Jev:
- The verdict IS the output (no reasoning chain)
- The probability distribution IS the explanation (more informative than text)
- Evidence ID linkage is known by the host (it constructed the packet)

### Options for the gap

**(a) Empty claims (recommended for v0.1)**

Set `claims: vec![]` for all Jev-produced assessments. The verdict + confidence carry the signal. The host knows which evidence was assessed (it built the packet). The root model generates final claims from evidence text.

Pros: Simplest. No protocol change. Clean separation.
Cons: Root model loses child reasoning. Mitigated by Jev's verdict being more precise than LLM text.

**(b) Code-assembled claims**

Build `ClaimV1` in host code: `text = "Evidence '{safe_text[..80]}' assessed as {verdict}"`, `supports = [evidence_id]`.

Pros: Gives root model something textual to work with.
Cons: Adds noise — the snippet is already in the evidence; duplicating it is redundant.

**(c) Probability-enriched claims (future)**

Extend `ItemAssessmentV1` with optional `probabilities: Option<HashMap<String, f64>>`. The root model receives richer structured data instead of generated text.

Pros: Strictly better signal than text claims.
Cons: Protocol change. Requires root model to understand probability fields.

### Recommendation

Start with **(a)** empty claims. If root model quality degrades without child reasoning text, upgrade to **(b)** or **(c)**. The finish validation is unaffected because it only checks final claims from the root model.

## The `missing` field gap

`ItemAssessmentV1.missing` is a `Vec<String>` of free-text descriptions of evidence the child model wanted but didn't have. Jev can't produce these.

**Resolution:** Set `missing: vec![]`. The host knows what evidence exists (it queried the store). If an item has `verdict: Unclear` with low confidence, the host can infer insufficiency without needing the model to articulate what's missing.

## Summary mapping table

| Assess contract | Jev implementation | Status |
|---|---|---|
| Verdict (4 variants) | Choice(4 criteria) | **Direct map** |
| Confidence (3 levels) | Choice confidence → threshold bins | **Close enough** (tunable thresholds) |
| Claims text | Empty vec (v0.1) | **Gap accepted** — not structurally required |
| Claims evidence IDs | Host-assigned from packet | **Bridgeable in code** |
| Missing evidence | Empty vec | **Gap accepted** — host infers from verdict+confidence |
| Batch (≤4 items) | N questions per HTTP call | **Supported** via Jev batch semantics |

## Sources consulted

- src/rlm/model.rs — ModelBoundary trait, ChildResultV1
- src/rlm/protocol.rs — ItemAssessmentV1, Verdict, Confidence, ClaimV1
- src/rlm/validate.rs — Select operator uses verdict only
- src/rlm/finish.rs — validates FinalClaimV1, not child ClaimV1
- src/rlm/executor.rs — Assess operator batching
- https://docs.typesafe.ai/primitives/choice.md
- https://docs.typesafe.ai/primitives/noul.md
- https://docs.typesafe.ai/primitives/score.md
- https://docs.typesafe.ai/confidence.md
