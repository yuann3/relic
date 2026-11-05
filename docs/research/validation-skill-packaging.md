# Validation, skill packaging and activation for Relic v0.1

**Status:** research findings for [[V1] Research validation, skill packaging and activation](https://github.com/yuann3/relic/issues/32)  
**Decision scope:** how a cited procedure becomes an Agent Skill, how verifiers run, the CAS layout, activation, durable citations and catalog durability. These findings work inside [0012](../adr/0012-validation-boundary.md), [0013](../adr/0013-skill-catalog-direction.md) and [0016](../adr/0016-acceptance-fixture-and-reuse-gate.md); [[V2] Prototype validate, compile, activate and rollback on the Git-recovery fixture](https://github.com/yuann3/relic/issues/33) confirms or replaces them.  
**Research date:** 2026-11-05

## Decision

- **A compiled skill is one `SKILL.md`: no `scripts/`, no `references/`, no `allowed-tools`, no `hooks`.**
- **Recovered step text is not safe in `SKILL.md` as it stands.** Claude Code runs `` !`cmd` `` and ` ```! ` blocks when a skill is invoked, even inside a code fence. It also rewrites `$0`–`$N`, `$ARGUMENTS` and `${CLAUDE_*}`. The compiler escapes what can be escaped and refuses a step that cannot be.
- **A model-invocable skill does not make Relic execute anything.** Invoking the skill only loads text. Every command an agent then runs goes through the agent's own permission flow. The skill body tells the agent to show the steps and ask before running any.
- **The `<hash>/<name>/` CAS layout works.** Symlinked skill directories load in both Claude Code (tested on 2.1.318) and Prime Agent (in its code), under the link's name.
- **Durable citations should be observation id plus a keyed content hash**, not revision + ordinal + hash. Rescrub, `--shell` correction and a rebuild all give rows new revisions.
- **On macOS only `F_FULLFSYNC` is durable**, at about 4 ms per sync. `catalog.db` needs WAL, `synchronous=FULL` and `fullfsync=ON`. One activation costs about 18 ms on macOS, which is irrelevant to any gate.
- **The Claude Code transcript records a Skill invocation in a parseable way**, but Anthropic documents the format as internal and unstable. It stays release evidence, as ADR 0016 already has it.
- **Prime Agent discovers skills, but only v0.7.0 documented where.** Its current Rust releases do not. Prime activation stays deferred.

## 1. Recovered text inside a skill

### What Claude Code does to `SKILL.md`

Claude Code renders a skill's body before the model sees it ([skills docs](https://code.claude.com/docs/en/skills), fetched 2026-11-05):

- "The `` !`<command>` `` syntax runs shell commands before the skill content is sent to Claude. The command output replaces the placeholder". "The inline form is only recognized when `!` appears at the start of a line or immediately after whitespace." Multi-line: "a fenced code block opened with ` ```! `".
- "Injected commands never prompt for permission while the skill renders. Claude Code checks each one against your permission rules first… Outside auto mode, when a command's permission check returns anything other than allow, Claude Code aborts the invocation."
- Substitution: "`$ARGUMENTS` | All arguments passed", "`$ARGUMENTS[N]`", "`$N` | Shorthand for `$ARGUMENTS[N]`", "`$name` | Named argument", and `${CLAUDE_SESSION_ID}`, `${CLAUDE_SKILL_DIR}` and others. "To include a literal `$` before a digit, `ARGUMENTS`, or a declared argument name… escape it with a backslash". "A doubled backslash such as `\\$1` leaves both backslashes in place, and `$1` still expands". "A backslash doesn't prevent substitution of a `${CLAUDE_*}` variable".
- A kill switch exists for users, `"disableSkillShellExecution": true` (CHANGELOG 2.1.91), but Relic cannot rely on it being set.

### Experiments (Claude Code 2.1.318, `claude -p --model haiku`, project skills symlinked into a CAS under `/tmp`)

| Skill body | Allowlist | Result |
|---|---|---|
| ` ```! ` block running `touch` on a path outside the project | none | Invocation aborted by the permission check; nothing ran |
| `` 1. Run !`touch INJECTED-INLINE` `` and a ` ```! ` block | `Bash(touch:*)` | **Both files created.** The model saw "(Bash completed with no output)" |
| The same lines inside a ` ````text ` fence, and in an indented code block | `Bash(touch:*)` | **All three ran.** Markdown structure is not considered |
| The same line in `references/procedure.md` | `Bash(touch:*)` | Not run: only `SKILL.md` is rendered |
| `$0 $1 $ARGUMENTS $ARGUMENTS[0] ${CLAUDE_SKILL_DIR} ${CLAUDE_SESSION_ID} \$ARGUMENTS $$ARGUMENTS $HOME ${HOME}`, args `AAA BBB` | — | Rendered `AAA BBB [AAA BBB] AAA <link path> <session id> $ARGUMENTS $AAA BBB $HOME ${HOME}` |
| `` ! `touch SPACED` ``, ``!\`touch BACKSLASHED\` ``, `` ```! `` mid-line | `Bash(touch:*)` | Left as text |
| `$1` with a single argument passed | — | Left as text (no second argument) |

**Consequence:** a recovered command such as `awk '{print $1}'` reaches the agent changed. A recovered `` x=1 !`cmd` `` would run under an allowlist the user wrote for other reasons. `${CLAUDE_SKILL_DIR}` resolves to the symlink path, not the CAS path.

### Compile rule

The compiler renders each step's redacted `safe_text`. For each step:

- Each `$` directly followed by a digit, `ARGUMENTS` or `{CLAUDE_` is a hazard. A `$` before a digit or `ARGUMENTS` is escaped as `\$`; Claude Code removes the backslash, so the agent sees the text as recorded.
- A step is **refused**, with a message naming the step, if it holds anything that cannot be escaped:
  - `${CLAUDE_`;
  - a backslash already before `$<digit>` or `$ARGUMENTS`;
  - `!` followed by a backtick at the start of a line or after whitespace;
  - a line starting ` ```! `.

  Such sequences are rare in shell history. A refused step is never silently dropped.
- Relic declares no `arguments`, so no `$name` placeholder exists.
- Steps stay in `SKILL.md` rather than moving to `references/`. With a project skill, the model's `Read` of a file in the bundle was denied, because the symlink resolves outside the working directories: "`references/word.md`, is a symlink to `/private/tmp/relic-v1/cas/…`, which is outside the allowed working directories, so the read was denied". `SKILL.md` itself is loaded by Claude Code and needs no `Read`.

### Redacted spans

A `[REDACTED:<kind>]` marker stays in the step as written. The skill body says each marker is a value the user must supply, and that the agent asks for it rather than guessing it. Nothing redacted is ever recovered into the bundle. A step that is only a marker still carries its citations. Typed markers ([0003](../adr/0003-redaction-policy.md)) tell the agent what kind of value is needed.

### Evidence for the open `FinishV1.procedure` field set

Nothing here needs a new field. The compiler needs each step's text and its cited record ids, which `{ text, supports }` already gives. Parameter slots fall out of the markers. Keep the shape and settle it in V2, as [0012](../adr/0012-validation-boundary.md) says.

## 2. `scripts/`, model invocation and "silently executing recovered procedures"

- The spec allows `scripts/`, which "Contains executable code that agents can run" ([specification](https://agentskills.io/specification), fetched 2026-11-05). A script built from recovered steps would be Relic writing an executable from recalled commands, which [0012](../adr/0012-validation-boundary.md) forbids. **No `scripts/`.**
- `allowed-tools` lists "Tools Claude can use without asking permission during the turn that invokes this skill". "Workspace trust doesn't gate this field. Claude Code applies a project skill's `allowed-tools` even in a `-p` run in a folder you've never trusted" ([skills docs](https://code.claude.com/docs/en/skills)). Paired with any command, it would turn invocation into execution. **Relic never emits `allowed-tools`**, and never emits `hooks` either.
- With those removed and the hazards above escaped, invoking a compiled skill injects text and nothing else. Bash keeps "Permission required: Yes" ([tools reference](https://code.claude.com/docs/en/tools-reference)). Whether a command then runs without a prompt depends on the user's own agent settings (allow rules, auto or bypass mode), not on anything Relic wrote. That is not Relic silently executing a procedure. The skill body still states "show these steps and ask before running any of them", so the release session shows a confirmation.
- `disable-model-invocation: true` stays off: the reuse gate's release session needs the model to pick the skill ([0016](../adr/0016-acceptance-fixture-and-reuse-gate.md)).

## 3. CAS layout and activation

Layout: `<data dir>/catalog/<hash>/<name>/SKILL.md`. The activation link is `~/.claude/skills/<name>` or `<project>/.claude/skills/<name>`.

- **Claude Code:** "a `<skill-name>` entry in the enterprise, personal, or project location can be a symlink to a directory elsewhere on disk. Claude Code reads `SKILL.md` from the target" ([skills docs](https://code.claude.com/docs/en/skills)). Tested:
  - Two project symlinks into a CAS loaded, and the model invoked `relic-probe` and returned its word.
  - A link named `relic-alias` pointing at a bundle whose frontmatter says `relic-other` was listed as `relic-alias (relic-other)`, and both names invoke it (CHANGELOG 2.1.290).
  - Relic keeps link name, directory name and frontmatter `name` identical, so this case never arises.
- **Spec and validator:** "name … Must match the parent directory name". The reference validator compares `skill_dir.name` of the path it is given, unresolved ([validator.py L60-65](https://github.com/agentskills/agentskills/blob/83021e2be457a9b6679148719c1f6074f0ad975e/skills-ref/src/skills_ref/validator.py#L60-L65)). `<hash>/<name>/` satisfies it both through the link and at the target.
- **Prime Agent:** follows symlinks via `std::fs::metadata` ([discovery.rs L45-52](https://github.com/PrimeIntellect-ai/prime-agent/blob/74591cddef28d11249c3171112114fc4755375d7/crates/pa-core/src/packages/resolve/discovery.rs#L45-L52)). It keeps the link's name, and drops duplicates by realpath.
- **Reload:** Claude Code "watches skill directories for file changes… picks up the change within the current session" (CHANGELOG 2.1.0 added hot reload). No document covers a repointed symlink. Untested here; `/reload-skills` (2.1.152) is the fallback.
- **Frontmatter Relic emits:** `name`, `description` (1–1024 characters, non-empty), `metadata` (relic version, bundle hash). These are all in the spec, so `skills-ref validate` passes. It rejects any field outside the spec's six, so Relic emits no Claude Code-only field.
- **Unverified:** the personal location `~/.claude/skills` through a symlink. The docs say it works; I didn't touch the user's global skills directory.

## 4. Claude Code transcripts

- Official position: "The entry format is internal to Claude Code and changes between versions, so scripts that parse these files directly can break on any release" ([sessions docs](https://code.claude.com/docs/en/sessions), fetched 2026-11-05).
- Observed shape, from counting records in this machine's `~/.claude/projects` without reading payloads (1428 files, versions 2.1.288–2.1.318):
  - A model invocation is an `assistant` record with a content block `{"type":"tool_use","name":"Skill","input":{"skill":<name>,"args"?:<string>}}` (163 seen, 14 versions).
  - It is followed by a `user` record holding the `tool_result` ("Launching skill: <name>") and `toolUseResult: {"success": true, "commandName": <name>}`.
  - A typed `/name` arrives as a user message containing `<command-name>` (141 seen).
  - `attachment` records of type `skill_listing` carry a `names` array, and `invoked_skills` carries `skills: [{name, path, content}]`.
  - Every record carries `version`, the Claude Code version.
- The rendered skill body, with substitutions applied, is stored as an `isMeta` user message. So the transcript holds the skill text too: an imported trace re-imports a compiled skill's step text as agent evidence.
- **Conclusion:** parseable, and seen unchanged across Claude Code 2.1.288–2.1.318 here, but the format is explicitly unstable. The S10 adapter pins a tested version range with fixtures and treats anything else as an unknown variant ([source contracts](source-evidence-contracts.md)). ADR 0016 already keeps it as release evidence, not a gate.

## 5. Prime Agent

Read from source; no Prime binary is installed here. Versions read: v0.7.0 (`be9e2fa`, TypeScript) and `main` at `74591cd` (v0.10.2+36, Rust).

- **Directories, highest first:**
  1. `--skill` paths;
  2. project settings;
  3. project `.prime/agent/skills`, then `.agents/skills` from the cwd up to the git root;
  4. global settings;
  5. `~/.prime/agent/skills` (or `$PRIME_AGENT_CODING_AGENT_DIR/skills`), then `~/.agents/skills`;
  6. packages;
  7. built-in skills.

  Source: [auto.rs L55-118](https://github.com/PrimeIntellect-ai/prime-agent/blob/74591cddef28d11249c3171112114fc4755375d7/crates/pa-core/src/packages/resolve/auto.rs#L55-L118), [resolve/mod.rs L98-107](https://github.com/PrimeIntellect-ai/prime-agent/blob/74591cddef28d11249c3171112114fc4755375d7/crates/pa-core/src/packages/resolve/mod.rs#L98-L107).
- **Fields and rules:** it reads only `name`, `description` and `disable-model-invocation`. Name rules are warnings; only an empty description is rejected. Skills reach the model as an `<available_skills>` block in the system prompt. There is no skill tool.
- **Documentation:** v0.7.0 documented the directories in `packages/coding-agent/docs/skills.md`. The Rust port (`39bc99a9`, 2026-09-29) deleted that file. Current releases state the directories only inside the bundled `skill-creator` skill, which omits `.agents/skills`.
- **Session JSONL** (version 3): no typed skill event. `/skill:<name>` becomes a user message starting `<skill name="X" location="…">`. A model-chosen use is an ordinary `python_repl` call.
- **Conclusion:** symlink activation into `~/.prime/agent/skills` would work mechanically. Today's release has no documented directory, so 0013's deferral holds. `~/.agents/skills` is a cross-agent location worth revisiting with S11.

## 6. Durable citations

[0013](../adr/0013-skill-catalog-direction.md) proposes revision + ordinal + content hash. What the store does to those fields:

| Event | Observation id | Revision + ordinal | `safe_text` |
|---|---|---|---|
| Append, align | kept | kept | kept |
| Rescrub after a redaction-rule change | **new rows** (`INSERT … SELECT` into a new revision, src/store/mod.rs:490-520) | new revision, same ordinal | **rewritten** |
| `--shell` correction (`replace_kind`) | deleted, re-imported | new revision | may differ |
| `forget` | deleted | gone | gone |
| Quarantine and rebuild | all new | all new | same if rules unchanged |

- Observation ids are `AUTOINCREMENT` and never reused (src/store/schema.rs:116), so an id never comes back as a different command. Revision + ordinal adds nothing an id lacks, and both break on the same events.
- **Recommendation:** a durable citation is `{ observation id, blake3 keyed hash of safe_text under the installation key }`. It resolves when the row with that id exists and its text hashes the same; otherwise it is **stale**. A stale citation does not block a skill. `relic skill show` says which citations are stale.
- Re-resolving across a rescrub or a rebuild (by source, kind, time and re-redacted text) is a heuristic. Leave it out of v0.1.
- **Privacy:** a bundle holds redacted step text derived from evidence, which `forget` does not reach.
  - V2 must decide what `forget` does about a cited record: at minimum, name the skills that cite it, which needs the catalog to index cited ids.
  - The keyed hash lets someone holding the key confirm a guessed text. The boundary guard has the same property ([0002](../adr/0002-source-identity-and-revisions.md)).

## 7. Verifiers

- `source/git.rs`'s `bounded` runner already has what [0012](../adr/0012-validation-boundary.md) needs: fixed argv, no shell, its own process group killed at the timeout, output capped, and stderr drained. The verifier generalizes it, setting a cwd the user chose.
- `main` sets `umask(0o077)` and drops the old value (src/main.rs:17-18). A verifier must run under the user's original umask, so `main` keeps the returned mask and the verifier's `pre_exec` restores it.
- The Git fixture's verifier, `git rev-parse --verify <ref>`, runs in well under the 10 s bound.

## 8. Durability costs

Pointer transaction = one `INSERT OR REPLACE` plus one history `INSERT`. CAS install = write 4 KiB, sync the file, sync its directory, rename, sync the parent. Swap = `symlink` + `rename` + sync the directory. 40 samples each, script kept outside the repo.

| Operation | macOS 27.0.1, APFS SSD, p50 / p95 | Linux (OrbStack container, overlayfs; indicative only), p50 / p95 |
|---|---|---|
| Pointer txn, WAL, `synchronous=FULL`, `fullfsync=OFF` | 0.07 / 0.09 ms | 1.18 / 2.23 ms |
| Pointer txn, WAL, `synchronous=FULL`, `fullfsync=ON` | 4.22 / 5.40 ms | n/a |
| Pointer txn, rollback journal, FULL, `fullfsync=ON` | 13.80 / 14.89 ms | 2.05 / 3.42 ms (`fullfsync` n/a) |
| CAS install, `fsync` | 0.38 / 0.53 ms | 1.29 / 2.56 ms |
| CAS install, `F_FULLFSYNC` | 9.37 / 9.75 ms | n/a |
| Symlink swap, `fsync` | 0.15 / 0.23 ms | 0.46 / 0.62 ms |
| Symlink swap, `F_FULLFSYNC` | 4.21 / 5.41 ms | n/a |

- On macOS, `fsync` only reaches the drive's cache. SQLite's `fullfsync` defaults to off, so `synchronous=FULL` alone is not durable there.
- Rust's `File::sync_all` uses `F_FULLFSYNC` on Apple targets. Relic syncs CAS files and directories with `sync_all` and sets `PRAGMA fullfsync = ON` and `checkpoint_fullfsync = ON` on `catalog.db`.
- Activation, about 18 ms on macOS, is a rare explicit user action, so these costs do not matter for any gate.
- A Linux run on bare ext4 is still owed to the release slice.

## Open for V2

- Whether repointing an active symlink is seen mid-session, in both Claude Code and Prime.
- What `forget` does to skills that cite a forgotten record.
- Whether the compile refusals ever trigger on the fixture's steps.
- The personal `~/.claude/skills` symlink, tested in a throwaway `HOME`.
