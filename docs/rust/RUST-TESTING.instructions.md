# Rust Testing Instructions

These preferences apply to Rust tests for this project.

## Testing Bias

- Prefer high-level integration tests that exercise real behavior through the
  CLI or deep module interfaces.
- Use real temporary directories and real SQLite databases by default.
- Avoid mocks for core filesystem and SQLite behavior unless a test would
  otherwise be brittle, slow, or impossible.
- Keep fixtures tiny and generate them inside tests.
- Use pure unit tests for stable deterministic functions and invariants.
- Do not test private helper functions just because they exist.

## Behavior Over Choreography

- Assert observable outcomes:
  - files exist at expected CAS paths
  - SQLite rows contain expected durable state
  - reruns are idempotent
  - dry-run leaves durable state unchanged
  - command summaries communicate the behavior
- Do not assert incidental implementation details such as buffer count, exact
  transaction timing, private helper calls, or worker topology.
- Direct SQLite inspection is allowed in integration tests. The catalog schema
  is part of the product contract for this local media store.
- Inspect persisted facts, not incidental batching or connection behavior.

## Trace And Probe Strategy

- Deep, narrow modules can make internals hard to observe.
- Prefer trace-level logging or explicit telemetry/probe streams when a test
  needs insight into internal decisions without exposing private APIs.
- Use this sparingly. Filesystem and database outcomes should usually be the
  primary assertions.
- This is especially important in Rust because implementation structure may need
  substantial refactoring after borrow-checker feedback.

## Keeping Integration Tests Fast

- Keep fixture data small.
- Share compiled binaries through normal `assert_cmd` usage.
- Put expensive setup behind test helper builders.
- Add focused integration tests at deep module boundaries when CLI setup hides
  the behavior under test.
- Put genuinely slow large-media or crash-consistency tests behind ignored tests
  or a separate command.
- If high-level tests become slow, improve the harness before abandoning the
  integration-first bias.

## Dry Run Testing

- Specify dry-run as no durable mutation, not no work.
- Dry-run may scan, read metadata, hash files, read an existing catalog, and
  report planned outcomes.
- Dry-run must not create or mutate the store, staging directory, blob files,
  database, WAL, or SHM files.
- If future dry-run behavior needs mutation-like bookkeeping, prefer a temporary
  or in-memory clone of the SQLite database over branching deeply or adding
  test-only code.

## Time And Metadata

- Use fixed clocks where tests assert `first_seen_at_ms`, `last_seen_at_ms`, or
  catalog-created timestamps.
- Do not rely heavily on exact filesystem mtime values.
- Treat source mtime as advisory metadata, not a correctness signal.
- Future metadata-skip tests must cover both the default skip behavior and the
  opt-out path.

