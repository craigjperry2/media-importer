# Milestone 2 Execution Plan: Build Browse Tree

This plan guides a coding agent through implementing
`IMPLEMENTATION-MILESTONE2.md`. Treat the milestone file as the product contract;
this plan is an execution aid that resolves sequencing, implementation risks,
and remaining alignment questions.

## Alignment Questions

Confirm these before or during implementation. Proposed answers are included so
work can proceed if the product owner accepts them.

1. Should milestone 2 override the general SQLite dry-run rule for missing
   catalogs?
   - Proposed answer: yes. For `build-tree`, both real and dry-run fail clearly
     when the selected catalog is missing, uninitialized with `user_version = 0`,
     or newer than schema version 1. The milestone-specific catalog contract is
     stricter than the general import dry-run guidance.

2. Should an empty real build create the browse-tree root?
   - Proposed answer: yes. If validation succeeds and the run is not dry-run,
     create the browse-tree root even when there are zero desired links. Count
     that root in `directories_created`. Dry-run creates nothing and reports what
     would happen.

3. Should `directories_created` include the browse-tree root itself?
   - Proposed answer: yes. The root is a directory created by this command, so
     it should be counted consistently with child directories.

4. What lexical normalization rules should classify symlink ownership?
   - Proposed answer: normalize path text without filesystem access by applying
     Unix-style path component rules: discard `.`, collapse repeated separators,
     pop one normal component for each `..` when possible, and preserve leading
     `..` components for relative paths that climb above the start. Do not
     resolve symlinks or require the target to exist.

5. How should catalog paths containing backslashes or Windows-like prefixes be
   handled on Linux/macOS?
   - Proposed answer: reject them. Catalog `relative_path` values must be
     slash-normalized portable relative paths: non-empty UTF-8, no leading `/`,
     no `.` or `..` components, no empty components, no backslash, and no
     Windows-drive or UNC-looking prefixes such as `C:` or `//server`.

6. Should symlink replacement be atomic?
   - Proposed answer: use a temp symlink in the same directory followed by
     `rename` where practical. This avoids a visible missing link during normal
     replacement while still honoring the milestone's no-rollback semantics.

7. If desired-link application fails partway through, should stale cleanup run?
   - Proposed answer: no. Abort immediately. Stale cleanup and pruning run only
     after all desired symlink create/replace operations succeed.

## Implementation Strategy

Build one vertical slice that reaches CLI, catalog, filesystem, and tests. Keep
the public deep operation:

```rust
pub fn build_tree(config: BuildTreeConfig) -> color_eyre::Result<BuildTreeReport>;
```

Recommended module ownership:

- `cli`: parse `build-tree`, convert args to validated config, dispatch, render
  summary.
- `config`: own `BuildTreeOptions` to `BuildTreeConfig` conversion if that is
  consistent with the existing import shape.
- `paths`: own browse-root validation, store-root validation for existing
  build-tree stores, containment checks, catalog relative-path validation, output
  filename suffixing, CAS path construction, lexical normalization, and relative
  symlink target generation.
- `catalog`: own read-only schema checks and the live materialization query.
- `materialize`: own planning, collision detection, CAS target validation,
  browse-tree walking, blocker detection, symlink diffing, apply, stale cleanup,
  pruning, and report counters.
- `store`: expose only narrow CAS helpers if needed; do not let materialization
  construct blob paths from raw strings.

Avoid putting SQL, filesystem walking, or symlink classification in the CLI.

## Sequenced Work

### 1. Add Types And CLI Wiring

Add `build-tree` beside `import` with:

```text
media-importer build-tree \
  --store <STORE_ROOT> \
  --browse-tree <BROWSE_TREE_ROOT> \
  [--db <DB_PATH>] \
  [--dry-run] \
  [--hash-digits <N>]
```

Implementation notes:

- Keep clap argument structs separate from validated config.
- Default `--db` to `<STORE_ROOT>/catalog.sqlite`.
- Default `--hash-digits` to `6`.
- Validate `--hash-digits` in `1..=64`.
- Do not expose `--json`, `gc`, or `audit`.
- Render exactly two summary families: real and dry-run wording.

### 2. Add Build-Tree Path Validation

Do not reuse import-oriented path validation blindly.

Required validation:

- `--store` must already exist, canonicalize to a real directory, and not be a
  symlink.
- `<store>/blobs` must already exist as a real directory and not be a symlink.
- Existing `--browse-tree` must be a real directory and not a symlink.
- Missing `--browse-tree` must have an existing real parent directory; append
  the intended final component after canonicalizing the parent.
- Reject store/browse overlap in either direction.
- Reject explicit `--db` inside the browse tree.
- Allow default or explicit DB paths inside the store.
- Dry-run performs all validation but creates nothing.

Implementation guardrail: use `symlink_metadata` when checking whether an
existing path itself is a symlink. Use `metadata` only when following links is
explicitly intended.

### 3. Add Read-Only Catalog API

Add a materialization-specific catalog opener, separate from import dry-run's
`open_if_exists` behavior.

Contract:

- Open SQLite read-only.
- Do not run migrations.
- Do not create the DB or parent directories.
- Do not intentionally create WAL or SHM files; prefer read-only flags and fail
  rather than opening writable or migrating. If SQLite requires an existing WAL
  or SHM file for a valid catalog state, document the behavior in the catalog
  helper and tests.
- Fail if missing.
- Fail if `PRAGMA user_version = 0`.
- Fail if `PRAGMA user_version > 1`.
- Require `PRAGMA user_version = 1`.

Add a colocated SQL file for live materialization entries. The query should:

- Join `source_files` to `blobs`.
- Include only `blobs.deleted_at_ms IS NULL`.
- Group by `(source_files.relative_path, source_files.blob_hash)`.
- Select `source_files.relative_path`, `source_files.blob_hash`, and
  `blobs.size_bytes`.
- Avoid source-root-specific output columns.
- Order by `relative_path, blob_hash`.

The catalog API should return typed entries, not raw rows.

### 4. Implement Pure Path Helpers First

These helpers carry much of the risk and deserve focused unit tests.

Helpers:

- Validate catalog `relative_path` from persisted text.
- Transform a final filename by appending `_<hash-prefix>` before the final
  extension when there is a non-empty stem.
- Treat dotfiles and extensionless files as extensionless.
- Compute relative symlink target text from an absolute symlink parent directory
  to an absolute CAS blob path.
- Lexically normalize symlink target text interpreted relative to a symlink
  parent.
- Check whether a normalized target path points inside `<store>/blobs`.

Unit tests should cover the examples from the milestone before filesystem
mutation code is written.

### 5. Build The Planner

Use one planning path for real and dry-run.

Planning order:

1. Load desired live catalog entries.
2. Validate each catalog relative path defensively.
3. Build desired output paths under the browse-tree root.
4. Detect hash-suffix output collisions before any mutation.
5. Validate every CAS target exists, is a regular file, and matches cataloged
   blob size.
6. Walk the browse tree if it exists.
7. Classify owned symlinks, user-managed symlinks, non-symlinks, real
   directories, and symlinked-directory blockers.
8. Compute desired link actions: create, unchanged, replace, or blocker.
9. Compute stale owned symlinks not present in the desired set.
10. Validate desired parent directories are real directories or creatable.
11. Compute directories that would be created.
12. Compute directories that would be pruned after stale owned symlink removal.

Known plan errors must be reported before mutation.

### 6. Implement Apply

Real runs apply only after planning succeeds.

Apply order:

1. Create the browse-tree root if missing.
2. Create desired parent directories.
3. Create or replace desired symlinks.
4. Remove stale owned symlinks.
5. Prune empty directories bottom-up, excluding the browse-tree root.

Failure semantics:

- Abort on first error.
- Do not roll back already completed filesystem work.
- Do not run stale cleanup if desired-link application fails.
- Never overwrite non-symlink files.
- Never replace symlinks pointing outside the configured store's `blobs` tree.
- Never delete non-symlink entries.

### 7. Add Tests In Risk Order

Start with pure helpers, then deep materialization tests, then CLI smoke tests.

Suggested order:

1. Filename suffix helper tests.
2. Relative symlink target helper tests.
3. Catalog relative path validation tests.
4. Read-only catalog schema-version tests.
5. Materialization creates expected relative symlinks.
6. Re-run reports unchanged links.
7. Replacement of owned absolute/non-canonical links.
8. Stale owned symlink cleanup, including previous hash-digit lengths.
9. User-managed symlinks and non-symlink blockers.
10. Symlinked root and symlinked parent blockers.
11. Dry-run performs no mutation and reports would counters.
12. Store/browse/db overlap failures.
13. Missing/corrupt CAS target failures.
14. Hash-suffix collision failure before mutation.
15. CLI help exposes `import` and `build-tree`.

Use tiny catalogs directly where that is clearer than invoking `import`.

## Counter Semantics

Use these meanings consistently in reports and tests:

- `desired_links`: number of desired output symlinks after catalog dedupe.
- `links_created`: desired links absent before apply and created in a real run;
  dry-run equivalent is "would be created".
- `links_unchanged`: desired symlinks whose existing target text exactly matches
  the canonical expected relative target.
- `links_replaced`: owned desired-path symlinks whose target text differs from
  the expected canonical target.
- `stale_links_removed`: owned symlinks under the browse tree that are not
  desired by the current plan.
- `directories_created`: browse-tree root plus child directories that are
  needed and absent before apply.
- `directories_pruned`: empty directories under the browse tree, excluding the
  root, after planned stale-link cleanup.

Replacement should increment only `links_replaced`, not `links_created`.

## Review Checklist

Before considering the milestone complete:

- CLI code does not own catalog queries, symlink planning, or cleanup.
- Catalog SQL lives in `.sql` files and is loaded with `include_str!`.
- `build-tree` has a stricter read-only catalog path than import dry-run.
- All known plan errors fail before filesystem mutation.
- Dry-run creates no browse root, directories, symlinks, DB files, WAL files, or
  SHM files.
- Symlink ownership never requires the target to exist.
- Cleanup removes only owned symlinks under the browse tree.
- Directory traversal never follows symlinked directories.
- The test suite covers both normal behavior and blocker safety cases.

## Verification Commands

Run from the project root inside the dev environment:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
prek run --all-files
```
