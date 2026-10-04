# Interactive FTS measurements for search-as-you-type

**Status:** measured. The option A latency claim was superseded on 2026-10-03 by the hot-token study; the recommendation is pending the #11 architecture lock.
**Decision scope:** FTS5 schema options, result ordering, minimum query length, and recency pre-narrowing for the TUI's per-keystroke search; consequences for the #8 latency and size gates
**Research date:** 2026-10-03
**Evidence:** `benches/interactive_fts.rs` (run it yourself, below); raw output in appendices A (original run) and B (hot-token run)

## Decision

**Revised 2026-10-03 after the hot-token study (below).** The original decision is kept under [Superseded decision](#superseded-decision-original-10-text). Its latency claim for option A, "every keystroke under 8 ms (worst 7.45 ms, `cargo re*`)", is **wrong**. The #10 query set had no prefixes of high-frequency tokens. With them in the set, option A's worst keystroke is **21.0 ms** (`git tas*`). Three of 98 keystrokes the TUI sends exceed 20 ms, and nine exceed 10 ms.

Keep **`ORDER BY rowid DESC`** for interactive search. Nothing below changes that. BM25 `rank` still costs about 0.5 µs per match on every query.

### Cost model under `rowid DESC LIMIT n`

| Query shape | What FTS5 does | Cost at 1M (p95) |
|---|---|---|
| Whole word, any frequency | Walks the doclist newest-first and stops after n rows | 0.03–0.06 ms (`"task"`, 93% of rows: 0.04 ms) |
| Prefix whose **exact** length has a prefix index | Reads one stored prefix term, then stops early as for a whole word | 0.04–0.06 ms (`"ta"*` with `prefix='2'`: 0.05 ms, down from 19.4 ms) |
| Prefix with no prefix index of exactly its length | **Merges the full doclist of every term under the prefix into one list before returning the first row.** `LIMIT` cannot cut the merge short, and neither can a `rowid >=` bound. | About **13 ns per posting** for a single term, 19–30 ns per posting when several large terms merge, plus about 5–6 µs per term. Prefix length does not matter: `"tas"*` and `"task"*` both cost 12.6 ms, against 0.04 ms for the whole word `"task"`. |
| Two tokens that rarely share a row (sparse AND) | Cannot stop early. Every row of the rarer token seeks into the other's doclist. | `"git" "task"` (0 matches): **6.8 ms on every schema**. This floor adds to any merge: `"git" "tas"*` costs 12.6 + 6.8 ≈ 21 ms. |

So an unindexed prefix costs in proportion to **the postings under the prefix (the expanded terms' total doclist size)**, not the prefix length or the page size. A token that appears in every row of a 1M history costs about 13 ms as an unindexed prefix, before any AND. That cost grows linearly with history size. A prefix index helps only at the exact lengths it lists: with `prefix='2 3 4'`, `"taski"*` (5 chars, over a 93% token) still costs 12.5 ms.

### Size and worst keystroke per configuration

Fixture: 1M records, 53.3 MiB source. The keystroke set is 98 queries the TUI sends: the 22-query #10 set without its 1-char prefixes, plus every 2-char to full-length prefix of the 8 most frequent tokens, alone and after `git` / `cargo`, plus the whole-word forms. Measurements are p95 at page 200 (`LIMIT 201`), from a 50-run cell. "taskid corpus" is the same fixture with `# task-N` renamed `# taskid-N`. It stands in for a dominant token longer than 4 characters, such as `docker`, `kubectl` or `python3` in a real history.

| Configuration | DB size / source | Worst sent keystroke, fixture | That query | Sent keystrokes > 20 ms | Worst, taskid corpus |
|---|---|---|---|---|---|
| A: baseline schema (shipped) | 3.04x | **20.95 ms** | `"git" "tas"*` | 3 of 98 | 24.5 ms |
| `prefix='2'` | 3.41x (+0.37x) | 20.83 ms | `"git" "tas"*` | 2 of 98 | 27.1 ms¹ (`git taski*`) |
| `prefix='2 3'` | 3.76x (+0.72x) | 21.06 ms | `"git" "task"*` | 1 of 98 | not built |
| `prefix='2 3 4'` | 4.02x (+0.98x) | **6.82 ms** | `"git" "task"` (sparse-AND floor) | 0 | **21.2 ms** (`git taski*`) |
| `prefix='1 2 3 4'` | 4.38x (+1.34x) | 6.84 ms | `"git" "tas"*` | 0 | not built |
| Baseline schema + OR-of-terms rewrite for `*` tokens of 2+ chars (term list in memory) | 3.04x + term list | 7.21 ms | `"git" "tas"*` | 0 | 12.8 ms¹ (2nd worst 6.6) |
| **`prefix='2'` + OR-of-terms rewrite for `*` tokens of 3+ chars** | **3.41x** + term list | **6.69 ms** | `"git" "task"` (sparse-AND floor) | **0** | **6.6 ms** |

The "taskid corpus" column is the worst over the 18 keystrokes built on `taskid`, not over the fixture column's 98, so the two columns are not the same keystroke set.

¹ Single cells under load average 7–10. Under taskid `prefix='2'`, the neighbouring `git taskid*` cell measures 20.85 ms. Under the taskid hybrid, the second-worst keystroke (`git taskid*`, the same single-term OR) measures 6.64 ms, and `git tas*` through the same OR measures 6.8 ms on the fixture.

On this fixture `prefix='2'` and `prefix='2 3'` sit right at the 20 ms line, 20.8–21.1 ms. The margin is inside the noise and means nothing. What decides them is the cost model: a token in about 93% of rows, behind a lead word it never co-occurs with, costs about 21 ms at 1M, and that cost doubles with a 2M history. **`prefix='2 3 4'` is the cheapest pure-schema configuration that passes here** (+0.98x), and it passes only because the fixture's dominant token has 4 characters. The taskid corpus shows it failing for a longer one.

### Query rewrites without an index

Each rewrite was measured on the baseline schema against the plain prefix query. The full table is under [Query rewrites](#query-rewrites-on-the-baseline-schema).

- **Whole word only** (send `"task"` instead of `"task"*`) takes 0.01–0.07 ms, but it **changes results**. It drops every longer completion: `ta` and `tas` return 0 of the 201 prefix rows, and a newer `tasks` row is lost behind older `task` rows. Not acceptable silently. Usable only as an interim frame that the real result replaces.
- **Whole word OR prefix** (`"task" OR "task"*`) gives the same rows and the same cost or worse (`git task*`: 27–35 ms across the two runs). The prefix merge still runs.
- **Exact-first + rowid bound.** A full page of whole-word hits gives a rowid floor, and the prefix query then runs with `rowid >= floor`. It gives the same rows, and no saving: the merge ignores the rowid bound (`ta*` 19.7 ms, `git task*` 27.5 ms).
- **Per-keystroke fts5vocab range scan + OR of the expanded whole terms** gives the same rows and is 20–60% cheaper. It stays proportional to postings, because fts5vocab counts each term by reading its doclist: `t*` takes 15.3 ms, `git task*` 16.3 ms. Not bounded.
- **OR of expanded whole terms taken from an in-memory term list** gives the **same rows**. Every term is a whole-word iterator, so `rowid DESC` stops early again. Cost follows the **number of expanded terms**, about 6 µs per term (`1*`: 1,111 terms, 6.5 ms; `12*`: 111 terms, 0.6 ms), plus any sparse-AND floor. For dominant terms it costs about 300x less than the plain prefix query (`tas*`: 12.6 → 0.04 ms). **But the cost multiplies, not adds, behind a lead word the OR never co-occurs with**: a review re-measurement on this fixture (Python's SQLite 3.53.4, so relative numbers only) gave `"git" AND (OR of N terms)` at 0.26 ms for 1 term, 2.9 ms for 100, 12.8 ms for 300, and 73 ms for the 1,111 terms under `1` — against 8.5 ms for the plain `"git" "1"*`. Behind a lead word that does co-occur (`docker`) it is milder (11 vs 6.9 ms at 1,111 terms). It is the only rewrite that keeps prefix semantics, and its cost is bounded only for narrow expansions. It needs a term list: loading the full fts5vocab list takes 86 ms on the 1M fixture (10,098 terms; a real history's long tail will have more). It reads every posting, so it runs off the UI thread. Prefix semantics hold only while that list is current, so a stale list is a correctness bug, not a slowdown (see below).

### Recommendation for #11

1. **Keep `rowid DESC` and the 2-char gate** (`MIN_PREFIX_CHARS = 2`). Do not claim that the gate bounds cost. It removes only 1-char prefixes.
2. **Add `prefix='2'` to `observation_fts`** (+0.37x, 3.04x → 3.41x; 2 s rebuild at 1M). On this fixture it is not strictly needed — the baseline schema plus the rewrite for 2+ char tokens already passes at 3.04x (7.21 ms) — but 2-char prefixes have the widest expansions in terms and postings, and the multiplicative OR cost (item 3) makes a wide 2-char OR behind a sparse lead word the likeliest failure on real data. It is insurance whose need was not measured here. With the index, 2-char prefixes take 0.05 ms plus any sparse-AND floor.
3. **Rewrite a 3+ char last token as an OR of its expanded whole terms**, from an in-memory term list:
   - Load the list once through `fts5vocab(..., 'row')` (86 ms at 1M), off the UI thread.
   - Reload it when `PRAGMA data_version` changes. Until a reload finishes, send the plain prefix query, because a stale list silently misses new terms. Note that every command another shell records changes `data_version`, so with hooks active the slow plain query may run often; incremental term updates may be needed.
   - Cap the OR width low — about 100 expanded terms, or better a measured cost budget — and send the plain prefix query above it. An OR's cost multiplies behind a lead word it never co-occurs with (300 terms behind `git`: 12.8 ms; 1,111 terms: 73 ms, against 8.5 ms for the plain prefix), so the earlier "about 1,500 terms" cap was wrong.
   - Keep the plain prefix query when the tokenizer splits the last word into a phrase (`task-1*`): FTS5 phrases cannot contain OR.
   - The rewrite needs the query's last word folded the way `unicode61 remove_diacritics 2` folds it, so it should use the same tokenizer rules or the plain query.
   
   Measured worst sent keystroke: 6.7 ms on the fixture and 6.6 ms on the taskid corpus, the sparse-AND floor — **but the widest OR in that run had only 2 terms**, so this holds for narrow expansions only. Lead-word + wide-OR keystrokes (`git 1*`, `git 12*`, `docker 1*`) must be added to the bench before relying on it.
4. **If #11 rejects a term list:** `prefix='2 3 4'` (4.02x) meets the gate on this fixture, but a dominant token of 5+ characters brings back the unbounded case (21 ms on the taskid corpus). That is another +0.61x on a size gate that already fails.
5. **If #11 rejects both:** option A as shipped misses `<20 ms` p95 at 1M (21 ms), and the miss grows linearly with history size.
6. **The sparse-AND floor is a separate, unbounded cost.** No prefix scheme touches it. It scales with the rarer token's postings when the two tokens never share a row: `git task` takes 6.8 ms with 33k `git` rows. At about 0.2 µs per row of the rarer token, two 30%-frequency tokens that never co-occur would take roughly 60 ms at 1M. That figure is extrapolated, not measured. #11 should budget it, for example by interrupting a query that runs past its time budget through SQLite's progress handler and keeping the previous page. It was not measured beyond the cases here.

Before locking, run `interactive_fts --hot` on an anonymized real history. It prints the top tokens, the term counts under each prefix and every measurement above, so the term-count cap and the per-term cost can be checked against real long tails.

## Superseded decision (original #10 text)

> **Superseded.** Kept as written for the record. Its latency claims for option A ("worst 7.45 ms", "the only expensive case left is a short prefix") are wrong for prefixes of high-frequency tokens; see [Decision](#decision) and [Hot-token prefixes](#hot-token-prefixes-follow-up-measurement). The rank findings and size numbers still stand.

The TUI should **order interactive results by recency (`ORDER BY rowid DESC`)**, not by BM25 `rank`. Ordering is what decides latency, and the schema option barely matters next to it:

- With `ORDER BY rank`, cost grows linearly with the number of matches, about **0.5 µs per matching row**: 33k matches take 16 ms, 108k take 52 ms, 475k take 260 ms. Prefix indexes, `detail=` options and short-query gates do not change this. FTS5 has to score every match before it can return the top 200. The 0.5 µs/match model holds for single-term queries only: rank meets `<20 ms` for a single whole word or long prefix under about 35k matches (3.5% of the corpus), but multi-token queries whose last token is a short prefix miss it even when selective (`git c*`, 13k matches / 1.3%, 47 ms; `cargo re*`, 18k / 1.8%, 30 ms).
- With `ORDER BY rowid DESC LIMIT n`, FTS5 walks the doclist newest-first and **stops after n rows**. A whole-word query for `cargo` (108k matches) drops from 52 ms to **0.03 ms**. ~~The only expensive case left is a short prefix (`c*`, `ca*`)~~ **(superseded: any prefix without a prefix index of exactly its length is expensive when the terms under it have large doclists; `"task"*` costs 12.6 ms)**, because FTS5 first merges the doclists of every term with that prefix.

Two ways to meet `<20 ms` on every keystroke:

| Option | Schema change | Worst FTS p95, page 200, every keystroke | DB size cost |
|---|---|---|---|
| **A (recommended for v0.1):** baseline schema, `rowid DESC`, a 1-char last token is not sent to FTS. The TUI keeps the previous page or shows the unfiltered recent page, which is a primary-key scan and was not measured. | none | ~~**7.45 ms** (`cargo re*`)~~ **superseded: 20.95 ms (`git tas*`) once hot-token prefixes are included** | **0** |
| **B:** add `prefix='1'` to `observation_fts`, `rowid DESC`, no length gate | one FTS option + rebuild (2 s at 1M) | ~~**7.14 ms**~~ (all 22 queries, 1-char included); **superseded: `prefix='1'` does not touch 2+ char prefixes, so B has the same ~21 ms worst case as A** | **+19.1 MiB (+0.36x source)** |

`prefix='1 2 3'` brings every query under 1.5 ms, but it costs +57 MiB (+1.07x). That is far more speed than a 20 ms budget needs, so it is not worth paying. Adding the per-row metadata fetch to either option costs 0.25 ms (N+1 lookups) or 0.06 ms (JOIN) per 200-row page. This is negligible, so ~~the worst keystroke stays under 8 ms end to end~~ (superseded: about 21 ms at 1M) before channel and render time.

Option A is the default because the size gate already fails (see below) and ~~the zero-cost version is fast enough~~ (superseded: it is not, see [Decision](#decision)). Option B is the knob to turn if 1-char filtering proves necessary in use.

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
  This takes about 40 min, nearly all of it slow rank cells. The hot-token study alone (rowid DESC only, about 10 min):
  ```console
  cargo run --release --bin interactive_fts -- target/fixtures/1m.db target/fts-variants 50 --hot
  ```

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

**V1 (minimum length).** A gate does not rescue `rank`. At 2 chars `ca*` still costs 256 ms p95, and at 3 chars `car*` still costs 76 ms. The gate only helps together with `rowid DESC`, where it removes the merge cost of 1-char prefixes. The UX cost: with a 2-char gate, typing `g`, `c`, `d` and the `c` in `git c` produces no new FTS query, and the TUI keeps or shows the recency page. With a 3-char gate, `gi`, `ca`, `git co` and `cargo re` are skipped as well. ~~The 3-char gate buys nothing under `rowid DESC`, because 2-char prefixes already cost at most 7.5 ms.~~ **Superseded:** that held for this query set only. A 2-char prefix of a dominant token costs 19.4 ms (`ta*`), and gating at any length leaves 3+ char prefixes of it at 12.5 ms or more; see [Hot-token prefixes](#hot-token-prefixes-follow-up-measurement).

**V2 (`ORDER BY rowid DESC LIMIT n`).** The early exit is real. The query plan shows the vtab consuming the order (`INDEX 192:M1`, no temp B-tree). Single whole words take 0.02-0.04 ms whatever their frequency, and page 50 and page 200 cost the same. Prefix tokens without a prefix index pay to merge every doclist under the prefix before the walk can start, and that cost grows with how many terms and rows sit under it: `cargo*` 1.5 ms, `ca*` 5 ms, `c*` 18-20 ms, against 0.03 ms for the exact word `cargo`. The token AND `cargo re*` (7.5 ms) is the slowest query in this set that passes a 2-char gate (**superseded** as a worst case: `git tas*` costs 21 ms). Sparse ANDs stay cheap (0.9-1.7 ms) here, because FTS5 seeks through doclists with its doclist index instead of scanning them. (Not in general: a sparse AND against a dominant token, `git task`, costs 6.8 ms on every schema; see the hot-token study.)

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

## Hot-token prefixes (follow-up measurement)

A reviewer found that option A's "<8 ms per keystroke" did not hold for prefixes of high-frequency tokens. This section measures that case directly. It supersedes the latency claims for option A.

### Method

- **Command:** `interactive_fts target/fixtures/1m.db target/fts-variants 50 --hot`, on the same machine and fixture as above. Every cell is `ORDER BY rowid DESC LIMIT 201`, with one warm-up and 50 timed runs. The cells report p95, and every variant's rows were compared with the baseline's: all identical.
- **Machine state:** other agent sessions were running. Load average was 7.3 before and 10.2 after, on 16 cores. A first run of the same command, before the hybrid rows and the taskid `prefix='2'` variant were added (load 4.9 before, 14.6 after), gave the same picture: most fixture cells land within ±1 ms of this run, and every conclusion holds in both. A few p95 cells moved more under load, up to 7.5 ms (`git task*` word-OR-prefix: 27.3 ms against 34.8 ms). Outliers that touch a conclusion are footnoted.
- **Top tokens:** read through a temp `fts5vocab(main, observation_fts, 'row')` table on a copy of the database. The fixture's `# task-NNNN` comments put `task` in 93.3% of rows:

  | # | Token | Chars | Rows | Share |
  |---|---|---|---|---|
  | 1 | `task` | 4 | 933,329 | 93.3% |
  | 2 | `rs` | 2 | 231,514 | 23.2% |
  | 3 | `src` | 3 | 230,588 | 23.1% |
  | 4 | `log` | 3 | 155,681 | 15.6% |
  | 5 | `toml` | 4 | 153,679 | 15.4% |
  | 6 | `app` | 3 | 153,332 | 15.3% |
  | 7 | `config` | 6 | 153,275 | 15.3% |
  | 8 | `cargo` | 5 | 108,126 | 10.8% |
  | 9 | `go` | 2 | 107,419 | 10.7% |
  | 10 | `integration` | 11 | 77,413 | 7.7% |

- **Keystrokes:** for each of the top 8 tokens, every prefix from 2 characters to the full token (`ta* tas* task*`, `co* con* conf* confi* config*`), the whole word, and the same behind `git` and `cargo`. Behind `git` the pair is a sparse AND, because `git` rows never carry `# task-N`. Behind `cargo` it is dense, because every `cargo` row does. The 22-query #10 set is added on top, giving 102 keystrokes, 98 of which the TUI sends under its 2-char gate.
- **Schemas:** baseline, `prefix='2'`, `'2 3'`, `'2 3 4'` and `'1 2 3 4'`. The **taskid corpus** is the fixture with `# task-N` renamed `# taskid-N` and FTS rebuilt, built at baseline, `prefix='2'` and `prefix='2 3 4'`.

### Size

| Variant | DB MiB | Ratio to 53.3 MiB source | FTS MiB |
|---|---|---|---|
| baseline | 162.0 | 3.04x | 31.7 |
| `prefix='2'` | 181.9 | 3.41x | 51.6 |
| `prefix='2 3'` | 200.3 | 3.76x | 70.0 |
| `prefix='2 3 4'` | 214.4 | 4.02x | 84.1 |
| `prefix='1 2 3 4'` | 233.5 | 4.38x | 103.2 |

Each indexed prefix length costs about 18–20 MiB (0.35x) on this corpus.

### Dominant-token keystrokes (p95 ms, page 200)

| Query | Postings under last token | baseline | `prefix='2'` | `prefix='2 3'` | `prefix='2 3 4'` | `prefix='1 2 3 4'` |
|---|---|---|---|---|---|---|
| `"ta"*` | 1,027,297 (4 terms) | 19.38 | **0.05** | 0.06 | 0.04 | 0.05 |
| `"tas"*` | 933,329 | 12.52 | 12.51 | **0.05** | 0.06 | 0.04 |
| `"task"*` | 933,329 | 13.10 | 12.52 | 12.57 | **0.06** | 0.05 |
| `"task"` (word) | 933,329 | 0.04 | 0.06 | 0.04 | 0.05 | 0.05 |
| `"git" "ta"*` | 1,027,297 | 20.27 | 0.82 | 0.83 | 0.81 | 0.83 |
| `"git" "tas"*` | 933,329 | **20.95** | **20.83** | 6.84 | 6.76 | 6.84 |
| `"git" "task"*` | 933,329 | 20.81 | 20.68 | **21.06** | 6.77 | 6.77 |
| `"git" "task"` (word) | 933,329 | 6.77 | 6.68 | 6.97 | 6.82 | 6.79 |
| `"cargo" "ta"*` | 1,027,297 | 19.48 | 0.09 | 0.11 | 0.09 | 0.10 |
| `"cargo" "tas"*` | 933,329 | 12.51 | 12.40 | 0.10 | 0.11 | 0.09 |
| `"cargo" "task"*` | 933,329 | 12.56 | 12.51 | 12.60 | 0.12 | 0.10 |
| `"confi"*` | 153,275 | 2.03 | 2.03 | 2.04 | 2.11 | 2.01 |
| `"c"*` (1-char, not sent) | 560,579 (14 terms) | 16.49 | 15.17 | 15.16 | 15.15 | 0.04 |

- **Early exit on an indexed length is real.** `"ta"*` drops from 19.4 ms to 0.05 ms with `prefix='2'`, the same cost as a whole word. The next keystroke, `"tas"*`, goes straight back to 12.5 ms, because `prefix='2'` does not cover 3 characters. FTS5 picks a prefix index only when its length equals the token's length exactly.
- **The sparse-AND floor.** `"git" "task"` returns 0 rows and costs 6.8 ms on every schema. No index changes it. When the prefix is unindexed it adds to the merge (≈ 12.5 + 6.8 ≈ 21 ms).

### A 5+ character dominant token (taskid corpus, p95 ms)

| Query | taskid baseline | taskid `prefix='2'` | taskid `prefix='2 3 4'` |
|---|---|---|---|
| `"task"*` | 12.66 | 12.53 | 0.06 |
| `"taski"*` | 12.55 | 12.47 | **13.25** |
| `"taskid"*` | 12.80 | 12.43 | **12.74** |
| `"git" "taski"*` | 21.57 | 27.05¹ | **21.23** |
| `"git" "taskid"*` | 21.29 | 20.85 | **21.21** |
| `"taskid"` (word) | 0.05 | 0.06 | 0.05 |

¹ An outlier under load. The neighbouring `git taskid*` cell measures 20.85 ms.

When only lengths up to 4 are indexed, a 5- or 6-character prefix of a dominant token pays the full merge, exactly as on the baseline schema. `prefix='2 3 4'` therefore does not bound cost in general. It bounds cost only for tokens of 4 characters or fewer.

### Cost model fit (baseline, single-token prefix, p50)

| Query | Terms | Postings | p50 ms | ns per posting |
|---|---|---|---|---|
| `"12"*` | 111 | 10,314 | 0.48 | 46 |
| `"1"*` | 1,111 | 103,587 | 6.34 | 61 |
| `"carg"*` | 1 | 108,126 | 1.43 | 13 |
| `"config"*` | 1 | 153,275 | 2.01 | 13 |
| `"rs"*` | 1 | 231,514 | 3.04 | 13 |
| `"task"*` | 1 | 933,329 | 12.33 | 13 |
| `"ta"*` | 4 | 1,027,297 | 19.22 | 19 |
| `"c"*` | 14 | 560,579 | 17.03 | 30 |
| `"t"*` | 8 | 1,412,133 | 35.98 | 25 |

A single large term costs a steady **13 ns per posting**. Merging several large terms costs more per posting (19–30 ns). Many small terms add about 5–6 µs per term (`1*`). The bench prints the full table, with every hot prefix.

### Query rewrites on the baseline schema

p95 ms. "Rows" means the result is identical to the plain prefix query.

| Query | Terms | Plain | Whole word only (rows kept) | Word OR prefix | Exact-first + rowid bound | vocab scan + OR | OR of terms (list in memory) |
|---|---|---|---|---|---|---|---|
| `"ta"*` | 4 | 19.76 | 0.01 (0 of 201) | 19.88 | 19.72 | 11.32 | **0.09** |
| `"tas"*` | 1 | 12.55 | 0.01 (0 of 201) | 12.63 | 12.52 | 9.46 | **0.04** |
| `"task"*` | 1 | 12.59 | 0.04 (201 of 201) | 12.61 | 12.54 | 9.43 | **0.04** |
| `"git" "tas"*` | 1 | 20.83 | 0.02 (0 of 0) | 20.96 | 20.73 | 16.26 | **6.82** |
| `"git" "task"*` | 1 | 20.82 | 6.85 (0 of 0) | 34.79 | 27.49 | 16.25 | **7.22** |
| `"cargo" "ta"*` | 4 | 19.76 | 0.02 (0 of 201) | 19.84 | 19.79 | 11.00 | **0.13** |
| `"t"*` | 8 | 37.15 | 0.01 (0 of 201) | 36.97 | 35.61 | 15.25 | **0.18** |
| `"1"*` | 1,111 | 6.38 | 0.02 (0 of 201) | 6.42 | 6.41 | 8.04 | 6.49 |
| `"12"*` | 111 | 0.50 | 0.01 (3 of 201) | 0.49 | 0.53 | 0.75 | 0.61 |

Every rewrite except "whole word only" returned exactly the plain prefix rows for every probe (62 probes in the appendix). Whole word only changes results.

### Hybrid configurations (all sent keystrokes, p95 ms)

| Schema | OR rewrite applied to `*` tokens of | Fixture worst | Taskid worst | Most terms in one OR | Rows differ |
|---|---|---|---|---|---|
| baseline | ≥ 2 chars | 7.21 (`git tas*`) | 12.84¹ (next 6.64) | 6 | 0 |
| `prefix='2'` | ≥ 3 chars | **6.69** (`git task`) | **6.55** (`git tas*`) | 2 | 0 |

¹ An outlier under load. `git tas*` measures 6.8 ms through the same OR on the fixture.

The full term list (10,098 terms) loads through fts5vocab in 86 ms p50 at 1M.

## What this implies for the #8 gates

- **FTS query over 1M, warm p95 `<20 ms`:** ~~**met** for the interactive path with option A or B, at every measured keystroke. The worst case is 7.5 ms, 2.7x headroom before channel and render.~~ **Superseded: not met by option A or B** once prefixes of high-frequency tokens are measured (21 ms, `git tas*`). It is met with 3x headroom by `prefix='2'` plus the OR-of-terms rewrite (6.7 ms), or by `prefix='2 3 4'` on this fixture only (6.8 ms); see [Decision](#decision). **Not met** by any schema if results are BM25-ranked over all matches. #11 must pick: (1) recency order for interactive and default search, with rank as an opt-in mode that is either bounded by a window or allowed to be slow; or (2) keep rank and relax the gate to "p95 `<20 ms` for single whole-word or long-prefix queries under about 35k matches". Even that narrower gate excludes ordinary keystrokes: multi-token queries ending in a 1–2 char prefix miss it at 1–2% selectivity (`git c*` 47 ms, `cargo re*` 30 ms), so option (2) cannot serve search-as-you-type. Option (1) is recommended. Shell history search (Ctrl-R, fzf, atuin) is recency-first anyway, and BM25 over 40-byte commands says little about relevance.
- **DB size `<2x` source:** **not met by any FTS variant, and not because of FTS.** The cheapest measured schema, `detail=none`, which breaks path search, is 2.78x. The base table plus its indexes is 2.44x before any FTS index. Even with no FTS and no secondary indexes, `observation` alone is 1.64x. Levers outside this slice: drop the redundant `observation_revision` index (−0.23x, to about 2.81x); shrink per-row hashes and locators (24 of the roughly 92 bytes per row); question whether `observation_time` (0.34x) is needed. #11 should renegotiate the gate to about **3x** for full-detail FTS. If option B is chosen, it should budget `prefix='1'` at +0.36x on top. **Superseded:** the revised recommendation budgets `prefix='2'` at +0.37x (3.41x) instead, or `prefix='2 3 4'` at +0.98x (4.02x) if no term list is kept. A denominator note: #8 measured against raw history-file bytes, including zsh's `: ts:dur;` prefix. Against the sanitized text alone (39.2 MB), every ratio is about 1.4x higher.

## Open questions for #11

1. **Recency is rowid order, not time order.** `rowid DESC` means "most recently imported", which matches `observed_at_ms` only while imports arrive in time order. A backfilled older history (say, bash imported after zsh) would sort above newer commands. Options: accept "recently imported"; allocate rowids per time bucket at import; or order by `observed_at_ms` through the `observation_time` index. The last is a different plan with no FTS early exit and was not measured.
2. **Rank inside a recency window.** "Newest K matches by rowid, then re-sort by bm25" (for example K=2,000) would keep some relevance at bounded cost. It was not measured.
3. **1-char UX.** Option A shows the unfiltered recent page (or keeps the previous page) for a 1-char last token. If users expect `g` to filter, take option B (+0.36x).
4. **Size gate wording.** Decide whether the denominator is raw history bytes or sanitized text, and whether the gate covers FTS or only the base store.
5. **Real data.** The fixture has about 35 distinct leading tokens and one command per second. Real histories have longer tails (more distinct terms per prefix, so slower prefix merges without a prefix index) and sparser timestamps. Run `interactive_fts --hot` on an anonymized real 1M history before locking, covering the recommended configuration.
6. **Cold cache.** Everything here is warm OS cache. The #8 cold gate (`<100 ms` p95) was not re-measured for these variants.

7. **Sparse-AND budget.** Two tokens that never share a row cannot stop early. The cost scales with the rarer one's postings (`git task`: 6.8 ms over 33k `git` rows), and no prefix scheme changes it. Decide whether to time-box queries through the progress handler.
8. **Term-list freshness.** The OR-of-terms rewrite is exact only while the in-memory term list matches the index. Decide the reload trigger (`PRAGMA data_version`) and confirm that the plain prefix fallback during a reload is acceptable.
9. **Real term distribution.** Measure, on a real history, how many terms 3+ char prefixes expand to (per-term cost is about 6 µs) and how large the top tokens' doclists are (13 ns per posting unindexed). `interactive_fts --hot` prints both.

## Appendix A: raw output of the original #10 run

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

## Appendix B: raw output of the hot-token run

<details>
<summary>Full output of <code>interactive_fts target/fixtures/1m.db target/fts-variants 50 --hot</code></summary>

=== interactive FTS measurements (--hot) ===
host: Mac16,5 | Apple M4 Max | 16 | 137438953472
os: 27.0.1
load before: { 7.28 8.01 7.00 }
sqlite (bundled): 3.49.1
timed runs per cell: 50 (+1 discarded warm-up); cells are p50 / p95 / max ms
records: 1000000, source history: 55906677 bytes (53.3 MiB), unvacuumed fixture db: 177213440 bytes


# Hot-token prefixes: rowid DESC, page 200 (limit 201), p95 ms

## Size per hot-study schema (after rebuild + VACUUM)

| variant | corpus | DB MiB | ratio to source | fts MiB |
|---|---|---|---|---|
| baseline | fixture | 162.0 | 3.04x | 31.7 |
| prefix='2' | fixture | 181.9 | 3.41x | 51.6 |
| prefix='2 3' | fixture | 200.3 | 3.76x | 70.0 |
| prefix='2 3 4' | fixture | 214.4 | 4.02x | 84.1 |
| prefix='1 2 3 4' | fixture | 233.5 | 4.38x | 103.2 |
| taskid baseline | taskid | 163.8 | 3.07x | 31.7 |
| taskid prefix='2' | taskid | 183.7 | 3.45x | 51.6 |
| taskid prefix='2 3 4' | taskid | 216.2 | 4.06x | 84.1 |

## fixture

Full term list through fts5vocab (what an in-memory term cache would load): 10098 terms, 85.53 / 87.67 / 87.67 ms (p50 / p95 / max, 10 runs)

### Top 10 tokens by document frequency (fts5vocab 'row', 10098 distinct terms)

| # | token | chars | rows containing it | share of rows |
|---|---|---|---|---|
| 1 | `task` | 4 | 933329 | 93.3% |
| 2 | `rs` | 2 | 231514 | 23.2% |
| 3 | `src` | 3 | 230588 | 23.1% |
| 4 | `log` | 3 | 155681 | 15.6% |
| 5 | `toml` | 4 | 153679 | 15.4% |
| 6 | `app` | 3 | 153332 | 15.3% |
| 7 | `config` | 6 | 153275 | 15.3% |
| 8 | `cargo` | 5 | 108126 | 10.8% |
| 9 | `go` | 2 | 107419 | 10.7% |
| 10 | `integration` | 11 | 77413 | 7.7% |

### Every keystroke (p95 ms; `!` marks rows that differ from baseline)

| query | class | `*` chars | terms under last token | postings under last token | matches | baseline | prefix='2' | prefix='2 3' | prefix='2 3 4' | prefix='1 2 3 4' |
|---|---|---|---|---|---|---|---|---|---|---|
| `"g"*` | 1-char | 1 | 4 | 207359 | 199756 | 5.02 | 5.06 | 5.10 | 5.07 | 0.05 |
| `"c"*` | 1-char | 1 | 14 | 560579 | 475508 | 16.49 | 15.17 | 15.16 | 15.15 | 0.04 |
| `"d"*` | 1-char | 1 | 5 | 247227 | 231251 | 6.10 | 5.91 | 5.97 | 5.97 | 0.03 |
| `"gi"*` | 2-char | 2 | 1 | 33236 | 33236 | 0.47 | 0.06 | 0.03 | 0.03 | 0.05 |
| `"ca"*` | 2-char | 2 | 3 | 199590 | 188532 | 4.75 | 0.06 | 0.04 | 0.05 | 0.04 |
| `"do"*` | 2-char | 2 | 2 | 110189 | 107715 | 2.27 | 0.06 | 0.05 | 0.04 | 0.04 |
| `"ku"*` | 2-char | 2 | 1 | 33417 | 33417 | 0.48 | 0.06 | 0.04 | 0.03 | 0.04 |
| `"git"*` | 3-char | 3 | 1 | 33236 | 33236 | 0.47 | 0.47 | 0.05 | 0.04 | 0.04 |
| `"car"*` | 3-char | 3 | 1 | 108126 | 108126 | 1.45 | 1.46 | 0.05 | 0.04 | 0.05 |
| `"doc"*` | 3-char | 3 | 2 | 110189 | 107715 | 2.24 | 2.27 | 0.05 | 0.04 | 0.05 |
| `"cargo"*` | word* | 5 | 1 | 108126 | 108126 | 1.47 | 1.49 | 1.46 | 1.46 | 1.47 |
| `"docker"*` | word* | 6 | 1 | 33352 | 33352 | 0.47 | 0.50 | 0.47 | 0.47 | 0.47 |
| `"git"` | word | word | 1 | 33236 | 33236 | 0.03 | 0.04 | 0.05 | 0.03 | 0.04 |
| `"cargo"` | word | word | 1 | 108126 | 108126 | 0.02 | 0.03 | 0.04 | 0.04 | 0.04 |
| `"docker"` | word | word | 1 | 33352 | 33352 | 0.03 | 0.06 | 0.04 | 0.04 | 0.04 |
| `"git" "c"*` | two-word | 1 | 14 | 560579 | 13272 | 16.83 | 15.50 | 15.31 | 15.26 | 0.14 |
| `"git" "co"*` | two-word | 2 | 3 | 188483 | 6622 | 3.78 | 0.17 | 0.15 | 0.15 | 0.15 |
| `"git" "com"*` | two-word | 3 | 1 | 1842 | 1842 | 0.12 | 0.12 | 0.10 | 0.10 | 0.10 |
| `"cargo" "bu"*` | two-word | 2 | 0 | 0 | 0 | 0.02 | 0.03 | 0.04 | 0.04 | 0.04 |
| `"cargo" "re"*` | two-word | 2 | 6 | 274105 | 17736 | 6.90 | 0.16 | 0.15 | 0.14 | 0.15 |
| `"cargo" "com"*` | sparse AND | 3 | 1 | 1842 | 148 | 0.78 | 0.79 | 0.83 | 0.77 | 0.80 |
| `"docker" "git"*` | sparse AND | 3 | 1 | 33236 | 0 | 1.56 | 1.55 | 1.19 | 1.19 | 1.21 |
| `"ta"*` | hot prefix | 2 | 4 | 1027297 | 935172 | 19.38 | 0.05 | 0.06 | 0.04 | 0.05 |
| `"tas"*` | hot prefix | 3 | 1 | 933329 | 933329 | 12.52 | 12.51 | 0.05 | 0.06 | 0.04 |
| `"task"*` | hot prefix | 4 | 1 | 933329 | 933329 | 13.10 | 12.52 | 12.57 | 0.06 | 0.05 |
| `"task"` | hot word | word | 1 | 933329 | 933329 | 0.04 | 0.06 | 0.04 | 0.04 | 0.05 |
| `"git" "ta"*` | lead + hot prefix | 2 | 4 | 1027297 | 1843 | 20.27 | 0.82 | 0.83 | 0.81 | 0.83 |
| `"git" "tas"*` | lead + hot prefix | 3 | 1 | 933329 | 0 | 20.95 | 20.83 | 6.84 | 6.76 | 6.84 |
| `"git" "task"*` | lead + hot prefix | 4 | 1 | 933329 | 0 | 20.81 | 20.68 | 21.06 | 6.77 | 6.77 |
| `"git" "task"` | lead + hot word | word | 1 | 933329 | 0 | 6.77 | 6.68 | 6.97 | 6.82 | 6.79 |
| `"cargo" "ta"*` | lead + hot prefix | 2 | 4 | 1027297 | 103166 | 19.48 | 0.09 | 0.11 | 0.09 | 0.10 |
| `"cargo" "tas"*` | lead + hot prefix | 3 | 1 | 933329 | 103046 | 12.51 | 12.40 | 0.10 | 0.11 | 0.09 |
| `"cargo" "task"*` | lead + hot prefix | 4 | 1 | 933329 | 103046 | 12.56 | 12.51 | 12.60 | 0.12 | 0.10 |
| `"cargo" "task"` | lead + hot word | word | 1 | 933329 | 103046 | 0.07 | 0.10 | 0.09 | 0.09 | 0.10 |
| `"rs"*` | hot prefix | 2 | 1 | 231514 | 231514 | 3.14 | 0.07 | 0.04 | 0.04 | 0.04 |
| `"rs"` | hot word | word | 1 | 231514 | 231514 | 0.03 | 0.05 | 0.05 | 0.04 | 0.05 |
| `"git" "rs"*` | lead + hot prefix | 2 | 1 | 231514 | 7741 | 3.16 | 0.20 | 0.17 | 0.13 | 0.16 |
| `"git" "rs"` | lead + hot word | word | 1 | 231514 | 7741 | 0.12 | 0.17 | 0.16 | 0.14 | 0.16 |
| `"cargo" "rs"*` | lead + hot prefix | 2 | 1 | 231514 | 7750 | 3.17 | 0.23 | 0.21 | 0.19 | 0.20 |
| `"cargo" "rs"` | lead + hot word | word | 1 | 231514 | 7750 | 0.17 | 0.21 | 0.20 | 0.20 | 0.21 |
| `"sr"*` | hot prefix | 2 | 1 | 230588 | 230588 | 3.03 | 0.06 | 0.05 | 0.04 | 0.05 |
| `"src"*` | hot prefix | 3 | 1 | 230588 | 230588 | 3.04 | 3.23 | 0.06 | 0.04 | 0.05 |
| `"src"` | hot word | word | 1 | 230588 | 230588 | 0.03 | 0.06 | 0.04 | 0.04 | 0.03 |
| `"git" "sr"*` | lead + hot prefix | 2 | 1 | 230588 | 7600 | 3.16 | 0.18 | 0.17 | 0.14 | 0.15 |
| `"git" "src"*` | lead + hot prefix | 3 | 1 | 230588 | 7600 | 3.09 | 3.16 | 0.18 | 0.14 | 0.17 |
| `"git" "src"` | lead + hot word | word | 1 | 230588 | 7600 | 0.11 | 0.17 | 0.14 | 0.14 | 0.16 |
| `"cargo" "sr"*` | lead + hot prefix | 2 | 1 | 230588 | 7711 | 3.17 | 0.22 | 0.22 | 0.20 | 0.21 |
| `"cargo" "src"*` | lead + hot prefix | 3 | 1 | 230588 | 7711 | 3.14 | 3.17 | 0.22 | 0.20 | 0.21 |
| `"cargo" "src"` | lead + hot word | word | 1 | 230588 | 7711 | 0.17 | 0.22 | 0.19 | 0.20 | 0.20 |
| `"lo"*` | hot prefix | 2 | 1 | 155681 | 155681 | 2.60 | 0.06 | 0.05 | 0.04 | 0.04 |
| `"log"*` | hot prefix | 3 | 1 | 155681 | 155681 | 2.61 | 2.62 | 0.04 | 0.04 | 0.05 |
| `"log"` | hot word | word | 1 | 155681 | 155681 | 0.03 | 0.04 | 0.04 | 0.04 | 0.05 |
| `"git" "lo"*` | lead + hot prefix | 2 | 1 | 155681 | 6761 | 2.70 | 0.17 | 0.16 | 0.13 | 0.14 |
| `"git" "log"*` | lead + hot prefix | 3 | 1 | 155681 | 6761 | 2.77 | 2.73 | 0.16 | 0.13 | 0.15 |
| `"git" "log"` | lead + hot word | word | 1 | 155681 | 6761 | 0.11 | 0.13 | 0.15 | 0.13 | 0.14 |
| `"cargo" "lo"*` | lead + hot prefix | 2 | 1 | 155681 | 5368 | 2.78 | 0.25 | 0.24 | 0.22 | 0.23 |
| `"cargo" "log"*` | lead + hot prefix | 3 | 1 | 155681 | 5368 | 2.77 | 2.79 | 0.25 | 0.23 | 0.24 |
| `"cargo" "log"` | lead + hot word | word | 1 | 155681 | 5368 | 0.21 | 0.22 | 0.25 | 0.22 | 0.24 |
| `"to"*` | hot prefix | 2 | 1 | 153679 | 153679 | 2.14 | 0.05 | 0.05 | 0.04 | 0.04 |
| `"tom"*` | hot prefix | 3 | 1 | 153679 | 153679 | 2.06 | 2.06 | 0.06 | 0.05 | 0.04 |
| `"toml"*` | hot prefix | 4 | 1 | 153679 | 153679 | 2.05 | 2.05 | 2.06 | 0.05 | 0.05 |
| `"toml"` | hot word | word | 1 | 153679 | 153679 | 0.03 | 0.06 | 0.04 | 0.04 | 0.05 |
| `"git" "to"*` | lead + hot prefix | 2 | 1 | 153679 | 5121 | 2.14 | 0.19 | 0.17 | 0.15 | 0.16 |
| `"git" "tom"*` | lead + hot prefix | 3 | 1 | 153679 | 5121 | 2.16 | 2.15 | 0.18 | 0.15 | 0.16 |
| `"git" "toml"*` | lead + hot prefix | 4 | 1 | 153679 | 5121 | 2.16 | 2.16 | 2.14 | 0.17 | 0.16 |
| `"git" "toml"` | lead + hot word | word | 1 | 153679 | 5121 | 0.14 | 0.18 | 0.17 | 0.15 | 0.17 |
| `"cargo" "to"*` | lead + hot prefix | 2 | 1 | 153679 | 79780 | 2.09 | 0.10 | 0.09 | 0.08 | 0.08 |
| `"cargo" "tom"*` | lead + hot prefix | 3 | 1 | 153679 | 79780 | 2.08 | 2.09 | 0.10 | 0.09 | 0.08 |
| `"cargo" "toml"*` | lead + hot prefix | 4 | 1 | 153679 | 79780 | 2.07 | 2.06 | 2.08 | 0.09 | 0.08 |
| `"cargo" "toml"` | lead + hot word | word | 1 | 153679 | 79780 | 0.06 | 0.08 | 0.08 | 0.08 | 0.08 |
| `"ap"*` | hot prefix | 2 | 2 | 230097 | 230097 | 4.74 | 0.05 | 0.04 | 0.04 | 0.06 |
| `"app"*` | hot prefix | 3 | 1 | 153332 | 153332 | 2.03 | 2.06 | 0.05 | 0.03 | 0.04 |
| `"app"` | hot word | word | 1 | 153332 | 153332 | 0.04 | 0.04 | 0.04 | 0.04 | 0.04 |
| `"git" "ap"*` | lead + hot prefix | 2 | 2 | 230097 | 7708 | 4.73 | 0.17 | 0.14 | 0.13 | 0.16 |
| `"git" "app"*` | lead + hot prefix | 3 | 1 | 153332 | 5067 | 2.13 | 2.13 | 0.17 | 0.15 | 0.16 |
| `"git" "app"` | lead + hot word | word | 1 | 153332 | 5067 | 0.13 | 0.15 | 0.17 | 0.15 | 0.17 |
| `"cargo" "ap"*` | lead + hot prefix | 2 | 2 | 230097 | 7769 | 4.86 | 0.21 | 0.20 | 0.20 | 0.22 |
| `"cargo" "app"*` | lead + hot prefix | 3 | 1 | 153332 | 5147 | 2.19 | 2.23 | 0.25 | 0.22 | 0.23 |
| `"cargo" "app"` | lead + hot word | word | 1 | 153332 | 5147 | 0.21 | 0.22 | 0.23 | 0.23 | 0.24 |
| `"co"*` | hot prefix | 2 | 3 | 188483 | 183019 | 3.68 | 0.06 | 0.04 | 0.04 | 0.04 |
| `"con"*` | hot prefix | 3 | 1 | 153275 | 153275 | 2.09 | 2.03 | 0.04 | 0.03 | 0.04 |
| `"conf"*` | hot prefix | 4 | 1 | 153275 | 153275 | 2.03 | 2.03 | 2.04 | 0.04 | 0.04 |
| `"confi"*` | hot prefix | 5 | 1 | 153275 | 153275 | 2.03 | 2.03 | 2.04 | 2.11 | 2.01 |
| `"config"*` | hot prefix | 6 | 1 | 153275 | 153275 | 2.03 | 2.02 | 2.06 | 2.03 | 2.03 |
| `"config"` | hot word | word | 1 | 153275 | 153275 | 0.03 | 0.04 | 0.05 | 0.04 | 0.05 |
| `"git" "con"*` | lead + hot prefix | 3 | 1 | 153275 | 5049 | 2.11 | 2.13 | 0.17 | 0.15 | 0.16 |
| `"git" "conf"*` | lead + hot prefix | 4 | 1 | 153275 | 5049 | 2.13 | 2.15 | 2.14 | 0.15 | 0.16 |
| `"git" "confi"*` | lead + hot prefix | 5 | 1 | 153275 | 5049 | 2.15 | 2.15 | 2.15 | 2.10 | 2.15 |
| `"git" "config"*` | lead + hot prefix | 6 | 1 | 153275 | 5049 | 2.14 | 2.14 | 2.24 | 2.09 | 2.13 |
| `"git" "config"` | lead + hot word | word | 1 | 153275 | 5049 | 0.12 | 0.14 | 0.18 | 0.14 | 0.17 |
| `"cargo" "co"*` | lead + hot prefix | 2 | 3 | 188483 | 7750 | 3.81 | 0.21 | 0.19 | 0.19 | 0.19 |
| `"cargo" "con"*` | lead + hot prefix | 3 | 1 | 153275 | 5049 | 2.23 | 2.19 | 0.24 | 0.22 | 0.23 |
| `"cargo" "conf"*` | lead + hot prefix | 4 | 1 | 153275 | 5049 | 2.25 | 2.19 | 2.18 | 0.23 | 0.23 |
| `"cargo" "confi"*` | lead + hot prefix | 5 | 1 | 153275 | 5049 | 2.19 | 2.20 | 2.21 | 2.23 | 2.20 |
| `"cargo" "config"*` | lead + hot prefix | 6 | 1 | 153275 | 5049 | 2.27 | 2.19 | 2.21 | 2.21 | 2.17 |
| `"cargo" "config"` | lead + hot word | word | 1 | 153275 | 5049 | 0.21 | 0.21 | 0.24 | 0.23 | 0.25 |
| `"carg"*` | hot prefix | 4 | 1 | 108126 | 108126 | 1.46 | 1.46 | 1.46 | 0.05 | 0.05 |
| `"git" "ca"*` | lead + hot prefix | 2 | 3 | 199590 | 2585 | 4.96 | 0.29 | 0.24 | 0.27 | 0.29 |
| `"git" "car"*` | lead + hot prefix | 3 | 1 | 108126 | 2585 | 1.61 | 1.68 | 0.23 | 0.20 | 0.23 |
| `"git" "carg"*` | lead + hot prefix | 4 | 1 | 108126 | 2585 | 1.63 | 1.64 | 1.63 | 0.21 | 0.22 |
| `"git" "cargo"*` | lead + hot prefix | 5 | 1 | 108126 | 2585 | 1.60 | 1.64 | 1.63 | 1.62 | 1.60 |
| `"git" "cargo"` | lead + hot word | word | 1 | 108126 | 2585 | 0.19 | 0.22 | 0.22 | 0.20 | 0.22 |

### Summary: worst keystroke per schema

| variant | ratio to source | worst p95, keystrokes the TUI sends (last token >= 2 chars or whole) | that query | worst p95, 1-char included | sent keystrokes > 20 ms | sent keystrokes > 10 ms | rows differ |
|---|---|---|---|---|---|---|---|
| baseline | 3.04x | 20.95 | `"git" "tas"*` | 20.95 | 3 of 98 | 9 | 0 |
| prefix='2' | 3.41x | 20.83 | `"git" "tas"*` | 20.83 | 2 of 98 | 6 | 0 |
| prefix='2 3' | 3.76x | 21.06 | `"git" "task"*` | 21.06 | 1 of 98 | 3 | 0 |
| prefix='2 3 4' | 4.02x | 6.82 | `"git" "task"` | 15.26 | 0 of 98 | 0 | 0 |
| prefix='1 2 3 4' | 4.38x | 6.84 | `"git" "tas"*` | 6.84 | 0 of 98 | 0 | 0 |

### Hybrid: prefix index for short prefixes, OR of expanded terms above it (p95 ms)

| schema | OR rewrite for `*` tokens of | worst p95, keystrokes the TUI sends | that query | 2nd worst | most terms in one OR | sent keystrokes > 20 ms | rows differ from plain |
|---|---|---|---|---|---|---|---|
| baseline | >= 2 chars | 7.21 | `"git" "tas"*` | 6.83 (`"git" "task"*`) | 6 | 0 | 0 |
| prefix='2' | >= 3 chars | 6.69 | `"git" "task"` | 6.69 (`"git" "tas"*`) | 2 | 0 | 0 |

### Query rewrites on the baseline schema (p95 ms)

Columns: plain prefix (what Store sends); whole word only (changes results); word OR prefix; exact-first (whole-word page bounds a `rowid >=` range on the prefix query; same rows); per-keystroke fts5vocab term scan; OR of the expanded whole terms given a term list; scan + OR together (same rows).

| query | terms | postings | plain | whole word only | rows kept | word OR prefix | exact-first | rows same | vocab scan | OR of terms | scan + OR | rows same |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `"ta"*` | 4 | 1027297 | 19.76 | 0.01 | 0 of 201 | 19.88 | 19.72 | yes | 11.09 | 0.09 | 11.32 | yes |
| `"tas"*` | 1 | 933329 | 12.55 | 0.01 | 0 of 201 | 12.63 | 12.52 | yes | 9.43 | 0.04 | 9.46 | yes |
| `"task"*` | 1 | 933329 | 12.59 | 0.04 | 201 of 201 | 12.61 | 12.54 | yes | 9.41 | 0.04 | 9.43 | yes |
| `"git" "ta"*` | 4 | 1027297 | 20.38 | 0.03 | 0 of 201 | 20.53 | 20.47 | yes | 11.51 | 1.05 | 11.53 | yes |
| `"git" "tas"*` | 1 | 933329 | 20.83 | 0.02 | 0 of 0 | 20.96 | 20.73 | yes | 9.38 | 6.82 | 16.26 | yes |
| `"git" "task"*` | 1 | 933329 | 20.82 | 6.85 | 0 of 0 | 34.79 | 27.49 | yes | 9.44 | 7.22 | 16.25 | yes |
| `"cargo" "ta"*` | 4 | 1027297 | 19.76 | 0.02 | 0 of 201 | 19.84 | 19.79 | yes | 11.36 | 0.13 | 11.00 | yes |
| `"cargo" "tas"*` | 1 | 933329 | 12.80 | 0.02 | 0 of 201 | 12.70 | 12.51 | yes | 9.37 | 0.07 | 9.50 | yes |
| `"cargo" "task"*` | 1 | 933329 | 12.56 | 0.07 | 201 of 201 | 12.62 | 12.40 | yes | 9.38 | 0.07 | 9.45 | yes |
| `"rs"*` | 1 | 231514 | 3.03 | 0.03 | 201 of 201 | 3.06 | 3.05 | yes | 2.40 | 0.03 | 2.54 | yes |
| `"git" "rs"*` | 1 | 231514 | 3.16 | 0.12 | 201 of 201 | 3.22 | 3.26 | yes | 2.42 | 0.12 | 2.62 | yes |
| `"cargo" "rs"*` | 1 | 231514 | 3.16 | 0.18 | 201 of 201 | 3.28 | 3.39 | yes | 2.52 | 0.18 | 2.63 | yes |
| `"sr"*` | 1 | 230588 | 3.03 | 0.01 | 0 of 201 | 3.01 | 3.01 | yes | 2.57 | 0.03 | 2.59 | yes |
| `"src"*` | 1 | 230588 | 3.03 | 0.03 | 201 of 201 | 3.05 | 3.05 | yes | 2.50 | 0.03 | 2.54 | yes |
| `"git" "sr"*` | 1 | 230588 | 3.40 | 0.02 | 0 of 201 | 3.13 | 3.12 | yes | 2.45 | 0.11 | 2.68 | yes |
| `"git" "src"*` | 1 | 230588 | 3.11 | 0.11 | 201 of 201 | 3.17 | 3.22 | yes | 2.56 | 0.11 | 2.61 | yes |
| `"cargo" "sr"*` | 1 | 230588 | 3.18 | 0.02 | 0 of 201 | 3.16 | 3.17 | yes | 2.56 | 0.18 | 2.73 | yes |
| `"cargo" "src"*` | 1 | 230588 | 3.16 | 0.17 | 201 of 201 | 3.24 | 3.30 | yes | 2.55 | 0.18 | 2.73 | yes |
| `"lo"*` | 1 | 155681 | 2.64 | 0.01 | 0 of 201 | 2.62 | 2.61 | yes | 1.73 | 0.03 | 1.77 | yes |
| `"log"*` | 1 | 155681 | 2.64 | 0.03 | 201 of 201 | 2.66 | 2.66 | yes | 1.74 | 0.03 | 1.85 | yes |
| `"git" "lo"*` | 1 | 155681 | 2.71 | 0.02 | 0 of 201 | 2.72 | 2.73 | yes | 1.66 | 0.11 | 1.78 | yes |
| `"git" "log"*` | 1 | 155681 | 2.71 | 0.11 | 201 of 201 | 2.78 | 2.81 | yes | 1.72 | 0.11 | 1.83 | yes |
| `"cargo" "lo"*` | 1 | 155681 | 2.79 | 0.02 | 0 of 201 | 2.79 | 2.80 | yes | 1.70 | 0.21 | 1.91 | yes |
| `"cargo" "log"*` | 1 | 155681 | 2.82 | 0.21 | 201 of 201 | 2.91 | 3.01 | yes | 1.66 | 0.21 | 1.87 | yes |
| `"to"*` | 1 | 153679 | 2.02 | 0.01 | 0 of 201 | 2.03 | 2.01 | yes | 1.53 | 0.03 | 1.62 | yes |
| `"tom"*` | 1 | 153679 | 2.04 | 0.01 | 0 of 201 | 2.05 | 2.12 | yes | 1.52 | 0.03 | 1.56 | yes |
| `"toml"*` | 1 | 153679 | 2.03 | 0.03 | 201 of 201 | 2.05 | 2.05 | yes | 1.55 | 0.03 | 1.51 | yes |
| `"git" "to"*` | 1 | 153679 | 2.14 | 0.02 | 0 of 201 | 2.14 | 2.20 | yes | 1.54 | 0.14 | 1.66 | yes |
| `"git" "tom"*` | 1 | 153679 | 2.14 | 0.02 | 0 of 201 | 2.14 | 2.08 | yes | 1.55 | 0.15 | 1.70 | yes |
| `"git" "toml"*` | 1 | 153679 | 2.14 | 0.15 | 201 of 201 | 2.24 | 2.23 | yes | 1.55 | 0.14 | 1.66 | yes |
| `"cargo" "to"*` | 1 | 153679 | 2.05 | 0.02 | 0 of 201 | 2.06 | 2.03 | yes | 1.52 | 0.05 | 1.61 | yes |
| `"cargo" "tom"*` | 1 | 153679 | 2.04 | 0.02 | 0 of 201 | 2.07 | 2.06 | yes | 1.52 | 0.06 | 1.57 | yes |
| `"cargo" "toml"*` | 1 | 153679 | 2.06 | 0.05 | 201 of 201 | 2.10 | 2.06 | yes | 1.53 | 0.06 | 1.57 | yes |
| `"ap"*` | 2 | 230097 | 4.79 | 0.01 | 0 of 201 | 4.71 | 4.74 | yes | 2.72 | 0.05 | 2.61 | yes |
| `"app"*` | 1 | 153332 | 2.00 | 0.03 | 201 of 201 | 2.10 | 2.05 | yes | 1.67 | 0.04 | 1.73 | yes |
| `"git" "ap"*` | 2 | 230097 | 4.78 | 0.02 | 0 of 201 | 4.80 | 4.82 | yes | 2.55 | 0.16 | 2.71 | yes |
| `"git" "app"*` | 1 | 153332 | 2.11 | 0.13 | 201 of 201 | 2.18 | 2.23 | yes | 1.67 | 0.13 | 1.90 | yes |
| `"cargo" "ap"*` | 2 | 230097 | 4.93 | 0.02 | 0 of 201 | 4.86 | 4.85 | yes | 2.50 | 0.22 | 2.77 | yes |
| `"cargo" "app"*` | 1 | 153332 | 2.19 | 0.22 | 201 of 201 | 2.31 | 2.41 | yes | 1.68 | 0.22 | 1.88 | yes |
| `"co"*` | 3 | 188483 | 3.70 | 0.01 | 0 of 201 | 3.71 | 3.68 | yes | 2.11 | 0.07 | 2.21 | yes |
| `"con"*` | 1 | 153275 | 2.00 | 0.01 | 0 of 201 | 2.01 | 2.01 | yes | 1.64 | 0.03 | 1.66 | yes |
| `"conf"*` | 1 | 153275 | 2.00 | 0.01 | 0 of 201 | 2.02 | 2.01 | yes | 1.58 | 0.03 | 1.60 | yes |
| `"confi"*` | 1 | 153275 | 2.02 | 0.01 | 0 of 201 | 2.04 | 2.06 | yes | 1.62 | 0.03 | 1.66 | yes |
| `"config"*` | 1 | 153275 | 2.04 | 0.03 | 201 of 201 | 2.06 | 2.05 | yes | 1.56 | 0.03 | 1.61 | yes |
| `"git" "con"*` | 1 | 153275 | 2.13 | 0.02 | 0 of 201 | 2.13 | 2.12 | yes | 1.63 | 0.12 | 1.68 | yes |
| `"git" "conf"*` | 1 | 153275 | 2.13 | 0.02 | 0 of 201 | 2.14 | 2.11 | yes | 1.59 | 0.12 | 1.70 | yes |
| `"git" "confi"*` | 1 | 153275 | 2.15 | 0.03 | 0 of 201 | 2.15 | 2.11 | yes | 1.63 | 0.12 | 1.76 | yes |
| `"git" "config"*` | 1 | 153275 | 2.13 | 0.12 | 201 of 201 | 2.21 | 2.40 | yes | 1.69 | 0.13 | 1.75 | yes |
| `"cargo" "co"*` | 3 | 188483 | 3.82 | 0.02 | 0 of 201 | 3.87 | 3.78 | yes | 2.13 | 0.22 | 2.35 | yes |
| `"cargo" "con"*` | 1 | 153275 | 2.20 | 0.02 | 0 of 201 | 2.20 | 2.20 | yes | 1.56 | 0.21 | 1.84 | yes |
| `"cargo" "conf"*` | 1 | 153275 | 2.18 | 0.02 | 0 of 201 | 2.20 | 2.20 | yes | 1.64 | 0.21 | 1.87 | yes |
| `"cargo" "confi"*` | 1 | 153275 | 2.27 | 0.03 | 0 of 201 | 2.26 | 2.19 | yes | 1.60 | 0.20 | 1.83 | yes |
| `"cargo" "config"*` | 1 | 153275 | 2.21 | 0.21 | 201 of 201 | 2.34 | 2.35 | yes | 1.56 | 0.21 | 2.00 | yes |
| `"carg"*` | 1 | 108126 | 1.52 | 0.01 | 0 of 201 | 1.49 | 1.52 | yes | 1.13 | 0.03 | 1.13 | yes |
| `"git" "ca"*` | 3 | 199590 | 4.96 | 0.03 | 0 of 201 | 10.93 | 4.97 | yes | 2.41 | 0.36 | 2.51 | yes |
| `"git" "car"*` | 1 | 108126 | 1.66 | 0.03 | 0 of 201 | 1.67 | 1.62 | yes | 1.13 | 0.19 | 1.30 | yes |
| `"git" "carg"*` | 1 | 108126 | 1.62 | 0.02 | 0 of 201 | 1.63 | 1.66 | yes | 1.14 | 0.19 | 1.34 | yes |
| `"git" "cargo"*` | 1 | 108126 | 1.87 | 0.25 | 201 of 201 | 1.79 | 1.78 | yes | 1.10 | 0.19 | 1.30 | yes |
| `"c"*` | 14 | 560579 | 17.44 | 0.01 | 0 of 201 | 17.50 | 17.19 | yes | 6.34 | 0.29 | 6.63 | yes |
| `"t"*` | 8 | 1412133 | 37.15 | 0.01 | 0 of 201 | 36.97 | 35.61 | yes | 15.19 | 0.18 | 15.25 | yes |
| `"1"*` | 1111 | 103587 | 6.38 | 0.02 | 0 of 201 | 6.42 | 6.41 | yes | 1.60 | 6.49 | 8.04 | yes |
| `"12"*` | 111 | 10314 | 0.50 | 0.01 | 3 of 201 | 0.49 | 0.53 | yes | 0.16 | 0.61 | 0.75 | yes |

### Cost model: single-token prefix on the baseline schema

| query | terms | postings | p50 ms | p95 ms | ns per posting (p50) |
|---|---|---|---|---|---|
| `"12"*` | 111 | 10314 | 0.48 | 0.50 | 46.4 |
| `"1"*` | 1111 | 103587 | 6.34 | 6.38 | 61.2 |
| `"carg"*` | 1 | 108126 | 1.47 | 1.52 | 13.5 |
| `"con"*` | 1 | 153275 | 1.96 | 2.00 | 12.8 |
| `"conf"*` | 1 | 153275 | 1.97 | 2.00 | 12.8 |
| `"confi"*` | 1 | 153275 | 1.98 | 2.02 | 12.9 |
| `"config"*` | 1 | 153275 | 2.00 | 2.04 | 13.1 |
| `"app"*` | 1 | 153332 | 1.98 | 2.00 | 12.9 |
| `"to"*` | 1 | 153679 | 1.98 | 2.02 | 12.9 |
| `"tom"*` | 1 | 153679 | 2.01 | 2.04 | 13.1 |
| `"toml"*` | 1 | 153679 | 2.01 | 2.03 | 13.1 |
| `"lo"*` | 1 | 155681 | 2.61 | 2.64 | 16.7 |
| `"log"*` | 1 | 155681 | 2.62 | 2.64 | 16.8 |
| `"co"*` | 3 | 188483 | 3.63 | 3.70 | 19.3 |
| `"ap"*` | 2 | 230097 | 4.64 | 4.79 | 20.2 |
| `"sr"*` | 1 | 230588 | 2.98 | 3.03 | 12.9 |
| `"src"*` | 1 | 230588 | 2.99 | 3.03 | 13.0 |
| `"rs"*` | 1 | 231514 | 2.96 | 3.03 | 12.8 |
| `"c"*` | 14 | 560579 | 17.03 | 17.44 | 30.4 |
| `"tas"*` | 1 | 933329 | 12.29 | 12.55 | 13.2 |
| `"task"*` | 1 | 933329 | 12.33 | 12.59 | 13.2 |
| `"ta"*` | 4 | 1027297 | 19.22 | 19.76 | 18.7 |
| `"t"*` | 8 | 1412133 | 35.98 | 37.15 | 25.5 |

## taskid corpus (fixture with `# task-N` renamed `# taskid-N`)

Full term list through fts5vocab (what an in-memory term cache would load): 10098 terms, 86.11 / 86.96 / 86.96 ms (p50 / p95 / max, 10 runs)

### Top 10 tokens by document frequency (fts5vocab 'row', 10098 distinct terms)

| # | token | chars | rows containing it | share of rows |
|---|---|---|---|---|
| 1 | `taskid` | 6 | 933329 | 93.3% |
| 2 | `rs` | 2 | 231514 | 23.2% |
| 3 | `src` | 3 | 230588 | 23.1% |
| 4 | `log` | 3 | 155681 | 15.6% |
| 5 | `toml` | 4 | 153679 | 15.4% |
| 6 | `app` | 3 | 153332 | 15.3% |
| 7 | `config` | 6 | 153275 | 15.3% |
| 8 | `cargo` | 5 | 108126 | 10.8% |
| 9 | `go` | 2 | 107419 | 10.7% |
| 10 | `integration` | 11 | 77413 | 7.7% |

### Every keystroke (p95 ms; `!` marks rows that differ from taskid baseline)

| query | class | `*` chars | terms under last token | postings under last token | matches | taskid baseline | taskid prefix='2' | taskid prefix='2 3 4' |
|---|---|---|---|---|---|---|---|---|
| `"ta"*` | hot prefix | 2 | 4 | 1027297 | 935172 | 19.75 | 0.05 | 0.04 |
| `"tas"*` | hot prefix | 3 | 1 | 933329 | 933329 | 12.54 | 12.43 | 0.05 |
| `"task"*` | hot prefix | 4 | 1 | 933329 | 933329 | 12.66 | 12.53 | 0.06 |
| `"taski"*` | hot prefix | 5 | 1 | 933329 | 933329 | 12.55 | 12.47 | 13.25 |
| `"taskid"*` | hot prefix | 6 | 1 | 933329 | 933329 | 12.80 | 12.43 | 12.74 |
| `"taskid"` | hot word | word | 1 | 933329 | 933329 | 0.05 | 0.06 | 0.05 |
| `"git" "ta"*` | lead + hot prefix | 2 | 4 | 1027297 | 1843 | 20.94 | 0.82 | 0.83 |
| `"git" "tas"*` | lead + hot prefix | 3 | 1 | 933329 | 0 | 24.48 | 20.70 | 6.98 |
| `"git" "task"*` | lead + hot prefix | 4 | 1 | 933329 | 0 | 21.52 | 20.85 | 7.14 |
| `"git" "taski"*` | lead + hot prefix | 5 | 1 | 933329 | 0 | 21.57 | 27.05 | 21.23 |
| `"git" "taskid"*` | lead + hot prefix | 6 | 1 | 933329 | 0 | 21.29 | 20.85 | 21.21 |
| `"git" "taskid"` | lead + hot word | word | 1 | 933329 | 0 | 6.98 | 6.48 | 6.90 |
| `"cargo" "ta"*` | lead + hot prefix | 2 | 4 | 1027297 | 103166 | 20.19 | 0.09 | 0.10 |
| `"cargo" "tas"*` | lead + hot prefix | 3 | 1 | 933329 | 103046 | 13.00 | 12.43 | 0.11 |
| `"cargo" "task"*` | lead + hot prefix | 4 | 1 | 933329 | 103046 | 12.99 | 12.37 | 0.12 |
| `"cargo" "taski"*` | lead + hot prefix | 5 | 1 | 933329 | 103046 | 12.70 | 12.83 | 12.78 |
| `"cargo" "taskid"*` | lead + hot prefix | 6 | 1 | 933329 | 103046 | 12.51 | 12.91 | 12.37 |
| `"cargo" "taskid"` | lead + hot word | word | 1 | 933329 | 103046 | 0.10 | 0.09 | 0.09 |

### Summary: worst keystroke per schema

| variant | ratio to source | worst p95, keystrokes the TUI sends (last token >= 2 chars or whole) | that query | worst p95, 1-char included | sent keystrokes > 20 ms | sent keystrokes > 10 ms | rows differ |
|---|---|---|---|---|---|---|---|
| taskid baseline | 3.07x | 24.48 | `"git" "tas"*` | 24.48 | 6 of 18 | 15 | 0 |
| taskid prefix='2' | 3.45x | 27.05 | `"git" "taski"*` | 27.05 | 4 of 18 | 12 | 0 |
| taskid prefix='2 3 4' | 4.06x | 21.23 | `"git" "taski"*` | 21.23 | 2 of 18 | 6 | 0 |

### Hybrid: prefix index for short prefixes, OR of expanded terms above it (p95 ms)

| schema | OR rewrite for `*` tokens of | worst p95, keystrokes the TUI sends | that query | 2nd worst | most terms in one OR | sent keystrokes > 20 ms | rows differ from plain |
|---|---|---|---|---|---|---|---|
| taskid baseline | >= 2 chars | 12.84 | `"git" "tas"*` | 6.64 (`"git" "taskid"*`) | 4 | 0 | 0 |
| taskid prefix='2' | >= 3 chars | 6.55 | `"git" "tas"*` | 6.55 (`"git" "taskid"*`) | 1 | 0 | 0 |

load after: { 10.17 8.42 7.35 }

</details>
