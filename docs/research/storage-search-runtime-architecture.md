# Storage, search, and runtime architecture for Relic v0.1

**Status:** recommended architecture pending the performance spike  
**Decision scope:** local process, SQLite/FTS, import/redaction hot path, concurrency, Git, dependencies, durability, and measurement  
**Research date:** 2026-08-16

## Decision

Relic v0.1 should be a **single synchronous Rust process per invocation** with one local SQLite database and no daemon or async runtime. The hot local path is:

```text
approved source bytes
 -> BufReader + byte parser
 -> compiled redaction pipeline
 -> prepared SQLite statements in one batched transaction
 -> external-content FTS5 index

query
 -> bounded FTS5 rowids/rank
 -> metadata joins/projections
 -> optional fzf/model/TUI only after narrowing
```

Use `rusqlite` with a known bundled SQLite/FTS5 build for release artifacts, compact integer row IDs, externally stored sanitized text indexed by FTS5, bounded read-only Git subprocesses, and ordinary threads only for the model's depth-one child calls. Do not add Tokio, an ORM, async SQL, `git2`, embeddings, a vector store, a scripting VM, a plugin system, or a daemon.

This is a design hypothesis until [Prove the ingestion and search performance envelope](https://github.com/yuann3/relic/issues/8) measures it. No latency or throughput below is presented as already achieved.

## Why one synchronous process

Relic's common operations are short, local, and serial at their authority boundary:

- native history import has one SQLite writer;
- SQLite search is a blocking local call;
- source metadata checks are blocking filesystem calls;
- Git is a bounded subprocess after evidence narrowing;
- model network latency dwarfs an executor choice;
- depth-one semantic batches need at most a few concurrent calls.

Tokio would add feature/dependency/binary/startup surface without removing SQLite's single-writer constraint. Use a main thread for CLI/import/query and at most 2–4 scoped ordinary workers for independent child model calls. The TUI can use a blocking terminal event thread plus bounded channels; it does not justify making the domain/storage core async.

A daemon would improve repeated connection startup but violate the simple local product boundary, complicate hook installation, crash recovery, migration, authentication, and idle footprint, and make ordinary `search` dependent on service health. The `<20 ms` warm search/no-change targets must first be attempted without it.

## SQLite and FTS facts that shape the design

SQLite's FTS5 external-content tables obtain original column values from a separate content table using `content=` and `content_rowid=`. The application must keep the content and FTS index consistent; SQLite documents triggers as one option and the `rebuild` command for repairing an inconsistent index ([FTS5 external-content tables](https://www.sqlite.org/fts5.html#external_content_and_contentless_tables), [FTS5 rebuild/integrity commands](https://www.sqlite.org/fts5.html#the_rebuild_command)). Relic controls every write, so it should update base rows and FTS rows explicitly in the same transaction rather than hiding behavior in triggers.

WAL mode permits readers and a writer to proceed concurrently, but there is still only one writer. Readers see a consistent snapshot. WAL requires same-host shared memory and is not suitable for a network filesystem ([SQLite WAL](https://www.sqlite.org/wal.html)). Relic's state directory is local-only and must reject/document unsupported network locations.

SQLite documents that `synchronous=NORMAL` in WAL mode preserves consistency but a committed transaction may roll back after power loss, while `FULL` adds durability synchronization ([SQLite `synchronous`](https://www.sqlite.org/pragma.html#pragma_synchronous)). The observation/FTS index is rebuildable from approved sources, so `NORMAL` is the default import/query trade-off. Content-addressed skill assets must be fsynced before catalog pointer transactions; the catalog prototype must decide whether those rare pointer mutations temporarily require `FULL` or a separate durable metadata path.

`PRAGMA mmap_size` and page cache tuning are workload/platform decisions, not universal optimizations. Leave mmap disabled/default until macOS/Linux measurements demonstrate lower latency without violating RSS. Configure a bounded negative `cache_size` only after measuring actual process RSS ([SQLite pragmas](https://www.sqlite.org/pragma.html)).

`rusqlite` is a thin, synchronous SQLite binding and exposes prepared statements, transactions, backup/integrity facilities, and optional bundled SQLite builds ([rusqlite documentation](https://docs.rs/rusqlite/latest/rusqlite/), [rusqlite source/features](https://github.com/rusqlite/rusqlite)). Release builds should pin and bundle a SQLite version/compile configuration proven to include FTS5 so behavior does not vary with the host's system SQLite. The prototype must record the resulting binary-size cost rather than assuming it fits.

## Schema direction

This is a schema shape for the spike, not final naming:

```sql
CREATE TABLE schema_meta (
  key   TEXT PRIMARY KEY,
  value BLOB NOT NULL
) WITHOUT ROWID;

CREATE TABLE source (
  id                  INTEGER PRIMARY KEY,
  logical_id          BLOB NOT NULL UNIQUE, -- keyed stable ID
  kind                INTEGER NOT NULL,
  private_locator     BLOB NOT NULL,        -- protected/local, never model-visible
  parser_contract     TEXT NOT NULL,
  enabled             INTEGER NOT NULL CHECK (enabled IN (0,1))
);

CREATE TABLE source_revision (
  id                  INTEGER PRIMARY KEY,
  source_id           INTEGER NOT NULL REFERENCES source(id),
  revision_key        BLOB NOT NULL UNIQUE,
  file_identity       BLOB,
  opened_len          INTEGER NOT NULL,
  stable_len          INTEGER NOT NULL,
  boundary_guard      BLOB NOT NULL,
  snapshot_fingerprint BLOB NOT NULL,
  parser_contract     TEXT NOT NULL,
  redaction_revision  TEXT NOT NULL,
  imported_at_ms      INTEGER NOT NULL,
  complete            INTEGER NOT NULL CHECK (complete IN (0,1))
);

CREATE TABLE observation (
  id                  INTEGER PRIMARY KEY,
  revision_id         INTEGER NOT NULL REFERENCES source_revision(id),
  ordinal             INTEGER NOT NULL,
  kind                INTEGER NOT NULL,
  observed_at_ms      INTEGER,
  safe_text           TEXT NOT NULL,
  safe_hash           BLOB NOT NULL,
  keyed_fingerprint   BLOB NOT NULL,
  flags               INTEGER NOT NULL DEFAULT 0,
  provenance          BLOB NOT NULL,
  locator              BLOB NOT NULL,
  UNIQUE(revision_id, ordinal)
);

CREATE VIRTUAL TABLE observation_fts USING fts5(
  safe_text,
  content='observation',
  content_rowid='id',
  tokenize='unicode61 remove_diacritics 2'
);

CREATE INDEX observation_time ON observation(observed_at_ms, id);
CREATE INDEX observation_revision ON observation(revision_id, ordinal);
```

Keep the hot table narrow. Store source-specific typed facts that are queried often—cwd/repo, exit status, session, tool kind—in compact nullable columns or narrow side tables after query evidence proves the need. Do not encode everything into one unqueryable JSON blob, but do not pre-normalize dozens of speculative tables either. `provenance` and `locator` should use a versioned compact representation; JSON is acceptable for the first correctness spike, with size measured.

The FTS external-content configuration stores sanitized text once in the `observation` table; FTS stores its token index, not another authoritative body. Import explicitly executes base-row and matching FTS insert/delete operations inside the same transaction. Tests run FTS integrity/rebuild commands after injected crashes and mutations.

Search uses a bound and deterministic tie-break:

```sql
SELECT o.id, o.observed_at_ms, o.kind, o.safe_text, bm25(observation_fts) AS score
FROM observation_fts
JOIN observation AS o ON o.id = observation_fts.rowid
WHERE observation_fts MATCH ?1
ORDER BY score, o.id DESC
LIMIT ?2;
```

FTS query syntax is not accepted raw from untrusted model/user text. A query compiler escapes literals and permits a tiny explicitly parsed filter language. Model-generated plans use typed search terms/filters, not SQL or FTS syntax.

## Import and redaction hot path

### Byte pipeline

- Open only approved regular files, without following the final symlink by default.
- Capture identity/length, stream with `BufReader`, and `fstat` after reading.
- Scan boundaries with `memchr`/small byte state machines. Do not call `.lines()` and do not load the whole history into `Vec<String>`.
- Bound record length, warning count, nesting, JSON line length, and total imported bytes.
- Parse to an ephemeral borrowed/raw record where possible.
- Redact before allocating/persisting final safe text.
- Reuse prepared statements; never prepare or commit per row.
- Commit a complete source revision, observations, and FTS changes atomically.

### Redaction

Compile matchers once per process/config revision:

```text
cheap structural/literal/prefix screen
 -> Aho-Corasick candidate spans
 -> bounded contextual regex-automata checks
 -> bounded entropy/token heuristics on candidates only
 -> typed replacement markers
```

`aho-corasick` and `regex-automata` expose finite-automata-based matching and configurable construction/search behavior ([Aho-Corasick crate](https://docs.rs/aho-corasick/latest/aho_corasick/), [regex-automata crate](https://docs.rs/regex-automata/latest/regex_automata/)). The prototype must compare their feature/binary costs and may start with a smaller hand-written detector set if the general regex machinery breaks the binary gate. Never run unbounded pathological backtracking regex over every byte.

Raw buffers do not enter SQLite, tracing, benchmark error output, caches, or model fixtures. Security tests place unique canaries in every supported field and search the complete state directory and captured mock transport after import.

### Transactions and batches

A single transaction for 100k–1M rows provides atomic revision visibility and avoids per-commit fsync. Prepared statement reuse is mandatory. `5k`, `10k`, and `20k` are **measurement batch points**, not decisions; if one full transaction with reused statements is fastest and memory-bounded, use it. If periodic savepoints improve memory/failure diagnostics, they must not expose partial revisions.

Import remains single-writer. A parser producer/SQLite consumer pipeline is added only if profiling proves parsing/redaction and writes overlap beneficially without exceeding RSS; the baseline is deliberately one thread to minimize copying and queue memory.

## Incremental and no-change paths

The source contract owns correctness. Runtime optimization follows it:

1. read the small configured source list;
2. `stat` only those paths;
3. compare identity/length/mtime hints and the prior stable-boundary guard;
4. if unchanged, do no parser, Git, provider, TUI, or FTS initialization;
5. for a proven clean append, parse from the last complete boundary;
6. otherwise stream a new revision/full reparse.

The `<20 ms` no-change budget includes process startup, config parsing, SQLite open, statements, and source stats. `--help` and `--version` must return before config, database, source, Git, TUI, or provider construction.

Do not optimize with a second sidecar authority. SQLite remains the cursor/index truth. If opening/configuring SQLite cannot meet the gate, measure and fix initialization/features before inventing cache invalidation.

## Pragmas to prototype

Apply/verify on connection initialization:

```sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;       -- initialize once and verify returned mode
PRAGMA synchronous = NORMAL;     -- rebuildable index default
PRAGMA temp_store = MEMORY;
PRAGMA busy_timeout = 250;
```

Prototype candidates, not defaults:

```text
cache_size: 8, 16, 32 MiB effective caps
mmap_size: 0 vs bounded platform values
wal_autocheckpoint/checkpoint policy after bulk import
journal_size_limit
optimize cadence and analysis_limit
```

Bulk import should not flip undocumented “fast” pragmas that risk corruption. Measure WAL/checkpoint tail latency and total bytes. Keep write transactions short enough for another invocation to receive a typed `Busy` quickly rather than hanging; serialize within one process and rely on SQLite locking across processes.

## Git boundary

Use the installed `git` executable with fixed argv vectors, `--no-optional-locks`, an explicit `-C <approved path>`, bounded output, timeout, sanitized environment, and no shell. Candidate operations include `rev-parse --show-toplevel`, `rev-parse --verify`, `reflog` formatting, and bounded `log/show/diff` after source evidence selects a repository/object. Git's official commands define the semantics ([Git `rev-parse`](https://git-scm.com/docs/git-rev-parse), [Git reflog](https://git-scm.com/docs/git-reflog), [Git log](https://git-scm.com/docs/git-log)).

Do not run Git during every native import or FTS query. Do not link `libgit2`: it adds a large build/security/configuration surface and can differ from the user's installed Git behavior. No candidate/model supplies an option beginning with `-`; typed arguments are inserted after `--` where supported.

## Process and module shape

One Cargo package, library core, one CLI binary, optional compile-time TUI feature:

```text
src/
  lib.rs                    domain types; no CLI/TUI/provider initialization
  bin/relic.rs              argument parse and composition root
  source/                   byte readers, revisions, importer traits
    zsh.rs bash.rs hook.rs prime.rs claude.rs
  redact/                   compiled policy and safe-text boundary
  store/                    rusqlite schema, migrations, import/query/catalog txns
  search/                   typed query -> escaped FTS/filter plan
  evidence/                 handles, provenance, citation resolver
  git/                      bounded fixed-argv subprocess adapter
  rlm/                      typed PlanV1 coordinator
  model/                    blocking HTTP + optional Codex exec adapters
  validate/                 explicit workspace/probe boundary
  compile/                  Agent Skill bundle and local catalog
  app/                      command/use-case API and versioned events
  ui/                       feature-gated Ratatui renderer/update loop
```

Modules communicate with typed rows/handles and bounded iterators, not copied transcript strings. Avoid public trait abstractions until two implementations require them; source adapters and model adapters are the two real polymorphic seams.

## Dependency envelope

Core candidates, all pinned and feature-audited:

| Need | Candidate | Constraint |
|---|---|---|
| SQLite/FTS | `rusqlite` + bundled SQLite | required; measure bundled binary/RSS; only needed features |
| serialization | `serde`, `serde_json` | JSONL/model/wire; avoid arbitrary `Value` in hot domain |
| config | `toml` or a small explicit parser | initialize only for commands needing config |
| hashing | `blake3` | keyed private identity and public safe hashes |
| byte scan | `memchr` | hot parser boundaries |
| redaction | `aho-corasick`; evaluate `regex-automata` | compile once; feature/binary gate |
| IDs/random | `getrandom` plus narrow typed wrappers | avoid UUID text in hot tables |
| CLI | compare `lexopt`/small parser with minimal `clap` features | startup/binary decision belongs to spike |
| HTTP | `ureq` with minimal TLS/features | blocking and only initialized for model commands |
| errors | `thiserror` or manual enums | compile-time convenience; no generic anyhow domain errors |
| TUI | optional `ratatui`, `crossterm` | separate feature/artifact; no core dependency |

Development-only: property/fuzz tools, `criterion` if useful, `cargo-bloat`, `cargo-deny`, fixture generators. No benchmark framework enters the runtime dependency graph.

Before accepting a dependency, record:

```console
cargo tree -e features
cargo tree --duplicates
cargo bloat --release --crates
```

Compare stripped binary bytes and warm/cold startup before/after. A dependency that saves a small amount of source code but initializes global state or adds megabytes without measured value is rejected.

## Migrations, durability, and recovery

- Embed numbered forward-only SQL migrations and run each under an exclusive transaction; record `user_version` plus Relic schema metadata.
- Back up/restore the small durable catalog metadata before an irreversible migration. The observation index can be rebuilt from approved sources but active compiled skills may not be reconstructible from mutable history.
- CAS skill payload: stream to same-filesystem temp, hash, fsync, no-replace rename/install, fsync directory, then commit catalog pointer.
- Validate SQLite quick/integrity checks through `relic doctor`; run FTS integrity/rebuild independently.
- If an observation DB is corrupt, quarantine it and rebuild from sources rather than silently returning partial evidence.
- Never silently delete an evidence revision referenced by an investigation/skill. GC comes after real retention semantics.
- Bound WAL growth and checkpoint after large successful imports when no reader snapshot blocks it; expose diagnostic bytes/timing.

## Benchmark and proof plan

### Reproducible fixtures

Generate deterministic 1k/100k/1M corpora with fixed seed and known expected event hashes:

- mixed zsh plain/extended/metafied/multiline/malformed/tied timestamps;
- Bash timestamped and ambiguous unmarked multiline;
- clean appends, incomplete tails, same-length rewrites, truncation, inode rotation;
- hook frames and Prime/Claude supported/unknown JSONL variants;
- duplicate commands and realistic command-length/token distributions;
- secrets spanning early/late/chunk boundaries;
- FTS terms with common/rare/prefix/phrase/Unicode/adversarial syntax;
- Git correlations only for bounded selected subsets.

Fixture generation time is excluded. Validate exact expected observation/citation hashes before timing.

### Measurement protocol

- macOS arm64 and Linux x86_64 release artifacts, portable CPU settings;
- fresh process per CLI sample;
- at least warm p50/p95 and explicitly labeled cold filesystem-cache runs;
- `/usr/bin/time -l` on macOS and `/usr/bin/time -v` on Linux for max RSS;
- `hyperfine` or a checked-in equivalent command harness with preparation commands, raw JSON results, machine/toolchain metadata;
- SQLite DB/WAL/SHM and total state bytes measured after checkpoint;
- stripped binary bytes for headless and TUI/provider builds;
- parser, redaction, SQLite base insert, FTS insert, checkpoint, and query spans measured separately with an in-process development counter, disabled from normal output.

Do not use a hot in-process microbenchmark to claim CLI startup. Do not call a filesystem-cache-flushed run “cold” without recording the actual method and privileges.

### Acceptance matrix

| Gate | Required result |
|---|---:|
| `relic --help` warm fresh process | p95 `<15 ms` |
| unchanged approved-source check | p95 `<20 ms` |
| FTS query, 1M events | warm p95 `<20 ms`; cold p95 `<100 ms` |
| import 100k mixed native records | p95 `<1 s` |
| import 1M mixed native records | p95 `<10 s` |
| incremental 100 complete records | p95 `<25 ms` |
| import/non-model recall max RSS | `<64 MiB` |
| headless stripped artifact | target `8–12 MiB`; failure requires dependency evidence/decision |
| full TUI/provider artifact | target `12–18 MiB` |
| sanitized DB + FTS after checkpoint | `<2x` sanitized source bytes |
| correctness/privacy | zero expected-hash mismatch; zero redaction canary in state/log/mock egress |

Performance gates never override correctness/privacy. A failing architecture is changed or the destination is explicitly renegotiated; numbers are not silently relaxed.

## Rejected alternatives

| Alternative | Reason for v0.1 rejection |
|---|---|
| Tokio/async core | no asynchronous local bottleneck; cost/complexity before evidence |
| daemon/index service | service lifecycle and idle footprint; one-shot gates must be attempted first |
| SQLx/ORM/pool | compile/runtime surface and abstraction over one local connection |
| Tantivy/another index | duplicates SQLite metadata/transaction boundary; FTS5 is sufficient hypothesis to test |
| embeddings/vector DB | privacy, model/download/index cost, weak deterministic evidence; not needed for lexical narrowing + RLM |
| `git2`/libgit2 | large native surface and behavioral/config divergence from installed Git |
| Rhai/Python/JS/WASM | arbitrary/general execution and dependency cost; typed PlanV1 suffices |
| raw history mirror | violates privacy/storage boundary and doubles authority |
| per-row commit | fsync/WAL amplification and partial revision visibility |
| unbounded producer/consumer queues | RSS and cancellation failure |
| mmap/cache “tuning” by default | platform/workload-dependent; benchmark first |
| `opt-level=z` as universal release | may reduce code size while damaging parser/hash/FTS throughput; compare against `opt-level=2` |

## Risks and prototype decisions still open

1. The bundled SQLite and TLS/TUI features may exceed binary targets; measure each feature increment and consider two official artifacts, not runtime compromise.
2. `<1 s` for 100k with strong redaction may require matcher/schema changes; profile stages before concurrency.
3. External-content FTS consistency is application-owned; crash/fault tests and rebuild checks are mandatory.
4. `unicode61` tokenization may not match shell punctuation well. Prototype an additional sanitized token column or tokenizer options before writing a custom tokenizer.
5. FTS `bm25()` ranking over broad common terms may dominate latency; enforce result/query bounds and measure realistic distributions.
6. SQLite open/config/schema checks may threaten `<20 ms`; cache prepared SQL in-process only where useful and eliminate unnecessary startup work before considering a daemon.
7. The source identity/guard strategy may cause safe full reparses; correctness wins, but fixture frequency determines user impact.
8. Codex subprocess/TUI work must not enter headless local paths through eager construction or broad feature defaults.
