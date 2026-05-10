# Rust Migration Plan

## Summary

- Build a side-by-side Rust implementation first, keeping Python as the behavioral oracle during migration.
- Default to synchronous Rust with blocking filesystem and SQLite APIs; add bounded threading only after benchmarks show a real bottleneck.
- Do not support existing Python-created SQLite catalogs. The Rust version may create a cleaner catalog schema and require a fresh catalog/rescan.
- Remove the Python implementation once Rust reaches agreed behavioral parity.

## Key Changes

- Extend `flake.nix` to support both stacks during migration:
  - Keep Python 3.13, `uv`, `pytest`, `ruff`, `mypy`, and `prek`.
  - Add Rust toolchain packages: `rustc`, `cargo`, `rustfmt`, `clippy`, and optionally `cargo-nextest`.
  - Add dev commands/docs for `uv run pytest`, `cargo test`, `cargo clippy`, and running the Rust CLI.
- Add a Rust Cargo package alongside `src/media_importer`, mirroring the current architecture:
  - `cli`: command parsing, progress reporting, orchestration.
  - `models`: owned domain structs and action enums.
  - `scanner`: filesystem traversal, symlink ignoring.
  - `hashing`: BLAKE2b hashing.
  - `catalog`: raw SQLite access, no ORM.
  - `planner`: pure state-to-action logic.
  - `executor`: all filesystem and DB side effects.
- Use owned Rust data structures (`PathBuf`, `String`, `Vec<Action>`) at module boundaries to avoid unnecessary lifetime coupling.
- Do not let planner structs own database connections. Catalog reads should produce owned snapshots that are passed into pure planner functions.
- Preserve core user behavior: content-addressed deduped store, dry-run mode, browse symlink overlay, source-root validation, verify-store behavior, query capability, and failure handling.

## Equivalence Strategy

- The current Python tests cannot all run unchanged against Rust because several import Python private helpers and inspect Python dataclasses.
- Keep Python tests as the baseline while Python exists.
- Add a black-box pytest equivalence suite that can run the same scenarios against either implementation by invoking a CLI command path.
- Compare observable outcomes, not internals:
  - exit codes
  - stdout/stderr where behavior matters
  - final store files
  - browse symlinks and targets
  - query results
  - catalog-level invariants, not Python schema identity
- Keep the Python implementation until the Rust CLI passes the Rust unit tests plus the black-box equivalence suite.

## Runtime And Rust Design

- Start synchronous:
  - Filesystem scanning, hashing, copying, symlink updates, and SQLite transactions use blocking APIs.
  - This matches the current workload: local disk IO and SQLite writes, where async would mostly wrap blocking work.
- Avoid async initially:
  - Async adds runtime choice, `Send`/`Sync` constraints, blocking-pool concerns, and more complex tests without clear benefit here.
- Design for future parallelism:
  - Keep scanner, hasher, planner, and executor boundaries explicit.
  - If benchmarks justify it, add a bounded threaded hash/copy pipeline later, with SQLite writes still serialized through executor transactions.

## Test Plan

- Baseline before migration: `uv run pytest`, `uv run mypy src tests`, `uv run ruff check .`.
- Rust checks during migration: `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`.
- Equivalence scenarios:
  - first scan imports files and dedupes identical content
  - repeated scan is idempotent
  - dry-run produces no filesystem or DB side effects
  - browse root creates stable source-relative symlinks with hash suffixes
  - stale source files prune browse links and empty directories
  - verify-store removes missing blob records and broken browse links
  - copy failure does not record failed blobs
  - overlapping source roots are rejected
  - query filters work for extension, name, and hash

## Assumptions

- Rust will eventually install the primary `media-importer` command, but during migration it may use a temporary command name such as `media-importer-rs`.
- The Rust catalog schema can differ from Python's because legacy SQLite catalog support is explicitly out of scope.
- The canonical content-addressed store layout should remain unless a later benchmark or design review proves a change is worth the user-facing churn.
- Python is deleted only after Rust passes the agreed parity suite and README/Nix/pre-commit workflows are updated.
