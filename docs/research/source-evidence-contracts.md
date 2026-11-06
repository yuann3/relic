# Source and evidence contracts for Relic v0.1

**Status:** recommended specification  
**Decision scope:** ingestion, provenance, identity, shell hooks, and adapter truthfulness  
**Research date:** 2026-08-16

## Decision

Relic must treat every input as a **versioned, untrusted external artifact**, not as a database it owns. The import pipeline is:

```text
approved source
  -> stable bounded snapshot/read window
  -> byte parser with warnings
  -> ephemeral typed raw observation
  -> redact and normalize
  -> transactional sanitized observation + FTS row
  -> immutable evidence handle
```

Raw shell histories and agent transcripts remain in their original locations. Relic persists sanitized observations, field-level provenance, parse warnings, and locators sufficient to explain the source. It never stores raw records and never makes missing facts appear through inference.

V1 ships five importer families:

1. native zsh history;
2. native Bash history;
3. metadata-only zsh/Bash hook frames;
4. Git correlations, always as enrichment rather than command truth;
5. versioned Prime Agent and Claude Code JSONL adapters.

Each adapter owns a parser contract version. Unknown record variants are retained as bounded warnings/opaque metadata or skipped; they do not crash the whole import or get silently interpreted as commands.

## Primary-source findings

### Zsh history is a byte format, not line-delimited UTF-8

With `EXTENDED_HISTORY`, zsh writes `: <beginning time>:<elapsed seconds>;<command>`; `SHARE_HISTORY` also writes extended entries. Zsh documents that history can be incrementally appended, imported across shells, and trimmed/re-written under multiple options, so file order is write/import order rather than a globally reliable session chronology ([zsh History options](https://zsh.sourceforge.io/Doc/Release/Options.html#History)).

The canonical reader/writer in `Src/hist.c` handles escaped leading colons, backslash-newline continuation, extended prefixes, and zsh's internal metafied byte representation ([zsh `hist.c`](https://github.com/zsh-users/zsh/blob/master/Src/hist.c)). Therefore:

- parse raw bytes before any UTF-8 conversion;
- recognize an extended prefix strictly, not any command beginning with `:`;
- join physical backslash-newline continuations into one logical record;
- unescape `\:` where the canonical history encoding uses it for a literal leading colon;
- unmetafy before charset decoding;
- preserve malformed prefixes, dangling metafy bytes, embedded NULs, invalid encodings, and oversized records as bounded warnings;
- mark lossy text as lossy evidence, never exact bytes.

The timestamp is second-resolution. Duration `0` may be unavailable; it is not proof of instantaneous execution. Identical timestamps are ties. `INC_APPEND_HISTORY`, `INC_APPEND_HISTORY_TIME`, `SHARE_HISTORY`, expiration, deduplication, private shells, crashes, and parallel shells mean absence is not evidence that a command did not execute.

### Bash history cannot always recover logical entries

Bash documents the history list, `HISTFILE`, `HISTFILESIZE`, `HISTCONTROL`, `HISTIGNORE`, `HISTTIMEFORMAT`, `cmdhist`, `lithist`, and append/overwrite behavior ([Bash history facilities](https://www.gnu.org/software/bash/manual/html_node/Bash-History-Facilities.html), [Bash variables](https://www.gnu.org/software/bash/manual/html_node/Bash-Variables.html)). GNU Readline's history-file implementation is the authoritative byte/file behavior ([Readline `histfile.c`](https://cgit.git.savannah.gnu.org/cgit/bash.git/tree/lib/readline/histfile.c)).

Persisted timestamp markers normally have the shape `#<epoch-seconds>` and apply to the following history entry. The comment character is configurable and the file does not self-describe it, so V1 defaults to `#` and accepts an explicit source override.

When timestamp markers delimit entries, physical lines until the next marker may be one literal multiline entry. Without markers, a `lithist` multiline command is indistinguishable from multiple commands. Relic must import each physical line separately with `multiline_ambiguous=true`; invoking a shell parser to guess would manufacture evidence.

Parallel Bash sessions may append, overwrite on exit, reorder, or erase history. Native history does not encode stable session identity, cwd, end time, duration, exit status, output, expanded argv, Git HEAD, or success.

### Hooks have asymmetric guarantees

Zsh provides `preexec` and `precmd` hook points, and its supported `add-zsh-hook` helper composes functions instead of replacing unrelated user hooks ([zsh hook functions](https://zsh.sourceforge.io/Doc/Release/Functions.html#Hook-Functions), [`add-zsh-hook`](https://zsh.sourceforge.io/Doc/Release/User-Contributions.html#Hook-Functions)). V1 may pair a pre-frame and post-frame by `(installation_id, session_id, sequence)`.

Bash has no equally clean composable pre-execution event. `PROMPT_COMMAND` runs before the primary prompt and can preserve the preceding status; a `DEBUG` trap also fires for functions and simple/internal commands and is not one event per entered command. V1 deliberately uses post-only `PROMPT_COMMAND` composition and makes no start/duration/cwd-before claim.

Hooks never edit configuration silently. `relic hook install --print zsh|bash` prints a snippet for review. Frames contain command metadata only and travel to a helper on stdin—not argv—so commands do not leak through process listings. No output, stdin, terminal contents, expanded environment, keystrokes, clipboard, or expanded argv are captured.

### Git is evidence enrichment, not historical shell replay

Git reflogs record ref updates and can expire; they are not command logs ([Git reflog](https://git-scm.com/docs/git-reflog)). Relic may use explicit `git -C`/path arguments, an observed hook cwd, recorded agent cwd/repo, commit IDs, and bounded timestamp windows as clues. It must not replay `cd` commands to invent historical cwd: histories merge sessions and omit directory stacks, functions, aliases, subshells, and moved paths.

A Git association has one of these qualities:

- **exact:** source encoded cwd/repo and Git verifies the root/object;
- **high:** hook observed cwd and optional HEAD at command time;
- **low:** a timestamped native command falls inside an explicitly approved repository window;
- **none:** no encoded/observed path or usable time.

Only exact/high relations participate automatically in scope routing. Low relations are displayed as inference and require confirmation.

### Prime Agent and Claude Code traces are versioned implementation formats

Prime Agent 0.7.0 identifies its public repository in its installed `package.json`; its first-party session manager writes typed JSONL records ([Prime Agent repository](https://github.com/PrimeIntellect-ai/prime-agent), [session-manager source](https://github.com/PrimeIntellect-ai/prime-agent/blob/main/packages/coding-agent/src/core/session-manager.ts)). Local shape inspection without reading personal payloads confirms an initial `session` record followed by typed records such as `message`, `git_state`, `model_change`, `child_usage_attributed`, and `agent_status`, with record IDs, parent links, timestamps, model/tool message envelopes, and optional cwd/Git metadata.

The V1 Prime adapter is pinned to a tested Prime schema/package range. It imports model messages, tool calls/results, status, usage, and session/Git facts as distinct observation kinds. A tool result is not a shell success claim unless the record explicitly associates the result with that tool call and encodes the result/status.

Claude Code's official hook contract exposes `session_id`, `cwd`, and `transcript_path`, and hooks receive typed JSON on stdin ([Claude Code hooks](https://code.claude.com/docs/en/hooks)). The exact on-disk transcript is not documented as a stable third-party interchange standard. Local shape inspection of Claude Code 2.1.233, without retaining payload text, finds project session JSONL records including `user`, `assistant`, `system`, `attachment`, `file-history-*`, `queue-operation`, and worktree/session metadata. User/assistant envelopes can include cwd, version, Git branch, parent UUID, timestamps, message blocks, tool-use results, and side-chain flags.

Therefore the Claude adapter is version-detected and fixture-tested, not “generic JSONL.” It treats unknown additive fields as ignorable, unknown record types as opaque/skipped with counters, missing required identity fields as malformed, and incompatible major shapes as `UnsupportedSourceVersion`. Relic never indexes global `history.jsonl` and full project transcripts as though they had identical semantics.

#### The Claude Code adapter as built ([S10](https://github.com/yuann3/relic/issues/34))

`src/source/claude.rs`, contract `claude-code-v1`. The Prime adapter ([S11](https://github.com/yuann3/relic/issues/35)) was dropped from v0.1, so Claude Code is the only agent source.

- **Pinned to 2.1.288–2.1.318.** The fixture `tests/fixtures/claude-code/2.1.318.jsonl` is a real `claude -p` session recorded on 2.1.318. Records describing the recording machine (account and org ids, skill, agent and tool listings, system prompt, hooks) were removed; the session's own `user`, `assistant` and queue records are unchanged. The parser was also run over 966 local sessions from 2.1.288–2.1.316 (1.9 GB, 4 s): no record failed to parse except the 79 lines over the 1 MiB record cap. A `user` or `assistant` record from any other version stops the parse there, with the cursor before it. Nothing from that version on is stored, so a later Relic that knows it resumes at that record; skipping it would have lost those records for good behind the cursor.
- **What becomes a record:** three new observation kinds. `AgentCommand` (source label `claude`) is a Bash call's `command`. `AgentOutput` (`claude-out`) is its result as the model saw it: prefixed `failed:` or `interrupted:` when the record says so, and over 1 KiB cut to its first and last 508 bytes. `AgentTool` (`claude-tool`) is `<Tool> <arg>` for Skill (with its args), Read, Edit, Write, Grep, Glob, WebSearch, WebFetch and Agent. Prompts, replies, thinking, tool descriptions, skill bodies (`isMeta`), attachments, MCP and other tools' calls and results, and the `api-request-blob` copies of calls are never stored. The canary gate puts a secret in each and finds none.
- **Results pair by `tool_use_id`.** Most Bash results (17,618 of 27,639 locally) carry no `toolUseResult`, and failed ones carry a string, so a result is Bash's only because it answers a Bash call. A call and its result can fall either side of an append boundary, so a Claude scan always reads the session from its first byte and yields only the records past the boundary. Its output equals a whole reparse; a session that grew is read again in full. Unchanged sessions are skipped by their stat hint.
- **Timestamps** are read only in the `YYYY-MM-DDTHH:MM:SS[.fff]Z` form these versions write, kept to the second like other sources.
- **`relic import --claude [DIR]`** imports every `*.jsonl` below `$CLAUDE_CONFIG_DIR/projects` or `~/.claude/projects`, subagent sessions included, each its own source. A single `*.jsonl` file whose first line is a JSON object is detected without the flag. Locally: 59,164 records from 966 sessions in 6.8 s at 35 MB peak RSS, a 61 MB database, and 0.07 s to re-import unchanged.
- **For the reuse gate (S5):** a model-chosen skill use is an `AgentTool` record `Skill <name>`, and the records after it in the session's ordinal order are what the agent did next, so "the Skill invocation before the first search-type tool call" is a query over one revision. Typed `/name` invocations are user prompts and are not kept.

## Normative V1 model

```rust
struct Source {
    source_id: SourceId,             // keyed logical identity
    kind: SourceKind,
    approved_locator: ApprovedPath,  // private; never model-visible
    parser_contract: ParserContract,
}

struct SourceRevision {
    revision_id: SourceRevisionId,
    source_id: SourceId,
    parser_contract: ParserContract,
    file_identity: FileIdentity,     // platform dev/inode or equivalent
    opened_len: u64,
    stable_len: u64,                 // complete parsed boundary
    mtime_hint: Option<SystemTime>,
    boundary_guard: KeyedHash,
    snapshot_fingerprint: KeyedHash,
    imported_at: Timestamp,
    complete: bool,
}

struct Observation {
    observation_id: ObservationId,
    revision_id: SourceRevisionId,
    ordinal: u64,
    locator: SourceLocator,
    kind: ObservationKind,
    safe_payload: SafePayload,
    field_provenance: FieldProvenance,
    keyed_fingerprint: KeyedHash,
    parser_warnings: SmallVec<ParseWarning>,
}

struct EvidenceHandle {
    evidence_id: EvidenceId,         // opaque/keyed; no path leakage
    observation_id: ObservationId,
    revision_id: SourceRevisionId,
    safe_payload_hash: Blake3Hash,
    parser_contract: ParserContract,
    redaction_revision: RedactionRevision,
}

enum FactOrigin {
    EncodedBySource,
    ObservedByHook,
    ImportedRelation,
    InferredByRule { rule: RuleId },
    UserAsserted,
    ConfirmedByValidation,
}

enum FactQuality { Exact, High, Low, Unknown }
```

`FieldProvenance` is per field, not one confidence number for an event. Every inferred fact names its rule and inputs. Missing remains `None`; `Unknown` is not false.

### Sanitized evidence remains citable after source mutation

Relic stores the sanitized payload and its hash. A citation resolves to that immutable safe observation even after the live history is compacted. Its source locator explains origin but does not promise the mutable raw file still contains the bytes. The UI distinguishes “safe indexed copy available” from “raw source revision still matches.”

Paths and common command fingerprints must not be exposed as public unsalted hashes. Generate logical source IDs, event fingerprints, and displayed evidence IDs with keyed BLAKE3 using an installation key. Canonical internal rows—not shortened display strings—remain authoritative.

## Incremental import contract

Every file importer follows this state machine:

```text
open approved regular file without following a final symlink
 -> fstat identity and length
 -> compare to prior cursor
 -> unchanged: metadata/guard agreement => stop
 -> clean append: parse [stable_boundary, opened_length)
 -> otherwise: create new revision and stream reparse
 -> fstat again
 -> if identity changed or relevant prefix raced: retry/defer
 -> commit revision + observations + FTS in one transaction
```

A metadata tuple is a fast no-change hint, not cryptographic truth. The cursor retains a small keyed guard window ending at the last stable logical boundary. Same identity + monotonic length + matching guard permits append parsing. Truncation, inode replacement, mtime regression with changed guard, parser/redaction contract change, or a failed before/after stability check creates a new revision/full reparse.

Do not set-deduplicate. Repeated identical commands are separate occurrences. During a full reparse, sequence alignment of keyed logical-record fingerprints may preserve identity only when unambiguous; otherwise create new observations/revision relations rather than falsely merging events.

### Per-source cursor

| Source | Cursor and idempotency | Incomplete tail |
|---|---|---|
| zsh history | file identity, stable byte boundary, logical ordinal, boundary guard, parser version; event fingerprint includes canonical safe record plus occurrence context | do not advance past trailing continuation/metafy/prefix fragment |
| Bash history | identity, stable physical/logical boundary, ordinal, guard, configured timestamp marker/parser version | do not advance past an unresolved timestamp marker or bounded partial final line |
| hook file | one writer per session file; `(installation, session, sequence, phase)` is unique; frame is length-prefixed and checksummed | ignore until complete frame arrives |
| Prime JSONL | file identity, newline boundary, record ID/type/version where present, keyed record hash; parent links retained | stop before incomplete JSON line |
| Claude JSONL | detected Claude version/schema family, identity, newline boundary, UUID/type where present, keyed record hash | stop before incomplete JSON line |

For the first release, a transactional streaming reparse of 100k native records is preferred over fragile tail cleverness and must still meet the accepted `<1 s` goal. Append optimization is enabled only after the prototype proves rewrite/truncation correctness.

## Hook frame V1

Use private state directories (`0700`) and per-session append files (`0600`). A session has one writer, avoiding lock contention. The helper accepts a bounded binary/JSON payload on stdin and writes a length-prefixed, checksummed frame. It is silent and always returns quickly enough not to damage prompt rendering; failures increment a local diagnostic counter but never alter shell status.

```rust
struct HookFrameV1 {
    version: u8,
    installation_id: InstallationId,
    session_id: SessionId,       // random 128+ bits at shell init
    sequence: u64,
    phase: HookPhase,            // zsh_pre | zsh_post | bash_post
    receipt_wall_time: Timestamp,
    monotonic_ns: Option<u64>,
    command_text: Option<Bytes>, // typed command form, sent on stdin
    cwd_before: Option<Bytes>,
    cwd_after: Option<Bytes>,
    exit_status: Option<i32>,
    pipeline_statuses: Vec<i32>,
    history_number: Option<u64>,
    git_head_before: Option<Oid>,
    git_head_after: Option<Oid>,
}
```

Zsh pre/post frames may derive duration from monotonic timestamps when paired. Unmatched pre-frames are `Incomplete`, not failed. Background-job completion is outside the command event. Bash post frames have no start, duration, or cwd-before. Hook installation tests coexistence with existing hooks and scalar/array `PROMPT_COMMAND`; it never assumes exclusive ownership.

Git HEAD capture is optional and should remain off until its added prompt latency is measured. A later background enrichment can often resolve current repository facts without putting `git` in the hook path.

## Redaction boundary

The only component allowed to see an ephemeral raw parsed record is the importer/redactor pipeline. Before SQLite, FTS, diagnostic logs, caches, event JSON, model calls, or compiled assets:

1. apply source-specific structural extraction;
2. discard known secret-only fields and output bodies outside scope;
3. run compiled literal/prefix/assignment/token matchers;
4. run bounded contextual regex/entropy checks only on candidates;
5. replace matched spans with typed redaction markers;
6. compute keyed identity over canonicalized private input and public hash over safe payload;
7. drop raw buffers promptly.

Dry-run output itself is sensitive. `relic import --dry-run` reports counts, source types, warning classes, and redaction categories by default; showing safe samples requires an explicit TTY confirmation. `recall --show-context` displays exactly the sanitized model packet, never raw pre-redaction bytes.

No detector proves absence of secrets. Documentation and UI say “policy redacted,” not “secret-free.”

## Evidence-quality rules

| Claim | Minimum support | Forbidden shortcut |
|---|---|---|
| command text occurred | exact native/hook/agent encoded record | inferred from nearby prose |
| command started at time | encoded extended-history/hook/agent timestamp, with source resolution noted | file mtime |
| cwd at execution | hook/agent encoded cwd | replaying `cd` from merged history |
| command succeeded | paired hook exit status or explicit tool result/status | following command, duration, or lack of error text |
| output was produced | explicit supported agent tool-result event | shell history alone |
| Git repo/HEAD | observed hook/agent fact or Git-verified explicit path/object | timestamp proximity presented as exact |
| procedure solved problem | current explicit verifier or user assertion plus citations | command sequence alone |
| command did not run | never established solely by absence | history absence |

Native history supports “observed command” and sometimes approximate start time. It does not support historical success. Agent messages are hostile prose; only adapter-understood structured tool/result fields become tool observations.

## Fail-closed and bounded failure behavior

- Source not approved, non-regular file, unsafe ownership/permissions, or final symlink: do not import; explain.
- File races during read: no partial commit; retry within a small bound or defer.
- Record over limit: quarantine one warning with byte range/hash, skip bounded bytes, continue at a proven boundary when possible.
- Invalid bytes: safe escaped display or lossy decode flag; never inject invalid raw text into FTS/model.
- Unknown adapter version/type: count and expose it; never reinterpret.
- Agent graph cycles/missing parents: keep record identity but mark broken relation.
- Hook sequence gaps/duplicate frames: keep gap diagnostic; unique key makes replay idempotent.
- Parser or redaction upgrade: new source revision and re-index; old evidence remains addressable for investigations already committed.

## Tests required before trusting an adapter

### Parser/fuzz corpus

- zsh plain/extended, escaped colon, multiline continuation, metafied bytes, Meta-at-EOF, malformed prefixes, tied timestamps, huge/NUL/invalid records;
- Bash timestamps, configured marker, `cmdhist`/`lithist`, timestamp without entry, ambiguous unmarked multiline, append/overwrite/truncate;
- partial writes at every byte boundary and rotation/replacement during read;
- duplicate commands, reordered/rewritten subsequences, parallel sessions;
- Prime and Claude golden fixtures for supported versions, unknown additive fields/types, malformed lines, missing IDs, parent cycles, tool-call/result linking;
- redaction canaries in every field and proof no canary enters SQLite, FTS, logs, cache, or mock model transport.

### Hook integration

Run real zsh and the macOS system Bash 3.2 plus a current GNU Bash on Linux. Verify status preservation, pipeline statuses where promised, multiline commands, cancellation, `exec`, shell death, background jobs, nested prompts, existing zsh hooks, scalar/array `PROMPT_COMMAND`, history disabled, private modes, Unicode/invalid bytes, helper missing/slow/full disk, and measured prompt overhead.

### Incremental invariants

Property/model tests assert:

- re-importing an unchanged revision adds zero observations;
- clean append adds each complete record exactly once;
- crash before commit exposes neither revision nor FTS rows;
- rewrite/truncation never silently attaches an old cursor to new bytes;
- repeated identical commands remain distinct;
- every displayed/model citation resolves to the stored safe payload/hash/parser/redaction contract.

## Implications for the performance prototype

The blocked ingestion/search prototype must use representative byte fixtures, not generated newline-only ASCII:

| Fixture | Required cases |
|---|---|
| 100k and 1M native | mixed zsh extended/plain/metafied/multiline and Bash timestamped/ambiguous records |
| append | 100 complete records plus incomplete zsh/Bash tails |
| rewrite | same length/mtime-like metadata but changed boundary guard; truncation and inode replacement |
| hooks | thousands of paired/unpaired session frames and sequence gaps |
| agents | Prime/Claude typed messages, tool events, unknown variants, malformed lines |
| security | secret canaries early/late/across chunks and in nested agent fields |

Measure parser, redaction, SQLite, and FTS time separately; bytes read/allocated; peak RSS; DB/FTS bytes; warning counts; no-change stat/guard path; clean append; full reparse. The implementation may enable append cursors only if outputs are byte-for-byte equivalent to full reparse under the fault corpus.

## Open decisions deferred to implementation/prototypes

- Exact zsh unmetafication implementation should be validated against pinned `hist.c` fixtures rather than rewritten from memory.
- Whether source revision snapshot fingerprints hash all bytes or sampled/guard regions depends on import throughput; evidence payload hashes remain exact regardless.
- Hook encoding may be compact JSON-with-length or a hand-written binary frame. Benchmark process startup and bytes before adding a codec dependency.
- Whether Git HEAD capture belongs in the synchronous hook path is a latency measurement, not an assumption.
- Prime and Claude supported version ranges must be derived from checked-in fixtures and explicit adapter compatibility tests; neither local JSONL should be marketed as a stable public interchange API.
