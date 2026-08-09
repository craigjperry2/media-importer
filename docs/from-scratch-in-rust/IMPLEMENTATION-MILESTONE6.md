# Rust Implementation Guide: Milestone 6

This guide records implementation decisions for milestone 6 of the Rust rewrite
of `media-importer`. `SPEC.md` remains the source of product intent. Milestones
1 through 5 implemented import, browse-tree materialization, audit, garbage
collection, and cooperative single-node coordination. Milestone 6 makes repeat
imports metadata-fast without weakening the existing CAS safety rules.

The Python implementation is deprecated historical context and is not a
behavior oracle.

## Companion Instructions

Use these colocated instruction files:

- `RUST.instructions.md`
- `RUST-ARCHITECTURE.instructions.md`
- `RUST-TESTING.instructions.md`
- `SQLITE.instructions.md`

## Milestone 6: Metadata-Fast Idempotent Import

An unchanged source file should not be opened or read for content on a repeated
import. Use the catalog's source identity and advisory filesystem metadata to
reuse the previously recorded BLAKE3 identity.

Preserve the four-command CLI. Extend `import` only:

```text
media-importer import \
  --store <STORE_ROOT> \
  --source <SOURCE_DIRECTORY> \
  [--db <DB_PATH>] \
  [--dry-run] \
  [--chunk-size <BYTES>] \
  [--no-metadata-skip]
```

Metadata skipping is enabled by default. `--no-metadata-skip` forces the
milestone-5 behavior and hashes every regular source file.

## Resolved Scope Decisions

- A skip candidate is identified by canonical `source_root` plus normalized
  `relative_path`.
- Skip only when catalog size equals observed size and both stored and observed
  modified times are present and equal.
- A missing modified time never qualifies for skipping.
- Before skipping, require the cataloged CAS path to be a regular file whose
  metadata size equals the catalog blob size.
- Do not rehash the CAS blob during import. `audit` remains the content-integrity
  operation.
- A matching blob marked for deletion may be metadata-skipped, but the source
  observation update and resurrection must be atomic.
- Metadata equality is an optimization contract, not proof that content cannot
  have changed. Document the filesystem-fidelity assumption and the opt-out.
- A metadata mismatch, missing source row, missing CAS file, or CAS size
  mismatch falls back to the ordinary one-pass hash-and-stage path. Existing
  store safety checks still decide whether repair or failure is appropriate.
- Do not add inode, ctime, sampling, or filesystem-specific change journals in
  this milestone.

## Catalog Read Interface

Add one typed lookup that returns everything needed to make the skip decision
without leaking SQL into `ingest`:

```rust
pub struct KnownSourceFile {
    pub blob_hash: BlobHash,
    pub source_size_bytes: u64,
    pub modified_at_ms: Option<i64>,
    pub blob_size_bytes: u64,
    pub blob_deleted_at_ms: Option<i64>,
}

pub fn known_source_file(
    &self,
    source_root: &str,
    relative_path: &SourceRelativePath,
) -> Result<Option<KnownSourceFile>>;
```

Keep the SQL in a colocated `.sql` file. Validate all integer and hash values at
the catalog boundary. The lookup must use the same writable catalog connection
as the current real import so its observation and subsequent update are
coherent under the exclusive store lock. Dry-run uses the existing read-only
snapshot policy.

## Pure Skip Classification

Put the comparison in a pure helper with a typed result. It must be unit-testable
without a filesystem or SQLite connection.

```rust
pub enum ImportDisposition {
    Skip {
        blob_hash: BlobHash,
        size_bytes: u64,
        resurrection_required: bool,
    },
    Hash { reason: HashReason },
}
```

`HashReason` should distinguish at least:

- metadata skipping disabled;
- no catalog observation;
- size changed;
- modified time unavailable;
- modified time changed;
- catalog/source inconsistency; and
- CAS entry missing or invalid.

Do not expose these reasons as unstable prose from low-level modules. Emit a
structured trace field or observation event suitable for later reporting.

## Real Import Flow

For each scanned candidate:

1. Read the known catalog observation.
2. Run the pure metadata classification.
3. For a tentative skip, perform a no-follow CAS regular-file and size check.
4. If still eligible, do not open the source file and do not create staging.
5. Atomically update `last_seen_at_ms`, increment `seen_count`, refresh advisory
   source metadata, and clear `deleted_at_ms` for the referenced blob.
6. Otherwise use the existing one-pass source-to-staging hash/write/install flow
   and record the imported file as before.

Never commit a source observation for a blob that exists only in staging.
Metadata skipping must not change the store lock lifetime or staging purge
ordering.

Add a catalog operation specifically for observing a known unchanged source.
It must verify that the source row still points at the expected blob and that
the blob size still matches. Treat an unexpected affected-row count as an
operational error rather than silently inserting a new row.

## Dry-Run Flow

Dry-run uses the same classification against its read-only catalog snapshot.
It must not update `seen_count`, resurrect a row, create staging, or mutate any
file. An eligible skip must not read source content. A non-skip candidate may be
hashed exactly as in the existing dry-run behavior.

If the store or catalog is absent, every candidate requires hashing for an
accurate deduplication plan, but dry-run still creates nothing.

## Reporting And Observability

Extend `ImportReport` with at least:

```rust
pub files_skipped: u64,
pub bytes_skipped: u64,
pub files_hashed: u64,
pub bytes_hashed: u64,
```

Keep existing counters backward-compatible in meaning:

- a metadata-skipped file counts as `files_seen` and a reused blob;
- it does not count as bytes written;
- its source record counts as updated after a real successful catalog update;
- dry-run reports the corresponding planned outcome.

Add trace events for the skip/hash decision and for source content reads. Tests
must be able to observe byte counts through a narrow probe or injected observer;
they must not infer I/O behavior from wall-clock timing.

Do not add the final TTY dashboard or JSON Lines renderer yet. Milestone 10 will
consume these structured events.

## Failure Semantics

- A catalog lookup failure stops the import before reading that candidate.
- A failed CAS metadata check falls back only for absence or an expected size
  mismatch. Permission and unexpected I/O failures remain operational errors.
- A source that changes after scanning is still detected by the existing
  before/read/after checks when hashing occurs.
- Earlier committed observations may remain if a later file fails, matching the
  existing resilient per-file semantics.
- Skip classification must never convert catalog corruption into a successful
  reuse.

## Testing Requirements

Add integration tests proving:

- an unchanged second import performs zero source-content reads;
- the second import still increments `seen_count` and updates
  `last_seen_at_ms`;
- changed size forces hashing;
- changed mtime with the same size forces hashing;
- absent or unavailable mtime forces hashing;
- `--no-metadata-skip` forces hashing of unchanged files;
- a missing CAS blob prevents skipping and follows existing recovery/error
  semantics;
- a wrong-size CAS blob is not silently reused;
- an eligible marked blob is resurrected atomically without source reads;
- dry-run uses skipping but leaves catalog, WAL, SHM, staging, CAS, permissions,
  and timestamps unchanged;
- path spelling aliases still resolve to the same canonical source identity;
- the existing changed-content and deduplication tests continue to pass.

Use a byte-counting reader seam, telemetry observer, or trace probe. Do not use a
timing threshold as evidence that content was skipped.

## Recommended Implementation Sequence

1. Add the catalog lookup SQL and typed result.
2. Add and unit-test pure skip classification.
3. Add the unchanged-source catalog update/resurrection operation.
4. Integrate classification into real import.
5. Integrate the read-only form into dry-run.
6. Add counters and trace/probe events.
7. Add behavior-level integration tests and update README import documentation.
8. Run all repository checks.

## Explicit Non-Goals

- mount-point workers or parallel hashing;
- a catalog writer thread or batched commits;
- managed WAL checkpoints;
- relationship schema or media metadata extraction;
- TTY dashboards or structured non-TTY rendering;
- filesystem change journals, inode caches, or content sampling;
- treating mtime as a cryptographic correctness proof; or
- literal direct-I/O/page-cache bypass.

## Definition Of Done

Milestone 6 is complete when an unchanged repeat import can be proven to avoid
opening source files for content, all skip fallbacks preserve correctness, the
opt-out is covered, dry-run remains mutation-free, documentation states the
metadata-fidelity tradeoff, and all format, lint, test, and pre-commit checks
pass.
