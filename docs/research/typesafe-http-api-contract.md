# TypeSafe HTTP API contract for Relic's Rust client

**Status:** research findings
**Decision scope:** Jev wire protocol, existing Rust SDKs, client design for Relic
**Research date:** 2026-09-28

## Decision

Relic should use the `typesafe-ai` crate (Twister915/typesafe-ai, Apache 2.0) which provides a blocking `ureq` backend behind a feature flag, typed primitives (Noul, Choice, Score), and batch support. This matches Relic's synchronous, no-Tokio architecture. If the crate proves insufficient, a thin custom client is ~200 LOC with `ureq` + `serde`.

## Wire protocol

### Endpoint

`POST https://api.typesafe.ai/v1/systemone`

### Authentication

`Authorization: Bearer <API_KEY>` header. Key from `TYPESAFE_API_KEY` env var.

### Request schema

```json
{
  "state": "<string | object | array>",
  "model": "jev-latest",
  "questions": {
    "<question_id>": {
      "type": "noul | choice | score",
      "instructions": "<string | object | array>",
      "criteria": "<type-specific>"
    }
  }
}
```

**State**: the content to evaluate. Shared across all questions in the request.

**Model**: `"jev-latest"` is the alias; responses include exact version (e.g. `"jev-1.13.0"`).

**Questions**: a `Map<String, Question>`. Multiple questions per request, all evaluated against the same state in parallel. Question IDs are for code; they are not sent to the model.

### Question types

**Noul** (binary yes/no probability):
```json
{
  "type": "noul",
  "instructions": "Does this evidence mention certificate renewal?",
  "criteria": {
    "true": "The text describes a certificate renewal action or outcome",
    "false": "The text is unrelated to certificate renewal"
  }
}
```

**Choice** (select one from options, max 255):
```json
{
  "type": "choice",
  "instructions": "How does this evidence relate to the stated objective?",
  "criteria": {
    "supports": "Directly supports the objective with observed action or outcome",
    "contradicts": "Contradicts or disproves the objective",
    "unclear": "Ambiguous or insufficient to judge",
    "irrelevant": "Unrelated to the objective"
  }
}
```

**Score** (ordered scale, 2-10 levels):
```json
{
  "type": "score",
  "instructions": "Rate the relevance of this evidence to the search query.",
  "criteria": [
    "Completely irrelevant; no connection to the query",
    "Tangentially related; mentions similar topics but doesn't address the query",
    "Partially relevant; addresses part of the query or a related subtopic",
    "Directly relevant; addresses the query with specific information",
    "Highly relevant; directly answers the query with concrete evidence"
  ]
}
```

### Response schema

```json
{
  "model": "jev-1.13.0",
  "answers": {
    "<question_id>": { /* type-specific answer */ }
  },
  "usage": {
    "input_tokens": 347,
    "output_tokens": 0
  }
}
```

**Noul answer**:
```json
{ "type": "noul", "noul": 0.92 }
```

**Choice answer**:
```json
{
  "type": "choice",
  "choice": "supports",
  "confidence": 0.85,
  "probabilities": {
    "supports": 0.85,
    "contradicts": 0.05,
    "unclear": 0.07,
    "irrelevant": 0.03
  }
}
```

**Score answer**:
```json
{
  "type": "score",
  "score": 3.2,
  "confidence": 0.72,
  "legend": { "0": "Completely irrelevant", "1": "Tangentially related", "2": "Partially relevant", "3": "Directly relevant", "4": "Highly relevant" },
  "probabilities": { "0": 0.01, "1": 0.03, "2": 0.12, "3": 0.52, "4": 0.32 }
}
```

### Error codes

| Code | Meaning | Action |
|------|---------|--------|
| 401 | Invalid/missing API key | Check `TYPESAFE_API_KEY` |
| 422 | Request validation failed | Fix request schema |
| 429 | Rate limit exceeded | Exponential backoff |
| 529 | Service overloaded | Exponential backoff |

### Batch semantics

Multiple questions in one request, all evaluated against the same state. Adding questions adds minimal latency. This is critical for Relic: each Assess batch can send one Verdict question per evidence item in a single HTTP call.

## Existing Rust SDKs

### Twister915/typesafe-ai (recommended)

- **License**: Apache 2.0
- **Features**: Async (reqwest) + blocking (ureq) via feature flags
- **Typed primitives**: Noul, Choice, Score as Rust enums
- **Batch support**: Multiple questions per `evaluate()` call
- **Observability**: Retry iterators, error introspection with status codes/headers
- **Maturity**: 14 stars, 3 forks, 1 release, 12 commits
- **Fit for Relic**: Blocking `ureq` backend = no Tokio dependency. Matches Relic's sync architecture exactly.

### gilljon/typesafe-ai-rs

- **License**: unlisted
- **Features**: Async (reqwest) + blocking via feature flag
- **Targets**: SDK v0.6.0 parity
- **Extras**: Cancellation tokens, model discovery, retry policies
- **Maturity**: v0.1.0
- **Fit for Relic**: Also viable but heavier (reqwest default, more abstraction).

### Other options

- `codeitlikemiley/typesafe-sdk-rust`, `aoprisan/typesafe-ai-rust-sdk`, `zcoder-run/rust-sysone` — less mature, fewer details available.

### Custom client option

If no crate fits, the API is one POST endpoint:

```rust
use serde::{Deserialize, Serialize};
use ureq;

pub fn system_one(api_key: &str, request: &SystemOneRequest) -> Result<SystemOneResponse, Error> {
    let resp = ureq::post("https://api.typesafe.ai/v1/systemone")
        .set("Authorization", &format!("Bearer {api_key}"))
        .set("Content-Type", "application/json")
        .send_json(serde_json::to_value(request)?)?;
    Ok(resp.into_json()?)
}
```

~200 LOC for full typed request/response structs with `serde`.

## Recommendation

1. **Start with `typesafe-ai` crate** (Twister915) using the blocking `ureq` feature. It's Apache 2.0, matches Relic's sync architecture, and provides typed primitives.
2. **Pin the version** and wrap it behind Relic's existing `ModelBoundary` trait so the crate can be swapped later.
3. If the crate has rough edges (new library, 12 commits), **fall back to custom ~200 LOC client** — the API surface is tiny.

## Sources consulted

- https://docs.typesafe.ai/api.md (HTTP API specification)
- https://docs.typesafe.ai/sdk/javascript.md (JS SDK patterns)
- https://docs.typesafe.ai/sdk/python.md (Python SDK patterns)
- https://github.com/typesafe-ai/typesafe-sdk-js/blob/v0.6.0/src/types.ts (TypeScript type definitions)
- https://github.com/Twister915/typesafe-ai (Rust SDK with ureq blocking backend)
- https://github.com/gilljon/typesafe-ai-rs (Alternative Rust SDK)
