# SQLite Instructions

These preferences apply to SQLite usage in this project.

## Access Style

- Use raw SQL through `rusqlite`.
- Do not use ORMs.
- Keep schema SQL deterministic, visible, and reviewable.
- Keep application SQL in colocated `.sql` files and load it with
  `include_str!`. Inline SQL in Rust should be limited to trivial one-liners
  where a separate file would make the code harder to read.
- Keep SQLite connections hidden behind a catalog module. Callers should not
  choreograph transactions or manipulate raw connections.

## Schema Versions And Migrations

- Use `PRAGMA user_version` for schema migration tracking.
- Implement a tiny migration runner from the first schema.
- On writable open:
  - read `PRAGMA user_version`
  - if `0`, create schema version 1 inside a transaction and set
    `user_version = 1`
  - if current, proceed
  - if newer than the application supports, fail clearly
- Do not support down migrations.
- Do not add a migration framework until complexity justifies it.

## PRAGMAs

- Set behavior PRAGMAs every time a writable catalog connection is opened:
  - `PRAGMA foreign_keys = ON;`
  - `PRAGMA journal_mode = WAL;`
  - `PRAGMA synchronous = NORMAL;`
- `foreign_keys` is per connection.
- `journal_mode=WAL` persists but should still be requested on open.
- `synchronous=NORMAL` should be set for writable connections.
- Dry-run read-only opens must not attempt mutating PRAGMAs or migrations.

## Timestamp Storage

- Persist machine timestamps as integer Unix epoch milliseconds.
- Prefer columns named with `_ms` suffix, such as `created_at_ms`.
- Store source filesystem modified time as nullable advisory metadata.
- Do not store RFC3339 text for machine timestamps unless there is a specific
  user-facing reason.

## Constraints And Upserts

- Use explicit constraints to encode durable identity and idempotency.
- Prefer `UNIQUE` constraints plus upserts for repeated observations.
- SQLite constraints are backstops. Rust newtypes and constructors enforce the
  real invariants.
- Enforce at least `CHECK(length(hash) = 64)` for BLAKE3 hash text.
- Enforce minimal path checks such as non-empty source root and non-empty
  relative path that is not absolute.
- Do not try to encode all path safety in SQL.

## Dry Run Catalog Access

- If the database path does not exist during dry-run, treat the catalog as empty.
- If it exists, open read-only if possible.
- Do not create database files, WAL files, SHM files, or parent directories.
- Do not run migrations in dry-run.
- If dry-run needs an isolated catalog snapshot, use SQLite's backup API to copy
  into a temp or in-memory database before inspection. Do not copy
  `catalog.sqlite`, `-wal`, and `-shm` files independently; that is not a safe
  snapshot without filesystem-level atomicity.

### Same-Store Coordinated Read Snapshots

As a narrow exception to the preceding guidance, a shared reader that already
holds the application's same-store advisory lock may copy the main catalog
database and its WAL, if non-empty, into a private temporary directory and
open only that copy. Never copy the SHM sidecar. SQLite may create SHM beside
the temporary copy, but the source catalog, WAL, and SHM must remain untouched.

This exception is safe only against cooperating commands using the same store
lock. External SQLite writers or filesystem editors are unsupported and must
not be treated as safe snapshot participants.
