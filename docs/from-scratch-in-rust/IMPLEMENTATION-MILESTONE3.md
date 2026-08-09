# Rust Implementation Guide: Milestone 3

This guide records implementation decisions for milestone 3 of the Rust rewrite
of `media-importer`. The source of product intent is `SPEC.md`; this file
captures the concrete choices a coding agent should follow while implementing
the next vertical slice.

Milestone 1 implemented directory import into the content-addressed store (CAS)
and catalog. Milestone 2 added browse-tree materialization. Milestone 3 adds a
read-only integrity audit of the authoritative catalog and CAS state.

The existing Python implementation is deprecated historical context. Do not port
it file-by-file and do not use it as a behavior oracle.

## Companion Instructions

Use these colocated instruction files when implementing this guide:

- `RUST.instructions.md`
- `RUST-ARCHITECTURE.instructions.md`
- `RUST-TESTING.instructions.md`
- `SQLITE.instructions.md`

## Milestone 3: Audit Catalog And CAS Integrity

Implement one end-to-end vertical slice:

```text
media-importer audit \
  --store <STORE_ROOT> \
  [--db <DB_PATH>] \
  [--chunk-size <BYTES>]
```

Expose `audit` in addition to the existing `import` and `build-tree` commands.
Do not expose a placeholder `gc` command.

`audit` verifies the authoritative catalog and CAS in both directions:

- every cataloged blob must have the expected regular CAS file;
- every well-formed CAS blob file must have a catalog row;
- cataloged size and BLAKE3 identity must match the file's content; and
- SQLite structure, foreign keys, and catalog domain values must be valid.

Audit reports integrity findings but never repairs, deletes, creates, or updates
store state.

## CLI Contract

- `--store` is required.
- `--store` must already exist as a real directory, not a symlink.
- `<STORE_ROOT>/blobs` must already exist as a real directory, not a symlink.
- `audit` must not create store directories.
- `--db` is optional and defaults to `<STORE_ROOT>/catalog.sqlite`.
- The catalog must exist and have schema version 1.
- Explicit `--db` is allowed inside or outside the store root.
- `--chunk-size` is optional, defaults to the existing
  `DEFAULT_CHUNK_SIZE`, and must be non-zero.
- Do not expose `--dry-run`; audit is intrinsically read-only.
- Do not expose `--quick`, `--repair`, `--delete`, `--json`, or `--workers` in
  milestone 3.

Use `clap` derive for parsing. Keep CLI argument structs separate from validated
application config.

Suggested validated config shape:

```rust
pub struct AuditConfig {
    pub store_root: StoreRoot,
    pub db_path: PathBuf,
    pub chunk_size: NonZeroUsize,
}
```

Reuse the existing store and database path validation rules where they express
the same contract. Give the existing build-tree-specific store validator a
command-neutral name if both commands use it.

## Process Exit Status

Exit status is part of the milestone 3 CLI contract:

- `0`: audit completed and found no integrity problems;
- `1`: invalid configuration or an operational error prevented audit from
  completing; and
- `2`: audit completed but found one or more integrity problems.

Integrity findings are an expected command outcome, not `color_eyre` errors.
Return them in an audit report, render the full report, and then select exit
status 2 when the report is not clean.

Refactor the binary entrypoint as needed so it can map successful command
outcomes to explicit `std::process::ExitCode` values while still rendering
`color_eyre` reports for operational failures. Existing successful `import` and
`build-tree` behavior remains exit status 0, and their errors remain exit status
1.

Do not call `std::process::exit` from library, audit, or rendering code.

## Output Contract

Print deterministic, human-readable findings followed by a concise summary to
stdout. Reserve stderr for tracing and operational/configuration errors.

Each finding line should include a stable category and enough identity to locate
the problem. Exact punctuation may follow existing CLI style, but category names
and ordering must be testable. Suggested examples:

```text
MISSING_BLOB 0123...cdef blobs/01/23/0123...cdef
SIZE_MISMATCH 4567...abcd expected=42 actual=41
HASH_MISMATCH 89ab...0123 actual=ffff...eeee
ORPHAN_BLOB cdef...4567 blobs/cd/ef/cdef...4567
INVALID_CAS_ENTRY blobs/zz reason=invalid first-level shard
CATALOG_INTEGRITY reason=foreign key violation table=source_files rowid=7

Audit complete
Catalog blobs: 4
CAS blob files: 4
Blobs hashed: 3
GC candidates: 1
Findings: 6
```

Requirements:

- Sort findings deterministically by category, then stable path or catalog
  identity.
- Print all inspectable findings, not only the first one.
- Print `Audit clean` or an equivalently unambiguous clean heading when there
  are no findings.
- Include counters for catalog blobs, CAS blob files, blobs hashed, blobs marked
  deleted, and total findings.
- A blob marked deleted is counted as a GC candidate, not an integrity finding.
- Use paths relative to the store where practical so output is stable across
  machines and test directories.
- Do not print one progress line per healthy blob.
- Use `tracing` for internal diagnostics.
- Defer progress bars and structured non-TTY output.

The exact report fields may differ, but keep findings structured until the CLI
renderer. Do not build user-facing output strings inside catalog or filesystem
inspection code.

## Module Boundaries

Add an `audit` module with one deep operation:

```rust
pub fn audit_store(config: AuditConfig) -> color_eyre::Result<AuditReport>;
```

Suggested domain shape:

```rust
pub struct AuditReport {
    pub catalog_blobs: u64,
    pub cas_blob_files: u64,
    pub blobs_hashed: u64,
    pub gc_candidates: u64,
    pub findings: Vec<AuditFinding>,
}

impl AuditReport {
    pub fn is_clean(&self) -> bool;
}
```

Use typed finding variants rather than a bag of preformatted strings. Expected
variants include:

- catalog integrity failure;
- invalid catalog blob row;
- invalid catalog source row;
- missing blob;
- non-regular blob at an expected path;
- size mismatch;
- hash mismatch;
- orphan blob; and
- invalid CAS entry.

Preserve these boundaries:

- CLI parses arguments, validates command-level input, dispatches, renders
  findings and summaries, and selects the process exit status.
- `paths` owns typed hash validation, CAS path construction, relative display
  paths, and pure CAS-layout classification helpers.
- `catalog` owns read-only SQLite access, schema-version checks, integrity
  PRAGMAs, row decoding, and raw SQL.
- `hashing` owns bounded-memory BLAKE3 streaming.
- `audit` orchestrates catalog checks, CAS traversal, reconciliation, hashing,
  finding collection, and report construction.
- `store` may expose read-only filesystem inspection behind a narrow interface
  if doing so reduces duplication without mixing audit policy into import code.

Do not let CLI code choreograph SQL queries or filesystem traversal. Do not let
the catalog module inspect CAS paths.

## Read-Only And Quiescence Contract

Audit must not mutate durable application state:

- Open the catalog read-only.
- Do not run migrations or mutating PRAGMAs.
- Do not create the database, WAL, SHM, store, blobs, or staging paths.
- Do not purge staging.
- Do not chmod blobs.
- Do not acquire a catalog run lock in milestone 3.

The store must be quiescent while audit runs. The user must not run `import`, a
future `gc`, or another store-mutating process concurrently against the same
store and catalog.

SQLite can provide a consistent database read transaction, but the database and
CAS filesystem cannot be snapshotted atomically by this command. A concurrent
import can therefore appear temporarily as an orphan CAS file or a missing
cataloged file. Document this precondition in CLI help and user-facing docs.
Run-lock enforcement is provided by milestone 5's same-store advisory
directory lock.

## Catalog Access And Checks

Milestone 3 makes no schema changes. Use schema version 1.

Add a purpose-specific read-only catalog API. It may share internal connection
setup with materialization, but its public methods should describe audit
behavior rather than expose `rusqlite::Connection`.

On open:

- Fail operationally if the catalog path is missing or not a regular file.
- Open with `SQLITE_OPEN_READ_ONLY`.
- Fail operationally if `PRAGMA user_version` is 0, newer than 1, or otherwise
  not exactly 1.
- Start a read transaction so all catalog checks and row reads use one snapshot.
- Do not use the dry-run backup path; audit should inspect the configured
  catalog itself.

Run both SQLite checks:

- `PRAGMA integrity_check`; and
- `PRAGMA foreign_key_check`.

Treat rows returned by either check as integrity findings and continue to domain
row loading when SQLite still permits safe queries. If corruption prevents a
check, transaction, or required query from completing, return an operational
error because the audit could not complete.

Keep non-trivial SQL in colocated `.sql` files loaded with `include_str!`.
Catalog queries must have deterministic `ORDER BY` clauses.

Load every `blobs` row, including rows with non-null `deleted_at_ms`. Decode and
validate:

- hash is exactly 64 lowercase hexadecimal characters;
- `size_bytes` is non-negative; and
- duplicate hash identity is impossible under the schema, but any SQLite
  integrity report remains a finding.

Load the source-file fields needed to validate durable milestone 1 invariants:

- `source_root` is non-empty;
- `relative_path` passes `SourceRelativePath::from_catalog_text`;
- `blob_hash` passes `BlobHash` validation;
- `size_bytes` is non-negative and equals the referenced blob's cataloged size;
- `seen_count` is positive; and
- `first_seen_at_ms` is not later than `last_seen_at_ms`.

Invalid domain rows are findings. Do not abort merely because one row cannot be
converted into a domain newtype. Preserve its row ID or raw identity in the
finding and continue where possible.

Foreign-key violations may cause the same source row to produce both a
`CATALOG_INTEGRITY` finding and a domain-row finding. Prefer deduplicating exact
equivalents in the report, but do not suppress distinct evidence merely to keep
the count small.

## Strict CAS Layout

The only valid blob-file layout is:

```text
<STORE_ROOT>/blobs/<hash[0..2]>/<hash[2..4]>/<full_hash>
```

Walk `<STORE_ROOT>/blobs` without following symlinks. Traverse in deterministic
lexical order and use `symlink_metadata` or entry file types so symlinks are
classified without dereferencing them.

Valid layout rules:

- The first-level shard is exactly two lowercase hexadecimal characters.
- The second-level shard is exactly two lowercase hexadecimal characters.
- The filename is exactly 64 lowercase hexadecimal characters.
- Both shard names equal the corresponding prefix of the filename.
- A valid blob entry is a regular file at exactly that depth.
- Empty valid shard directories are allowed and are not findings.

Report `INVALID_CAS_ENTRY` for:

- files at the blobs root or either shard-directory level;
- malformed or mismatched shard names;
- malformed blob filenames;
- directories below the blob-file depth;
- symlinks at any depth, including symlinks to regular files or directories;
- sockets, FIFOs, devices, or other non-regular entries; and
- regular files below malformed directory paths.

Never follow a symlink and never inspect content outside the real blobs
directory. A malformed subtree may be traversed only when each path component is
a real directory; report its entries without interpreting them as valid blobs.

Only regular files at a valid canonical CAS path count toward `cas_blob_files`
or participate in catalog reconciliation. Invalid regular files remain findings
but are not also reported as orphan blobs.

## Bidirectional Reconciliation

Build typed, bounded metadata indexes for catalog rows and valid CAS entries,
keyed by `BlobHash`. Catalog and CAS identities may be held in memory for this
milestone; blob contents must never be buffered wholesale.

For every catalog blob row:

1. Derive the one expected path with `StoreRoot::blob_path`.
2. If no valid CAS regular file exists there, report `MISSING_BLOB` or
   `NON_REGULAR_BLOB` as appropriate.
3. If the file exists, compare filesystem length with catalog `size_bytes`.
4. If the size differs, report `SIZE_MISMATCH`.
5. Regardless of the size comparison, stream the whole valid regular file
   through BLAKE3 using `--chunk-size`.
6. If the computed hash differs from the catalog hash and filename, report
   `HASH_MISMATCH` with expected and actual hashes.

For every valid canonical CAS regular file with no catalog row, report
`ORPHAN_BLOB`. Hash orphan files as well and report `HASH_MISMATCH` if their
content does not match their filename. This distinguishes an unindexed but
internally valid CAS object from a misnamed or corrupt one.

Hash each valid CAS file at most once even when reconciling both directions.
`blobs_hashed` counts files whose full stream completed successfully.

Use sequential bounded-memory hashing in milestone 3. Reuse the existing
`hash_file` behavior, improving its context strings so it is not source-file
specific if necessary. Do not add a worker pool or parallel traversal.

## Deleted Blob Semantics

`blobs.deleted_at_ms IS NOT NULL` marks logical deletion. It does not mean the
CAS file should already be absent.

For a deleted catalog blob:

- count it as one GC candidate;
- require and verify its CAS file exactly like a live blob;
- report missing, malformed, size-mismatched, or hash-mismatched content as an
  integrity finding; and
- do not classify its valid CAS file as orphaned.

Audit does not infer deletion from source-file references and does not change
`deleted_at_ms`. Physical removal belongs to the future mark-and-sweep `gc`
milestone.

## Finding Versus Operational Error

An integrity finding means audit successfully inspected a relevant object and
found store state that violates the contract. Findings are accumulated and lead
to exit status 2.

Continue after item-local problems such as:

- missing, malformed, symlinked, or non-regular CAS entries;
- size or hash mismatches;
- orphan blobs;
- an individual blob that cannot be opened, read, or statted; and
- invalid decodable catalog domain values.

Represent an unreadable or concurrently vanished blob as a finding with its I/O
context, then continue with other entries. The quiescence precondition means
such races do not need retry logic in this milestone.

Return an operational error and exit status 1 when audit cannot establish or
traverse the required baseline, including:

- invalid CLI paths or options;
- missing catalog, store root, or blobs root;
- unsupported or uninitialized schema version;
- failure to open the catalog read-only or start its read transaction;
- SQLite corruption severe enough that required checks or row enumeration
  cannot complete;
- failure to enumerate the blobs root; or
- failure to allocate or maintain the audit's required in-memory indexes.

Do not panic on malformed disk or catalog state.

## Execution Order

Use one deterministic audit path:

1. Validate command paths and options.
2. Open the catalog read-only and begin one read snapshot.
3. Run SQLite integrity and foreign-key checks.
4. Load and domain-validate all blob and required source-file rows.
5. Walk and classify the full CAS blobs tree without following symlinks.
6. Reconcile catalog rows and valid CAS entries in both directions.
7. Stream and hash every valid CAS blob file once.
8. Sort findings deterministically and construct the report.
9. Render findings and summary.
10. Exit 0 when clean or 2 when findings exist.

The operation performs no apply phase and has no rollback behavior because it
mutates nothing.

## Testing Requirements

Use the testing preferences in `RUST-TESTING.instructions.md`. Prefer CLI
integration tests with tiny real directories and SQLite catalogs, plus focused
unit tests for pure CAS-layout classification.

Milestone 3 acceptance tests should cover:

- CLI help exposes `import`, `build-tree`, and `audit`, but not `gc`.
- A clean imported store audits successfully with exit status 0.
- Clean output includes deterministic counters and no findings.
- The default catalog path inside the store is accepted.
- An explicit catalog path is accepted.
- Missing store root, blobs directory, or catalog fails with exit status 1.
- Symlinked store or blobs roots fail with exit status 1.
- Uninitialized, older, or newer catalog schema versions fail with exit status
  1.
- Audit creates no database, WAL, SHM, staging, store, or CAS paths.
- Audit does not change catalog bytes, blob bytes, permissions, or mtimes.
- `PRAGMA integrity_check` failures are reported when a fixture can safely
  exercise them.
- Foreign-key violations are findings and produce exit status 2.
- Invalid blob hash text and negative blob sizes are findings.
- Invalid source relative paths, non-positive `seen_count`, reversed seen
  timestamps, and source/blob size disagreement are findings.
- A missing live catalog blob is a finding.
- A missing deleted catalog blob is a finding and is still counted as a GC
  candidate.
- A valid deleted catalog blob is only a GC candidate, not a finding.
- A catalog/CAS size mismatch is a finding.
- A size-mismatched regular blob is still fully hashed so independent content
  corruption is reported in the same run.
- Same-size content corruption is detected by full BLAKE3 verification.
- A well-formed unindexed CAS file is an orphan finding.
- An orphan whose content does not match its filename reports both relevant
  facts without hashing the file twice.
- Valid CAS files with mismatched shard prefixes are invalid entries, not
  orphans.
- Uppercase, short, non-hex, misplaced, and over-deep CAS paths are findings.
- Symlinks at every CAS layout level are findings and are never followed.
- Non-regular filesystem entries are findings on supported test platforms.
- Empty valid shard directories are accepted.
- An unreadable individual blob is reported and does not prevent inspection of
  later blobs where platform permissions make this test reliable.
- Multiple findings are accumulated and printed in deterministic order.
- Integrity findings exit 2, while operational failures exit 1.
- `--chunk-size 0` is rejected and a small non-zero chunk size still verifies
  multi-chunk content correctly.
- Existing `import` and `build-tree` success and error exit behavior is
  unchanged after entrypoint refactoring.
- Pure CAS-layout tests cover valid paths, malformed levels, shard mismatches,
  excessive depth, and non-UTF-8 names supported by the host filesystem.

Tests may inspect SQLite directly and construct intentionally invalid schema-v1
catalogs with foreign-key enforcement disabled. Keep fixtures small. Do not add
large-store performance tests or rely on multi-gigabyte media fixtures.

## Documentation Updates

Update user-facing command documentation in the same implementation change:

- show the `audit` invocation and exit statuses;
- state that audit fully rehashes CAS files and may be I/O intensive;
- state that the store must be quiescent during audit;
- define deleted blobs as GC candidates that remain expected in the CAS; and
- state explicitly that milestone 3 reports but never repairs problems.

## Explicit Non-Goals

Milestone 3 does not include:

- browse-tree verification;
- source filesystem reinspection;
- staging cleanup or validation;
- repair, quarantine, chmod, move, or deletion behavior;
- mark-and-sweep garbage collection;
- automatic marking of unreferenced blobs;
- run-lock acquisition or concurrent mutation detection;
- quick or metadata-only audit modes;
- JSON or machine-versioned output;
- TTY progress dashboards;
- parallel hashing or mount-point workers;
- catalog schema changes;
- media metadata or relationship validation; or
- large-catalog performance testing.

## Future Milestones

The next safety-dependent vertical slice should be mark-and-sweep `gc`, built on
the catalog/CAS classification and exit semantics established here. Still
deferred:

- repair and quarantine workflows;
- metadata-skip optimization;
- mount-point workers and parallel audit hashing;
- single-writer database thread;
- batched catalog writes and managed WAL checkpoints;
- progress dashboards and structured non-TTY logs;
- media metadata extraction and relationship tables;
- browse-tree audit and configurable merge policies; and
- large-store performance and fault-injection testing.
