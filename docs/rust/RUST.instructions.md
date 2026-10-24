# Rust Instructions

These preferences apply to Rust work for this project and can be reused for
future Rust projects where they fit.

## Implementation Style

- Build in thin vertical slices of end-to-end behavior.
- Prefer a small coherent feature that reaches the CLI, real filesystem, and
  real persistence over broad horizontal layers such as "write all database
  code" or "write all tests" first.
- Keep each slice runnable, tested, and reviewable.
- Use Rust 2024 edition for this fresh application.
- Use the stable Rust toolchain provided by Nixpkgs. Do not use nightly-only
  features unless a future decision explicitly requires them.
- Do not define a formal MSRV yet. This is an application, not a public library.

## Error Handling

- Use `color-eyre` for CLI-grade error reports.
- Call `color_eyre::install()` once in `main`.
- It is acceptable for application operations in this CLI crate to return
  `color_eyre::Result<T>`.
- Prefer `.wrap_err(...)` and `.wrap_err_with(...)` at IO, SQLite, parsing, and
  path-boundary failures.
- Include useful context in errors: paths, hashes, schema versions, command
  names, and operation names.
- Do not discard source errors by converting everything to strings.
- Avoid a large custom error enum until a real library/API boundary benefits
  from typed errors.
- Small typed domain errors are fine inside pure core logic when they clarify
  invariants.

## Paths And Identity

- Treat path canonicalization as a safety feature, not cleanup.
- Use `Path` and `PathBuf` internally. Convert to durable strings only at the
  persistence boundary.
- Do not use lossy path conversion for persisted identities.
- Centralize path serialization, containment checks, and normalization.
- Deliberately canonicalize stable identities at system boundaries so repeated
  runs are idempotent across path spelling differences.
- Do not silently create user-selected parent directories outside a tool-owned
  root. If an explicit path has a missing parent, fail with context.
- Prefer Rust types that make unsafe path joins unrepresentable. For example,
  distinguish a validated store root from a source-relative path and a blob
  hash.

## Logging And Observability

- Use `tracing` from the start.
- Use trace-level logging to expose internal decisions when that helps tests or
  debugging without exposing private module internals.
- Logging is not a substitute for user-facing errors. Failures still go through
  contextual `color-eyre` errors.
- Prefer logs for operational insight and reports for stable command outcomes.

## Time

- Persist machine timestamps as integer Unix epoch milliseconds.
- Centralize access to the current time behind a small abstraction when code
  writes timestamps and tests need deterministic assertions.
- Treat filesystem modified times as advisory metadata. Some filesystems do not
  update mtimes reliably as a performance optimization.

## Tooling

- Required Rust checks:
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
- Configure these checks in pre-commit/`prek` hooks from the first Rust scaffold
  slice.
- Keep tests in pre-commit while the suite is small; this encourages a fast
  feedback loop.
- Use narrow local `#[allow(...)]` attributes only when a clippy lint conflicts
  with a deliberate design choice. Include a short explanation.
- Do not use broad crate-level lint suppression.
- Defer extra tools such as `cargo nextest`, coverage tooling, and dependency
  policy tools until there is a concrete need.

