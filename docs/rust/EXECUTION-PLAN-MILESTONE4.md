# Milestone 4 Execution Plan: Mark-And-Sweep Garbage Collection

This plan guides a coding agent through implementing
`IMPLEMENTATION-MILESTONE4.md`. It sequences the work, resolves implementation
choices left open by that guide, and makes partial-failure and crash-recovery
semantics explicit.

Use this precedence order:

1. `IMPLEMENTATION-MILESTONE4.md` is the milestone contract.
2. This plan controls choices that the milestone contract leaves open.
3. `SPEC.md` and the colocated Rust, architecture, testing, and SQLite
   instructions apply where they do not conflict with deliberate milestone-4
   deferrals.

In particular, milestone 4 deliberately requires an existing catalog for both
real and dry-run GC, and deliberately defers the single-writer thread, managed
checkpoints, progress dashboards, and structured output. Those milestone
choices override the more general instructions for this slice.

If this plan accidentally conflicts with the milestone contract, stop and
resolve the conflict rather than silently choosing one. Do not use
`docs/old_python/` as a behavior oracle.

## Resolved Safety Decisions

### The Two-Run Boundary Is Committed State, Not Process Exit Status

The fixed run-start value of `deleted_at_ms` is the only mark generation in
schema version 1:

- `NULL` at run start can be marked but not swept in that run;
- non-`NULL` at run start can be swept if still unreachable; and
- a reference at run start prevents sweeping and clears any mark.

Interpret “two successful real GC runs” at the per-blob state-transition level:
the mark must have committed in an earlier invocation, then the sweep must
commit in a later invocation. An invocation may exit `1` because a later sweep
candidate failed while still successfully committing marks prepared earlier in
that invocation. User documentation must not imply that two process exit-`0`
results are required.

Any valid non-`NULL` integer `deleted_at_ms`, including a value written by
external tooling, counts as a prior mark. There is no run ID, mark generation,
or time grace in schema version 1.

### Quiescence Is A Hard, Unenforced Precondition

Before milestone 5, milestone 4 did not add a run lock. Milestone 5 now
provides same-store advisory coordination, so cooperating commands no longer
require manual quiescence. External writers remain unsupported. Correctness at
the milestone-4 boundary therefore required that no
`import`, `gc`, or external catalog/CAS writer touches the selected store and
catalog from path validation until the command returns.

The writable SQLite reservation prevents ordinary catalog writes from changing
the snapshot, but it does not protect CAS operations that happen before another
command's catalog transaction. Do not claim concurrent GC/import correctness.
Make the limitation prominent in `gc --help`, the README, tracing context, and
the module-level GC documentation.

Fail operationally on `SQLITE_BUSY` or another inability to acquire the
immediate transaction. Do not wait indefinitely and do not fall back to an
unreserved snapshot.

### Real GC Requires An Existing WAL-Mode Catalog

Use a purpose-specific real-GC open:

- validate the final database path with `symlink_metadata`;
- open with `SQLITE_OPEN_READ_WRITE | SQLITE_OPEN_NOFOLLOW`, without
  `SQLITE_OPEN_CREATE`;
- require `PRAGMA user_version = 1`;
- require the current `PRAGMA journal_mode` to already be `wal`;
- enable `foreign_keys` and set `synchronous=NORMAL`;
- request `journal_mode=WAL` only after confirming that it is already WAL, so
  the request is idempotent; and
- begin `TransactionBehavior::Immediate` before the authoritative snapshot.

Application-created schema-v1 catalogs are already initialized in WAL mode.
Treat any other journal mode as an operational configuration error for real GC
rather than changing durable journal mode before the safety preflight.
Re-read and require `user_version = 1` inside the immediate transaction before
enumerating the authoritative snapshot; the earlier open-policy check is not a
substitute for an in-transaction check.

A real run may create or update SQLite WAL/SHM coordination state while
acquiring its writable reservation. “No mutation on blocked preflight” means no
application-row changes and no CAS changes. The stricter no-sidecar/no-copy
rule applies to dry-run.

Dry-run uses the existing milestone-3 hybrid read-only, WAL-aware policy. It
must inspect the configured database directly and must not use
`ReadOnlyCatalog::open_if_exists`, the SQLite backup API, a temporary copy, or a
writable transaction. Include `SQLITE_OPEN_NOFOLLOW` alongside the existing
read-only and URI flags so audit and GC do not regress final-component database
path safety during the shared open-policy refactor.

### File Identity Revalidation

Replace the current hard-coded Unix open flag with `libc::O_NOFOLLOW` or an
equivalent platform-provided constant. The existing literal is not portable
between Linux and macOS.

For every present sweep candidate:

1. Compute the canonical path only through
   `StoreRoot::blob_path(&BlobHash)`.
2. Open the final component read-only with `O_NOFOLLOW`.
3. Require opened-file metadata to describe a regular file.
4. Record expected length plus Unix device and inode identity from the opened
   file.
5. Hash that already-open file and compare streamed length, opened metadata
   length, catalog length, and BLAKE3 identity.
6. Close the preflight file after hashing; do not retain one descriptor per
   candidate.
7. Immediately before unlink, use `symlink_metadata` and require a regular file
   with the same length, device, and inode as the preflight-opened file.
8. Unlink only the canonical path derived from the typed hash.

Device/inode comparison narrows replacement races but does not make path-based
unlink race-free. Final-component replacement after revalidation and
parent-directory replacement remain outside the guarantee and are covered only
by the quiescence precondition. Do not describe this as adversarially safe
filesystem mutation.

### Filesystem Deletion Durability

For a newly removed candidate, use this order:

```text
revalidate -> unlink -> fsync containing shard directory
             -> stage defensive catalog delete -> commit
```

Open and sync the containing shard directory through a small store-owned Unix
helper. Treat failure to unlink or sync as a mutation-time operational error.
Do not stage that candidate's catalog delete unless its unlink and directory
sync both succeeded.

For an interrupted-sweep candidate already absent at run start, sync the
nearest existing canonical parent directory before staging the catalog delete.
This persists the observed absence as far as the supported filesystem permits
without creating missing shard directories.

This ordering covers ordinary process interruption and strengthens power-loss
ordering, but it is not a claim of globally atomic SQLite/filesystem mutation.
SQLite and the filesystem can still fail independently.

Never prune shard directories. Never chmod a candidate before deletion.

### “Bytes Reclaimed” Means Logical Cataloged Bytes

Keep the output label required by the milestone contract, but define it as the
sum of cataloged logical sizes for present files whose sweeps were finalized.
It is not a measurement of physical blocks returned to ZFS. Compression,
snapshots, deduplication, sparse allocation, reflinks, and other hard links can
make physical space recovery differ.

For incomplete reports, also retain internal/report counters for:

- CAS files successfully unlinked and synced;
- catalog sweeps durably finalized;
- logical bytes unlinked; and
- logical bytes whose sweep was durably finalized.

This distinguishes physical progress from completed logical actions.

### Dry-Run Timestamp Semantics

Dry-run performs no explicit chmod, timestamp update, file creation, deletion,
SQLite write, checkpoint, migration, or backup. Ordinary reads may update
filesystem access time according to mount policy. Tests assert contents,
names, permissions, modification times, database bytes, and absence of newly
created sidecars; they do not require access times to remain unchanged.

## Stable Outcome And Progress Model

Keep planned work separate from durably completed work.

Recommended domain shapes:

```rust
pub enum GcActionKind {
    Mark,
    Resurrect,
    Sweep,
}

pub enum SweepSourceState {
    Present,
    AlreadyAbsent,
}

pub struct GcAction {
    pub kind: GcActionKind,
    pub hash: BlobHash,
    pub size_bytes: u64,
    pub sweep_source_state: Option<SweepSourceState>,
}

pub struct GcReport {
    pub dry_run: bool,
    pub catalog_blobs: u64,
    pub reachable_blobs: u64,
    pub sweep_candidates_hashed: u64,
    pub planned_marks: u64,
    pub planned_resurrections: u64,
    pub planned_sweeps: u64,
    pub completed_marks: u64,
    pub completed_resurrections: u64,
    pub completed_sweeps: u64,
    pub cas_files_removed: u64,
    pub bytes_reclaimable: u64,
    pub bytes_unlinked: u64,
    pub bytes_reclaimed: u64,
    pub actions: Vec<GcAction>,
    pub findings: Vec<IntegrityFinding>,
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

Names may differ, but preserve these meanings:

- In a completed dry-run, `actions` contains the fully preflighted plan and is
  rendered as `WOULD_*`.
- In a completed real run, `actions` contains only durably committed logical
  actions.
- In a blocked run, render findings and a blocked summary, not action lines.
  Planned counters may remain available in the report for diagnostics.
- In an incomplete real run, `actions` contains only logical actions known to
  have committed. Physical removals not known to have matching committed row
  deletes are represented only by partial-progress counters and error context.
- `bytes_reclaimable` counts present, fully preflighted sweep candidates.
- `bytes_unlinked` counts successful unlink-plus-directory-sync operations in
  the current invocation, even if the catalog commit later fails.
- `bytes_reclaimed` counts present candidates whose logical sweep is known to
  have committed. Already-absent candidates contribute zero.

If the final SQLite commit returns an error, conservatively report no staged
catalog actions as completed. Keep known unlink/sync progress in
`cas_files_removed` and `bytes_unlinked`, drop/roll back the transaction as far
as SQLite permits, and return `Incomplete`. Do not guess whether a failed
commit became durable.

On the next invocation, read the catalog and filesystem anew. The interrupted
sweep rule, not an in-memory retry plan, determines recovery.

## Target Architecture

Keep one deep public operation:

```rust
pub fn collect_garbage(config: GcConfig) -> color_eyre::Result<GcOutcome>;
```

Recommended ownership:

- `cli`: parse and validate `gc`, render structured actions/findings/summaries,
  and map outcomes to exit codes.
- `config`: own `GcOptions -> GcConfig` conversion and reuse command-neutral
  existing-store/catalog validation.
- `integrity` or another small shared module: own finding types, stable
  identities, deterministic sorting, checked finding growth, and path
  rendering shared by audit and GC.
- `paths`: own `BlobHash`, `StoreRoot`, canonical CAS path construction, and
  path identity helpers.
- `catalog`: own purpose-specific real/dry GC opens, SQLite inspection,
  reachability snapshot, transaction lifetime, and defensive mutations.
- `store`: own strict CAS traversal, safe no-follow open, opened-file identity,
  pre-unlink revalidation, unlink, directory sync, and the test deletion seam.
- `hashing`: own bounded streaming BLAKE3 over an already-open file.
- `audit`: retain full-audit orchestration and public report behavior.
- `gc`: classify the fixed plan, run deletion-safety reconciliation, coordinate
  catalog/store operations, and assemble complete/blocked/incomplete reports.

Do not expose `rusqlite::Connection` or raw SQL outside `catalog`. A
crate-private catalog transaction/session type with behavior-level methods is
acceptable.

Suggested catalog shape:

```rust
pub struct GcCatalog {
    connection: rusqlite::Connection,
}

pub struct GcTransaction<'connection> {
    transaction: rusqlite::Transaction<'connection>,
}

impl GcCatalog {
    pub fn open_existing_for_gc(path: &Path) -> Result<Self>;
    pub fn begin_immediate(&mut self) -> Result<GcTransaction<'_>>;
}

impl GcTransaction<'_> {
    pub fn inspect_and_snapshot(&self) -> Result<GcCatalogSnapshot>;
    pub fn stage_mark(&self, entry: &GcCatalogBlob, marked_at_ms: i64)
        -> Result<()>;
    pub fn stage_resurrection(&self, entry: &GcCatalogBlob) -> Result<()>;
    pub fn stage_sweep(&self, entry: &GcCatalogBlob) -> Result<()>;
    pub fn commit(self) -> Result<()>;
}
```

The exact Rust borrowing shape may differ. Preserve the important behavior:
the same immediate transaction owns the snapshot and all staged real-run
catalog mutations while GC performs the filesystem preflight and apply steps.

Dry-run should use a separate purpose-specific function that returns the same
typed snapshot without exposing a writable session:

```rust
pub fn inspect_catalog_for_gc_dry_run(path: &Path)
    -> Result<GcCatalogSnapshot>;
```

## Catalog Snapshot And SQL

Use a deterministic query with an explicit `ORDER BY blobs.hash` and an
`EXISTS` reachability expression:

```sql
SELECT
    blobs.rowid,
    blobs.hash,
    blobs.size_bytes,
    blobs.created_at_ms,
    blobs.deleted_at_ms,
    EXISTS (
        SELECT 1
        FROM source_files
        WHERE source_files.blob_hash = blobs.hash
    ) AS referenced
FROM blobs
ORDER BY blobs.hash;
```

Keep the final SQL in a colocated `.sql` file. Decode raw values defensively
using the milestone-3 rules. A valid typed entry is:

```rust
pub struct GcCatalogBlob {
    pub hash: BlobHash,
    pub size_bytes: u64,
    pub marked_at_ms: Option<i64>,
    pub referenced: bool,
}
```

Use purpose-specific defensive SQL:

- mark only the expected hash when `deleted_at_ms IS NULL` and no source
  reference exists;
- resurrect only the expected hash when `deleted_at_ms IS NOT NULL` and a
  source reference exists;
- sweep only the expected hash when `deleted_at_ms IS NOT NULL` and no source
  reference exists.

Require exactly one affected row for every planned mutation. A zero or
multi-row result is a mutation-time operational failure. Do not weaken the
predicate merely because an immediate transaction should prevent ordinary
concurrent writers.

Use one fixed clock value for all marks in one real run. Read the clock only
after a clean preflight and before staging mutations. Dry-run must not need a
wall-clock value.

## Strict GC Preflight

GC reuses audit's catalog validation and strict CAS classification but does not
perform audit's full-content verification.

The preflight order is:

1. Obtain the real-run immediate transaction or dry-run read snapshot.
2. Run integrity, foreign-key, semantic-schema, blob-row, and source-row
   validation.
3. Build the valid typed catalog index and fixed action plan.
4. Walk the entire blobs tree without following symlinks.
5. Reconcile valid CAS membership with valid catalog membership.
6. Compare filesystem metadata length with catalog size for every valid
   catalog blob whose canonical CAS file is present.
7. Safely open and fully hash each present run-start sweep candidate in hash
   order.
8. Sort and deduplicate findings.
9. Begin mutation only when findings are empty.

Catalog findings do not excuse skipping the CAS walk when traversal remains
possible. Continue collecting independent findings. If missing required schema
objects or SQLite stepping failure prevents a trustworthy catalog enumeration,
return an operational error instead of pretending preflight completed.

Use these state-specific reconciliation rules:

- valid CAS file without a valid catalog row: `ORPHAN_BLOB`, never hash or
  mutate it;
- valid catalog row without a valid CAS file:
  - marked and unreachable at run start: interrupted sweep, not a finding;
  - otherwise: `MISSING_BLOB`;
- expected canonical path occupied by a non-regular entry:
  `NON_REGULAR_BLOB`, never interrupted sweep;
- present catalog/CAS pair with different lengths: `SIZE_MISMATCH`;
- sweep candidate with hash mismatch: `HASH_MISMATCH`;
- non-candidate with matching size: do not hash it.

Any blobs-root or nested enumeration/stat failure is a stable `CAS_IO_ERROR`
finding and produces `Blocked`, as required by the milestone-4 safety list. The
CAS inspection result must carry a completeness flag or inaccessible-prefix
set. When traversal is incomplete, do not derive misleading `MISSING_BLOB` or
`ORPHAN_BLOB` findings from the incomplete index. Candidate open/stat/read
failures are stable `BLOB_IO_ERROR` findings and also block mutation.

Do not convert a preflight candidate I/O finding into an operational error.
After mutation begins, however, revalidation/unlink/sync failures are
operational and produce `Incomplete`.

## Fixed Plan

Classify with a pure function:

```text
referenced  marked  action
----------  ------  -----------
yes         no      unchanged
yes         yes     resurrect
no          no      mark
no          yes     sweep
```

Build all plan vectors before hashing or mutation. Sort each vector by full
`BlobHash`. Never reclassify an entry in the current invocation.

Maintain separate vectors for:

- marks;
- resurrections;
- one hash-sorted sweep vector whose entries distinguish present candidates
  with preflight-opened identity from already-absent interrupted sweeps; and
- unchanged entries, which need only contribute to counters.

Metadata memory may be
`O(catalog rows + valid CAS files + actions + findings)`. Hash content memory
must be `O(chunk size)`, and candidates are hashed sequentially.

Use `try_reserve`/`try_reserve_exact` for required collection and buffer growth
where Rust exposes fallible reservation. Use checked arithmetic for counters and
byte totals. Do not claim that every possible global allocator failure is
recoverable; Rust may abort on allocation failure outside fallible collection
growth.

## Apply Algorithm

After a clean real-run preflight:

1. Fallibly reserve enough report/action/progress capacity for the entire fixed
   plan. Allocation failure here is an operational error before mutation.
2. Read one fixed mark timestamp.
3. Stage every mark in hash order, checking affected-row count.
4. Stage every resurrection in hash order, checking affected-row count.
5. Process the single sweep vector in hash order:
   - for a present candidate:
     1. revalidate type, length, device, and inode;
     2. unlink the canonical file;
     3. sync its containing shard directory;
     4. record physical progress;
     5. stage the defensive catalog delete;
     6. record the sweep as staged, not completed;
   - for an already-absent candidate:
     1. sync the nearest existing canonical parent;
     2. stage the defensive catalog delete;
     3. record the logical sweep as staged with `AlreadyAbsent`.
6. Commit once.
7. Only after commit succeeds, convert all staged catalog actions into completed
   actions and renderable counters.

Do not perform fallible collection growth after the first filesystem mutation.
All apply-time recording uses the capacity reserved in step 1 plus checked
counter arithmetic.

Although action output is sorted as `MARK`, `RESURRECT`, `SWEEP`, do not print
while applying. Buffer structured state and render only after the outcome is
known.

### Failure During Mark Or Resurrection

- Stop immediately.
- No CAS deletion has started.
- Roll back by dropping the transaction.
- Return `Incomplete` with no completed action lines and zero physical-progress
  counters.

### Failure During Candidate Revalidation, Unlink, Or Directory Sync

- Stop at that candidate; do not touch later candidates.
- Do not stage a catalog delete for the failing candidate unless unlink and
  sync both completed.
- Attempt to commit marks, resurrections, and earlier staged sweeps if the
  SQLite transaction remains usable.
- If commit succeeds, report those logical actions as completed and preserve
  physical progress counters.
- Return `Incomplete` with the filesystem error even after a successful partial
  commit.

### Failure During Defensive Catalog Delete

The file may already be durably absent.

- Stop immediately.
- Attempt to commit marks, resurrections, and earlier catalog sweeps if SQLite
  remains usable.
- Do not report the failing candidate as a completed sweep.
- Include its successful unlink in physical-progress counters.
- Return `Incomplete` with hash-specific context that its catalog row may
  remain and the next GC run will treat it as an interrupted sweep.

### Failure During Commit

- Report no staged catalog actions as known completed.
- Preserve only known unlink/sync counters.
- Return `Incomplete`.
- Do not reopen and invent completion based on assumptions about SQLite's
  failed-commit state. The next invocation performs authoritative recovery.

### Already-Absent Candidate Delete Failure

- No file was removed and no bytes were reclaimed.
- Stop and follow the same partial-commit rules.
- Leave the row for the next invocation.

## Import Resurrection

Modify `Catalog::record_imported_file` without changing its public behavior:

1. Begin the existing per-file transaction.
2. Insert the blob row if absent.
3. Read and validate the existing blob size.
4. Clear `deleted_at_ms` for the imported blob.
5. Upsert the source observation.
6. Commit once.

Make the resurrection update conditional on the expected hash and size.
Require one matching blob row after insertion/validation. Do not clear an
unrelated mark and do not clear the mark before size validation.

The CAS install remains outside the SQLite transaction as established in
milestone 1. Document that rerunning a failed import can normally adopt an
orphaned installed blob by reusing it and completing the catalog record. GC
still blocks on unresolved orphans and does not become a repair command.

Add a regression test that:

- imports a blob;
- marks it through a test fixture;
- reimports the same content;
- observes `deleted_at_ms IS NULL`; and
- runs `build-tree` to prove the blob is live again.

## CLI Dispatch And Rendering

Add `Gc` to the Clap command and validated command enum. Reuse
`DEFAULT_CHUNK_SIZE`, parse the GC CLI value as `usize`, and convert it to
`NonZeroUsize` during `GcOptions -> GcConfig` validation. This deliberately
routes `gc --chunk-size 0` through the normal invalid-configuration path with
exit status 1, rather than Clap's parse-error exit status 2.

Map outcomes:

- `Complete`: render and return `0`;
- `Blocked`: render all findings and return `2`;
- `Incomplete`: render the partial report to stdout, then return/render the
  contextual error on stderr with exit `1`;
- outer `Err`: no useful GC report exists; render the operational error on
  stderr and return `1`.

Do not route `Incomplete` through `?` before rendering its report.

Render actions only from structured action values:

- real completed mark: `MARK <hash> bytes=<size>`;
- real completed resurrection: `RESURRECT <hash>`;
- real completed present sweep: `SWEEP <hash> bytes=<size>`;
- real completed interrupted sweep:
  `SWEEP <hash> bytes=<size> state=already-absent`;
- dry-run equivalents use `WOULD_*`.

Sort actions by kind order `MARK`, `RESURRECT`, `SWEEP`, then full hash.

Incomplete summaries must make both kinds of progress visible. At minimum
include:

- planned actions by kind;
- completed catalog actions by kind;
- CAS files removed;
- logical bytes unlinked;
- logical bytes reclaimed by finalized sweeps;
- sweep candidates hashed; and
- findings.

Do not print planned action lines in a blocked run. Findings precede the
`GC blocked` summary using the milestone-3 stable order.

## Documentation Decisions

Update `README.md` with:

- the GC invocation and exit statuses;
- source-record reachability and the fact that missing source files do not
  remove references;
- the committed-state interpretation of the two-run boundary;
- import and GC resurrection;
- full candidate hashing and potentially high I/O cost;
- orphan and integrity blocking behavior;
- interrupted-sweep recovery and non-atomicity;
- the unenforced quiescence requirement;
- logical, not physical, meaning of “bytes reclaimed”;
- empty shard retention; and
- the recommendation to rerun `build-tree` after GC because a previously
  materialized tree is not a reachability root and may contain stale links.

Do not add repair instructions that bypass catalog/CAS invariants. It is
reasonable to advise rerunning the import that created an orphan when the
source remains available, followed by `audit`, before retrying GC.

Update current milestone-status text in `README.md`. Do not rewrite historical
milestone guides that correctly describe earlier command surfaces.

## Sequenced Implementation

### 1. Protect Existing Behavior

- Run the current format, lint, and test suite.
- Add focused regression assertions for existing command help and exit codes if
  coverage is insufficient.
- Do not mix unrelated cleanup into the shared audit/store refactor.

### 2. Extract Shared Integrity And CAS Inspection

- Move only behavior-neutral finding identity, sorting, escaping, and checked
  growth out of `audit`.
- Move strict CAS layout classification, walker, and safe-open behavior behind
  store/path-focused interfaces.
- Add an explicit direct dependency for the selected platform API and replace
  the hard-coded no-follow flag with `libc::O_NOFOLLOW` or its equivalent; do
  not rely on a transitive dependency or another numeric literal.
- Preserve audit's public report and output byte-for-byte except where an
  existing platform-safety test proves the literal flag was wrong.
- Keep audit hashing every valid CAS blob; GC later selects only sweep
  candidates.

Run audit tests before adding GC behavior.

### 3. Add GC Config And CLI Surface

- Add `GcOptions`, `GcConfig`, Clap arguments, validated existing paths, help
  text, command dispatch, and an initial no-op renderer/outcome skeleton.
- Reuse audit's command-neutral store/blobs/catalog path validation.
- Do not expose milestone non-goal flags.

### 4. Add Import Resurrection

- Add purpose-specific SQL to clear the mark inside the import transaction.
- Add catalog-level and CLI-visible build-tree regression coverage.
- Run the full existing import and materialization suites.

### 5. Add Purpose-Specific GC Catalog Access

- Implement dry-run direct read-only inspection.
- Implement real read-write/no-create/no-follow open and WAL verification.
- Implement the immediate transaction wrapper.
- Add deterministic snapshot and defensive mutation SQL.
- Unit-test raw decoding, reachability, affected-row checks, fixed timestamps,
  and rollback behavior.

### 6. Implement Pure Classification And Report Accounting

- Implement the four-state classifier.
- Implement fixed sorted plan construction.
- Implement checked counters and planned/completed/physical-progress
  distinctions.
- Unit-test report ordering and incomplete accounting without filesystem or
  SQLite fault choreography.

### 7. Implement GC Preflight

- Reconcile strict CAS and catalog indexes.
- Implement the narrow interrupted-sweep exception.
- Compare every cataloged file's size.
- Hash only present sweep candidates with portable no-follow open.
- Record device/inode identity for mutation-time revalidation.
- Return `Blocked` on findings before calling any mutator.

### 8. Implement Apply And Fault Seam

- Add a narrow store deletion interface used by production and a test wrapper.
- Implement revalidation, unlink, directory sync, and already-absent parent
  sync.
- Stage catalog changes through the retained immediate transaction.
- Implement the four mutation-failure branches above.
- Buffer action lines until commit outcome is known.

### 9. Complete Rendering And Documentation

- Render complete, dry-run, blocked, and incomplete reports.
- Map exit codes in the binary without `process::exit`.
- Update README and command help.

### 10. Run Risk-Ordered Tests And Full Verification

- Add integration coverage in `tests/gc_milestone.rs`.
- Re-run all existing command suites after every shared-module refactor.
- Finish with the repository-wide commands at the end of this plan.

## Test Matrix

Prefer tiny real files and real SQLite catalogs. Add a narrow injected store
mutator only for deterministic mutation failures.

### CLI And Validation

- Help exposes exactly `import`, `build-tree`, `audit`, and `gc`.
- GC help states reachability, two committed transitions, candidate hashing,
  dry-run, and quiescence.
- Missing or symlinked store, blobs directory, or DB returns `1`.
- Real GC opens no missing DB and performs no migration.
- Real GC rejects a non-WAL catalog without changing its journal mode.
- Dry-run observes committed WAL data, creates no sidecar, and rejects unsafe
  WAL/SHM state.
- Zero chunk size is rejected.

### Pure Plan And Counters

- Cover all four reachability/mark states.
- A newly marked row cannot enter the same run's sweep vector.
- Plan and action order is stable by kind and hash.
- Checked count and byte overflow becomes an operational error.
- Already-absent candidates contribute zero reclaimable/reclaimed bytes.
- Incomplete accounting distinguishes unlinked bytes from finalized bytes.

### Complete And Dry Runs

- Clean reachable store is a successful no-op.
- First real run marks and does not remove.
- Second invocation sweeps with no time delay.
- Repeated dry-runs return the same plan and preserve durable state.
- Dry-run hashes present prior-mark candidates across multiple chunks.
- Fixed clock value is shared by every new mark.
- Empty shard directories remain.
- Completed sweep is idempotent.

### Resurrection And Reachability

- Marked referenced row is resurrected by GC.
- Reimport atomically resurrects and restores build-tree visibility.
- Replacing one source path eventually collects the old blob.
- Multiple references keep the blob reachable until every catalog reference is
  replaced or otherwise removed through fixture-supported valid operations.
- Missing files on a source disk alone do not remove catalog reachability.

### Blocking Findings

- Orphan, malformed entry, symlink, non-regular entry, missing live/newly
  markable file, and size mismatch block all application-row and CAS mutation.
- Candidate hash mismatch and candidate open/read failure block all mutation.
- Corrupt same-size non-candidate is not hashed by GC.
- Catalog integrity, schema, foreign-key, and invalid-row findings block all
  mutation.
- Findings remain deterministically ordered after the shared audit refactor.

### Interrupted Sweep

- Prior-marked unreachable missing file finalizes only the catalog row.
- It renders `state=already-absent`.
- It counts zero bytes reclaimed.
- Missing referenced or unmarked file remains a finding.
- Missing candidate with a non-regular canonical object is not treated as
  interrupted.

### Mutation Failures

With multiple sorted sweep candidates, inject failure at:

- pre-unlink revalidation;
- unlink;
- directory sync;
- defensive catalog delete where practical; and
- final commit through focused catalog tests where practical.

Prove:

- processing stops at the first failure;
- later files and rows remain untouched;
- earlier removed files are reconciled when commit succeeds;
- new marks/resurrections commit on a later filesystem failure when SQLite
  remains usable;
- physical-only progress is not rendered as a completed sweep;
- incomplete report precedes the operational error;
- the next GC invocation completes interrupted candidates; and
- a mark committed during an exit-`1` invocation is sweepable on the next
  invocation because it predates that run.

### Platform Safety

On Unix:

- a final-component symlink cannot be opened as a candidate;
- Linux and macOS builds use the platform `O_NOFOLLOW`, not a numeric literal;
- same-size path replacement with different device/inode fails revalidation;
- deletion never uses a discovered raw path;
- containing-directory sync is attempted after unlink; and
- sync failure prevents the catalog delete for that candidate.

Do not attempt flaky live adversarial race tests. Test the deterministic
identity comparison and injected failure behavior, while documenting the
remaining quiescence boundary.

## Failure Classification Matrix

Return outer `Err` and exit `1` without a GC summary when no useful report can
be constructed, including:

- CLI/path validation failure;
- unsupported schema version;
- real catalog not already in WAL mode;
- database open or immediate-transaction failure;
- unsafe dry-run WAL/SHM state;
- SQLite corruption or stepping failure that prevents required enumeration;
- checked allocation/counter failure before a report exists.

Return `Blocked(report)` and exit `2` when preflight completes with one or more
typed integrity findings, including root or nested CAS enumeration failures. No
application-row or CAS mutation may have occurred.

Return `Incomplete { report, error }` and exit `1` after mutation has begun but
could not complete, including:

- mark/resurrection affected-row mismatch;
- pre-unlink candidate revalidation failure;
- unlink or directory-sync failure;
- sweep affected-row mismatch or SQLite statement failure; or
- commit failure.

Return `Complete(report)` and exit `0` for:

- clean no-op;
- mark-only plan;
- resurrection-only plan;
- one or more finalized sweeps;
- mixed successful actions; and
- any clean dry-run.

## Review Checklist

Before considering milestone 4 complete:

- The plan is fixed from run-start catalog state.
- A current-run mark cannot be swept in the same invocation.
- Real GC uses read-write/no-create/no-follow and retains one immediate
  transaction.
- Dry-run uses direct WAL-aware read-only access and creates no sidecars or
  backups.
- No application mutation occurs before every preflight finding is collected.
- Only present sweep candidates are hashed; orphans and non-candidates are not.
- The safe-open flag is portable across Linux and macOS.
- Revalidation compares canonical path, type, size, device, and inode.
- File removal precedes directory sync, catalog delete, and commit.
- Empty shard directories remain.
- Every catalog mutation uses a defensive predicate and checks one affected
  row.
- Import resurrection and source observation commit atomically.
- Partial reports distinguish planned, committed, and physical-only progress.
- A failed commit does not produce speculative completed action lines.
- Findings/actions/output order is deterministic.
- Existing import, build-tree, and audit behavior remains intact.
- README documents source-record retention, stale browse-tree risk, logical byte
  accounting, quiescence, and orphan blocking.

## Verification Commands

Run from the project root inside `nix develop` or the active direnv shell:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
prek run --all-files
```

The implementation is complete only when all commands pass and the acceptance
requirements in `IMPLEMENTATION-MILESTONE4.md` are represented by tests or
deliberately covered by equivalent higher-level behavior tests.
