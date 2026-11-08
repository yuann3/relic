# 0016. Git reflog recovery is the acceptance fixture, and reuse is gated on fewer model calls and egress bytes

Status: Accepted · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

- The destination requires that "a held-out second task selects the exact skill before broad archaeology with improved calls/tokens/time and no loss of verified success" ([Define the Relic v0.1 destination](https://github.com/yuann3/relic/issues/2)). The map lists the acceptance fixture and the held-out sibling scenario under "Not yet specified" ([Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1)).
- The RLM acceptance plan calls for 12 seeded investigations:
  - 8 answerable cases whose decisive evidence needs conditional traversal across at least two source or relation kinds and is absent from the initial FTS top 20.
  - 4 insufficient or contradictory cases.

  If the bounded RLM does not beat FTS-only, fixed top-20 plus one model call, and a fixed hand-coded pipeline, it does not ship under the RLM label (docs/research/rlm-model-boundary.md:535-537). None of these cases has been built ([Prove bounded RLM recall with exact citations](https://github.com/yuann3/relic/issues/9)).
- The fixture needs an offline, CI-safe verifier. The Jev prototype's certbot example would plausibly need network access and root (inferred, not measured; [[P1] Prove Jev across all three integration surfaces](https://github.com/yuann3/relic/issues/18)).
- `BudgetLedger` counts calls, ops, rows, handles, materialised bytes and egress bytes (src/rlm/budget.rs:4-60). Bytes are the hard limit. Tokens are "additional accounting"; when a provider omits usage, the reservation stays consumed (docs/research/rlm-model-boundary.md:238, :279).
- Adding an operator requires evidence from a held-out fixture (docs/research/rlm-model-boundary.md:513). The kernel executes five operators ([0009](0009-rlm-kernel.md)).
- CI runs against mock servers. Release evidence comes from real endpoints, with exact versions (docs/research/rlm-model-boundary.md:549).
- Fixture style: seeded, deterministic corpora (src/fixture.rs). Git landed in S3 and shell hooks in [S8](https://github.com/yuann3/relic/issues/30): a hook record's exit status is a line of its text, such as `exit 1 · took 2.3 s · cwd: /srv`. The Claude Code adapter lands after the validation/skill tracer, so until then the fixture's Claude-trace leg is synthetic.

## Decision

Fixture:

- Task: "recover a lost commit or deleted branch via reflog". The evidence spans zsh history, shell-hook exit statuses, a Claude Code trace and Git.
- The corpus is deterministic and synthetic, in the style of src/fixture.rs.
- Verifier: `git rev-parse --verify <ref>`, run in a temp repo.
- Held-out sibling: recover a dropped stash in a different repo. The sibling is unseen at compile time.
- The fixture also supplies one answerable case among the 12 seeded investigations, and a contradictory variant that serves as an insufficient-evidence case.

CI reuse gate:

- Run 1 has no skill active. Run 2 is the sibling task with the skill active.
- A deterministic host step before recall matches the question against active catalog metadata ([0013](0013-skill-catalog-direction.md)). The step sits outside the operator kernel, so [0009](0009-rlm-kernel.md) is not reopened.
- Run 2 passes when all of the following hold:
  - The step selects the exact expected skill id before any search or expand op.
  - Run 2 makes strictly fewer model calls than run 1.
  - Run 2 sends fewer egress bytes than run 1.
  - The same verifier still passes ([0012](0012-validation-boundary.md); only success confirmed by a verifier counts).
- Tokens are reported where the provider reports usage. Wall time is reported but not gated, because responses are recorded.

Release evidence (not gating):

- One real Claude Code session on the sibling task. Its imported trace must show the Skill invocation before the first search-type tool call. The session is recorded with exact versions, the same way as the `relic model check` smoke tests.

## Amends

- The destination's "improved calls/tokens/time" ([Define the Relic v0.1 destination](https://github.com/yuann3/relic/issues/2)) becomes a gate on model calls and egress bytes only. Tokens and wall time are reported, not gated.
- The map's "Not yet specified" entry for the acceptance fixture and held-out sibling is now specified by this ADR.

## Consequences

- S3 (shell hooks, then Git) supplies the exit statuses and the Git leg. S4's validate → compile → activate → rollback prototype runs on this fixture.
- S5 owns the rest: replacing the synthetic Claude-trace leg with the Claude Code adapter that [S10](https://github.com/yuann3/relic/issues/34) built (the Prime adapter, S11, was dropped from v0.1), the full fixture, the 12 seeded investigations, the reuse gate and the release dogfood session.
- The reuse gate runs in CI against recorded model responses. It needs no live model.
- The fixture as built ([R1](https://github.com/yuann3/relic/issues/39)) is `tests/recovery/mod.rs`, shared by the R tickets' tests. `shop` loses a commit to `git reset --hard` and has `feature/coupons` deleted by a Claude Code session; 2000 seeded noise commands come first. The contradictory variant typed the rescue outside the repository, so the hook saw it fail and Git has no `rescue`. The held-out sibling `notes` has its stash imported, then dropped. Every leg goes through `relic import`. A Git ref's observed-at is when it was imported, so that is the one field that differs between two builds.
- The host step's pick seeds the investigation ([R2](https://github.com/yuann3/relic/issues/40)): before the first root call, the skill's cited records, in step order, fill an evidence handle, and every root state carries the skill's name, that handle and the first 8 records, each cut to 1 KiB, as citable items. In every state, not only the first: the root sees only the state in front of it, so records shown once would be lost to the next inspection. This is where run 2 saves calls: it searches only for what the skill lacks. The seed is root state, not an operator, so [0009](0009-rlm-kernel.md) stays closed.
- The gate as built ([R3](https://github.com/yuann3/relic/issues/41)) is `reuse_gate_a_skill_makes_the_held_out_sibling_cheaper_and_still_verified` in `tests/recall_cli.rs`, on the fixture and one scripted model that plays both runs by one policy: with no skill it searches, expands and has helpers assess; with a skill it searches only for what the skill lacks. Run 1 recovers `shop`'s commit, is validated with `git rev-parse --verify rescue`, compiled and activated; run 2, after the sibling is imported, recovers the dropped stash, the user branches it, and `git rev-parse --verify` passes on that branch. At the time of writing, run 1 takes 6 calls and 25,590 egress bytes, run 2 takes 2 and 14,661. The script decides how much run 2 saves, so the gate proves the pick, the seed and the metering, not that a real model takes the shortcut: that is R4's and R5's evidence.
- The catalog must expose enough metadata for the host step to match the question deterministically.

## Rejected alternatives

- Toolchain repair (fix a broken cargo build, verify with `cargo check`): the verifier is slow and its timing noisy.
- Service operations (certbot, docker compose): the verifier needs network access, root or a daemon, so it fails the offline and CI requirements.
- Making an agent-side Claude Code session the gate: it is non-deterministic, needs a live paid agent and the unbuilt adapter, and depends on an undocumented transcript format. It stays as release evidence.
- Measuring only inside Relic, with no agent run: that proves reuse inside Relic but not that an external agent picks the skill.
- Skill lookup as a new RLM operator: it reopens the kernel, and adding an operator requires held-out fixture evidence.
- Gating tokens: providers may omit usage.
- Gating wall time: with recorded responses it measures only local work.

## Open facts

- Whether imported Claude Code transcripts record a Skill invocation in a way that is parseable and stable across versions. The release evidence depends on it (S5). [V1](https://github.com/yuann3/relic/issues/32) found it parseable (a `Skill` tool use with `{skill, args}`, unchanged across 2.1.288–2.1.318) but documented as internal and unstable ([docs/research/validation-skill-packaging.md](../research/validation-skill-packaging.md)). S10 imports it as an `AgentTool` record `Skill <name>`, pinned to 2.1.288–2.1.318 with a recorded 2.1.318 fixture ([docs/research/source-evidence-contracts.md](../research/source-evidence-contracts.md)).
- Whether the answerable seeded cases' conditional traversal across two source or relation kinds is reachable with five operators now that Relate is cut ([0009](0009-rlm-kernel.md)) (S5).
- Whether the bounded RLM beats FTS-only, top 20 plus one model call, and the fixed pipeline on the 12 seeded investigations. This decides the RLM label (S5).
