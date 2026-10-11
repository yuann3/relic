# Coding Standard

`cargo clippy --all-targets -- -D warnings` and `cargo nextest run` enforce mechanical rules. Don't review what the toolchain already catches.

## Surgical changes

- Change what the task needs. Nothing else.
- No drive-by refactors, renames, or dependency bumps.
- Bugs found along the way are follow-ups, not in-scope fixes.
- Every change should delete something. A change that only adds is suspect.

## Simplicity

- Simplest thing that works.
- No abstraction until there is a second caller.
- Prefer deleting to adding.

## No backward compatibility

- Break APIs freely. Update every call site.
- No compat shims, deprecation paths, or re-exports for old names.

## Tests

- Every test must fail when the implementation is wrong.
- No asserting against your own mock. No `assert!(true)`.
- A fix without a reproducing test is a guess.
- Test the seam, not the halves — run real data through both sides.
- Scratch scripts stay scratch. Don't promote throwaway checks to committed tests.

## Comments

- Comment why, never what.
- Worth it: rejected alternatives, silent traps, vendored edits, non-obvious values.
- Not worth it: restating the next line, section banners, commented-out code.

## Error visibility

- A caught-and-retried error must leave a visible trace. Silent retries are hidden crashes.

## Clippy

`cargo clippy --all-targets -- -D warnings` must pass with zero diagnostics. No `#[allow(clippy::*)]` without a comment explaining why.
