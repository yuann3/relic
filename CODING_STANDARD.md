# Coding Standard

## Tautological tests considered harmful

Tests must verify meaningful behavior through public interfaces. A test that cannot fail when the implementation is wrong — or that passes by restating the implementation — is worse than no test. Every test must be able to catch a real bug.

Prohibited patterns:
- Asserting that a constructor returns a value you just constructed
- Asserting that a setter sets a field
- Mocking the thing under test then asserting the mock's behavior
- `assert!(true)` or `assert_eq!(x, x)`

## Clippy must always be green

All code must pass `cargo clippy -- -D warnings` with zero diagnostics. CI and local development both enforce this. No `#[allow(clippy::*)]` without a comment explaining why the lint is wrong for that specific case.
