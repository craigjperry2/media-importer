# Rust Implementation Guide: Milestone 5

This guide records implementation decisions for milestone 5 of the Rust rewrite
of `media-importer`. The source of product intent is `SPEC.md`; this file
captures the concrete choices a coding agent should follow while implementing
the next vertical slice.

Milestone 1 implemented directory import into the content-addressed store (CAS)
and catalog. Milestone 2 added browse-tree materialization. Milestone 3 added a
read-only integrity audit. Milestone 4 added explicit, two-run mark-and-sweep
garbage collection. Milestone 5 adds cooperative single-node process
coordination around every command.

The existing Python implementation is deprecated historical context. Do not
port it file-by-file and do not use it as a behavior oracle.

## Companion Instructions

Use these colocated instruction files when implementing this guide:

- `RUST.instructions.md`
- `RUST-ARCHITECTURE.instructions.md`
- `RUST-TESTING.instructions.md`
- `SQLITE.instructions.md`

## Milestone 5: Single-Node Store Coordination

Implement one cross-command vertical slice using a kernel-managed advisory lock
on the canonical store-root directory.

Every command operating on an existing store participates for its complete
behavior-level run:

```text
command                 mode
----------------------  ---------
import                   exclusive
import --dry-run         shared, when the store already exists
build-tree               exclusive
build-tree --dry-run     shared
audit                    shared
gc                       exclusive
gc --dry-run             shared
```

An `import --dry-run` whose store does not yet exist is the single exception. It
runs without a lock and continues to treat the absent store and catalog as
empty. It must not create the store merely to establish coordination.

This milestone is intentionally a cooperative, single-node safeguard against
accidentally overlapping CLI invocations. It is not distributed locking,
leader election, a durable lease system, or a defence against non-cooperating
processes.

Do not add a command or option. Preserve the existing narrow CLI surface:
`import`, `build-tree`, `audit`, and `gc`.

## Resolved Scope Decisions

The following choices are part of the milestone contract:

- use the canonical `STORE_ROOT` directory itself as the coordination inode;
- use the operating system's blocking shared/exclusive file-lock facility;
- hold the opened directory handle for the complete command operation;
- allow multiple shared readers concurrently;
- allow only one exclusive command and never overlap it with a shared command;
- wait indefinitely for a conflicting cooperating command to finish;
- release automatically when the owning handle closes or the process exits;
- acquire locks in the deep behavior APIs, not only in CLI dispatch;
- make no catalog schema change and create no lock file;
- provide no timeout, polling loop, heartbeat, owner token, stale-lock
  detection, manual unlock, force, or no-wait interface;
- coordinate only commands using the same canonical store root; and
- leave cross-store sharing of an external catalog or browse-tree path
  unsupported and uncoordinated.

These decisions replace earlier speculative references to a catalog-backed
`run_locks` table. Milestone 5 does not implement a durable lock record.

## Goals And Safety Invariants

Milestone 5 must preserve these invariants:

- two cooperating exclusive commands for the same store never execute their
  command bodies concurrently;
- a cooperating shared command and exclusive command for the same store never
  execute their command bodies concurrently;
- multiple cooperating shared commands for the same store may run
  concurrently;
- the lock is held before staging cleanup, catalog access, CAS traversal, or
  browse-tree planning and remains held through the last command side effect;
- failure or process exit cannot leave a stale lock that blocks later runs;
- readers and dry runs do not create a lock artifact or otherwise weaken their
  established durable no-mutation contracts;
- path aliases that resolve to the same store directory coordinate on the same
  inode;
- a new real import performs only the unavoidable creation of its store-root
  directory before acquiring the exclusive lock;
- command output, exit status, reports, and domain behavior remain unchanged
  except for waiting and lock-acquisition failures; and
- lock-acquisition failures are contextual operational errors, never integrity
  findings or successful reports.

The lock is advisory. Manual filesystem edits, direct SQLite clients, older
versions of the application, and other software that does not acquire the same
lock remain outside the guarantee.

## Coordination Model

### Shared Mode

Shared mode applies to commands that promise no durable edits:

- `audit`;
- `import --dry-run` when the store exists;
- `build-tree --dry-run`; and
- `gc --dry-run`.

Any number of shared holders may coexist. A shared holder blocks a later
exclusive acquisition until every shared holder releases its handle. Shared
acquisition also waits behind an existing exclusive holder.

Taking a shared kernel lock is coordination, not a durable edit. It must not
create or modify a file, directory, catalog, WAL, SHM, staging entry,
permission, application timestamp, or browse-tree entry.

### Exclusive Mode

Exclusive mode applies to every command that edits durable application state:

- real `import`;
- real `build-tree`; and
- real `gc`.

Only one exclusive holder may exist for a store, and it excludes all shared
holders. Classify real `build-tree` as exclusive even though it only reads the
catalog and CAS: it edits the browse tree, and its plan should not become stale
behind a concurrent import or GC using the same store.

### Missing-Store Dry-Run Exception

`import --dry-run` currently supports a store that does not exist and must
continue creating nothing. Since no store-directory inode exists, this case
does not acquire a shared lock.

The behavior is:

1. retain existing configuration and overlap validation;
2. observe that the store root does not exist;
3. trace that coordination was skipped for a missing-store dry run;
4. treat the store and catalog as empty using the existing dry-run behavior;
5. perform no durable mutation.

A real import may create the store concurrently with this exceptional dry run.
The resulting dry-run report is not promised to reflect that concurrently
created state. Do not add parent-directory locking or create a placeholder
store to close this narrow bootstrap race.

## Lock Identity And Filesystem Primitive

Open the validated canonical `StoreRoot` directory and apply a blocking
kernel-managed advisory lock to that open directory handle.

Use the stable standard-library APIs supplied by the pinned Rust toolchain:

- `std::fs::File::lock_shared()` for shared acquisition;
- `std::fs::File::lock()` for exclusive acquisition; and
- ownership of the `File` handle for RAII release.

Do not add a locking crate and do not call `libc::flock` directly unless a
future pinned toolchain change removes the required stable API. The current
project toolchain supports the standard API on both target operating-system
families.

The opened handle should represent a real directory and must not follow a
last-component symlink. Reuse the repository's Unix no-follow posture where
needed. Existing configuration canonicalizes store identity, and commands that
require a pre-existing store reject a symlink at their input boundary. Lock
acquisition should still fail with useful context if the canonical path no
longer opens as the expected directory. Ensure the descriptor does not leak
across `exec`.

Linux and macOS are the only supported platforms. Network filesystems,
distributed hosts, Windows locking semantics, and container namespaces are not
part of this milestone's contract.

## Lock Type And Deep Interface

Add a focused module such as `run_lock` or `coordination`. Prefer a narrow API
similar to:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

#[must_use = "the lock is released when its guard is dropped"]
pub struct StoreRunLock {
    directory: std::fs::File,
}

impl StoreRunLock {
    pub fn acquire(
        store_root: &StoreRoot,
        mode: LockMode,
    ) -> color_eyre::Result<Self>;
}
```

Exact names may differ. Preserve these properties:

- construction opens the canonical store directory and blocks until the
  requested lock is acquired;
- the guard owns the only directory handle needed to retain that acquisition;
- dropping the guard closes the handle and releases the lock;
- callers cannot construct a guard that does not hold the requested lock;
- callers do not receive the raw descriptor or choreograph unlock calls;
- the module performs no printing and knows nothing about command reports; and
- the module does not create the store directory.

Do not implement lock upgrade or downgrade. Every command knows its fixed mode
before acquisition.

Prefer release by closing the owned handle rather than explicit unlock before
drop. This ensures early `return`, `?`, panic unwinding, and normal process
termination all follow the same ownership model. The operating system releases
the lock after abnormal process termination as well; no stale-lock cleanup is
required.

## Acquisition Timing And Lifetime

Configuration parsing and path validation remain CLI-facing and occur before
the deep API is called. Lock as the first operation inside each deep command
API once the store directory exists.

The intended boundary is:

```text
parse CLI
  -> validate config and canonicalize store identity
  -> create store root only for a real import when absent
  -> acquire shared or exclusive store lock
  -> perform the complete command operation
  -> construct final outcome/report
  -> drop the guard
  -> render in CLI code
```

Rendering may occur after release because it does not inspect or mutate store
state. The report must be fully constructed before release.

The lock must cover:

- import staging-directory creation and purge;
- source scanning for import;
- source hashing and CAS staging/install;
- import catalog initialization, migration, resurrection, and observations;
- complete build-tree planning and application;
- complete audit catalog/CAS inspection and hashing;
- complete GC snapshot, preflight, hashing, mutation, commit, and incomplete
  outcome construction; and
- temporary catalog backup creation and use during an existing-store import
  dry run.

Do not acquire only around SQLite writes or individual CAS mutations. The point
is to prevent a command's cross-filesystem plan from becoming stale during the
operation.

## New-Store Import Bootstrap

A real import may name a store root that does not exist. Its sequence must be:

1. run the existing configuration, source/store overlap, explicit DB, and
   parent validation;
2. create only the configured store-root directory;
3. open that directory and acquire the exclusive lock;
4. while holding the lock, create or validate `blobs` and `staging`;
5. purge stale staging entries;
6. initialize or open the catalog; and
7. run the existing import behavior.

Two real imports racing to create the same new store may both return
successfully from the idempotent directory-creation step, but they then open
the same directory inode and serialize before any CAS, staging, or catalog
work.

Do not create `blobs`, `staging`, the catalog, a lock file, or other store
contents before exclusive acquisition. If acquiring the new directory lock
fails, return an operational error and leave at most the newly created empty
store-root directory. Do not attempt to remove it, because another process may
already have observed or opened it.

## Command Integration

### Import

Keep `import_source(ImportConfig) -> Result<ImportReport>` as the deep public
operation.

- For a real import, create the root if needed, acquire exclusive, then run the
  existing real import.
- For a dry run with an existing store, acquire shared before opening or
  copying any catalog state, scanning, hashing, or checking CAS presence.
- For a dry run with a missing store, take the documented unlocked exception.
- Keep the guard in the outer orchestration frame so helper returns cannot
  release it early.

Do not move locking into `Store::ingest_file` or individual catalog methods.

### Build Tree

Keep `build_tree(BuildTreeConfig) -> Result<BuildTreeReport>` as the deep public
operation.

- Real build-tree acquires exclusive before loading materialization entries or
  walking the browse tree.
- Dry-run acquires shared before planning.
- Hold through plan construction and, for a real run, every browse-tree edit.

The coordination identity remains the store root. Two commands using different
stores but the same browse-tree path are unsupported and need not coordinate.

### Audit

Keep `audit_store(AuditConfig) -> Result<AuditReport>` as the deep public
operation.

- Acquire shared before opening the catalog, inspecting the CAS, or hashing a
  blob.
- Hold until the complete report, findings, and counters have been finalized.

Replace user documentation that requires cooperating application commands to
remain quiescent manually. Continue warning that non-cooperating external
catalog or filesystem writers remain unsafe.

### Garbage Collection

Keep `collect_garbage(GcConfig) -> Result<GcOutcome>` as the deep public
operation.

- Real GC acquires exclusive before opening its writable catalog connection or
  beginning its immediate transaction.
- GC dry-run acquires shared before its read-only catalog snapshot and CAS
  preflight.
- Retain the guard while constructing `Complete`, `Blocked`, or `Incomplete`.
- Do not let a mutation failure or partial-report path drop the guard before
  the best trustworthy outcome has been assembled.

The SQLite immediate transaction remains required for GC's catalog transaction
semantics. The store lock does not replace transactions, PRAGMAs, affected-row
checks, or recovery ordering.

## Blocking And Observability Contract

Acquisition is blocking and has no application timeout. Do not use a try-lock
polling loop.

Emit structured tracing around acquisition:

- immediately before a potentially blocking request, include the canonical
  store path, command, and requested mode;
- immediately after success, include the same fields and indicate acquisition;
  and
- add useful path and mode context to open or acquisition failures.

Do not print a stable stdout line merely because a command waited. Existing
stdout reports and action ordering must remain unchanged. Tracing continues to
use stderr under the existing subscriber and is opt-in through the established
filter.

There is no promise of waiter ordering or writer fairness beyond the operating
system primitive. Do not build a userspace queue.

If opening or acquiring the lock fails, return a contextual `color_eyre`
operational error. Existing CLI error handling maps it to exit status 1. Do not
render a successful, blocked-integrity, or partial-mutation report when the
command body never started.

## Failure And Recovery Semantics

The directory handle is the lock lifetime. There is no durable ownership state
to repair.

- A normal return releases when the guard drops.
- An early error releases during stack unwinding.
- A panic releases while unwinding when the process is not configured to
  abort.
- Process exit or termination closes the descriptor and releases the kernel
  lock.
- A waiting process then acquires according to normal operating-system
  scheduling.

Do not catch termination signals merely to unlock. Do not write PID files,
timestamps, owner names, or heartbeat rows. Do not inspect whether a previous
process still exists.

Advisory locking does not make existing SQLite/filesystem mutation sequences
globally atomic. Preserve all milestone 1-4 crash-recovery and partial-failure
semantics.

## Module Boundaries

Preserve these responsibilities:

- `cli` parses, validates command-level options, dispatches, renders, and
  selects process exit status;
- `config` constructs validated command configurations and classifies no lock
  behavior;
- `paths` owns canonical `StoreRoot` identity and directory validation;
- `run_lock` or `coordination` owns directory opening, shared/exclusive
  acquisition, tracing at that boundary, and the RAII guard;
- `ingest`, `materialize`, `audit`, and `gc` select their fixed mode and retain
  the guard around their complete behavior;
- `catalog` continues to own SQLite access and must not acquire the store lock;
  and
- `store` continues to own CAS/staging operations and must not acquire the run
  lock internally.

Do not pass a lock guard through scanner, hashing, store, or catalog APIs. Its
lexical lifetime in the command orchestrator should make the protected region
obvious without coupling lower layers to coordination mechanics.

Do not introduce a generic resource-lock graph, async abstraction, global
singleton, daemon, lock manager, or trait hierarchy. One concrete store lock is
the proven requirement.

## Catalog And Schema

Milestone 5 makes no SQLite schema change. Continue using schema version 1.

Do not add:

- `run_locks`, `leases`, or ownership tables;
- lock acquisition or release SQL;
- heartbeat or expiry timestamps;
- new migrations or `user_version` changes; or
- SQLite busy-timeout changes as a substitute for store coordination.

SQLite locking continues to protect SQLite itself. The directory lock protects
cooperating application commands whose correctness spans the catalog, CAS,
staging area, and browse tree.

## Testing Strategy

Use real temporary directories and real processes. File-lock behavior is a
process-coordination contract and must not be proved only with mocks or two
handles in one test process.

Add a focused integration test module such as
`tests/run_lock_milestone.rs`. Build a deterministic child-process harness
rather than relying on large files, sleeps, or hoping a real command remains
busy long enough.

One suitable harness is for the integration-test executable to spawn itself in
a narrowly selected helper-test mode using `std::env::current_exe()`. Coordinate
parent and child readiness through pipes or tiny sentinel files outside the
store. The helper may acquire a real `StoreRunLock`, signal "acquired", wait for
an explicit release signal, and then exit. Keep helper-only behavior under test
configuration; do not expose a production CLI command or binary.

Timeouts are appropriate in the test harness only to fail deterministically
instead of hanging the suite. They are not product lock timeouts.

### Primitive Acceptance Tests

Cover the lock matrix with separate processes:

- shared plus shared both acquire before either releases;
- exclusive blocks a later exclusive until release;
- exclusive blocks a later shared until release;
- shared blocks a later exclusive until release;
- a blocked child proceeds after the holder exits normally;
- a blocked child proceeds after the holder process is forcibly terminated;
- a path alias resolving to the same store coordinates on the same directory
  inode; and
- different store roots do not block one another.

The forced-termination case proves there is no stale durable lock. Keep it
platform-gated to supported Unix behavior and make child cleanup robust.

### Command-Boundary Acceptance Tests

Prove that the public deep operations select and retain the intended modes:

- an exclusive helper lock prevents real import from beginning staging purge,
  CAS creation, or catalog creation until release;
- an exclusive helper lock prevents audit and each existing-store dry run from
  inspecting application state until release;
- a shared helper lock allows audit and dry-run readers to proceed but blocks
  real import, real build-tree, and real GC;
- real build-tree holds exclusive through both planning and application;
- real GC holds exclusive through preflight and mutation/outcome construction;
  and
- early operational errors release the lock so a later command can acquire it.

Use the narrowest deterministic seam necessary to observe "command body has
not begun". Prefer durable-state observations and explicit test probes over
asserting private helper call order.

### No-Mutation And Regression Tests

Preserve and extend existing behavioral coverage:

- `import --dry-run` against a missing store still creates nothing and
  succeeds without a lock;
- shared acquisition on an existing store does not change its directory tree,
  catalog bytes, WAL/SHM presence, file permissions, or application-managed
  timestamps;
- no `.media-importer.lock` or other lock artifact appears;
- a new real import creates only the store root before exclusive acquisition;
- two real imports racing to initialize the same new store serialize and
  converge on valid idempotent state;
- existing import idempotency, build-tree reconciliation, audit findings, GC
  two-run lifecycle, GC recovery, output, and exit statuses remain unchanged;
  and
- all pre-existing dry-run no-durable-mutation assertions continue to pass.

Avoid exact wall-clock timing assertions. Demonstrate blocked/unblocked state
with handshake events and bounded test waits.

## Acceptance Criteria

Milestone 5 is complete only when all of the following are observable:

- all four deep command APIs acquire coordination themselves;
- real import, build-tree, and GC use exclusive mode;
- audit and every existing-store dry run use shared mode;
- missing-store import dry-run remains unlocked and mutation-free;
- conflicting invocations wait rather than fail immediately;
- compatible readers overlap;
- lock ownership spans the complete behavior-level operation;
- normal return, error, and process termination release ownership;
- there is no persistent lock file, database row, or stale-lock workflow;
- same-store path aliases coordinate and distinct stores remain independent;
- acquisition failures provide contextual exit-1 errors;
- stdout/report contracts and exit statuses from milestones 1-4 remain stable;
  and
- Linux and macOS behavior is covered at the appropriate platform boundary.

## Recommended Implementation Sequence

Implement this milestone as one reviewable vertical slice:

1. Add failing interprocess tests for shared/shared, shared/exclusive, and
   exclusive/exclusive behavior on a store directory.
2. Add the focused RAII lock module using the stable standard-library API.
3. Add normal-exit, forced-exit, path-alias, distinct-store, and contextual
   failure tests.
4. Integrate real and dry-run import, including the missing-store exception and
   new-store bootstrap ordering.
5. Integrate build-tree, audit, and GC at their deep public entry points.
6. Add deterministic command-boundary tests proving acquisition happens before
   command work and lasts through outcome construction.
7. Re-run and repair all milestone 1-4 regression and no-mutation tests.
8. Update CLI help and README concurrency documentation.
9. Run the full formatting, lint, test, and hook suite.

Keep the first implementation concrete. Do not generalize beyond the second
lock mode and four current command orchestrators.

## Documentation Updates

Update current user-facing documentation in the same implementation change:

- explain that commands using one store coordinate automatically on a
  single-node advisory lock;
- document the shared/exclusive mode matrix;
- state that conflicting commands wait indefinitely;
- explain that the operating system releases locks after process exit;
- state that no lock file or catalog lock row exists;
- document the unlocked missing-store `import --dry-run` exception;
- replace the current audit and GC instructions that require users to prevent
  overlap between cooperating `media-importer` commands;
- retain warnings about non-cooperating external filesystem and SQLite edits;
  and
- state that separate stores sharing an external catalog or browse-tree path
  are unsupported and uncoordinated.

Update stale future-work lists that specifically promise catalog-backed run
locking. Do not rewrite the historical behavior contracts in completed
milestone guides; this guide supersedes that deferred design choice.

CLI help should mention automatic waiting concisely where concurrency is
currently described. Do not add lock status to normal command summaries.

## Explicit Non-Goals

Milestone 5 does not include:

- distributed, network, database-backed, or cross-host locking;
- coordination across containers that do not share the same kernel lock
  context;
- protection from non-cooperating programs or manual edits;
- coordinating different store roots that share an external catalog or browse
  tree;
- a lock file, PID file, catalog lock table, lease, owner token, or heartbeat;
- stale-lock detection, lock breaking, force unlock, or recovery commands;
- lock acquisition timeout, no-wait mode, progress UI, or waiter queue;
- reader-to-writer upgrade or writer-to-reader downgrade;
- fairness guarantees or starvation prevention beyond the operating system;
- multiple-resource lock ordering or deadlock detection;
- a daemon or global process singleton;
- a catalog schema migration;
- changing GC's SQLite transaction model or filesystem/catalog recovery order;
- making SQLite and filesystem mutations globally atomic;
- new source-record management, repair, quarantine, or adoption behavior;
- metadata-skip optimization;
- mount-point workers, parallel hashing, or a writer thread;
- managed WAL checkpoints;
- JSON or machine-versioned output;
- TTY progress dashboards; or
- Windows support.

## Future Milestones

The planned spec-convergence sequence is now documented:

1. `IMPLEMENTATION-MILESTONE6.md`: metadata-fast idempotent import;
2. `IMPLEMENTATION-MILESTONE7.md`: single catalog writer, batching, and managed
   passive WAL checkpoints;
3. `IMPLEMENTATION-MILESTONE8.md`: bounded mount-aware parallel ingestion;
4. `IMPLEMENTATION-MILESTONE9.md`: semantic blob relationships and
   relationship-aware reachability;
5. `IMPLEMENTATION-MILESTONE10.md`: TTY dashboards and structured non-TTY
   output; and
6. `IMPLEMENTATION-MILESTONE11.md`: staging recovery, architecture/platform
   conformance, and extended I/O/fault verification.

Potential post-spec work remains intentionally unplanned: explicit source-record
management, repair/quarantine, configurable retention, browse-tree audit, and
configurable merge policies.

Revisit the single-node coordination design only if a concrete deployment later
introduces multiple hosts, isolated lock namespaces, or deliberately shared
resources across store roots.

## Definition Of Done

Before milestone 5 is handed off:

- the implementation and test names are traceable to every acceptance criterion
  above;
- the mode matrix and missing-store exception match code, help, and README;
- no reader or dry run creates coordination state;
- no completed milestone's behavioral tests regress;
- no new schema or unused abstraction has been introduced; and
- all required commands pass from the repository root inside the development
  environment:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
prek run --all-files
```

Before handoff, report:

- files changed;
- the final lock module and public deep-API integration points;
- the shared/exclusive command matrix implemented;
- interprocess and command-boundary acceptance tests added;
- regression and no-mutation coverage retained;
- Linux and macOS verification performed;
- full verification command results; and
- remaining limitations, especially advisory-only and same-store-only scope.
