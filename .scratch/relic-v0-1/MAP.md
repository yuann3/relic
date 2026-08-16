# Relic v0.1: ship the evidence-backed terminal experience compiler

Status: open  
Labels: wayfinder:map

## Destination

Ship a working Relic v0.1 for macOS and Linux: import local experience artifacts, recall an evidence-cited procedure through bounded RLM traversal, explicitly validate it, compile and manually activate an Agent Skill, and prove faster held-out second-run reuse while meeting the hard performance and privacy gates.

## Notes

- This map explicitly carries execution through the MVP. It does not stop at a specification or implementation plan.
- Use primary-source research and targeted prototypes before locking hard-to-reverse architecture.
- Consult `research`, `prototype`, `grilling`, `domain-modeling`, `code-quality-guideline`, and `tdd` as applicable.
- Required sources: native zsh/Bash history, zsh/Bash metadata hooks, local Git, Prime Agent traces, and Claude Code traces.
- Local indexing/search/evidence inspection must work offline. RLM recall uses a configured model.
- Default model boundary: a small OpenAI-compatible HTTP interface. Official Codex/ChatGPT subscription support is desirable but non-blocking and must not rely on private or copied credentials.
- Search uses SQLite FTS5 for corpus narrowing and optional external `fzf` for bounded interactive ranking, with a built-in fallback.
- CLI and focused Ratatui TUI are both MVP surfaces; all important operations remain scriptable with versioned JSON.
- Redact before SQLite, FTS, logs, caches, and model egress. Never persist raw source artifacts.
- Hooks capture metadata only: command, timestamp, cwd, session identity, duration, and exit status—not output, terminal input, environment values, or keystrokes.
- Validation never automatically reruns reconstructed commands. Only explicit user-approved probes/verifiers may run, and a workspace is not called a sandbox.
- Relic owns a minimal content-addressed local skill catalog with explicit activation and rollback. Generic promotion governance remains outside Relic.
- Hard local performance gates: `<15 ms` warm help, `<20 ms` unchanged-source check, `<20 ms` warm FTS over 1M events, `<1 s` import of 100k native records, `<25 ms` incremental import of 100 records, `<64 MiB` non-model peak RSS, and an `8–12 MiB` stripped headless binary target. Model latency is reported separately.

## Decisions so far

- [Define the Relic v0.1 destination](issues/00-define-relic-v0-1-destination.md) — The MVP includes execution through cited recall, explicit validation, skill compilation/activation, and measured held-out reuse; harnessd remains separate.
- [Specify the bounded RLM and model boundary](issues/03-specify-rlm-and-model-boundary.md) — Use a seven-operator, depth-one typed RLM kernel with scoped evidence handles, hierarchical budgets, deterministic abstention, and one blocking capability-negotiated Chat Completions adapter.
- [Determine whether Codex subscription authentication is officially usable](issues/04-determine-codex-subscription-authentication.md) — Optionally delegate to stable installed `codex login`/`codex exec`; never own OAuth or credentials, and keep separately billed OpenAI-compatible HTTP as the fallback.

## Not yet specified

- The final bounded contexts, ubiquitous language, schemas, and ADR set after source/runtime/RLM research resolves.
- The exact vertical implementation slices for source ingestion, the evidence store, RLM recall, validation, skill compilation/catalog, CLI, and TUI; these graduate only after architecture decisions and performance prototypes expose the right seams.
- Release packaging, installation, hook lifecycle, migrations, corruption recovery, observability, and cross-platform CI details.
- The exact end-to-end acceptance fixture and held-out sibling scenario.
- Security/adversarial test matrices beyond the standing privacy and execution boundaries.

## Out of scope

- `harnessd`, candidate governance, canaries, leases, automatic promotion, and multi-consumer control-plane behavior; these belong to a separate project.
- A daemon, cloud sync, team accounts, remote registry, HTTP server, marketplace, or distributed service.
- Embeddings/vector search, a scripting VM, generic plugin ABI, provider framework, or bundled model.
- Atuin, asciinema, Codex trace ingestion, Cursor, Gemini CLI, Goose, Aider, fish, PowerShell, Windows, and WSL-specific behavior in v0.1.
- Capturing terminal output/input or silently executing recovered procedures.
