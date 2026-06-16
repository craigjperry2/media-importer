# Rust Implementation Guide: Milestone 2

This guide records implementation decisions for milestone 2 of the Rust rewrite
of `media-importer`. The source of product intent is `SPEC.md`; this file
captures the concrete choices a coding agent should follow while implementing
the next vertical slice.

Milestone 1 implemented directory import into the content-addressed store (CAS)
and catalog. Milestone 2 adds browse-tree materialization from that existing
catalog and CAS.

The existing Python implementation is deprecated historical context. Do not port
it file-by-file and do not use it as a behavior oracle.

## Companion Instructions

Use these colocated instruction files when implementing this guide:

- `RUST.instructions.md`
- `RUST-ARCHITECTURE.instructions.md`
- `RUST-TESTING.instructions.md`
- `SQLITE.instructions.md`

## Milestone 2: Build Browse Tree

Implement one end-to-end vertical slice:

```text
media-importer build-tree \
  --store <STORE_ROOT> \
  --browse-tree <BROWSE_TREE_ROOT> \
  [--db <DB_PATH>] \
  [--dry-run] \
  [--hash-digits <N>]
```

Expose the `build-tree` command in addition to the existing `import` command.
Do not expose placeholder commands for `gc` or `audit`.

`build-tree` materializes a browse tree of symlinks from live catalog rows to CAS
blob files. The browse tree is a generated presentation view. It is not the CAS
and it does not own non-symlink user files.

## CLI Contract

- `--store` is required.
- `--store` must already exist as a real directory.
- `<STORE_ROOT>/blobs` must already exist as a real directory.
- `build-tree` must not create store directories.
- `--browse-tree` is required.
- `--browse-tree` names the browse tree root.
- If the browse tree root exists, it must be a real directory, not a symlink.
- If the browse tree root does not exist, its parent must already exist.
- Real `build-tree` may create only the final browse tree root and children
  below it.
- Dry-run must create nothing.
- `--db` is optional and defaults to `<STORE_ROOT>/catalog.sqlite`.
- Explicit `--db` is allowed inside the store root.
- Explicit `--db` must not be inside the browse tree.
- `--dry-run` validates and reports the plan without mutating the filesystem.
- `--hash-digits` is optional and defaults to `6`.
- `--hash-digits` must be in the inclusive range `1..=64`.
- Do not expose `--json` in milestone 2.

Use `clap` derive for parsing. Keep CLI argument structs separate from validated
application config.

Install `color-eyre` in `main`, initialize `tracing-subscriber`, convert CLI
arguments into validated config, call a deep materialization API, then render a
summary.

## Output

On success, print a concise human-readable summary to stdout.

Example real run:

```text
Build tree complete
Desired links: 3
Links created: 2
Links unchanged: 1
Links replaced: 0
Stale links removed: 1
Directories created: 2
Directories pruned: 1
```

Example dry run:

```text
Dry run complete
Desired links: 3
Links that would be created: 2
Links that would be left unchanged: 1
Links that would be replaced: 0
Stale links that would be removed: 1
Directories that would be created: 2
Directories that would be pruned: 1
```

Use `tracing` for internal diagnostics. Defer progress bars and structured
non-TTY logs.

## Module Boundaries

Add a `materialize` module, or an equivalently named browse-tree module, with one
deep operation:

```rust
pub fn build_tree(config: BuildTreeConfig) -> color_eyre::Result<BuildTreeReport>;
```

Suggested config/report shape:

```rust
pub struct BuildTreeConfig {
    pub store_root: StoreRoot,
    pub db_path: PathBuf,
    pub browse_tree_root: BrowseTreeRoot,
    pub dry_run: bool,
    pub hash_digits: NonZeroUsize,
}

pub struct BuildTreeReport {
    pub dry_run: bool,
    pub desired_links: u64,
    pub links_created: u64,
    pub links_unchanged: u64,
    pub links_replaced: u64,
    pub stale_links_removed: u64,
    pub directories_created: u64,
    pub directories_pruned: u64,
}
```

The exact type names may differ, but preserve the boundaries:

- CLI parses arguments, validates command-level input, dispatches, and renders
  summaries.
- `paths` owns canonicalization, containment checks, path serialization, CAS path
  construction, browse-tree path validation, and pure path helpers.
- `catalog` owns read-only SQLite access and raw SQL.
- `materialize` owns desired-link planning, symlink diffing, filesystem
  mutation, stale-link cleanup, and directory pruning.
- `store` may expose CAS path helpers only through typed store/path APIs.

Do not let CLI code choreograph catalog queries, symlink planning, or cleanup.

## Path Validation

Use the existing path-safety style from milestone 1 and add browse-tree-specific
validation.

Required path/domain behavior:

- `StoreRoot` for `build-tree` must be canonical and existing.
- Existing store roots must be real directories, not symlinks.
- `StoreRoot::blobs_dir()` must exist and be a real directory.
- `BrowseTreeRoot` should be a validated intended browse-tree root.
- If the browse tree exists, canonicalize it and require a real directory.
- If the browse tree does not exist, require its parent exists and is a real
  directory, canonicalize the parent, and append the intended final component.
- Reject store/browse-tree overlap in either direction.
- Reject explicit database paths inside the browse tree.
- Allow the default catalog path inside the store.
- Allow explicit database paths inside the store.
- Existing symlinked parent directory components needed for desired links are
  blockers and must fail clearly.
- Only real directories should be traversed or created inside the browse tree.

Defensively validate catalog `relative_path` values before using them, even
though milestone 1 writes only validated UTF-8 source-relative paths. Reject
catalog paths that are empty, absolute, parent-traversing, platform-prefixed, or
otherwise not slash-normalized relative paths.

## Catalog Access

Milestone 2 makes no schema changes. Use schema version 1.

`build-tree` is read-only with respect to the catalog:

- Open the catalog read-only.
- Do not run migrations.
- Do not create database files.
- Do not create WAL or SHM files if avoidable.
- Fail clearly if the database is missing.
- Fail clearly if `PRAGMA user_version` is `0`.
- Fail clearly if `PRAGMA user_version` is newer than `1`.
- Require `PRAGMA user_version = 1` for milestone 2.

Keep raw SQL inside the catalog module in colocated `.sql` files loaded with
`include_str!`.

Add a behavior-level catalog method that returns typed live materialization rows.
The materialization module must not own SQL.

Suggested row shape:

```rust
pub struct LiveMaterializationEntry {
    pub relative_path: SourceRelativePath,
    pub blob_hash: BlobHash,
    pub blob_size_bytes: u64,
}
```

Desired links are a projection over live blobs:

- Include only rows joined to `blobs.deleted_at_ms IS NULL`.
- Include all live catalog entries; do not filter by media extension or MIME
  type.
- Collapse duplicate observations that would produce the same desired link.
- Group by `(source_files.relative_path, source_files.blob_hash)`.
- Select `blobs.size_bytes` from the joined `blobs` row as authoritative for CAS
  target validation.
- Do not select source-root-specific columns for materialization.
- Order desired entries deterministically by `relative_path, blob_hash`.

The query should express the dedupe rule deliberately with `GROUP BY`, not by
blindly adding `SELECT DISTINCT` to hide a poorly understood row shape.

## Browse-Tree Naming

The browse tree preserves cataloged relative directory paths and transforms only
the final filename component.

For every desired link, append an underscore plus the configured BLAKE3 hash
prefix to the filename:

```text
Movies/Clashing Name.mp4 + abc123... -> Movies/Clashing Name_abc123.mp4
Movies/README + abc123...           -> Movies/README_abc123
Movies/.env + abc123...             -> Movies/.env_abc123
```

Rules:

- The hash suffix is presentation-only behavior.
- Store blob filenames remain extensionless full BLAKE3 hashes.
- Preserve a final extension only when there is a non-empty stem before it.
- Extensionless names receive the suffix at the end of the whole filename.
- Dotfiles such as `.env` are treated like extensionless names.
- Multiple-dot names preserve only the final extension as the extension.
- Unicode UTF-8 filenames are allowed when already valid catalog relative paths.
- The hash prefix length is `--hash-digits`, defaulting to `6`.
- Valid hash prefix length is `1..=64`.

Filename suffix generation should be a pure helper with focused tests.

## Hash-Suffix Collision Handling

Detect output path collisions after applying the configured hash prefix length.

A collision occurs if two different full blob hashes and relative paths produce
the same transformed browse-tree output path. This is unlikely with a 6-hex
prefix but possible because the browse-tree identity uses only the configured
prefix.

On collision:

- Fail before mutating anything.
- Include the colliding output path and involved hashes in the error when
  practical.
- Tell the user to discard and rebuild the browse tree with more hash digits,
  for example `--hash-digits 12`.

Do not silently pick one entry.

## Symlink Targets

Create Unix symlinks directly. This project targets Linux and macOS only; do not
design Windows symlink behavior in milestone 2.

Symlink targets must be relative paths from the symlink parent directory to the
CAS blob file.

Add a pure helper for computing canonical relative symlink targets between two
absolute paths. Test at least:

- same-directory paths
- parent-directory paths
- nested-directory paths
- sibling-directory paths

An existing desired symlink is unchanged only when the stored symlink target text
exactly equals the expected canonical relative target. If an owned symlink
resolves to the correct blob but uses absolute or non-canonical relative target
text, replace it.

## Owned Symlinks

An owned symlink means:

- the symlink is located somewhere under the browse tree; and
- its target path text, interpreted relative to the symlink parent and
  normalized lexically, points somewhere inside the configured CAS store's
  `blobs` tree.

Ownership does not require the target to exist. A dangling symlink is still owned
if its link text lexically resolves inside the configured store's `blobs` tree.

Cleanup and replacement rules:

- Existing correct desired symlinks are left unchanged.
- Existing owned desired symlinks with wrong target text are replaced.
- Existing desired-path symlinks that point outside the configured store's
  `blobs` tree are blockers and fail.
- Existing non-symlink entries at desired output paths are blockers and fail.
- Existing non-symlink entries elsewhere under the browse tree are left alone.
- Existing user-managed symlinks pointing outside the configured store's `blobs`
  tree are left alone unless they block a desired output path.
- Stale cleanup removes only owned symlinks that are not desired by the current
  plan.
- Stale cleanup is not limited to the current `--hash-digits` naming pattern, so
  rebuilding with a different hash length removes older owned symlink names.

## CAS Target Validation

Before creating or replacing any symlink, validate every desired CAS target:

- The blob path derived from `StoreRoot + BlobHash` must exist.
- It must be a regular file.
- Its size must match `blobs.size_bytes`.

Fail clearly on missing or size-mismatched CAS files. Do not create browse-tree
links to missing or corrupt content.

`build-tree` aborts on the first error. There is no rollback. Earlier successful
changes from prior commands remain in place, and if an unexpected error occurs
during the apply phase, earlier changes in the current run may also remain.
Future `audit` or rerunning `build-tree` can reconcile state.

## Planning And Apply Order

Use one planning path for real runs and dry-run.

Order:

1. Validate command paths and options.
2. Open catalog read-only and load desired live materialization entries.
3. Validate catalog relative paths.
4. Generate browse-tree output paths.
5. Detect hash-suffix output collisions.
6. Validate CAS targets exist and match cataloged blob sizes.
7. Walk the browse tree, classify owned symlinks, detect blockers, and compute
   stale owned symlinks.
8. Validate desired parent directories are real directories or creatable.
9. For real runs only, create or update desired symlinks.
10. For real runs only, remove stale owned symlinks.
11. For real runs only, prune empty directories bottom-up under the browse tree.

Apply desired links before stale cleanup so a link that remains desired is not
briefly removed during rebuild.

Dry-run performs steps 1 through 8 and reports what would happen, but performs no
mutation.

## Directory Pruning

After stale owned symlink cleanup, prune empty directories bottom-up under the
browse tree.

Rules:

- Never remove the browse tree root itself.
- Remove only empty directories.
- Leave non-empty directories alone.
- Symlinked directories are blockers during validation and are never traversed as
  directories.
- Dry-run counts directories that would be pruned without removing them.

## Failure Semantics

- Abort on first error.
- Do not roll back prior successful filesystem changes.
- Fail before mutation for known plan errors such as invalid catalog paths,
  suffix collisions, missing CAS blobs, size-mismatched CAS blobs, and blockers.
- Never overwrite non-symlink files.
- Never replace user-managed symlinks pointing outside the configured store's
  `blobs` tree.
- Never delete non-symlink files during stale cleanup.
- Never delete symlinks outside the browse tree.
- Never mutate the catalog.

## Testing Requirements

Use the testing preferences in `RUST-TESTING.instructions.md`.

Milestone 2 acceptance tests should cover:

- CLI help exposes both `import` and `build-tree`.
- Real `build-tree` creates expected relative symlinks into CAS blobs.
- Symlink filenames preserve extensions and append the default 6-character hash
  suffix.
- Extensionless files and dotfiles receive the suffix at the end of the filename.
- `--hash-digits` changes the suffix length.
- Duplicate catalog observations with the same `(relative_path, blob_hash)`
  produce one desired symlink.
- Multiple source roots with the same relative path but different blobs produce
  stable suffixed links.
- Re-running `build-tree` reports existing correct links as unchanged.
- An owned symlink with absolute or non-canonical target text is replaced with
  the canonical relative target.
- Stale owned symlinks are removed.
- Stale owned symlinks from a previous hash-digit length are removed.
- User-managed symlinks pointing outside the store blobs tree are left alone.
- A user-managed symlink at a desired output path fails as a blocker.
- A non-symlink file at a desired output path fails as a blocker.
- A symlinked browse tree root fails.
- Symlinked parent directories below the browse tree fail as blockers.
- Empty directories are pruned after stale cleanup, excluding the browse tree
  root.
- Dry-run creates, replaces, removes, and prunes nothing.
- Dry-run reports the same planned counters using "would" wording.
- Missing browse tree parent fails.
- Missing store root fails.
- Missing store `blobs` directory fails.
- Store/browse-tree overlap is rejected in either direction.
- Explicit `--db` inside the browse tree is rejected.
- Default catalog inside the store is accepted.
- Missing catalog fails clearly.
- Uninitialized catalog with `user_version = 0` fails clearly.
- Newer catalog schema version fails clearly.
- Deleted blobs are not materialized.
- Missing CAS blob files fail clearly.
- CAS blob size mismatch fails clearly.
- Hash-suffix output collision fails before mutation and advises rerunning with
  more hash digits.
- Pure filename suffix helper covers normal extensions, multiple dots, dotfiles,
  extensionless names, Unicode UTF-8 names, and configurable hash lengths.
- Pure relative symlink target helper covers same-directory, parent-directory,
  nested-directory, and sibling-directory cases.

Tests may inspect SQLite directly and may construct tiny catalogs directly when
that is clearer than going through `import`. Keep fixtures small. Do not add
ordinary large-catalog performance tests in milestone 2.

## Future Milestones

These are intentionally deferred:

- `audit`
- `gc`
- metadata-skip optimization
- mount-point workers
- single-writer database thread
- batched catalog writes
- managed WAL checkpoints
- catalog-backed run locking
- progress dashboards
- structured non-TTY logs
- media metadata extraction
- relationship tables
- configurable browse-tree merge policies beyond hash-suffixed filenames
- repair modes for missing or corrupt CAS blobs
- large-catalog performance testing
