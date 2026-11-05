# 0013. Skills live in a separate content-addressed catalog and activate by symlink swap

Status: Accepted — provisional internals · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

- The destination requires that Relic "compiles and manually activates a content-addressed Agent Skill" ([Define the Relic v0.1 destination](https://github.com/yuann3/relic/issues/2)). Map rule: "Relic owns a minimal content-addressed local skill catalog with explicit activation and rollback. Generic promotion governance remains outside Relic." Out of scope: candidate governance, canaries, leases, automatic promotion and multi-consumer control-plane behavior ([Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1)).
- No research ticket or prototype covers skill compilation or the catalog. The design text that exists, all in docs/research/storage-search-runtime-architecture.md:
  - The CAS install sequence: temp file, hash, fsync, no-replace rename, fsync the directory, then commit the pointer (:271).
  - An open question: do pointer mutations need `synchronous=FULL` or "a separate durable metadata path" (:49)?
  - A module sketch that puts "catalog txns" in `store/` (:223).
  - The observation DB can be rebuilt, and is quarantined and rebuilt on corruption (:273). Active skills "may not be reconstructible from mutable history" (:270). [0005](0005-evidence-store-transactions-and-read-only-open.md) amends the first: the observation store holds history its sources have trimmed, so it cannot be fully rebuilt either.
- Evidence handles are scoped to one investigation, and `verify_handle` rejects a handle from another investigation (src/rlm/evidence.rs:64-71). [0002](0002-source-identity-and-revisions.md) makes them random per investigation. A skill cannot cite evidence through them.
- Agent Skills spec: a skill is a directory holding `SKILL.md`, which is YAML frontmatter plus Markdown. `name` must be 1-64 characters of `[a-z0-9-]` and "Must match the parent directory name". `scripts/` is optional and "Contains executable code that agents can run" (https://agentskills.io/specification, fetched 2026-10-04).
- Claude Code loads `~/.claude/skills/<name>/SKILL.md` and project `.claude/skills/<name>/SKILL.md`. A `<name>` entry may be a symlink, and Claude Code reads `SKILL.md` from the target. By default a skill can be invoked by both the model and the user. `disable-model-invocation: true` hides the skill's description from the model (https://code.claude.com/docs/en/skills, fetched 2026-10-04).
- How Prime Agent discovers skills has not been researched.

## Decision

Binding:

- Skill Catalog is its own bounded context, and its code goes in the reserved top-level module `catalog/` ([CONTEXT.md](../../CONTEXT.md)).
- A skill bundle is immutable content-addressed content. A skill name is a stable pointer to a bundle. Activation and rollback are explicit user actions.
- Skill frontmatter comes from a hand-written emitter. No YAML crate ([0015](0015-dependency-envelope.md)).

Provisional (the validate → compile → activate → rollback prototype confirms or replaces each item):

- Storage: a separate `catalog.db` with `synchronous=FULL`, plus a content-addressed directory under the XDG data directory. The split is justified by FULL sync and its own failure blast radius, not by reconstructibility: neither skills nor the observation store can be fully rebuilt from sources.
- Skills cite evidence durably by revision + ordinal + content hash, never by a per-investigation evidence handle.
- Activation is an atomic symlink swap of `~/.claude/skills/<name>` (or project `.claude/skills/<name>`) to a `<hash>/<name>/` bundle. It requires an explicit target flag and a confirmation.
- Rollback swaps the symlink back.
- For any other consumer, activation in v0.1 only updates the catalog and writes nothing outside Relic's directories.
- Prime activation is deferred until research finds a documented skills directory.

## Amends

Provisional, like the items above. Both change the storage research behind [Choose the storage, search, and runtime architecture](https://github.com/yuann3/relic/issues/4):

- Catalog transactions leave `store/` (the sketch at docs/research/storage-search-runtime-architecture.md:223). The open question at :49 is answered with a separate durable metadata path rather than FULL sync on the observation DB.
- The sketch's `compile/` module (:230) becomes `catalog/`.

## Consequences

- Quarantining or rebuilding the observation DB never touches `catalog.db` or the bundle directory.
- Activation writes outside Relic's directories only into Claude Code's skills directory, and only after an explicit target flag and a confirmation.
- The catalog adds stored state, so the privacy canary gate ([0004](0004-egress-seam-and-privacy-canary.md)) covers `catalog.db` and the bundles.
- The pre-recall step of the reuse gate reads active catalog metadata ([0016](0016-acceptance-fixture-and-reuse-gate.md)).
- Owner: S4, the validation/skill tracer. Order: research, then the prototype on the Git fixture, then compile, catalog, activate and rollback. When the prototype finishes, this ADR must be updated to state each provisional item as either confirmed or replaced.

## Rejected alternatives

- Catalog tables inside the observation DB, with FULL sync for pointer transactions: quarantine and rebuild would have to carve out the catalog, and a corrupt index could take the catalog down with it.
- Filesystem only (a CAS directory plus one symlink per name, with no DB): history and rollback would need a pointer log, which reimplements a journal, and listing and auditing would be weak.
- Citing evidence by evidence handle: evidence handles are random per investigation, and the integrity check rejects them in any other investigation by design.
- Activation by copying the bundle into the consumer directory with a marker file: rollback would overwrite files non-atomically, and drift detection would be needed.
- Activation that only updates the catalog, for Claude Code too: activation would then change nothing an agent loads, so no release session could show an agent picking the skill.
- Activation for several consumers, or for Prime, in v0.1: several consumers is the out-of-scope control plane, and no documented Prime skills directory is known yet.

## Open facts

- Answered by [V1](https://github.com/yuann3/relic/issues/32) in [docs/research/validation-skill-packaging.md](../research/validation-skill-packaging.md), for V2 to confirm. A bundle is a portable Agent Skill: one `SKILL.md` with spec frontmatter only, and no `scripts/` or `allowed-tools`. Activation targets the cross-client `~/.agents/skills/<name>` (or the project `.agents/skills/<name>`), which most harnesses read. `.claude/skills/<name>` is an opt-in second link, because Claude Code does not read `.agents/skills` yet. A step that some harness would rewrite or execute (`$1`, `` !` ``, `${CLAUDE_*}`, …) is shown by record id rather than embedded. Citations are an observation id plus a keyed hash. `catalog.db` needs `fullfsync=ON` on macOS. Prime is reached through `~/.agents/skills`, and Kiro (no symlinks) is not supported.
- The validation/skill research ticket must answer:
  - How a cited procedure that contains `[REDACTED:*]` spans becomes a reusable skill.
  - Whether skills may ship `scripts/`.
  - Whether a skill the model can invoke, and that tells an agent to run recovered steps, conflicts with the out-of-scope "silently executing recovered procedures". `disable-model-invocation: true` hides the skill from the model, so the release session in [0016](0016-acceptance-fixture-and-reuse-gate.md) could not show the model invoking it.
- Whether the `<hash>/<name>/` layout satisfies both the spec's name-matches-directory rule and Claude Code's symlink loading. Untested.
- What happens to durable citations after the observation DB is quarantined and rebuilt, and after a rescrub purges a revision ([0003](0003-redaction-policy.md) turns citations to purged rows into stale-handle errors).
- The cost of FULL-sync pointer transactions in `catalog.db`, and how CAS fsync and rename behave on macOS and on Linux.
- Whether Prime Agent has a documented skills directory, and where it is.
