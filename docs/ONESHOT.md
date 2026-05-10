# ONESHOT: Media Importer Reimplementation Spec

This document describes the observable behavior of the Media Importer application.
A coding agent should be able to build an equivalent project from this spec even
if the internal code structure differs.

## Product Goal

Build a command-line tool named `media-importer` that incrementally consolidates
files from one or more source directories into a deduplicated, content-addressed
store, while maintaining a SQLite catalog as both an index and a hash cache.

The tool also optionally maintains a human-browsable symlink overlay that mirrors
source-relative paths while leaving real file bytes only in the canonical store.

## Technology Constraints

- Use Python 3.13 or newer.
- Expose a console script named `media-importer`.
- Use only the Python standard library at runtime.
- Use raw `sqlite3`; do not use an ORM.
- Keep state-comparison/planning pure: planning may inspect the catalog and
  filesystem but must not write to the database or filesystem.
- Keep all database writes, file copies, symlink creation/removal, and directory
  creation/removal in the executor/effectful layer.
- Use `pathlib.Path` or an equivalent path abstraction consistently.

## Core Concepts

### Blob

A blob is unique file content stored once in the canonical store.

Required fields:

- `file_hash`: BLAKE2b hexadecimal digest of the file bytes.
- `size_bytes`: source file size in bytes.
- `store_path`: path relative to the store root.
- `first_seen_at`: Unix timestamp recorded when the blob is first cataloged.

### File Observation

A file observation is a source-path sighting of a blob.

Required fields:

- `file_path`: source file path.
- `file_name`: basename.
- `file_format`: lowercase suffix, including the leading dot, or empty string.
- `size_bytes`: size in bytes.
- `mtime`: filesystem modification time.
- `file_hash`: content hash, nullable before hashing.
- `last_seen_at`: Unix timestamp for the latest scan that saw this path.
- `source_root`: resolved source root used for browse mode, nullable.
- `source_rel_path`: source-relative file path used for browse mode, nullable.
- `browse_root`: resolved browse root used for browse mode, nullable.
- `browse_rel_path`: browse-root-relative symlink path, nullable.

### Action Plan

Planning produces explicit action objects/records such as:

- add a blob row
- copy a source file to a store path
- insert or update an observation row
- mark a blob stale/delete it from the catalog
- record a source root
- create or update a browse symlink
- remove a browse symlink
- update browse metadata for an observation
- delete a stale observation

Dry runs print these planned actions and perform no writes.

## Hashing

- Hash file contents with `hashlib.blake2b()`.
- Read files incrementally in chunks; default chunk size may be 8192 bytes.
- The hash result must be independent of chunk size.

## Scanning

- Recursively walk each source directory.
- Resolve each source root before walking.
- Do not follow symlinks while walking directories.
- Skip symlink files.
- If `stat()` or hashing a file raises `OSError`, skip that file and count it
  as skipped.
- For each regular file, capture basename, lowercase suffix, byte size, mtime,
  and eventually BLAKE2b hash.

## Store Layout

The canonical store is content addressed:

```text
<store>/<first-two-hash-chars>/<full-hash><lowercase-extension>
```

Examples:

```text
store/ab/abcdef....jpg
store/00/001122....mp4
```

Duplicate content is stored once, even if seen from many source paths. Each
source path still receives its own observation row.

## SQLite Catalog

Create parent directories for the database path when opening for writes.

Required tables:

```sql
CREATE TABLE IF NOT EXISTS blobs (
    file_hash TEXT PRIMARY KEY,
    size_bytes INTEGER,
    store_path TEXT UNIQUE,
    first_seen_at REAL
);

CREATE TABLE IF NOT EXISTS source_files (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_path TEXT UNIQUE,
    file_name TEXT,
    file_format TEXT,
    size_bytes INTEGER,
    mtime REAL,
    file_hash TEXT,
    last_seen_at REAL,
    source_root TEXT,
    source_rel_path TEXT,
    browse_root TEXT,
    browse_rel_path TEXT,
    FOREIGN KEY(file_hash) REFERENCES blobs(file_hash) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS source_roots (
    source_root TEXT PRIMARY KEY
);
```

Required indexes:

- `source_files(file_name)`
- `source_files(file_format)`
- `source_files(file_hash)`

Enable foreign keys and use WAL journal mode for writable catalogs.

For compatibility with older databases, ensure missing browse/source metadata
columns are added to `source_files`.

Read-only behavior:

- If the database exists, open it read-only.
- If the database does not exist and the operation is read-only/dry-run, use an
  in-memory initialized schema so queries and dry-run planning can proceed
  without creating a database file.

## Command Line Interface

### `media-importer scan`

Required options:

- `--store PATH`
- `--db PATH`
- `--source PATH`, repeatable

Optional flags:

- `--dry-run`
- `--browse-root PATH`
- `--rehash-all`

Behavior:

1. Print progress to stderr.
2. Scan all source roots.
3. Reuse cached hashes when possible.
4. Plan blob additions, file copies, and observation upserts.
5. In live mode, execute copies and database updates in bounded batches.
6. In dry-run mode, print the number of actions and each action to stdout.
7. Exit non-zero if execution fails.

Hash cache behavior:

- Look up an existing observation by exact `file_path`.
- If `--rehash-all` is false and existing `size_bytes` and `mtime` match the
  current file, reuse the stored `file_hash`.
- Otherwise rehash the file.
- Preserve existing browse metadata during rehash unless current browse mode
  supplies a browse root.

Deduplication behavior:

- A scan must deduplicate within the current action plan.
- Live batched execution must also deduplicate across batch boundaries.
- If two source files have identical content in the same run, only one blob row
  and one store copy are needed, but both observations are inserted/updated.

Batching behavior:

- Live scans process canonical copy/database actions in bounded batches.
- The current implementation uses a 256 MiB byte threshold, but an equivalent
  implementation only needs bounded incremental execution.
- Dry runs compute and print the complete action list up front.

Progress behavior:

- At scan start, print either `Starting scan across N source(s)` or
  `Starting dry-run planning across N source(s)`.
- When entering a source, print `Scanning source: <path>`.
- During planning, periodically report processed files, new files to copy,
  observations to record, planned actions, and skipped files when nonzero.
- Live mode prints `Executing scan batches`, copy progress every 25 copies, and
  a completion or error summary.
- Dry-run mode prints a dry-run completion summary.

### `media-importer scan --browse-root`

Browse mode adds a symlink overlay under `--browse-root`.

Additional behavior:

- Resolve store root, browse root, source roots, and file paths to absolute
  canonical paths before computing source-relative paths or symlink targets.
- Persist source roots in `source_roots`.
- Enrich each observation with:
  - resolved `source_root`
  - `source_rel_path`, relative to that source root
  - resolved `browse_root`
- After canonical scan planning/execution, reconcile browse symlinks and stale
  source observations.

Browse symlink layout:

- The browse tree mirrors source-relative directories.
- Every browse filename is suffixed with the first seven characters of the file
  hash before the extension.
- This suffix is unconditional, not only on collision.

Example:

```text
source/Movies/zabba/zabba.mp4
hash = abcdef123...
browse/Movies/zabba/zabba_abcdef1.mp4 -> ../../store/ab/abcdef123....mp4
```

The symlink target should be relative from the symlink parent to the canonical
store file.

Collision and duplicate behavior:

- Two files with the same source-relative path from different roots must not
  collide because their hash suffixes differ when content differs.
- Two files with the same content and same relative path from different roots
  may naturally resolve to the same browse path and target; creating/updating
  that symlink idempotently is acceptable.
- Duplicate content from different source paths still receives distinct
  observation rows.

Idempotency:

- Re-running the same browse scan should not create duplicate browse entries.
- If an existing symlink already points to the desired relative target, leave it
  unchanged.
- If an existing symlink points elsewhere, replace the symlink.
- If a non-symlink filesystem entry exists at the desired browse path, fail
  rather than deleting or replacing user data.

Stale source cleanup in browse mode:

- Detect stale observations only under the source roots included in the current
  scan.
- An observation is stale when its `last_seen_at` is older than the current scan
  timestamp and the current scan did not observe that exact file path.
- For stale observations:
  - remove their browse symlink if browse metadata is present
  - delete the `source_files` row
- After removing a browse symlink, prune empty parent directories upward until
  reaching the browse root. Never remove the browse root itself.

Overlapping source roots:

- In browse mode, reject overlapping source roots before scanning.
- Check overlaps among the current `--source` arguments.
- Check overlaps between current sources and previously recorded source roots.
- Exact same source root is allowed for rescans.
- Parent/child relationships are not allowed, because they would make the same
  physical file map to multiple source-relative paths.

Dry-run browse behavior:

- `scan --dry-run --browse-root` must plan canonical and browse actions but
  must not create the store, browse tree, or database file.
- Dry-run browse planning must reconcile against an in-memory merged view of:
  - existing live catalog observations
  - current scan observations
  - stale observations removed for the scanned roots
- This is necessary so dry-run output reflects the browse paths that live mode
  would create.

### `media-importer verify-store`

Required options:

- `--store PATH`
- `--db PATH`

Optional flag:

- `--dry-run`

Behavior:

1. For every cataloged blob, check whether `<store>/<store_path>` exists.
2. If a cataloged blob file is missing:
   - plan removal of every browse symlink for observations referencing that hash
   - then delete the blob row from `blobs`
   - rely on `ON DELETE CASCADE` to remove referencing `source_files` rows
3. Walk the store directory for regular non-symlink files.
4. For every store file not indexed by a blob row:
   - hash it
   - if no blob with that hash exists, add a blob row whose `store_path` is the
     file path relative to the store root
   - do not copy or move the file
5. Skip unreadable/unhashable unindexed store files.
6. In dry-run mode, print planned actions and perform no writes.
7. In live mode, execute actions and exit non-zero on failure.

`verify-store` does not accept `--browse-root`; it must use persisted
`browse_root` and `browse_rel_path` metadata to remove broken browse symlinks.

### `media-importer query`

Required option:

- `--db PATH`

Optional filters:

- `--ext EXT`: exact match against `source_files.file_format`
- `--name TEXT`: substring match against `source_files.file_name` using SQL
  `LIKE '%TEXT%'`
- `--hash HASH`: exact match against `source_files.file_hash`

Behavior:

- Open the catalog read-only.
- Print each matching row as a Python-style dictionary to stdout.
- Combine filters with `AND`.
- If the database does not exist, return no rows and do not create it.

## Executor Semantics

### File Copy

- For each copy action, create the destination parent directory.
- Copy metadata-preserving from source to a temporary path beside the final
  destination, for example `<dest>.tmp`.
- Flush/fsync the temporary file.
- Atomically replace the final destination with the temporary file.
- If a copy fails, record that hash as failed and continue processing other
  copy actions.
- Do not insert blob or observation rows for failed hashes.
- Successful copies in the same execution should still be committed even if
  other hashes failed.

### Database Writes

- Perform database mutations in transactions.
- Upsert observations by `file_path`, updating all observation fields.
- Insert blobs with `INSERT OR IGNORE` semantics.
- Delete stale blobs by `store_path`.
- Record source roots with `INSERT OR IGNORE`.
- Update browse metadata by `file_path`.
- Delete stale observations by `file_path`.
- If database/browse filesystem execution fails after copies, report failure
  and exit non-zero from the CLI.

### Browse Path Safety

Before creating or removing a browse symlink:

- Reject absolute `browse_rel_path`.
- Reject any `..` component.
- Resolve the browse root.
- Resolve the symlink parent with `strict=False`.
- Ensure the final link path is still under the resolved browse root.
- If not, raise an error before creating directories or symlinks.

Removal safety:

- Remove only symlinks.
- If a non-symlink exists at the browse path, raise an error.
- Missing browse paths are harmless.

## Important Behavioral Boundaries

- Ordinary scans without `--browse-root` do not delete source observations just
  because source files disappear. Stale source cleanup is part of browse
  reconciliation.
- `verify-store` removes catalog rows for missing canonical blobs and therefore
  may cascade-delete observations.
- Scanner ignores symlinks in both source trees and store verification walks.
- File extensions in store paths and observations are lowercase.
- The catalog stores paths as strings and returns them as path objects or their
  equivalent.
- Relative CLI paths must work correctly; internally resolve paths where needed
  for stable source-relative and symlink behavior.

## Suggested Test Coverage

An equivalent implementation should pass tests for:

- chunked BLAKE2b hashing produces stable results
- dry-run scan does not create store or database
- first scan plans/adds one blob, one copy, and one observation per unique file
  content/path as appropriate
- duplicate content deduplicates to one blob and one copy
- duplicate content deduplicates across live scan batches
- second unchanged scan only updates observations
- copy failure does not create blob/observation rows for the failed hash
- partial copy failure preserves successful rows
- missing canonical store file is detected by `verify-store`
- unindexed store files can be indexed by `verify-store`
- query filters by extension, name substring, and hash
- browse scan creates source-relative hash-suffixed symlinks
- browse scan works with relative CLI paths
- browse rescan is idempotent
- browse hash suffixes prevent collisions across same relative paths
- browse dry-run leaves filesystem and database untouched
- browse dry-run plans against existing catalog state
- stale source files remove browse symlinks and empty directories
- `verify-store` removes browse symlinks for missing canonical blobs
- overlapping browse source roots are rejected
- absolute or parent-traversing browse relative paths are rejected before any
  directory or symlink is created
- non-symlink browse entries are never silently replaced or removed

## Reference Commands

```sh
pytest
media-importer scan --store store-root --db db.sqlite --source src
media-importer scan --store store-root --db db.sqlite --source src --dry-run
media-importer scan --store store-root --browse-root browse --db db.sqlite --source src
media-importer verify-store --store store-root --db db.sqlite
media-importer query --db db.sqlite --ext .jpg --name vacation
```
