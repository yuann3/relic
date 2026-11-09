# 0002. A source is its resolved path, and Relic keeps every imported record when the file is rewritten

Status: Accepted · Date: 2026-10-04 · Lock: [Lock the Relic v0.1 architecture](https://github.com/yuann3/relic/issues/11)

## Context

- **Identity.** Master keys a source by the path string as typed (`src/store/mod.rs:54-71`). As a result `./h`, `$PWD/h` and a symlink become three sources and three full copies ([\[D1\] Fix the e2e dogfood findings from the workspace prototype](https://github.com/yuann3/relic/issues/22)). Inode cannot serve as identity: with zsh's default `HIST_SAVE_BY_COPY`, every trim renames a new file over the history. A probe of zsh 5.9 showed `fc -W` replacing a symlinked `HISTFILE` with a regular file and leaving the target stale. macOS bash 3.2 writes through the link in place. [Specify source and evidence contracts](https://github.com/yuann3/relic/issues/3) says not to follow a final symlink (`docs/research/source-evidence-contracts.md:162, :250`). The user's own `~/.zsh_history` is a regular file.
- **Rewrites are routine.** zsh trims once the file grows 20% past `SAVEHIST`. bash truncates to `HISTFILESIZE` lines (default 500, timestamp lines included). Master detects change by file length only (`src/import.rs:36-50`), so it misses same-length rewrites and re-imports the whole file whenever the length changes. The WIP `47d9678` deletes every stored row on any non-append change, and its tests treat a removed line as a user scrubbing a secret.
- **Raw-derived hashes leak.** `keyed_fingerprint` is an unkeyed 8-byte FNV-1a of the raw command (`src/import.rs:111-138`). The surrounding safe text is stored in clear, so anyone holding the DB can brute-force a short redacted secret.
- **Memory.** Import reads the whole file with `fs::read`, builds a `Vec<RawRecord>`, and commits in 10k batches. The 353 MiB peak from [Prove the ingestion and search performance envelope](https://github.com/yuann3/relic/issues/8) came from one bench process that generated five fixtures and imported through `fs::read`; single-import RSS was never measured. D1 found a 2 MB line, unbounded reads of non-regular files, and bash marker mode chosen by sniffing the first 10 lines.
- **zsh metafication.** zsh 5.9 writes 部 (`E9 83 A8`) as `E9 83 A3 A8`. Master decodes it lossily with no unmetafy step (`src/source/zsh.rs:43-65`). The source contract requires unmetafying before charset decoding and turning dangling Meta into bounded warnings plus lossy marking (`source-evidence-contracts.md:45-47`).
- **Shell kind.** `--shell` defaults to zsh (`src/cli.rs:25, 76-93`), and the kind is frozen on first import, so `~/.bash_history` is mis-parsed with no fix.
- **Numbers.** The unchanged-source gate is <20 ms and the 100-record incremental gate is <25 ms. The 0.41 ms "incremental" figure came from a fresh 100-record file, so appending to a large store has never been measured. A full import costs 242 ms per 100k records. BLAKE2b, measured unkeyed during the lock (keying does not change throughput), takes 2.2 ms over 3.4 MB (100k records) and 38 ms over 55 MB (1M). BLAKE3 is unmeasured.

## Decision

**Identity**
- Resolve parent directories and keep the final file name. Open with `O_NOFOLLOW`. A final symlink is refused with a message that names its target.
- `logical_id` is random, assigned at first import. A source is looked up by that resolved path, stored in the private locator. The raw path lives only there.
- `(dev, ino)` is a change hint only, never identity.

**Change detection**
- The unchanged check is the stat hint alone: `(dev, ino, size, mtime_ns, ctime_ns)` equal means stop.
- On change, compute keyed BLAKE3 over the old prefix `[0, stable_len)`. If it matches, parse only `[stable_len, len)`. Never commit a partial last record.
- If keyed BLAKE3 over a 1M-record history cannot fit the <25 ms incremental gate, fall back to the source contract's keyed boundary guard: a keyed hash of the last K bytes before `stable_len`.
- A prefix mismatch is a rewrite and goes to anchor alignment.

**Rewrite: anchor-align, else supersede**
- The anchor is the last N records before the stored boundary, matched in the new file by safe-text hash plus occurrence context. If the anchor is found uniquely and unambiguously, every record after it is appended to the existing revision.
- With no unique, unambiguous anchor, supersede: create a new revision and keep the old one.
- Removing a line from the history file never removes it from Relic.
- Only three paths delete evidence: an explicit, logged `relic forget` purge (it may land in a later slice), shell-kind correction (below), and rescrub ([0003](0003-redaction-policy.md)).

**Keys and hashes**
- Delete the per-record `keyed_fingerprint`. Alignment uses safe-text hash plus occurrence context.
- The installation key is 32 random bytes from `getrandom`, kept in a 0600 file beside the DB. It keys only the prefix digest and its boundary-guard fallback. It protects only against a leak of the DB file alone. Losing it is harmless: the next import falls back to keyless anchor alignment.
- Since then the key has two more uses, both keyed hashes of stored safe text, never of raw bytes: a skill's durable citations ([0013](0013-skill-catalog-direction.md)), and the mark `relic forget` keeps of a Git ref ([S13](https://github.com/yuann3/relic/issues/38)). Losing it now also costs those: citations go stale, and a forgotten Git ref comes back on the next import of its repository until it is forgotten again.
- Evidence handles are random per investigation and use no key.

**Streaming parser**
- The parser is an iterator over `impl BufRead` that yields owned records, with a hard per-record byte cap and no batch `Vec`.
- bash marker mode is detected per record, which removes the 10-line sniff.

**zsh unmetafy**
- Unmetafy in the byte reader, before CRLF and continuation handling.
- A dangling Meta produces a bounded warning and the record's lossy flag, never a silent U+FFFD.
- Bump the zsh parser contract.

**Shell kind**
- Detect from content first (`: n:n;` means zsh, `#<epoch>` means bash), then from the file name. Refuse an ambiguous file unless `--shell` is given.
- A later `--shell` that differs from the stored kind explicitly replaces that source and logs the replacement on stderr.

## Amends

Standing decision [Specify source and evidence contracts](https://github.com/yuann3/relic/issues/3), research `docs/research/source-evidence-contracts.md`:
- :96 and :155. Event fingerprints are removed rather than keyed. Displayed evidence handles are random per investigation rather than keyed BLAKE3. Logical source IDs are random rather than keyed; lookup is by resolved path in the private locator.
- :173. The keyed whole-prefix hash replaces the guard window, which survives only as the measured fallback. Truncation and inode replacement now go to anchor alignment before any new revision or full reparse.
- :175 and :181. Alignment matches safe-text hash plus occurrence context instead of keyed record fingerprints. The "only when unambiguous" rule stands.
- :187. The <25 ms incremental gate in [Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1) overrides the preference to ship a full reparse first.
- :226, step 6. No keyed identity is computed over canonical private input per record.

Research for [Specify the bounded RLM and model boundary](https://github.com/yuann3/relic/issues/5), `docs/research/rlm-model-boundary.md:83`: the keyed-BLAKE3 `EvidenceId` construction becomes random per investigation. Its properties are unchanged: opaque, no paths or ordinals, and a typed error across investigations.

## Consequences

- Slice S1a (import integrity) owns all of this, together with the atomic transaction in [0005](0005-evidence-store-transactions-and-read-only-open.md). The `fix/import-pipeline` tests that encode replace-on-rewrite (`import_truncated_file_drops_removed_records`, `import_same_length_rewrite_replaces_old_records`) are wrong under this ADR. Invert them; do not merge them.
- Deleting a line from a history file is not a scrub. `relic forget` is the scrub.
- A zsh trim adds only genuinely new records, so rowid order stays meaningful for newest-first search ([0006](0006-search-ordering-and-query-budget.md)) and the DB does not grow a duplicate of the corpus ([0008](0008-gate-change-database-size.md)).
- The anchor's safe-text hash is recomputed from stored safe text. [0008](0008-gate-change-database-size.md) drops the stored `safe_hash`.
- Required proofs:
  - `./h`, `$PWD/h` and `sub/../h` are one source.
  - A deleted key file creates no duplicate source, and the next import anchor-aligns.
  - A final symlink is refused and the message names its target.
  - A same-length rewrite is detected.
  - A head trim keeps old rows and adds only new ones.
  - An ambiguous anchor supersedes and keeps the old revision.
  - A partial trailing record is not committed.
  - A committed seam test feeds a reader that errors after N bytes, and the parser yields its first record without reading the whole file.
  - The real-zsh writer test is skipped when zsh is absent. Hand-built Meta pairs and a Meta at EOF produce a warning and the lossy flag.
  - Content and filename shell detection, refusal of an ambiguous file, and a differing `--shell` that logs on stderr and replaces the source.
- Gates owned by S1a ([0014](0014-performance-contract.md)):
  - Fresh-process `/usr/bin/time` peak RSS below 64 MiB for `relic import` at 1M and 2M records, with no material growth between them.
  - 100 records appended to a 1M store, timed including open, under 25 ms.
- The <20 ms unchanged check (fresh process, including startup and open) has no earlier owner; S6's release run measures it ([0014](0014-performance-contract.md)).
- The key file follows the permission rules in [0003](0003-redaction-policy.md).

## Rejected alternatives

- Path as typed: every spelling duplicates the whole history (D1).
- `realpath`, following the final symlink: contradicts the source contract, and zsh's first save-by-copy turns the link into a regular file, so the identity changes and the corpus duplicates.
- `(dev, inode)` identity: zsh's rename-on-trim makes every trim look like a new source.
- Follow a final symlink for reading, with identity as the name given: amends the source contract for a setup zsh already breaks, and a link and its target still become two sources.
- Full parse and redaction of the whole file on every change (WIP): O(file) per append is very likely to miss the <25 ms gate, and it builds a whole-file `Vec`.
- Replace on rewrite (WIP): every routine trim deletes history Relic held, and citations break. Contradicts the source contract and [Choose the storage, search, and runtime architecture](https://github.com/yuann3/relic/issues/4).
- Always supersede: every trim re-inserts the whole file, flooding newest-first results and the size gate.
- Keyed BLAKE3 over raw input per record: keeps a raw-derived hash per row, protected only by a key stored beside the DB.
- Installation key in the OS keychain or Secret Service: adds platform dependencies, and headless Linux has no Secret Service.
- Unkeyed BLAKE3 over safe text as a fingerprint: duplicates the safe-text hash.
- Keyed hash of the resolved path as `logical_id`: a lost key yields a new identity and a full duplicate, and it protects nothing because the raw path is already in the private locator.
- Unkeyed hash of the raw prefix: a DB holder can brute-force a short redacted secret, because the surrounding safe text is stored.
- Keyed boundary guard window as the primary check (the source contract): misses a same-length edit before the window combined with an append. It survives only as the fallback.
- Keyed `EvidenceId`s that are deterministic across investigations: nothing needs them.
- Visitor parser over a borrowed, reused buffer: premature, since import already has a 3.7–4.1x throughput margin, and lifetimes complicate the redact-to-insert seam.
- `mmap`: touched pages count toward RSS, and the file can change under the map.
- Keep `fs::read` and drop only the `Vec`: RSS stays proportional to file size.
- Unmetafy after line splitting: a second pass that rests on a subtle ordering argument.
- Lossy decode only: fails the source contract and garbles CJK text.
- zsh default with no detection: bash histories are mis-parsed silently.
- Required `--shell`: friction on every first import.

## Open facts

- Keyed BLAKE3 time over a 1M-record prefix (28–55 MB, depending on the fixture), measured inside the <25 ms append gate. This decides between the prefix hash and the guard-window fallback (S1a).
- Anchor size N, guard window K (fallback only) and the per-record byte cap. The lock sets none of them (S1a).
- Peak RSS of a single fresh-process import has never been measured.
