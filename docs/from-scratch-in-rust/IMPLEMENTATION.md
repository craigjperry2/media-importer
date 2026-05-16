# Rust Implementation Guide

This guide records implementation decisions for the Rust rewrite of
`media-importer`. The source of product intent is `SPEC.md`; this file captures
the concrete choices a coding agent should follow while implementing it.

The existing Python implementation is deprecated historical context. Do not port
it file-by-file and do not use it as a behavior oracle.

## Companion Instructions

Use these colocated instruction files when implementing this guide:

- `RUST.instructions.md`
- `RUST-ARCHITECTURE.instructions.md`
- `RUST-TESTING.instructions.md`
- `SQLITE.instructions.md`

## Project Layout

- Make the repository root a Cargo workspace.
- Create the initial package at `crates/media-importer`.
- Package name: `media-importer`.
- Binary name: `media-importer`.
- Library crate name: `media_importer`.
- Use Rust 2024 edition.
- Start with a single package containing `src/main.rs` and `src/lib.rs`.
- Use internal modules before extracting separate crates.

Suggested initial modules:

- `cli`
- `config`
- `paths`
- `scanner`
- `hashing`
- `store`
- `catalog`
- `ingest`
- `telemetry` or `reporting`

## Dependency Baseline

Use major-compatible dependency declarations and let `Cargo.lock` pin resolved
versions.

Runtime dependencies:

- `blake3 = "1"`
- `clap = { version = "4", features = ["derive"] }`
- `color-eyre = "0.6"`
- `rusqlite` with `bundled` feature
- `uuid = { version = "1", features = ["v4"] }`
- `walkdir = "2"`
- `tracing = "0.1"`
- `tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }`

Test dependencies:

- `assert_cmd = "2"`
- `assert_fs = "1"`
- `predicates = "3"`

Defer until needed:

- `indicatif`
- `crossbeam-channel`
- `cargo nextest`
- coverage tooling
- dependency policy tooling such as `cargo deny`

## Slice 0: Agent And Rust Workspace Preparation

Before implementing import behavior, prepare the repo for clean-slate Rust work.
The active Python implementation should be removed from this branch rather than
kept as deprecated code. The Python implementation is not an oracle for Rust
behavior. Keep `docs/old_python/` only as historical archived context.

Scope:

- Add the Cargo workspace and `crates/media-importer` scaffold:
  - root `Cargo.toml` workspace
  - package `crates/media-importer`
  - binary `media-importer`
  - library crate `media_importer`
  - Rust 2024 edition
- Add a minimal binary smoke test proving the scaffolded binary runs.
- Replace Python pre-commit hooks before removing Python tooling:
  - remove `ruff`, `mypy`, and `pytest` hooks
  - add `cargo fmt --all --check`
  - add `cargo clippy --workspace --all-targets -- -D warnings`
  - add `cargo test --workspace`
- Update `flake.nix` using Nixpkgs default Rust tooling:
  - keep `bash`
  - keep `prek`
  - keep `sqlite`
  - `cargo`
  - `rustc`
  - `rustfmt`
  - `clippy`
  - `rust-analyzer`
  - remove `python313`, `uv`, and `ruff`
  - remove virtualenv setup from `shellHook`
  - keep `prek install` after hooks are converted to Rust commands
- Remove active Python implementation artifacts:
  - `src/media_importer/`
  - `tests/`
  - `pyproject.toml`
  - `uv.lock`
- Update `AGENTS.md` so agents treat the Rust rewrite docs as authoritative,
  use Cargo commands, and follow the Rust module boundary guidance instead of
  the old Python planner/executor split.
- Update the top-level `README.md` so it describes:
  - Rust rewrite status
  - Nix setup
  - Cargo checks
  - the planned milestone 1 `import` command
  - `docs/old_python/` as historical archive material only
- Update `.gitignore` from Python artifacts to Rust/local artifacts such as
  `target/`, `.direnv/`, and editor/cache leftovers as needed.
- Update `.vscode/settings.json` to remove Python test settings and use
  Rust/Cargo-friendly settings where useful.
- Do not add CI yet.

Ordering:

1. Add the Rust workspace scaffold and Rust dependencies.
2. Replace pre-commit hooks with Rust checks.
3. Update `flake.nix` to remove Python tooling and shell virtualenv setup.
4. Delete Python package, tests, and Python lock/config files.
5. Update `AGENTS.md`, `README.md`, `.gitignore`, and editor settings.
6. Run the Rust checks from the final environment.

Validation:

- `nix develop` enters without trying to run `uv`, activate `.venv`, or call
  Python tools.
- `prek run --all-files` executes only Rust-oriented hooks.
- `cargo fmt --all --check` passes.
- `cargo clippy --workspace --all-targets -- -D warnings` passes.
- `cargo test --workspace` passes, including the binary smoke test.

Keep this slice reviewable and conventional-commit friendly. Do not assume
permission to commit.

## Milestone 1: Directory Import Into The CAS

Implement one end-to-end vertical slice:

```text
media-importer import \
  --store <STORE_ROOT> \
  --source <SOURCE_DIRECTORY> \
  [--db <DB_PATH>] \
  [--dry-run] \
  [--chunk-size <BYTES>]
```

Only expose the `import` command in milestone 1. Do not expose placeholder
commands for `build-tree`, `gc`, or `audit`.

### CLI Contract

- `--source` is required and must be an existing directory.
- Single-file import is out of scope.
- `--store` is required.
- `--db` is optional and defaults to `<STORE_ROOT>/catalog.sqlite`.
- `--dry-run` performs no durable mutation.
- `--chunk-size` is optional and must be non-zero.
- Do not expose `--workers` in milestone 1.
- Do not expose `--json` in milestone 1.

Use `clap` derive for parsing. Keep CLI argument structs separate from validated
application config.

Install `color-eyre` in `main`, initialize `tracing-subscriber`, convert CLI
arguments into validated config, call `ingest::import_source`, then render a
summary.

### Output

On success, print a concise human-readable summary to stdout.

Example real import:

```text
Import complete
Files seen: 3
Blobs created: 2
Blobs reused: 1
Source records inserted: 3
Source records updated: 0
Bytes seen: 12345
Bytes written: 8192
```

Example dry run:

```text
Dry run complete
Files seen: 3
Blobs that would be created: 2
Blobs that would be reused: 1
Source records that would be inserted: 3
Source records that would be updated: 0
Bytes seen: 12345
Bytes that would be written: 8192
```

Use `tracing` for internal diagnostics. Defer progress bars and structured
non-TTY logs.

## Path Validation And Store Layout

Store layout:

```text
<STORE_ROOT>/
  blobs/<hex[0..2]>/<hex[2..4]>/<full_blake3_hex>
  staging/
  catalog.sqlite
```

Blob filenames are the lowercase BLAKE3 hash only. Do not include extensions.

Required path/domain newtypes:

- `BlobHash`: exactly 64 lowercase ASCII hex characters.
- `StoreRoot`: validated intended store root.
- `SourceRoot`: canonical absolute source directory.
- `SourceRelativePath`: validated slash-normalized relative path.
- `StagingFileName`: UUID-based staging filename.

The `paths` module owns:

- canonicalization
- source/store/db overlap checks
- slash-normalized relative path serialization
- CAS path construction
- staging path construction
- SQLite path serialization helpers

Rules:

- Canonicalize `source_root`.
- If `--store` exists, canonicalize it and require a directory.
- If `--store` does not exist, require its parent exists, canonicalize the
  parent, and append the intended final component.
- Real import may create that exact store root and known children.
- Missing store parents are errors.
- Reject source/store overlap in either direction, including not-yet-existing
  store paths.
- If explicit `--db` is provided, its parent must already exist and be
  canonicalized.
- Explicit `--db` must not be inside the source directory.
- If `--db` is omitted, derive it from the intended `StoreRoot` without
  separately canonicalizing the not-yet-existing store directory.
- Explicit `--db` inside a not-yet-existing store path is not allowed.
- Store `source_root` as canonical absolute text.
- Store `relative_path` as slash-normalized text.
- Reject non-UTF-8 durable path identities in milestone 1.
- Reject relative paths that are absolute, empty, contain `..`, or have platform
  prefix/root components.
- Do not add scanner name-based skips for `.git`, `catalog.sqlite`, `blobs`, or
  hidden files.

## Scanner

Milestone 1 scanner behavior:

- Use `walkdir`.
- Input is a canonical `SourceRoot`.
- Include hidden files and directories.
- Ignore symlinked files and symlinked directories.
- Process regular files only.
- Fail on traversal or metadata errors with contextual `color-eyre` errors.
- Return a collected, sorted `Vec<SourceFileCandidate>`.
- Sort by `SourceRelativePath` for deterministic behavior.
- Do not hash.
- Do not write files.
- Do not write SQLite rows.
- Empty source directory import succeeds with zero files.

Suggested candidate shape:

```rust
pub struct SourceFileCandidate {
    pub absolute_path: PathBuf,
    pub relative_path: SourceRelativePath,
    pub size_bytes: u64,
    pub modified_at_ms: Option<i64>,
}
```

`modified_at_ms` is advisory metadata only. If mtime is unavailable or before
Unix epoch, store `None` and trace/debug it. Do not depend on mtime for
correctness in milestone 1.

## Store And Hashing

Expose one deep real-import operation:

```rust
impl Store {
    pub fn ingest_file(
        &self,
        source_path: &Path,
        expected_size: u64,
        chunk_size: NonZeroUsize,
    ) -> color_eyre::Result<StoredBlob>;
}
```

Suggested return types:

```rust
pub struct StoredBlob {
    pub hash: BlobHash,
    pub size_bytes: u64,
    pub outcome: StoreOutcome,
}

pub enum StoreOutcome {
    Created,
    Reused,
}
```

Behavior:

- Read the source file in chunks.
- Hash with BLAKE3 while writing to a UUID-named staging file.
- Count bytes read and fail if they differ from scanner `expected_size`.
- After hashing, re-stat the source. Fail if size changed or if both initial and
  final mtimes are present and differ.
- Compute final CAS path from `StoreRoot + BlobHash`.
- Ensure final parent directories exist.
- Never stage directly to the final path.
- Use hard-link-then-unlink to install the staged file without overwriting an
  existing blob.
- If final blob already exists, verify size, ensure read-only permissions, and
  remove the staged file.
- If final blob exists with different size, fail as corruption.
- If install succeeds, remove the staging path and apply read-only permissions.
- Blob mode on Unix is `0444`; non-Unix may use `set_readonly(true)`.
- If chmod fails, fail the import.
- Staging names use UUIDs. They do not need process IDs.
- Staging and blobs must live on the same filesystem.

`std::fs::rename` must not be used blindly to install blobs because it can
overwrite the destination on Unix. Hard-link-then-unlink gives the needed
"create if absent" behavior.

Do not require full fsync durability in milestone 1. Handle errors from normal
read/write/link/chmod/remove operations. Defer fsync discipline to a durability
hardening milestone.

## Staging Cleanup

On real import startup:

- Ensure `<STORE_ROOT>/staging` exists.
- Purge only the contents of staging from prior failed runs.
- Do not delete the staging directory itself.
- Fail with context if purge fails.
- Scope purge tightly to the configured store root.

Dry-run:

- Do not create staging.
- Do not purge staging.

## Catalog Schema

Use raw SQL through `rusqlite`. No ORM.

Schema version 1:

```sql
CREATE TABLE blobs (
    hash TEXT PRIMARY KEY CHECK(length(hash) = 64),
    size_bytes INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    deleted_at_ms INTEGER
);

CREATE TABLE source_files (
    id INTEGER PRIMARY KEY,
    source_root TEXT NOT NULL CHECK(length(source_root) > 0),
    relative_path TEXT NOT NULL CHECK(
        length(relative_path) > 0
        AND substr(relative_path, 1, 1) != '/'
    ),
    blob_hash TEXT NOT NULL REFERENCES blobs(hash),
    size_bytes INTEGER NOT NULL,
    modified_at_ms INTEGER,
    first_seen_at_ms INTEGER NOT NULL,
    last_seen_at_ms INTEGER NOT NULL,
    seen_count INTEGER NOT NULL DEFAULT 1,
    UNIQUE(source_root, relative_path)
);

CREATE INDEX idx_source_files_blob_hash
ON source_files(blob_hash);

PRAGMA user_version = 1;
```

Migration behavior:

- On writable open, enable PRAGMAs and run migrations.
- If `user_version == 0`, create schema v1 inside a transaction.
- If `user_version == 1`, proceed.
- If `user_version > 1`, fail clearly.
- No down migrations.

Timestamp semantics:

- `blobs.created_at_ms`: when this catalog first recorded the content hash.
- `source_files.modified_at_ms`: observed source filesystem mtime, nullable and
  advisory.
- `source_files.first_seen_at_ms`: first time this source path was observed.
- `source_files.last_seen_at_ms`: most recent time this source path was
  observed.
- On blob conflict, preserve `blobs.created_at_ms`.
- On source conflict, preserve `first_seen_at_ms`, update `last_seen_at_ms`,
  update hash/size/mtime, and increment `seen_count`.
- `seen_count` counts every observation of the source path, even if content
  changed.

## Catalog API

Hide `rusqlite::Connection` behind `Catalog`.

Suggested behavior-level methods:

```rust
pub enum BlobRecordOutcome {
    Inserted,
    AlreadyPresent,
}

pub enum SourceObservationOutcome {
    Inserted,
    Updated,
}

impl Catalog {
    pub fn open_or_initialize(path: &Path) -> color_eyre::Result<Self>;
    pub fn record_imported_file(
        &mut self,
        blob: BlobRecord,
        observation: SourceObservation,
    ) -> color_eyre::Result<(BlobRecordOutcome, SourceObservationOutcome)>;
}
```

Use a transaction per successfully imported file:

- Insert/upsert `blobs`.
- Insert/upsert `source_files`.
- Commit.

Do not wrap the whole import in one transaction. Defer the single-writer thread,
batching, and managed checkpoints to a later milestone.

## Ingest Interface

Expose one deep operation:

```rust
pub fn import_source(config: ImportConfig) -> color_eyre::Result<ImportReport>;
```

Suggested config/report shape:

```rust
pub struct ImportConfig {
    pub store_root: StoreRoot,
    pub db_path: PathBuf,
    pub source_root: SourceRoot,
    pub dry_run: bool,
    pub chunk_size: NonZeroUsize,
}

pub struct ImportReport {
    pub files_seen: u64,
    pub blobs_created: u64,
    pub blobs_reused: u64,
    pub bytes_seen: u64,
    pub bytes_written: u64,
    pub source_records_inserted: u64,
    pub source_records_updated: u64,
}
```

Use a small clock abstraction for timestamp-writing behavior:

```rust
pub trait Clock {
    fn now_ms(&self) -> i64;
}
```

The CLI path uses `SystemClock`. Tests may use a fixed clock.

Real import flow:

1. Validate and canonicalize CLI config.
2. Reject path overlaps.
3. Create store root and known children if needed.
4. Purge staging contents.
5. Open or initialize catalog.
6. Scan source directory.
7. For each candidate:
   - call `Store::ingest_file`
   - record blob and source observation in a per-file catalog transaction
   - update `ImportReport`
8. Abort on first error.

There is no rollback. Earlier successfully imported files remain imported. ZFS
snapshots are the rollback mechanism.

## Dry Run

Dry-run means no durable mutation.

Dry-run may:

- validate paths
- scan source files
- read metadata
- hash source files
- read existing CAS files
- read an existing catalog if that can be done without mutation
- produce a report

Dry-run must not:

- create the store root
- create `blobs`
- create `staging`
- purge staging
- create or mutate blob files
- create or mutate the database
- create WAL or SHM files
- run migrations

Dry-run flow:

- Scan files.
- Hash each source file through a read-only helper.
- Check CAS presence by filesystem path:
  - missing means blob would be created
  - present with matching size means blob would be reused
  - present with different size fails as corruption
- If catalog exists, open read-only and count source record insert/update
  outcomes.
- If catalog is missing, treat it as empty.

Do not call mutating `Store::ingest_file` in dry-run. Use read-only helpers such
as `hash_file` and `blob_exists_with_size`.

Future dry-run planning may use a temporary or in-memory clone of SQLite if
normal write paths become useful, but the real store and catalog must remain
unchanged.

## Failure Semantics

- Abort the import on the first error in milestone 1.
- Do not roll back prior successful files.
- If a blob is installed but the subsequent DB commit fails, the final CAS file
  may be unreferenced. Later `audit`/`gc` can handle this.
- Never commit a DB record for a blob that only exists in staging.
- If a source file changes during import, fail with context.
- If a CAS blob exists with a different size for the computed hash, fail as
  corruption.
- Do not integrate with ZFS commands in milestone 1.

## Testing Requirements

Use the testing preferences in `RUST-TESTING.instructions.md`.

Milestone 1 acceptance tests should cover:

- CLI smoke test.
- Real import of a small directory creates expected CAS blob files.
- SQLite contains expected `blobs` and `source_files` rows.
- Re-running import reuses blobs and updates source observations idempotently.
- Identical content at different source paths creates one blob and multiple
  source records.
- Same source path with changed content updates `source_files.blob_hash` and
  increments `seen_count`.
- Dry-run against a missing store creates nothing.
- Dry-run against an existing store/catalog leaves durable state unchanged.
- Stale staging contents are purged on real import but not dry-run.
- Symlinks below the source root are ignored.
- Hidden files are included.
- Empty source directory succeeds.
- Source/store overlap is rejected.
- Explicit DB inside source is rejected.
- Blob files are read-only after import.
- Path sharding is correct.
- Schema initializes with `user_version = 1`.
- Unsupported newer schema version fails clearly.

Tests may inspect SQLite directly.

## Future Milestones

These are intentionally deferred.

### Metadata-Skip Optimization

Milestone 1 always hashes every regular source file. Do not optimize reruns with
mtime/size yet.

Future optimization:

- May default on.
- Reuse prior hash when `(source_root, relative_path, size_bytes,
  modified_at_ms)` match.
- Must be documented as relying on filesystem metadata fidelity.
- Must provide an opt-out flag/config such as `--no-metadata-skip`.
- Must trace when hashing is skipped and why.

### `build-tree`

Future command:

```text
media-importer build-tree --store <STORE_ROOT> --output <TREE_ROOT>
```

Materialize presentation symlinks from catalog state. Import must not build the
tree automatically.

Safety semantics:

- Symlink targets are relative from the link parent to the canonical blob.
- Existing correct symlinks are left unchanged.
- Existing incorrect symlinks may be replaced.
- Non-symlink entries at desired output paths are errors, never overwritten.
- Reject absolute, empty, or parent-traversing materialized relative paths before
  creating directories or links.
- Cleanup removes only symlinks and prunes only empty directories below the
  output root.

### `gc`

Future command:

```text
media-importer gc --store <STORE_ROOT> [--dry-run]
```

Use mark-and-sweep. Never run GC automatically as part of import.

### `audit`

Future command:

```text
media-importer audit --store <STORE_ROOT>
```

Check consistency between SQLite and the CAS filesystem. Mutate nothing by
default.

Useful checks:

- Report cataloged blobs whose CAS file is missing.
- Walk the CAS without following symlinks.
- Report regular CAS files that are not indexed by the catalog.
- Report CAS files whose size differs from catalog metadata.
- Defer any repair mode until its locking, dry-run, and failure semantics are
  specified.

### Relationships And Media Metadata

Defer relationship tables and media metadata tables until the first feature
needs them. Do not add unused schema in milestone 1.

### Catalog Run Locking

Future mutating commands should use a catalog-backed `run_locks` table, or an
equivalent SQLite-backed lock, to reject accidental concurrent live runs against
the same catalog.

The lock should be acquired atomically before mutating work begins, record an
owner token plus acquired/heartbeat timestamps, refresh during long runs, and be
released on normal completion. Dry-run and read-only commands do not acquire the
write lock. Stale lock breaking, waiting, and force-unlock behavior must be
explicitly specified before implementation.

### Concurrency And Writer Thread

Defer mount-point workers, single-writer DB thread, batching, and managed WAL
checkpoints. Preserve narrow catalog and store interfaces so these can be added
without changing CLI behavior or tests.

### Durability Hardening

Future work may add fsync discipline for staged files, destination directories,
chmod metadata, and coordination with SQLite durability settings.

### Resilient Batch Import

Future work may continue after per-file failures and report a failure manifest.
Milestone 1 aborts on first error.

### Quality And Tooling

Consider later:

- CI
- `cargo nextest`
- coverage tooling
- dependency/license/security policy tooling
- explicit test runtime budgets
