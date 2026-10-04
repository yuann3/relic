# 0014. One committed gate harness measures every performance gate, and each gate has one owning slice

Status: Accepted · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

- benches/perf_spike.rs times each gate once (one `Instant` per import, query, unchanged check and incremental import) and computes no percentile, yet [Prove the ingestion and search performance envelope](https://github.com/yuann3/relic/issues/8) labels its figures "p95" (benches/perf_spike.rs:55-186).
- That ticket's 353 MiB RSS came from one process that generated and held several fixtures and opened four DBs in turn; a fresh-process RSS for one production import was never measured (benches/perf_spike.rs:24-276).
- Its incremental figure (0.41 ms) imported 100 records into an empty DB; its unchanged check (0.03 ms) was timed in-process after open, while the budget includes startup, config, SQLite open and stat (docs/research/storage-search-runtime-architecture.md:178).
- The lock ticket lists warm `--help` and TUI binary size as never measured. [Prove the interactive search workspace](https://github.com/yuann3/relic/issues/10) measured both on macOS (M4 Max): `--help` p95 1.7 ms headless, 1.95 ms full; stripped binary 2.2 MiB headless, 2.7 MiB full. No committed harness in tests/ or benches/ reproduces either.
- All performance work so far ran on macOS arm64. Never measured: cold FTS, Linux x86_64, the ureq/rustls delta, RSS with the four-child pool, import with production redaction, shell-hook and agent-trace parsers.
- The research protocol requires fresh-process samples on macOS arm64 and Linux x86_64, `/usr/bin/time -l`/`-v` for RSS, raw JSON with machine and toolchain metadata, and state bytes after checkpoint. It forbids claiming CLI startup from a hot in-process microbenchmark and calling a cache-flushed run "cold" without the method (docs/research/storage-search-runtime-architecture.md:294-305).

## Decision

- benches/perf_spike.rs is replaced by one committed gate harness:
  - a fresh process per sample;
  - N ≥ 20 samples per gate;
  - nearest-rank p95;
  - raw JSON output with machine metadata.
- Every gate result recorded against this contract comes from that harness.
- Correction to the lock ticket's list: warm `--help` (1.7 ms) and TUI binary size were measured on macOS, but no harness reproduces them.
- Gate ownership:
  - S1a (import): fresh-process peak RSS at 1M and 2M; 100 records appended to a 1M store, timed as a fresh process including open.
  - S1b (redaction): 100k and 1M import with production redaction; redaction binary delta; secure-delete cost.
  - S1c (read path): hot-term and query-budget bench; whether the interrupt lands mid-merge.
  - S2 (recall core): ureq/rustls stripped delta; the Ctrl-C proof; RSS with the four-child pool, reported but not gated, because the RSS budget is non-model.
  - S6 (release): cold FTS with the cache-flush method recorded; the whole matrix on Linux x86_64.
  - The DB size gate is restated ([0008](0008-gate-change-database-size.md)) once S1a and S1b write production fields.

## Consequences

Status in the tables below:
- **unreproduced**: a passing macOS arm64 number exists under the gate's conditions, but no committed harness reproduces it.
- **measured**: a number exists, but not under the gate's conditions or not on shipping code.
- **amended**: replaced by a gate-change ADR.
- **unmeasured**: no number exists.

The lock names no earlier owner for warm `--help`, the unchanged-source check or the full binary; S6's release run measures them.

### Map hard gates ([Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1))

| Gate | Status | Evidence so far | Owner | Measurement method |
|---|---|---|---|---|
| Warm `--help` p95 <15 ms | unreproduced | 1.7 ms headless, 1.95 ms full (workspace prototype) | S6 | Fresh-process `relic --help`, warm, N ≥ 20, nearest-rank p95; every dependency addition also reports startup ([0015](0015-dependency-envelope.md)) |
| Unchanged-source check p95 <20 ms | measured | 0.03 ms in-process after open, excluding startup and open (performance-envelope spike) | S6 | Fresh-process import of an unchanged source against a 1M store, including startup, config, SQLite open and stat ([0002](0002-source-identity-and-revisions.md)) |
| Warm FTS over 1M events p95 <20 ms | amended | 20.95 ms worst sent keystroke (`git tas*`); whole words 0.03–0.06 ms under rowid DESC (docs/research/interactive-fts-measurements.md:16-31) | S1c | [0007](0007-gate-change-warm-fts-hot-prefixes.md): whole-word/indexed-prefix keystrokes p95 <20 ms; unindexed hot-prefix and sparse-AND keystrokes end within the [0006](0006-search-ordering-and-query-budget.md) deadline plus epsilon; 1M fixture, warm cache |
| Import of 100k native records <1 s | measured | 242 ms with the spike redactor (performance-envelope spike) | S1b | Fresh-process `relic import` of 100k records with production redaction ([0003](0003-redaction-policy.md)) |
| Incremental import of 100 records <25 ms | measured | 0.41 ms into an empty DB (performance-envelope spike); no append path exists | S1a | Fresh process appending 100 records to a 1M store, timed including open; this gate overrides the source contract's full-reparse preference ([0002](0002-source-identity-and-revisions.md)) |
| Non-model peak RSS <64 MiB | measured | 353 MiB from a multi-fixture bench process (performance-envelope spike); TUI about 19 MB (workspace prototype); single import never measured | S1a | Fresh-process `/usr/bin/time -l` (macOS) / `-v` (Linux) on `relic import` at 1M and 2M; <64 MiB at both, no material growth between |
| Stripped headless binary, 8–12 MiB target | unreproduced | 2.2 MiB (workspace prototype), 2.5 MiB (performance-envelope spike), `--no-default-features` | S1b, S2, S6 | Stripped release build size; S1b reports the redaction delta, S2 the ureq/rustls delta, S6 Linux x86_64; every addition shows `cargo bloat` ([0015](0015-dependency-envelope.md)) |

### Storage acceptance-matrix gates ([Choose the storage, search, and runtime architecture](https://github.com/yuann3/relic/issues/4))

| Gate | Status | Evidence so far | Owner | Measurement method |
|---|---|---|---|---|
| Cold FTS over 1M p95 <100 ms | unmeasured | — | S6 | Harness run with the cache-flush method and privileges recorded |
| Import of 1M native records <10 s | measured | 2,734 ms with the spike redactor (performance-envelope spike) | S1b | Fresh-process `relic import` of 1M records with production redaction |
| Full TUI/provider binary, 12–18 MiB target | measured | 2.7 MiB full, without a provider (workspace prototype) | S2, S6 | Stripped release build with default features, after ureq lands |
| Sanitized DB + FTS <2x sanitized source bytes | amended | 3.04x raw source bytes on placeholder fields | S1a + S1b | [0008](0008-gate-change-database-size.md): DB + `-wal` + `-shm` after checkpoint on the 1M fixture over raw source bytes, production fields written |
| Zero redaction canary in state, log or mock egress | unmeasured | — | Every slice adding an egress path or stored field | Privacy canary gate ([0004](0004-egress-seam-and-privacy-canary.md)) |

### Gates set by the lock

| Gate | Status | Owner | Measurement method |
|---|---|---|---|
| Keyed BLAKE3 over the old prefix fits the incremental gate at 1M, else fall back to the keyed boundary guard | unmeasured (BLAKE2b: 38 ms / 55 MB) | S1a | Inside the incremental-import sample above ([0002](0002-source-identity-and-revisions.md)) |
| Query budget: 51-token hot phrase and 400×`git` rank return `TooBroad` or `Cancelled` within budget + epsilon | unmeasured | S1c | 1M-fixture bench ([0006](0006-search-ordering-and-query-budget.md)) |
| Secure-delete cost on a 1M store | unmeasured | S1b | Harness timing before `PRAGMA secure_delete=ON` ships ([0003](0003-redaction-policy.md)) |
| Ctrl-C during a root call and during a four-child batch exits 130 within 250 ms, `Cancelled` written, no late commits | unmeasured | S2 | Against a hanging mock server; failure switches the transport ([0010](0010-model-boundary.md)) |
| Peak RSS with the four-child pool | unmeasured; reported, not gated | S2 | `/usr/bin/time` during a recall against a mock server |

Moving the bench bins out of the shipped crate (so `tempfile` becomes dev-only) happens with the harness replacement, not as a drive-by ([0015](0015-dependency-envelope.md)).

## Rejected alternatives

- Require every gate before the lock: impossible for the ureq delta, worker RSS and shell-hook/agent-trace parsers, whose code does not exist; it blocks the lock on implementation.
- Keep perf_spike.rs: single-sample "p95", in-process startup claims, and a multi-fixture RSS figure that does not measure the gate.

## Open facts

- Every number in the tables above on Linux x86_64 (S6); the ns-per-posting cost model was fitted only on an Apple M4 Max.
- Whether FTS5's pending-index buffer stays bounded inside one transaction per revision at 1M and 2M (shows up in the S1a RSS run).
