# Release build profile and the portable Linux binary

**Status:** measured, for [[L2] Research the release build profile and the portable Linux binary](https://github.com/yuann3/relic/issues/47)
**Decision scope:** `[profile.release]`, `panic`, and musl versus glibc for the Linux release binary. Distribution and targets are [L1](https://github.com/yuann3/relic/issues/46)'s; the full Linux x86_64 gate matrix is [L4](https://github.com/yuann3/relic/issues/49)'s.
**Research date:** 2026-11-13
**Evidence:** the gate harness `benches/gates.rs` ([0014](../adr/0014-performance-contract.md)) run on every variant; raw reports in [`release-build-profile/`](release-build-profile/).

## Decision

- **`lto = "fat"` and `codegen-units = 1`.** The stripped binary shrinks about 14% on every platform measured (macOS headless 5.01 → 4.30 MiB, full 5.45 → 4.66 MiB). Import and search stay within run-to-run noise on macOS and on Linux. A clean release build takes about 12 s longer. Thin LTO changes nothing.
- **`opt-level` stays 3.** `"s"` saves another 1 MiB but costs 15–50% of import and search. `"z"` doubles them and fails the 1M import gate. `2` saves only 0.2 MiB more and is no faster.
- **`panic` stays `unwind`.** `abort` saves 0.37 MiB. The panic hook still restores the terminal, but the process then dies of SIGABRT instead of exiting 101, and a panic on any worker thread takes the whole process down.
- **`strip` is left to the release pipeline.** Cargo's release default already drops debug info. Stripping symbols saves another 0.6 MiB but leaves a panic backtrace without function names; L5 decides what the shipped artifact keeps. The size gates measure the stripped binary either way.
- **For Linux, both libcs pass every gate.** The choice belongs to L1:
  - **musl, static:** one binary that runs on any Linux with kernel 3.15 (2014) or newer. Import is about a quarter slower (1M import 3.9 s, still 2.5× inside its 10 s gate) and peak RSS is about 3 MiB lower. It needs one code fix first, below.
  - **glibc:** the faster import, but only if it's built against an old glibc. Built on a current distro (glibc 2.43, checked on the arm64 build) it requires glibc 2.34, which rules out Debian 11, Ubuntu 20.04 and RHEL 8.
- **No `target-cpu`.** Nothing in the build sets one, so release binaries use each target's baseline instruction set.

## Method

- Each variant was built clean into its own target directory from `9c6be2f`, with Cargo's `CARGO_PROFILE_RELEASE_*` variables, headless (`--no-default-features`) and full.
- The gate harness then ran on each pair with 20 fresh-process samples per gate. Each run builds 100k, 1M and 2M histories, imports them, and searches the 1M store. The tables show p50; the reports hold min, max, p95 and every sample.
- `base` ran twice on each platform, first and last, to show the drift across a session.
- Machines:
  - macOS: Apple M4 Max, 16 cores, 128 GiB, macOS 27.0, APFS. Load average 5–9 throughout, from other work on the machine.
  - Linux arm64: an OrbStack VM on the same Mac (Linux 7.2.9, Ubuntu 26.04, glibc 2.43, btrfs on a virtual disk, 15 vCPUs). Its absolute numbers are not a Linux baseline; only the comparisons between variants are used.
  - Linux x86_64: an OrbStack VM under Rosetta, used for sizes only. Emulated timings mean nothing.
- Toolchain: rustc 1.99.0 on every machine. Bundled SQLite 3.49.1.

Run-to-run noise is large: the two `base` runs differ by up to 22% on 100k import on macOS and 16% on 1M import on Linux. A difference under about 10% below is not evidence.

## Results

### macOS arm64 (p50)

| Variant | import 100k ms | import 1M | append 100 ms | `git commit` ms | `ta*` ms | `git tas*` ms | 400×`git` rank ms | RSS 2M MiB | headless MiB | full MiB | gates pass |
|---|---|---|---|---|---|---|---|---|---|---|---|
| base (run 1) | 346 | 3.77 s | 4.2 | 2.9 | 24.6 | 25.9 | 32.1 | 28.6 | 5.01 | 5.45 | all |
| base (run 2) | 423 | 4.06 s | 4.1 | 2.4 | 23.5 | 24.2 | 32.1 | 28.6 | 5.01 | 5.45 | all |
| thin LTO | 353 | 3.78 s | 4.1 | 3.1 | 24.3 | 25.5 | 31.9 | 28.6 | 5.03 | 5.49 | all |
| fat LTO, 1 CGU | 353 | 3.80 s | 3.9 | 2.5 | 23.9 | 25.1 | 31.2 | 28.5 | 4.30 | 4.66 | all |
| fat, 1 CGU, opt 2 | 346 | 3.87 s | 4.6 | 3.3 | 26.8 | 28.1 | 33.0 | 28.4 | 4.07 | 4.41 | all |
| fat, 1 CGU, opt "s" | 417 | 4.36 s | 4.1 | 2.8 | 29.2 | 29.3 | 34.7 | 28.0 | 3.33 | 3.65 | all |
| fat, 1 CGU, opt "z" | 697 | 7.73 s | 6.2 | 4.0 | 48.6 | 50.8 | 58.9 | 27.8 | 2.81 | 3.05 | fails import 1M (p95 11.8 s) |

Warm `--help` is 1.8–2.4 ms in every variant, against 15 ms.

### Linux arm64, glibc and musl (p50)

| Variant | import 100k ms | import 1M | append 100 ms | `git commit` ms | `ta*` ms | `git tas*` ms | 400×`git` rank ms | RSS 2M MiB | headless MiB | full MiB | gates pass |
|---|---|---|---|---|---|---|---|---|---|---|---|
| glibc base (run 1) | 279 | 3.16 s | 1.0 | 0.5 | 20.7 | 20.6 | 27.1 | 27.3 | 5.27 | 5.77 | all |
| glibc base (run 2) | 302 | 3.66 s | 1.1 | 0.6 | 21.9 | 22.6 | 30.0 | 27.3 | 5.27 | 5.77 | all |
| glibc fat LTO, 1 CGU | 268 | 3.27 s | 1.0 | 0.5 | 22.2 | 23.3 | 29.8 | 27.0 | 4.64 | 5.02 | all |
| glibc fat, 1 CGU, opt "s" | 458 | 4.44 s | 1.2 | 0.6 | 29.4 | 28.0 | 41.0 | 26.3 | 3.39 | 3.71 | all |
| musl base | 399 | 3.99 s | 1.1 | 0.6 | 22.3 | 22.4 | 30.4 | 24.2 | 5.27 | 5.71 | all |
| musl fat LTO, 1 CGU | 342 | 3.93 s | 1.1 | 0.6 | 22.5 | 23.5 | 32.2 | 24.0 | 4.64 | 5.02 | all |

Warm `--help` is 0.1–0.3 ms. The append and whole-word figures are far below macOS's: the VM's disk and fsync are not a bare machine's, which is why L4 measures on one.

### Linux x86_64 sizes (stripped)

| Variant | headless MiB | full MiB |
|---|---|---|
| glibc base | 5.88 | 6.36 |
| glibc fat LTO, 1 CGU | 5.23 | 5.65 |
| musl fat LTO, 1 CGU | 5.37 | 5.78 |

x86_64 is about 12% larger than arm64, and well under the 8–12 MiB and 12–18 MiB targets. The musl build is the patched copy described under musl.

### Build time (clean, macOS, headless/full)

base 17/18 s, thin 18/19 s, fat LTO 29/31 s, `"s"` 20/21 s, `"z"` 18/21 s.

## `panic = "abort"`

The five PTY panic tests (`tests/pty.rs`, `panic_after_*`) were run against the release full binary of each profile:

- **unwind (fat LTO):** all five pass.
- **abort (fat LTO):** all five fail their first assertion, the exit code: the process dies of signal 6 instead of exiting 101. Run again with that assertion removed, all five pass: the panic hook runs before the abort, so raw mode, the alternate screen, the cursor and bracketed paste are all restored, and the panic message shows.

So abort would keep the terminal safe but change what a crash looks like: the shell prints "Abort trap", macOS writes a crash report, and a panic on a worker thread (search, recall, model calls) ends the process instead of that thread. It saves 0.37 MiB, 8% of a binary at a third of its size target. Not worth either change.

## musl

- **It doesn't link today.** `catalog::rename_with` calls `libc::renameat2` for the atomic swap that activation uses. The libc crate declares it for musl, but the musl Rust links statically has no such symbol: `undefined reference to 'renameat2'`. Calling the syscall directly (`libc::syscall(libc::SYS_renameat2, …)`) works. That's what glibc's wrapper does, and it needs Linux 3.15. With that one change, applied to a copy for this research, musl links on arm64 and x86_64, and the catalog tests pass under musl on arm64.
- **The binary is fully static.** `file` reports "statically linked"; it needs nothing from the system. TLS roots are compiled in (`webpki-roots`), so it doesn't depend on the system's CA certificates.
- **The cost is import throughput.** Against glibc with the same profile, 100k import is 28% slower with fat LTO and 32–43% without; 1M import is 20% slower with fat LTO and 9–26% without. This is consistent with musl's allocator, which import exercises heavily. Search is within noise. Every gate still passes with room: 1M import at 3.9 s against 10 s, 100k at 0.34–0.40 s against 1 s.
- **Peak RSS is lower:** 24 MiB against 27 MiB.
- A faster allocator such as mimalloc would likely win the import time back. It's a new dependency outside [0015](../adr/0015-dependency-envelope.md)'s allowed list, so it isn't considered here.

## glibc

A binary links against the glibc it's built on, and the dynamic loader refuses it on an older one. Built on Ubuntu 26.04 (glibc 2.43), the binary's strong symbol versions require **glibc 2.34** (`__libc_start_main@GLIBC_2.34`, among others). Symbols from newer versions (`pidfd_spawnp@GLIBC_2.39`, `statx`, `getrandom`, `gettid`) are weak; std checks for them at run time and falls back.

Relic's own floor is lower:
- **2.28**, from `renameat2`.
- **2.17**, which is Rust's own minimum for `x86_64-unknown-linux-gnu`, if `renameat2` goes through `syscall` as for musl.

Reaching that floor means building against an old glibc. Either build in an old distribution's container, or use a linker that targets a glibc version, as `cargo-zigbuild` does with `x86_64-unknown-linux-gnu.2.28`. Neither is set up here.

## Still open

- Every number on bare Linux x86_64: [L4](https://github.com/yuann3/relic/issues/49).
- musl or glibc, and which glibc floor: [L1](https://github.com/yuann3/relic/issues/46).
- The `renameat2` change, needed by musl and by a glibc floor under 2.28. It belongs to whichever ticket picks one of them.
- Whether shipped binaries keep their symbols: [L5](https://github.com/yuann3/relic/issues/50).
