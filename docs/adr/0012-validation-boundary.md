# 0012. Validation runs only a user-authored verifier; Relic never executes a recalled procedure

Status: Accepted — provisional internals · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

- The destination: Relic "runs only explicit approved validation" ([Define the Relic v0.1 destination](https://github.com/yuann3/relic/issues/2)). Map rule: "Validation never automatically reruns reconstructed commands. Only explicit user-approved probes/verifiers may run, and a workspace is not called a sandbox." Out of scope: "silently executing recovered procedures" ([Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1)).
- No research ticket or prototype covers validation. The lock fixes its boundary (inputs, outputs, privacy and execution rules, domain placement) and leaves internals to a research ticket and a prototype that block only the validation slice.
- Cited observations hold redacted safe text. A reconstructed command can contain `[REDACTED:<kind>]` spans ([0003](0003-redaction-policy.md)) and cannot run as-is.
- Model output "cannot construct ... shell, commands, or verifier execution". A `FinishV1` "is evidence-backed text, not permission to run, compile, activate, or validate commands" (docs/research/rlm-model-boundary.md:491-492).
- Evidence rule: "procedure solved problem" needs a "current explicit verifier or user assertion plus citations". A "command sequence alone" is not enough (docs/research/source-evidence-contracts.md:243). `FactOrigin` already has `UserAsserted` and `ConfirmedByValidation` (:137-144). Redaction applies before SQLite, logs and compiled assets (:219).
- `FinishV1.procedure` is `Vec<ProcedureStepV1 { text, supports }>`, with no argv, cwd, parameter or expected-artifact slots (src/rlm/protocol.rs:143-176). The finish gate rejects a step that has no citation (src/rlm/finish.rs:50-57).
- Fixed-argv, no-shell subprocesses are already proven for fzf (src/picker.rs) and planned for Git (docs/research/storage-search-runtime-architecture.md:208).

## Decision

Validation boundary (binding):

- Validation means one thing: running a verifier argv that the user wrote. The argv is fixed and runs without a shell. The user chooses the cwd. Output and run time are bounded. The child process runs under the user's original umask, not Relic's `0o077` ([0003](0003-redaction-policy.md)).
- Relic never runs the procedure, any step of it, or anything derived from it.
- The verifier argv passes through the current redactor before storage.
- Each run stores a `ValidationRecord { redacted argv, cwd, exit status, duration, Relic version, workspace assurance label }` as a `ConfirmedByValidation` fact.
- A user's assertion that the procedure worked is stored as `UserAsserted`, which is a lower grade than `ConfirmedByValidation`.
- Only success confirmed by a verifier counts toward the reuse gate ([0016](0016-acceptance-fixture-and-reuse-gate.md)).
- Validation is its own bounded context, and its code goes in the reserved top-level module `validation/` ([CONTEXT.md](../../CONTEXT.md)).

Procedure shape:

- `FinishV1.procedure` keeps the shape `{ text, supports }` and is marked provisional.
- A procedure step may reference only cited observations.
- The model never authors executable argv.
- The validation/skill prototype decides the final field set.

Internals stay open: how the verifier argv is entered, the output and time bound values, the vocabulary of workspace assurance labels, and where and how the record is stored.

## Amends

- Map rule "Only explicit user-approved probes/verifiers may run" ([Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1)): validation runs only a user-authored verifier. Procedure-derived probes are rejected (below).
- Storage research sketch behind [Choose the storage, search, and runtime architecture](https://github.com/yuann3/relic/issues/4): the `validate/` "explicit workspace/probe boundary" module (docs/research/storage-search-runtime-architecture.md:229) becomes `validation/`, verifier only.

## Consequences

- Recall stays read-only. No code path leads from a `FinishV1` or a procedure step to a subprocess.
- "Validation" and "verifier" mean only this. Plan admission, the finish gate and the handle integrity check use their own terms ([CONTEXT.md](../../CONTEXT.md)).
- `ValidationRecord` adds stored fields, so the privacy canary gate ([0004](0004-egress-seam-and-privacy-canary.md)) covers the stored verifier argv.
- Once the prototype settles the procedure field set, the root model's JSON schema and prompts from the adapter slice may need a second revision.
- Owner: S4, the validation/skill tracer. Order: research, then the validate → compile → activate → rollback prototype on the Git fixture ([0016](0016-acceptance-fixture-and-reuse-gate.md)), then the validation implementation. The research and prototype tickets block only S4.

## Rejected alternatives

- Approved probes derived from procedure steps, each argv shown verbatim for confirmation: redacted steps cannot run, and confirming step by step drifts toward executing reconstructed procedures, which the map forbids.
- Attestation (`UserAsserted`) as the main validation path: it is weaker evidence. It is kept only as a lower grade that the reuse gate never counts.
- Optional typed step fields now (command observation reference, parameter slots, expected artifact): the schema would grow before a prototype shows the need, and the model could fill parameters it has no evidence for, which pulls toward model-authored commands.
- Lock validation internals now from first principles: the procedure-to-argv problem and the durable-citation problem have not been examined, so this choice is the one most likely to be reversed.
- Research and prototype validation before locking anything: this would block D1's integrity fixes, the adapter and Jev, none of which depend on the design of validation.

## Open facts

- Answered by [V1](https://github.com/yuann3/relic/issues/32) in [docs/research/validation-skill-packaging.md](../research/validation-skill-packaging.md): the three questions below, and that a verifier needs the umask `main` currently discards.
- The validation/skill research ticket answers the three questions listed in [0013](0013-skill-catalog-direction.md) Open facts: redacted spans in a reusable skill, `scripts/`, and whether a model-invocable skill that tells an agent to run recovered steps is "silently executing recovered procedures".
