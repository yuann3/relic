# 0011. Relic restores the terminal on every exit path and follows grep's exit-code convention

Status: Accepted · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

[[D1] Fix the e2e dogfood findings from the workspace prototype](https://github.com/yuann3/relic/issues/22) and the WIP `fix/tui` / `fix/cli` branches surfaced these terminal and CLI defects:

- **Fatal signals.** Master installs a panic hook but no signal handlers. A signal sent to relic's pid alone (`kill`, `timeout`, a supervisor) leaves the tty raw on the alternate screen and orphans fzf. A real tty hangup already kills both, because they share the foreground group (`tests/pty.rs:258-300`; `src/picker.rs:108`, `:159-160`). The panic path's `emergency_restore` is already async-signal-safe (`src/ui/term.rs:19-32`, `:165-207`). libc is a direct dependency. The research asks for children to be killed as a process group (`docs/research/tui-fzf-integration.md:254`).
- **Ctrl-Z.** Arrives as `Key::Ctrl('z')` and is a no-op (`src/ui/term.rs:375`; `src/ui/model.rs:515`). The research allows "suspend unsupported" only if a signal crate breaks the size/startup gates (`tui-fzf-integration.md:258`). signal-hook and libc are already in the build, so that precondition fails. The fzf hand-off already proves the pause-reader → restore → resume → redraw cycle in PTY tests.
- **Esc+letter.** crossterm 0.29 parses ESC plus the next byte in the same `read()` as Alt+key (`crossterm-0.29.0/src/event/sys/unix/parse.rs:78-88`; [crossterm: escape sequence parsing should not depend on read() syscall response size](https://github.com/crossterm-rs/crossterm/issues/993)). Relic drops every Alt key (`src/ui/term.rs:370-373`), so both keystrokes are lost. ESC followed by `[` or `O` is taken as a CSI/SS3 introducer inside crossterm. Relic binds no Alt chords.
- **2 s `clear()` hang.** ratatui-core 0.1.2's `Terminal::clear()` queries the cursor position even for a fullscreen viewport (`buffers.rs:147-151`). crossterm then waits 2000 ms and errors. Relic calls `terminal.clear()?` at startup and after fzf (`src/ui/mod.rs:67`, `:247`), so on a non-answering terminal the TUI stalls 2 s and then exits with an error. Upstream issue: [ratatui: Inline viewport construction and Terminal::clear still issue CPR queries](https://github.com/ratatui/ratatui/issues/2640). [ratatui: avoid cursor position queries during resize](https://github.com/ratatui/ratatui/pull/2485) made `resize()` query-free.
- **crossterm reader bugs.** These are kept as ignored PTY tests (`tests/pty.rs:190`, `:245`):
  - [crossterm: Unix Mio source loses remaining readiness tokens after returning Resize](https://github.com/crossterm-rs/crossterm/issues/1126): keys typed during a resize storm stall until the next keystroke.
  - [crossterm: Orphaned crossterm process consumes 100% CPU](https://github.com/crossterm-rs/crossterm/issues/793): after a hangup with SIGHUP ignored, the reader spins at 100% CPU.

  Neither corrupts data or leaves the tty raw.
- **Exit codes.** Master returns 1 for both no-match and runtime errors, including `--fzf needs an interactive terminal` and a query whose only word is too short. It collapses fzf no-match (1) and cancel (130) into 130 (`src/main.rs:10-27`; `src/picker.rs:138-142`). Scripts cannot tell "nothing found" from "could not look".

## Decision

- **Fatal signals (TERM, INT, HUP, QUIT).** Applies in the TUI and in `search --fzf`. A libc `sigaction` handler:
  1. stops fzf: SIGTERM, a short `waitpid` grace, then SIGKILL;
  2. runs the existing async-signal-safe emergency restore;
  3. re-raises with `SIG_DFL`, so the exit status is 128+n.

  An inherited `SIG_IGN` (e.g. `nohup`) is honoured, not overridden. fzf is signalled by pid, not by process group, because it must stay in the foreground group.
- **Esc+letter.** Split Alt+key into Esc followed by the key. Relic binds no Alt chords. Esc followed by `[` or `O` in one read stays lost inside crossterm (accepted). Comment on the upstream escape-parsing issue.
- **`clear()` hang.** Replace `terminal.clear()` with `terminal.resize(full rect)`, which is query-free. Add a PTY test with a non-answering terminal. Comment on the upstream `Terminal::clear` issue.
- **Ctrl-Z.** Key-driven suspend:
  1. pause the reader;
  2. restore the terminal;
  3. `raise(SIGTSTP)` with `SIG_DFL`;
  4. on `fg`, resume and do a full redraw.

  It reuses the fzf hand-off path, with a PTY test for suspend + `fg`. An external SIGTSTP keeps its default disposition (deferred).
- **Exit codes.** Grep convention, identical for text and `--json`:

  | Code | Meaning |
  |---|---|
  | 0 | match / success |
  | 1 | no match, including an empty `--json` page and fzf no-match |
  | 2 | any error, including usage errors and a busy store (typed `busy` message); a query with nothing searchable is a usage error |
  | 130 | picker cancelled |
  | 128+n | killed by signal n |

  fzf's 1 and 130 stay distinct. The contract is documented in `USAGE` (`src/cli.rs`) and pinned by CLI tests.
- **crossterm reader bugs.** Accepted for v0.1. Keep the ignored PTY tests linked to the resize-stall and hangup-spin upstream issues, and comment on both. Revisit a `[patch]` fork only if upstream has not moved by release.

## Amends

[Choose the TUI and fzf integration architecture](https://github.com/yuann3/relic/issues/7) research:

- `tui-fzf-integration.md:254`, "killed as a process group": fzf is killed by pid. This is an accepted deviation, because fzf must stay in the foreground group.
- `tui-fzf-integration.md:258`, suspend on Ctrl-Z/SIGTSTP: narrowed to key-driven Ctrl-Z. An external SIGTSTP keeps its default disposition (deferred). The "suspend unsupported" option is closed, because its precondition is not met.

## Consequences

- Owner: [[D1] Fix the e2e dogfood findings from the workspace prototype](https://github.com/yuann3/relic/issues/22), narrowed to this terminal and exit-code work plus its remaining plain bugs. It runs in parallel from S0.
- Required proofs:
  - **Signals:** PTY tests show a fatal signal sent to relic's pid, in the TUI and in `search --fzf`, leaves a cooked tty, no fzf process, and exit 128+n. Under an inherited `SIG_IGN` the signal stays ignored.
  - **`clear()` hang:** a PTY test with a non-answering terminal shows no 2 s stall and no error exit.
  - **Ctrl-Z:** a PTY test covers suspend + `fg` with a full redraw.
  - **Exit codes:** a CLI test pins each code, including empty `--json`, fzf 1 vs 130, a busy store, and a query with nothing searchable.
- The busy store's exit 2 relies on the typed Busy of [0005](0005-evidence-store-transactions-and-read-only-open.md). Recall's Ctrl-C exit 130 ([0010](0010-model-boundary.md)) equals 128+SIGINT.
- Under `set -e`, an empty JSON result fails, so JSON callers must handle exit 1.
- A genuine Alt+letter now acts as Esc (which may quit) followed by the letter.
- Release (S6) checks upstream movement on the two reader bugs before deciding on any fork.

## Rejected alternatives

- **signal-hook iterator thread → normal restore.** Unbounded latency when the main thread is blocked, so it still needs a fallback. It would also add crossterm to headless `search --fzf`.
- **fzf in its own process group; restore on Drop only.** Does not restore the terminal on a fatal signal. It also needs `tcsetpgrp`, or fzf's tty I/O gets SIGTTIN/SIGTTOU.
- **Ctrl-Z unsupported.** Breaks a universal Unix expectation, and the research precondition for it is not met.
- **Full SIGTSTP/SIGCONT handlers.** A handler state machine for the rare external-stop case.
- **Keep dropping Alt keys.** Loses both keystrokes.
- **Kitty keyboard protocol (`DISAMBIGUATE_ESCAPE_CODES`).** Must be popped on every exit path, and querying support reintroduces a cursor-query timeout. Not needed while Relic binds no Alt chords.
- **Patch crossterm with an Esc timeout.** A fork to maintain.
- **Wait for the upstream ratatui fix.** Timing is outside our control.
- **Vendor or patch ratatui-core.** A maintenance burden for a two-line behaviour difference.
- **`[patch]` crossterm fork for the reader bugs now.** A standing vendored edit for a cosmetic stall and a SIGHUP-ignored-only spin.
- **crossterm `use-dev-tty`.** Still busy-loops on hangup, and adds dependencies with an unmeasured size.
- **Relic-side hangup watchdog thread.** Another thread and wakeup source for the nohup case only.
- **0/1 exit codes only.** Scripts cannot tell "nothing found" from "could not look".
- **sysexits-style granular codes.** Unfamiliar, and a larger contract to version.
- **`--json` always exits 0.** Text and JSON modes would disagree.
- **Nothing-searchable query exits 1.** It is a usage error, not an absence of matches.

## Open facts

- Whether zsh and bash restore a cooked tty when an external SIGTSTP stops relic in raw mode and `fg` resumes it. This decides whether the deferred external-stop case needs work after v0.1.
