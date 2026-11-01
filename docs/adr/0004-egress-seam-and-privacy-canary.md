# 0004. Model adapters accept only a redacted, capped, ledgered `EgressRequest`, and a privacy canary gates every egress path and stored field

Status: Accepted · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

- Text leaves the machine on root calls (the question plus inspected safe text) and child calls (the packet plus the model-authored objective). Today the root question goes out unredacted (`src/rlm/coordinator.rs:334`), as does the Assess `objective` (`src/rlm/executor.rs:134-136`). `ModelBoundary` takes a raw `&str` (`src/rlm/model.rs:18-29`), so no type stops unredacted text from reaching an adapter.
- The 256 KiB `model_egress_bytes` fence exists (`src/rlm/protocol.rs:258`, `src/rlm/budget.rs:83-86`), but nothing reserves it. The 64 KiB root cap is only a test assertion (`src/rlm/coordinator.rs:555-558`), and Inspect's `limit`/`max_item_bytes` are uncapped (`src/rlm/validate.rs:269-275`).
- Research requires:
  - re-running the current redactor whenever a root or child packet is materialized;
  - redacting the question and model output before logs, cache and display;
  - a payload audit that never records bearer bytes (`docs/research/rlm-model-boundary.md:484-485`).
- Research also requires canaries in every field and zero canaries in the state dir and captured mock egress (`docs/research/storage-search-runtime-architecture.md:159, 321`; `docs/research/rlm-model-boundary.md:554`). No such test exists. [Prove bounded RLM recall with exact citations](https://github.com/yuann3/relic/issues/9) deferred egress leak detection to the lock.
- Re-redacting stored safe text is safe only with a marker-preserving redactor ([0003](0003-redaction-policy.md)). The WIP redactor demotes its own markers on a second pass.
- [Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1) lists "provider framework" as out of scope. Root and child are already two callers of one boundary.

## Decision

- Adapters accept only an `EgressRequest` newtype. Its only constructor must:
  - run the current redactor over every free-text field: question, objective, safe text (again), and query;
  - enforce per-request caps: 64 KiB for root, 24 KiB for child;
  - reserve the request's bytes from the ledger;
  - write a payload audit with the research's full field list (`rlm-model-boundary.md:485`): provider profile, endpoint origin, role, exact sanitized byte count, request hash, evidence handles, redaction revision and user-visible preview. Never bearer bytes.
- Model output is redacted before persistence or display.
- One type with two callers (root and child). It is not a provider framework.
- Privacy canary gate:
  - Put a canary in every field of a history, then run import, search and recall against a fake transport.
  - Grep every byte of the state dir (including `-wal` and FTS) and every captured request body. Zero canaries passes.
  - The gate includes `redact(redact(x)) == redact(x)`.
  - It gates every slice that adds an egress path or a stored field.

## Consequences

- Every model adapter, including a conditional Jev child adapter ([0010](0010-model-boundary.md)), takes `EgressRequest`. A new adapter cannot bypass redaction, caps, ledger reservation or audit.
- The 24 KiB child cap matches the host's Assess batch sizing (≤4 items, shrunk to stay ≤24 KiB; [0009](0009-rlm-kernel.md)). The ledger reservations are the ones [0009](0009-rlm-kernel.md) and [0010](0010-model-boundary.md) require for every outbound call.
- The audit persists in the recall log beside the store ([[S12] Persist recall audits and outcomes in a JSONL log beside the store](https://github.com/yuann3/relic/issues/37)) without its preview or evidence ids: the preview is evidence text, which `forget` could not reach there, and evidence ids mean nothing outside their investigation.
- S2 (recall core) builds the seam with the Chat Completions adapter and ends with the canary gate, whose state-dir grep also covers the fields S1a and S1b store. From then on, any slice that adds an egress path or stored field must pass it before it closes.
- The gate runs real data through both sides of the seam (CODING_STANDARD "test the seam").

## Rejected alternatives

- Redact only at entry points and trust the invariant: the model-authored objective and rows from an older redaction revision go out unchecked, every new adapter must remember the rule, and caps and the byte fence have no home.
- Redact the final serialized HTTP body in the transport: runs detectors over JSON escapes, can corrupt structure and redact evidence handles (breaking citations), gives no meaningful preview, and needs an interception point per transport.
- Unit tests per redaction detector only, instead of a canary gate: misses leaks through paths that bypass the redactor, such as an unredacted field reaching a request body, WAL pages or FTS segments.

## Open facts

- Inspect output size at realistic handle sizes is unmeasured. Whether Inspect also needs caps at plan admission, or only the 64 KiB root cap, is for S2 to establish.
