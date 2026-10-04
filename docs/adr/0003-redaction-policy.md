# 0003. Redact with a precision-leaning structural denylist, typed idempotent markers, rescrub on revision change, and owner-only files

Status: Accepted · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

- Standing rule from [Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1): redact before SQLite, FTS, logs, caches and model egress; never persist raw source artifacts. Research extends it to compiled assets (`docs/research/source-evidence-contracts.md:219`). No detector proves absence; product text says "policy redacted", never "secret-free" (`:231`).
- Master redactor (`src/redact.rs`) misses every D1 case (URL userinfo, `-p<pw>`, `--password=`, `sshpass -p`, lower/mixed-case secret names, short Bearer tokens) and redacts git SHAs, digests and long paths ([[D1] Fix the e2e dogfood findings from the workspace prototype](https://github.com/yuann3/relic/issues/22)); a probe adds UUIDs and mixed-case paths. Redacting a SHA or path breaks search and recall; missing a secret leaks it to SQLite and possibly a provider.
- The `fix/redactor` WIP (hand-written span detectors, ~740 non-test lines, no dependencies) passes 79/80 corpus cases; the PEM detector is defined but never called. A probe found it still misses `echo pw | sudo -S`, `htpasswd -b`, `openssl -k`, `gh secret set --body`, and base64 containing `/` after an unknown `--*-key` flag — the last a regression against master.
- The WIP is not idempotent: a second pass rewrites a real `[REDACTED]` (e.g. from a vendor-prefix token) to `[redacted]`. A user-typed `[REDACTED]` and a real one both index as FTS token `redacted`; `[REDACTED:password]` indexes as `redacted` + `password` (fts5vocab probe).
- Research names aho-corasick (primary) and regex-automata, gated on binary size, and permits a hand-written set if they break the gate (`docs/research/storage-search-runtime-architecture.md:145-159`). Import time with production redaction and the engine's binary delta are unmeasured.
- `redaction_revision` is hard-coded `'r1'` (`src/store/mod.rs:83`). Every master-imported DB holds the secrets the master redactor missed. Freed SQLite pages, FTS5 segments and the WAL can retain old text.
- Under umask 022, master creates the DB 0644 and the data dir 0755 (`src/main.rs:45-50`); read commands also create `-wal`/`-shm`. Bundled SQLite gives `-wal`/`-shm` the DB's mode (verified on the release binary). `fix/cli` sets umask 077 on import only and does not tighten an existing data dir. The map allows user-approved verifiers and probes, which would inherit Relic's umask.

## Decision

Policy:
- Precision-leaning structural denylist. Redact known secret carriers: secret names, flags, headers, URL userinfo, vendor formats, JWT/PEM, and random-looking runs in secret context. Keep SHAs, digests, UUIDs and paths.
- Add rules for: `sudo -S` stdin echo, `htpasswd -b`, `openssl -k` / `-pass pass:`, `gh secret set --body`, wire the PEM detector, and restore coverage of base64 containing `/` after unknown `--*-key` / `--*-secret` flags.
- User-config deny/allow patterns are deferred.

Engine and interface:
- Hand-written detectors now. Only the interface is locked: spans in, markers out, idempotent on existing well-formed markers, and a `redaction_revision` constant.
- aho-corasick / regex-automata may replace the engine later, behind a measured gate ([0015](0015-dependency-envelope.md)).

Marker:
- Typed `[REDACTED:<kind>]`.
- A user-typed literal that looks like a marker is escaped at import.
- Well-formed markers are never re-redacted.

Gate: importing 100k records with production redaction takes <1 s.

Rescrub (on a `redaction_revision` change):
1. Re-redact stored safe text into a new revision.
2. Purge the old revision's rows.
3. Rebuild FTS.
4. VACUUM with secure delete.
5. Checkpoint the WAL.

Citations to purged rows become stale-handle errors. This is the one deliberate exception to "old evidence stays addressable". `PRAGMA secure_delete=ON` (plus FTS5 secure-delete if bundled SQLite 3.49.1 supports it) applies from v0.1 on, once its cost on a 1M store is measured.

File permissions:
- `umask(0o077)` at the start of `main`, for every command.
- On a write open, tighten an existing data dir and the db/`-wal`/`-shm` files to owner-only.
- User-approved verifier and probe subprocesses get the user's original umask restored (`pre_exec`).
- Warn, don't refuse, on foreign ownership. Refusing group- or world-writable stores is deferred.

## Amends

- `docs/research/storage-search-runtime-architecture.md:145-159` (under [Choose the storage, search, and runtime architecture](https://github.com/yuann3/relic/issues/4)): the engine is hand-written detectors by default, not aho-corasick. The regex machinery becomes a later, measured option.
- `docs/research/source-evidence-contracts.md:173, :257` (under [Specify source and evidence contracts](https://github.com/yuann3/relic/issues/3)): a redaction contract change no longer means "new revision and full reparse; old evidence remains addressable". It now means a rescrub from stored safe text, and the old revision is purged. The new revision is derived and never edited in place, so the "immutable safe observations" decision still holds.

## Consequences

- S1b (redaction) owns policy, typed markers, rescrub, secure delete and umask ([0014](0014-performance-contract.md) gate ownership). It must prove:
  - a corpus test covering every new rule, the PEM case, the `--*-key`/`--*-secret` base64-with-`/` case, and SHA/digest/UUID/path preservation;
  - `redact(redact(x)) == redact(x)` over the corpus, including vendor-prefix markers;
  - a user-typed marker literal survives import distinguishable from a real marker;
  - 100k and 1M import with production redaction (100k <1 s), the redaction binary delta, and the secure-delete cost on a 1M store;
  - a rescrub test: after a revision bump, old secret bytes are absent from the DB, `-wal` and FTS, and a citation to a purged row is a stale-handle error;
  - a permissions test: a pre-existing 0755 data dir and 0644 DB/`-wal`/`-shm` under umask 022 end up owner-only after a write open.
- Markers feed the egress seam's re-redaction and the privacy canary ([0004](0004-egress-seam-and-privacy-canary.md)). Idempotence is what keeps that re-redaction from demoting real markers.
- FTS matches the marker kind (a search for `password` hits rows with a redacted password). This is accepted.
- The validation slice spawns the verifier with the original umask ([0012](0012-validation-boundary.md)).
- The read path is read-only ([0005](0005-evidence-store-transactions-and-read-only-open.md)), so a rescrub runs only on a writer path.

## Rejected alternatives

- Recall-leaning denylist plus aggressive generic entropy: wipes SHAs, digests, UUIDs and build IDs, so evidence-cited recall breaks (master's failure).
- Allowlist of known-safe token shapes: mangles hostnames, package names and prose; evidence quality collapses.
- User-config deny/allow patterns now: adds config surface and a revision bump per config change. Deferred, not rejected outright.
- Curated regex ruleset (aho-corasick/regex-automata) now: binary and RSS cost unmeasured, and it handles shell quoting worse than the WIP.
- Untyped `[REDACTED]` with a `[redacted]` escape only: indistinguishable in FTS and to the model, and breaks re-redaction.
- Out-of-band span list per observation: correct, but schema, provenance and DB-size cost for a rare collision.
- Require a re-import, leaving old revisions: old secrets stay forever, violating redact-before-SQLite.
- Refuse to open an old-revision DB until the user runs `relic rescrub`: blocking UX and an extra command.
- Explicit file modes only (DirBuilder 0700, pre-created 0600 DB): every future writer must remember the rule.
- Fail closed on group/world-writable or foreign-owned stores: can lock users out of legitimate setups. Deferred.

## Open facts

- Import time with production redaction at 100k and 1M records, and the redaction engine's binary delta (S1b).
- Cost of `PRAGMA secure_delete=ON` and VACUUM on a 1M store, and whether bundled SQLite 3.49.1 (libsqlite3-sys 0.33) supports FTS5 secure-delete (S1b).
- Which write command triggers rescrub, and how long it takes on a 1M store (S1b).
- False-positive and false-negative rates on a realistic privacy-safe corpus. Current evidence is 80 hand-picked cases plus a probe of about 25.
