# Interactive FTS measurements for search-as-you-type

**Status:** measured, recommendation pending the #11 architecture lock
**Decision scope:** FTS5 schema options, result ordering, minimum query length, and recency pre-narrowing for the TUI's per-keystroke search; consequences for the #8 latency and size gates
**Research date:** 2026-10-03
**Evidence:** `benches/interactive_fts.rs` (run it yourself, below); raw output in the appendix

## Decision

The TUI should **order interactive results by recency (`ORDER BY rowid DESC`)**, not by BM25 `rank`. Ordering is what decides latency, and the schema option barely matters next to it:

- With `ORDER BY rank`, cost grows linearly with the number of matches, about **0.5 µs per matching row**: 33k matches take 16 ms, 108k take 52 ms, 475k take 260 ms. Prefix indexes, `detail=` options and short-query gates do not change this. FTS5 has to score every match before it can return the top 200. The 0.5 µs/match model holds for single-term queries only: rank meets `<20 ms` for a single whole word or long prefix under about 35k matches (3.5% of the corpus), but multi-token queries whose last token is a short prefix miss it even when selective (`git c*`, 13k matches / 1.3%, 47 ms; `cargo re*`, 18k / 1.8%, 30 ms).
- With `ORDER BY rowid DESC LIMIT n`, FTS5 walks the doclist newest-first and **stops after n rows**. A whole-word query for `cargo` (108k matches) drops from 52 ms to **0.03 ms**. The only expensive case left is a short prefix (`c*`, `ca*`), because FTS5 first merges the doclists of every term with that prefix.

Two ways to meet `<20 ms` on every keystroke:

| Option | Schema change | Worst FTS p95, page 200, every keystroke | DB size cost |
|---|---|---|---|
| **A (recommended for v0.1):** baseline schema, `rowid DESC`, a 1-char last token is not sent to FTS. The TUI keeps the previous page or shows the unfiltered recent page, which is a primary-key scan and was not measured. | none | **7.45 ms** (`cargo re*`) | **0** |
| **B:** add `prefix='1'` to `observation_fts`, `rowid DESC`, no length gate | one FTS option + rebuild (2 s at 1M) | **7.14 ms** (all 22 queries, 1-char included) | **+19.1 MiB (+0.36x source)** |

`prefix='1 2 3'` brings every query under 1.5 ms, but it costs +57 MiB (+1.07x). That is far more speed than a 20 ms budget needs, so it is not worth paying. Adding the per-row metadata fetch to either option costs 0.25 ms (N+1 lookups) or 0.06 ms (JOIN) per 200-row page. This is negligible, so the worst keystroke stays under 8 ms end to end, before channel and render time.

Option A is the default because the size gate already fails (see below) and the zero-cost version is fast enough. Option B is the knob to turn if 1-char filtering proves necessary in use.

BM25 `rank` stays useful for explicit, non-interactive searches, such as `relic search` and RLM recall. There it should run over a bounded recency window (V5) or accept the cost. It should not be the per-keystroke default.

## Method

- **Hardware:** Mac16,5 (Apple M4 Max, 16 cores, 128 GiB), macOS 27.0.1. Bundled SQLite 3.49.1 via rusqlite 0.35.
- **Machine state:** other agent sessions and a browser were running. Load average was 4.5 before the run and 3.0 after, on 16 cores. Every query runs on a single thread. A few p95/max outliers come from this noise, for example baseline `ca*` rank p50 145 ms against p95 256 ms. The p50 columns are the steadier signal.
- **Corpus:** `fixture_db` 1M diverse zsh records (`generate_diverse_zsh_history(1_000_000, 77)`, the #8 corpus). Source history is **55,906,677 bytes (53.3 MiB)**, regenerated in the bench with the same seed.
- **Variants:** for each schema, the bench copies `1m.db`, drops `observation_fts`, recreates it with the extra `fts5()` options, runs `INSERT INTO observation_fts(observation_fts) VALUES('rebuild')`, then runs `VACUUM` and `wal_checkpoint(TRUNCATE)`. The baseline is copied and vacuumed only. DB bytes are main file plus WAL. A per-area breakdown comes from `dbstat`.
- **Queries:** raw rusqlite SQL that mirrors `Store::search_prefix`. Each word is a quoted token, the last word gets `*` in prefix mode, and there is no raw FTS syntax. A unit test (`baseline_query_matches_store_search_prefix`) checks that the bench's baseline query returns exactly the ids `Store::search_prefix` returns, in the same order. The query is `SELECT rowid FROM observation_fts WHERE observation_fts MATCH ?1 [AND rowid > ?3] ORDER BY {rank | rowid DESC} LIMIT ?2`. The limit is page+1, matching `app::search`'s truncation probe. Pages are 200 (the TUI's) and 50 (what #8 measured).
- **Timing:** one discarded warm-up, then **50 timed runs per cell**. Results are nearest-rank p50 / p95 / max in ms, warm OS cache, with Store's read pragmas (`cache_size=-16384`, `temp_store=MEMORY`). Every run must return the same row count. Every variant's row list is compared with the baseline's for the same order and page; all were identical.
- **Query set (22):** 1-char `g* c* d*`. 2-char `gi* ca* do* ku*`. 3-char `git* car* doc*`. Full words as the TUI sends them (`cargo* docker*`; `git*` is the 3-char row). Exact words as #8 measured them (`git cargo docker`, Words mode). The keystrokes toward a two-word query: `git c*`, `git co*`, `git com*`, `cargo bu*`, `cargo re*`. And two **sparse ANDs**, frequent words that rarely or never share a row (`cargo com*` with 148 matches, `docker git*` with 0). The sparse ANDs are the worst case for an early-exit rowid walk.
- **Reproduce:**
  ```console
  cargo run --release --bin fixture_db -- target/fixtures/1m.db 1000000
  cargo run --release --bin interactive_fts -- target/fixtures/1m.db target/fts-variants 50
  ```
  This takes about 40 min, nearly all of it slow rank cells.

## Size per variant

Ratios are DB bytes divided by the 53.3 MiB source history file, measured after rebuild and `VACUUM`.

| Variant | DB MiB | Ratio | observation | obs indexes | FTS | Rebuild |
|---|---|---|---|---|---|---|
| V0 baseline | 162.0 | **3.04x** | 87.4 | 42.9 | 31.7 | - |
| V3 `prefix='1'` | 181.1 | 3.40x | 87.4 | 42.9 | 50.8 | 2.0 s |
| V3 `prefix='2 3'` | 200.3 | 3.76x | 87.4 | 42.9 | 70.0 | 2.8 s |
| V3 `prefix='1 2 3'` | 219.3 | 4.11x | 87.4 | 42.9 | 89.0 | 3.3 s |
| V4 `detail=column` | 161.9 | 3.04x | 87.4 | 42.9 | 31.6 | 1.6 s |
| V4 `detail=none` | 148.1 | 2.78x | 87.4 | 42.9 | 17.8 | 1.3 s |
| V3+V4 `prefix='1', detail=none` | 154.2 | 2.89x | 87.4 | 42.9 | 23.9 | 1.5 s |
| V3+V4 `prefix='1 2 3', detail=none` | 167.6 | 3.14x | 87.4 | 42.9 | 37.3 | 2.2 s |

Baseline objects in MiB: `observation` 87.4, `observation_fts_data` 22.1, `observation_time` 18.1, `sqlite_autoindex_observation_1` 12.4, `observation_revision` 12.4, `observation_fts_docsize` 9.6. Average observation column bytes: `safe_text` 39.2, `safe_hash` 8, `keyed_fingerprint` 8, `locator` 8, `provenance` 0.

**The FTS index is not what breaks the size gate.** The full FTS5 index is 0.59x of source. The `observation` table alone is **1.64x**, and its secondary indexes add 0.80x. `observation_revision (revision_id, ordinal)` duplicates the autoindex that `UNIQUE(revision_id, ordinal)` already creates, wasting 12.4 MiB (0.23x). `detail=column` saves nothing on this corpus: nearly every token occurs once per command, so the position lists are already tiny.

## Results by variant (FTS step, page 200, p95 ms)

All per-query p50 / p95 / max values for both pages are in the appendix. This table gives the worst p95 at page 200 over the 22 queries, under each minimum-length gate (V1). The gate applies to the length of the `*` token; whole-word queries always run.

| Variant | Order | V5 newest 10% | Size | Worst, all | Worst, prefix ≥2 | Worst, prefix ≥3 | Queries over 20 ms (all) |
|---|---|---|---|---|---|---|---|
| V0 baseline | rank | - | 3.04x | 267.2 | 256.3 | 76.4 | 11 of 22 |
| V0 baseline | rank | yes | 3.04x | 60.1 | 20.2 | 10.9 | `g* c* d* ca* git c*` |
| **V2 baseline** | **rowid DESC** | - | **3.04x** | 20.2 | **7.45** | 2.42 | `c*` (20.24) |
| V2+V5 baseline | rowid DESC | yes | 3.04x | 17.9 | 7.50 | 2.42 | none |
| **V3 `prefix='1'`** | **rowid DESC** | - | **3.40x** | **7.14** | 7.14 | 2.32 | none |
| V3 `prefix='1'` | rank | yes | 3.40x | 28.3 | 20.5 | 11.0 | `c* ca*` |
| V3 `prefix='2 3'` | rowid DESC | - | 3.76x | 16.1 | 1.60 | 1.60 | none |
| V3 `prefix='1 2 3'` | rowid DESC | - | 4.11x | 1.44 | 1.44 | 1.44 | none |
| V3 `prefix='1 2 3'` | rank | - | 4.11x | 248.6 | 92.5 | 57.2 | 9 of 22 |
| V3 `prefix='1 2 3'` | rank | yes | 4.11x | 27.0 | 10.9 | 8.8 | `c*` |
| V4 `detail=column` | rowid DESC | - | 3.04x | 17.9 | 7.37 | 2.52 | none |
| V4 `detail=column` | rank | - | 3.04x | 458.1 | 196.6 | 121.5 | 18 of 22 |
| V4 `detail=none` | rowid DESC | - | 2.78x | 31.4 | 15.5 | 5.97 | `c* git c*` |
| V4 `detail=none` | rank | - | 2.78x | 580.6 | 200.1 | 124.6 | 18 of 22 |
| V3+V4 `prefix='1', detail=none` | rowid DESC | - | 2.89x | 16.0 | 16.0 | 6.28 | none |
| V3+V4 `prefix='1 2 3', detail=none` | rowid DESC | - | 3.14x | 5.58 | 5.58 | 5.58 | none |

### What each variant showed

**V0 baseline (`ORDER BY rank`, what `Store::search_prefix` does today).** Cost scales with match count, not with query length. `gi*`, `git*`, `ku*` and `docker` (about 33k matches each) sit at 16-19 ms, at the edge of the gate. `cargo`, `car*`, `doc*` and `do*` (about 108k) take 52-58 ms. `ca*` (189k) takes 95-145 ms at p50, and the 1-char prefixes (200k-475k) take 100-270 ms. This reproduces #8's 42-74 ms finding and extends it. Page size (200 against 50) makes no difference.

**V1 (minimum length).** A gate does not rescue `rank`. At 2 chars `ca*` still costs 256 ms p95, and at 3 chars `car*` still costs 76 ms. The gate only helps together with `rowid DESC`, where it removes the merge cost of 1-char prefixes. The UX cost: with a 2-char gate, typing `g`, `c`, `d` and the `c` in `git c` produces no new FTS query, and the TUI keeps or shows the recency page. With a 3-char gate, `gi`, `ca`, `git co` and `cargo re` are skipped as well. The 3-char gate buys nothing under `rowid DESC`, because 2-char prefixes already cost at most 7.5 ms.

**V2 (`ORDER BY rowid DESC LIMIT n`).** The early exit is real. The query plan shows the vtab consuming the order (`INDEX 192:M1`, no temp B-tree). Single whole words take 0.02-0.04 ms whatever their frequency, and page 50 and page 200 cost the same. Prefix tokens without a prefix index pay to merge every doclist under the prefix before the walk can start, and that cost grows with how many terms and rows sit under it: `cargo*` 1.5 ms, `ca*` 5 ms, `c*` 18-20 ms, against 0.03 ms for the exact word `cargo`. The token AND `cargo re*` (7.5 ms) is the slowest query that passes a 2-char gate. Sparse ANDs stay cheap (0.9-1.7 ms), because FTS5 seeks through doclists with its doclist index instead of scanning them.

**V3 (prefix index).** A prefix index turns `x*` into one stored term, so `rowid DESC` early-exits for prefixes too. `prefix='1'` fixes exactly the 1-char cases (`c*` goes from 20 ms to 0.06 ms, `git c*` from 18 ms to 0.14 ms) for +0.36x. `prefix='2 3'` fixes 2-3 char prefixes but leaves `c*` at 16 ms, for +0.72x. `prefix='1 2 3'` takes every query under 1.5 ms for +1.07x. Under `rank`, prefix indexes help single-term prefixes only a few percent (`g*` goes from 103 to 96 ms), because scoring still visits every match. They help only where the merge dominated a selective AND: `git c*` goes from 47 to 16 ms with `prefix='1'`.

**V4 (`detail=column` / `detail=none`).** `detail=column` saves no space and **doubles** rank cost, so it is rejected. `detail=none` saves 13.9 MiB (0.26x) but makes rank 2x slower, and `rowid DESC` prefix merges slower (`c*` 31 ms). It also **breaks queries Relic generates today.** Both `detail` options reject any multi-token phrase (`fts5: phrase queries are not supported (detail!=full)`). Relic's escaping turns any word that the tokenizer splits into a phrase: `"src/main.rs"`, `"task-12"*`, `"user@host"`. Searching for a path would fail. `detail=none` also rejects column filters, and both reject `NEAR` (neither is user-reachable). Adopting either would mean redefining escaping as "split into tokens and AND them", which loses adjacency, to save 0.26x. Not recommended.

**V5 (recency pre-narrowing, `AND rowid > ?`).** FTS5 consumes the rowid range (`INDEX 32:M1>` / `192:M1>`). Under `rank` it cuts cost in proportion to the matches left: newest 10% gives about 10x less, which brings most queries under 20 ms. It leaves `c*` (60 ms), `d*` (25 ms), `g*` (22 ms) and `git c*` (41 ms) over the gate, and `ca*` right on it (20.2 ms). Under `rowid DESC` it changes nothing, because the walk already stops early. The fixture writes one command per second, so 1M rows span only 11.6 days and "last 30 days" would cover everything. The bench therefore uses the newest 10% of rowids (100k rows, 27.8 h of fixture time) as the window. A real 1M-command history spans years, so a 30-day window would be much smaller than 10%.

## End-to-end page cost: FTS plus metadata (page 200, p95 ms)

| Query | Order | FTS only | FTS + N+1 lookups (Store today) | JOIN on FTS subquery | Naive JOIN | `app::search` (real Store) |
|---|---|---|---|---|---|---|
| `g*` | rank | 114.4 | 108.6 | 120.5 | 113.6 | 111.1 |
| `g*` | rowid DESC | 5.13 | 5.30 | 5.40 | 5.20 | n/a |
| `gi*` | rank | 16.5 | 16.6 | 17.4 | 16.5 | 17.1 |
| `gi*` | rowid DESC | 0.46 | 0.69 | 0.52 | 0.50 | n/a |
| `cargo` | rank | 52.3 | 52.0 | 52.9 | 50.3 | 51.4 |
| `cargo` | rowid DESC | 0.03 | 0.27 | 0.09 | 0.07 | n/a |
| `git com*` | rank | 1.59 | 1.80 | 2.13 | 1.63 | 2.30 |
| `git com*` | rowid DESC | 0.12 | 0.37 | 0.18 | 0.15 | n/a |
| `g*` (prefix='1') | rowid DESC | 0.05 | 0.29 | 0.11 | 0.09 | n/a |

**The metadata fetch does not matter.** 200 per-row lookups add about 0.23 ms, and a single JOIN adds about 0.06 ms. SQLite's planner pushes `ORDER BY ... LIMIT` into the FTS vtab even in the naive `fts JOIN observation` form, so the JOIN does not fetch metadata for every match. Store's two-step N+1 is fine to keep. A JOIN is a small, optional cleanup.

## What this implies for the #8 gates

- **FTS query over 1M, warm p95 `<20 ms`:** **met** for the interactive path with option A or B, at every measured keystroke. The worst case is 7.5 ms, 2.7x headroom before channel and render. **Not met** by any schema if results are BM25-ranked over all matches. #11 must pick: (1) recency order for interactive and default search, with rank as an opt-in mode that is either bounded by a window or allowed to be slow; or (2) keep rank and relax the gate to "p95 `<20 ms` for single whole-word or long-prefix queries under about 35k matches". Even that narrower gate excludes ordinary keystrokes: multi-token queries ending in a 1–2 char prefix miss it at 1–2% selectivity (`git c*` 47 ms, `cargo re*` 30 ms), so option (2) cannot serve search-as-you-type. Option (1) is recommended. Shell history search (Ctrl-R, fzf, atuin) is recency-first anyway, and BM25 over 40-byte commands says little about relevance.
- **DB size `<2x` source:** **not met by any FTS variant, and not because of FTS.** The cheapest measured schema, `detail=none`, which breaks path search, is 2.78x. The base table plus its indexes is 2.44x before any FTS index. Even with no FTS and no secondary indexes, `observation` alone is 1.64x. Levers outside this slice: drop the redundant `observation_revision` index (−0.23x, to about 2.81x); shrink per-row hashes and locators (24 of the roughly 92 bytes per row); question whether `observation_time` (0.34x) is needed. #11 should renegotiate the gate to about **3x** for full-detail FTS. If option B is chosen, it should budget `prefix='1'` at +0.36x on top. A denominator note: #8 measured against raw history-file bytes, including zsh's `: ts:dur;` prefix. Against the sanitized text alone (39.2 MB), every ratio is about 1.4x higher.

## Open questions for #11

1. **Recency is rowid order, not time order.** `rowid DESC` means "most recently imported", which matches `observed_at_ms` only while imports arrive in time order. A backfilled older history (say, bash imported after zsh) would sort above newer commands. Options: accept "recently imported"; allocate rowids per time bucket at import; or order by `observed_at_ms` through the `observation_time` index. The last is a different plan with no FTS early exit and was not measured.
2. **Rank inside a recency window.** "Newest K matches by rowid, then re-sort by bm25" (for example K=2,000) would keep some relevance at bounded cost. It was not measured.
3. **1-char UX.** Option A shows the unfiltered recent page (or keeps the previous page) for a 1-char last token. If users expect `g` to filter, take option B (+0.36x).
4. **Size gate wording.** Decide whether the denominator is raw history bytes or sanitized text, and whether the gate covers FTS or only the base store.
5. **Real data.** The fixture has about 35 distinct leading tokens and one command per second. Real histories have longer tails (more distinct terms per prefix, so slower prefix merges without a prefix index) and sparser timestamps. Re-measure options A and B on an anonymized real 1M history before locking.
6. **Cold cache.** Everything here is warm OS cache. The #8 cold gate (`<100 ms` p95) was not re-measured for these variants.

## Appendix: raw output

<details>
<summary>Full output of <code>interactive_fts target/fixtures/1m.db target/fts-variants 50</code></summary>

=== interactive FTS measurements ===
host: Mac16,5 | Apple M4 Max | 16 | 137438953472
os: 27.0.1
load before: { 4.52 3.86 4.35 }
sqlite (bundled): 3.49.1
timed runs per cell: 50 (+1 discarded warm-up); cells are p50 / p95 / max ms
records: 1000000, source history: 55906677 bytes (53.3 MiB), unvacuumed fixture db: 177213440 bytes

## Size per schema (after rebuild + VACUUM)

| variant | DB MiB | ratio to source | observation MiB | obs indexes MiB | fts MiB | rebuild s |
|---|---|---|---|---|---|---|
| V0 baseline | 162.0 | 3.04x | 87.4 | 42.9 | 31.7 | 0.0 |
| V3 prefix='1' | 181.1 | 3.40x | 87.4 | 42.9 | 50.8 | 2.0 |
| V3 prefix='1 2 3' | 219.3 | 4.11x | 87.4 | 42.9 | 89.0 | 3.3 |
| V3 prefix='2 3' | 200.3 | 3.76x | 87.4 | 42.9 | 70.0 | 2.8 |
| V4 detail=column | 161.9 | 3.04x | 87.4 | 42.9 | 31.6 | 1.6 |
| V4 detail=none | 148.1 | 2.78x | 87.4 | 42.9 | 17.8 | 1.3 |
| V3+V4 prefix='1' detail=none | 154.2 | 2.89x | 87.4 | 42.9 | 23.9 | 1.5 |
| V3+V4 prefix='1 2 3' detail=none | 167.6 | 3.14x | 87.4 | 42.9 | 37.3 | 2.2 |

baseline objects (MiB): observation 87.4, observation_fts_data 22.1, observation_time 18.1, sqlite_autoindex_observation_1 12.4, observation_revision 12.4, observation_fts_docsize 9.6, observation_fts_idx 0.0, sqlite_schema 0.0
avg observation column bytes: safe_text 39.2, safe_hash 8.0, keyed_fingerprint 8.0, provenance 0.0, locator 8.0

recency window (V5): rowid > 900000 = newest 100000 rows, 27.8 h of fixture time

## Query plans on the baseline schema

- rank : SCAN observation_fts VIRTUAL TABLE INDEX 32:M1
- rowid DESC : SCAN observation_fts VIRTUAL TABLE INDEX 192:M1
- rank + rowid range: SCAN observation_fts VIRTUAL TABLE INDEX 32:M1>
- rowid DESC + rowid range: SCAN observation_fts VIRTUAL TABLE INDEX 192:M1>

## V0 baseline

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 199756 | 103.11 / 105.54 / 105.77 | 103.84 / 105.82 / 106.80 | 5.91 / 6.76 / 6.90 | 5.68 / 6.53 / 7.22 | yes |
| `"c"*` | 1-char | 475508 | 261.17 / 267.20 / 267.67 | 259.18 / 268.76 / 274.40 | 18.68 / 20.24 / 21.98 | 17.83 / 18.95 / 19.74 | yes |
| `"d"*` | 1-char | 231251 | 120.00 / 125.50 / 126.87 | 121.86 / 124.85 / 127.12 | 6.27 / 6.49 / 6.62 | 6.26 / 6.46 / 6.57 | yes |
| `"gi"*` | 2-char | 33236 | 16.69 / 17.58 / 17.74 | 16.68 / 17.42 / 17.95 | 0.48 / 0.50 / 0.51 | 0.46 / 0.48 / 0.49 | yes |
| `"ca"*` | 2-char | 188532 | 144.63 / 256.31 / 312.14 | 103.15 / 179.59 / 244.42 | 4.77 / 4.98 / 5.03 | 4.85 / 5.00 / 5.06 | yes |
| `"do"*` | 2-char | 107715 | 55.17 / 57.80 / 57.93 | 55.41 / 57.24 / 58.99 | 2.27 / 2.38 / 2.40 | 2.25 / 2.36 / 2.41 | yes |
| `"ku"*` | 2-char | 33417 | 16.62 / 17.15 / 17.40 | 19.99 / 27.94 / 28.73 | 0.45 / 0.46 / 0.48 | 0.44 / 0.45 / 0.47 | yes |
| `"git"*` | 3-char | 33236 | 17.15 / 19.21 / 20.43 | 16.53 / 17.34 / 17.71 | 0.45 / 0.48 / 0.49 | 0.45 / 0.47 / 0.48 | yes |
| `"car"*` | 3-char | 108126 | 53.05 / 76.36 / 79.13 | 62.01 / 70.57 / 72.28 | 1.44 / 1.49 / 1.60 | 1.44 / 1.50 / 1.51 | yes |
| `"doc"*` | 3-char | 107715 | 54.61 / 56.72 / 57.61 | 54.89 / 56.03 / 56.44 | 2.27 / 2.42 / 2.49 | 2.26 / 2.34 / 2.36 | yes |
| `"cargo"*` | word* | 108126 | 52.88 / 54.35 / 55.06 | 52.81 / 54.09 / 55.57 | 1.45 / 1.53 / 1.85 | 1.46 / 1.57 / 1.76 | yes |
| `"docker"*` | word* | 33352 | 16.60 / 17.31 / 17.42 | 16.73 / 17.27 / 17.49 | 0.47 / 0.50 / 0.51 | 0.47 / 0.51 / 0.57 | yes |
| `"git"` | word | 33236 | 15.85 / 16.23 / 16.39 | 15.78 / 16.24 / 16.32 | 0.03 / 0.03 / 0.04 | 0.02 / 0.03 / 0.03 | yes |
| `"cargo"` | word | 108126 | 51.30 / 52.74 / 53.14 | 50.57 / 52.33 / 52.80 | 0.02 / 0.03 / 0.04 | 0.02 / 0.02 / 0.02 | yes |
| `"docker"` | word | 33352 | 15.65 / 15.94 / 15.97 | 15.56 / 16.17 / 16.55 | 0.03 / 0.03 / 0.04 | 0.02 / 0.03 / 0.04 | yes |
| `"git" "c"*` | two-word | 13272 | 46.44 / 47.10 / 47.38 | 46.46 / 47.37 / 47.85 | 17.44 / 17.86 / 18.09 | 17.28 / 17.98 / 18.13 | yes |
| `"git" "co"*` | two-word | 6622 | 13.72 / 14.82 / 15.49 | 13.76 / 15.12 / 15.31 | 3.92 / 4.12 / 4.86 | 3.91 / 4.15 / 4.33 | yes |
| `"git" "com"*` | two-word | 1842 | 1.59 / 1.69 / 1.74 | 1.58 / 1.65 / 1.71 | 0.12 / 0.13 / 0.14 | 0.07 / 0.09 / 0.10 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.02 / 0.03 | 0.02 / 0.02 / 0.02 | 0.02 / 0.03 / 0.03 | 0.02 / 0.03 / 0.04 | yes |
| `"cargo" "re"*` | two-word | 17736 | 29.28 / 29.85 / 30.23 | 29.12 / 29.58 / 29.70 | 7.23 / 7.45 / 7.59 | 7.26 / 7.47 / 10.14 | yes |
| `"cargo" "com"*` | sparse AND | 148 | 1.85 / 1.90 / 1.97 | 1.84 / 1.90 / 1.94 | 0.81 / 0.86 / 0.88 | 0.35 / 0.38 / 0.38 | yes |
| `"docker" "git"*` | sparse AND | 0 | 1.18 / 1.20 / 1.22 | 1.17 / 1.19 / 1.22 | 1.64 / 1.72 / 1.75 | 1.62 / 1.70 / 1.74 | yes |

## V0 baseline + V5 newest-10% rowid range

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 20046 | 20.94 / 21.58 / 21.76 | 20.82 / 21.66 / 24.09 | 5.14 / 5.32 / 5.38 | 5.11 / 5.27 / 5.32 | yes |
| `"c"*` | 1-char | 47664 | 59.25 / 60.13 / 60.51 | 59.22 / 59.89 / 61.53 | 17.41 / 17.72 / 17.74 | 17.39 / 17.71 / 18.07 | yes |
| `"d"*` | 1-char | 22970 | 24.75 / 25.36 / 27.18 | 24.96 / 26.21 / 26.63 | 6.32 / 6.53 / 6.61 | 6.19 / 6.62 / 10.28 | yes |
| `"gi"*` | 2-char | 3353 | 2.80 / 3.08 / 3.08 | 2.78 / 2.92 / 2.96 | 0.48 / 0.50 / 0.52 | 0.47 / 0.53 / 0.56 | yes |
| `"ca"*` | 2-char | 18895 | 19.72 / 20.17 / 20.37 | 19.66 / 20.17 / 20.54 | 4.91 / 5.06 / 5.12 | 4.88 / 5.02 / 5.27 | yes |
| `"do"*` | 2-char | 10755 | 10.07 / 10.40 / 10.60 | 10.10 / 10.28 / 10.33 | 2.35 / 2.80 / 3.16 | 2.34 / 2.42 / 2.49 | yes |
| `"ku"*` | 2-char | 3280 | 2.58 / 2.63 / 2.64 | 2.59 / 2.63 / 2.65 | 0.47 / 0.49 / 0.49 | 0.46 / 0.46 / 0.47 | yes |
| `"git"*` | 3-char | 3353 | 2.61 / 2.66 / 2.68 | 2.59 / 2.67 / 2.68 | 0.48 / 0.49 / 0.68 | 0.46 / 0.55 / 0.82 | yes |
| `"car"*` | 3-char | 10794 | 8.56 / 8.71 / 8.73 | 8.58 / 9.14 / 9.42 | 1.50 / 1.84 / 3.48 | 1.49 / 1.51 / 1.77 | yes |
| `"doc"*` | 3-char | 10755 | 10.56 / 10.85 / 10.95 | 10.18 / 10.59 / 10.63 | 2.35 / 2.42 / 2.44 | 2.30 / 2.35 / 2.35 | yes |
| `"cargo"*` | word* | 10794 | 8.41 / 8.63 / 8.67 | 8.45 / 8.67 / 8.69 | 1.46 / 1.50 / 1.51 | 1.42 / 1.46 / 1.50 | yes |
| `"docker"*` | word* | 3292 | 2.64 / 2.74 / 2.94 | 2.62 / 2.72 / 2.76 | 0.46 / 0.48 / 0.48 | 0.46 / 0.47 / 0.48 | yes |
| `"git"` | word | 3353 | 1.88 / 1.95 / 2.03 | 1.86 / 1.98 / 2.08 | 0.03 / 0.03 / 0.04 | 0.02 / 0.02 / 0.03 | yes |
| `"cargo"` | word | 10794 | 5.91 / 6.29 / 7.63 | 5.86 / 6.14 / 6.43 | 0.03 / 0.03 / 0.03 | 0.02 / 0.02 / 0.03 | yes |
| `"docker"` | word | 3292 | 1.83 / 1.90 / 1.99 | 1.83 / 1.86 / 1.87 | 0.03 / 0.03 / 0.03 | 0.02 / 0.03 / 0.03 | yes |
| `"git" "c"*` | two-word | 1397 | 39.77 / 40.61 / 42.10 | 40.03 / 43.09 / 66.30 | 17.46 / 17.87 / 18.11 | 17.39 / 17.99 / 18.05 | yes |
| `"git" "co"*` | two-word | 718 | 10.05 / 10.47 / 10.71 | 10.04 / 10.35 / 10.40 | 3.94 / 4.15 / 7.99 | 3.89 / 3.98 / 4.02 | yes |
| `"git" "com"*` | two-word | 184 | 0.67 / 0.70 / 0.72 | 0.66 / 0.68 / 0.70 | 0.13 / 0.14 / 0.15 | 0.08 / 0.10 / 0.10 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.03 / 0.05 | 0.02 / 0.02 / 0.02 | 0.02 / 0.03 / 0.03 | 0.02 / 0.03 / 0.03 | yes |
| `"cargo" "re"*` | two-word | 1754 | 18.72 / 19.20 / 19.40 | 19.43 / 22.13 / 23.47 | 7.20 / 7.50 / 7.54 | 7.14 / 7.34 / 7.42 | yes |
| `"cargo" "com"*` | sparse AND | 15 | 1.60 / 1.85 / 1.89 | 1.66 / 1.91 / 1.94 | 0.16 / 0.17 / 0.18 | 0.16 / 0.18 / 0.20 | yes |
| `"docker" "git"*` | sparse AND | 0 | 1.33 / 1.55 / 1.62 | 1.42 / 1.64 / 1.65 | 1.62 / 1.70 / 1.72 | 1.58 / 1.65 / 1.67 | yes |

## V3 prefix='1'

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 199756 | 96.22 / 100.83 / 101.20 | 98.18 / 100.68 / 101.13 | 0.05 / 0.06 / 0.07 | 0.05 / 0.05 / 0.05 | yes |
| `"c"*` | 1-char | 475508 | 238.04 / 247.68 / 263.32 | 230.26 / 236.78 / 244.18 | 0.06 / 0.06 / 0.06 | 0.05 / 0.05 / 0.05 | yes |
| `"d"*` | 1-char | 231251 | 109.78 / 111.20 / 111.74 | 108.43 / 111.28 / 111.43 | 0.04 / 0.05 / 0.05 | 0.04 / 0.04 / 0.04 | yes |
| `"gi"*` | 2-char | 33236 | 16.33 / 16.81 / 17.33 | 16.27 / 16.76 / 16.87 | 0.46 / 0.48 / 0.49 | 0.45 / 0.46 / 0.48 | yes |
| `"ca"*` | 2-char | 188532 | 97.47 / 98.93 / 99.30 | 96.50 / 98.65 / 99.25 | 4.80 / 4.85 / 4.85 | 4.82 / 4.90 / 4.92 | yes |
| `"do"*` | 2-char | 107715 | 53.91 / 57.73 / 58.74 | 54.51 / 55.86 / 55.95 | 2.30 / 2.38 / 2.46 | 2.29 / 2.42 / 2.46 | yes |
| `"ku"*` | 2-char | 33417 | 16.38 / 16.87 / 17.05 | 16.50 / 17.17 / 17.46 | 0.49 / 0.51 / 0.52 | 0.48 / 0.50 / 0.51 | yes |
| `"git"*` | 3-char | 33236 | 16.23 / 16.70 / 16.85 | 16.39 / 16.82 / 16.91 | 0.48 / 0.51 / 0.52 | 0.48 / 0.50 / 0.52 | yes |
| `"car"*` | 3-char | 108126 | 52.48 / 53.94 / 54.12 | 52.95 / 56.62 / 57.22 | 1.44 / 1.50 / 1.52 | 1.41 / 1.47 / 1.49 | yes |
| `"doc"*` | 3-char | 107715 | 54.30 / 55.75 / 56.44 | 54.35 / 55.62 / 56.52 | 2.28 / 2.32 / 2.33 | 2.27 / 2.32 / 2.35 | yes |
| `"cargo"*` | word* | 108126 | 53.53 / 56.33 / 57.45 | 55.96 / 59.22 / 113.64 | 1.43 / 1.44 / 1.44 | 1.39 / 1.40 / 1.46 | yes |
| `"docker"*` | word* | 33352 | 17.51 / 19.67 / 22.77 | 17.42 / 26.75 / 98.37 | 0.45 / 0.46 / 0.48 | 0.45 / 0.45 / 0.46 | yes |
| `"git"` | word | 33236 | 15.83 / 18.18 / 19.53 | 15.35 / 15.67 / 15.82 | 0.03 / 0.03 / 0.04 | 0.03 / 0.03 / 0.03 | yes |
| `"cargo"` | word | 108126 | 51.95 / 55.46 / 57.77 | 51.14 / 54.70 / 56.04 | 0.05 / 0.05 / 0.05 | 0.04 / 0.05 / 0.07 | yes |
| `"docker"` | word | 33352 | 15.67 / 16.28 / 16.34 | 15.71 / 16.18 / 16.29 | 0.04 / 0.04 / 0.04 | 0.03 / 0.03 / 0.04 | yes |
| `"git" "c"*` | two-word | 13272 | 15.47 / 16.35 / 17.15 | 15.30 / 15.70 / 16.06 | 0.13 / 0.14 / 0.14 | 0.09 / 0.09 / 0.10 | yes |
| `"git" "co"*` | two-word | 6622 | 13.62 / 13.96 / 14.04 | 13.58 / 13.79 / 14.02 | 3.79 / 3.84 / 3.96 | 3.78 / 4.35 / 4.97 | yes |
| `"git" "com"*` | two-word | 1842 | 1.55 / 1.57 / 1.58 | 1.55 / 1.58 / 1.64 | 0.11 / 0.14 / 0.15 | 0.08 / 0.21 / 0.28 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.02 / 0.02 | 0.02 / 0.02 / 0.03 | 0.05 / 0.05 / 0.06 | 0.05 / 0.06 / 0.09 | yes |
| `"cargo" "re"*` | two-word | 17736 | 28.74 / 29.94 / 30.93 | 29.32 / 30.67 / 31.78 | 7.00 / 7.14 / 7.18 | 6.98 / 7.36 / 7.49 | yes |
| `"cargo" "com"*` | sparse AND | 148 | 1.86 / 1.94 / 1.94 | 1.87 / 1.98 / 2.04 | 0.83 / 0.91 / 0.97 | 0.37 / 0.42 / 0.46 | yes |
| `"docker" "git"*` | sparse AND | 0 | 1.25 / 1.36 / 1.51 | 1.24 / 1.25 / 1.25 | 1.64 / 1.75 / 1.80 | 1.65 / 1.74 / 1.76 | yes |

## V3 prefix='1' + V5 newest-10% rowid range

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 20046 | 11.27 / 11.49 / 11.80 | 11.13 / 11.63 / 11.81 | 0.06 / 0.07 / 0.09 | 0.05 / 0.06 / 0.07 | yes |
| `"c"*` | 1-char | 47664 | 26.93 / 28.26 / 29.04 | 27.13 / 28.38 / 28.88 | 0.06 / 0.07 / 0.15 | 0.05 / 0.07 / 0.11 | yes |
| `"d"*` | 1-char | 22970 | 13.05 / 13.64 / 14.68 | 12.90 / 13.39 / 13.63 | 0.05 / 0.06 / 0.09 | 0.04 / 0.05 / 0.06 | yes |
| `"gi"*` | 2-char | 3353 | 2.66 / 2.73 / 2.74 | 2.73 / 3.04 / 4.81 | 0.49 / 0.56 / 0.59 | 0.47 / 0.56 / 0.60 | yes |
| `"ca"*` | 2-char | 18895 | 19.69 / 20.52 / 20.71 | 19.94 / 20.71 / 20.86 | 4.79 / 5.04 / 5.07 | 4.83 / 5.06 / 5.19 | yes |
| `"do"*` | 2-char | 10755 | 10.41 / 10.61 / 10.81 | 10.24 / 10.63 / 10.88 | 2.40 / 2.55 / 2.60 | 2.38 / 2.54 / 2.57 | yes |
| `"ku"*` | 2-char | 3280 | 2.81 / 2.93 / 2.94 | 2.77 / 2.93 / 2.99 | 0.48 / 0.53 / 0.59 | 0.46 / 0.49 / 0.51 | yes |
| `"git"*` | 3-char | 3353 | 2.78 / 2.95 / 3.01 | 2.76 / 3.09 / 3.56 | 0.47 / 0.52 / 0.54 | 0.45 / 0.49 / 0.60 | yes |
| `"car"*` | 3-char | 10794 | 8.42 / 8.72 / 8.80 | 9.07 / 9.92 / 11.90 | 1.39 / 1.43 / 1.45 | 1.40 / 1.43 / 1.47 | yes |
| `"doc"*` | 3-char | 10755 | 10.75 / 10.95 / 11.12 | 10.55 / 11.15 / 11.65 | 2.27 / 2.32 / 2.34 | 2.27 / 2.37 / 2.45 | yes |
| `"cargo"*` | word* | 10794 | 8.78 / 9.13 / 9.43 | 8.67 / 9.01 / 13.23 | 1.41 / 1.49 / 1.52 | 1.40 / 1.43 / 1.45 | yes |
| `"docker"*` | word* | 3292 | 2.58 / 2.65 / 2.70 | 2.56 / 2.61 / 2.64 | 0.46 / 0.48 / 0.49 | 0.45 / 0.47 / 0.50 | yes |
| `"git"` | word | 3353 | 1.82 / 1.88 / 1.89 | 1.82 / 1.86 / 1.87 | 0.03 / 0.04 / 0.04 | 0.03 / 0.03 / 0.04 | yes |
| `"cargo"` | word | 10794 | 5.88 / 6.00 / 6.15 | 5.87 / 6.02 / 6.05 | 0.05 / 0.05 / 0.06 | 0.04 / 0.05 / 0.09 | yes |
| `"docker"` | word | 3292 | 1.81 / 1.96 / 2.01 | 1.85 / 1.89 / 1.90 | 0.04 / 0.04 / 0.05 | 0.03 / 0.04 / 0.04 | yes |
| `"git" "c"*` | two-word | 1397 | 6.92 / 7.03 / 7.05 | 6.90 / 7.13 / 7.65 | 0.14 / 0.14 / 0.15 | 0.09 / 0.10 / 0.10 | yes |
| `"git" "co"*` | two-word | 718 | 10.01 / 10.52 / 11.63 | 9.96 / 10.05 / 10.07 | 3.85 / 3.94 / 3.96 | 3.76 / 3.91 / 3.93 | yes |
| `"git" "com"*` | two-word | 184 | 0.62 / 0.64 / 0.64 | 0.61 / 0.62 / 0.63 | 0.11 / 0.11 / 0.12 | 0.08 / 0.08 / 0.09 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.02 / 0.03 | 0.02 / 0.02 / 0.02 | 0.05 / 0.05 / 0.05 | 0.05 / 0.05 / 0.06 | yes |
| `"cargo" "re"*` | two-word | 1754 | 18.72 / 19.25 / 19.48 | 19.18 / 20.23 / 22.45 | 7.12 / 7.29 / 8.73 | 7.00 / 7.26 / 7.29 | yes |
| `"cargo" "com"*` | sparse AND | 15 | 1.49 / 1.56 / 1.86 | 1.45 / 1.53 / 1.59 | 0.17 / 0.18 / 0.19 | 0.17 / 0.19 / 0.20 | yes |
| `"docker" "git"*` | sparse AND | 0 | 1.17 / 1.22 / 1.33 | 1.17 / 1.21 / 1.22 | 1.56 / 1.62 / 1.64 | 1.59 / 1.64 / 1.71 | yes |

## V3 prefix='1 2 3'

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 199756 | 95.30 / 101.75 / 102.55 | 96.05 / 101.64 / 102.66 | 0.04 / 0.04 / 0.04 | 0.03 / 0.03 / 0.03 | yes |
| `"c"*` | 1-char | 475508 | 231.26 / 248.60 / 251.05 | 231.83 / 239.78 / 243.53 | 0.04 / 0.04 / 0.05 | 0.03 / 0.03 / 0.03 | yes |
| `"d"*` | 1-char | 231251 | 108.73 / 114.00 / 116.73 | 108.94 / 113.92 / 114.74 | 0.05 / 0.05 / 0.06 | 0.04 / 0.04 / 0.04 | yes |
| `"gi"*` | 2-char | 33236 | 15.74 / 16.04 / 16.21 | 15.56 / 16.23 / 16.58 | 0.04 / 0.05 / 0.05 | 0.03 / 0.04 / 0.04 | yes |
| `"ca"*` | 2-char | 188532 | 89.70 / 92.51 / 93.01 | 89.67 / 91.56 / 92.39 | 0.04 / 0.04 / 0.05 | 0.03 / 0.05 / 0.05 | yes |
| `"do"*` | 2-char | 107715 | 50.74 / 52.26 / 52.51 | 50.08 / 51.24 / 51.81 | 0.04 / 0.05 / 0.06 | 0.04 / 0.04 / 0.04 | yes |
| `"ku"*` | 2-char | 33417 | 15.78 / 16.31 / 17.85 | 15.89 / 16.47 / 17.19 | 0.04 / 0.04 / 0.05 | 0.04 / 0.04 / 0.04 | yes |
| `"git"*` | 3-char | 33236 | 15.72 / 16.17 / 16.24 | 15.62 / 16.04 / 16.41 | 0.05 / 0.05 / 0.05 | 0.04 / 0.04 / 0.04 | yes |
| `"car"*` | 3-char | 108126 | 51.06 / 57.19 / 71.95 | 50.47 / 52.25 / 52.83 | 0.04 / 0.04 / 0.04 | 0.03 / 0.03 / 0.03 | yes |
| `"doc"*` | 3-char | 107715 | 50.48 / 51.84 / 52.74 | 50.10 / 51.99 / 52.22 | 0.05 / 0.05 / 0.06 | 0.04 / 0.05 / 0.05 | yes |
| `"cargo"*` | word* | 108126 | 52.77 / 54.70 / 54.95 | 53.07 / 54.80 / 55.18 | 1.40 / 1.44 / 1.48 | 1.38 / 1.44 / 1.46 | yes |
| `"docker"*` | word* | 33352 | 16.47 / 16.86 / 17.19 | 16.42 / 17.31 / 17.48 | 0.48 / 0.53 / 0.53 | 0.47 / 0.48 / 0.49 | yes |
| `"git"` | word | 33236 | 15.83 / 16.42 / 16.66 | 15.75 / 16.51 / 16.94 | 0.03 / 0.04 / 0.04 | 0.03 / 0.03 / 0.04 | yes |
| `"cargo"` | word | 108126 | 50.55 / 51.80 / 52.34 | 50.28 / 51.98 / 53.84 | 0.05 / 0.05 / 0.05 | 0.04 / 0.05 / 0.06 | yes |
| `"docker"` | word | 33352 | 15.42 / 16.01 / 16.13 | 15.51 / 16.22 / 16.28 | 0.04 / 0.05 / 0.05 | 0.04 / 0.04 / 0.04 | yes |
| `"git" "c"*` | two-word | 13272 | 15.22 / 15.78 / 15.86 | 15.22 / 15.60 / 15.72 | 0.15 / 0.16 / 0.17 | 0.08 / 0.09 / 0.10 | yes |
| `"git" "co"*` | two-word | 6622 | 7.39 / 7.70 / 9.02 | 7.39 / 7.84 / 8.30 | 0.14 / 0.15 / 0.15 | 0.08 / 0.09 / 0.09 | yes |
| `"git" "com"*` | two-word | 1842 | 1.52 / 1.54 / 1.56 | 1.53 / 1.56 / 1.58 | 0.10 / 0.11 / 0.13 | 0.06 / 0.06 / 0.06 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.02 / 0.02 | 0.02 / 0.02 / 0.02 | 0.05 / 0.05 / 0.05 | 0.05 / 0.05 / 0.05 | yes |
| `"cargo" "re"*` | two-word | 17736 | 16.64 / 17.00 / 17.22 | 16.55 / 17.00 / 17.05 | 0.14 / 0.15 / 0.16 | 0.09 / 0.10 / 0.10 | yes |
| `"cargo" "com"*` | sparse AND | 148 | 1.81 / 1.88 / 1.94 | 1.81 / 1.85 / 1.90 | 0.78 / 1.01 / 1.33 | 0.34 / 0.40 / 0.45 | yes |
| `"docker" "git"*` | sparse AND | 0 | 0.77 / 0.81 / 0.83 | 0.77 / 0.80 / 0.81 | 1.16 / 1.21 / 1.21 | 1.16 / 1.21 / 1.23 | yes |

## V3 prefix='1 2 3' + V5 newest-10% rowid range

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 20046 | 10.90 / 11.26 / 11.35 | 10.83 / 11.05 / 11.18 | 0.04 / 0.04 / 0.05 | 0.03 / 0.03 / 0.03 | yes |
| `"c"*` | 1-char | 47664 | 26.36 / 26.98 / 27.14 | 26.29 / 27.03 / 27.84 | 0.04 / 0.04 / 0.05 | 0.03 / 0.04 / 0.05 | yes |
| `"d"*` | 1-char | 22970 | 12.48 / 12.80 / 13.05 | 12.67 / 13.02 / 13.06 | 0.05 / 0.05 / 0.05 | 0.04 / 0.04 / 0.05 | yes |
| `"gi"*` | 2-char | 3353 | 1.89 / 1.97 / 2.01 | 1.90 / 1.96 / 2.02 | 0.04 / 0.04 / 0.05 | 0.03 / 0.03 / 0.04 | yes |
| `"ca"*` | 2-char | 18895 | 10.49 / 10.86 / 11.05 | 10.46 / 10.92 / 11.25 | 0.04 / 0.04 / 0.04 | 0.03 / 0.03 / 0.04 | yes |
| `"do"*` | 2-char | 10755 | 5.92 / 6.10 / 6.14 | 5.85 / 6.08 / 6.17 | 0.05 / 0.05 / 0.05 | 0.04 / 0.04 / 0.04 | yes |
| `"ku"*` | 2-char | 3280 | 1.87 / 1.96 / 2.01 | 1.89 / 1.96 / 2.03 | 0.04 / 0.05 / 0.05 | 0.04 / 0.04 / 0.04 | yes |
| `"git"*` | 3-char | 3353 | 1.93 / 2.07 / 2.67 | 1.91 / 1.99 / 2.01 | 0.05 / 0.05 / 0.06 | 0.04 / 0.04 / 0.04 | yes |
| `"car"*` | 3-char | 10794 | 5.92 / 6.15 / 6.21 | 5.92 / 6.26 / 6.51 | 0.04 / 0.04 / 0.04 | 0.03 / 0.04 / 0.05 | yes |
| `"doc"*` | 3-char | 10755 | 5.84 / 6.00 / 6.04 | 5.88 / 6.07 / 6.24 | 0.05 / 0.06 / 0.06 | 0.04 / 0.05 / 0.05 | yes |
| `"cargo"*` | word* | 10794 | 8.48 / 8.81 / 8.90 | 8.42 / 8.55 / 8.74 | 1.39 / 1.47 / 1.50 | 1.39 / 1.46 / 1.51 | yes |
| `"docker"*` | word* | 3292 | 2.57 / 2.67 / 2.71 | 2.60 / 2.72 / 2.93 | 0.46 / 0.48 / 0.49 | 0.45 / 0.46 / 0.46 | yes |
| `"git"` | word | 3353 | 1.91 / 2.02 / 2.09 | 1.89 / 1.97 / 1.99 | 0.04 / 0.04 / 0.04 | 0.03 / 0.03 / 0.03 | yes |
| `"cargo"` | word | 10794 | 5.88 / 6.04 / 6.06 | 5.89 / 6.05 / 6.12 | 0.05 / 0.05 / 0.05 | 0.04 / 0.05 / 0.05 | yes |
| `"docker"` | word | 3292 | 1.87 / 1.97 / 2.01 | 1.86 / 1.90 / 1.94 | 0.04 / 0.05 / 0.05 | 0.04 / 0.04 / 0.04 | yes |
| `"git" "c"*` | two-word | 1397 | 6.76 / 6.94 / 7.16 | 6.79 / 7.03 / 7.06 | 0.14 / 0.15 / 0.15 | 0.08 / 0.08 / 0.09 | yes |
| `"git" "co"*` | two-word | 718 | 3.07 / 3.17 / 3.25 | 3.06 / 3.12 / 3.14 | 0.15 / 0.16 / 0.16 | 0.08 / 0.10 / 0.12 | yes |
| `"git" "com"*` | two-word | 184 | 0.64 / 0.65 / 0.65 | 0.63 / 0.66 / 0.67 | 0.10 / 0.10 / 0.11 | 0.06 / 0.06 / 0.06 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.03 / 0.03 | 0.02 / 0.02 / 0.03 | 0.05 / 0.05 / 0.05 | 0.05 / 0.05 / 0.05 | yes |
| `"cargo" "re"*` | two-word | 1754 | 5.31 / 5.44 / 5.52 | 5.29 / 5.50 / 5.53 | 0.14 / 0.15 / 0.15 | 0.09 / 0.10 / 0.10 | yes |
| `"cargo" "com"*` | sparse AND | 15 | 1.37 / 1.42 / 1.46 | 1.37 / 1.40 / 1.46 | 0.15 / 0.16 / 0.16 | 0.15 / 0.16 / 0.17 | yes |
| `"docker" "git"*` | sparse AND | 0 | 0.79 / 0.85 / 0.88 | 0.77 / 0.82 / 0.83 | 1.16 / 1.21 / 1.24 | 1.17 / 1.25 / 1.26 | yes |

## V3 prefix='2 3'

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 199756 | 102.79 / 104.60 / 105.05 | 101.81 / 104.86 / 105.87 | 5.08 / 5.21 / 5.24 | 5.02 / 5.12 / 5.24 | yes |
| `"c"*` | 1-char | 475508 | 257.28 / 268.18 / 270.96 | 258.31 / 283.62 / 319.58 | 15.32 / 15.93 / 21.29 | 15.23 / 15.65 / 16.44 | yes |
| `"d"*` | 1-char | 231251 | 124.28 / 127.81 / 129.51 | 123.78 / 127.09 / 127.89 | 5.98 / 6.03 / 6.22 | 6.02 / 6.42 / 6.56 | yes |
| `"gi"*` | 2-char | 33236 | 16.63 / 17.05 / 17.47 | 15.85 / 16.89 / 17.43 | 0.03 / 0.04 / 0.04 | 0.03 / 0.03 / 0.03 | yes |
| `"ca"*` | 2-char | 188532 | 91.42 / 94.78 / 95.14 | 93.28 / 98.08 / 111.31 | 0.05 / 0.05 / 0.06 | 0.04 / 0.04 / 0.05 | yes |
| `"do"*` | 2-char | 107715 | 52.04 / 53.40 / 53.64 | 52.84 / 54.93 / 55.41 | 0.05 / 0.05 / 0.06 | 0.04 / 0.05 / 0.06 | yes |
| `"ku"*` | 2-char | 33417 | 16.00 / 17.02 / 18.12 | 16.51 / 17.27 / 17.42 | 0.04 / 0.04 / 0.05 | 0.03 / 0.03 / 0.04 | yes |
| `"git"*` | 3-char | 33236 | 16.18 / 17.05 / 17.11 | 16.22 / 17.38 / 18.51 | 0.05 / 0.05 / 0.05 | 0.04 / 0.05 / 0.05 | yes |
| `"car"*` | 3-char | 108126 | 51.22 / 53.19 / 54.06 | 50.65 / 54.28 / 55.91 | 0.05 / 0.06 / 0.06 | 0.05 / 0.07 / 0.12 | yes |
| `"doc"*` | 3-char | 107715 | 52.69 / 54.68 / 55.54 | 50.22 / 53.59 / 54.23 | 0.05 / 0.07 / 0.18 | 0.05 / 0.05 / 0.05 | yes |
| `"cargo"*` | word* | 108126 | 52.39 / 53.87 / 54.83 | 51.71 / 53.34 / 54.79 | 1.51 / 1.60 / 1.62 | 1.49 / 1.59 / 1.62 | yes |
| `"docker"*` | word* | 33352 | 16.21 / 16.65 / 17.31 | 16.06 / 16.49 / 16.75 | 0.48 / 0.57 / 0.60 | 0.47 / 0.50 / 0.51 | yes |
| `"git"` | word | 33236 | 15.54 / 15.98 / 16.67 | 15.52 / 16.63 / 16.82 | 0.05 / 0.06 / 0.07 | 0.05 / 0.05 / 0.06 | yes |
| `"cargo"` | word | 108126 | 52.25 / 54.93 / 58.67 | 52.47 / 54.02 / 55.24 | 0.04 / 0.06 / 0.12 | 0.03 / 0.04 / 0.07 | yes |
| `"docker"` | word | 33352 | 16.24 / 16.67 / 17.48 | 16.14 / 17.01 / 17.18 | 0.04 / 0.06 / 0.07 | 0.03 / 0.04 / 0.05 | yes |
| `"git" "c"*` | two-word | 13272 | 42.76 / 43.43 / 43.65 | 43.56 / 44.84 / 44.97 | 15.71 / 16.12 / 16.20 | 15.37 / 15.92 / 16.00 | yes |
| `"git" "co"*` | two-word | 6622 | 7.34 / 7.69 / 7.80 | 7.40 / 7.78 / 7.95 | 0.15 / 0.15 / 0.15 | 0.09 / 0.09 / 0.10 | yes |
| `"git" "com"*` | two-word | 1842 | 1.51 / 1.61 / 1.63 | 1.69 / 2.14 / 3.40 | 0.10 / 0.10 / 0.10 | 0.07 / 0.07 / 0.07 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.02 / 0.04 | 0.02 / 0.03 / 0.03 | 0.03 / 0.03 / 0.04 | 0.03 / 0.03 / 0.03 | yes |
| `"cargo" "re"*` | two-word | 17736 | 17.31 / 17.96 / 18.47 | 17.41 / 23.54 / 25.71 | 0.14 / 0.14 / 0.15 | 0.08 / 0.09 / 0.09 | yes |
| `"cargo" "com"*` | sparse AND | 148 | 1.96 / 2.05 / 2.13 | 1.98 / 2.47 / 2.76 | 0.83 / 0.86 / 0.87 | 0.38 / 0.38 / 0.39 | yes |
| `"docker" "git"*` | sparse AND | 0 | 0.81 / 0.93 / 0.94 | 0.80 / 0.90 / 0.98 | 1.14 / 1.17 / 1.18 | 1.14 / 1.17 / 1.19 | yes |

## V3 prefix='2 3' + V5 newest-10% rowid range

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 20046 | 21.24 / 21.78 / 21.99 | 21.24 / 21.76 / 21.92 | 5.06 / 5.19 / 5.34 | 5.13 / 5.28 / 5.31 | yes |
| `"c"*` | 1-char | 47664 | 57.11 / 58.42 / 58.79 | 56.81 / 58.58 / 60.51 | 15.43 / 15.95 / 16.00 | 15.27 / 15.59 / 15.77 | yes |
| `"d"*` | 1-char | 22970 | 24.67 / 25.07 / 25.29 | 24.64 / 25.02 / 25.32 | 6.01 / 6.11 / 6.15 | 5.97 / 6.71 / 9.46 | yes |
| `"gi"*` | 2-char | 3353 | 1.85 / 2.02 / 2.08 | 1.84 / 2.00 / 2.03 | 0.04 / 0.04 / 0.05 | 0.03 / 0.03 / 0.03 | yes |
| `"ca"*` | 2-char | 18895 | 10.65 / 10.93 / 11.24 | 10.78 / 11.11 / 12.07 | 0.05 / 0.05 / 0.06 | 0.04 / 0.05 / 0.08 | yes |
| `"do"*` | 2-char | 10755 | 6.13 / 6.42 / 6.98 | 6.20 / 6.52 / 6.68 | 0.05 / 0.06 / 0.07 | 0.04 / 0.05 / 0.05 | yes |
| `"ku"*` | 2-char | 3280 | 1.89 / 2.20 / 2.31 | 1.92 / 2.01 / 2.24 | 0.04 / 0.05 / 0.08 | 0.03 / 0.04 / 0.04 | yes |
| `"git"*` | 3-char | 3353 | 2.02 / 2.19 / 2.29 | 1.96 / 2.11 / 2.17 | 0.05 / 0.06 / 0.06 | 0.04 / 0.05 / 0.05 | yes |
| `"car"*` | 3-char | 10794 | 5.94 / 6.06 / 6.13 | 6.06 / 6.33 / 6.34 | 0.06 / 0.07 / 0.09 | 0.05 / 0.06 / 0.07 | yes |
| `"doc"*` | 3-char | 10755 | 5.94 / 6.08 / 6.35 | 5.82 / 6.15 / 6.19 | 0.05 / 0.06 / 0.08 | 0.05 / 0.05 / 0.07 | yes |
| `"cargo"*` | word* | 10794 | 8.63 / 9.04 / 9.11 | 8.91 / 9.18 / 9.27 | 1.47 / 1.56 / 1.76 | 1.45 / 1.58 / 1.64 | yes |
| `"docker"*` | word* | 3292 | 2.58 / 2.63 / 2.67 | 2.57 / 2.67 / 2.70 | 0.46 / 0.53 / 0.53 | 0.46 / 0.51 / 0.52 | yes |
| `"git"` | word | 3353 | 1.89 / 1.96 / 1.99 | 1.87 / 1.91 / 1.91 | 0.05 / 0.07 / 0.08 | 0.05 / 0.06 / 0.06 | yes |
| `"cargo"` | word | 10794 | 6.10 / 6.38 / 6.56 | 6.08 / 6.31 / 6.39 | 0.04 / 0.05 / 0.05 | 0.03 / 0.03 / 0.04 | yes |
| `"docker"` | word | 3292 | 1.86 / 1.92 / 2.00 | 1.85 / 1.93 / 1.98 | 0.05 / 0.06 / 0.06 | 0.03 / 0.03 / 0.04 | yes |
| `"git" "c"*` | two-word | 1397 | 36.90 / 37.76 / 37.98 | 36.87 / 38.86 / 42.18 | 15.50 / 16.22 / 16.87 | 15.57 / 15.93 / 15.99 | yes |
| `"git" "co"*` | two-word | 718 | 3.11 / 3.25 / 3.25 | 3.10 / 3.16 / 3.21 | 0.15 / 0.15 / 0.16 | 0.09 / 0.10 / 0.10 | yes |
| `"git" "com"*` | two-word | 184 | 0.63 / 0.68 / 0.70 | 0.63 / 0.68 / 0.70 | 0.10 / 0.10 / 0.10 | 0.07 / 0.07 / 0.07 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.02 / 0.04 | 0.02 / 0.02 / 0.03 | 0.03 / 0.04 / 0.04 | 0.03 / 0.04 / 0.04 | yes |
| `"cargo" "re"*` | two-word | 1754 | 5.53 / 5.68 / 5.80 | 5.33 / 5.54 / 5.58 | 0.14 / 0.15 / 0.15 | 0.09 / 0.09 / 0.09 | yes |
| `"cargo" "com"*` | sparse AND | 15 | 1.41 / 1.44 / 1.47 | 1.43 / 1.47 / 1.50 | 0.17 / 0.19 / 0.20 | 0.18 / 0.19 / 0.19 | yes |
| `"docker" "git"*` | sparse AND | 0 | 0.76 / 0.83 / 0.84 | 0.75 / 0.80 / 0.84 | 1.13 / 1.19 / 1.21 | 1.13 / 1.16 / 1.18 | yes |

## V4 detail=column

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 199756 | 192.87 / 199.30 / 209.48 | 191.95 / 198.95 / 202.21 | 5.16 / 5.49 / 5.58 | 5.24 / 5.52 / 6.66 | yes |
| `"c"*` | 1-char | 475508 | 446.44 / 458.09 / 485.15 | 445.20 / 471.47 / 478.60 | 17.26 / 17.90 / 18.01 | 17.00 / 17.66 / 17.96 | yes |
| `"d"*` | 1-char | 231251 | 219.48 / 224.90 / 229.07 | 222.46 / 230.04 / 235.45 | 6.37 / 6.70 / 6.73 | 6.32 / 6.63 / 6.70 | yes |
| `"gi"*` | 2-char | 33236 | 41.68 / 45.46 / 46.05 | 41.21 / 42.28 / 42.55 | 0.47 / 0.49 / 0.50 | 0.46 / 0.49 / 0.49 | yes |
| `"ca"*` | 2-char | 188532 | 190.47 / 196.60 / 279.33 | 188.33 / 194.61 / 199.89 | 4.77 / 4.99 / 6.25 | 4.88 / 5.12 / 5.40 | yes |
| `"do"*` | 2-char | 107715 | 111.65 / 116.75 / 116.98 | 111.08 / 116.28 / 116.75 | 2.34 / 2.42 / 2.44 | 2.26 / 2.37 / 2.43 | yes |
| `"ku"*` | 2-char | 33417 | 42.87 / 44.31 / 44.71 | 42.95 / 44.04 / 46.49 | 0.46 / 0.50 / 0.60 | 0.46 / 0.48 / 0.54 | yes |
| `"git"*` | 3-char | 33236 | 41.56 / 42.61 / 42.97 | 41.87 / 43.71 / 44.03 | 0.48 / 0.52 / 0.54 | 0.48 / 0.53 / 0.58 | yes |
| `"car"*` | 3-char | 108126 | 109.18 / 112.61 / 114.13 | 109.40 / 116.26 / 119.73 | 1.48 / 1.55 / 1.57 | 1.47 / 1.55 / 1.57 | yes |
| `"doc"*` | 3-char | 107715 | 114.61 / 116.94 / 119.17 | 112.82 / 115.29 / 119.29 | 2.33 / 2.52 / 2.55 | 2.39 / 2.47 / 2.57 | yes |
| `"cargo"*` | word* | 108126 | 110.56 / 121.51 / 142.74 | 108.35 / 110.35 / 110.78 | 1.43 / 1.50 / 1.56 | 1.40 / 1.45 / 1.48 | yes |
| `"docker"*` | word* | 33352 | 42.72 / 45.37 / 45.75 | 42.69 / 43.57 / 44.29 | 0.46 / 0.48 / 0.49 | 0.45 / 0.47 / 0.48 | yes |
| `"git"` | word | 33236 | 41.23 / 42.59 / 42.93 | 40.72 / 41.75 / 42.35 | 0.03 / 0.03 / 0.04 | 0.02 / 0.03 / 0.03 | yes |
| `"cargo"` | word | 108126 | 104.67 / 107.17 / 107.51 | 104.87 / 107.16 / 108.76 | 0.04 / 0.04 / 0.04 | 0.03 / 0.03 / 0.03 | yes |
| `"docker"` | word | 33352 | 41.79 / 43.41 / 44.48 | 41.92 / 44.63 / 46.05 | 0.03 / 0.03 / 0.03 | 0.02 / 0.02 / 0.03 | yes |
| `"git" "c"*` | two-word | 13272 | 60.67 / 63.81 / 63.96 | 59.70 / 61.04 / 63.68 | 17.40 / 17.91 / 17.97 | 17.27 / 17.67 / 17.88 | yes |
| `"git" "co"*` | two-word | 6622 | 22.12 / 24.01 / 24.63 | 23.89 / 25.49 / 26.09 | 3.84 / 4.03 / 4.09 | 3.93 / 4.08 / 4.18 | yes |
| `"git" "com"*` | two-word | 1842 | 2.90 / 3.27 / 3.50 | 2.72 / 3.30 / 3.43 | 0.12 / 0.14 / 0.14 | 0.08 / 0.10 / 0.14 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.02 / 0.03 | 0.02 / 0.02 / 0.02 | 0.03 / 0.04 / 0.05 | 0.03 / 0.04 / 0.06 | yes |
| `"cargo" "re"*` | two-word | 17736 | 49.85 / 51.59 / 52.49 | 49.79 / 51.15 / 51.40 | 7.18 / 7.37 / 7.42 | 7.15 / 7.48 / 7.49 | yes |
| `"cargo" "com"*` | sparse AND | 148 | 1.98 / 2.10 / 2.12 | 1.96 / 2.07 / 2.10 | 0.81 / 0.90 / 0.93 | 0.34 / 0.38 / 0.41 | yes |
| `"docker" "git"*` | sparse AND | 0 | 1.26 / 1.34 / 1.36 | 1.26 / 1.34 / 1.39 | 1.64 / 1.72 / 1.74 | 1.60 / 1.65 / 1.69 | yes |

## V4 detail=column + V5 newest-10% rowid range

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 20046 | 30.19 / 31.61 / 32.52 | 30.18 / 31.06 / 31.54 | 5.09 / 5.27 / 5.41 | 5.12 / 5.44 / 5.53 | yes |
| `"c"*` | 1-char | 47664 | 80.19 / 81.25 / 81.63 | 78.76 / 80.55 / 81.50 | 16.92 / 17.30 / 17.78 | 16.78 / 17.59 / 17.97 | yes |
| `"d"*` | 1-char | 22970 | 35.20 / 35.96 / 36.32 | 34.45 / 35.80 / 37.22 | 6.30 / 6.37 / 6.52 | 6.31 / 6.61 / 6.67 | yes |
| `"gi"*` | 2-char | 3353 | 4.28 / 4.77 / 4.92 | 4.16 / 4.31 / 4.41 | 0.46 / 0.48 / 0.49 | 0.46 / 0.47 / 0.48 | yes |
| `"ca"*` | 2-char | 18895 | 28.31 / 29.19 / 29.38 | 26.76 / 27.85 / 28.07 | 4.69 / 4.81 / 4.85 | 4.76 / 5.04 / 5.09 | yes |
| `"do"*` | 2-char | 10755 | 14.14 / 14.82 / 15.15 | 14.08 / 14.42 / 14.63 | 2.35 / 2.49 / 2.60 | 2.24 / 2.34 / 2.36 | yes |
| `"ku"*` | 2-char | 3280 | 3.99 / 4.66 / 5.57 | 3.86 / 3.97 / 4.16 | 0.47 / 0.48 / 0.49 | 0.45 / 0.46 / 0.47 | yes |
| `"git"*` | 3-char | 3353 | 3.80 / 3.89 / 3.92 | 3.84 / 3.91 / 3.96 | 0.46 / 0.47 / 0.47 | 0.45 / 0.48 / 0.49 | yes |
| `"car"*` | 3-char | 10794 | 12.46 / 13.98 / 14.27 | 12.63 / 14.03 / 14.73 | 1.40 / 1.47 / 1.49 | 1.40 / 1.43 / 1.43 | yes |
| `"doc"*` | 3-char | 10755 | 14.92 / 15.63 / 15.71 | 15.60 / 16.32 / 17.47 | 2.27 / 2.33 / 2.37 | 2.25 / 2.38 / 2.49 | yes |
| `"cargo"*` | word* | 10794 | 13.70 / 14.41 / 14.63 | 13.06 / 14.32 / 15.95 | 1.45 / 1.54 / 1.57 | 1.38 / 1.44 / 1.49 | yes |
| `"docker"*` | word* | 3292 | 4.09 / 4.25 / 4.38 | 4.80 / 5.03 / 5.09 | 0.46 / 0.47 / 0.48 | 0.45 / 0.46 / 0.47 | yes |
| `"git"` | word | 3353 | 3.81 / 4.10 / 4.29 | 3.90 / 4.09 / 4.29 | 0.03 / 0.04 / 0.04 | 0.03 / 0.03 / 0.03 | yes |
| `"cargo"` | word | 10794 | 11.45 / 11.80 / 12.00 | 10.84 / 11.72 / 12.51 | 0.04 / 0.05 / 0.05 | 0.03 / 0.03 / 0.03 | yes |
| `"docker"` | word | 3292 | 3.95 / 4.21 / 4.25 | 3.90 / 4.21 / 4.29 | 0.03 / 0.03 / 0.04 | 0.02 / 0.03 / 0.03 | yes |
| `"git" "c"*` | two-word | 1397 | 40.72 / 41.95 / 90.72 | 41.06 / 42.27 / 42.53 | 17.05 / 17.61 / 18.84 | 17.22 / 17.79 / 17.86 | yes |
| `"git" "co"*` | two-word | 718 | 11.01 / 11.35 / 12.94 | 10.81 / 11.21 / 11.29 | 3.97 / 4.17 / 4.36 | 3.80 / 4.02 / 5.37 | yes |
| `"git" "com"*` | two-word | 184 | 0.70 / 0.73 / 0.76 | 0.69 / 0.72 / 0.74 | 0.12 / 0.12 / 0.13 | 0.08 / 0.09 / 0.10 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.02 / 0.02 | 0.02 / 0.02 / 0.02 | 0.03 / 0.04 / 0.04 | 0.03 / 0.03 / 0.03 | yes |
| `"cargo" "re"*` | two-word | 1754 | 20.22 / 20.97 / 21.09 | 20.32 / 21.36 / 21.50 | 7.00 / 7.16 / 7.27 | 6.99 / 7.21 / 7.36 | yes |
| `"cargo" "com"*` | sparse AND | 15 | 1.50 / 1.65 / 2.33 | 1.42 / 1.65 / 1.72 | 0.15 / 0.17 / 0.18 | 0.15 / 0.16 / 0.16 | yes |
| `"docker" "git"*` | sparse AND | 0 | 1.23 / 1.27 / 1.30 | 1.20 / 1.23 / 1.26 | 1.66 / 1.77 / 1.82 | 1.66 / 1.77 / 1.92 | yes |

## V4 detail=none

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 199756 | 200.99 / 210.96 / 211.67 | 196.79 / 204.44 / 206.66 | 11.35 / 11.51 / 11.57 | 11.32 / 11.50 / 11.60 | yes |
| `"c"*` | 1-char | 475508 | 453.27 / 580.61 / 618.87 | 480.02 / 505.18 / 545.65 | 30.57 / 30.91 / 31.02 | 30.55 / 31.00 / 31.32 | yes |
| `"d"*` | 1-char | 231251 | 237.85 / 242.44 / 257.27 | 237.05 / 241.59 / 245.77 | 13.44 / 13.66 / 14.01 | 13.44 / 13.88 / 13.96 | yes |
| `"gi"*` | 2-char | 33236 | 46.66 / 48.99 / 49.45 | 43.84 / 48.51 / 49.60 | 1.87 / 1.94 / 1.94 | 1.87 / 1.93 / 1.95 | yes |
| `"ca"*` | 2-char | 188532 | 189.77 / 200.13 / 201.97 | 188.10 / 196.16 / 197.95 | 10.94 / 11.13 / 12.10 | 10.54 / 10.90 / 11.03 | yes |
| `"do"*` | 2-char | 107715 | 112.96 / 117.81 / 118.87 | 112.46 / 114.54 / 116.61 | 5.76 / 5.94 / 6.19 | 5.78 / 5.86 / 5.96 | yes |
| `"ku"*` | 2-char | 33417 | 43.94 / 45.15 / 45.79 | 44.00 / 45.42 / 46.96 | 1.81 / 1.84 / 1.91 | 1.80 / 1.83 / 1.84 | yes |
| `"git"*` | 3-char | 33236 | 42.58 / 44.13 / 45.22 | 42.43 / 45.02 / 45.75 | 1.81 / 1.87 / 1.90 | 1.80 / 1.85 / 1.88 | yes |
| `"car"*` | 3-char | 108126 | 111.44 / 113.52 / 116.89 | 111.93 / 114.45 / 115.09 | 5.52 / 5.63 / 5.68 | 5.53 / 5.70 / 5.72 | yes |
| `"doc"*` | 3-char | 107715 | 112.35 / 114.46 / 115.80 | 139.11 / 195.73 / 209.09 | 5.80 / 5.97 / 6.00 | 5.78 / 5.90 / 5.95 | yes |
| `"cargo"*` | word* | 108126 | 118.89 / 124.58 / 126.29 | 111.83 / 119.03 / 121.98 | 5.54 / 5.75 / 5.93 | 5.53 / 5.71 / 5.75 | yes |
| `"docker"*` | word* | 33352 | 43.45 / 46.90 / 48.99 | 43.38 / 45.30 / 45.77 | 1.82 / 1.91 / 1.96 | 1.82 / 1.87 / 1.89 | yes |
| `"git"` | word | 33236 | 38.99 / 40.40 / 40.75 | 39.55 / 41.79 / 42.98 | 0.03 / 0.03 / 0.04 | 0.02 / 0.02 / 0.02 | yes |
| `"cargo"` | word | 108126 | 100.25 / 102.30 / 103.13 | 100.30 / 101.81 / 103.35 | 0.05 / 0.05 / 0.05 | 0.04 / 0.04 / 0.08 | yes |
| `"docker"` | word | 33352 | 39.84 / 40.79 / 41.52 | 39.96 / 41.62 / 42.00 | 0.08 / 0.09 / 0.09 | 0.08 / 0.09 / 0.09 | yes |
| `"git" "c"*` | two-word | 13272 | 86.93 / 87.86 / 88.21 | 86.78 / 87.94 / 88.33 | 30.85 / 31.37 / 31.74 | 30.68 / 31.03 / 31.45 | yes |
| `"git" "co"*` | two-word | 6622 | 33.47 / 34.07 / 34.18 | 33.53 / 34.24 / 34.68 | 9.84 / 10.17 / 10.30 | 9.91 / 10.46 / 11.23 | yes |
| `"git" "com"*` | two-word | 1842 | 2.59 / 2.81 / 2.85 | 2.59 / 2.73 / 2.75 | 0.22 / 0.23 / 0.24 | 0.20 / 0.20 / 0.21 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.02 / 0.03 | 0.02 / 0.02 / 0.02 | 0.05 / 0.05 / 0.06 | 0.05 / 0.05 / 0.05 | yes |
| `"cargo" "re"*` | two-word | 17736 | 62.01 / 63.10 / 64.37 | 61.80 / 62.82 / 63.07 | 15.27 / 15.54 / 15.67 | 15.18 / 15.51 / 15.65 | yes |
| `"cargo" "com"*` | sparse AND | 148 | 1.97 / 2.03 / 2.16 | 1.99 / 2.03 / 2.06 | 0.75 / 0.77 / 0.78 | 0.42 / 0.45 / 0.46 | yes |
| `"docker" "git"*` | sparse AND | 0 | 2.54 / 2.61 / 2.71 | 2.51 / 2.55 / 2.66 | 3.02 / 3.15 / 3.21 | 3.02 / 3.15 / 3.22 | yes |

## V4 detail=none + V5 newest-10% rowid range

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 20046 | 40.39 / 40.96 / 43.59 | 40.37 / 41.05 / 41.81 | 11.37 / 11.72 / 11.73 | 11.39 / 11.65 / 11.78 | yes |
| `"c"*` | 1-char | 47664 | 101.97 / 104.22 / 133.72 | 101.93 / 103.78 / 103.98 | 30.63 / 31.32 / 31.54 | 30.65 / 31.16 / 31.24 | yes |
| `"d"*` | 1-char | 22970 | 46.97 / 47.59 / 47.64 | 46.80 / 47.79 / 48.64 | 13.48 / 13.83 / 13.89 | 13.47 / 13.71 / 13.91 | yes |
| `"gi"*` | 2-char | 3353 | 6.39 / 6.69 / 6.79 | 6.43 / 6.68 / 7.01 | 1.82 / 1.85 / 1.93 | 1.81 / 1.92 / 1.93 | yes |
| `"ca"*` | 2-char | 18895 | 37.35 / 37.99 / 38.05 | 37.27 / 38.01 / 38.30 | 10.54 / 10.93 / 11.02 | 10.56 / 10.88 / 10.93 | yes |
| `"do"*` | 2-char | 10755 | 21.10 / 22.01 / 22.99 | 20.74 / 21.55 / 21.59 | 5.76 / 5.90 / 6.02 | 5.77 / 6.00 / 6.01 | yes |
| `"ku"*` | 2-char | 3280 | 6.49 / 6.75 / 6.94 | 6.50 / 6.82 / 6.96 | 1.81 / 1.88 / 1.92 | 1.81 / 1.89 / 1.92 | yes |
| `"git"*` | 3-char | 3353 | 6.41 / 6.65 / 6.93 | 6.38 / 6.58 / 7.06 | 1.82 / 1.91 / 1.94 | 1.80 / 1.87 / 1.88 | yes |
| `"car"*` | 3-char | 10794 | 20.29 / 20.97 / 21.16 | 20.31 / 21.35 / 21.87 | 5.58 / 5.72 / 5.78 | 5.52 / 5.70 / 5.78 | yes |
| `"doc"*` | 3-char | 10755 | 20.77 / 21.43 / 21.57 | 20.74 / 21.47 / 21.60 | 5.78 / 6.01 / 6.03 | 5.77 / 5.93 / 5.99 | yes |
| `"cargo"*` | word* | 10794 | 20.08 / 20.98 / 21.46 | 20.05 / 21.04 / 21.09 | 5.55 / 5.72 / 5.79 | 5.53 / 5.69 / 5.82 | yes |
| `"docker"*` | word* | 3292 | 6.44 / 6.67 / 6.75 | 6.45 / 6.80 / 6.94 | 1.82 / 1.90 / 1.91 | 1.80 / 1.83 / 1.93 | yes |
| `"git"` | word | 3353 | 3.07 / 3.19 / 3.40 | 3.04 / 3.20 / 3.22 | 0.03 / 0.03 / 0.03 | 0.02 / 0.02 / 0.03 | yes |
| `"cargo"` | word | 10794 | 9.27 / 9.99 / 10.10 | 9.15 / 9.52 / 9.69 | 0.05 / 0.06 / 0.06 | 0.04 / 0.05 / 0.05 | yes |
| `"docker"` | word | 3292 | 3.08 / 3.22 / 3.32 | 3.09 / 3.19 / 3.19 | 0.08 / 0.10 / 0.13 | 0.08 / 0.08 / 0.09 | yes |
| `"git" "c"*` | two-word | 1397 | 67.39 / 68.37 / 69.54 | 67.32 / 67.68 / 68.15 | 30.87 / 31.25 / 31.48 | 30.83 / 31.37 / 31.47 | yes |
| `"git" "co"*` | two-word | 718 | 22.34 / 22.90 / 23.19 | 22.25 / 22.93 / 23.51 | 9.87 / 10.22 / 10.33 | 9.82 / 10.02 / 10.07 | yes |
| `"git" "com"*` | two-word | 184 | 1.06 / 1.10 / 1.13 | 1.06 / 1.08 / 1.10 | 0.23 / 0.24 / 0.25 | 0.21 / 0.22 / 0.23 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.02 / 0.02 | 0.02 / 0.02 / 0.02 | 0.05 / 0.05 / 0.06 | 0.05 / 0.06 / 0.06 | yes |
| `"cargo" "re"*` | two-word | 1754 | 35.43 / 36.02 / 36.06 | 35.44 / 36.25 / 36.32 | 15.21 / 15.58 / 15.71 | 15.20 / 15.56 / 15.60 | yes |
| `"cargo" "com"*` | sparse AND | 15 | 1.56 / 1.59 / 1.59 | 1.56 / 1.58 / 1.61 | 0.26 / 0.28 / 0.28 | 0.26 / 0.27 / 0.28 | yes |
| `"docker" "git"*` | sparse AND | 0 | 2.50 / 2.54 / 2.62 | 2.50 / 2.55 / 2.56 | 2.98 / 3.06 / 3.11 | 3.02 / 3.17 / 3.19 | yes |

## V3+V4 prefix='1' detail=none

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 199756 | 177.38 / 179.99 / 181.57 | 182.64 / 189.89 / 191.88 | 0.08 / 0.09 / 0.09 | 0.08 / 0.08 / 0.09 | yes |
| `"c"*` | 1-char | 475508 | 410.64 / 436.67 / 441.91 | 407.14 / 428.09 / 433.13 | 0.10 / 0.12 / 0.12 | 0.09 / 0.11 / 0.13 | yes |
| `"d"*` | 1-char | 231251 | 206.85 / 211.45 / 213.22 | 202.40 / 212.07 / 216.14 | 0.10 / 0.13 / 0.15 | 0.08 / 0.10 / 0.11 | yes |
| `"gi"*` | 2-char | 33236 | 42.50 / 43.88 / 43.97 | 42.44 / 42.87 / 43.13 | 1.88 / 1.94 / 1.97 | 1.90 / 1.96 / 1.98 | yes |
| `"ca"*` | 2-char | 188532 | 189.60 / 199.24 / 204.92 | 195.14 / 204.26 / 204.65 | 10.97 / 11.24 / 11.36 | 10.56 / 11.18 / 11.32 | yes |
| `"do"*` | 2-char | 107715 | 116.24 / 121.49 / 122.15 | 115.79 / 121.67 / 121.85 | 6.02 / 6.26 / 6.28 | 5.93 / 6.23 / 6.31 | yes |
| `"ku"*` | 2-char | 33417 | 46.04 / 47.76 / 48.49 | 45.74 / 47.84 / 48.93 | 1.90 / 2.01 / 2.05 | 1.81 / 1.85 / 1.87 | yes |
| `"git"*` | 3-char | 33236 | 46.24 / 48.42 / 48.94 | 46.03 / 48.08 / 48.73 | 1.83 / 1.89 / 1.93 | 1.81 / 1.83 / 1.88 | yes |
| `"car"*` | 3-char | 108126 | 115.11 / 121.90 / 123.00 | 116.16 / 119.30 / 119.77 | 5.52 / 5.74 / 5.97 | 5.89 / 6.03 / 6.06 | yes |
| `"doc"*` | 3-char | 107715 | 116.66 / 121.27 / 122.55 | 120.74 / 123.57 / 124.14 | 6.11 / 6.28 / 6.43 | 5.76 / 6.16 / 6.19 | yes |
| `"cargo"*` | word* | 108126 | 113.96 / 118.25 / 118.92 | 114.70 / 118.57 / 120.88 | 5.76 / 5.90 / 6.00 | 5.75 / 5.98 / 6.03 | yes |
| `"docker"*` | word* | 33352 | 46.57 / 48.50 / 48.95 | 44.71 / 47.83 / 48.48 | 1.88 / 1.99 / 1.99 | 1.88 / 1.97 / 2.01 | yes |
| `"git"` | word | 33236 | 40.25 / 44.96 / 46.14 | 39.00 / 39.77 / 40.36 | 0.04 / 0.05 / 0.06 | 0.03 / 0.04 / 0.04 | yes |
| `"cargo"` | word | 108126 | 98.38 / 100.07 / 100.61 | 101.67 / 107.06 / 107.86 | 0.07 / 0.08 / 0.14 | 0.06 / 0.08 / 0.17 | yes |
| `"docker"` | word | 33352 | 41.37 / 44.37 / 44.63 | 39.40 / 40.38 / 42.24 | 0.06 / 0.07 / 0.14 | 0.05 / 0.07 / 0.08 | yes |
| `"git" "c"*` | two-word | 13272 | 27.02 / 27.47 / 28.29 | 26.93 / 27.62 / 28.39 | 0.18 / 0.21 / 0.21 | 0.13 / 0.15 / 0.17 | yes |
| `"git" "co"*` | two-word | 6622 | 32.90 / 33.62 / 33.94 | 32.89 / 33.56 / 33.71 | 10.25 / 10.48 / 11.27 | 10.17 / 10.31 / 10.36 | yes |
| `"git" "com"*` | two-word | 1842 | 2.50 / 2.64 / 2.79 | 2.50 / 2.57 / 2.66 | 0.24 / 0.26 / 0.35 | 0.21 / 0.27 / 0.35 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.02 / 0.02 | 0.02 / 0.02 / 0.02 | 0.07 / 0.07 / 0.09 | 0.07 / 0.07 / 0.08 | yes |
| `"cargo" "re"*` | two-word | 17736 | 61.13 / 63.44 / 66.18 | 61.29 / 64.64 / 65.61 | 15.59 / 15.95 / 16.04 | 15.79 / 16.26 / 16.44 | yes |
| `"cargo" "com"*` | sparse AND | 148 | 2.10 / 2.16 / 2.17 | 2.13 / 2.18 / 2.21 | 0.91 / 0.93 / 0.94 | 0.55 / 0.59 / 0.59 | yes |
| `"docker" "git"*` | sparse AND | 0 | 2.49 / 2.53 / 2.57 | 2.49 / 2.51 / 2.53 | 3.14 / 3.25 / 3.27 | 3.06 / 3.20 / 3.27 | yes |

## V3+V4 prefix='1' detail=none + V5 newest-10% rowid range

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 20046 | 17.49 / 17.96 / 18.20 | 17.73 / 19.85 / 20.01 | 0.08 / 0.09 / 0.09 | 0.08 / 0.08 / 0.08 | yes |
| `"c"*` | 1-char | 47664 | 42.46 / 44.93 / 45.54 | 43.32 / 45.86 / 46.75 | 0.10 / 0.10 / 0.10 | 0.09 / 0.09 / 0.09 | yes |
| `"d"*` | 1-char | 22970 | 21.28 / 22.41 / 23.24 | 20.75 / 22.07 / 22.16 | 0.10 / 0.11 / 0.11 | 0.08 / 0.08 / 0.08 | yes |
| `"gi"*` | 2-char | 3353 | 6.49 / 7.13 / 7.42 | 6.65 / 7.28 / 7.56 | 1.83 / 2.10 / 2.26 | 1.93 / 2.01 / 2.04 | yes |
| `"ca"*` | 2-char | 18895 | 38.45 / 39.62 / 39.82 | 39.21 / 40.17 / 40.21 | 11.04 / 11.29 / 11.34 | 10.52 / 10.81 / 10.93 | yes |
| `"do"*` | 2-char | 10755 | 21.72 / 22.72 / 23.76 | 20.70 / 21.66 / 21.98 | 5.75 / 5.92 / 6.04 | 5.75 / 5.90 / 5.97 | yes |
| `"ku"*` | 2-char | 3280 | 6.61 / 6.96 / 7.14 | 6.48 / 6.62 / 6.71 | 1.81 / 1.90 / 1.91 | 1.80 / 1.84 / 1.99 | yes |
| `"git"*` | 3-char | 3353 | 6.46 / 6.63 / 6.82 | 6.47 / 6.88 / 7.05 | 1.80 / 1.89 / 1.94 | 1.81 / 1.86 / 1.91 | yes |
| `"car"*` | 3-char | 10794 | 20.41 / 22.38 / 23.14 | 20.76 / 22.30 / 23.55 | 5.56 / 5.72 / 5.77 | 5.51 / 5.68 / 5.69 | yes |
| `"doc"*` | 3-char | 10755 | 21.25 / 22.74 / 22.77 | 20.99 / 22.62 / 22.66 | 5.88 / 6.04 / 6.16 | 5.95 / 6.15 / 6.20 | yes |
| `"cargo"*` | word* | 10794 | 20.13 / 22.03 / 22.33 | 20.20 / 21.91 / 21.94 | 5.52 / 5.55 / 5.66 | 5.50 / 5.56 / 5.58 | yes |
| `"docker"*` | word* | 3292 | 7.28 / 7.57 / 7.67 | 7.22 / 7.48 / 7.56 | 1.81 / 1.94 / 1.96 | 1.80 / 1.83 / 1.85 | yes |
| `"git"` | word | 3353 | 3.08 / 3.53 / 3.68 | 3.32 / 3.56 / 5.63 | 0.04 / 0.04 / 0.04 | 0.03 / 0.03 / 0.04 | yes |
| `"cargo"` | word | 10794 | 9.41 / 10.68 / 10.85 | 9.46 / 9.62 / 9.65 | 0.07 / 0.07 / 0.07 | 0.06 / 0.06 / 0.07 | yes |
| `"docker"` | word | 3292 | 2.92 / 3.01 / 3.09 | 2.95 / 3.09 / 3.22 | 0.06 / 0.06 / 0.07 | 0.05 / 0.05 / 0.06 | yes |
| `"git" "c"*` | two-word | 1397 | 7.10 / 7.23 / 7.52 | 7.57 / 8.05 / 8.70 | 0.18 / 0.19 / 0.19 | 0.13 / 0.13 / 0.13 | yes |
| `"git" "co"*` | two-word | 718 | 22.07 / 23.86 / 23.97 | 22.54 / 23.53 / 23.75 | 10.00 / 10.33 / 10.42 | 9.89 / 10.12 / 10.20 | yes |
| `"git" "com"*` | two-word | 184 | 1.01 / 1.09 / 1.16 | 0.99 / 1.07 / 1.10 | 0.24 / 0.25 / 0.26 | 0.19 / 0.19 / 0.21 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.02 / 0.03 | 0.02 / 0.03 / 0.03 | 0.07 / 0.07 / 0.07 | 0.07 / 0.07 / 0.07 | yes |
| `"cargo" "re"*` | two-word | 1754 | 36.99 / 38.18 / 40.64 | 36.76 / 37.95 / 38.18 | 15.65 / 16.40 / 16.51 | 15.39 / 16.09 / 16.23 | yes |
| `"cargo" "com"*` | sparse AND | 15 | 1.83 / 1.87 / 1.87 | 1.81 / 1.88 / 1.91 | 0.35 / 0.41 / 0.45 | 0.36 / 0.39 / 0.51 | yes |
| `"docker" "git"*` | sparse AND | 0 | 2.64 / 2.72 / 2.88 | 2.61 / 2.71 / 2.72 | 3.17 / 3.32 / 3.62 | 3.06 / 3.16 / 3.18 | yes |

## V3+V4 prefix='1 2 3' detail=none

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 199756 | 177.58 / 191.66 / 192.57 | 178.63 / 187.65 / 189.88 | 0.07 / 0.07 / 0.08 | 0.06 / 0.06 / 0.07 | yes |
| `"c"*` | 1-char | 475508 | 406.72 / 436.43 / 440.23 | 395.98 / 419.62 / 424.70 | 0.05 / 0.05 / 0.05 | 0.04 / 0.04 / 0.05 | yes |
| `"d"*` | 1-char | 231251 | 200.12 / 212.44 / 215.05 | 201.27 / 216.38 / 221.24 | 0.07 / 0.07 / 0.07 | 0.06 / 0.06 / 0.07 | yes |
| `"gi"*` | 2-char | 33236 | 40.02 / 43.29 / 43.32 | 39.93 / 43.29 / 43.44 | 0.07 / 0.07 / 0.08 | 0.07 / 0.07 / 0.07 | yes |
| `"ca"*` | 2-char | 188532 | 174.51 / 181.85 / 183.32 | 165.62 / 171.96 / 180.46 | 0.05 / 0.05 / 0.05 | 0.05 / 0.05 / 0.05 | yes |
| `"do"*` | 2-char | 107715 | 101.06 / 102.24 / 102.72 | 101.05 / 103.85 / 104.07 | 0.07 / 0.07 / 0.07 | 0.06 / 0.06 / 0.06 | yes |
| `"ku"*` | 2-char | 33417 | 39.88 / 40.47 / 40.51 | 42.14 / 45.18 / 53.92 | 0.07 / 0.08 / 0.08 | 0.07 / 0.08 / 0.11 | yes |
| `"git"*` | 3-char | 33236 | 39.33 / 42.89 / 43.35 | 38.84 / 40.43 / 40.88 | 0.08 / 0.08 / 0.08 | 0.07 / 0.07 / 0.08 | yes |
| `"car"*` | 3-char | 108126 | 102.57 / 108.88 / 109.42 | 109.27 / 112.14 / 113.48 | 0.07 / 0.07 / 0.08 | 0.07 / 0.07 / 0.07 | yes |
| `"doc"*` | 3-char | 107715 | 108.76 / 111.62 / 111.83 | 104.86 / 107.46 / 108.44 | 0.09 / 0.10 / 0.10 | 0.09 / 0.09 / 0.10 | yes |
| `"cargo"*` | word* | 108126 | 115.68 / 119.93 / 121.37 | 115.44 / 119.71 / 120.41 | 5.52 / 5.58 / 5.61 | 5.50 / 5.63 / 5.69 | yes |
| `"docker"*` | word* | 33352 | 43.61 / 48.59 / 49.37 | 42.82 / 44.01 / 47.53 | 1.80 / 1.83 / 1.88 | 1.80 / 1.83 / 1.85 | yes |
| `"git"` | word | 33236 | 39.57 / 44.03 / 44.85 | 41.80 / 44.54 / 44.67 | 0.06 / 0.06 / 0.07 | 0.06 / 0.06 / 0.06 | yes |
| `"cargo"` | word | 108126 | 103.40 / 110.39 / 111.07 | 102.64 / 111.55 / 112.22 | 0.08 / 0.08 / 0.08 | 0.07 / 0.07 / 0.07 | yes |
| `"docker"` | word | 33352 | 41.07 / 45.22 / 45.34 | 44.36 / 45.52 / 46.74 | 0.05 / 0.06 / 0.06 | 0.05 / 0.05 / 0.05 | yes |
| `"git" "c"*` | two-word | 13272 | 27.86 / 31.22 / 31.36 | 31.01 / 31.65 / 31.89 | 0.17 / 0.17 / 0.17 | 0.11 / 0.11 / 0.11 | yes |
| `"git" "co"*` | two-word | 6622 | 14.19 / 15.18 / 15.50 | 14.16 / 15.64 / 17.09 | 0.18 / 0.19 / 0.19 | 0.11 / 0.11 / 0.12 | yes |
| `"git" "com"*` | two-word | 1842 | 3.30 / 3.59 / 3.65 | 2.29 / 3.37 / 3.55 | 0.11 / 0.11 / 0.11 | 0.08 / 0.08 / 0.09 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.02 / 0.02 | 0.02 / 0.02 / 0.03 | 0.07 / 0.08 / 0.08 | 0.07 / 0.08 / 0.10 | yes |
| `"cargo" "re"*` | two-word | 17736 | 32.42 / 36.69 / 38.02 | 33.42 / 36.89 / 37.64 | 0.22 / 0.22 / 0.22 | 0.17 / 0.17 / 0.17 | yes |
| `"cargo" "com"*` | sparse AND | 148 | 1.73 / 1.80 / 1.81 | 1.73 / 1.80 / 1.82 | 0.63 / 0.64 / 0.65 | 0.30 / 0.31 / 0.31 | yes |
| `"docker" "git"*` | sparse AND | 0 | 0.85 / 0.87 / 0.91 | 0.85 / 0.88 / 0.88 | 1.25 / 1.27 / 1.28 | 1.28 / 1.30 / 1.30 | yes |

## V3+V4 prefix='1 2 3' detail=none + V5 newest-10% rowid range

| query | class | matches | rank p200 | rank p50 | rowid DESC p200 | rowid DESC p50 | same rows as baseline |
|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 20046 | 17.67 / 18.51 / 18.53 | 18.02 / 18.71 / 19.22 | 0.07 / 0.07 / 0.08 | 0.06 / 0.07 / 0.07 | yes |
| `"c"*` | 1-char | 47664 | 42.61 / 45.48 / 45.70 | 41.76 / 44.39 / 45.07 | 0.05 / 0.05 / 0.05 | 0.04 / 0.04 / 0.05 | yes |
| `"d"*` | 1-char | 22970 | 21.69 / 22.50 / 22.69 | 19.99 / 21.14 / 23.59 | 0.07 / 0.07 / 0.08 | 0.06 / 0.07 / 0.07 | yes |
| `"gi"*` | 2-char | 3353 | 3.08 / 3.31 / 3.43 | 3.59 / 3.81 / 3.85 | 0.07 / 0.08 / 0.08 | 0.07 / 0.07 / 0.07 | yes |
| `"ca"*` | 2-char | 18895 | 16.56 / 18.43 / 18.55 | 16.41 / 17.15 / 17.33 | 0.05 / 0.05 / 0.06 | 0.05 / 0.05 / 0.05 | yes |
| `"do"*` | 2-char | 10755 | 9.47 / 9.76 / 9.82 | 9.59 / 9.88 / 10.00 | 0.07 / 0.07 / 0.07 | 0.06 / 0.06 / 0.06 | yes |
| `"ku"*` | 2-char | 3280 | 3.16 / 3.37 / 3.44 | 3.15 / 3.24 / 3.28 | 0.07 / 0.08 / 0.08 | 0.07 / 0.07 / 0.07 | yes |
| `"git"*` | 3-char | 3353 | 3.08 / 3.30 / 3.36 | 3.08 / 3.27 / 3.36 | 0.08 / 0.08 / 0.08 | 0.07 / 0.07 / 0.08 | yes |
| `"car"*` | 3-char | 10794 | 9.57 / 9.77 / 10.04 | 9.60 / 9.86 / 9.91 | 0.07 / 0.07 / 0.08 | 0.07 / 0.07 / 0.07 | yes |
| `"doc"*` | 3-char | 10755 | 9.93 / 11.53 / 11.64 | 11.01 / 11.49 / 11.96 | 0.09 / 0.09 / 0.10 | 0.09 / 0.09 / 0.09 | yes |
| `"cargo"*` | word* | 10794 | 21.75 / 22.81 / 23.65 | 20.70 / 22.10 / 22.58 | 5.51 / 5.54 / 5.56 | 5.48 / 5.53 / 5.57 | yes |
| `"docker"*` | word* | 3292 | 6.64 / 6.92 / 8.14 | 7.33 / 8.10 / 8.25 | 1.80 / 1.83 / 1.96 | 1.80 / 1.82 / 1.83 | yes |
| `"git"` | word | 3353 | 3.70 / 4.08 / 4.66 | 3.58 / 3.98 / 4.09 | 0.06 / 0.06 / 0.07 | 0.06 / 0.06 / 0.06 | yes |
| `"cargo"` | word | 10794 | 10.63 / 11.06 / 11.45 | 10.57 / 10.90 / 11.37 | 0.07 / 0.08 / 0.08 | 0.07 / 0.07 / 0.08 | yes |
| `"docker"` | word | 3292 | 3.78 / 4.02 / 4.13 | 3.62 / 3.90 / 3.92 | 0.06 / 0.06 / 0.06 | 0.05 / 0.05 / 0.05 | yes |
| `"git" "c"*` | two-word | 1397 | 7.85 / 8.47 / 8.58 | 7.95 / 8.34 / 8.63 | 0.16 / 0.17 / 0.17 | 0.11 / 0.11 / 0.11 | yes |
| `"git" "co"*` | two-word | 718 | 3.85 / 4.18 / 4.29 | 3.35 / 4.00 / 4.08 | 0.18 / 0.19 / 0.19 | 0.11 / 0.12 / 0.12 | yes |
| `"git" "com"*` | two-word | 184 | 0.78 / 0.83 / 0.84 | 0.77 / 0.78 / 0.79 | 0.11 / 0.11 / 0.11 | 0.08 / 0.08 / 0.09 | yes |
| `"cargo" "bu"*` | two-word | 0 | 0.02 / 0.02 / 0.02 | 0.02 / 0.02 / 0.02 | 0.07 / 0.07 / 0.08 | 0.07 / 0.07 / 0.09 | yes |
| `"cargo" "re"*` | two-word | 1754 | 5.76 / 6.06 / 6.30 | 5.74 / 5.95 / 6.02 | 0.22 / 0.22 / 0.23 | 0.17 / 0.17 / 0.18 | yes |
| `"cargo" "com"*` | sparse AND | 15 | 1.30 / 1.33 / 1.35 | 1.30 / 1.33 / 1.46 | 0.17 / 0.17 / 0.17 | 0.17 / 0.17 / 0.19 | yes |
| `"docker" "git"*` | sparse AND | 0 | 0.85 / 0.86 / 0.87 | 0.85 / 0.86 / 0.87 | 1.28 / 1.30 / 1.30 | 1.28 / 1.29 / 1.31 | yes |

## Summary: worst p95 (ms) at page 200 over the query set

| variant | order | V5 range | ratio | all queries | prefix >= 2 chars | prefix >= 3 chars | queries > 20 ms p95 (all) |
|---|---|---|---|---|---|---|---|
| V0 baseline | rank | - | 3.04x | 267.20 | 256.31 | 76.36 | `"g"*` `"c"*` `"d"*` `"ca"*` `"do"*` `"car"*` `"doc"*` `"cargo"*` `"cargo"` `"git" "c"*` `"cargo" "re"*` |
| V0 baseline | rank | newest 10% | 3.04x | 60.13 | 20.17 | 10.85 | `"g"*` `"c"*` `"d"*` `"ca"*` `"git" "c"*` |
| V0 baseline | rowid DESC | - | 3.04x | 20.24 | 7.45 | 2.42 | `"c"*` |
| V0 baseline | rowid DESC | newest 10% | 3.04x | 17.87 | 7.50 | 2.42 | none |
| V3 prefix='1' | rank | - | 3.40x | 247.68 | 98.93 | 56.33 | `"g"*` `"c"*` `"d"*` `"ca"*` `"do"*` `"car"*` `"doc"*` `"cargo"*` `"cargo"` `"cargo" "re"*` |
| V3 prefix='1' | rank | newest 10% | 3.40x | 28.26 | 20.52 | 10.95 | `"c"*` `"ca"*` |
| V3 prefix='1' | rowid DESC | - | 3.40x | 7.14 | 7.14 | 2.32 | none |
| V3 prefix='1' | rowid DESC | newest 10% | 3.40x | 7.29 | 7.29 | 2.32 | none |
| V3 prefix='1 2 3' | rank | - | 4.11x | 248.60 | 92.51 | 57.19 | `"g"*` `"c"*` `"d"*` `"ca"*` `"do"*` `"car"*` `"doc"*` `"cargo"*` `"cargo"` |
| V3 prefix='1 2 3' | rank | newest 10% | 4.11x | 26.98 | 10.86 | 8.81 | `"c"*` |
| V3 prefix='1 2 3' | rowid DESC | - | 4.11x | 1.44 | 1.44 | 1.44 | none |
| V3 prefix='1 2 3' | rowid DESC | newest 10% | 4.11x | 1.47 | 1.47 | 1.47 | none |
| V3 prefix='2 3' | rank | - | 3.76x | 268.18 | 94.78 | 54.93 | `"g"*` `"c"*` `"d"*` `"ca"*` `"do"*` `"car"*` `"doc"*` `"cargo"*` `"cargo"` `"git" "c"*` |
| V3 prefix='2 3' | rank | newest 10% | 3.76x | 58.42 | 10.93 | 9.04 | `"g"*` `"c"*` `"d"*` `"git" "c"*` |
| V3 prefix='2 3' | rowid DESC | - | 3.76x | 16.12 | 1.60 | 1.60 | none |
| V3 prefix='2 3' | rowid DESC | newest 10% | 3.76x | 16.22 | 1.56 | 1.56 | none |
| V4 detail=column | rank | - | 3.04x | 458.09 | 196.60 | 121.51 | `"g"*` `"c"*` `"d"*` `"gi"*` `"ca"*` `"do"*` `"ku"*` `"git"*` `"car"*` `"doc"*` `"cargo"*` `"docker"*` `"git"` `"cargo"` `"docker"` `"git" "c"*` `"git" "co"*` `"cargo" "re"*` |
| V4 detail=column | rank | newest 10% | 3.04x | 81.25 | 29.19 | 15.63 | `"g"*` `"c"*` `"d"*` `"ca"*` `"git" "c"*` `"cargo" "re"*` |
| V4 detail=column | rowid DESC | - | 3.04x | 17.91 | 7.37 | 2.52 | none |
| V4 detail=column | rowid DESC | newest 10% | 3.04x | 17.61 | 7.16 | 2.33 | none |
| V4 detail=none | rank | - | 2.78x | 580.61 | 200.13 | 124.58 | `"g"*` `"c"*` `"d"*` `"gi"*` `"ca"*` `"do"*` `"ku"*` `"git"*` `"car"*` `"doc"*` `"cargo"*` `"docker"*` `"git"` `"cargo"` `"docker"` `"git" "c"*` `"git" "co"*` `"cargo" "re"*` |
| V4 detail=none | rank | newest 10% | 2.78x | 104.22 | 37.99 | 21.43 | `"g"*` `"c"*` `"d"*` `"ca"*` `"do"*` `"car"*` `"doc"*` `"cargo"*` `"git" "c"*` `"git" "co"*` `"cargo" "re"*` |
| V4 detail=none | rowid DESC | - | 2.78x | 31.37 | 15.54 | 5.97 | `"c"*` `"git" "c"*` |
| V4 detail=none | rowid DESC | newest 10% | 2.78x | 31.32 | 15.58 | 6.01 | `"c"*` `"git" "c"*` |
| V3+V4 prefix='1' detail=none | rank | - | 2.89x | 436.67 | 199.24 | 121.90 | `"g"*` `"c"*` `"d"*` `"gi"*` `"ca"*` `"do"*` `"ku"*` `"git"*` `"car"*` `"doc"*` `"cargo"*` `"docker"*` `"git"` `"cargo"` `"docker"` `"git" "c"*` `"git" "co"*` `"cargo" "re"*` |
| V3+V4 prefix='1' detail=none | rank | newest 10% | 2.89x | 44.93 | 39.62 | 22.74 | `"c"*` `"d"*` `"ca"*` `"do"*` `"car"*` `"doc"*` `"cargo"*` `"git" "co"*` `"cargo" "re"*` |
| V3+V4 prefix='1' detail=none | rowid DESC | - | 2.89x | 15.95 | 15.95 | 6.28 | none |
| V3+V4 prefix='1' detail=none | rowid DESC | newest 10% | 2.89x | 16.40 | 16.40 | 6.04 | none |
| V3+V4 prefix='1 2 3' detail=none | rank | - | 3.14x | 436.43 | 181.85 | 119.93 | `"g"*` `"c"*` `"d"*` `"gi"*` `"ca"*` `"do"*` `"ku"*` `"git"*` `"car"*` `"doc"*` `"cargo"*` `"docker"*` `"git"` `"cargo"` `"docker"` `"git" "c"*` `"cargo" "re"*` |
| V3+V4 prefix='1 2 3' detail=none | rank | newest 10% | 3.14x | 45.48 | 22.81 | 22.81 | `"c"*` `"d"*` `"cargo"*` |
| V3+V4 prefix='1 2 3' detail=none | rowid DESC | - | 3.14x | 5.58 | 5.58 | 5.58 | none |
| V3+V4 prefix='1 2 3' detail=none | rowid DESC | newest 10% | 3.14x | 5.54 | 5.54 | 5.54 | none |

## V1: queries skipped by a minimum prefix length (baseline, rank, page 200)

| query | prefix chars | matches | baseline rank p95 | runs at min 2 | runs at min 3 |
|---|---|---|---|---|---|
| `"g"*` | 1 | 199756 | 105.54 | skipped | skipped |
| `"c"*` | 1 | 475508 | 267.20 | skipped | skipped |
| `"d"*` | 1 | 231251 | 125.50 | skipped | skipped |
| `"gi"*` | 2 | 33236 | 17.58 | yes | skipped |
| `"ca"*` | 2 | 188532 | 256.31 | yes | skipped |
| `"do"*` | 2 | 107715 | 57.80 | yes | skipped |
| `"ku"*` | 2 | 33417 | 17.15 | yes | skipped |
| `"git"*` | 3 | 33236 | 19.21 | yes | yes |
| `"car"*` | 3 | 108126 | 76.36 | yes | yes |
| `"doc"*` | 3 | 107715 | 56.72 | yes | yes |
| `"cargo"*` | 5 | 108126 | 54.35 | yes | yes |
| `"docker"*` | 6 | 33352 | 17.31 | yes | yes |
| `"git"` | whole word | 33236 | 16.23 | yes | yes |
| `"cargo"` | whole word | 108126 | 52.74 | yes | yes |
| `"docker"` | whole word | 33352 | 15.94 | yes | yes |
| `"git" "c"*` | 1 | 13272 | 47.10 | skipped | skipped |
| `"git" "co"*` | 2 | 6622 | 14.82 | yes | skipped |
| `"git" "com"*` | 3 | 1842 | 1.69 | yes | yes |
| `"cargo" "bu"*` | 2 | 0 | 0.02 | yes | skipped |
| `"cargo" "re"*` | 2 | 17736 | 29.85 | yes | skipped |
| `"cargo" "com"*` | 3 | 148 | 1.90 | yes | yes |
| `"docker" "git"*` | 3 | 0 | 1.20 | yes | yes |

## Feature probes (rowid DESC, page 200, rows compared to baseline)

| probe | kind | V0 baseline | V3 prefix='1' | V3 prefix='1 2 3' | V3 prefix='2 3' | V4 detail=column | V4 detail=none | V3+V4 prefix='1' detail=none | V3+V4 prefix='1 2 3' detail=none |
|---|---|---|---|---|---|---|---|---|---|
| `"src/main.rs"` | phrase (tokenizer splits a word) | ok (201 rows) | ok (201 rows) | ok (201 rows) | ok (201 rows) | error: fts5: phrase queries are not supported (detail!=full) | error: fts5: phrase queries are not supported (detail!=full) | error: fts5: phrase queries are not supported (detail!=full) | error: fts5: phrase queries are not supported (detail!=full) |
| `"task-12"*` | phrase with prefix last token | ok (201 rows) | ok (201 rows) | ok (201 rows) | ok (201 rows) | error: fts5: phrase queries are not supported (detail!=full) | error: fts5: phrase queries are not supported (detail!=full) | error: fts5: phrase queries are not supported (detail!=full) | error: fts5: phrase queries are not supported (detail!=full) |
| `"git" "commit"` | two-token AND | ok (201 rows) | ok (201 rows) | ok (201 rows) | ok (201 rows) | ok (201 rows) | ok (201 rows) | ok (201 rows) | ok (201 rows) |
| `NEAR("git" "commit", 1)` | NEAR (not user-reachable; Relic escapes it) | ok (201 rows) | ok (201 rows) | ok (201 rows) | ok (201 rows) | error: fts5: NEAR queries are not supported (detail!=full) | error: fts5: NEAR queries are not supported (detail!=full) | error: fts5: NEAR queries are not supported (detail!=full) | error: fts5: NEAR queries are not supported (detail!=full) |
| `safe_text : "git"` | column filter (not user-reachable) | ok (201 rows) | ok (201 rows) | ok (201 rows) | ok (201 rows) | ok (201 rows) | error: fts5: column queries are not supported (detail=none) | error: fts5: column queries are not supported (detail=none) | error: fts5: column queries are not supported (detail=none) |

## End-to-end page=200 cost: FTS + metadata (p50 / p95 / max ms)

### V0 baseline

| query | order | FTS only | FTS + N+1 lookups | JOIN on FTS subquery | naive JOIN | app::search (real Store) |
|---|---|---|---|---|---|---|
| `"g"*` | rank | 106.71 / 114.44 / 114.80 | 102.98 / 108.60 / 109.68 | 113.59 / 120.46 / 120.99 | 104.67 / 113.59 / 114.41 | 107.46 / 111.08 / 113.22 |
| `"g"*` | rowid DESC | 4.97 / 5.13 / 5.14 | 5.19 / 5.30 / 5.44 | 5.11 / 5.40 / 5.67 | 5.05 / 5.20 / 5.27 | n/a |
| `"gi"*` | rank | 15.98 / 16.53 / 16.65 | 16.25 / 16.58 / 16.65 | 16.85 / 17.39 / 17.42 | 16.13 / 16.45 / 16.82 | 16.63 / 17.13 / 17.25 |
| `"gi"*` | rowid DESC | 0.45 / 0.46 / 0.48 | 0.68 / 0.69 / 0.69 | 0.51 / 0.52 / 0.53 | 0.48 / 0.50 / 0.50 | n/a |
| `"git"*` | rank | 15.79 / 16.19 / 16.27 | 15.97 / 16.56 / 16.70 | 16.81 / 17.26 / 17.43 | 16.07 / 16.40 / 16.52 | 16.95 / 17.40 / 17.51 |
| `"git"*` | rowid DESC | 0.47 / 0.48 / 0.48 | 0.69 / 0.71 / 0.73 | 0.51 / 0.52 / 0.55 | 0.49 / 0.50 / 0.51 | n/a |
| `"cargo"` | rank | 50.81 / 52.27 / 52.58 | 51.29 / 51.98 / 52.02 | 52.01 / 52.91 / 53.32 | 49.59 / 50.28 / 51.30 | 50.63 / 51.43 / 52.34 |
| `"cargo"` | rowid DESC | 0.02 / 0.03 / 0.03 | 0.26 / 0.27 / 0.27 | 0.09 / 0.09 / 0.10 | 0.06 / 0.07 / 0.07 | n/a |
| `"git" "com"*` | rank | 1.55 / 1.59 / 1.63 | 1.79 / 1.80 / 1.81 | 2.09 / 2.13 / 2.14 | 1.62 / 1.63 / 1.67 | 2.26 / 2.30 / 2.33 |
| `"git" "com"*` | rowid DESC | 0.11 / 0.12 / 0.12 | 0.35 / 0.37 / 0.39 | 0.17 / 0.18 / 0.19 | 0.15 / 0.15 / 0.16 | n/a |

### V3 prefix='1'

| query | order | FTS only | FTS + N+1 lookups | JOIN on FTS subquery | naive JOIN |
|---|---|---|---|---|---|
| `"g"*` | rank | 93.57 / 95.33 / 97.39 | 92.81 / 94.91 / 95.23 | 94.99 / 97.17 / 97.31 | 92.92 / 95.43 / 97.04 |
| `"g"*` | rowid DESC | 0.05 / 0.05 / 0.06 | 0.28 / 0.29 / 0.29 | 0.11 / 0.11 / 0.12 | 0.09 / 0.09 / 0.09 |
| `"gi"*` | rank | 15.96 / 16.24 / 16.34 | 16.09 / 16.66 / 16.69 | 16.60 / 17.02 / 17.11 | 16.11 / 16.56 / 16.67 |
| `"gi"*` | rowid DESC | 0.45 / 0.47 / 0.49 | 0.68 / 0.70 / 0.73 | 0.52 / 0.55 / 0.56 | 0.49 / 0.51 / 0.54 |
| `"git"*` | rank | 15.94 / 16.21 / 16.34 | 16.13 / 17.13 / 17.38 | 17.33 / 17.88 / 18.09 | 16.47 / 17.00 / 17.04 |
| `"git"*` | rowid DESC | 0.46 / 0.47 / 0.48 | 0.69 / 0.72 / 0.73 | 0.51 / 0.52 / 0.54 | 0.49 / 0.51 / 0.51 |
| `"cargo"` | rank | 50.47 / 51.59 / 51.89 | 50.03 / 51.13 / 51.29 | 51.44 / 53.24 / 53.97 | 49.89 / 51.18 / 52.00 |
| `"cargo"` | rowid DESC | 0.05 / 0.05 / 0.06 | 0.28 / 0.29 / 0.30 | 0.11 / 0.12 / 0.13 | 0.08 / 0.09 / 0.10 |
| `"git" "com"*` | rank | 1.55 / 1.59 / 1.62 | 1.79 / 1.82 / 1.87 | 2.10 / 2.15 / 2.17 | 1.60 / 1.63 / 1.64 |
| `"git" "com"*` | rowid DESC | 0.11 / 0.11 / 0.11 | 0.34 / 0.35 / 0.36 | 0.17 / 0.17 / 0.18 | 0.15 / 0.15 / 0.16 |


load after: { 2.96 3.26 3.43 }

</details>
