# Rust Implementation Guide: Milestone 7

This guide records implementation decisions for milestone 7. `SPEC.md` is the
source of product intent. Milestone 6 made repeat imports metadata-fast.
Milestone 7 routes every SQLite mutation through one dedicated writer thread,
batches import records, and manages passive WAL checkpoints.

The Python implementation is deprecated historical context and is not a
behavior oracle.

## Companion Instructions

- `RUST.instructions.md`
- `RUST-ARCHITECTURE.instructions.md`
- `RUST-TESTING.instructions.md`
- `SQLITE.instructions.md`

## Milestone 7: Catalog Writer, Batching, And Checkpoints

Add `crossbeam-channel` as a runtime dependency. Each mutating command starts
one command-scoped catalog writer thread. That thread exclusively owns the
writable `rusqlite::Connection`; callers submit typed requests through bounded
channels and receive typed results.

Do not add a daemon, global singleton, new command, or user-visible tuning
flags. The existing process-level store lock remains the outer coordination
boundary.

## Required Invariants

- No writable SQLite connection is created or used outside the writer thread.
- Schema migration, import writes, GC writes, transaction control, and managed
  checkpoints all execute on that thread.
- Read-only audit, build-tree, and dry-run snapshot connections remain in their
  existing read-only paths and do not use the writer.
- Import records are committed in bounded batches by count or elapsed time.
- A committed catalog row never points at a blob that exists only in staging.
- Every configured checkpoint is explicitly `PASSIVE`; never use `FULL`,
  `RESTART`, or `TRUNCATE` in normal command execution.
- Writer failure, receiver closure, panic, or join failure is returned as a
  contextual operational error.
- Shutdown flushes a final partial import batch before returning success.
- Callers cannot access the raw connection, transaction, or SQL.

## Writer Interface

Use a deep interface along these lines:

```rust
pub struct CatalogWriterConfig {
    pub channel_capacity: NonZeroUsize,
    pub max_batch_records: NonZeroUsize,
    pub max_batch_latency: Duration,
    pub checkpoint_every_batches: NonZeroUsize,
}

pub struct CatalogWriterHandle {
    request_tx: crossbeam_channel::Sender<CatalogWriteRequest>,
    join_handle: Option<std::thread::JoinHandle<Result<WriterExit>>>,
}

impl CatalogWriterHandle {
    pub fn spawn(path: PathBuf, config: CatalogWriterConfig) -> Result<Self>;
    pub fn record_imported_file(&self, request: ImportedFileRequest) -> Result<()>;
    pub fn observe_unchanged_source(&self, request: UnchangedSourceRequest) -> Result<()>;
    pub fn begin_gc(&self) -> Result<GcWriteSession>;
    pub fn finish(self) -> Result<CatalogWriterReport>;
}
```

Exact names may differ. Preserve these properties:

- producer requests have stable sequence IDs;
- committed outcomes are returned in a way that lets import construct exact
  report counters;
- the request channel is bounded and provides backpressure;
- only the handle owns shutdown/join responsibility;
- dropping a response receiver cannot crash the writer;
- `finish` is explicit and reports all queued failures.

Do not expose `Connection`, `Transaction`, SQL strings, or channel mechanics to
`ingest`, `gc`, or CLI modules.

## Import Batching

The writer receives both newly hashed imports and metadata-skipped observations.
Accumulate them until the first of:

- `max_batch_records` is reached;
- `max_batch_latency` elapses after the first pending record; or
- shutdown/flush is requested.

Apply one transaction per batch. Preserve input sequence when producing
per-record outcomes so reports are deterministic even after milestone 8 adds
parallel producers.

If any record in a batch fails validation or SQL execution, roll back the whole
batch and fail the writer. Do not retry a subset automatically. Import must stop
submitting work, join the writer, and return the contextual error. CAS blobs
already installed before the failed batch may remain as auditable/adoptable
orphans, matching the established crash boundary.

The batch transaction must retain existing behavior:

- insert or validate the blob row;
- upsert or strictly update the source observation;
- increment `seen_count` exactly once per successful observation;
- preserve `first_seen_at_ms`;
- update `last_seen_at_ms` and advisory metadata;
- resurrect referenced blobs atomically; and
- validate affected-row counts and stored sizes.

## Managed Passive Checkpoints

Disable reliance on SQLite's automatic checkpoint trigger for application
control by setting `PRAGMA wal_autocheckpoint=0` on writer connections. After
each configured number of successful batches, execute:

```sql
PRAGMA wal_checkpoint(PASSIVE);
```

Also request a final passive checkpoint during clean writer shutdown after the
last commit. Parse and validate the returned busy/log/checkpointed counts. A busy
reader is not corruption: record it as a checkpoint outcome and continue when
SQLite reports success. An actual SQLite error fails the command.

Emit structured events containing batch size, commit sequence, elapsed time,
and checkpoint result. Do not assert exact WAL byte sizes in normal tests.

## Garbage Collection Integration

Real GC currently owns a writable transaction while coordinating filesystem
deletion. Replace direct connection/transaction access with a writer-side GC
session protocol:

```text
BeginGc -> StageMark / StageResurrection / StageSweep -> CommitGc
                                                 \-> RollbackGc
```

The writer may use explicit `BEGIN IMMEDIATE`, statements on its owned
connection, and explicit `COMMIT`/`ROLLBACK` to avoid a self-referential Rust
transaction type. Only one GC session may be active. Reject import-batch
requests while it is active.

Preserve milestone-4 behavior exactly:

- fixed preflight plan;
- no mutation when findings exist;
- filesystem unlink and directory sync before staging the matching catalog
  sweep;
- partial progress commit on later mutation failure;
- interrupted-sweep recovery;
- exact action counters and exit statuses.

Run a passive checkpoint after a successful or partial GC commit according to
the same writer policy. Dry-run GC remains read-only and starts no writer.

## Schema Migration And Open Policy

Move writable open, required PRAGMAs, and migrations into the writer startup
routine. Startup must send a ready result only after:

1. opening the database;
2. setting `foreign_keys=ON`, `journal_mode=WAL`, `synchronous=NORMAL`, and
   `wal_autocheckpoint=0`;
3. applying supported migrations transactionally; and
4. validating the resulting schema version.

If startup fails, join the thread and return the original contextual error.
Read-only open policies remain unchanged.

## Observability And Probe Contract

Expose behavior-level events suitable for trace subscribers and tests:

- writer started and ready;
- batch opened, committed, or rolled back;
- record sequence committed with its domain outcome;
- checkpoint requested and completed, including result counts;
- GC session begun, committed, partially committed, or rolled back;
- shutdown flush completed; and
- writer stopped or failed.

Tests must consume events or persisted outcomes. Do not expose private worker
functions or assert a particular loop structure.

## Failure And Shutdown Semantics

- The first writer error becomes the canonical failure; retain secondary join or
  shutdown context without replacing it.
- Producers stop promptly when the writer closes the request channel.
- A successful command must explicitly finish and join the writer before
  releasing the store run lock or rendering its final report.
- A panic is converted into an operational error with the catalog path and
  command context.
- Do not detach a writer thread.
- Do not checkpoint read-only or dry-run databases.

## Testing Requirements

Add behavior-level tests proving:

- all imported rows persist correctly across multiple batches;
- count-triggered and time-triggered partial batches commit;
- shutdown flushes a final incomplete batch;
- one failing record rolls back its complete batch;
- records committed by earlier batches survive a later failure;
- metadata-skipped and hashed records can share a batch;
- `seen_count`, resurrection, timestamps, and report outcomes remain exact;
- automatic checkpoints are disabled on the writer connection;
- configured passive checkpoints emit observable completion results;
- busy passive checkpoints do not corrupt or fail successful work;
- a checkpoint SQLite error is reported operationally;
- real GC retains its mark/sweep, partial-progress, and recovery behavior through
  the writer protocol;
- dry-run commands create no writer and preserve database sidecars;
- writer startup, send, response, panic, and join failures release the store
  lock and do not hang;
- no production writable `Connection::open` remains outside the catalog writer
  implementation.

Use tiny batches and short injected intervals in tests. Do not make the suite
sleep for production latency values.

## Recommended Implementation Sequence

1. Add the dependency, writer config, request/response types, and probe events.
2. Move writable open, PRAGMAs, and migration into writer startup.
3. Implement import batching and ordered outcomes.
4. Integrate real import and metadata-skip updates.
5. Add passive checkpoint policy and result parsing.
6. Replace direct real-GC mutation with the GC session protocol.
7. Delete obsolete writable catalog entry points rather than leaving bypasses.
8. Add policy tests, integration tests, and README operational notes.
9. Run format, clippy, workspace tests, and all hooks.

## Explicit Non-Goals

- parallel hashing or mount-point scheduling;
- multiple SQLite writer threads;
- a process-global writer daemon;
- user-facing batch/checkpoint tuning flags;
- active/truncating checkpoints during normal operation;
- changing GC's mark/sweep policy;
- semantic relationships; or
- final dashboard and JSON Lines rendering.

## Definition Of Done

Milestone 7 is complete when every production SQLite mutation is demonstrably
channelled through one owned writer thread, import records batch by count/time,
explicit passive checkpoints are observable, GC behavior is preserved, no
writable bypass remains, and all repository checks pass.
