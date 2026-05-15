# Rust Architecture Instructions

These preferences describe the shape expected from Rust implementation work.

## Core Shape

- Prefer a functional core with an imperative shell.
- Put side effects at the edges of the call stack.
- Keep pure/domain logic free of filesystem, SQLite, thread, process, and wall
  clock dependencies where practical.
- Model important domain invariants with types rather than comments.
- Start with internal modules before extracting crates. Extract a crate only
  after boundaries have proven stable or another consumer exists.

## Deep, Narrow Interfaces

- Prefer deep, narrow module interfaces.
- A caller should ask for meaningful behavior, not orchestrate internal steps.
- Hide threading, batching, caching, persistence, staging, and filesystem
  details behind module APIs.
- Do not leak implementation mechanics into callers or tests.
- Keep transaction choreography, path construction, and filesystem mutation
  owned by the module responsible for that concern.

## Module Boundaries

- Organize modules by responsibility, not by the legacy Python implementation.
- CLI modules parse arguments, validate command-level input, dispatch commands,
  and render output.
- Path modules own canonicalization, durable path identity, containment checks,
  and path serialization.
- Scanner modules discover source files. They do not hash, write blobs, or write
  SQLite rows.
- Store modules own staging, hashing/write loops, CAS placement, blob chmod, and
  CAS presence checks.
- Catalog modules own SQLite connections, migrations, PRAGMAs, transactions, and
  raw SQL.
- Ingest modules orchestrate scanner, store, catalog, and reporting through
  behavior-level interfaces.

## Types As Guardrails

- Use small newtypes where they prevent category mistakes:
  - validated blob hash
  - validated store root
  - canonical source root
  - validated slash-normalized source-relative path
  - staging file name
- CAS paths should be constructible only from trusted components such as a
  store root and a validated blob hash.
- Source-relative paths should not be accepted as raw strings outside the module
  that validates them.
- Avoid heavy typestate machinery in early slices. Use simple constructors and
  focused tests.

## Evolution

- Expect borrow-checker feedback to reshape implementation internals.
- Protect behavior with tests that avoid private helper coupling so refactors
  remain cheap.
- Add abstraction only when it removes real complexity, captures an invariant,
  or matches a proven local pattern.
- Avoid speculative generic code. Prefer concrete types and operations until the
  second use case is real.

