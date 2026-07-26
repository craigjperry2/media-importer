# Rust Implementation Guide: Milestone 4

This guide records implementation decisions for milestone 4 of the Rust rewrite
of `media-importer`. The source of product intent is `SPEC.md`; this file
captures the concrete choices a coding agent should follow while implementing
the next vertical slice.

Milestone 1 implemented directory import into the content-addressed store (CAS)
and catalog. Milestone 2 added browse-tree materialization. Milestone 3 added a
read-only integrity audit. Milestone 4 adds explicit, two-run mark-and-sweep
garbage collection.

The existing Python implementation is deprecated historical context. Do not port
it file-by-file and do not use it as a behavior oracle.

## Companion Instructions

Use these colocated instruction files when implementing this guide:

- `RUST.instructions.md`
- `RUST-ARCHITECTURE.instructions.md`
- `RUST-TESTING.instructions.md`
- `SQLITE.instructions.md`

## Milestone 4: Mark And Sweep Garbage Collection

Implement one end-to-end vertical slice:

```text
media-importer gc \
  --store <STORE_ROOT> \
  [--db <DB_PATH>] \
  [--dry-run] \
  [--chunk-size <BYTES>]
```

Expose `gc` in addition to the existing `import`, `build-tree`, and `audit`
commands.

The catalog is authoritative for reachability. A blob is reachable when at
least one `source_files` row references it. Garbage collection partitions
cataloged blobs from the snapshot at the beginning of the run:

- reachable and unmarked: unchanged;
- reachable and marked: resurrect by clearing `deleted_at_ms`;
- unreachable and unmarked: mark by setting `deleted_at_ms`;
- unreachable and already marked: sweep its CAS file and catalog row.

A newly marked blob must never be swept in the same run. With no intervening
reference, two successful real GC runs are therefore required to remove a blob.
There is no time-based grace period in milestone 4; being marked before the
current run is the grace boundary.

Never run GC automatically during import.

## Goals And Safety Invariants

Milestone 4 must preserve these invariants:

- only cataloged, unreachable blobs may be swept;
- an unmarked blob may not be physically deleted;
- a blob newly marked by the current run may not be physically deleted;
- a blob referenced by any `source_files` row may not remain marked;
- every sweep candidate with a present CAS file must be fully hashed and match
  its catalog identity before any mutation begins;
- any safety-preflight finding prevents all mutation;
- orphan CAS files are findings and must never be deleted by GC;
- dry-run performs the full plan and safety preflight but makes no durable
  changes;
- empty valid CAS shard directories are left in place; and
- SQLite and filesystem operations are ordered for safe, idempotent recovery
  even though they cannot form one atomic transaction.

The command does not infer reachability by checking whether original source
files still exist. `source_files` is durable catalog state. A source path that
was not present during a later import remains a reference unless a future
explicit catalog-management feature removes it.

Future relationship tables may add more reachability roots. Milestone 4 uses
only schema-v1 `source_files` references because no relationship schema exists
yet.

## CLI Contract

- `--store` is required.
- `--store` must already exist as a real directory, not a symlink.
- `<STORE_ROOT>/blobs` must already exist as a real directory, not a symlink.
- `gc` must not create store or blobs directories.
- `--db` is optional and defaults to `<STORE_ROOT>/catalog.sqlite`.
- The catalog must already exist as a real regular file, not a symlink, and
  have schema version 1.
- Explicit `--db` is allowed inside or outside the store root.
- `--dry-run` performs the same reads, CAS classification, size checks, and
  sweep-candidate hashing as a real run but does not mutate anything.
- `--chunk-size` is optional, defaults to the existing
  `DEFAULT_CHUNK_SIZE`, and must be non-zero.
- Do not expose `--mark-only`, `--sweep-only`, `--force`, `--yes`,
  `--grace-period`, `--json`, or `--workers` in milestone 4.

Use `clap` derive for parsing. Keep CLI argument structs separate from validated
application config.

Suggested validated config shape:

```rust
pub struct GcConfig {
    pub store_root: StoreRoot,
    pub db_path: PathBuf,
    pub dry_run: bool,
    pub chunk_size: NonZeroUsize,
}
```

Reuse the existing command-neutral validation for an existing store, blobs
directory, and catalog. Do not duplicate audit path validation.

The `gc --help` text must state:

- GC uses catalog references, not the current contents of source files;
- newly unreachable blobs are marked and only previously marked blobs are
  swept;
- dry-run performs a complete read-only preflight;
- sweep candidates are fully hashed, so GC may be I/O intensive; and
- the store and catalog must remain quiescent for the entire command.

## Process Exit Status

Reuse the three-way process outcome established by audit:

- `0`: GC or GC dry-run completed successfully with no safety findings;
- `1`: invalid configuration or an operational/mutation error prevented the
  run from completing; and
- `2`: the safety preflight completed, found one or more integrity problems,
  and prevented all mutation.

Marks, resurrections, sweeps, and an empty plan are normal successful outcomes.
They do not cause exit status 2.

Safety findings are structured command outcomes, not `color_eyre` failures.
Render every finding and the report, then select exit status 2. Operational
failures still use contextual `color_eyre` reports on stderr.

A mutation-time failure can occur after some files have already been removed.
Render a partial-progress report before returning exit status 1. The GC API and
CLI dispatch must preserve that report instead of losing it behind `?`.

Do not call `std::process::exit` from library, GC, catalog, store, or rendering
code.

## Output Contract

Print deterministic action lines followed by a concise human-readable summary
to stdout. Reserve stderr for tracing and operational/configuration errors.

Only print actions for blobs whose state would change or did change. Do not
print one line for each unchanged reachable blob.

Suggested real-run output:

```text
MARK 0123...cdef bytes=42
RESURRECT 4567...abcd
SWEEP 89ab...0123 bytes=84

GC complete
Catalog blobs: 4
Reachable blobs: 2
Blobs marked: 1
Blobs resurrected: 1
Blobs swept: 1
Bytes reclaimed: 84
Sweep candidates hashed: 1
Findings: 0
```

Suggested dry-run output:

```text
WOULD_MARK 0123...cdef bytes=42
WOULD_RESURRECT 4567...abcd
WOULD_SWEEP 89ab...0123 bytes=84

GC dry run complete
Catalog blobs: 4
Reachable blobs: 2
Blobs that would be marked: 1
Blobs that would be resurrected: 1
Blobs that would be swept: 1
Bytes that would be reclaimed: 84
Sweep candidates hashed: 1
Findings: 0
```

Requirements:

- Order actions deterministically by `MARK`, `RESURRECT`, `SWEEP`, then full
  blob hash within each category.
- Print the full 64-character blob hash in actual output; abbreviated hashes
  above are illustrative only.
- Include the cataloged byte size on mark and sweep actions.
- Report reclaimable bytes from cataloged sizes using checked arithmetic.
  Count only present sweep candidates; an interrupted sweep whose file is
  already absent contributes zero reclaimable and reclaimed bytes.
- Distinguish logical sweeps finalized from an already-missing CAS file when
  useful, for example `state=already-absent`.
- On a blocked preflight, print stable findings using the milestone 3 finding
  categories and rendering conventions, then a `GC blocked` summary.
- On a mutation-time failure, print only actions known to have completed, a
  `GC incomplete` summary, and enough counters to make partial progress clear.
- Do not expose raw OS error prose as a stable action or finding identity.
- Use `tracing` for internal diagnostics.
- Defer progress bars and structured non-TTY output.

Keep actions and findings structured until the CLI renderer. Do not build
user-facing output strings in catalog or filesystem code.

## Report And Outcome Types

Add typed GC action, report, and outcome types. One possible shape is:

```rust
pub enum GcActionKind {
    Mark,
    Resurrect,
    Sweep,
}

pub struct GcAction {
    pub kind: GcActionKind,
    pub hash: BlobHash,
    pub size_bytes: u64,
    pub already_absent: bool,
}

pub struct GcReport {
    pub dry_run: bool,
    pub catalog_blobs: u64,
    pub reachable_blobs: u64,
    pub sweep_candidates_hashed: u64,
    pub bytes_reclaimable: u64,
    pub bytes_reclaimed: u64,
    pub actions: Vec<GcAction>,
    pub findings: Vec<AuditFinding>,
}

pub enum GcOutcome {
    Complete(GcReport),
    Blocked(GcReport),
    Incomplete {
        report: GcReport,
        error: color_eyre::Report,
    },
}
```

The exact names may differ. Preserve these observable distinctions:

- a completed report;
- a preflight-blocked report with no mutations;
- an operational failure before any useful report can be constructed; and
- a mutation-time failure carrying a partial report.

All counters use checked arithmetic. Allocation failures for required indexes,
plans, actions, and findings are operational errors rather than panics.

## Module Boundaries

Add a `gc` module with one deep operation:

```rust
pub fn collect_garbage(config: GcConfig) -> color_eyre::Result<GcOutcome>;
```

Inject the existing `Clock` abstraction through a testable internal entry point
when setting `deleted_at_ms`.

Preserve these boundaries:

- CLI parses arguments, validates command-level input, dispatches, renders
  actions/findings/summaries, and selects process exit status.
- `paths` owns typed hash validation, canonical CAS path construction, and
  relative display paths.
- `catalog` owns SQLite open policy, schema and integrity inspection, row
  decoding, reference queries, transactions, mark/resurrection/delete
  mutations, and raw SQL.
- `hashing` owns bounded-memory BLAKE3 streaming over safely opened files.
- `store` owns strict CAS traversal, no-follow inspection, candidate
  revalidation, and blob-file removal.
- `audit` owns audit orchestration and its public report.
- `gc` owns reachability classification, the fixed run-start plan, safety
  preflight, mutation ordering, partial-failure handling, and GC report
  construction.

Do not let CLI code choreograph SQL, reachability queries, traversal, hashing, or
deletion. Do not expose `rusqlite::Connection` outside `catalog`. Do not let
catalog code inspect CAS paths.

Milestone 3 currently contains reusable catalog-integrity and strict-CAS
inspection behavior. Refactor shared, behavior-neutral pieces rather than
copying them into GC:

- typed integrity findings and deterministic sorting may move to a small shared
  integrity/reporting module;
- strict CAS walking and safe-open behavior may move behind a store-focused
  read-only inspection interface;
- purpose-specific audit and GC orchestration remain separate; and
- the public GC API must not call CLI rendering or shell out to the `audit`
  command.

Avoid a broad generic framework. Extract only the concrete checks now shared by
audit and GC.

## Catalog Reachability Snapshot

Milestone 4 makes no schema change. Continue to require schema version 1.

Load every `blobs` row and enough reference information to classify it at the
beginning of the run. The run-start value of `deleted_at_ms` is fixed for the
plan even if later catalog mutations occur:

```text
referenced  deleted_at_ms  action
----------  -------------  -----------------------------
yes         NULL           unchanged
yes         non-NULL       resurrect
no          NULL           mark
no          non-NULL       sweep
```

Reachability is `EXISTS` of at least one `source_files.blob_hash` reference.
Do not use source path existence, `seen_count`, timestamps, file extensions, or
browse-tree state as reachability signals.

Suggested typed snapshot entry:

```rust
pub struct GcCatalogBlob {
    pub hash: BlobHash,
    pub size_bytes: u64,
    pub marked_at_ms: Option<i64>,
    pub referenced: bool,
}
```

Catalog queries must have deterministic `ORDER BY` clauses. Keep non-trivial SQL
in colocated `.sql` files loaded with `include_str!`.

Before trusting the snapshot:

- require `PRAGMA user_version = 1`;
- run `PRAGMA integrity_check`;
- run `PRAGMA foreign_key_check`;
- validate the semantic schema-v1 shape;
- decode values defensively using the milestone 3 raw-value rules; and
- preserve invalid rows as findings rather than panicking.

GC may reuse the audit schema and domain validation implementation. It must not
silently accept a catalog that audit would classify as structurally invalid.

Any catalog integrity or invalid-row finding blocks the run before mutation.
If SQLite corruption prevents required checks or enumeration from completing,
return an operational error because no trustworthy plan exists.

### Real-Run Open Policy

A real GC run opens the existing catalog writable but must not initialize or
migrate it:

- validate that the main database already exists as a real regular file;
- apply the established writable behavior PRAGMAs;
- require exactly schema version 1;
- begin an immediate transaction before taking the authoritative reachability
  snapshot; and
- retain the transaction or an equivalent write reservation through preflight
  and application so another catalog writer cannot invalidate the plan.

The quiescence precondition is still required because a SQLite transaction
cannot lock the CAS filesystem.

### Dry-Run Open Policy

Dry-run must inspect the configured catalog itself through the milestone 3
read-only WAL-aware policy:

- do not initialize, migrate, checkpoint, or copy the catalog;
- do not create WAL or SHM sidecars;
- observe committed data in an existing usable WAL; and
- fail operationally on an unsafe WAL/SHM state rather than reading stale data.

Dry-run must not acquire a writable transaction merely to simplify shared code.

## Import Resurrection

Import must atomically clear `blobs.deleted_at_ms` whenever it records a source
reference to that blob.

This is required even when the CAS file is reused and the blob row already
exists. Otherwise a successful import could leave referenced media hidden from
`build-tree` and eligible for a later sweep.

Update the catalog import transaction so these facts commit together:

- the blob row exists with the expected size;
- `deleted_at_ms` is `NULL`; and
- the source observation references the blob.

An error anywhere in the transaction must roll back the resurrection. Do not
clear a mark in a separate transaction before validating the existing blob size
and recording the source observation.

Add an observable import regression test proving that re-importing content for
a marked blob clears the mark and makes it visible to `build-tree`.

GC also plans `RESURRECT` for any marked row already referenced at the run-start
snapshot. This repairs valid pre-milestone state or a mark introduced by
external catalog tooling. Such a blob is never a sweep candidate in that run.

## Strict CAS Safety Preflight

Run the complete safety preflight before the first mark, resurrection, catalog
deletion, or filesystem deletion.

GC does not perform a full milestone 3 audit. It uses a narrower deletion-safety
preflight:

1. Validate catalog integrity, schema, domain rows, and foreign keys.
2. Walk the entire blobs tree using the strict milestone 3 layout rules without
   following symlinks.
3. Require a one-to-one membership match between valid CAS blob files and
   catalog rows.
4. Compare filesystem length with cataloged size for every cataloged blob.
5. Fully stream and BLAKE3-hash every run-start sweep candidate whose CAS file
   is present.
6. Revalidate each candidate immediately before removal.

Do not hash unchanged live blobs, blobs being newly marked, or blobs being
resurrected. Users who need a full-store content check use `audit`.

The following are safety findings and block all mutation:

- any orphan valid CAS blob;
- any malformed, misplaced, symlinked, or non-regular CAS entry;
- any cataloged blob missing from the valid CAS index, except the interrupted
  sweep recovery case below;
- any catalog/CAS size mismatch;
- any sweep-candidate hash mismatch;
- any candidate-local open, stat, or read failure;
- any catalog integrity, schema, foreign-key, or domain-row finding; and
- any local CAS enumeration or metadata failure that prevents a complete,
  trustworthy plan.

Reuse the milestone 3 finding categories and stable ordering. Do not invent
weaker GC-only interpretations for the same invalid state.

### Orphan Policy

An orphan is a valid canonical CAS regular file with no catalog row.

Every orphan is a blocking `ORPHAN_BLOB` finding. GC must never remove, adopt,
mark, quarantine, or hash an orphan in milestone 4. An orphan has no catalog
identity through which it can satisfy the two-run lifecycle.

Repair and quarantine remain future work.

### Interrupted Sweep Recovery

One missing-file state is not a blocking finding:

- the blob was already marked at the beginning of the run;
- it is still unreachable; and
- its canonical CAS file is absent.

Treat this exact state as an interrupted prior sweep. Plan a logical sweep that
deletes the remaining catalog row without attempting another filesystem
deletion. Render it with `state=already-absent` and count zero newly reclaimed
bytes.

A missing file for a reachable blob or a newly markable blob remains a blocking
`MISSING_BLOB` finding. A non-regular entry at any expected blob path is never
an interrupted sweep.

This exception is deliberately narrow. It makes the chosen file-first deletion
order retryable after a crash.

## Fixed Plan And Two-Run Boundary

Build the complete action plan from the run-start catalog snapshot before
applying anything.

The plan is immutable:

- rows classified for `MARK` cannot enter the sweep set in that run;
- rows classified for `RESURRECT` cannot enter the sweep set;
- only rows classified as already marked and unreachable may be swept; and
- a dry run reports the exact same logical plan a real run would use from the
  same quiescent state.

Use typed, bounded metadata indexes keyed by `BlobHash`. Metadata memory may be
`O(catalog rows + valid CAS files + actions + findings)`. Blob content memory
must remain `O(chunk size)`, and candidate files are hashed sequentially.

## Apply Ordering

After a clean preflight, apply actions in a deterministic order while preserving
the fixed run-start classification.

Recommended logical ordering:

1. Prepare catalog mark and resurrection mutations.
2. Process run-start sweep candidates in full-hash order.
3. Immediately before each physical deletion, no-follow revalidate that the
   path is still the same canonical regular file with the expected size.
4. Remove the CAS file.
5. Delete its catalog row with a defensive predicate requiring that it remains
   marked and unreferenced.
6. For an interrupted-sweep candidate whose file is already absent, perform
   only step 5.
7. Commit catalog changes corresponding to successful actions.

The exact internal transaction choreography may vary to satisfy Rust borrowing
and partial-failure handling, but it must preserve these properties:

- no mutation occurs before the full preflight passes;
- a catalog row is never deleted before its CAS file;
- every catalog deletion rechecks marked-and-unreferenced state;
- new marks and resurrections commit atomically with their catalog changes;
- catalog rows for successfully removed files are committed whenever SQLite
  remains usable; and
- action lines are reported as completed only when their relevant durable
  effects are known to have completed.

Deleting a 0444 blob does not require changing the blob's permissions when its
parent directory is writable. Do not chmod a sweep candidate before unlinking
it.

Do not prune now-empty first- or second-level shard directories.

## Mutation-Time Failures

SQLite and filesystem deletion cannot participate in one atomic transaction.
Milestone 4 explicitly uses file-first ordering:

```text
unlink canonical CAS file -> delete marked catalog row -> commit
```

If a mutation-time filesystem error occurs:

- stop at the first failing candidate;
- do not attempt later sweep candidates;
- preserve or commit catalog deletions for files already removed whenever the
  SQLite connection remains usable;
- preserve successful marks and resurrections when they can be committed
  consistently;
- return an incomplete report and exit status 1; and
- make the next GC run able to resume from the remaining marked candidates.

If the process crashes after unlinking a file but before deleting or committing
its catalog row, the interrupted-sweep rule above completes it on a later run.

If a catalog delete or commit fails after unlinking, return an operational error
with explicit context that the file may already be absent. Do not claim a
transaction can restore an unlinked file. A later GC must be able to reconcile
the remaining marked row.

Do not continue deleting merely to maximize reclaimed bytes after the command
has lost its expected mutation path.

## Filesystem Race Boundary

The store and catalog must be quiescent while GC runs. The user must not run
`import`, another `gc`, or any other store-mutating process against the same
store and catalog concurrently.

Use the same no-follow safety posture as audit:

- traverse with `symlink_metadata` and entry file types;
- never traverse a symlinked directory;
- safely open a sweep candidate with `O_NOFOLLOW` on Unix;
- hash the opened file rather than reopening by path;
- compare streamed size, opened-file metadata, and catalog size;
- immediately before unlink, re-check the final path without following it; and
- never delete outside the canonical `StoreRoot::blob_path(BlobHash)`.

Full directory-descriptor-relative traversal and catalog-backed run locking
remain deferred. Parent-directory replacement by an external actor is outside
the milestone 4 guarantee and is covered by the quiescence precondition.

The writable SQLite transaction prevents ordinary concurrent catalog writes
from silently changing the plan, but it does not make concurrent CAS mutation
safe.

## Dry-Run Semantics

Dry-run is a complete read-only planning operation:

- classify all catalog rows;
- inspect the full strict CAS layout;
- reconcile catalog and CAS membership;
- compare every cataloged size;
- fully hash each present sweep candidate;
- recognize interrupted sweeps;
- construct and render `WOULD_*` actions; and
- compute reclaimable bytes and all counters.

Dry-run must not:

- set or clear `deleted_at_ms`;
- delete catalog rows;
- remove blob files;
- create or prune shard directories;
- create or purge staging state;
- initialize, migrate, checkpoint, or modify SQLite;
- create database, WAL, or SHM files; or
- alter file permissions or timestamps.

Tests must assert durable no-mutation behavior, including database bytes and the
absence of newly created SQLite sidecars where they were absent before.

## Catalog And CAS Mutation Details

Keep GC SQL purpose-specific and defensive. Suggested operations include:

- deterministic reachability snapshot;
- mark one or more `deleted_at_ms IS NULL` unreachable rows at a fixed run
  timestamp;
- clear `deleted_at_ms` for referenced marked rows;
- delete one already-marked row only when no `source_files` reference exists;
  and
- optionally verify affected-row counts to detect stale plans.

Unexpected affected-row counts are operational failures. Do not silently report
an action completed if its defensive predicate matched no row.

Use the existing `Clock` abstraction and one fixed epoch-millisecond timestamp
for every mark in a run. Do not overwrite the original timestamp of rows that
were already marked at run start.

Physical removal belongs in a narrow store method that accepts a validated
`BlobHash` and expected metadata. Raw catalog text must never become a deletion
path.

## Testing Requirements

Use the testing preferences in `RUST-TESTING.instructions.md`. Prefer CLI
integration tests with tiny real directories and SQLite catalogs, plus focused
unit tests for pure reachability classification and report ordering.

Milestone 4 acceptance tests should cover:

- CLI help exposes `import`, `build-tree`, `audit`, and `gc`.
- GC help documents the two-run lifecycle, hashing, dry-run, and quiescence.
- Missing or symlinked store, blobs directory, or catalog fails with exit 1.
- Uninitialized, older, newer, or structurally invalid catalogs fail safely.
- `--chunk-size 0` is rejected.
- A clean store with only referenced unmarked blobs is a successful no-op.
- The first real run marks an unreferenced blob but leaves its CAS file and
  catalog row present.
- The second real run hashes and sweeps that previously marked, still
  unreferenced blob.
- Running dry-run twice makes no changes and continues to report the same plan.
- A dry run of a newly unreachable blob reports `WOULD_MARK`, not
  `WOULD_SWEEP`.
- A dry run of a previously marked blob reports `WOULD_SWEEP` and hashes it.
- No time delay is required between mark and sweep runs.
- Mark timestamps use one injected fixed clock value.
- A marked blob that gains a source reference is resurrected and not swept.
- Re-importing marked content atomically clears `deleted_at_ms`.
- Resurrected content becomes visible to `build-tree`.
- Replacing a source path's content can leave its old blob unreferenced; two GC
  runs then collect the old blob while preserving the new one.
- Multiple source references keep a blob reachable until all references are
  absent.
- A valid orphan CAS file produces `ORPHAN_BLOB`, exit 2, and no mutation.
- Invalid CAS entries, symlinks, missing live blobs, non-regular blobs, and size
  mismatches produce findings, exit 2, and no mutation.
- A corrupt sweep candidate produces `HASH_MISMATCH`, exit 2, and no mutation.
- A corrupt non-candidate with unchanged size is not rehashed by GC; full
  content verification remains `audit` behavior.
- Multiple sweep candidates are hashed and actioned in deterministic hash
  order.
- Sweep-candidate hashing works across multiple chunks.
- An already-marked, unreachable row with an absent CAS file is finalized as an
  interrupted sweep.
- A missing unmarked or referenced blob remains a blocking finding.
- Empty shard directories remain after their final blob is swept.
- Re-running after a completed sweep is idempotent.
- Counters and byte totals distinguish dry-run, completed, blocked, and
  incomplete outcomes.
- Finding and action output is deterministic.
- Existing `import`, `build-tree`, and `audit` behavior and exit statuses remain
  unchanged.

Add a narrow fault-injection seam only where real filesystem behavior cannot
reliably exercise the agreed partial-sweep contract. A behavior-level test must
prove:

- deletion stops at the first injected mutation failure;
- later candidates are untouched;
- rows for already removed files are reconciled as far as SQLite permits;
- the command returns an incomplete report and operational failure; and
- rerunning can complete the remaining work.

Do not couple tests to private SQL statement order, buffer counts, or the exact
internal transaction shape.

## Documentation Updates

Update user-facing command documentation in the same implementation change:

- show the GC invocation and exit statuses;
- define catalog `source_files` references as the current reachability roots;
- explain the two-run mark-and-sweep lifecycle;
- explain automatic resurrection during import and GC;
- state that dry-run performs full safety checks and candidate hashing;
- state that orphans and other integrity findings block all mutation;
- explain interrupted-sweep recovery and the non-atomic filesystem/SQLite
  boundary;
- state that GC leaves empty shard directories in place; and
- state the quiescence requirement prominently.

Update stale milestone status text that still says `gc` is unavailable, without
rewriting historical milestone contracts.

## Explicit Non-Goals

Milestone 4 does not include:

- deleting or pruning `source_files` based on the current source filesystem;
- deleting, adopting, repairing, or quarantining orphan CAS files;
- time-based retention or configurable grace periods;
- separate mark-only or sweep-only modes;
- user-selected blob deletion;
- relationship-aware reachability;
- browse-tree rebuilding or cleanup as part of GC;
- shard-directory pruning;
- full rehashing of non-candidate blobs;
- catalog-backed run locking;
- directory-descriptor-relative filesystem mutation;
- globally atomic SQLite/filesystem deletion;
- JSON or machine-versioned output;
- TTY progress dashboards;
- parallel hashing or mount-point workers;
- a catalog schema migration;
- managed WAL checkpoints or a single-writer thread;
- large-store performance testing; or
- general repair mode.

## Future Milestones

Likely follow-up work includes:

- catalog-backed run locking for all mutating commands;
- explicit source-record management and deletion policy;
- repair or quarantine workflows for orphans and corrupt blobs;
- configurable retention periods;
- relationship tables and relationship-aware reachability;
- metadata-skip optimization;
- mount-point workers and parallel hashing;
- single-writer database coordination, batching, and managed WAL checkpoints;
- progress dashboards and structured non-TTY logs;
- browse-tree audit and configurable merge policies; and
- large-store performance and crash/fault-injection campaigns.
