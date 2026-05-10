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
  `InsertObservationAction`; execution copies canonical files into the store and
  upserts `source_files`.
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
deterministically, most likely by extending `source_files` with source-root and
source-relative browse path fields rather than creating a parallel ORM-like
layer.
3. Add explicit action types for browse-tree maintenance so dry-run output shows
planned symlink work and all side-effects remain in `executor.py`.
4. When browse mode is enabled, plan creation/update of symlinks pointing at
canonical store blobs using the source-relative path from the scanned root.
5. Prune obsolete browse links for files that are no longer present under the
scanned source roots, and remove empty directories created only for browse
links.
6. Extend `verify-store` handling so missing canonical blobs do not leave broken
browse entries behind.
7. Resolve browse-path collisions within a directory by keeping the first path
unchanged and prefixing later conflicting filenames with `1_`, `2_`, and so on
until a free name is found.

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
- Compute browse-relative paths from each scanned source root.
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
- Add collision tests proving that the first matching path keeps its original
  name and later conflicts become `1_<name>`, `2_<name>`, etc.

## Resolved behavior

- If two or more observations map to the same browse-relative path, the first
  one keeps the unmodified filename.
- Later collisions in that same directory are deconflicted by prefixing the
  filename with an incrementing number (`1_`, `2_`, ...), checking for the next
  available name each time.
- The implementation should make that numbering deterministic so repeated scans
  do not churn symlink names unnecessarily.
