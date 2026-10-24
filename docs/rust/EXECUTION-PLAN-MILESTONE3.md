# Milestone 3 Execution Plan: Audit Catalog And CAS Integrity

This plan guides a coding agent through implementing
`IMPLEMENTATION-MILESTONE3.md`. Treat that milestone file, `SPEC.md`, and the
colocated Rust, architecture, testing, and SQLite instructions as the product
contract. This file resolves implementation choices, sequences the work, and
defines stable finding and counter semantics.

If this plan conflicts with the milestone contract, the milestone contract wins.
Do not use `docs/old_python/` as a behavior oracle.

### SQLite Integrity Results

- The single `PRAGMA integrity_check` row `ok` means clean and is not a finding.
- Every other `integrity_check` row is a `CATALOG_INTEGRITY` finding.
- Every `PRAGMA foreign_key_check` row is a `CATALOG_INTEGRITY` finding.
- Use `PRAGMA integrity_check` without a reduced error limit. SQLite's default
  maximum is accepted for this milestone; do not claim the result is an
  unbounded list of every possible corruption message.

This interpretation is required because SQLite returns `ok` as a row for a
healthy database. See the official SQLite
[`integrity_check` documentation](https://www.sqlite.org/pragma.html#pragma_integrity_check).

### Read-Only SQLite And WAL

Use a hybrid read-only open policy. `immutable=1` is not sufficient for every
catalog: SQLite can ignore committed changes that exist only in a non-empty WAL
when the main database is opened as immutable.

1. Inspect `<db>-wal` and `<db>-shm` with `symlink_metadata` before opening.
2. If no non-empty WAL exists, open the main database through a correctly
   encoded URI with `mode=ro&immutable=1` and SQLite URI/read-only flags. The
   quiescence precondition makes the immutable assertion valid and prevents
   sidecar creation.
3. If a non-empty WAL exists, require both WAL and SHM sidecars to already exist
   as real readable regular files, then open with `mode=ro` without
   `immutable=1`. This lets SQLite read committed WAL frames without creating a
   missing sidecar.
4. If a non-empty WAL exists without a usable SHM, return an operational error.
   Do not ignore the WAL, copy the files, create SHM, or checkpoint. Tell the
   user that the catalog must be cleanly closed/checkpointed by a writer before
   retrying.

Requirements:

- Percent-encode the filesystem path correctly when constructing the SQLite
  URI; do not interpolate an arbitrary path into a URI without encoding it.
- Preserve and read an existing non-empty WAL as part of the database state.
  Never copy, remove, truncate, or checkpoint it.
- Treat failure to open a valid WAL-backed snapshot read-only as an operational
  error. Do not fall back to writable access or to a database-only file copy.
- Document in code that `immutable=1` is safe only when there is no non-empty
  WAL and because audit requires no concurrent store or catalog mutation.
- Ordinary read-only WAL access may update coordination fields in an existing
  SHM file. Treat SHM as an existing SQLite sidecar, not catalog content. Audit
  must not create it, and must not change main DB, WAL, blob, or store metadata.
- Add a test proving that audit creates no `-wal` or `-shm` files when they are
  absent, a test that committed existing WAL content is observed, and a test
  that non-empty WAL without usable SHM fails rather than auditing stale data.

SQLite documents that a read-only WAL database requires readable WAL/SHM,
permission to create them, or the immutable URI parameter. See
[`Read-Only Databases`](https://www.sqlite.org/wal.html#readonly).

### Catalog Schema Validation

`PRAGMA user_version = 1` is necessary but not sufficient to establish schema
version 1. Validate the semantic schema shape before trusting row queries:

- required tables: `blobs`, `source_files`;
- required columns in their schema-v1 order, with expected declared types,
  nullability, and primary-key roles;
- the `blobs.hash` primary key;
- the `source_files(source_root, relative_path)` uniqueness constraint;
- the `source_files.blob_hash -> blobs.hash` foreign key;
- the `idx_source_files_blob_hash` index over `blob_hash`;
- the schema-v1 `CHECK` constraints on hash length, source-root emptiness, and
  minimally relative source paths;
- no missing required constraints or incompatible replacement objects.

Use SQLite schema/PRAGMA inspection queries in colocated `.sql` files. Compare
semantic properties rather than whole raw `CREATE TABLE` strings, because
formatting is not schema identity. Inspect normalized `sqlite_schema.sql`
fragments only for `CHECK` constraints that SQLite does not expose through a
dedicated PRAGMA. Extra user-defined tables or indexes are a
`CATALOG_INTEGRITY reason=schema_mismatch` finding, but they do not by
themselves prevent required row enumeration. Missing or incompatible required
objects are findings first, then an operational error if the required audit
queries cannot safely run.

Do not migrate or repair a schema during audit.

### Malformed SQLite Values

Decode audit rows through `rusqlite::types::ValueRef` or an equivalent raw-value
representation. SQLite schema version 1 is not `STRICT`, so a damaged database
can store unexpected storage classes.

- A wrong storage class or out-of-range value is an invalid-row finding, not an
  immediate query failure.
- Preserve the table name, SQLite row ID where available, field name, and a
  bounded escaped representation of the raw value.
- Bound raw BLOB/text rendering so a malformed row cannot force huge output or
  memory use. Include the original byte length when truncating.
- A SQLite engine error that prevents stepping to subsequent rows is an
  operational failure because enumeration did not complete.

Validate the milestone-listed domain fields. Also require timestamp columns to
be SQLite integers when non-null, but do not impose new positivity or ordering
rules beyond `first_seen_at_ms <= last_seen_at_ms`. Any non-null
`deleted_at_ms` with the correct integer storage class marks one GC candidate.

### Finding Categories

Use exactly these stable output categories:

- `BLOB_IO_ERROR`
- `CAS_IO_ERROR`
- `CATALOG_INTEGRITY`
- `HASH_MISMATCH`
- `INVALID_BLOB_ROW`
- `INVALID_CAS_ENTRY`
- `INVALID_SOURCE_ROW`
- `MISSING_BLOB`
- `NON_REGULAR_BLOB`
- `ORPHAN_BLOB`
- `SIZE_MISMATCH`

Represent them as typed `AuditFinding` variants. Reason values should be stable
snake-case tokens; optional human context may follow them. Do not expose raw OS
error prose as a sorting key.

Do not deduplicate distinct evidence. Deduplicate only findings with the same
category, stable identity, reason, and structured details. In particular:

- a foreign-key violation and an invalid source row are distinct findings;
- an orphan with bad content produces both `ORPHAN_BLOB` and `HASH_MISMATCH`;
- a cataloged non-regular object at a canonical blob path produces both
  `INVALID_CAS_ENTRY` and `NON_REGULAR_BLOB`;
- a malformed shard plus an absent expected catalog path produces
  `INVALID_CAS_ENTRY` and `MISSING_BLOB`.

### Finding Order And Path Rendering

Sort findings by:

1. category text;
2. stable identity bytes;
3. reason token;
4. remaining structured fields.

Use store-relative paths when the entry is inside the store. On Linux and
macOS, sort filesystem names by their raw `OsStr` bytes. Render non-UTF-8 and
control bytes with deterministic `\xNN` escaping; do not use lossy conversion as
the stable identity. Escape spaces and ordinary printable UTF-8 only where
needed to keep one finding per output line.

### CAS Traversal Cardinality

Emit one `INVALID_CAS_ENTRY` for every invalid filesystem entry encountered.
When an invalid entry is a real directory, report that directory and continue
through it in lexical order. Every descendant under an already malformed path
is also invalid and gets its own finding. Never traverse a symlinked directory.

Only a regular file at exactly
`blobs/<two-lower-hex>/<two-lower-hex>/<matching-64-lower-hex>` is a valid CAS
blob. Invalid regular files are neither counted as CAS blob files nor hashed nor
reported as orphans.

### Local I/O Failures

- Failure to enumerate the blobs root is an operational error.
- Failure to enumerate or stat an entry below the root is a `CAS_IO_ERROR`
  finding; continue with reachable siblings.
- Failure to stat, safely open, or fully read one valid blob is a
  `BLOB_IO_ERROR` finding; continue with other blobs.
- A file that vanishes before reconciliation is a local finding, not an
  operational failure.

A failed or partial hash does not increment `blobs_hashed` and must not produce
a hash mismatch from incomplete data.

### Resource Use

Metadata memory may be `O(catalog rows + valid CAS files + findings)`. Blob
content memory must be `O(chunk size)` and one file must be hashed at a time.

- Use `try_reserve`/`try_reserve_exact` before potentially large collection and
  hash-buffer growth, and convert allocation failures into operational errors.
- Keep `--chunk-size` as any non-zero `usize`, as required by the milestone; do
  not invent an undocumented upper bound.
- Never read an entire blob or malformed SQLite value into a presentation
  string.
- Use checked counter increments and return an operational error on overflow.

### Filesystem Race Boundary

The quiescence requirement is a correctness precondition, not an enforced lock.
Still avoid following a final-component symlink during hashing:

- classify with `symlink_metadata`;
- on Unix, open the final blob with `O_NOFOLLOW` through a small platform helper;
- verify metadata from the opened file is regular;
- hash the already-open file, not the path;
- compare the streamed byte count with both catalog size and the opened-file
  metadata length.

Full directory-descriptor-relative traversal is deferred. Document that parent
directory replacement by an external actor remains outside milestone 3 and is
covered only by the quiescence precondition.

## Target Architecture

Keep one deep public operation:

```rust
pub fn audit_store(config: AuditConfig) -> color_eyre::Result<AuditReport>;
```

Recommended ownership:

- `cli`: parse `audit`, convert options to validated config, render findings and
  summary, map command outcome to process exit status.
- `config`: own `AuditOptions -> AuditConfig` conversion.
- `paths`: own command-neutral existing-store validation, `BlobHash`, CAS path
  construction, raw relative-path display identities, and pure layout
  classification.
- `catalog`: own hybrid read-only open, transaction scope, schema checks,
  integrity PRAGMAs, raw row decoding, domain validation, and SQL.
- `hashing`: own bounded allocation and streaming BLAKE3 over an already-open
  file.
- `store`: optionally own a narrow read-only CAS walker and safe-open helper.
- `audit`: reconcile catalog and CAS indexes, schedule each valid blob once for
  hashing, accumulate findings, and construct the report.

Do not expose `rusqlite::Connection` outside `catalog`. Do not let `catalog`
inspect CAS paths. Do not let CLI code choreograph SQL or traversal.

Suggested internal catalog result:

```rust
pub struct CatalogAuditSnapshot {
    pub blob_rows_seen: u64,
    pub gc_candidates: u64,
    pub valid_blobs: BTreeMap<BlobHash, CatalogBlob>,
    pub findings: Vec<AuditFinding>,
}
```

Keep invalid rows out of the typed reconciliation index while still counting
them in `blob_rows_seen`. Source rows do not need to leave the catalog boundary;
their findings can be returned with the snapshot.

Use `BTreeMap` or explicitly sort hash-map output before report construction.
Add `Hash`/`Ord` derives to domain types only where their semantics support it.

## Sequenced Implementation

### 1. Add CLI, Config, And Exit Outcomes

Add:

```text
media-importer audit \
  --store <STORE_ROOT> \
  [--db <DB_PATH>] \
  [--chunk-size <BYTES>]
```

- Expose `import`, `build-tree`, and `audit`; do not expose `gc`.
- Reuse `DEFAULT_CHUNK_SIZE` and Clap's `NonZeroUsize` parsing.
- Rename `StoreRoot::validate_existing_for_build_tree` to a command-neutral
  existing-store validator and use it for both read-oriented commands.
- Require store and blobs roots to be real directories, not symlinks.
- Require the selected DB to exist as a regular file. Reject a DB path whose
  final component is a symlink; this makes “regular file” consistent with the
  store-root safety rule.
- Allow explicit DB paths inside or outside the store.
- Return a small CLI outcome enum or `ExitCode` from dispatch: success `0`,
  operational/configuration error `1`, findings `2`.
- Keep `color_eyre` installation and error rendering in the binary. Do not call
  `process::exit` in library or renderer code.

Add regression tests for import/build-tree exit `0` and error exit `1` before
changing the entrypoint.

### 2. Define Report And Finding Types

Define all variants and stable fields before writing traversal logic. Each
finding must provide:

- category;
- stable identity key;
- optional relative path;
- structured expected/actual/reason fields;
- deterministic comparison independent of OS error display text.

Define counters exactly:

- `catalog_blobs`: every `blobs` row stepped successfully, valid or invalid;
- `cas_blob_files`: valid canonical regular CAS files discovered;
- `blobs_hashed`: valid CAS files whose complete stream was hashed once;
- `gc_candidates`: blob rows with a valid non-null integer `deleted_at_ms`;
- `findings`: final deduplicated finding count.

`AuditReport::is_clean()` is exactly `findings.is_empty()`.

### 3. Implement Purpose-Specific Catalog Audit Access

Do not add audit behavior to materialization's public API. Introduce a
purpose-specific catalog operation that owns the entire snapshot lifetime.

Recommended shape:

```rust
pub fn inspect_catalog_for_audit(path: &Path) -> Result<CatalogAuditSnapshot>;
```

Inside that call:

1. Validate the final path with `symlink_metadata`.
2. Inspect WAL/SHM and open through the hybrid read-only policy above.
3. Require `user_version == 1`.
4. Begin one deferred read transaction. Keep the `Transaction` as a local
   variable so it borrows the local connection; do not build a self-referential
   catalog struct.
5. Run and collect `integrity_check`, excluding only the exact clean `ok` row.
6. Run and collect `foreign_key_check` with table, rowid, parent, and FK index.
7. Inspect semantic schema v1.
8. Enumerate blob rows in deterministic order using raw values.
9. Enumerate source rows in deterministic row-ID order using raw values.
10. Commit or explicitly finish the read transaction and close the connection.

Put non-trivial SQL in files such as:

- `catalog/sql/audit_blobs.sql`
- `catalog/sql/audit_source_files.sql`
- `catalog/sql/audit_schema_objects.sql`
- `catalog/sql/integrity_check.sql`
- `catalog/sql/foreign_key_check.sql`

Blob validation:

- hash is TEXT and a valid `BlobHash`;
- size is INTEGER and non-negative;
- created timestamp is INTEGER;
- deleted timestamp is NULL or INTEGER.

Source validation:

- preserve and validate integer row ID;
- `source_root` is non-empty TEXT;
- `relative_path` is TEXT accepted by `SourceRelativePath`;
- `blob_hash` is valid TEXT and references a valid loaded blob;
- size is a non-negative INTEGER and equals referenced catalog blob size;
- modified timestamp is NULL or INTEGER;
- first/last timestamps are INTEGER and ordered;
- seen count is a positive INTEGER.

One row may report multiple field-level reasons, but emit one
`INVALID_BLOB_ROW` or `INVALID_SOURCE_ROW` per row with sorted reason tokens.
This prevents noisy output while retaining all invalid fields.

### 4. Implement Pure CAS Classification

Create a pure classifier that receives relative path components plus entry
kind, and returns either a validated canonical `BlobHash` or a stable invalid
reason.

Cover:

- root-level files;
- first and second shard names;
- uppercase, short, long, and non-hex names;
- shard/filename prefix mismatch;
- files at shallow depth;
- directories at blob depth and below;
- over-deep entries;
- symlinks and other non-regular types at every depth;
- non-UTF-8 names;
- empty valid shard directories.

The classifier must not access the filesystem and must not build user-facing
lines.

### 5. Build The Read-Only CAS Walker

Walk from the already validated blobs root without following symlinks.

For each directory:

1. Collect entries with raw names.
2. Sort by raw name bytes.
3. Classify using `symlink_metadata` or reliable entry type information.
4. Record valid canonical regular files by `BlobHash`.
5. Record invalid entries and local I/O findings.
6. Recurse only into real directories, including malformed ones.

Do not hash during traversal. Finish classification first so reconciliation and
hash scheduling are deterministic.

### 6. Reconcile And Hash

Use one deterministic hash-key-ordered pass across the union of valid catalog
hashes and valid CAS hashes.

- Catalog only: inspect the expected path and report `MISSING_BLOB` or
  `NON_REGULAR_BLOB` as appropriate.
- CAS only: report `ORPHAN_BLOB`, then hash it.
- Both: compare opened-file length with catalog size, report any mismatch, and
  hash regardless of that mismatch.
- Deleted catalog blobs follow the same reconciliation and hashing path as live
  blobs and are never orphans.

Hash each valid CAS hash at most once. Refactor hashing so the audit passes an
already safely opened file and receives both BLAKE3 and streamed byte count.
After a successful complete stream:

- increment `blobs_hashed`;
- compare streamed length with opened metadata and catalog size;
- compare actual hash with the filename/catalog identity;
- emit at most one `SIZE_MISMATCH` and one `HASH_MISMATCH` per blob identity.

If the file changes or disappears between traversal and open, emit the relevant
I/O/non-regular finding and continue. Do not retry under milestone 3.

### 7. Render Stable Output

Sort and deduplicate structured findings before rendering. Print all findings,
then exactly one blank line, then the summary.

Dirty example:

```text
HASH_MISMATCH blobs/89/ab/89ab... expected=89ab... actual=ffff...
ORPHAN_BLOB blobs/cd/ef/cdef... hash=cdef...

Audit complete
Catalog blobs: 4
CAS blob files: 4
Blobs hashed: 3
GC candidates: 1
Findings: 2
```

Clean example:

```text
Audit clean
Catalog blobs: 4
CAS blob files: 4
Blobs hashed: 4
GC candidates: 0
Findings: 0
```

All findings and summaries go to stdout. Configuration and operational errors
remain `color_eyre` reports on stderr. Tracing also writes to stderr.

### 8. Update User Documentation

Update the root `README.md`, which is currently stale at milestone 1. Document:

- all currently exposed commands;
- audit invocation and exit statuses `0`, `1`, and `2`;
- full rehashing and potentially high I/O cost;
- the quiescent-store requirement;
- deleted blobs as expected CAS files and GC candidates;
- report-only behavior with no repair or deletion.

Do not manually edit generated or historical architecture material unless the
repository explicitly identifies it as current user documentation.

### 9. Add Tests In Risk Order

Prefer a new `tests/audit_milestone.rs` plus focused unit tests for pure path
classification. Reuse fixture helpers where practical without coupling tests to
private implementation choreography.

Implement in this order:

1. Entry-point regression tests for existing command exit behavior.
2. Pure CAS classification, including non-UTF-8 names on Unix.
3. Clean imported store, counters, output, and exit `0`.
4. Finding output and exit `2`; operational failure and exit `1`.
5. Missing/default/explicit DB and store path validation, including symlinks.
6. No-created-file and unchanged bytes/permissions/mtime assertions.
7. Healthy `integrity_check` proves the `ok` row is ignored.
8. Foreign-key violations and safely constructible integrity failures.
9. Schema-v1 impostors: missing table, wrong column, wrong FK, missing index,
   and extra incompatible object.
10. Wrong SQLite storage classes and oversized raw malformed values continue to
    later rows.
11. Every listed blob/source domain invariant.
12. Missing, non-regular, size-mismatched, and hash-mismatched catalog blobs.
13. Deleted blob counter and verification semantics.
14. Orphans, including orphan plus hash mismatch and single-hash counting.
15. Every malformed CAS depth/type/shard case and empty valid shards.
16. Nested traversal errors and unreadable blobs where OS permissions permit.
17. Deterministic multi-finding ordering and deduplication.
18. Small multi-chunk hashing, zero chunk rejection, and allocation-failure
    behavior where it can be tested without destabilizing the test process.
19. Existing WAL state, missing-sidecar failure, and absence of WAL/SHM
    creation.

For immutability tests, snapshot the names and relevant metadata under the
store, DB, and sidecar paths before audit and compare afterward. Do not rely
only on checking the main database mtime.

## Operational Error Matrix

Return exit `1` without an audit summary when:

- CLI or path validation fails;
- the catalog/store/blobs root is missing or has the wrong root type;
- catalog open, schema-version read, or read transaction start fails;
- required schema/queries cannot be safely enumerated;
- SQLite stepping fails before all required rows are read;
- the blobs root cannot be enumerated;
- checked allocation or counter arithmetic fails.

Return a complete report and exit `2` when baseline enumeration succeeds but
one or more typed findings exist. Local nested filesystem failures remain
findings even though the affected subtree or file could not be fully inspected;
the finding must state that limitation.

## Review Checklist

Before considering milestone 3 complete:

- Audit performs no durable writes, migrations, backups, cleanup, chmod, or
  locking.
- SQLite `ok` is treated as clean.
- Schema version and semantic schema shape are both checked.
- Malformed SQLite values do not panic or prematurely abort valid later rows.
- Traversal never follows symlinks and hashes only canonical regular files.
- Every valid CAS file is hashed no more than once.
- Streamed byte count participates in size validation.
- Invalid CAS files are not counted or orphaned.
- Deleted blobs remain expected and verified.
- Finding categories, reasons, ordering, escaping, and counters are stable.
- Operational errors use exit `1`; findings use exit `2`.
- Existing commands retain their exit behavior.
- README documents cost, quiescence, deletion semantics, and no-repair behavior.

## Verification Commands

Run from the project root inside `nix develop` or the active direnv shell:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
prek run --all-files
```

The implementation is complete only when all commands pass and the acceptance
tests in `IMPLEMENTATION-MILESTONE3.md` are represented or deliberately covered
by an equivalent higher-level test.
