# Milestone 4 Completion Handover

## Objective

Finish milestone 4 so that the implementation satisfies
`IMPLEMENTATION-MILESTONE4.md`, the resolved safety model in
`EXECUTION-PLAN-MILESTONE4.md`, and the repository Rust, architecture, testing,
and SQLite instructions.

The GC implementation is substantial and its current checks pass. Do not
rewrite it. Close the remaining contract, accounting, and acceptance-coverage
gaps with focused changes.

## Authority And Scope

Use this precedence order:

1. `IMPLEMENTATION-MILESTONE4.md`
2. `EXECUTION-PLAN-MILESTONE4.md` for choices left open by the milestone guide
3. The repository-level Rust, architecture, testing, and SQLite instructions

The Python implementation is deprecated historical context and is not a
behavior oracle.

Keep this work limited to milestone 4 completion:

- resolve the GC parse-status ambiguity explicitly;
- make all GC counters and post-mutation failures follow the checked,
  partial-report model;
- add the missing behavior-level acceptance coverage;
- add only the narrow fault seams needed for deterministic failure tests; and
- update current documentation where the resolved behavior changes or needs
  clarification.

Do not add schema changes, source-management commands, repair mode, run locks,
parallel hashing, JSON output, shard pruning, or other future-milestone work.

## Current Baseline

At handover time:

- `gc` is exposed through the CLI;
- fixed run-start classification, two-run mark/sweep, GC resurrection, import
  resurrection, strict CAS preflight, candidate hashing, dry-run, interrupted
  sweep recovery, and file-first deletion are implemented;
- complete, blocked, and incomplete report shapes exist;
- the repository documentation describes GC;
- `cargo fmt --all --check` passes;
- `cargo clippy --workspace --all-targets -- -D warnings` passes;
- `cargo test --workspace` passes with 60 tests; and
- `prek run --all-files` passes.

Preserve these behaviors and keep existing `import`, `build-tree`, and `audit`
output and exit behavior unchanged.

The relevant implementation is concentrated in:

- `crates/media-importer/src/gc.rs`
- `crates/media-importer/src/catalog.rs`
- `crates/media-importer/src/store.rs`
- `crates/media-importer/src/cli.rs`
- `crates/media-importer/src/config.rs`
- `crates/media-importer/src/main.rs`
- `crates/media-importer/tests/gc_milestone.rs`

## Shortcomings To Close

### 1. Resolve GC Invalid-Value Exit Status

The milestone guide reserves exit `1` for invalid configuration and exit `2`
for a completed safety preflight with findings. The execution plan also asks
for Clap `NonZeroUsize` parsing, which makes Clap reject
`gc --chunk-size 0` before dispatch with exit `2`.

Use the milestone guide's precedence and make the GC behavior unambiguous:

- keep `GcConfig.chunk_size` as `NonZeroUsize`;
- parse the GC CLI value into an ordinary `usize`;
- convert it to `NonZeroUsize` during `GcOptions -> GcConfig` validation;
- return the resulting invalid-configuration error through the normal
  operational path with exit `1`; and
- add a CLI integration test that asserts exit `1`, stderr context, no GC
  summary, and no durable mutation for `gc --chunk-size 0`.

Do not change parsing or exit behavior for the other commands in this
milestone-completion change. Update the execution plan's statement about GC
using Clap `NonZeroUsize` parsing so the two authoritative documents no longer
conflict.

If maintainers choose standard Clap exit `2` instead, stop and record that
decision by updating both milestone documents before changing code. Do not
leave the current ambiguity implicit.

### 2. Use Checked GC Progress Arithmetic Everywhere

Replace the unchecked increments of staged marks, resurrections, and sweeps
with checked operations carrying useful counter-specific context.

Audit all `GcReport`, `StagedProgress`, plan, action, finding, and byte counters.
The desired rule is:

- checked conversion from collection lengths to report counters;
- checked addition for counts and byte totals; and
- no overflow error may bypass the correct `GcOutcome` after mutation begins.

In particular, the current apply path can return an outer `Err` from checked
physical-progress or reclaimed-byte arithmetic after a CAS file has already
been removed. That loses the required incomplete report.

Restructure apply-time accounting so that:

1. counter transitions are checked before the corresponding irreversible
   mutation where possible;
2. values are assigned only after the operation they describe succeeds;
3. any arithmetic failure before the first mutation is an outer operational
   error or a rollback-only incomplete outcome, as appropriate;
4. any arithmetic failure after an earlier filesystem mutation commits or
   reconciles earlier progress as far as SQLite permits and returns
   `GcOutcome::Incomplete`; and
5. no action is reported as completed unless its catalog effect is known to
   have committed.

The preflight already computes checked total reclaimable bytes. Use that fact
to simplify or prove safe the partial-byte transitions, but retain explicit
checked arithmetic as required by the contract.

Add focused unit tests for checked report/progress transitions. Do not attempt
to create multi-exabyte files. Test the pure accounting boundary directly with
near-`u64::MAX` values.

### 3. Complete The Acceptance Matrix

Extend behavior-level coverage in `tests/gc_milestone.rs`. Reuse tiny real
files and real SQLite catalogs. Direct SQLite fixture manipulation is allowed,
but assert observable CLI, catalog, and filesystem outcomes rather than
private statement order.

#### CLI And Existing-Path Validation

Cover:

- missing store;
- symlinked store on Unix;
- missing blobs directory;
- symlinked blobs directory on Unix;
- missing catalog;
- symlinked catalog on Unix;
- uninitialized, older, and newer schema versions;
- a structurally invalid but enumerable schema;
- real GC refusing to create or migrate a missing/uninitialized catalog;
- `gc --chunk-size 0` using the resolved exit status; and
- the absence of milestone non-goal flags in `gc --help`.

For failure cases, assert that no application row or CAS blob changes.

#### Dry-Run Catalog Policy

Cover GC directly, even where audit already tests the shared reader:

- committed data in a live, usable WAL is observed by the GC plan;
- a non-empty WAL without usable SHM fails operationally;
- no WAL or SHM is created when neither existed;
- database, existing sidecar, blob, permission, and modification-time state is
  unchanged; and
- repeated dry-runs produce the same action plan.

Do not assert access-time stability.

#### Reachability And Resurrection

Cover:

- a clean referenced store as an exact successful no-op;
- deleting the original source file does not remove catalog reachability;
- replacing one source path's content leaves the old blob unreachable, after
  which two GC invocations collect only the old blob;
- two source records keep one blob reachable until both references are removed
  or replaced;
- a marked referenced row is resurrected without hashing or sweeping;
- re-import resurrection uses the expected fixed atomic catalog behavior; and
- resurrected content is visible to `build-tree`.

The existing resurrection test can remain, but add the missing source
replacement and multiple-reference lifecycle tests.

#### Blocking Findings And No-Mutation Proof

For each representative class below, take a durable-state snapshot before GC,
assert exit `2` and the stable finding category, then prove all planned marks,
resurrections, CAS files, and catalog rows remained unchanged:

- orphan canonical blob;
- malformed CAS entry;
- symlinked CAS entry on Unix;
- non-regular object at an expected blob path;
- missing referenced blob;
- missing unmarked/unreachable blob;
- catalog/CAS size mismatch;
- sweep-candidate hash mismatch;
- candidate open/read failure where it can be exercised reliably;
- catalog integrity or foreign-key finding;
- semantic-schema finding; and
- invalid blob or source row.

Shared audit tests demonstrate the shared inspectors, but they do not by
themselves prove that GC blocks every mutation path. Table-driven GC tests are
preferred to duplicating large fixtures.

Also prove that:

- a missing marked and unreachable blob is the only missing-file state treated
  as an interrupted sweep; and
- a non-regular canonical object is never treated as already absent.

#### Ordering, Counters, And Fixed Clock

Cover:

- multiple mark, resurrection, and sweep actions ordered by kind and full hash;
- multiple sweep candidates hashed and applied in hash order;
- one injected fixed clock value persisted on every new mark;
- dry-run, complete, blocked, and incomplete summaries with exact planned,
  completed, physical, and byte counters;
- present versus already-absent candidates contributing the correct byte
  totals; and
- idempotence after a completed sweep.

Do not merely assert that mark timestamps are equal. Assert the exact injected
clock value through a testable internal GC entry point.

### 4. Expand Deterministic Mutation-Failure Coverage

Keep the production public interface deep and narrow. Add or refine only
crate-private/test-only seams needed to exercise these branches:

- failure before unlink during candidate revalidation;
- unlink failure;
- directory-sync failure after unlink;
- defensive catalog-delete failure where practical;
- final commit failure where practical; and
- failure on a later candidate after earlier marks, resurrections, and sweeps
  have been staged.

For each applicable branch, prove:

- processing stops at the first failure;
- later candidates remain untouched;
- catalog rows for earlier removed files are committed when SQLite remains
  usable;
- marks and resurrections commit on a later filesystem failure;
- a file removed without a committed row deletion is represented only as
  physical progress, not a completed `SWEEP` action;
- the incomplete report is rendered before the operational error;
- exit status is `1`; and
- a subsequent real GC completes recovery from authoritative catalog and CAS
  state.

The existing second-removal fault test is a useful base, but it does not cover
all distinct state transitions above.

Prefer small injected operations over a generic filesystem or database
framework. Suitable approaches include:

- a store mutator used by GC tests that can remove a file and then return a
  sync-like error;
- focused store unit tests for identity revalidation and `O_NOFOLLOW`; and
- a narrow catalog transaction fault hook compiled only for tests if a real
  SQLite failure cannot be triggered without corrupting preflight.

Do not weaken schema validation or add persistent triggers solely to force a
mutation error.

### 5. Add Platform-Safety Unit Coverage

On Unix, add focused tests proving:

- `open_blob_no_follow` refuses a final-component symlink;
- replacing a candidate path with a same-size, different-inode file causes
  revalidation to fail and preserves the replacement;
- removal accepts only a typed hash and derives the canonical path internally;
- a successful unlink attempts containing-directory sync; and
- a sync failure prevents staging that candidate's catalog deletion.

Avoid flaky concurrent race tests. The quiescence boundary remains a documented
precondition.

## Recommended Work Sequence

### Phase 1: Lock The Contract

1. Add the failing GC zero-chunk exit-status test.
2. Resolve the parsing/documentation ambiguity as described above.
3. Add a short traceability checklist in the test module or this handover
   document showing which test covers each milestone acceptance bullet.

Run the focused GC tests.

### Phase 2: Fix Accounting Before Adding Fault Tests

1. Introduce small checked progress helpers.
2. Remove unchecked GC increments.
3. Ensure every post-mutation failure returns `Incomplete` with the best
   trustworthy report.
4. Add pure overflow/accounting tests.

Run unit tests, GC integration tests, Clippy, and formatting.

### Phase 3: Close Core Behavior Coverage

Add reachability, dry-run WAL, blocking-finding, ordering, and exact-clock tests
in risk order. Keep fixtures small and prefer table-driven helpers.

Run `gc_milestone`, `audit_milestone`, and `import_milestone` after each shared
fixture or inspector change.

### Phase 4: Close Failure And Platform Coverage

Add the narrow store/catalog fault seams and deterministic recovery tests.
Verify incomplete CLI rendering and exit status through at least one
end-to-end test; use deep-module tests for failure points that cannot be
reliably produced through the CLI.

### Phase 5: Final Documentation And Verification

Update:

- `IMPLEMENTATION-MILESTONE4.md` only if an explicit contract ambiguity was
  resolved;
- `EXECUTION-PLAN-MILESTONE4.md` so its resolved choices match the code;
- `README.md` if observable exit behavior or recovery wording changed; and
- this handover document or an equivalent traceability note with final test
  names.

Do not rewrite historical milestone contracts unrelated to the ambiguity.

## Definition Of Done

Milestone 4 may be called complete only when:

- the GC exit-status contract has one documented interpretation and matching
  integration tests;
- every GC counter transition uses checked arithmetic;
- no failure after filesystem mutation can bypass an incomplete report;
- the milestone acceptance bullets are each represented by a named test or a
  documented, genuinely equivalent higher-level test;
- mutation-failure and recovery behavior is covered at the distinct
  filesystem/catalog boundaries;
- platform no-follow and identity-revalidation behavior has deterministic
  coverage;
- existing `import`, `build-tree`, and `audit` behavior remains unchanged; and
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
- the resolved exit-status decision;
- acceptance tests added, grouped by milestone requirement;
- any requirement covered by an equivalent existing test and why it is
  equivalent;
- verification command results; and
- remaining known limitations, if any.
