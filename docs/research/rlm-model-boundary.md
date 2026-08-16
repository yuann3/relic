# Bounded RLM and model boundary for Relic v0.1

**Status:** recommended specification  
**Decision scope:** the investigation/query kernel and the model HTTP boundary only  
**Research date:** 2026-08-16

## Decision

Relic v0.1 should implement a **domain-bounded, depth-one RLM scaffold** rather than the paper's Python REPL:

- A root planner sees the sanitized question, corpus/schema statistics, immutable result-handle metadata, the last bounded observation, and remaining budget. It never receives the corpus or an ever-growing chat transcript.
- The root emits `RootActionV1`, either a small acyclic `PlanV1` or a cited `FinishV1`. Rust validates and executes the plan over host-owned evidence handles.
- Seven operator families are enough for the prototype: `search`, `expand`, `relate`, `combine`, `assess`, `select`, and `inspect`. `assess` is the one programmatic semantic-map operator: the host batches a bounded external result set into typed, depth-one child model calls.
- Children receive only explicitly materialized, sanitized evidence packets. They cannot query the store, create evidence, invoke tools, or spawn children. Their assessments are hypotheses; only local evidence handles may support a final claim.
- A single blocking `POST /v1/chat/completions` adapter, with explicit compatibility capabilities, is the v0.1 model boundary. Prefer strict JSON Schema output, fall back only through a configured and tested compatibility mode, and set `stream: false`.
- Every investigation has pre-admission fences for calls, depth, operations, rows returned, bytes materialized, model-egress bytes, output tokens, money when prices are known, and wall time. A child reserves from the parent before it can start.

This retains the relevant RLM semantics—external artifact state, model-generated traversal programs, persistent symbolic intermediate results, and model calls inside a host-executed map—without arbitrary code execution, a provider framework, or context growth. It intentionally gives up the general REPL's arbitrary computation and recursion beyond one semantic leaf level.

## What counts as an RLM here

The RLM paper keeps the long prompt in an external environment, initially shows the root only metadata, and lets generated code inspect the prompt and invoke sub-models programmatically. It distinguishes that from an ordinary agent loop whose model issues tool calls one at a time. Depth 0 (externalization without subcalls) and depth 1 (plain LM subcalls) are distinct experimental conditions; externalization alone can help some tasks, while semantic work over many items benefits from subcalls ([paper §2, Algorithms 1–2](https://arxiv.org/html/2512.24601v3#S2), [§3.2](https://arxiv.org/html/2512.24601v3#S3.SS2), [Observation 2](https://arxiv.org/html/2512.24601v3#S4)).

Relic therefore calls an investigation an RLM investigation only when all four properties are present:

1. the large artifact remains in the local evidence store and is represented to the root by symbolic handles;
2. the root generates a program of traversal operations rather than receiving an automatic fixed top-k;
3. intermediate result handles persist across root turns without copying their contents into model history; and
4. the program can apply bounded semantic child calls over a dynamically selected handle set.

A root asking several agents the same prose question is **multi-agent delegation**, not RLM traversal. A fixed FTS/top-k prompt plus one answer call is retrieval-assisted generation, not RLM traversal. Relic's declarative plan is less expressive than the paper's general code REPL, so documentation should say “bounded RLM scaffold,” not claim equivalence to arbitrary recursive computation.

The official RLM implementation exposes iteration, depth, budget, timeout, token, error, and concurrency limits, and at depth greater than one creates another RLM; at maximum depth it falls back to a plain model call ([pinned constructor and limits](https://github.com/alexzhang13/rlm/blob/03a1774e72cc2341753d7aa2c107928b036fab0f/rlm/core/rlm.py#L50-L108), [child creation](https://github.com/alexzhang13/rlm/blob/03a1774e72cc2341753d7aa2c107928b036fab0f/rlm/core/rlm.py#L708-L783)). Its default local REPL executes in the host process and is explicitly not recommended for production ([official README](https://github.com/alexzhang13/rlm/blob/03a1774e72cc2341753d7aa2c107928b036fab0f/README.md#L80-L96)). Relic replaces that unsafe/general execution boundary with a closed algebra.

## Roles and trust boundaries

| Role | Receives | May do | Must not do |
|---|---|---|---|
| Root planner | Redacted question; operator/schema help; bounded `RootStateV1`; handle metadata; last observation; budget | Emit a plan or cited finish; name a bounded child objective | See the corpus wholesale; supply SQL/path/URL/header; execute a tool; spawn a child directly; claim child prose as evidence |
| Rust coordinator | Local store and source relations; policy; budgets; model profile | Validate plans, resolve handles, reserve budgets, materialize/redact packets, call models, persist trace | Execute model-authored code or shell; relax policy based on model output |
| Child assessor | One bounded `EvidencePacketV1` batch, the redacted question/objective, typed response schema | Classify, compare, extract candidate claims, identify contradictions/missing evidence | Query/follow/read; call a model/tool; create a citation; return an executable action |
| Model HTTP adapter | Already-redacted typed request and a fixed configured endpoint | Perform one Chat Completions request and normalize its response | Choose provider/endpoint; retry ambiguously by itself; cache unvalidated data; stream executable partial JSON |

Artifacts and source bytes are hostile data. System/developer instructions come only from versioned Relic prompt templates. Evidence is encoded as data records with opaque IDs, not concatenated as instructions.

## Protocol

The Rust declarations below are normative in shape, not a commitment to module names. Wire objects use `snake_case`, reject unknown fields, and carry `version: 1`. Length/range rules are host validation in addition to JSON Schema.

### Result handles and citation handles are different

```rust
struct ResultHandleV1 {
    version: u8,                       // 1
    id: HandleId,                      // random, investigation-scoped
    investigation_id: InvestigationId,
    kind: HandleKind,                  // evidence_set | assessment_set | inspection
    snapshot_set_hash: Blake3Hash,     // immutable source revision set
    lineage_hash: Blake3Hash,          // canonical operator + parent lineages
    count: u32,
    sanitized_bytes: u64,
    created_at_state: u32,
}

struct EvidenceHandleV1 {
    version: u8,                       // 1
    id: EvidenceId,                    // opaque `ev1_...`
    source_revision: SourceRevisionId,
    locator: EvidenceLocator,          // logical ordinal and optional byte range
    parser_version: String,
    redaction_revision: String,
    sanitized_content_hash: Blake3Hash,
}

enum EvidenceLocator {
    Logical { ordinal: u64 },
    Bytes { ordinal: u64, start: u64, end_exclusive: u64 },
}
```

`ResultHandleV1` is a capability for an immutable set held outside model context. Only the coordinator can dereference it, and only during its investigation. It is never a final citation. `EvidenceHandleV1` identifies one sanitized observation at one immutable source revision. It can be resolved locally for display and is the only model-visible citation primitive.

Generate `HandleId` from at least 128 random bits. Generate `EvidenceId` from keyed BLAKE3 over the canonical internal identity using a local installation key, so IDs do not expose paths, ordinals, or unkeyed source fingerprints. The internal record—not the short displayed ID—is authoritative. A citation resolver must verify the revision, locator, parser version, redaction revision, and content hash before returning text. A stale or cross-investigation handle is a typed error, never silently rebound. The exact construction of `SourceRevisionId` and byte-level locators remains owned by the source/evidence-contract ticket; the query boundary requires the fields above regardless of parser choice.

A model is given only packets made from allowed handles:

```rust
struct EvidencePacketV1 {
    packet_id: String,
    items: Vec<EvidenceItemV1>,        // <= policy batch/item caps
}

struct EvidenceItemV1 {
    item_id: String,                   // request-local, not a source identity
    citations: Vec<EvidenceHandleV1>,
    source_kind: SourceKind,
    observed_at: Option<Timestamp>,
    sanitized_text: String,
    truncation: Option<TruncationV1>,
}
```

The packet records exact UTF-8 bytes and citation allowlist sent to the call. Truncation is explicit; the model may not infer omitted prefix/suffix content.

### Root action and plan

```rust
struct RootActionV1 {
    version: u8,
    expected_state_version: u32,
    kind: RootActionKind,              // plan | finish
    plan: Option<PlanV1>,              // exactly one of plan/finish
    finish: Option<FinishV1>,
}

struct PlanV1 {
    ops: Vec<PlanOpV1>,                // 1..=8, acyclic, unique op_id
}

#[serde(tag = "op", rename_all = "snake_case")]
enum PlanOpV1 {
    Search(SearchV1),
    Expand(ExpandV1),
    Relate(RelateV1),
    Combine(CombineV1),
    Assess(AssessV1),
    Select(SelectV1),
    Inspect(InspectV1),
}

enum InputRefV1 {
    Handle(HandleId),                  // existing immutable result
    Output(OpId),                      // earlier op in this plan only
}
```

The seven operators are closed and deliberately not generic tools:

| Operator | Input → output | Typed fence and purpose |
|---|---|---|
| `search` | `SearchExprV1` → `evidence_set` | FTS terms/phrases plus enumerated source/time/cwd/status filters; ≤12 terms, ≤200 results; never SQL/regex/path glob |
| `expand` | evidence set → evidence set | Before/after an observed ordinal within `same_source` or `same_session`; each side ≤32, total capped |
| `relate` | evidence set → evidence set | Follow one host-defined edge: `same_session`, `same_cwd_time`, `git_state`, or `same_repo`; per-seed and total caps |
| `combine` | evidence sets → evidence set | `union`, `intersection`, or `difference`, stable dedupe/order, explicit limit; no expression evaluator |
| `assess` | evidence set → assessment set | Host materializes ≤64 selected items, batches ≤4 items/call, and makes only `ChildResultV1` calls; this is the bounded programmatic semantic map |
| `select` | assessment set → evidence set | Filter only by enumerated verdict, cited/contradicted flag, and confidence band; stable order and limit |
| `inspect` | evidence set → inspection + observation | Return ≤12 exact sanitized packets, ≤2 KiB/item, to the next root state; no arbitrary read |

Every operator declares an `op_id`, input(s), limit, and expected output kind. The validator resolves types before execution, rejects forward references/cycles, checks the plan against the current `expected_state_version`, computes a worst-case reservation, and only then runs the plan. An operator cannot accept a raw filename, URI, SQL fragment, environment name/value, shell command, or provider option. Unknown enum values and fields fail closed.

Example:

```json
{
  "version": 1,
  "expected_state_version": 3,
  "kind": "plan",
  "plan": {
    "ops": [
      {"op":"search","op_id":"s1","terms":["certificate","renew"],"match":"any","limit":120},
      {"op":"expand","op_id":"e1","input":{"output":"s1"},"scope":"same_session","before":8,"after":16,"limit":200},
      {"op":"relate","op_id":"g1","input":{"output":"e1"},"relation":"git_state","per_seed":2,"limit":80},
      {"op":"assess","op_id":"a1","input":{"output":"g1"},"objective":"find observed renewal steps and contrary outcomes","max_items":32,"batch_size":4},
      {"op":"select","op_id":"k1","input":{"output":"a1"},"verdicts":["supports","contradicts"],"limit":20},
      {"op":"inspect","op_id":"i1","input":{"output":"k1"},"limit":12,"max_item_bytes":2048}
    ]
  },
  "finish": null
}
```

The host may return fewer rows/items than requested. It never expands a limit to satisfy model intent.

### Child and final contracts

```rust
struct ChildResultV1 {
    version: u8,
    packet_id: String,
    assessments: Vec<ItemAssessmentV1>,
}

struct ItemAssessmentV1 {
    item_id: String,                   // must be in this packet
    verdict: Verdict,                  // supports | contradicts | unclear | irrelevant
    confidence: Confidence,            // low | medium | high; advisory only
    claims: Vec<ClaimV1>,              // bounded text, not evidence itself
    missing: Vec<String>,
}

struct ClaimV1 {
    text: String,                      // <= 400 UTF-8 bytes
    supports: Vec<EvidenceId>,         // subset of packet allowlist
    contradicts: Vec<EvidenceId>,      // subset of packet allowlist
}

struct FinishV1 {
    status: AnswerStatus,              // answered | partial | abstained
    answer: String,
    claims: Vec<FinalClaimV1>,
    procedure: Vec<ProcedureStepV1>,
    contradictions: Vec<EvidenceId>,
    missing: Vec<String>,
}
```

The coordinator rejects unknown `packet_id`, `item_id`, or citation IDs and any citation not present in that call's allowlist. It also enforces string, list, and aggregate byte caps. Child text is never promoted to evidence. A final claim or procedure step must resolve to local evidence; all user-visible procedural assertions must occur in a structured claim/step, not only in uncited `answer` prose.

Structured output is a syntax aid, not a trust boundary. The official OpenAI schema warns that generated function arguments may be invalid JSON or contain hallucinated parameters and directs callers to validate them ([OpenAPI schema](https://github.com/openai/openai-openapi/blob/2186421dca0cca7c1e67caa7739005e8b1ccc4dd/openapi.yaml#L31003-L31039)). Even strict JSON Schema supports only a subset of JSON Schema ([response format](https://github.com/openai/openai-openapi/blob/2186421dca0cca7c1e67caa7739005e8b1ccc4dd/openapi.yaml#L58388-L58445), [function strictness](https://github.com/openai/openai-openapi/blob/2186421dca0cca7c1e67caa7739005e8b1ccc4dd/openapi.yaml#L40577-L40614)). Relic must always deserialize into its Rust types and run semantic validation.

## Evidence sufficiency and abstention

The following rules are deterministic gates, not prompt advice:

1. Every factual final claim and every procedure step has at least one resolvable supporting `EvidenceId`. A child result, confidence label, search score, or cache entry is not support.
2. A claim of historical success requires observed success evidence appropriate to the source contract (for example, recorded exit status or an explicit verifier). A later command or absence of a recorded failure cannot silently become success.
3. Inference is labeled as inference. It cites the observations it is based on and names missing decisive evidence.
4. A material contradictory item is either resolved by a cited claim or listed in `contradictions`. It cannot be dropped merely because a child scored it low.
5. Truncated evidence cannot support a claim about omitted content.
6. If required support is missing, or an unresolved contradiction changes the safe procedure, the validator downgrades `answered` to `partial` or `abstained`. `abstained` requires a non-empty `missing` list and contains no executable procedure.
7. Any forged/stale citation, uncited procedural step, budget-invalid result, `finish_reason: length`, refusal, or invalid contract fails final validation. The coordinator may ask for one bounded correction if budget remains; otherwise it returns typed partial/abstention, never free-form salvage.

This proves citation integrity and declared sufficiency, not semantic truth. Whether the cited text actually entails the claim remains a model/evaluation problem and is an explicit unknown.

## Bounded root context

`RootStateV1` is rebuilt by the host for every root call. Relic does not resend the full Chat Completions history. It contains:

- the redacted question and fixed prompt/schema identifiers;
- corpus/source counts and available relation kinds;
- at most 64 live result-handle summaries (ID, kind, count, byte total, short host-generated label, lineage hash prefix);
- the last plan outcome and at most 12 inspected evidence items;
- validated candidate claim summaries with citations, bounded to 16 KiB total; and
- consumed/remaining budget.

The complete plan, call, and evidence trace remains external and content-addressed. A root can retrieve artifact content only through `inspect`, and a child only through `assess`. Host-generated state eviction is deterministic (unreferenced oldest handles first); evicted handles remain in the trace but cannot be used without a future explicit resume mechanism.

Default per-request encoded-size caps are 64 KiB for a root request and 24 KiB for a child request, including templates and schemas. The investigation-wide model-egress cap is lower than the sum of per-call maxima and therefore remains the final fence. Bytes, not provider token counts, are the privacy/context hard limit; token estimates and reported usage are additional accounting.

## Budget and cost lease

```rust
struct BudgetV1 {
    wall_ms: u64,
    root_calls: u16,
    plan_ops: u16,
    child_calls: u16,                  // logical model calls, cache hit or miss
    provider_attempts: u16,            // actual HTTP attempts
    max_concurrency: u8,
    max_depth: u8,
    rows_returned: u64,
    evidence_handles: u32,
    local_materialized_bytes: u64,
    model_egress_bytes: u64,
    estimated_input_tokens: u64,
    reserved_output_tokens: u64,
    max_cost_microusd: Option<u64>,
}
```

Recommended prototype defaults:

| Fence | Default |
|---|---:|
| Wall clock | 180 s |
| Root calls | 8 |
| Plan operations | 48 total, 8/plan |
| Child calls / actual provider attempts | 16 / 18 |
| Concurrent child calls | 4 |
| Recursion depth | 1; children have depth 0 and no call capability |
| Rows returned into handles | 2,000 |
| Live evidence handles | 512 |
| Local materialized sanitized bytes | 4 MiB |
| Total model egress | 256 KiB |
| Estimated input tokens | 96,000 |
| Reserved output tokens | 24,000; 1,536/root and 768/child maxima |
| Remote spend | US$1.00 when a price table is configured |

Before an operation begins, the coordinator calculates its worst-case rows, bytes, calls, output tokens, and configured price and atomically reserves them from the investigation. `assess` subdivides its reservation among batches before admitting workers. Unused rows/bytes/tokens may be released only from measured local counts or authoritative response usage; if usage is absent, keep the reservation consumed. Checks after a call are accounting, not a hard fence.

A currency fence is enforceable only when the profile supplies input/output price per million tokens and a tokenizer/upper-bound policy. A remote profile without prices requires explicit `--allow-unknown-cost`; call, byte, output-token, and wall fences still apply and the final cost is `unknown`, not zero. A local endpoint records zero provider price but still consumes all non-currency budgets.

Budget exhaustion stops admission immediately. Already committed claims may be returned as `partial`; otherwise the result is `abstained` with `missing`. Children never borrow unreserved remainder, and no child receives the parent's full budget object as authority.

## Investigation and call state machines

### Investigation

```text
Created
  -> RootPrepared -> RootInFlight -> ActionValidated
  -> PlanReserved -> ExecutingOps -> ObservationCommitted --+
  |                                                       |
  +-------------------------------------------------------+
  -> FinishValidating -> Completed | Partial | Abstained

Any nonterminal state -> Cancelling -> Cancelled
Any admission point   -> BudgetExhausted -> Partial | Abstained
Any invariant/storage failure -> Failed
```

`state_version` increments only after an observation or validated finish is durably committed. A plan names the version it read. A duplicate or late response for an older version is logged and discarded. Trace events are append-only and include causal parent IDs.

### One model call

```text
Prepared -> ContractChecked -> LogicalBudgetReserved -> CacheLookup
  cache hit  -> ContractRevalidated -> Committed
  cache miss -> ProviderBudgetReserved -> PayloadAudited -> InFlight
             -> ResponseBounded -> Parsed -> ContractValidated -> Committed

InFlight -> RetryEligible -> Backoff -> ProviderBudgetReserved -> InFlight
Any state -> CancelObserved -> Cancelled
Any validation failure -> Correctable | Failed
```

Only a fully received, size-bounded, parsed, contract-valid response can enter the cache or mutate investigation state. Cancellation and deadline tokens are checked before reservation, before every SQLite/operator batch, before each child admission, and before commit.

## Model HTTP boundary

### One adapter, explicit capabilities

Do not add a generic provider SDK. Implement one `ChatCompletionsClient` against a configured base URL plus `/chat/completions`, with bearer auth optional. The official API schema defines `https://api.openai.com/v1`, bearer authentication, and `POST /chat/completions` ([official OpenAPI root](https://github.com/openai/openai-openapi/blob/2186421dca0cca7c1e67caa7739005e8b1ccc4dd/openapi.yaml#L1-L20), [Chat request](https://github.com/openai/openai-openapi/blob/2186421dca0cca7c1e67caa7739005e8b1ccc4dd/openapi.yaml#L33087-L33433)). Relic's compatibility target is that request/response shape, not every OpenAI feature.

```rust
struct ModelProfileV1 {
    base_url: Url,
    auth: AuthMode,                    // none | bearer_secret_ref
    requested_model: String,
    model_revision: Option<String>,    // user/operator assertion for persistent caching
    output_mode: OutputMode,
    token_limit_field: TokenLimitField,
    root_max_output_tokens: u32,
    child_max_output_tokens: u32,
    prices: Option<PriceTableV1>,
}

enum OutputMode {
    OpenAiJsonSchema,                  // nested json_schema{name,schema,strict}
    FlatJsonSchema,                    // llama.cpp documented compatibility shape
    NamedFunction,                    // exactly one forced submit_* function
    JsonObject,
    PromptedJson,                     // last resort; must pass model check
}

enum TokenLimitField { MaxCompletionTokens, MaxTokens }
```

A profile is a small wire-variance declaration, not provider behavior/plugins. The model cannot change it. `relic model check` sends a synthetic, non-sensitive schema and records a capability receipt containing endpoint origin hash, model requested/returned, output mode, token-limit field, response/usage fields observed, Relic version, and time. It must not probe with user artifacts or silently downgrade a failed strict mode during an investigation.

The request includes only `model`, `messages`, `stream: false`, the configured output constraint, and one configured output-token limit. Named-function mode additionally forces one function and disables parallel tool calls where accepted. Avoid optional sampling/provider fields unless the profile explicitly owns them. Always request one choice and reject multiple choices.

Normalize a response into:

```rust
struct ModelResponseV1 {
    response_id: Option<String>,
    returned_model: Option<String>,
    finish_reason: FinishReason,
    payload: ModelPayload,             // content JSON or one named tool's arguments
    refusal: Option<String>,
    usage: Option<UsageV1>,
    system_fingerprint: Option<String>,
}
```

The current OpenAI schema prefers `max_completion_tokens`, marks `max_tokens` deprecated/incompatible with some reasoning models, defines `response_format: json_schema`, and exposes response model/ID/usage/finish reason ([request fields](https://github.com/openai/openai-openapi/blob/2186421dca0cca7c1e67caa7739005e8b1ccc4dd/openapi.yaml#L33120-L33425), [response](https://github.com/openai/openai-openapi/blob/2186421dca0cca7c1e67caa7739005e8b1ccc4dd/openapi.yaml#L33473-L33625)). These fields are optional provenance/accounting inputs in Relic except the one returned choice and a terminal finish reason.

### OpenAI-compatible variance and local models

“OpenAI-compatible” is not a versioned conformance guarantee:

- vLLM documents `/v1/chat/completions`, but also documents ignored fields (`user`), model-dependent parallel tool behavior, extra parameters, and sampling defaults inherited from a model repository's `generation_config.json` ([vLLM server docs](https://github.com/vllm-project/vllm/blob/d4801990a45792c7081652f8ebea4ee56ceb67f9/docs/serving/online_serving/openai_compatible_server.md#L1-L80)). It supports the official nested `response_format` JSON Schema form ([structured-output docs](https://github.com/vllm-project/vllm/blob/d4801990a45792c7081652f8ebea4ee56ceb67f9/docs/features/structured_outputs.md#L1-L95)) and named/required tool modes, but tool parsing and constraints can depend on mode, parser, template, and strict settings ([tool-calling docs](https://github.com/vllm-project/vllm/blob/d4801990a45792c7081652f8ebea4ee56ceb67f9/docs/features/tool_calling.md#L1-L105)).
- llama.cpp explicitly says it makes no strong compatibility claim. Its server supports synchronous/streaming Chat Completions but documents a flat schema-constrained `response_format` variant; OpenAI-style function calling requires `--jinja` and may require a compatible template ([server docs](https://github.com/ggml-org/llama.cpp/blob/ad1de39e0708e3ced9c71bb3c82d93a2c046a73f/tools/server/README.md#L1249-L1265), [function calling](https://github.com/ggml-org/llama.cpp/blob/ad1de39e0708e3ced9c71bb3c82d93a2c046a73f/tools/server/README.md#L1328-L1338)).

Therefore ship built-in configuration presets, not different client implementations:

- `openai`: `OpenAiJsonSchema` + `MaxCompletionTokens` + bearer secret;
- `vllm`: `OpenAiJsonSchema`, token field confirmed by `model check`, auth optional;
- `llama_cpp`: `FlatJsonSchema`, `MaxTokens`, auth optional; named tools only after a successful configured check;
- `custom`: user declares both capabilities and must pass `model check`.

HTTP is allowed without TLS only for a literal loopback address (`127.0.0.0/8` or `::1`). This provides local-model support without bundling a model or local inference runtime. Remote origins require HTTPS.

### Rust transport recommendation

Use audited `ureq` 3 with `default-features = false`, features `rustls` and `json`, behind Relic's tiny trait. At the inspected source revision, ureq is blocking by design, its default features are `rustls` plus `gzip`, and JSON is optional ([README](https://github.com/algesten/ureq/blob/5e803fc4ce031a0d667dc6cdb97d1413ac8cdab9/README.md#L20-L40), [feature table](https://github.com/algesten/ureq/blob/5e803fc4ce031a0d667dc6cdb97d1413ac8cdab9/Cargo.toml#L19-L36)). Disabling defaults avoids transparent compressed-body expansion; enable only what this boundary needs.

Configure the agent explicitly:

- no environment proxy (`proxy(None)`), no redirects (`max_redirects(0)`), fixed user agent;
- HTTPS-only for remote profiles; loopback HTTP is a separate validated case;
- connect/receive/global timeouts derived from the call deadline;
- response header cap and an explicit 1 MiB response-body cap;
- bearer secret inserted only at send time and never formatted with `Debug`/logged.

These are not safe defaults to assume: ureq's default configuration reads proxy environment variables and follows up to ten redirects ([source](https://github.com/algesten/ureq/blob/5e803fc4ce031a0d667dc6cdb97d1413ac8cdab9/src/config.rs#L850-L890)). It does expose an end-to-end global timeout from DNS through body read ([source](https://github.com/algesten/ureq/blob/5e803fc4ce031a0d667dc6cdb97d1413ac8cdab9/src/config.rs#L662-L704)) and explicit body limits ([source](https://github.com/algesten/ureq/blob/5e803fc4ce031a0d667dc6cdb97d1413ac8cdab9/src/body/mod.rs#L29-L48)).

`reqwest`/Tokio remains an alternative only if the cancellation prototype proves the blocking design unacceptable. Do not adopt an async runtime merely for token streaming.

## Streaming boundary

Relic v0.1 does not consume model streams. It sets `stream: false`, validates a complete root/child object atomically, and emits its own local progress events (`call_started`, `call_finished`, `op_committed`) to the TUI.

This is a deliberate correctness boundary. The official stream schema sends content and tool arguments as deltas, may use a final empty-choice usage chunk, and warns that interrupted streams may never deliver total usage ([stream options](https://github.com/openai/openai-openapi/blob/2186421dca0cca7c1e67caa7739005e8b1ccc4dd/openapi.yaml#L31766-L31798), [stream response](https://github.com/openai/openai-openapi/blob/2186421dca0cca7c1e67caa7739005e8b1ccc4dd/openapi.yaml#L33662-L33767)). Executing a partially streamed plan would bypass schema, budget, and state-version validation.

A future stream implementation must accumulate bounded deltas, reject duplicate/out-of-order tool fragments, execute nothing until terminal validation, and treat usage as unknown after interruption. Streaming is not required for RLM semantics.

## Provenance and caching

Persist this envelope for every root and child logical call:

```rust
struct CallProvenanceV1 {
    call_id: CallId,
    parent_call_id: Option<CallId>,
    role: ModelRole,                   // root | child
    investigation_id: InvestigationId,
    state_version: u32,
    relic_version: String,
    kernel_version: u16,
    prompt_template_id: String,
    prompt_template_hash: Blake3Hash,
    response_schema_hash: Blake3Hash,
    redaction_revision: String,
    model_profile_hash: Blake3Hash,    // no secret values
    endpoint_origin_hash: Blake3Hash,
    requested_model: String,
    configured_model_revision: Option<String>,
    returned_model: Option<String>,
    provider_response_id: Option<String>,
    system_fingerprint: Option<String>,
    sanitized_request_hash: Blake3Hash,
    sanitized_response_hash: Option<Blake3Hash>,
    finish_reason: Option<String>,
    usage: Option<UsageV1>,
    attempt_count: u8,
    cache: CacheDisposition,
}
```

Do not claim deterministic replay from `seed` or `system_fingerprint`: the official schema calls seed best-effort, says determinism is not guaranteed, and now marks these fields deprecated ([schema](https://github.com/openai/openai-openapi/blob/2186421dca0cca7c1e67caa7739005e8b1ccc4dd/openapi.yaml#L33395-L33420), [response fingerprint](https://github.com/openai/openai-openapi/blob/2186421dca0cca7c1e67caa7739005e8b1ccc4dd/openapi.yaml#L33565-L33587)). Provenance supports comparison and audit, not reproduction by assertion.

Use two caches:

1. **Deterministic operator cache:** persistent and content-addressed by canonical operator, input lineage/snapshot hashes, kernel/parser/redaction versions, and limits. It stores only sanitized results.
2. **Model response cache:** exact-call, investigation-scoped by default. Key the canonical sanitized request, role, prompt/schema hashes, model profile semantics, requested model/revision, output mode, decoding/token parameters, and Relic/kernel/redaction versions. Auth secret and volatile call IDs are excluded.

A persistent model cache is allowed only when the user configures a model revision/immutable deployment identifier and an expiry policy. Mutable model aliases otherwise make a cross-run hit misleading. A cache hit retains the original response/provenance and adds a new `cache_hit_of`; it is revalidated against the current Rust contract and evidence allowlist before use. Any prompt, schema, model profile/revision, policy/redaction, source snapshot, or kernel change misses. Never cache a refusal, truncated response, invalid contract, ambiguous transport outcome, or cancelled call.

Redaction precedes hashing and persistence. The cache is not permission to store raw artifacts.

## Retry, idempotency, and cancellation

### Retry

Chat Completions is a `POST`. HTTP defines PUT/DELETE/safe methods as idempotent but not POST by default; a client should not automatically retry a non-idempotent request unless it knows the semantics are idempotent or can detect it was never applied ([RFC 9110 §9.2.2](https://www.rfc-editor.org/rfc/rfc9110.html#section-9.2.2)). The inspected OpenAI schema defines no portable idempotency-key contract for Chat Completions. Relic's `CallId` deduplicates local commits; it does not deduplicate provider billing/generation.

Default policy:

- one attempt;
- at most one retry for a transport failure proven to occur before request bytes were sent, or an explicit `429` rejection honoring bounded `Retry-After`;
- no automatic retry for disconnect after send, timeout in flight, `5xx`, malformed success body, or unknown transport phase;
- a schema/semantic correction is a new root/child logical call with the validation diagnostic, consuming a call and full budget reservation;
- every attempt shares the logical call ID plus an attempt ordinal and cannot commit twice.

If the transport cannot prove the failure phase, classify it `ambiguous` and do not retry. A user-initiated retry is a new logical call and may return a different answer/cost.

### Cancellation

There is no portable Chat Completions cancel endpoint. Cancellation therefore means:

1. set the investigation cancellation token;
2. stop all new operator/child admissions and cancel queued reservations;
3. interrupt local SQLite work through its progress/deadline hook;
4. mark in-flight logical calls `cancel_requested` and reject every late response/cache commit;
5. bound network lifetime by the pre-existing ureq global deadline;
6. for interactive Ctrl-C, durably append `Cancelled`, then terminate the CLI process so the OS closes sockets.

Because ureq uses blocking I/O, Relic cannot promise that a provider stopped computing or did not bill after the client closed. It must report such usage/cost as unknown. Individual speculative children are not detached and abandoned; an `assess` batch waits for admitted calls or their deadlines. The prototype must prove that whole-investigation Ctrl-C reaches a terminal CLI state promptly; if graceful cancellation cannot meet the acceptance gate, switch the adapter to cancellation-safe async reqwest rather than weakening the contract.

## Security and privacy boundary

1. Redact/exclude before indexing as required by the map, then re-run the current egress redactor when materializing every root/child packet. Redact the user's question and model output before logs/cache/display persistence as well.
2. The payload audit records provider profile, endpoint origin, role, exact sanitized byte count, request hash, evidence IDs, redaction revision, and user-visible preview. It never records bearer bytes.
3. Endpoint, path, auth mode, headers, proxy, and model are configuration—not plan fields. Disable redirects and ambient proxies. Require HTTPS remotely and literal loopback for cleartext local endpoints.
4. The model never receives raw source paths when a pseudonymous source ID suffices, source fingerprints, secret-placeholder maps, environment values, or unrelated handle contents.
5. Evidence strings are JSON data under a child/root template that explicitly treats them as untrusted historical text. Escape terminal control sequences. Prompt injection can influence classification, but it gains no code, tool, network, store-query, or policy capability.
6. Validate packet citation allowlists, output sizes, Unicode/control characters, enum values, state version, and all plan semantic fences. Unknown fields fail closed.
7. Response bodies are capped before JSON parsing. Compression is disabled. Error bodies and provider headers are bounded and redacted before trace storage.
8. Model output can propose search terms and host-enumerated relations only. It cannot construct SQL, regex, glob, filesystem access, HTTP, shell, commands, or verifier execution.
9. Recall remains read-only. A `FinishV1` is evidence-backed text, not permission to run, compile, activate, or validate commands.

## Alternatives considered

| Alternative | Disposition |
|---|---|
| Official-style Python/local RLM REPL | Reject for v0.1. Preserves full paper expressivity but adds host code execution; the official project warns its in-process local environment is not production-safe. |
| Rhai, JavaScript, Lua, WASM, or embedded Python | Reject. Adds a language/runtime, resource metering and sandbox surface before evidence quality is proven. |
| Model-generated SQL | Reject. It collapses policy, performance, and data-egress boundaries into string validation and exposes schema details. Host operators should own SQL. |
| Generic agent/tool framework or MCP host | Reject. Provider/tool abstractions, ambient capabilities, and transcript management are larger than the seven-operation domain kernel. |
| OpenAI Responses API as the only boundary | Defer. It has richer event/cancel/state semantics, but Chat Completions is the narrower common local-server surface and Relic needs no hosted tools. |
| Fixed FTS/top-k plus one LLM | Keep as a baseline and offline fallback. It cannot conditionally expand/join or programmatically map semantic work, so it does not prove the RLM claim. |
| Hand-coded fixed investigation pipeline | Keep as a benchmark. It is safer/cheaper but cannot adapt traversal to the question. If it matches the RLM on held-out tasks, prefer it. |
| Pure multi-agent delegation | Reject as the architecture. Children without external symbolic traversal are not RLM semantics and multiply context/cost without evidence capabilities. |
| Async reqwest/Tokio and streaming | Defer pending cancellation/footprint measurements. Adopt only if the blocking adapter fails the cancellation gate. |

## Open questions and prototype-owned facts

- The source/evidence ticket must finalize revision IDs, locators, source-quality grades, and what constitutes observed success for each adapter.
- The storage ticket must prove relation indexes and cancellable/deadline-bounded SQLite queries for `expand`/`relate`; “rows scanned” is not claimed from rows returned.
- It is unknown whether small local models reliably emit useful multi-turn `PlanV1`; constrained JSON only guarantees shape. Prompt/model-specific capability receipts may be necessary.
- The seven operators may be too weak or unnecessarily broad. Only held-out distributed-evidence fixtures can justify adding/removing one.
- JSON Schema subsets and token-limit fields vary by server/version/model. The presets above require real conformance tests, not user-agent sniffing.
- Monetary pre-reservation is approximate without a provider tokenizer and immutable prices. Byte/call/output fences are the hard cross-provider controls.
- Citation validity does not prove entailment. A challenger or deterministic source-specific verifier may be needed, but is not another unrestricted agent.
- ureq's blocking cancellation behavior, TLS/binary-size delta, and four-worker RSS must be measured against Relic's build gates.
- The best batching unit (event, episode, or source-joined packet) is an evaluation result, not a protocol fact.

## Prototype acceptance plan

The follow-on bounded-recall prototype is accepted only if all gates below pass. Record every request/response hash, state transition, reservation, and fixture result in machine-readable JSONL.

### 1. Protocol and executor

- Round-trip golden JSON for every `PlanOpV1`, `RootActionV1`, `ChildResultV1`, and `FinishV1`; reject unknown fields, wrong handle kinds, forward refs, cycles, duplicate op IDs, over-limit strings/lists, stale state versions, and invalid plan/finish combinations.
- Property/fuzz tests never panic on arbitrary JSON and never execute an operator before full structural, semantic, and budget validation.
- A fake model plus in-memory corpus deterministically executes a two-turn trace: `search -> expand/relate -> assess -> select -> inspect -> finish`. At least two child batches are selected from a runtime-created handle, demonstrating programmatic semantic mapping rather than hard-coded delegation.
- The root request never contains the corpus; each request stays within 64 KiB/root and 24 KiB/child, and investigation egress never exceeds its 256 KiB lease.

### 2. Evidence integrity and sufficiency

- Tests forge, alter, reuse across investigations, and stale an `EvidenceId`; all fail before display/commit.
- Removing any citation required by an answer fixture makes final validation fail or downgrade. An uncited procedure step is impossible to commit.
- Build 12 seeded investigations: 8 answerable cases whose decisive evidence requires conditional traversal across at least two source/relation kinds and is absent from the initial FTS top 20; 4 intentionally insufficient/contradictory cases.
- On the 8 answerable cases: zero unsupported final claims, zero uncited steps, and every displayed citation resolves to the exact sanitized fixture bytes/revision. On the 4 insufficient cases: zero claims of success and all terminate `partial`/`abstained` with the expected missing evidence.
- Report FTS-only, fixed top-20 + one model, fixed hand-coded pipeline, and bounded RLM results separately. If the bounded RLM does not win on the cases designed to require conditional joins after equalizing model/call budgets, do not ship the RLM label.

### 3. Budget, cache, retry, and cancellation

- Boundary tests set each lease dimension one unit below demand and prove rejection occurs before the violating operation/request. Maximum active children is four and observed recursion depth is exactly one.
- A repeated exact child call hits the investigation cache with zero new HTTP attempts. Mutating any source snapshot, evidence packet, prompt/schema/redaction version, model/profile/revision, or output parameter forces a miss. Invalid/refused/truncated/cancelled calls never hit later.
- A mock server exercises: connect failure before send, `429` + `Retry-After`, `500`, disconnect after body send, timeout, oversized body, invalid JSON, valid JSON with unknown fields, refusal, and `finish_reason: length`. Only the first two retry-eligible cases retry, never beyond provider-attempt and wall leases; no logical call commits twice.
- A hanging-server process test sends Ctrl-C during a root and during a four-child batch. Within 250 ms the coordinator stops admissions, writes `Cancelled`, and the CLI exits 130; no late response changes the trace/cache. Report provider usage/cost unknown. If this cannot be implemented safely with ureq, the transport decision fails and reqwest cancellation is prototyped.

### 4. Model compatibility and streaming

- Contract tests cover canonical OpenAI response/content, named tool arguments, vLLM documented nested schema behavior, and llama.cpp's documented flat schema shape, including absent usage/system fingerprint and extra provider response fields.
- `relic model check` passes against one real configured remote OpenAI-schema endpoint and one real local endpoint (vLLM or llama.cpp) using only a synthetic plan. CI may use mock servers; release evidence must include the real smoke tests and exact server/model versions.
- Every v0.1 request contains `stream: false`; an `text/event-stream` response is rejected. No partial content/action reaches the executor.

### 5. Privacy and footprint

- Seed API keys, PEM text, high-entropy assignments, private paths/hosts, control sequences, and an evidence prompt injection. Assert zero raw seeded secrets in SQLite, JSONL, cache, stderr/stdout, and captured model HTTP bodies; placeholders cannot be resolved by either model role.
- Redirect-to-another-origin and proxy-environment tests send zero authorization/artifact bytes to the second server/proxy. Remote HTTP and non-loopback local HTTP configurations fail before DNS/connect.
- `cargo tree` contains no Rhai/Python/JS/WASM engine, agent framework, provider SDK, or async runtime in the ureq build. Measure stripped binary and peak RSS deltas for ureq `rustls+json` and the four-child worker pool; feed actual numbers to the architecture ticket rather than assuming they meet the map.

## Primary sources

- Zhang et al., *Recursive Language Models*, arXiv v3: [mechanism and agent comparison](https://arxiv.org/html/2512.24601v3#S2), [experiments/depth](https://arxiv.org/html/2512.24601v3#S3.SS2), [limitations](https://arxiv.org/html/2512.24601v3#S7).
- Official RLM repository, pinned commit `03a1774e72cc2341753d7aa2c107928b036fab0f`: [`RLM` implementation](https://github.com/alexzhang13/rlm/blob/03a1774e72cc2341753d7aa2c107928b036fab0f/rlm/core/rlm.py), [environment warning](https://github.com/alexzhang13/rlm/blob/03a1774e72cc2341753d7aa2c107928b036fab0f/README.md#L80-L96).
- OpenAI official OpenAPI 3.1 schema, pinned commit `2186421dca0cca7c1e67caa7739005e8b1ccc4dd`: [Chat request/response and structured output](https://github.com/openai/openai-openapi/blob/2186421dca0cca7c1e67caa7739005e8b1ccc4dd/openapi.yaml).
- vLLM official source/docs, pinned commit `d4801990a45792c7081652f8ebea4ee56ceb67f9`: [OpenAI-compatible server](https://github.com/vllm-project/vllm/blob/d4801990a45792c7081652f8ebea4ee56ceb67f9/docs/serving/online_serving/openai_compatible_server.md), [structured outputs](https://github.com/vllm-project/vllm/blob/d4801990a45792c7081652f8ebea4ee56ceb67f9/docs/features/structured_outputs.md), [tool calling](https://github.com/vllm-project/vllm/blob/d4801990a45792c7081652f8ebea4ee56ceb67f9/docs/features/tool_calling.md).
- llama.cpp official server docs, pinned commit `ad1de39e0708e3ced9c71bb3c82d93a2c046a73f`: [OpenAI-compatible endpoint and variance](https://github.com/ggml-org/llama.cpp/blob/ad1de39e0708e3ced9c71bb3c82d93a2c046a73f/tools/server/README.md#openai-compatible-api-endpoints).
- ureq official source, pinned commit `5e803fc4ce031a0d667dc6cdb97d1413ac8cdab9`: [README/features](https://github.com/algesten/ureq/blob/5e803fc4ce031a0d667dc6cdb97d1413ac8cdab9/README.md), [configuration/timeouts](https://github.com/algesten/ureq/blob/5e803fc4ce031a0d667dc6cdb97d1413ac8cdab9/src/config.rs), [body limits](https://github.com/algesten/ureq/blob/5e803fc4ce031a0d667dc6cdb97d1413ac8cdab9/src/body/mod.rs).
- IETF, [RFC 9110 §9.2.2, Idempotent Methods](https://www.rfc-editor.org/rfc/rfc9110.html#section-9.2.2).

The prior local Relic detailed design and RLM/Continual Harness paper audit under `/Users/yuan/Developer/Research` were used to identify questions and terminology; recommendations above were checked against the primary artifacts listed here.
