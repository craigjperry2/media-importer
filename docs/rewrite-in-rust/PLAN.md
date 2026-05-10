# Rust rewrite plan

## Purpose

Rewrite `media-importer` in Rust from the behavior described in
`docs/ONESHOT.md`.

The existing Python implementation under `src/` and `tests/` is not a trusted
oracle for the rewrite. Treat it as historical context only when needed for
project naming or repository layout. Implementation requirements come from
`docs/ONESHOT.md` and from decisions recorded in this plan.

Do not implement the rewrite layer by layer. Build thin vertical slices: each
increment should expose a small coherent behavior through the CLI, persist or
read real catalog state where appropriate, and include tests for that slice.

## Decisions to make before implementation

These choices should be resolved before the first Rust code lands because they
affect crate structure, test strategy, and CLI guarantees.

### Runtime model

Recommendation: start synchronous, single-process, single-runner.

The tool is filesystem and SQLite bound, and the spec requires a catalog-backed
run lock so accidental concurrent live runs against one catalog are rejected or
explicitly waited on. Synchronous code keeps path handling, SQLite transaction
scope, lock heartbeat, progress reporting, and copy failure semantics easier to
reason about.

Defer async unless there is a measured need for overlapping slow remote filesystems
or a future dashboard server. If concurrency is added later, prefer bounded worker
threads for hashing/copying with a single SQLite writer, not async file I/O as a
default architecture.

Decision to record:

- Use synchronous Rust for the initial rewrite.
- Introduce bounded threads only behind an explicit strategy if profiling shows
  a real bottleneck.

### Platform support

Recommendation: support Unix-like systems first: Linux and macOS.

The required browse tree uses symlinks. Windows symlink behavior requires
different privileges and path handling, so claiming full Windows support early
would add complexity to the core migration. Use Rust path APIs consistently so
future Windows support is not intentionally blocked, but tests and release
expectations should target Linux and macOS first.

Decision to record:

- Initial supported platforms are Linux and macOS.
- Windows is out of scope until symlink semantics and test coverage are designed.

### CLI compatibility

Recommendation: keep the executable name `media-importer` and implement the
commands in `docs/ONESHOT.md`: `scan`, `verify-store`, and `query`.

The Rust CLI should prefer explicit, stable output over matching Python-era text.
The required progress and dry-run messages in `ONESHOT.md` are the compatibility
target.

Decision to record:

- The CLI contract is `ONESHOT.md`, not the current Python command behavior.

### SQLite access

Recommendation: use direct SQLite bindings without an ORM.

Use a crate that allows explicit SQL, transaction control, PRAGMA configuration,
and read-only connection modes. Keep schema SQL deterministic and visible in the
codebase or migrations.

Decision to record:

- Use raw SQL through a direct SQLite crate.
- Do not introduce an ORM or query-builder layer that hides schema behavior.

### Path representation

Recommendation: use `Path`/`PathBuf` at boundaries and convert to catalog
strings only at the SQLite boundary.

The spec depends on resolved source roots, canonical file path identity,
source-relative browse paths, and relative symlink targets. Avoid treating paths
as generic strings in planning logic.

Decision to record:

- Domain types carry paths as path objects.
- Catalog serialization is centralized and tested.

### Hashing and copy strategy

Recommendation: default to a streaming/auto strategy for live scans.

For new or changed files, hash, plan, copy, and commit in bounded units so source
bytes are likely still hot in the OS page cache. Dry runs may compute the full
action list up front.

Decision to record:

- Default hash/copy chunk size target: 1-4 MiB.
- Default live batch threshold target: about 256 MiB.
- Make chunk size configurable via `--hash-chunk-bytes`.

### Temporary copy names

Recommendation: do not use a fixed `<dest>.tmp` name.

`ONESHOT.md` calls out that the example temp path is unsafe for retry scenarios.
Use unique temporary names beside the destination, flush/fsync, then atomically
replace the final path.

Decision to record:

- Temp files are created beside the destination with unique names.

### Progress interface

Recommendation: design structured progress events from the start.

The CLI can render human-readable progress to stdout, but scanner/planner/executor
logic should emit structured events through a reporter interface so a future
read-only dashboard can subscribe without rewriting import logic.

Decision to record:

- No ad hoc stdout writes from scanner/planner/executor internals.
- Errors and diagnostics that indicate failure go to stderr.

## Preparatory tasks

Complete these before writing the first functional Rust slice.

1. Update `flake.nix`.
   - Add a Rust toolchain while keeping Python tooling in place.
   - Include tools needed for formatting, linting, testing, and SQLite-backed
     integration tests.
   - Do not delete Python tooling yet.

2. Update pre-commit checks.
   - Extend the existing `prek` configuration to run Rust formatting and linting.
   - Keep existing Python checks until the migration intentionally removes them.
   - Decide whether Rust checks run through `cargo fmt`, `cargo clippy`, and
     `cargo test`, or a narrower fast pre-commit subset plus CI-level tests.

3. Update `AGENTS.md`.
   - State that the Rust rewrite is in progress.
   - Tell coding agents to ignore `src/` and `tests/` as behavioral oracles.
   - Point agents to `docs/ONESHOT.md` and this plan.
   - Preserve the pure planning/effectful execution boundary in Rust terms.

4. Add Rust project scaffolding.
   - Create the package layout and executable target for `media-importer`.
   - Add a minimal smoke test for invoking the binary.
   - Do not port Python implementation code.

5. Establish test utilities.
   - Provide temporary directory/database helpers for integration tests.
   - Provide helpers to inspect SQLite rows and filesystem effects.
   - Add cross-platform gates for symlink tests if Windows support is deferred.

## Proposed Rust architecture

Keep modules aligned to responsibilities, not to the Python files:

- `cli`: argument parsing, command dispatch, stdout/stderr rendering.
- `catalog`: SQLite connection setup, schema, raw SQL queries, transactions,
  read-only and writable open modes, run locks.
- `domain`: blobs, observations, source roots, actions, progress events, errors.
- `paths`: canonicalization, source-relative path computation, browse path
  safety, relative symlink targets, catalog path serialization.
- `hashing`: chunked BLAKE2b hashing and byte progress reporting.
- `scanner`: symlink-ignoring recursive source and store walks.
- `planner`: pure state comparison that returns explicit actions.
- `executor`: copies, symlinks, directory creation/pruning, transactions,
  lock refresh, and failure handling.
- `progress`: reporter traits and CLI renderer.

The exact crate layout can differ, but the boundaries must remain clear:

- Planning must not write to SQLite or the filesystem.
- Execution owns all database mutations and filesystem side effects.
- Catalog code uses direct SQL.
- CLI rendering is separate from core import logic.

Capture the above in a "code review agent" markdown file for consistency in
future PRs.

## Vertical slice roadmap

Each slice should leave the repository in a runnable and tested state. Prefer
integration tests that exercise the CLI plus focused unit tests for pure path,
planning, and hashing behavior.

### Slice 1: CLI skeleton and catalog initialization

Goal: `media-importer` exists and can open or initialize the catalog correctly.

Scope:

- Implement `scan`, `verify-store`, and `query` command shells with required
  argument validation.
- Implement SQLite schema creation for writable opens.
- Enable foreign keys and WAL for writable catalogs.
- Implement read-only behavior for missing databases: dry-run/read-only paths
  use an in-memory initialized schema or return no query rows as specified.
- Add tests for command validation and catalog schema creation.

Exit criteria:

- The binary runs.
- `query --db missing.sqlite` prints no rows and does not create the file.
- Writable catalog open creates parent directories for the database path.

### Slice 2: Hashing and source scanning

Goal: discover regular source files and hash them deterministically.

Scope:

- Recursively walk source directories without following directory symlinks.
- Skip symlink files.
- Skip files that fail `stat()` or hashing and count them as skipped.
- Capture basename, lowercase suffix, byte size, mtime, and canonical file path.
- Implement configurable chunked BLAKE2b hashing.
- Emit structured progress events for discovered files, processed files, skipped
  files, and bytes read.

Exit criteria:

- Hash digests are independent of chunk size.
- Relative CLI source paths resolve to stable canonical file identities.
- Symlinked files and directories are ignored.

### Slice 3: Minimal live scan without browse cleanup

Goal: import files into the content-addressed store and record observations.

Scope:

- Implement blob store path computation:
  `<store>/<first-two-hash-chars>/<full-hash><lowercase-extension>`.
- Implement hash cache reuse by exact canonical `file_path`, size, and mtime.
- Plan add-blob, copy-file, source-root, and observation upsert actions.
- Execute metadata-preserving copy via unique temp file beside destination,
  flush/fsync, atomic replace, and transaction-backed catalog writes.
- Deduplicate blobs within a plan and across live batch boundaries.
- Implement `--dry-run` for canonical actions without creating store or database.

Exit criteria:

- First scan stores one canonical blob per unique content hash.
- Duplicate content gets multiple observations but one blob/copy.
- Second unchanged scan reuses cached hashes.
- Copy failures do not create blob or observation rows for failed hashes, while
  successful hashes are preserved.

### Slice 4: Run locking and live execution batches

Goal: live mutating commands are protected and bounded.

Scope:

- Implement atomic run-lock acquisition in SQLite.
- Record owner token, acquired timestamp, and heartbeat timestamp.
- Refresh heartbeat during long scan and verify operations.
- Release lock on normal completion.
- Fail quickly with a clear error when another live owner holds the lock.
- Process live scan work in bounded batches with streaming/auto behavior.

Exit criteria:

- Concurrent live runs against the same catalog are rejected.
- Dry-run and read-only commands do not acquire the write lock.
- Live scans report batch execution progress.

### Slice 5: Browse tree creation and idempotent rescan

Goal: every scan maintains hash-suffixed browse symlinks.

Scope:

- Require `--browse-root` for `scan`.
- Resolve store root, browse root, source roots, and file paths before computing
  source-relative paths and symlink targets.
- Persist `source_roots` and browse metadata on observations.
- Plan and execute source-relative symlinks suffixed with the first seven hash
  characters before the extension.
- Use relative symlink targets from symlink parent to canonical store file.
- Leave correct existing symlinks unchanged; replace incorrect symlinks.
- Fail if a non-symlink exists at the desired browse path.
- Validate browse paths before creating directories or symlinks.

Exit criteria:

- Browse symlinks are created with unconditional hash suffixes.
- Re-running the same scan is idempotent.
- Relative CLI paths produce correct browse layout and symlink targets.
- Non-symlink browse entries are never silently replaced.

### Slice 6: Stale source cleanup and overlapping roots

Goal: rescans reconcile source-root state safely.

Scope:

- Reject overlapping roots among current sources.
- Reject parent/child overlap with previously recorded source roots.
- Allow exact same root rescans.
- Detect stale observations only under roots included in the current scan.
- Remove stale browse symlinks and prune empty browse directories up to, but not
  including, the browse root.
- Delete stale `source_files` rows.
- Preserve `source_roots` as a registry even when no observations remain.

Exit criteria:

- Deleted source files remove browse links and catalog observations.
- Prior empty roots still participate in overlap rejection.
- Empty browse directories are pruned safely.

### Slice 7: Dry-run browse planning

Goal: dry-run output reflects live browse behavior without side effects.

Scope:

- Plan canonical and browse actions in dry-run mode.
- Reconcile against an in-memory merged view of existing catalog observations,
  current scan observations, and stale observations removed for scanned roots.
- Print action count and each action to stdout.
- Avoid creating store, browse tree, or database files in dry-run mode.

Exit criteria:

- Dry-run plans expected browse symlinks.
- Existing catalog state affects dry-run browse output.
- Filesystem and missing database paths remain untouched.

### Slice 8: `verify-store`

Goal: reconcile catalog state with the canonical store.

Scope:

- Acquire the run lock in live mode.
- Check every cataloged blob exists under `<store>/<store_path>`.
- For missing blob files, plan browse symlink removals for referencing
  observations, then delete the blob row and rely on cascade deletion.
- Walk the store without following symlinks.
- Hash unindexed regular store files and add blob rows without moving files.
- Skip unreadable/unhashable unindexed store files.
- Use persisted browse metadata; do not accept `--browse-root`.
- Support dry-run without writes or lock acquisition.

Exit criteria:

- Missing canonical files are removed from the catalog.
- Broken browse symlinks for missing blobs are removed in live mode.
- Unindexed store files can be indexed.

### Slice 9: `query`

Goal: expose read-only JSON Lines catalog queries.

Scope:

- Open the catalog read-only.
- Support `--ext`, `--name`, and `--hash` filters combined with `AND`.
- Normalize extensions by lowercasing and adding a leading dot when missing.
- Print one JSON object per matching row to stdout.
- Return no rows and create no file when the database does not exist.

Exit criteria:

- Query filters behave exactly as specified.
- Output is valid JSON Lines.

### Slice 10: Progress and failure polish

Goal: make long-running behavior operationally usable.

Scope:

- Render required scan start, source entry, live execution, dry-run completion,
  copy progress, and error summaries.
- Include discovered files, processed files, remaining files when known, hashing
  bytes, copying bytes, speeds, new files to copy, observations, action counts,
  and skipped files when nonzero.
- Ensure failure diagnostics go to stderr and cause non-zero exit.
- Review lock cleanup and partial failure behavior.

Exit criteria:

- Progress is emitted through the reporter interface, not ad hoc core writes.
- Required stdout/stderr separation is covered by tests where practical.

## Cross-slice test checklist

Use `docs/ONESHOT.md` as the authoritative checklist. At minimum, preserve
coverage for:

- chunked BLAKE2b hashing stability
- symlink-ignoring source and store walks
- live streaming behavior that keeps hash and copy close for new files
- dry-run purity
- deduplication within a run and across batch boundaries
- cached unchanged observations
- partial copy failure semantics
- browse symlink creation, idempotency, stale cleanup, safety, and dry-run
  planning
- overlapping source-root rejection
- run-lock behavior
- `verify-store` missing blob and unindexed file reconciliation
- JSON Lines query filters
- progress routing and non-zero failure exits

## Open spec issues to resolve deliberately

`docs/ONESHOT.md` lists known issues. Do not accidentally solve them in a way
that changes user-visible behavior without recording the decision.

- The 7-character browse hash suffix can collide.
- Store paths include lowercase extensions, but blobs are keyed only by hash, so
  identical bytes first seen through `.jpg` versus `.jpeg` need a deterministic
  extension selection rule.
- Store blob corruption detection is intentionally out of scope.
- Fixed temp copy names are unsafe; use unique names as recorded above.

Before implementing store-path extension behavior, choose and document the rule.
A conservative option is "first successfully cataloged extension wins" because
blob rows are keyed by `file_hash` and `store_path` is unique.
