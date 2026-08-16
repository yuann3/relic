# TUI and fzf integration architecture for Relic v0.1

**Status:** recommended architecture pending the interactive prototype  
**Decision scope:** CLI/TUI composition, event/effect model, terminal lifecycle, optional fzf, accessibility, tests, and feature cost  
**Research date:** 2026-08-16

## Decision

Relic has **one application/use-case core and two presentation adapters**:

```text
CLI / versioned JSON ─┐
                      ├─> app commands -> typed events/effects -> domain/store/RLM
Ratatui TUI ──────────┘

SQLite FTS5 -> bounded search page -> internal TUI list
                                  └-> optional external fzf picker
```

The CLI is complete and authoritative for automation. The TUI is a focused, interruptible investigation workspace over the same commands/events; it contains no business logic. `fzf` is an optional external picker over a **bounded FTS result page**, never the index and never a required dependency. Relic does not pipe the million-event corpus to it.

Use a blocking main UI loop and a small number of bounded worker threads/channels, not Tokio. Compile Ratatui/Crossterm behind a `tui` Cargo feature and continuously measure both headless and full artifacts. The distributed full build includes the TUI; a headless build remains available for CI/minimal installations.

## Primary-source constraints

Ratatui is an immediate-mode renderer: application state is rendered each frame, and its `TestBackend` supports deterministic rendering tests ([Ratatui documentation](https://docs.rs/ratatui/latest/ratatui/), [Ratatui testing guidance](https://ratatui.rs/recipes/testing/)). Relic should own state and update semantics rather than storing domain state inside widgets.

Crossterm raw mode disables normal terminal line processing; alternate-screen entry/exit and raw-mode enable/disable must be paired. Its event API provides blocking `read` and timed `poll`, and it emits resize and key events ([Crossterm terminal module](https://docs.rs/crossterm/latest/crossterm/terminal/), [Crossterm event module](https://docs.rs/crossterm/latest/crossterm/event/)). Terminal restoration is therefore a correctness requirement, not UI polish.

The official fzf interface accepts candidates on stdin, supports NUL-delimited input/output with `--read0`/`--print0`, field selection/delimiters, bounded layout such as `--height`, multi/select controls, and deterministic noninteractive `--filter` mode ([fzf manual](https://github.com/junegunn/fzf/blob/master/man/man1/fzf.1), [fzf advanced examples](https://github.com/junegunn/fzf/blob/master/ADVANCED.md)). Relic must use fixed argv, not build shell expressions. fzf's powerful `--preview`, reload, execute, transform, and shell-command bindings are deliberately outside v0.1 because interpolating history text into commands creates a shell-injection boundary.

Rust's `std::process::Child` does not kill a process on drop; the parent must explicitly kill and wait/reap it ([Rust `Child`](https://doc.rust-lang.org/std/process/struct.Child.html)). Cancellation and panic paths therefore own child lifecycle explicitly.

## Shared application contract

Presentation adapters call use cases; they never call SQLite, Git, model HTTP, or shell helpers directly.

```rust
enum AppCommand {
    Search(SearchRequest),
    OpenEvidence(EvidenceId),
    StartRecall(RecallRequest),
    Cancel(OperationId),
    BuildValidationPlan(InvestigationId),
    RunApprovedValidation(ValidationRequest),
    CompileSkill(CompileRequest),
    ActivateSkill(AssetHash),
    RollbackSkill(StableSkillName),
    LaunchPicker(SearchPageId),
}

enum AppEvent {
    SearchCompleted { request_id: RequestId, page: SearchPage },
    EvidenceOpened(EvidenceView),
    RecallStepStarted { operation: OperationId, step: StepSummary },
    RecallStepCompleted { operation: OperationId, observation: BoundedSummary },
    RecallFinished(CitedFinding),
    ValidationChanged(ValidationView),
    CandidateCompiled(CandidateView),
    SkillActivated(SkillView),
    Failed { operation: Option<OperationId>, error: PublicError },
}
```

The RLM model boundary remains non-streaming in v0.1. “Progress” means typed step/call/budget events emitted by the coordinator, not partially parsed model JSON or token animation. This keeps model contracts deterministic while letting the TUI show that work is alive.

CLI rules:

- `--json` emits one versioned machine document or JSONL event stream on stdout;
- progress/diagnostics go to stderr;
- no ANSI, spinner, prompt, or fzf in non-TTY/JSON mode;
- commands that can mutate/execute require explicit flags or TTY confirmation according to their domain policy;
- TUI state is not a hidden prerequisite for any operation.

## TUI state/update/effect model

Use an Elm-like boundary without adopting a framework:

```rust
struct UiModel {
    route: Route,
    focus: Focus,
    query: String,
    search: SearchState,
    timeline: TimelineState,
    evidence: EvidenceState,
    recall: RecallState,
    candidate: CandidateState,
    modal: Option<Modal>,
    viewport: Rect,
    dirty: bool,
}

enum UiAction {
    Key(KeyIntent),
    Paste(String),
    Resize(u16, u16),
    Tick,
    App(AppEvent),
    PickerReturned(Option<EvidenceId>),
    Suspend,
    Resume,
    Quit,
}

enum UiEffect {
    Dispatch(AppCommand),
    StartDebouncedSearch { generation: u64, query: String },
    OpenFzf(SearchPageId),
    CopySafeText(String),
    SuspendProcess,
    Exit,
}

fn update(model: &mut UiModel, action: UiAction) -> SmallVec<UiEffect>;
fn view(frame: &mut Frame, model: &UiModel);
```

`update` is pure apart from mutating the supplied model. It returns effects; an effect runner performs I/O and sends results back as actions/events. `view` is pure rendering. Request/operation generation IDs fence late search/model/validation results after cancellation or a newer query.

### Thread shape

```text
main thread
  owns terminal, UiModel, update/effect dispatch, render

terminal input thread
  blocking Crossterm read -> bounded UiAction channel

one search worker
  read-only SQLite connection, latest-generation request wins

bounded operation workers
  RLM/validation/subprocess work -> typed AppEvent channel
```

Use bounded `sync_channel`s. The event thread may block on send under extreme input; it must not allocate an unbounded queue. Render only when state becomes dirty, on resize, or at a low-rate progress deadline—never a permanent 60 FPS loop. A spinner is optional decoration and disabled by reduced-motion/no-color settings; progress text and counters carry the meaning.

Debounce interactive search for roughly 50–100 ms as a prototype variable. The UI submits a typed escaped FTS query and a hard result cap. Old generations may finish but are discarded. Do not attempt unsafe cross-thread interruption of a running SQLite call; bounds and generation fencing are sufficient for the `<20 ms` target.

## Focused workspace

The default workspace answers three questions:

```text
What evidence matches?
What does the current finding claim and cite?
What can be safely done next?
```

A responsive wide layout:

```text
┌ Query / scope / budgets ──────────────────────────────────────────────┐
├ Results / episodes ─────┬ Timeline + RLM steps ──┬ Evidence detail ──┤
│ timestamp, source, rank │ handles, relations      │ exact safe text   │
│ confidence/status word │ call/budget progress    │ provenance/quality│
├ Candidate / validation status ───────────────────────────────────────┤
└ key hints · source warnings · model/local mode · operation state ───┘
```

Narrow terminals switch between named panes rather than crushing columns. Routes/tabs are `Search`, `Investigation`, `Candidate`, and `Validation`; this is not a dashboard, chat UI, or agent graph.

Essential interactions:

- typing changes the bounded FTS query;
- arrows/j/k move selection; Enter opens evidence;
- `f` invokes optional fzf over the current bounded page;
- `r` starts/cancels recall with a visible budget confirmation;
- citations open the exact sanitized observation and provenance;
- compile/activate/validation actions show a full confirmation with scope and executable effects;
- Esc cancels the current modal/operation before it exits the whole application;
- `?` shows discoverable key help; `/` focuses search.

The TUI never automatically executes a reconstructed procedure. Validation dialogs display the exact approved verifier argv, workspace assurance, cwd, capabilities, and expected artifacts.

## fzf contract

### Position in the pipeline

```text
user query/filter
 -> Relic query compiler
 -> SQLite FTS5 + metadata, max N candidates
 -> immutable SearchPage { id, evidence_id, one-line safe display }
 -> fzf ranks/selects that page
 -> Relic accepts opaque selected evidence IDs only
```

Start with `N=5,000` and a per-display-record cap of 2 KiB as prototype values. Broad queries that exceed the page explain that the user must narrow; fzf does not silently stand in for global indexing.

### Safe record protocol

Each candidate is one NUL-terminated record:

```text
<opaque evidence id>\t<escaped timestamp>\t<source label>\t<single-line sanitized summary>\0
```

Spawn a fixed argv similar in shape to:

```text
fzf --read0 --print0 --no-multi --delimiter=\t --with-nth=2.. --nth=2.. \
    --height=80% --layout=reverse --border
```

Exact supported flags/minimum version are locked by the prototype. Pipe records to child stdin, close it, read at most one bounded NUL-terminated result from stdout, validate the ID is a member of that immutable `SearchPage`, then discard all display text. Unknown output, multiple records, invalid UTF-8 ID, excessive bytes, nonzero exit, or selection after page invalidation is a typed non-selection/error.

Do not pass query/history text in argv. Do not use `sh -c`. Do not enable preview/execute/reload/transform bindings. Relic opens the selected citation in its own safe evidence pane.

### CLI and TUI use

Primary simple path:

```console
relic search "openssl linker" --fzf
```

When called from the TUI, Relic must first **fully restore** canonical mode and leave the alternate screen, then launch fzf attached to the controlling terminal while candidates use piped stdin and the selected record uses captured stdout. After wait/reap, it re-enters raw/alternate mode, forces a full redraw, and rechecks terminal size. Never run fzf while Crossterm's event thread is concurrently reading from the same terminal; pause/join or gate the reader first.

This suspend-run-restore sequence is accepted only if the PTY prototype proves it on macOS/Linux. If it is unreliable, v0.1 keeps `--fzf` as a CLI picker and the TUI's `f` action explains/launches it only by exiting with a resumable selection token. We do not build an embedded fuzzy clone merely to avoid that honest fallback.

### Discovery and fallback

- Resolve `fzf` from `PATH` only when requested; never during help/import/plain search.
- Run `fzf --version` once per process/use and parse conservatively.
- Prototype a tested minimum; reject an older/unknown incompatible version with install guidance.
- If missing, ordinary FTS and TUI filtering remain complete. The UI displays “fzf unavailable; using indexed search,” not an error at startup.
- Noninteractive/non-TTY/JSON mode never launches it.

## Terminal lifecycle

A `TerminalGuard` owns transitions:

```text
enter:
  verify stdin/stdout TTY
  enable raw mode
  enter alternate screen
  hide cursor
  clear/full draw

restore (idempotent):
  show cursor
  leave alternate screen
  disable raw mode
  flush
```

If entry fails halfway, restore completed steps in reverse. `Drop` performs a best-effort restore, but normal paths call an explicit fallible restore and report errors after the terminal is usable.

Install a panic hook that invokes a minimal idempotent restoration path before chaining/reporting. Avoid mutex-dependent complex work in panic restoration. Integration tests intentionally panic after every entry stage.

Ctrl-C arrives as a key event in raw mode: first press requests cooperative operation cancellation; a second press within a short window restores and exits. Workers receive a cancellation token and every late result is fenced. External children are killed as a process group where supported and then waited/reaped; dropping `Child` is insufficient.

Resize updates the viewport and marks dirty. Paste is treated as data and bounded. Mouse capture is off by default.

For Unix suspend (`Ctrl-Z`/`SIGTSTP`): restore terminal, stop the event reader, then perform the real suspension; on `SIGCONT`, re-enter, restart reading, recalculate size, and redraw. If adding a signal crate violates size/startup gates, ship a documented “suspend unsupported” behavior only after the prototype/architecture ticket explicitly accepts it—never leave the terminal raw.

## Feature and dependency boundary

```toml
[features]
default = ["tui"]          # full user release; CI also builds headless
tui = ["dep:ratatui", "dep:crossterm"]

[dependencies.ratatui]
optional = true
default-features = false
features = ["crossterm"]

[dependencies.crossterm]
optional = true
default-features = false
```

The exact current feature names are verified against pinned crate versions. Do not enable Crossterm's async event stream/futures support. The core library must compile/test with `--no-default-features`; no domain/app module imports Ratatui/Crossterm types.

Official artifacts:

- `relic`: full CLI + TUI, required MVP product artifact;
- `relic-headless` build profile/artifact from the same binary source with TUI disabled, used for CI/minimal environments.

Before accepting versions/features, record `cargo tree -e features`, duplicate crates, stripped binary bytes, warm help, and RSS. TUI crates are never initialized unless `relic ui` runs.

## Accessibility and output discipline

- Every status uses a word/icon pair; color is never the only signal.
- Respect `NO_COLOR`; provide explicit `--color=never|auto|always` for CLI and a monochrome TUI theme.
- Maintain readable contrast and terminal-default compatibility; avoid hardcoded “black” backgrounds.
- Key help is visible and all actions are reachable without a mouse.
- Narrow-screen pane switching preserves content; long safe text wraps without horizontal corruption.
- Unicode width is tested; offer ASCII symbols when terminal capability/locale is insufficient.
- Progress is state/counter based, not motion dependent. Reduced-motion mode disables spinner/tick-only redraw.
- Secret-redaction markers are visually distinct but copy as the safe marker, never raw content.
- The UI clearly labels inferred/unknown provenance and local versus model-egress actions.

## Tests

### Pure state and rendering

- table-driven `update` tests for every mode/action/effect;
- stale request generation and post-cancel result fencing;
- confirmation required before validation/activation;
- Ratatui `TestBackend` snapshots at wide/narrow/minimum sizes, monochrome, Unicode/ASCII, long/malformed text;
- property tests that arbitrary event sequences never produce an unapproved executable effect;
- CLI JSON schema goldens prove TUI additions cannot change machine stdout.

### PTY lifecycle

On macOS and Linux, use a real PTY to test:

- normal exit, Esc/Ctrl-C cancellation, double interrupt;
- panic after each raw/alternate/cursor stage;
- resize storms, terminal disconnect, stdin EOF;
- suspend/resume if supported;
- fzf missing, too old, cancel, success, malformed/multiple/oversized output, timeout, killed parent;
- fake fzf verifies fixed argv, NUL framing, page membership, no history text in argv, and child reaping;
- event reader is stopped while external fzf owns the terminal;
- terminal echo/canonical mode and cursor are restored after every case.

### Performance gates

Measure headless and full builds separately:

| Path | Required proof |
|---|---|
| headless `--help` | warm p95 `<15 ms` |
| plain `search --json` | no TUI/fzf/provider initialization |
| TUI launch to first frame | reported separately with fixture DB |
| interactive query to rendered page | FTS + channel + render p50/p95; target perceptually instant and never worse than bounded query contract |
| fzf launch/selection | startup, 100/1k/5k records, cancel, peak RSS; not included in core FTS claim |
| binary size | headless `8–12 MiB` target; full `12–18 MiB` target |
| idle TUI | effectively zero CPU except blocking input; no unconditional 60 FPS tick |

## Prototype storyboard

[Prove the interactive search workspace](https://github.com/yuann3/relic/issues/10) should implement only:

1. deterministic fixture DB with 100k/1M searchable observations;
2. shared `Search` app command and versioned JSON CLI;
3. TUI query/results/timeline/evidence panes with fake RLM progress events;
4. internal FTS filtering and bounded result paging;
5. `relic search --fzf`, then suspended-TUI fzf only if PTY tests pass;
6. cancellation, resize, panic restoration, narrow/monochrome rendering;
7. startup/query/binary/idle CPU measurements.

The human reacts to this rough artifact before visual polish or compile/validation panes are built.

## Rejected designs

| Design | Reason |
|---|---|
| TUI owns domain/store logic | duplicates CLI and makes JSON behavior diverge |
| generic chat interface | obscures evidence/provenance and expands product boundary |
| animated agent graph | visual theater; no operator decision improved |
| Tokio for UI | blocking terminal + bounded workers suffice; core stays synchronous |
| feed all history to fzf | unbounded memory/startup and bypasses SQLite index/scope |
| embed a fuzzy-search clone | unnecessary second search engine; FTS fallback is complete |
| run fzf inside raw TUI concurrently | two terminal readers and corrupt lifecycle |
| fzf preview/execute shell strings | shell-injection and subprocess explosion on hostile history text |
| token-stream partial JSON | nondeterministic contract and parsing complexity; typed progress suffices |
| fixed 60 FPS renderer | idle CPU waste; render on state changes/deadlines |
| unbounded event channels | input/model bursts become memory growth |
| color-only confidence/status | inaccessible and ambiguous |

## Risks deferred to the prototype

1. Suspending and restoring the TUI around fzf may differ across terminal emulators; CLI-only fzf is the safe fallback.
2. The full Ratatui/Crossterm artifact may break the size target; measure exact features before removing required behavior.
3. Blocking SQLite queries must remain bounded; otherwise the search worker needs connection interruption support, not more UI threads.
4. Unix suspend and process-group handling may require a small platform crate. Its value and size must be measured.
5. Shell-like command text includes tabs/newlines/NUL-like escaped data; the safe one-line/NUL protocol and ID-membership validation need fuzzing.
6. TUI snapshots do not prove terminal restoration; PTY tests are mandatory.
