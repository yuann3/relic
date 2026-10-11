# 0008. Amend the database-size gate: shrink the store, then restate the gate as the measured ratio to raw source bytes

Status: Accepted · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

- The 1M diverse fixture (53.3 MiB source) produces a 162.0 MiB store after `VACUUM`: 3.04x raw source bytes, about 170 B per observation. Breakdown: `observation` table 87.4 MiB (1.64x), secondary indexes 42.9 MiB (0.80x, of which `observation_time` 18.1), FTS 31.7 MiB (0.59x) (docs/research/interactive-fts-measurements.md:116-131).
- `<2x` is unreachable with full-detail FTS: the table plus FTS alone is 2.23x. `detail=none` is the smallest variant (2.78x) and `detail=column` saves nothing (3.04x). Both reject the phrase queries Relic generates for `src/main.rs` and `task-12*`, and `detail=none` slows prefix merges (`c*` 31 ms) (docs/research/interactive-fts-measurements.md:124-125, 166, 861-862).
- The `<2x` gate comes from the acceptance matrix of [Choose the storage, search, and runtime architecture](https://github.com/yuann3/relic/issues/4) ("sanitized DB + FTS after checkpoint `<2x` sanitized source bytes", docs/research/storage-search-runtime-architecture.md:320), not from the map's hard-gate list. [Prove the ingestion and search performance envelope](https://github.com/yuann3/relic/issues/8) and [Prove the interactive search workspace](https://github.com/yuann3/relic/issues/10) measured against raw history-file bytes; against sanitized bytes every ratio is about 1.4x higher (3.04x raw ≈ 4.3x sanitized) (docs/research/interactive-fts-measurements.md:308).
- The measured baseline holds placeholder fields: `safe_hash` and `keyed_fingerprint` are 8-byte FNV values and provenance is empty (src/import.rs:111-138). Two 32-byte BLAKE3 values would add about 0.86x (arithmetic). A number locked before production fields are written would be relaxed silently.
- Nothing reads the stored `safe_hash` (only the insert at src/store/mod.rs:169); evidence handles recompute their hash from `safe_text` (src/rlm/evidence.rs:50, 78-92). Dropping it saves about 0.14x (arithmetic).
- No query uses `observation_time` today, but [Specify the bounded RLM and model boundary](https://github.com/yuann3/relic/issues/5) plans time filters (docs/research/rlm-model-boundary.md:141).
- [0002](0002-source-identity-and-revisions.md) deletes the per-record keyed fingerprint.

## Decision

Old gate: sanitized DB + FTS after checkpoint `<2x` sanitized source bytes.

Shrink first:
- Drop the stored `safe_hash` column. Anchors recompute from stored safe text.
- The per-record `keyed_fingerprint` is gone ([0002](0002-source-identity-and-revisions.md)).
- Encode per-field provenance compactly at revision level.
- Keep `observation_time` for the RLM time filters.
- FTS stays full-detail, with no prefix index ([0007](0007-gate-change-warm-fts-hot-prefixes.md)).

New gate:
- Restated as the measured ratio of store bytes to raw source bytes on the 1M fixture, with production fields actually written. Expected about 3x.
- Measured after S1 and S2 by the gate harness (macOS arm64, seed-77 1M fixture, production redaction, secure delete on): **2.83x**. The gate is **store bytes < 3.0x raw source bytes**.
- The restated figure is a gate, not a target: raising it later takes another gate-change ADR.

Justification: no full-detail FTS schema reaches `<2x`, and the variants that come closer break generated phrase queries. Shrinking first deletes unread state and removes the placeholder caveat, so the restated number is real when locked.

User-perceptible impact: disk use only. On the 1M fixture the store is about 160 MiB at ~3x, where `<2x` sanitized bytes would allow about 75 MiB. Search results and latency are unaffected; phrase queries keep working.

## Amends

- The storage acceptance matrix row "sanitized DB + FTS after checkpoint `<2x` sanitized source bytes" (docs/research/storage-search-runtime-architecture.md:320): replaced by the measured ratio, restated after S1a + S1b. The denominator changes from sanitized to raw source bytes.
- The stored-hash clause of [Specify source and evidence contracts](https://github.com/yuann3/relic/issues/3), "Relic stores the sanitized payload and its hash" (docs/research/source-evidence-contracts.md:153): Relic stores the sanitized payload; its hash is recomputed, not stored.

## Consequences

- S1a (import) drops `safe_hash` and writes compact revision-level provenance; S1b (redaction) writes production-redacted text. The figure is restated only after both land.
- The restated number comes from the gate harness ([0014](0014-performance-contract.md)): DB, `-wal` and `-shm` bytes after checkpoint on the 1M fixture, divided by the fixture's raw source bytes, recorded with machine metadata.
- S0 records the amendment on the map, linking this ADR.
- The store carries no relation indexes, because Relate is cut from v0.1 ([0009](0009-rlm-kernel.md)).

## Rejected alternatives

- Keep `<2x`: unreachable with full-detail FTS (table + FTS alone is 2.23x).
- `detail=none` or `detail=column` FTS: both break generated phrase queries; `detail=column` saves nothing, `detail=none` slows prefix merges.
- Renegotiate only, to today's 3.04x: production hashes and provenance would push the real figure up unseen.
- Restate as bytes per observation plus a factor on text: a new metric the destination never named.
- Drop `observation_time` (−0.34x): conflicts with the planned RLM time filters.

## Open facts

- The restated ratio itself (S1a + S1b). The ~3x expectation is arithmetic, and the fixture was not re-measured after 9928b0f replaced `observation_revision` with the partial `observation_timed` index.
