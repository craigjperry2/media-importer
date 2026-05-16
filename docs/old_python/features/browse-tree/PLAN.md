# Browseable symlink tree implementation plan

## Problem

The store is currently content-addressed and deduplicated, which is good for
reliability but hard for humans and media tooling to browse directly. Add an
optional `scan` feature that, when the user supplies a browse root, builds and
maintains a separate directory tree of symlinks that exposes imported media
under human-readable source-relative paths while continuing to store the real
files in the canonical hash-based store.

## Current state

- `scan` accepts `--store`, `--db`, repeated `--source`, `--dry-run`, and
  `--rehash-all` in `src/media_importer/cli.py`.
- Planning currently produces `AddBlobAction`, `CopyFileAction`, and
  `InsertObservationAction`; execution copies canonical files into the store
  and upserts `source_files`.
- `source_files` stores absolute source path, file name, format, size, mtime,
  hash, and `last_seen_at`, but it does not persist source-root-relative paths
  or any browse-tree metadata.
- `verify-store` only reconciles canonical blobs; it has no concept of browse
  aliases/symlinks.
- The current tests cover idempotent scans, dry-run purity, atomic copy failure
  handling, and store drift.

## Proposed approach

1. Add an optional `--browse-root` argument to the `scan` command only. If
omitted, behavior remains unchanged.
2. Persist enough metadata to reconstruct and maintain browse links
deterministically, most likely by extending `source_files` with source-root,
browse-root, and source-relative browse path fields rather than creating a
parallel ORM-like layer. This ensures `verify-store` can clean up stale
symlinks without requiring the user to pass `--browse-root`.
3. Add explicit action types for browse-tree maintenance so dry-run output
shows planned symlink work and all side-effects remain in `executor.py`.
4. When browse mode is enabled, plan creation/update of symlinks pointing at
canonical store blobs using the source-relative path from the scanned root.
Because dry-run mode does not write observations to the database during the
scan phase, the planner must construct an in-memory merged view of all existing
catalog rows (across all source roots) plus the newly scanned observations to
correctly resolve global collisions and assign paths.
5. Prune obsolete browse links for files that are no longer present under the
scanned source roots, and remove empty directories created only for browse
links.
6. Extend `verify-store` handling so missing canonical blobs do not leave
broken browse entries behind.
7. Prevent browse-path collisions statelessly by always appending a short
snippet of the file's content hash (e.g., `_[hash]`) to the filename before the
extension.
8. Reject overlapping source roots. If the user attempts to scan a source root
that is a parent or child of another source root (either in the same scan or
across previous scans recorded in the catalog), the scan must be aborted. This
prevents a single physical file from mapping to multiple differing
source-relative paths, which would corrupt the `file_path` uniqueness
assumption.

## Implementation todos

1. **Design browse metadata**
- Extend the data model/schema with source-root-relative path information and
  any helper queries needed for browse-link planning and cleanup.
- Decide whether the existing `source_files` table is sufficient or whether a
  dedicated browse-link table is justified.

2. **Add CLI surface**
- Add `--browse-root` to `scan`.
- Thread the optional browse root through the planning/execution flow without
  changing behavior when the option is absent.

3. **Plan browse actions**
- Add action dataclasses for symlink creation/update and symlink removal.
- Ensure that `store_dir`, `browse_root`, all `source_roots`, and observation
  file paths are systematically resolved to absolute paths before planning or
  executing any actions. This guarantees that relative symlink targets remain
  accurate regardless of the current working directory.
- Compute browse-relative paths from each scanned source root (using the fully
  resolved absolute paths to avoid `ValueError` during path resolution).
- Detect existing/stale browse entries so repeated scans stay idempotent.

4. **Execute browse actions**
- Create/update symlinks under the browse root, ensuring parent directories
  exist.
- Remove stale symlinks and clean up empty browse directories safely.
- Keep DB updates and filesystem effects consistent with the existing executor
  transaction/copy flow.

5. **Handle verification and drift**
- Ensure `verify-store` removes or invalidates browse links that target missing
  canonical blobs.
- Keep the browse tree recoverable on subsequent scans.

6. **Add tests and docs**
- Add coverage for initial symlink creation, idempotent rescans, stale-link
  pruning, dry-run output, and verify-store interaction.
- Document the new CLI option and expected behavior in `README.md`.
- Add tests proving that all files receive a `<name>_[hash].ext` suffix to
  prevent collisions.

## Resolved behavior

- To prevent collisions and ensure that symlink assignments remain perfectly
  stable even if other conflicting files are added or removed, all browse paths
  must unconditionally append a short snippet of the file's content hash (e.g.,
  the first 7 characters) to the filename, before the extension.

