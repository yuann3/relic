# Validation, skill packaging and activation for Relic v0.1

**Status:** research findings for [[V1] Research validation, skill packaging and activation](https://github.com/yuann3/relic/issues/32)  
**Decision scope:** how a cited procedure becomes an Agent Skill, how verifiers run, the bundle layout, activation, durable citations and catalog durability. These findings work inside [0012](../adr/0012-validation-boundary.md), [0013](../adr/0013-skill-catalog-direction.md) and [0016](../adr/0016-acceptance-fixture-and-reuse-gate.md); [[V2] Prototype validate, compile, activate and rollback on the Git-recovery fixture](https://github.com/yuann3/relic/issues/33) confirms or replaces them.  
**Research date:** 2026-11-05  
**Target:** the [Agent Skills](https://agentskills.io) open standard and the harnesses that implement it, not one vendor.

## Decision

- **Relic emits portable Agent Skills.** A bundle is one `SKILL.md` holding only spec frontmatter (`name`, `description`, `metadata`), with no `scripts/`, `references/`, `allowed-tools` or vendor fields. It passes the reference validator, `skills-ref validate`.
- **Activation links into the cross-client `.agents/skills/` directories:** `~/.agents/skills/<name>` for the user, or `<project>/.agents/skills/<name>` for one project. Twelve harnesses read the user path, and Antigravity and Letta read the project one. Claude Code reads neither, so `.claude/skills/<name>` is an opt-in second link.
- **A symlinked `<hash>/<name>/` bundle works** in every harness checked with symlink evidence, except Kiro's IDE, which doesn't follow skill symlinks.
- **Step text goes into `SKILL.md` only when no harness would change or run it.** The standard defines no body preprocessing ("There are no format restrictions"), but Claude Code, Goose and Letta each rewrite or execute parts of the body, and no single escape works in all of them. A step that would trip one is shown by record id, and the agent reads its exact text with `relic show <id>`. Nothing is escaped.
- **Invoking a compiled skill makes nothing run.** The skill tells the agent to show the steps and ask before running any. Commands then go through the agent's own permission flow. This settles "silently executing recovered procedures".
- **Also:**
  - Durable citations are an observation id plus a keyed content hash.
  - `catalog.db` needs `fullfsync=ON` on macOS.
  - Claude Code's transcript records Skill use parseably but under an unstable format.
  - Prime discovers skills, including `~/.agents/skills`, but its current release documents no skills directory.

## 1. What the standard fixes and what it leaves open

Sources: the [specification](https://agentskills.io/specification), the [client implementation guide](https://agentskills.io/client-implementation/adding-skills-support), and the [agentskills/agentskills](https://github.com/agentskills/agentskills/tree/83021e2be457a9b6679148719c1f6074f0ad975e) repository at `83021e2`, all fetched 2026-11-05.

- **Authority.** The repository's `AGENTS.md`: "`docs/specification.mdx` is authoritative for format requirements. Explanatory documentation, examples, tests, and implementations do not add requirements to the format."
- **The spec covers what goes inside a skill directory.**
  - Frontmatter fields: `name`, `description`, `license`, `compatibility`, `metadata`, `allowed-tools` ("Experimental").
  - The name: 1–64 characters of `a-z0-9-`, not starting or ending with a hyphen, no `--`, and it "Must match the parent directory name".
  - The body: "There are no format restrictions… the agent will load this entire file once it's decided to activate a skill."
  - File references: "use relative paths from the skill root… one level deep".
- **The spec does not define:** a location, installation, update, version field, package format, symlink policy or any body preprocessing. No spec version is tagged yet. GOVERNANCE.md (adopted 2026-11-04 with the move to the Agentic AI Foundation) promises tagged releases with a changelog.
- **Locations come from the client guide, as a recommendation.** Scan a client-specific directory and "the **`.agents/skills/` convention**… for cross-client skill sharing". "Project-level skills override user-level skills." Gate project skills on folder trust. "Name doesn't match the parent directory name → warn, load anyway." The convention came from issue [#15](https://github.com/agentskills/agentskills/issues/15) and entered the guide through PR [#200](https://github.com/agentskills/agentskills/pull/200). Issue #15 is still open, and lists Claude Code as "Pending" ([claude-code#16345](https://github.com/anthropics/claude-code/issues/16345)).
- **Versioning, per the maintainers:** keep it out of `SKILL.md`. Use "a `metadata.version` field (which some people are already using)" (Discussion [#420](https://github.com/agentskills/agentskills/discussions/420#discussioncomment-17486815)), and treat versioning as "part of the distribution mechanism" (issue [#46](https://github.com/agentskills/agentskills/issues/46)).
- **Distribution, proposed only:** a `.well-known/agent-skills/index.json` index with `sha256:{hex}` digests and `.tar.gz`/`.zip` archives that "Reject… symlinks or hard links that resolve outside the skill directory" (PR [#254](https://github.com/agentskills/agentskills/pull/254), open).
- **The skills-ref validator:**
  - It compares the unresolved `Path.name` of the given directory with `name`.
  - It rejects any top-level field outside the six: "Unexpected fields in frontmatter".
  - It calls itself "a demonstration artifact".
- **The most widely used installer is Vercel's `skills` CLI** (`npx skills add`, [vercel-labs/skills](https://github.com/vercel-labs/skills/tree/a0a1bb93d342061ce4ddf3e8d88fe8314d9f1f9c) v1.7.6). It is not part of the standard.
  - Its default mode is the same model as Relic's: "copy into the canonical dir, symlink agent dirs to it". The canonical directory is `.agents/skills/<name>`.
  - It skips the per-agent link for agents that already read `~/.agents/skills`.

## 2. Where harnesses look, and whether a symlinked bundle loads

Read on 2026-11-05. Each open-source repository was read at its default-branch HEAD; closed-source harnesses from their official docs.

| Harness | User dirs | Project dirs | Symlinked skill dir | Name vs directory |
|---|---|---|---|---|
| Codex CLI (`ed8eac4`) | `~/.agents/skills` | `.agents/skills`, cwd up to repo root | followed (`DirectorySymlinkPolicy::Follow`); a symlinked *file* is skipped | name optional, no check |
| Gemini CLI (`1fe22af`) | `~/.gemini/skills`, `~/.agents/skills` | `.gemini/skills`, `.agents/skills` (trusted folders) | followed (glob `*/SKILL.md`; `/skills link` itself symlinks) | no check |
| OpenCode (`af5d3b5`) | `~/.config/opencode/skills`, `~/.claude/skills`, `~/.agents/skills` | `.opencode`, `.claude`, `.agents` `/skills`, in ancestors | followed (re-`stat` of `SymbolicLink`) | the directory is the id |
| Amp (closed) | `~/.config/agents/skills`, `~/.agents/skills`, `~/.config/amp/skills`, `~/.claude/skills` | `.agents/skills`, `.claude/skills`, in parents | followed (tested with `amp skills list`) | not enforced locally |
| VS Code Copilot (`8bf7aa0`) | `~/.agents/skills`, `~/.copilot/skills`, `~/.claude/skills` | `.agents/skills`, `.github/skills`, `.claude/skills` | followed (`readdir` resolves links) | folder name wins |
| Copilot CLI (closed) | `~/.copilot/skills`, `~/.agents/skills` | `.github`, `.claude`, `.agents` `/skills` | not documented | "Typically matches" |
| Cursor (closed) | `~/.agents`, `~/.cursor`, `~/.claude`, `~/.codex` `/skills` | the same under the project, nested | fixed in 2.5 (staff, forum; not in docs) | "Must match the parent folder name" |
| Devin Local / Windsurf (closed) | `~/.config/devin/skills`, `~/.agents/skills` | `.devin`, `.agents`, `.windsurf` `/skills` | not documented | spec charset |
| Junie CLI (closed) | `~/.junie/skills`, `~/.agents/skills` | `.junie/skills`, `.agents/skills` (trusted) | not documented | no rule |
| Factory Droid (closed) | `~/.factory/skills`, `~/.agents/skills` | `.factory/skills`, `.agents/skills` | not documented | invalid name shown as "Invalid" |
| Antigravity (closed) | `~/.gemini/config/skills` | `.agents/skills`, parents to repo root | not documented | defaults to folder |
| Goose (`bfe8ccd`) | `~/.agents/skills`, config dir, `~/.claude/skills`, `~/.config/agents/skills` | `.agents/skills`, `.goose/skills`, `.claude/skills` (cwd) | followed ("follows file and directory symlinks, including targets outside a root") | no check |
| Letta Code (`ee6e2c5`) | `~/.letta/skills` | `.agents/skills` | followed (`stat` of a symlink) | not enforced |
| Prime Agent (`74591cd`) | `~/.prime/agent/skills`, `~/.agents/skills` | `.prime/agent/skills`, `.agents/skills`, to git root | followed (`std::fs::metadata`) | warning only |
| Claude Code 2.1.318 | `~/.claude/skills` | `.claude/skills` | followed (documented, and tested) | listed as `link (frontmatter)` when they differ |
| Kiro (closed) | `~/.kiro/skills` | `.kiro/skills` | **not followed** in the IDE ([Kiro#6401](https://github.com/kirodotdev/Kiro/issues/6401), open) | "Must match folder name" |

**Consequences for activation:**

- **The activation targets are `~/.agents/skills/<name>` (user) and `<project>/.agents/skills/<name>` (project).** The user path reaches Codex, Gemini CLI, OpenCode, Amp, VS Code, Copilot CLI, Cursor, Devin, Junie, Factory, Goose and Prime. The project path also reaches Antigravity and Letta.
- **Claude Code needs its own link, `.claude/skills/<name>`,** until claude-code#16345 lands. It is offered as an opt-in second link, swapped and rolled back together with the first, with the target named in the confirmation. Each link is one discovery path for the same skill, not a second consumer to govern.
- **Kiro is not supported in v0.1.** It would need a copied directory, which loses the atomic swap and the single source of truth.
- **Layout rules every harness accepts:**
  - The link name, the bundle directory's name and frontmatter `name` are identical, and spec-valid.
  - `SKILL.md` is a regular file inside the bundle, never a symlink: Codex skips symlinked files.
  - Relic writes nothing else into a skills directory. OpenCode loads stray root-level `*.md` files as skills.
- **Project scope:** several harnesses load project skills only in trusted folders (Gemini CLI, Junie, VS Code). User scope is the default; project scope needs the explicit target flag.
- **Reload.** The working assumption for V2 is that any harness notices a repointed link. The sources split:
  - Watched (the swap is seen mid-session): Codex (10 s throttle), OpenCode, VS Code, Letta and Claude Code.
  - Re-read on every load: Goose.
  - Reload by command: Gemini CLI (`/skills reload`), Amp (`reload_skills`), Copilot CLI (`/skills reload`), Cursor (at start).

  So the activation message should say that a running session may need its reload command.

## 3. What harnesses do to the body

The standard defines no preprocessing. Implementations add their own; issue [#124](https://github.com/agentskills/agentskills/issues/124), on dynamic context injection, is an open proposal.

| Harness | What it changes in `SKILL.md` before the model sees it |
|---|---|
| Claude Code | Runs `` !`cmd` `` (at line start or after whitespace) and ` ```! ` blocks, **even inside code fences** (tested, below). Replaces `$ARGUMENTS`, `$ARGUMENTS[N]`, `$N` and `${CLAUDE_*}`. `\$` escapes all but `${CLAUDE_*}`, and "A doubled backslash such as `\\$1` leaves both backslashes in place, and `$1` still expands". |
| Goose | When arguments are passed, replaces `\$ARGUMENTS\[(?P<idx>\d+)\]\|\$ARGUMENTS\b\|\$(?P<pos>\d+)\|\$(?P<name>…)` (declared names only) everywhere, fences included. It has **no escape**: `\$1` keeps its backslash and still expands ([arguments.rs](https://github.com/block/goose/blob/bfe8ccd23de1d2c47a4cb2970d522f093ae7f2ba/crates/goose-agent/src/skills/arguments.rs#L37-L83)). Frontmatter is split on `---`. |
| Letta Code | Replaces `<SKILL_DIR>` and `${CLAUDE_SKILL_DIR}` across the whole file. |
| Factory Droid | Expands `${DROID_PLUGIN_ROOT}`, for plugin skills only. |
| VS Code Copilot | The model reads the raw file. The shared prompt parser recognises Markdown links, `#file:`, `#tool:` and `{{x}}` outside code spans and fences. Whether skills get them resolved was not confirmed. |
| Codex, Gemini CLI, OpenCode, Prime | Pass the body unchanged. |
| Amp, Copilot CLI, Cursor, Devin, Junie, Kiro, Antigravity | Not documented. |

### Claude Code experiments (2.1.318, `claude -p --model haiku`, project skills symlinked into a CAS under `/tmp`)

| Skill body | Allowlist | Result |
|---|---|---|
| ` ```! ` block running `touch` outside the project | none | invocation aborted by the permission check; nothing ran |
| `` 1. Run !`touch INJECTED-INLINE` `` and a ` ```! ` block | `Bash(touch:*)` | **both files created** |
| the same lines inside a ` ````text ` fence, and indented | `Bash(touch:*)` | **all three ran** |
| the same line in `references/procedure.md` | `Bash(touch:*)` | not run |
| `$0 $1 $ARGUMENTS $ARGUMENTS[0] ${CLAUDE_SKILL_DIR} \$ARGUMENTS $$ARGUMENTS $HOME`, args `AAA BBB` | — | `AAA BBB [AAA BBB] AAA <link path> $ARGUMENTS $AAA BBB $HOME` |
| `` ! `touch SPACED` ``, ``!\`touch BACKSLASHED\` ``, `` ```! `` mid-line | `Bash(touch:*)` | left as text, but see below |

A project skill's `references/` file could not be read through the symlink: "outside the allowed working directories, so the read was denied".

### Compile rule: shown or referenced, never escaped

No escape works everywhere: Claude Code's `\$` stays a literal backslash in Goose. So Relic does not escape. For each step, the compiler checks the redacted `safe_text` against the union of the hazards above:

- `$` followed by a digit; `$ARGUMENTS`; `${CLAUDE_`; `${DROID_`; `<SKILL_DIR>`;
- `!` immediately followed by a backtick, anywhere;
- ` ```! ` anywhere.

The 2.1.318 binary's patterns are ``/```!\s*\n?[\s\S]*?\n?```/g`` and ``/(?<=^|\s)!`[^`]+`/gm``. Neither anchors ` ```! ` to a line start, and `\s` covers Unicode whitespace such as a no-break space or `\v`. The mid-line ` ```! ` above was left as text only because no closing fence followed it; every fenced record supplies one. So Relic refuses both forms anywhere, a superset of both patterns. A record holding a bare `\r` is shown by id too: CommonMark ends a line there, which would let the rest out of its fence. Step prose is joined onto one line before the check.
- `#file:`, `#tool:` or `{{`, for VS Code. Steps sit in code fences, which VS Code skips, but checking costs nothing.

A step with none of these is shown in a fenced block. The fence is longer than any backtick run in the text, per CommonMark, so the text cannot close it. A step with any of them is **shown by reference**: the body names its record id, says the exact text is withheld because agents rewrite it, and tells the agent to read it with `relic show <id>`.

`relic show` is a read-only command that prints one record's redacted text and does not exist yet. `$1` inside `awk '{print $1}'` is common in shell history, so referencing will happen. It is a read-only Relic command, not a recovered procedure. It also reports a forgotten record as gone, which embedded text cannot.

Frontmatter is generated by Relic and never contains `---`, `$`, `!` or backticks.

### Redacted spans

A `[REDACTED:<kind>]` marker stays in the step as written. None of the harnesses checked treats it specially; VS Code would read a link only if `(` followed it directly. The body says each marker is a value the user must supply and the agent asks for it rather than guessing it. Nothing redacted is recovered into the bundle.

### Evidence for the open `FinishV1.procedure` field set

Nothing here needs a new field. The compiler needs each step's text and its cited record ids, which `{ text, supports }` gives. Keep the shape and settle it in V2, as [0012](../adr/0012-validation-boundary.md) says.

## 4. `scripts/`, model invocation and "silently executing recovered procedures"

- The spec allows `scripts/`, "executable code that agents can run". A script built from recovered steps would be Relic writing an executable from recalled commands, which [0012](../adr/0012-validation-boundary.md) forbids. **No `scripts/`.**
- `allowed-tools` pre-approves tools. In Claude Code, "Workspace trust doesn't gate this field", even for a `-p` run in an untrusted folder. **Relic never emits it**, nor any vendor field (hooks, `context`, `model`), which skills-ref rejects anyway.
- With those out, and hazardous steps referenced rather than embedded, invoking a compiled skill injects text and nothing else. Whether a command then runs without a prompt depends on the user's own agent settings, not on anything Relic wrote. The body says "show these steps and ask before running any of them".
- `disable-model-invocation` stays unset. The reuse gate's release session needs the model to pick the skill ([0016](../adr/0016-acceptance-fixture-and-reuse-gate.md)), and some harnesses (Prime, Gemini CLI) don't read the field anyway.

## 5. Bundle layout and frontmatter

- **Layout:** `<data dir>/catalog/<hash>/<name>/SKILL.md`. The link is `<skills dir>/<name>` → `<data dir>/catalog/<hash>/<name>`.
  - skills-ref checks the unresolved directory name, and `to-prompt` resolves the link. Both see `<name>`.
  - Prime and Codex both drop duplicates by real path, so two links to one bundle load once.
- **Frontmatter:**
  ```yaml
  name: <name>
  description: <what it recovers and when to use it, ≤1024 characters>
  metadata:
    relic-bundle: blake3:<hex>
    relic-version: <x.y.z>
  ```
  The spec says to make metadata keys "reasonably unique". `version` is left out of the top level, per the maintainers. The bundle hash names the content as the digest in PR #254 would: the content-addressed directory is already the version.
- **Not verified:** whether Copilot CLI, Devin, Junie, Factory and Antigravity follow a symlinked skill directory. All of them read `~/.agents/skills`, so V2 should test one or two of them before promising them.

## 6. Agent traces that record skill use

### Claude Code

- Official position: "The entry format is internal to Claude Code and changes between versions, so scripts that parse these files directly can break on any release" ([sessions docs](https://code.claude.com/docs/en/sessions), fetched 2026-11-05).
- Observed shape, from counting records in this machine's `~/.claude/projects` without reading payloads (1428 files, versions 2.1.288–2.1.318):
  - A model invocation is an `assistant` record with a content block `{"type":"tool_use","name":"Skill","input":{"skill":<name>,"args"?:<string>}}` (163 seen, 14 versions).
  - It is followed by a `user` record holding the `tool_result` ("Launching skill: <name>") and `toolUseResult: {"success": true, "commandName": <name>}`.
  - A typed `/name` arrives as a user message containing `<command-name>` (141 seen).
  - `attachment` records of type `skill_listing` carry a `names` array, and `invoked_skills` carries `skills: [{name, path, content}]`.
  - Every record carries `version`, the Claude Code version.
- The rendered skill body, with substitutions applied, is stored as an `isMeta` user message. So the transcript holds the skill text too: an imported trace re-imports a compiled skill's step text as agent evidence.
- **Conclusion:** parseable, and seen unchanged across Claude Code 2.1.288–2.1.318 here, but the format is explicitly unstable. The S10 adapter pins a tested version range with fixtures and treats anything else as an unknown variant ([source contracts](source-evidence-contracts.md)). ADR 0016 already keeps it as release evidence, not a gate.

### Prime Agent

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
- **Conclusion:** activation into `~/.agents/skills` reaches Prime with no Prime-specific step. Its skill use is not a typed transcript event, so Prime is not usable as reuse-gate evidence.

## 7. Durable citations

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

## 8. Verifiers

- `source/git.rs`'s `bounded` runner already has what [0012](../adr/0012-validation-boundary.md) needs: fixed argv, no shell, its own process group killed at the timeout, output capped, and stderr drained. The verifier generalizes it, setting a cwd the user chose.
- `main` sets `umask(0o077)` and drops the old value (src/main.rs:17-18). A verifier must run under the user's original umask, so `main` keeps the returned mask and the verifier's `pre_exec` restores it.
- The Git fixture's verifier, `git rev-parse --verify <ref>`, runs in well under the 10 s bound.

## 9. Durability costs

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

## V2 findings

[V2](https://github.com/yuann3/relic/issues/33) built the path in `src/validation.rs` and `src/catalog/`, and `a_validated_recall_becomes_a_skill_the_host_step_picks_for_a_sibling_question` in `tests/recall_cli.rs` runs it end to end on a lost-commit fixture: a scripted recall, `relic validate`, `relic skill compile`, `relic skill activate --user --yes`, a sibling question, a distractor, and `relic skill rollback`.

- **The V1 decisions held.** The bundle is spec-only, links go into `.agents/skills`, nothing is escaped, and citations are `[id, keyed hash]` pairs stored with each bundle in `catalog.db`.
- **The verifier** runs as section 8 describes and is labelled `unisolated`. Its argv is shell-quoted into one command line and redacted with its cwd, so `--password VALUE` given as two arguments is redacted as it would be in history: the test puts a canary in the verifier's argv and finds none in the catalog. Nothing it starts outlives the run, and a fatal signal to Relic kills its process group first. A record confirms only when the verifier exits 0, and compile needs one confirmed for the blake3 digest of the exact report file, so an edited report has to be validated again.
- **Commands come from the store, not the report.** Compile reads each cited record's `safe_text` from the store, and a forgotten record stops compilation. Step prose is the report's: a hand-edited step can change prose, which passes the same hazard check, but cannot change a recorded command.
- **On the fixture, one recovery step of three was shown by reference.** `git reflog | awk '{print $1}'` holds `$1`. `relic show` landed before the compiler.
- **`FinishV1.procedure` keeps `{ text, supports }`.** The compiler needed nothing more.
- **The catalog lives beside the store** in `<db>.catalog/`, so a `--db` store keeps its own skills, and a recall never creates one. SQLite deletes the WAL when the last connection closes, so after a command only `catalog.db` and the bundles remain.
- **Activation** refuses a path holding a real directory or a link Relic did not make, checks every link before swapping any, and puts back the links already swapped if a later swap fails. Rollback undoes the newest activation not yet undone, so repeating it walks back through history.
- **The host step** (`catalog::select`) takes words of 3 or more characters, minus stop words, from the question and from each active skill's name and description. It picks the skill sharing the most words, if it shares at least 2 and no other skill shares as many. It only considers links that still point where Relic put them, under the home directory or the cwd's projects, with the nearer project's skill shadowing the user's of the same name. Words any question holds, such as *work* and *why*, are stop words, so "why won't git work in my repo?" picks nothing. It runs before the first request, and the recall log records a `skill` line between `start` and the first `send`. The report's `skill` field gives the name and bundle hash. "I dropped a stash by mistake, how do I recover it with git?" picks `git-reflog-recovery`, and a certbot question picks nothing.
- **Deferred to S5:** the pick is logged and reported, but the investigation does not use it yet. Seeding the root state with the skill belongs with the seeded cases. The TUI passes no skills.

## Still open

- What `forget` does to skills that cite a forgotten record. The catalog now stores each bundle's cited ids, so naming those skills is one query ([S13](https://github.com/yuann3/relic/issues/38) covers forget for Git records).
- Whether the opt-in `.claude/skills` link is still needed once claude-code#16345 lands.
- Symlink loading in one or two of the closed harnesses that read `~/.agents/skills` (Cursor, Copilot CLI).
- A Linux run of the durability costs on bare ext4.
