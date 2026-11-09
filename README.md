# Relic

Relic remembers what you did in your terminal, so you can find it again and reuse it.

You've probably had this happen. A few months ago you fixed something nasty: a commit lost to `git reset --hard`, a build that wouldn't link, a certificate that wouldn't renew. Now it's happened again. You know you solved it once, but you can't remember how. The answer is somewhere in your shell history, your Git reflog and an old coding agent session, and none of those are easy to search, let alone together.

Relic pulls those traces into one store on your own machine. You can search them instantly, ask a model how you did something and get back an answer that points at the exact commands you ran, and turn a fix that worked into an Agent Skill that you or your coding agent can use next time.

## What it reads

- zsh and Bash history files
- an optional shell hook that records each command's exit status, directory and timing, never its output
- Git reflogs and refs, so a reset-away commit or a deleted branch stays findable after Git has forgotten it
- Claude Code sessions: the commands its Bash tool ran and what they printed, never your prompts or its replies

Secrets are redacted before anything is stored. Passwords, tokens and keys become markers like `[REDACTED:password]`, and the raw text never reaches the database, the search index, the logs or a model. If something slips through anyway, `relic forget <id>` deletes it for good, and it stays gone when you import again.

## Getting started

Relic is written in Rust and runs on macOS and Linux. There's no packaged release yet, so build it from source:

```sh
cargo install --path .
```

That includes the terminal UI. Add `--no-default-features` if you only want the command line.

Then import what you already have:

```sh
relic import ~/.zsh_history
relic import ~/src/shop        # a Git repository
relic import --claude          # your Claude Code sessions
```

To also capture exit status and directory from now on, add the hook to your shell, and import what it recorded whenever you like:

```sh
eval "$(relic hook zsh)"       # in ~/.zshrc; use bash at the end of ~/.bashrc
relic import --hooks
```

You can run any import again at any time. Only what's new gets added.

## Searching

```sh
relic search git reset
relic search cherry-pick --fzf
relic ui
```

Search runs entirely offline and is fast enough to update as you type. Results come most recently imported first, which is import order rather than when a command ran, or by relevance with `--rank`. `--json` gives you something to script against, `--fzf` lets you pick a result in fzf, and `relic ui` opens the terminal UI.

## Asking how you did something

Recall needs a model. Anything that speaks the OpenAI Chat Completions API works, including a local llama.cpp server. Put the settings in `~/.config/relic/env`:

```sh
RELIC_MODEL_NAME=your-model
RELIC_MODEL_API_KEY=your-key
# or, for a local server:
# RELIC_MODEL_PROFILE=llama_cpp
# RELIC_MODEL_URL=http://127.0.0.1:8080/v1
```

`relic model check` sends a small test request, never your history, to confirm it works. Then ask:

```sh
relic recall how did I get back the commit I lost in shop
```

Relic searches your history in small, bounded steps, has the model read only the records that look relevant, and answers with a procedure where every step cites the records it came from. If your history doesn't support an answer, it says so instead of guessing. Only redacted text leaves your machine, every request's size and destination is printed as it goes, and calls, bytes, tokens and cost are all capped.

## Turning a fix into a skill

When a recalled procedure works, you can keep it:

```sh
relic recall how did I recover the lost commit --json > report.json
relic validate report.json --cwd ~/src/shop -- git rev-parse --verify rescue
relic skill compile report.json --name recover-lost-commit \
  --description "Recover a commit lost to git reset --hard"
relic skill activate recover-lost-commit --user --claude
```

`validate` runs a check you write yourself and records whether it passed. Relic never runs the procedure on your behalf. A skill can only be compiled from a report that passed, and it comes out as a plain Agent Skill, a single `SKILL.md`, linked into `~/.agents/skills` (and `.claude/skills` with `--claude`) where agents that read skills will find it. `relic skill rollback` undoes an activation. The next time you ask Relic a similar question, recall starts from the skill instead of digging through everything again.

## Where it's at

The whole loop works end to end: import, search, cited recall, validation, skills, and reuse on a second, similar problem. The privacy rules and the performance targets are checked by the test suite. It's still early, though. Nothing is tagged or packaged yet, and the store format may change between versions (Relic upgrades an older store the next time you import).

Things on the list next:

- **Packaging.** Release builds, an easy install, and CI on Linux as well as macOS.
- **An honest benchmark.** There's a set of twelve seeded investigations in `tests/investigations.rs` that compares bounded recall with plain search and simpler pipelines. It hasn't had a clean run against a real model yet, so there's no claim yet that recall beats just searching.
- **More sources.** Traces from other coding agents, not only Claude Code.
- **Looking after your data.** A proper lifecycle for the shell hooks, and recovery for a damaged store that keeps the old file instead of deleting it.

If you want to dig into how it works, [CONTEXT.md](CONTEXT.md) maps the code and its vocabulary, and [docs/adr/](docs/adr/) has the design decisions and why they were made.
