# Rust Implementation Guide: Milestone 11

This guide records implementation decisions for milestone 11. `SPEC.md` remains
the source of product intent. Milestone 11 closes the remaining recovery,
architecture, platform-scope, and verification gaps after the feature work in
milestones 6 through 10.

The Python implementation is deprecated historical context and is not a
behavior oracle.

## Companion Instructions

- `RUST.instructions.md`
- `RUST-ARCHITECTURE.instructions.md`
- `RUST-TESTING.instructions.md`
- `SQLITE.instructions.md`

## Milestone 11: Final Spec Conformance And Resilience

This milestone adds no command and no catalog feature. It has four bounded
outcomes:

1. reconcile staging recovery with read-only command guarantees;
2. finish the functional-core/imperative-shell hashing boundary;
3. enforce the Linux/macOS-only platform contract; and
4. add repeatable performance, I/O, checkpoint, and crash-recovery verification.

Do not broaden this milestone into unrelated repair, quarantine, retention, or
media-analysis work.

## Staging Recovery Contract

Interpret `SPEC.md`'s purge-on-startup rule in combination with the established
dry-run/read-only contract:

- every real command holding an exclusive store lock (`import`, `build-tree`,
  and `gc`) inspects and purges stale store staging entries before its command
  body;
- `audit` and all dry runs never purge or create staging because they promise no
  durable mutation;
- read-only commands may emit a structured `stale_staging_detected` event or
  finding-like operational notice, but stale staging is not a CAS integrity
  finding and must not change exit status by itself;
- missing staging is an empty state for commands that do not need it;
- real import creates the application-owned staging directory when absent;
- real build-tree and GC do not create staging solely to purge it;
- cleanup occurs only after acquiring the exclusive canonical store lock;
- cleanup must never follow symlinks or escape the validated staging root.

Rename `prepare_for_import` into responsibility-focused store operations such as
`prepare_staging_for_import` and `purge_existing_staging`. Centralize traversal,
entry classification, and deletion in `store`; command modules choose only
whether their contract permits cleanup.

If cleanup fails, stop before catalog, CAS, GC, or browse-tree mutation and
return a contextual operational error.

## Hashing And I/O Boundary

Remove `std::fs` path opening from the hashing core. The hashing module should
own BLAKE3 state and deterministic chunk processing; store/audit/GC edges own
file opening, no-follow flags, metadata identity, reading, staging writes, and
filesystem errors.

A suitable split is:

```rust
pub struct BlobHasher {
    hasher: blake3::Hasher,
    size_bytes: u64,
}

impl BlobHasher {
    pub fn new() -> Self;
    pub fn update(&mut self, bytes: &[u8]) -> Result<()>;
    pub fn finish(self) -> Result<HashResult>;
}
```

An internal generic stream helper may accept `Read`/`Write`, but it must not
open paths or know store layout. Keep the source-to-staging loop at the store
edge so hashing and writing remain one pass. Audit and GC may reuse a read-only
stream adapter over already-open, no-follow blob handles.

Pure tests cover digest/length behavior with byte slices and synthetic readers.
Filesystem integration tests remain at store/audit/GC boundaries.

## Supported-Platform Enforcement

The product supports Linux and macOS only. Make this explicit at compile time:

```rust
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("media-importer supports Linux and macOS only");
```

Replace broad `#[cfg(unix)]`/`#[cfg(not(unix))]` fallbacks with the supported
target set where platform code is required. Remove non-Unix permission,
symlink, file-opening, and locking fallbacks that cannot be exercised on a
supported platform.

Do not add Windows CI or Windows path-prefix behavior. Remove tests whose only
purpose is rejecting Windows drive syntax. Continue rejecting absolute,
parent-traversing, empty, non-UTF-8-at-persistence-boundary, or otherwise unsafe
source-relative paths according to supported-platform semantics.

Keep Linux and macOS differences localized behind narrow platform helpers, for
example mount identity extraction or no-follow directory opening.

## Spec Conformance Matrix

Add a maintained table to this document or a nearby authoritative Rust rewrite
document mapping every `SPEC.md` requirement to implementation and tests. At
minimum cover:

- metadata-fast repeated import;
- one-pass source read and page-cache interpretation;
- BLAKE3 and two-level sharding;
- SQLite blob/source/relationship integrity;
- mark-and-sweep and offline materialization;
- mount-aware configurable workers;
- staging install/order/cleanup;
- WAL/NORMAL, single writer, batching, and passive checkpoints;
- four-command CLI;
- TTY and non-TTY reporting;
- functional boundaries and immutable 0444 blobs; and
- integration-first/probe-based testing.

Each entry should name at least one behavior-level test. Do not mark a
requirement complete based only on a unit test of an internal helper.

## Performance And I/O Verification

Add an ignored or separately invoked large-fixture test target so normal
pre-commit remains fast. It must use generated sparse/small-repeat fixtures or a
configurable external fixture and report measured facts rather than enforce
machine-specific throughput numbers.

Required scenarios:

- first import reports source bytes read equal to total hashed source bytes;
- unchanged second import reports zero source-content bytes read;
- `--no-metadata-skip` reports a full read on the second import;
- one worker per mount never overlaps reads on that mount;
- configured SSD-style parallelism reaches more than one active reader in a
  deterministic injected test;
- queues remain bounded with a slow store or writer;
- many import records produce multiple batches and passive checkpoints;
- a long run does not allow the WAL to grow without checkpoint attempts; and
- JSON Lines remains parseable and the terminal renderer remains responsive
  under sustained events.

Do not assert that an 8TB fixture completes in a literal number of minutes in
CI. The acceptance proof is zero unchanged content bytes plus bounded metadata
work; optional operator benchmarks may record elapsed time.

## Crash And Fault-Injection Campaign

Use the established probe approach and subprocesses to terminate or fail at
observable boundaries:

- after staging creation;
- during source copy;
- after CAS install but before catalog submission;
- after catalog submission but before batch commit;
- after batch commit but before checkpoint;
- during passive checkpoint;
- while multiple mount workers are active;
- during GC after unlink and before catalog sweep commit;
- during build-tree replacement; and
- during reporting shutdown/broken pipe.

For every boundary, specify and test the durable state allowed immediately after
failure and the recovery behavior of the next appropriate real command. Verify:

- stale staging is purged by the next exclusive real command;
- catalog rows never reference staging-only files;
- installed orphan blobs are reported/adoptable according to existing policy;
- committed batches remain valid;
- uncommitted batches are absent;
- WAL recovery and later passive checkpoints are safe;
- no process lock or thread remains stuck;
- GC interrupted-sweep recovery remains correct; and
- dry-run/audit never repair or clean the failed state.

Keep destructive subprocess fixtures inside isolated temporary directories.

## Security And Filesystem Regression Checks

While moving I/O boundaries, preserve or strengthen:

- no-follow opens for CAS and lock targets;
- source/store/database/browse-tree overlap rejection;
- canonical typed CAS path construction;
- regular-file and size validation;
- source-before/after mutation detection;
- 0444 Unix blob mode after create and reuse;
- refusal to treat malformed CAS entries as blobs;
- deterministic escaped output for unsafe filesystem bytes; and
- exclusive lock lifetime through all thread joins and final mutations.

Do not silently weaken these controls to simplify generic stream APIs.

## Testing Requirements

Normal workspace tests must prove:

- each exclusive real command purges pre-existing stale staging after locking;
- every read-only/dry-run command preserves the same staging bytes and metadata;
- symlinked or escaping staging entries are not followed;
- cleanup failure prevents later mutation;
- hashing core tests require no filesystem;
- store/audit/GC still produce identical BLAKE3 and byte counts;
- all blobs remain mode `0444` on Linux and macOS;
- unsupported targets fail intentionally rather than compile fallback behavior;
- the conformance matrix references existing test names/files;
- every fault-injection subprocess terminates and releases locks;
- recovery reruns reach a clean, auditable state where the established policy
  permits it.

The larger performance/fault campaign may be ignored by default only when a
documented command runs it in CI or release qualification. Deterministic small
fault tests that finish quickly should remain in the normal workspace suite.

## Documentation Updates

Update README and authoritative Rust docs to state:

- supported targets are Linux and macOS only;
- unchanged-import metadata fidelity and opt-out behavior;
- mount-worker tuning and the one-pass/page-cache interpretation;
- writer batching/checkpoint behavior;
- relationship-aware GC reachability;
- TTY versus JSON Lines output;
- which commands purge staging and why read-only commands do not; and
- how to run the extended conformance/fault campaign.

Remove milestone-5 future-work bullets that are now implemented. Do not claim
literal physical throughput or disk-read guarantees beyond what the probes
measure.

## Recommended Implementation Sequence

1. Centralize safe staging inspection/purge and integrate exclusive commands.
2. Refactor hashing state away from filesystem opening.
3. enforce supported targets and delete non-target fallbacks/tests.
4. Add the spec conformance matrix.
5. Add deterministic byte/concurrency/checkpoint probes.
6. Add crash subprocess boundaries and recovery assertions.
7. Add the extended test command and operator documentation.
8. Run formatting, clippy, workspace tests, extended tests, and all hooks on
   Linux and macOS.

## Explicit Non-Goals

- Windows support;
- direct I/O or cache-control syscalls;
- automatic repair or quarantine;
- source-record deletion policy or retention periods;
- automatic media metadata extraction;
- distributed coordination;
- exact physical-space accounting on ZFS; or
- machine-independent throughput thresholds.

## Definition Of Done

Milestone 11 is complete when every `SPEC.md` requirement has an evidence-backed
conformance-matrix entry, exclusive commands safely recover stale staging,
read-only commands remain mutation-free, hashing core no longer opens files,
unsupported platforms fail intentionally, the extended I/O/fault campaign
passes on Linux and macOS, and all standard repository checks pass.
