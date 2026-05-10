# Browse functionality implementation guide

This document is for the coding agent that will implement the optional browse
tree feature described in `PLAN.md` and specified by
`tests/test_browse_feature.py`.

## Goal

Add an optional `--browse-root` flag to `media-importer scan` that maintains a
human-browseable tree of symlinks mirroring the source-relative paths of
imported media while keeping the canonical store unchanged.

The real media files must continue to live only in the content-addressed store.
The browse tree is just a symlink overlay.

## Acceptance criteria

The implementation is done when all of these pass without weakening the tests:

- `tests/test_browse_feature.py`
- existing scan / planner / executor / catalog / CLI tests

The key required behaviors are:

1. `scan --browse-root ...` creates source-relative symlinks that point to the
canonical blob.
2. Re-running the same scan is idempotent.
3. Path collisions are prevented by ensuring all files are suffixed:
   - all browse symlinks become `name_[hash].ext` where `[hash]` is a short
     snippet (e.g., 7 chars) of the file's hash.
4. If a previously scanned source file disappears, its browse symlink is
removed and empty browse directories are pruned.
5. `verify-store` removes browse symlinks for blobs that no longer exist in the
canonical store.
6. `scan --dry-run --browse-root ...` plans the work but does not modify the
filesystem.

## Important constraints

- Keep `planner.py` pure. It should only compute actions.
- Keep all filesystem and database writes in `executor.py`.
- Use `pathlib.Path` consistently. The current codebase has already moved to
  `Path`.
- Do not replace the canonical hash-based store layout.
- Do not add an ORM. Stay with raw `sqlite3`.

## Recommended design

Use the existing `source_files` table as the source of truth for browse
metadata. Do **not** add a separate browse table unless absolutely necessary.

Recommended new `source_files` columns:

- `source_root TEXT`
- `source_rel_path TEXT`
- `browse_root TEXT`
- `browse_rel_path TEXT`

Rationale:

- `file_path` is already the stable key for an observation.
- each observation needs to remember which source root it came from and what
  its natural source-relative path is
- `browse_root` must be persisted so `verify-store` has the absolute path
  required to remove broken symlinks without needing a `--browse-root` CLI argument.
- `browse_rel_path` lets rescans remain stable and lets the planner compare
  desired vs current browse placement

## Required model changes

### `src/media_importer/models.py`

Extend `FileObservation` with optional browse-related fields so old tests that
construct it manually do not break:

- `source_root: Path | None = None`
- `source_rel_path: Path | None = None`
- `browse_root: Path | None = None`
- `browse_rel_path: Path | None = None`

Add explicit action types for browse maintenance and stale source cleanup.
Recommended actions:

- `CreateOrUpdateBrowseSymlinkAction`
  - `browse_root: Path`
  - `browse_rel_path: Path`
  - `target_store_path: Path`
- `RemoveBrowseSymlinkAction`
  - `browse_root: Path`
  - `browse_rel_path: Path`
- `UpdateBrowsePathAction`
  - `file_path: Path`
  - `browse_root: Path | None`
  - `browse_rel_path: Path | None`
- `DeleteObservationAction`
  - `file_path: Path`

Keep `AddBlobAction`, `CopyFileAction`, `InsertObservationAction`, and
`MarkStaleAction`.

## Required catalog changes

### `src/media_importer/catalog.py`

Extend schema initialization for the new `source_files` columns.

Update observation reads and writes so `FileObservation` round-trips:

- `file_path`
- `file_name`
- `file_format`
- `size_bytes`
- `mtime`
- `file_hash`
- `last_seen_at`
- `source_root`
- `source_rel_path`
- `browse_root`
- `browse_rel_path`

Add catalog helpers for planner use. Recommended helpers:

- `get_observations_for_hash(file_hash: str) -> list[FileObservation]`
- `get_stale_observations(source_roots: list[Path], last_seen_at: float) ->
  list[FileObservation]`
- `get_all_live_observations() -> list[FileObservation]`

Notes:

- `get_stale_observations(...)` should return rows under the scanned source
  roots whose `last_seen_at` is older than the current scan timestamp.
- `get_all_live_observations()` should return all rows across all source roots.

## Required CLI changes

### `src/media_importer/cli.py`

Add `--browse-root` to the `scan` command only:

```text --browse-root PATH ```

Keep it optional.

Thread it through to:

- `Planner(...)`
- `Executor(...)`

`verify-store` does not need a new CLI option if planner / executor can infer
browse cleanup from catalog state.

## Planner design

### `src/media_importer/planner.py`

Update `Planner.__init__` to accept:

- `catalog: Catalog`
- `store_dir: Path`
- `browse_root: Path | None = None`

### 1. Continue planning canonical store actions per observation

`plan_observation(...)` should remain focused on blob and observation
persistence. It should not perform filesystem writes.

Before `plan_observation(...)` is called, each scanned observation should be
enriched with:

- `source_root` (must be resolved/canonicalized to an absolute path first)
- `source_rel_path`

Use:

- `resolved_root = source_root.resolve()`
- `source_rel_path = obs.file_path.relative_to(resolved_root)`

### 2. Add a post-scan browse reconciliation phase

This is the key design choice.

Do **not** try to fully assign browse names one observation at a time during
streaming scan planning. Collision handling and stale cleanup are easier and
more deterministic if browse planning happens in a second phase after the scan
observations have been recorded or planned.

Add a planner method along these lines:

- `plan_browse_reconciliation(source_roots: list[Path], scanned_at: float, current_scan_observations: list[FileObservation]) ->
  list[Action]`

This method should:

1. find stale observations for the scanned roots
2. emit actions to remove their browse symlinks
3. emit actions to delete those stale observation rows
4. construct an in-memory merged view of the catalog's existing live observations plus the `current_scan_observations` (crucial for dry runs since planned rows aren't in the DB yet)
5. compute the desired browse path for every observation in this merged view
6. compare desired browse path vs stored `browse_rel_path`
7. emit actions to create/update symlinks and persist new `browse_rel_path`
values

### 3. Collision prevention

Collision handling must remain stable. By appending a short snippet of the
file's content hash to all browse paths, we can avoid stateful "first wins"
assignment logic.

For each observation:

1. start from `source_rel_path`
2. unconditionally append a short hash suffix (e.g., the first 7 chars of
`file_hash`) to the file stem, before the extension.

Example:

- `Movies/zabba/zabba_0000000.mp4`
- `Movies/zabba/zabba_a1b2c3d.mp4`
- `Movies/zabba/zabba_e5f6g7h.mp4`

### 4. Overlapping source roots prevention

To protect the unique `file_path` assumption, a single physical file must not be scanned under multiple conflicting source roots (e.g., scanning `/photos` and `/photos/2024`).

Update the planner (or CLI) to validate source roots before scanning:

1. Check for overlaps among the current scan's `--source` arguments.
2. Query the catalog for all existing unique `source_root` values.
3. Check for overlaps between the current scan's `--source` arguments and the catalog's existing source roots.
4. If an overlap is detected (where one root is a parent or child of another), raise a `ValueError` with a clear message and abort the scan. Identical source roots (exact matches) are allowed for rescans.

### 5. Stale source handling

The current scan flow only updates observations it sees. To satisfy the pruning
test, add a post-scan stale cleanup step.

Use the scan start time as `scanned_at`.

Any observation under one of the current `source_roots` whose `last_seen_at <
scanned_at` is stale for this run and should be removed from:

- browse tree
- `source_files`

### 6. Verify-store handling

`plan_verify_store()` must also clean browse entries when a canonical blob is
missing.

For each missing blob:

1. find observations referencing that hash
2. emit `RemoveBrowseSymlinkAction` for each observation with a browse path
3. emit the existing `MarkStaleAction`

This ordering matters because `MarkStaleAction` deletes the blob row, and the
existing foreign key cascade may remove source rows.

## Executor design

### `src/media_importer/executor.py`

Update `Executor.__init__` to accept:

- `catalog: Catalog`
- `store_dir: Path`
- `browse_root: Path | None = None`

### 1. Canonical copy behavior stays as-is

Do not change the content-addressed copy semantics beyond what is required to
coexist with browse actions.

### 2. Implement browse symlink actions

Add executor handling for:

- create/update browse symlink
- remove browse symlink
- remove empty parent directories after symlink deletion
- update `browse_rel_path` in `source_files`
- delete stale observations from `source_files`

Recommended symlink behavior:

- create parent directories as needed
- create symlinks that point to the canonical store file
- prefer **relative symlink targets** computed from the symlink’s parent
  directory to the store file
- if the symlink already exists and points to the correct target, do nothing
- if a wrong symlink exists, replace it
- if a non-symlink filesystem entry exists at the browse path, raise an
  explicit error instead of deleting user data silently

### 3. Empty directory cleanup

After removing a browse symlink, prune empty directories upward until:

- you reach `browse_root`, or
- the directory is no longer empty

Do not remove `browse_root` itself.

### 4. Database updates

Handle the new actions inside `_db_operations(...)`.

Recommended SQL responsibilities:

- `InsertObservationAction`: upsert the new metadata columns too
- `UpdateBrowsePathAction`: update `browse_root` and `browse_rel_path`
- `DeleteObservationAction`: delete from `source_files`
- `MarkStaleAction`: continue deleting from `blobs`

### 5. Action ordering

Make sure browse filesystem actions happen in an order that avoids broken
intermediate state:

- remove stale browse symlinks before deleting their DB rows
- create canonical blob files before creating symlinks that target them

If needed, split execution into phases instead of assuming one flat action list
is enough.

## Scan flow changes

### `src/media_importer/cli.py`

The current scan flow has two modes:

- dry-run: `_plan_scan_actions_with_progress(...)`
- live execution: `_execute_scan(...)` with batched writes

Update both paths so browse reconciliation runs after observation scanning.

### Recommended live execution flow

1. scan sources and batch canonical actions as today
2. flush the final scan batch
3. if `browse_root is not None`, run stale cleanup + browse reconciliation as a
second phase
4. fail the scan if either phase fails

### Recommended dry-run flow

Return the combined action list:

1. canonical scan actions
2. stale observation cleanup actions
3. browse symlink/update actions

This keeps dry-run honest and avoids modifying the filesystem.

## Suggested implementation order

1. **Schema + models**
   - add optional browse fields to `FileObservation`
   - add new action dataclasses
   - extend catalog schema and queries

2. **CLI plumbing**
   - add `--browse-root`
   - pass it into `Planner` and `Executor`

3. **Observation enrichment**
   - attach `source_root` and `source_rel_path` before planning insert actions

4. **Executor support**
   - implement browse symlink create/remove helpers
   - implement empty-dir pruning
   - add SQL support for new actions

5. **Planner browse reconciliation**
   - stale observation detection
   - unconditional hash suffix assignment
   - browse path update planning

6. **Verify-store browse cleanup**
   - remove browse symlinks before blob deletion

7. **README**
   - document `--browse-root`

## Edge cases to handle carefully

- overlapping source roots: must be explicitly rejected before scanning begins
- multiple source roots with the same relative path
- duplicate file content from different source paths: each source path still
  gets its own browse symlink, even if both point to the same canonical blob
- rescans with no changes must not churn `browse_rel_path`
- stale browse cleanup must only affect the source roots included in the
  current scan
- `verify-store` must clean browse links even though it only receives `--store`
  and `--db`

## Validation checklist

Run these after implementation:

```sh pytest ruff check . mypy src tests ```

## Minimum file set expected to change

- `src/media_importer/models.py`
- `src/media_importer/catalog.py`
- `src/media_importer/planner.py`
- `src/media_importer/executor.py`
- `src/media_importer/cli.py`
- `README.md`

## Tests that should go green

- `tests/test_browse_feature.py`
- `tests/test_cli.py`
- `tests/test_planner.py`
- `tests/test_executor.py`
- `tests/test_catalog.py`

The implementation should make the new tests pass by adding the missing
feature, not by softening the assertions.

