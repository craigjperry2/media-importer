# Rust Implementation Guide: Milestone 8

This guide records implementation decisions for milestone 8. `SPEC.md` remains
the source of product intent. Milestone 7 established the single catalog writer
and bounded batching. Milestone 8 introduces bounded, mount-aware source workers
without changing CAS or catalog correctness.

The Python implementation is deprecated historical context and is not a
behavior oracle.

## Companion Instructions

- `RUST.instructions.md`
- `RUST-ARCHITECTURE.instructions.md`
- `RUST-TESTING.instructions.md`
- `SQLITE.instructions.md`

## Milestone 8: Mount-Aware Parallel Ingestion

Extend `import` with one tuning option:

```text
media-importer import ... [--workers-per-mount <N>]
```

`--workers-per-mount` defaults to `1` and must be non-zero. The safe default is
appropriate for rotational media. Users may select a larger value for SSDs.
Do not attempt unreliable automatic HDD/SSD detection in this milestone.

The option limits simultaneous source-content reads independently for each
discovered source filesystem identity. It is not a global worker count.

## Resolved Scope Decisions

- Support Linux and macOS only.
- Use the source entry's Unix device identity (`st_dev`) as the stable mount
  grouping key for one import run.
- A source tree may cross nested mount points; classify every candidate from its
  own metadata rather than assuming the root's device.
- Default to one active content reader per mount identity.
- Metadata-skipped files consume no content-worker slot.
- Scanning, work scheduling, source reads, CAS staging/install, and catalog
  writing use bounded queues.
- Preserve one-pass hashing: each hashed source byte is read once and is fed to
  BLAKE3 and the staging writer in the same loop.
- Interpret the spec's page-cache statement as prohibiting a second source/CAS
  read to obtain the hash. Do not introduce `O_DIRECT`, `F_NOCACHE`, or
  filesystem-specific cache eviction; those APIs are alignment-sensitive and
  are not required for the one-pass guarantee.
- Keep one exclusive store run lock around the complete operation.

## Streaming Scanner

Replace `scan_source() -> Vec<SourceFileCandidate>` with a bounded producer or
iterator interface. Do not retain the complete source tree solely to sort it.

Each candidate includes:

```rust
pub struct SourceFileCandidate {
    pub absolute_path: PathBuf,
    pub relative_path: SourceRelativePath,
    pub size_bytes: u64,
    pub modified_at_ms: Option<i64>,
    pub mount_id: MountId,
}
```

`MountId` is an opaque typed domain value. Raw platform integers must not leak
into CLI or rendered output.

Traversal errors remain fatal and contextual. Do not silently omit unreadable
directories or files. Symlinks remain excluded. Preserve deterministic durable
identity even though discovery and completion order are no longer sorted.

## Scheduler And Worker Model

Add an ingestion executor behind a deep interface. A suitable shape is:

```rust
pub fn execute_import(
    candidates: impl Iterator<Item = Result<SourceFileCandidate>>,
    config: ExecutorConfig,
    store: &Store,
    catalog: &CatalogWriterHandle,
    observer: &dyn IngestObserver,
) -> Result<ImportReport>;
```

The exact signature may differ. The caller must not create threads, channels,
or batches itself.

For each `MountId`, allow at most `workers_per_mount` active content tasks.
Implement this with per-mount queues/workers or a scheduler with per-key
permits. Do not create an unbounded number of permanent threads for a malicious
number of device IDs; cap total live worker threads to a documented internal
limit and schedule excess mount groups fairly.

Use bounded queue capacities derived from validated internal configuration.
Channel backpressure is expected. Do not accumulate every scanned path while a
slow disk or writer catches up.

## Per-Candidate Flow

The logical flow remains:

1. scanner produces metadata and mount identity;
2. ingest performs the milestone-6 catalog metadata lookup;
3. eligible unchanged files update the catalog without entering a content
   worker;
4. other files enter the scheduler for their mount;
5. one worker opens, validates, hashes, and stages the source exactly once;
6. the worker installs or reuses the immutable CAS blob;
7. the completed observation is sent asynchronously to the milestone-7 catalog
   writer; and
8. committed writer outcomes update the final report by stable sequence ID.

Do not allow concurrent workers to write the same staging path. Existing
UUID-based staging names and atomic no-replace CAS installation must remain the
deduplication boundary. Concurrent identical content may race; exactly one
worker creates the final blob and the others verify and reuse it.

## Ordering And Determinism

Filesystem discovery and worker completion order need not be deterministic.
Persisted correctness and final aggregate counters must be deterministic for a
stable source snapshot.

Assign a monotonically increasing sequence ID at discovery. Use it to correlate
worker results and writer outcomes. Error messages should identify the source
path, not depend on whichever thread happened to log first. Structured output
in milestone 10 may report events in occurrence order, while final summaries
remain aggregate.

Do not sort millions of candidates merely to stabilize processing order.

## Backpressure And Memory Bounds

Document and test bounds for:

- scanner-to-scheduler candidates;
- queued work per mount;
- completed blobs awaiting catalog submission;
- catalog request capacity; and
- pending catalog outcome bookkeeping.

Configuration multiplication must use checked arithmetic. Reject impossible
allocations with context. Keep defaults modest enough for a laptop and avoid a
buffer allocation per idle worker.

## Failure And Cancellation Semantics

- The first scanner, worker, store, or catalog error triggers cooperative
  cancellation.
- Stop accepting new candidates, close producer channels, drain only work needed
  for safe thread shutdown, finish or fail the catalog writer, and join every
  thread.
- Never detach workers or return while a worker can still mutate staging/CAS.
- Already installed blobs and committed catalog batches may remain after a
  failure; rerun must safely reuse or adopt them.
- A failed worker must remove its staging file best-effort as today.
- Panic payloads from scanner, scheduler, worker, or writer threads become
  contextual operational errors.
- Hold the exclusive store lock until every thread has joined and the writer has
  finished.

## Observability Contract

Emit structured events for:

- candidate discovered and classified by mount;
- metadata skip;
- work queued/dequeued;
- source read started/completed with byte count;
- CAS blob installed/reused;
- active workers per mount;
- queue backpressure;
- catalog submission/commit; and
- cancellation and thread shutdown.

Events must not contain raw thread internals as part of the stable domain API.
They should be sufficient to prove maximum per-mount concurrency and exact
source byte counts through an injected observer.

## Testing Requirements

Add behavior-level tests proving:

- default configuration never has more than one active reader for one mount;
- configured `N` never permits more than `N` active readers for one mount;
- independent mount identities may make progress concurrently;
- metadata-skipped files never enter content workers;
- each hashed source file reports exactly its length in source bytes read once;
- deduplicated same-content files racing in parallel produce one immutable CAS
  blob and correct source rows;
- nested mount classification is driven by candidate metadata;
- queues remain within configured bounds under a deliberately blocked writer;
- discovery and completion reordering does not change final catalog/report
  outcomes;
- scanner, worker, store, writer, cancellation, and panic failures join every
  thread and release the run lock;
- a failed run can be rerun to a clean catalog and CAS;
- dry-run performs no writes and obeys the same metadata-skip behavior;
- `--workers-per-mount 0` fails validation without mutation;
- existing sequential behavior passes with the default value `1`.

Use injected mount identities and barriers for deterministic concurrency tests.
Do not require CI to provide multiple physical disks. Add an ignored large-tree
smoke benchmark if useful, but keep normal fixtures tiny.

## Recommended Implementation Sequence

1. Add `MountId`, platform extraction, and pure scheduler accounting types.
2. Convert scanning to a bounded streaming interface.
3. Add validated CLI/config worker count.
4. Implement the bounded scheduler and cancellation token/state.
5. Integrate store work and catalog-writer submissions.
6. Add ordered outcome aggregation and observer events.
7. Add deterministic concurrency, failure, and one-pass I/O tests.
8. Update README performance/tuning guidance and run all checks.

## Explicit Non-Goals

- automatic HDD/SSD detection;
- physical block-device topology discovery beneath a mount;
- cross-process worker sharing;
- direct/non-cached I/O;
- parallel SQLite writers;
- parallel audit or GC hashing;
- semantic relationships; or
- final TTY/structured output rendering.

## Definition Of Done

Milestone 8 is complete when import streams bounded work, enforces the configured
reader limit independently per source mount identity, can prove one-pass source
reads and zero reads for skipped files, joins cleanly on every failure path, and
passes all repository checks.
