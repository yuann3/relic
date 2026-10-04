# 0005. Each import revision commits in one transaction, and readers open the store read-only and check `user_version`

Status: Accepted · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

- **Imports are not atomic.** Master inserts the revision row outside any transaction and commits each 10k-record batch separately. It sets `complete=1` at the end, but no search query filters on it. An interrupted import is therefore searchable, and a retry duplicates it (`src/import.rs:62-94`, `src/store/mod.rs:79-96, 148-182, 225-255`). [Specify source and evidence contracts](https://github.com/yuann3/relic/issues/3) requires that a crash before commit exposes neither the revision nor its FTS rows (`docs/research/source-evidence-contracts.md:280`). The storage research prefers one full transaction (`docs/research/storage-search-runtime-architecture.md:163`), with `busy_timeout = 250` (:191). A 1M-record import takes about 2.7 s ([Prove the ingestion and search performance envelope](https://github.com/yuann3/relic/issues/8)). Production `relic import` never checkpoints; only the benches call `Store::checkpoint`.
- **Readers write.** `search` and both TUI connections use `Store::open`, which runs `journal_mode=WAL`, `CREATE … IF NOT EXISTS` and `INSERT OR IGNORE INTO schema_meta` (`src/store/schema.rs:6-27`, `src/main.rs:68`, `src/ui/mod.rs:51, 354`). That stamps Relic's schema into foreign files and fails with `database is locked` while an import writes. The WIP `fix/tui` works around the lock with a `WorkerStore` that reopens with 100 ms–5 s backoff.
- **Probes** were run on SQLite 3.53.4. Relic bundles 3.49.1.
  - A `mode=ro` reader reads while another connection holds `BEGIN IMMEDIATE`.
  - A `mode=ro` open in a read-only directory with no `-shm` fails.
  - A `mode=ro` open of a 0444 DB leaves 0444 `-shm`/`-wal` files behind. Once the DB is made writable again, the next writer fails.
  - `immutable=1` reads the read-only-directory case, but SQLite documents that it can return stale or inconsistent data when a crashed writer left WAL content behind.
- **Versioning.** Today a `schema_meta` row records the version, there is no `user_version` and no migration code. The research prescribes numbered forward-only migrations run under an exclusive transaction (`storage-search-runtime-architecture.md:269`). After [0002](0002-source-identity-and-revisions.md), the store holds history its sources no longer contain.

## Decision

**Atomic import**
- One transaction per revision: the revision row, its observations and its FTS rows commit together. An anchor-aligned append to an existing revision ([0002](0002-source-identity-and-revisions.md)) is likewise one transaction.
- Drop the `complete` column.
- A concurrent importer gets a typed Busy error after the 250 ms `busy_timeout`. WAL readers are unaffected.
- Checkpoint the WAL after a large import.

**Read-only open**
- `search` and both TUI connections open with `SQLITE_OPEN_READ_ONLY`, issue no DDL, and set only reader pragmas.
- An unwritable DB file or directory gets a clear error. There is no `immutable=1` fallback. Read-only media are deferred.

**Versioning**
- `PRAGMA user_version` is the version readers check.
- Only the import writer runs migrations. They are numbered, forward-only, and each runs in an exclusive transaction.
- A reader refuses a mismatched version and prints a hint.
- Prototype DBs are discarded. v0.1 ships at version 1 with zero migrations.

**Durability and corruption**
- `synchronous=NORMAL` stays. Power loss in WAL mode can lose the last committed transaction, but its content is still in the source file (shells trim the oldest lines), so the next import restores it.
- A corrupt DB is never deleted. It is quarantined by rename and kept, the store is rebuilt from sources, and the user is told that older history is in the quarantined file. The detailed recovery design is not yet specified; S6 owns it.

**Recovery busy**
- Before anything else, the read-path slice establishes one fact: on bundled SQLite 3.49.1, can a read-only WAL reader get `SQLITE_BUSY` during recovery after a writer crash?
- If yes, readers keep exactly one bounded retry that leaves a visible trace. There is no backoff loop.
- The `fix/tui` `WorkerStore` reopen/backoff loop is not carried over. If the answer is yes, the single bounded retry is all that replaces it.

## Amends

- `storage-search-runtime-architecture.md:204` asked for write transactions short enough that another invocation gets a typed Busy quickly. Now one import holds the write lock for its whole revision (about 2.7 s at 1M), and the 250 ms `busy_timeout` supplies the quick typed Busy instead.
- `storage-search-runtime-architecture.md:270` treated the observation index as rebuildable from approved sources. After [0002](0002-source-identity-and-revisions.md) that no longer holds, so a version mismatch is refused, never rebuilt.
- `storage-search-runtime-architecture.md:49, :189` justified `synchronous=NORMAL` by a rebuildable index. NORMAL stays, justified instead by the source file still holding the last transaction's content.
- `storage-search-runtime-architecture.md:273` quarantined and rebuilt a corrupt DB on the same assumption. The quarantined file is now kept and the user told, because a rebuild cannot restore trimmed history.

## Consequences

- Slice S1a owns the atomic import, alongside [0002](0002-source-identity-and-revisions.md). Its proofs:
  - A failure injected mid-import leaves no revision, observation or FTS row, and a retry stores everything once.
  - `search` returns results while an import is writing.
  - A second importer gets a typed Busy error, which exits 2 with the busy message ([0011](0011-terminal-and-cli-contract.md)).
- Slice S1c owns the read-only open and `user_version`. Its proofs:
  - `search` and the TUI never write. A foreign SQLite file or a mismatched `user_version` is refused with a hint.
  - An unwritable file or directory errors clearly and leaves no side files behind.
  - The recovery-busy fact is established before the worker code is deleted.
- Readers no longer contend for the write lock, so the TUI worker needs no reopen loop.
- Readers never create or migrate the DB. To a reader, a missing or foreign store is an error.
- During an import the WAL grows by the import's size until the checkpoint.
- Query interruption uses the SQLite progress handler ([0006](0006-search-ordering-and-query-budget.md)). Secure delete and VACUUM on rescrub are in [0003](0003-redaction-policy.md).

## Rejected alternatives

- Keep batch commits, filter on `complete=1` and clean up later: puts a check on every FTS hit, lets incomplete rows take LIMIT slots, and needs cleanup logic.
- Stage into a temp table, then swap in one short transaction: doubles write I/O and adds machinery.
- `immutable=1` fallback for unwritable files (WIP `fix/store-read-path`): returns silently stale results after a writer crash. Refusing also avoids the measured 0444 side-file trap.
- Read-write open with `PRAGMA query_only=1`: still fails in a read-only directory, and correctness depends on never forgetting the pragma.
- Rebuild on version mismatch: loses trimmed history and superseded revisions that the sources no longer contain.
- `synchronous=FULL` on the observation DB: pays durability syncs for a transaction the next import restores from the source.
- Delete a corrupt DB and rebuild: loses every record the sources have trimmed.
- No versioning until after v0.1: readers could not detect an incompatible DB, and the read-only open needs that check.

## Open facts

- Whether a read-only WAL reader can get `SQLITE_BUSY` during recovery after a writer crash on bundled 3.49.1 (S1c, before deleting the worker backoff).
- The probes ran on 3.53.4. S1c's tests must confirm the read-only behaviours on bundled 3.49.1.
- The detailed corruption-recovery design (S6).
- The import size that counts as "large" enough to trigger a checkpoint, and WAL bytes at 1M records. Neither has been measured.
