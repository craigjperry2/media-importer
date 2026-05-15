# Rust Implementation Interview

Date: 2026-05-15

Context: planning a fresh Rust implementation of `media-importer` from
`docs/from-scratch-in-rust/SPEC.md`, explicitly ignoring the deprecated Python
implementation as a behavior oracle.

Primary task: write `IMPLEMENTATION.md` to guide a coding agent through the
Rust implementation.

Secondary task: extract reusable Rust, architecture, testing, and SQLite
preferences into colocated instruction files.

## Initial User Prompt

The user asked:

> Interview me relentlessly about every aspect of this plan until we reach a
> shared understanding. Walk down each branch of the design tree, resolving
> dependencies between decisions one-by-one. For each question, provide your
> recommended answer.
>
> Ask the questions one at a time.
>
> Background: I'm planning on a fresh start for this project, written in rust.
> Instead of using the current project as a reference oracle, I've distilled the
> essential and reimagined features i want in
> `docs/from-scratch-in-rust/SPEC.md` - this means you can explicitly ignore the
> Python implementation in this repo, although i expect we'll need to review the
> nix flake configuration as we explore setting up rust tooling appropriately.
>
> Task: Write an `IMPLEMENTATION.md` to guide a coding agent through
> implementation in Rust. This document serves to capture implementation
> decisions so that the coding agent doesn't have to guess.
>
> Secondary task: as we discuss this, i expect to specify and capture general
> instructions suitable for guiding a coding agent on my tastes in Rust
> implementation (e.g. architect in a functional core, imperative shell style),
> my tastes in testing (e.g. prefer high level integration testing). A secondary
> task is therefore to capture generic coding preferences in
> `RUST.instructions.md` and `RUST-TESTING.instructions.md` etc.

## Source Material Read

The assistant read:

- `docs/from-scratch-in-rust/SPEC.md`
- `flake.nix`
- `docs/rewrite-in-rust/PLAN.md`
- `AGENTS.md`
- `.pre-commit-config.yaml`

Key facts discovered:

- The spec describes a content-addressable media store in Rust using BLAKE3,
  SQLite, staging, WAL mode, a narrow CLI, and deep module interfaces.
- The existing flake provided Python tooling but not Rust tooling.
- Existing pre-commit hooks ran Python formatting, linting, mypy, and pytest.
- The existing Python implementation and tests remain in the repo but are not
  authoritative for the Rust rewrite.

## Interview Transcript And Decisions

### 1. Rust Project Location

Question: where should the Rust implementation live relative to the existing
Python project?

Recommended answer: make the repository root a Rust workspace with the main
binary crate at `crates/media-importer`; treat Python as archival/reference-only
until later removal.

User decision: agreed.

### 2. Crate Count

Question: should the first Rust implementation be a single crate or multi-crate
workspace from day one?

Recommended answer: start with a single binary package at
`crates/media-importer`, structured internally around functional core and
imperative shell. Do not split out `media-importer-core` yet.

User decision: agreed.

### 3. Reusable Architecture Instructions

Question: should reusable Rust architecture guidance be extracted into a
separate instruction file?

Recommended answer: yes. Keep project-specific decisions in `IMPLEMENTATION.md`
and reusable preferences in colocated instruction files.

User decision: agreed, and added a strong preference for deep, narrow module
interfaces.

### 4. Instruction File Location

Question: should instruction files be under `docs/from-scratch-in-rust/` or a
repo-level location such as `.github/instructions/`?

Recommended answer: colocate them under `docs/from-scratch-in-rust/`.

User decision: agreed.

### 5. First Observable Milestone

Question: what should the first externally observable implementation milestone
be?

Recommended answer: implement
`media-importer import --store <store> --source <source>` for regular files,
with BLAKE3 addressing, SQLite metadata, idempotency, and no presentation tree.
Include `--dry-run`.

User decision: agreed.

Additional user preference: Rust changes should be implemented in thin vertical
slices of end-to-end functionality rather than broad horizontal layers. Capture
this in `RUST.instructions.md`.

Additional user testing preference: trace-level logging can help observe deep,
narrow modules without exposing internals to tests. Capture this in
`RUST-TESTING.instructions.md`.

### 6. Initial CLI Contract

Question: what CLI flags should milestone 1 expose?

Initial recommendation included `--workers`.

User correction: do not include `--workers` in milestone 1.

Updated decision:

```text
media-importer import \
  --store <STORE_ROOT> \
  --source <SOURCE_PATH> \
  [--db <DB_PATH>] \
  [--dry-run] \
  [--chunk-size <BYTES>]
```

### 7. Chunk Size

Question: should `--chunk-size` be exposed in milestone 1?

Recommended answer: yes. It is useful for testing streaming behavior with tiny
chunks, but tests must assert outcomes rather than buffer internals.

User decision: agreed.

### 8. Source Path Observation Semantics

Question: what should "source path observation" mean in the database?

Recommended answer: use a stable `source_files` table keyed by
`source_root + relative_path`, pointing to the current blob hash. Re-importing
the same path updates `last_seen_at_ms` and `seen_count`; changed content at the
same path updates the hash while preserving `first_seen_at_ms`.

User decision: agreed.

### 9. Source Root Identity

Question: how should `source_root` be identified for idempotency?

Recommended answer: canonicalize the source root at the CLI boundary and store
that canonical absolute path. Store relative paths relative to that root.

User decision: agreed, and noted this should be captured as general Rust advice:
deliberate path canonicalization is a safety feature.

### 10. Blob Path Format

Question: should blob filenames include extensions or only hashes?

Recommended answer: hash-only blob filenames:

```text
<STORE_ROOT>/blobs/<hex[0..2]>/<hex[2..4]>/<full_blake3_hex>
```

User decision: agreed, noting that this avoids `.jpg` versus `.jpeg`
inconsistencies.

### 11. Existing Blob Behavior

Question: what should happen when the computed blob already exists?

Recommended answer: do not rewrite it. Verify final blob size, ensure read-only
permissions, discard staging, and still record/update source observations.

User decision: agreed, but asked to resolve the design issue around DB commit
versus blob placement ordering.

### 12. Commit Ordering

Question: should import commit filesystem-first or database-first?

Recommended answer: filesystem-first. Install the blob into CAS before
committing DB rows. A crash can leave an unreferenced CAS blob, which audit/GC
can later handle. Never commit a DB record for a blob only in staging.

User decision: agreed.

### 13. Concurrent Install Strategy

Question: how should accidental concurrent imports be handled?

Recommended answer: support correctness without a global lock. Use UUID staging
names and atomic no-overwrite install semantics. Do not stage directly to final
paths. If another process wins, verify final size, ensure chmod, remove staging,
and continue.

User correction: staging names only need UUIDs, not process IDs.

User asked for more detail on rename versus hard link.

### 14. Rename Versus Hard Link

Explanation: `std::fs::rename` can overwrite an existing destination on Unix,
which is unsafe for CAS installation. A hard-link-then-unlink strategy gives
"create if absent" behavior because `hard_link(staged, final)` fails if final
already exists.

Question: is the same-filesystem requirement for staging acceptable?

Recommended answer: yes. Staging and blobs are both under the store root.

User decision: agreed.

### 15. Initial SQLite Schema

Question: what schema should milestone 1 start with?

Recommended answer: `blobs` and `source_files` only, migration-ready via
`PRAGMA user_version`. Use integer epoch milliseconds rather than RFC3339 text.

User decision: liked `user_version` and epoch milliseconds, and asked to capture
those in `SQLITE.instructions.md`.

Clarification: `UNIQUE(source_root, relative_path)` means re-importing the same
path triggers an upsert that increments `seen_count`.

### 16. Seen Count Semantics

Question: should `seen_count` increment every time the path is observed, even if
the hash changes?

Recommended answer: yes. The row represents source path identity, not a specific
content version.

User decision: agreed.

### 17. Migration Runner

Question: should milestone 1 include a migration runner or just create schema
v1?

Recommended answer: implement a tiny migration runner from day one. If
`user_version == 0`, create schema v1; if `1`, proceed; if newer, fail clearly.

User decision: agreed.

### 18. DB Writer Thread

Question: should the single-writer DB thread from the spec be implemented
immediately?

Recommended answer: defer it. Use synchronous DB writes behind a narrow catalog
interface in milestone 1. Preserve the future path for writer thread/batching.

User decision: agreed.

### 19. Ingest Public Interface

Question: what should the import module expose?

Recommended answer:

```rust
pub fn import_source(config: ImportConfig) -> Result<ImportReport>
```

with CLI-decoded validated config and a structured report.

User decision: agreed, and specified `color-eyre` for CLI-grade contextual
errors.

### 20. Error Handling

Question: how should error handling be standardized?

Recommended answer: use `color-eyre` broadly in the CLI crate, call
`color_eyre::install()` once in `main`, wrap errors with useful context, and
avoid large custom error enums until a real library boundary exists.

User decision: agreed.

### 21. CLI Parsing

Question: which crate should own CLI parsing, and how strict should CLI types
be?

Recommended answer: use `clap` derive. Keep CLI structs separate from validated
application config. Only expose implemented commands.

Question: should milestone 1 expose only `import` or all four commands with
stubs?

User decision: expose only `import`.

### 22. Rust Edition And Scanner Dependency

Question: which Rust edition and scanner dependency should be used?

Recommended answer: Rust 2024. Use `walkdir` for milestone 1; avoid `ignore`
unless gitignore-style behavior is desired.

User decision: agreed.

### 23. Logging

Question: how should milestone 1 handle logging and trace output?

Recommended answer: use `tracing` from day one, controlled by
`RUST_LOG`/`EnvFilter`. Keep output simple and defer the TTY dashboard.

User decision: agreed.

### 24. Test Surface

Question: should tests drive the compiled CLI, `ingest::import_source`, or both?

Recommended answer: both, with a bias toward CLI-level integration tests. Include
`lib.rs` so tests can call deep module interfaces where useful.

User decision: agreed, noting high-level tests might later need speed strategies
but should remain the bias.

### 25. Integration-First But Fast

Question: how should Rust testing instructions express the integration-first
but still fast preference?

Recommended answer: use real temp directories and SQLite by default, avoid
mocks, keep fixtures tiny, optimize the harness before abandoning high-level
tests, and use trace/probe outputs for observability.

Question: should tests inspect SQLite directly?

User decision: yes.

### 26. Dry-Run Contract

Question: how should dry-run be tested and specified?

Recommended answer: dry-run means no durable mutation, not no work. It may scan,
hash, and report, but must not create or modify store, staging, blobs, DB, WAL,
or SHM.

User decision: agreed, adding that future dry-run could use a temporary or
in-memory DB clone if write-like planning is useful.

### 27. Dry-Run DB Clone In Milestone 1

Question: should milestone 1 use a cloned/in-memory DB for dry-run?

Recommended answer: no. Avoid DB writes entirely in milestone 1, but document
the clone strategy for future dry-run complexity.

User decision: agreed.

### 28. Scanner Policy

Question: how should scanner treat hidden files, directories, permission errors,
and symlinks?

Recommended answer: include hidden files/directories, ignore symlinks, process
regular files only, fail on permission/read errors, and sort by relative path for
determinism.

User decision: agreed.

### 29. Source Metadata

Question: which file metadata should milestone 1 capture?

Recommended answer: source root, relative path, blob hash, size, nullable
modified time, first/last seen timestamps, and seen count. Do not store inode or
device. Always hash files in milestone 1.

User decision: agreed.

### 30. Metadata-Skip Warning

Question: should the guide warn agents not to optimize reruns with mtime/size in
milestone 1?

Recommended answer: yes. Metadata-skip is a later explicitly designed feature.

User decision: agreed.

### 31. Read-Only Blob Permissions

Question: how should blob permissions be handled?

Recommended answer: on Unix, set mode `0444` after installation or reuse. On
non-Unix, fall back to `set_readonly(true)`. Fail if chmod fails.

User decision: agreed.

### 32. Staging Cleanup

Question: how should staging cleanup work?

Recommended answer: real import purges only the contents of `<store>/staging`
on startup. Dry-run does not create or purge staging.

User decision: agreed.

### 33. Nix Flake

Question: how should the flake be updated for Rust?

Recommended answer: add Nixpkgs default Rust tooling alongside existing Python
tools: `rustc`, `cargo`, `rustfmt`, `clippy`, `rust-analyzer`, and keep `sqlite`.
Do not use `rust-overlay` yet.

User decision: Nixpkgs default is fine. Also expose rust-analyzer/LSP for coding
agent navigation where meaningfully useful, and update `AGENTS.md`/hooks.

### 34. Slice 0

Question: should there be a preparatory "agent environment" slice before
functional Rust work?

Recommended answer: yes. Slice 0 updates flake, Cargo workspace, scaffold,
hooks, `AGENTS.md`, and docs without implementing import behavior.

User decision: agreed.

### 35. Success Output

Question: what should milestone 1 print on success?

Recommended answer: concise human-readable summary to stdout, with logs behind
`tracing` and errors through `color-eyre`.

User decision: agreed.

### 36. JSON Output

Question: should milestone 1 support `--json`?

Recommended answer: defer it. Keep `ImportReport` structured so JSON can be
added later.

User decision: agreed.

### 37. Future `build-tree`

Question: how should `build-tree` be framed?

Recommended answer: later vertical slice. It materializes presentation symlinks
from DB state into a separate tree; import does not build symlinks.

User decision: agreed.

### 38. Future `gc`

Question: how should `gc` be framed?

Recommended answer: later mark-and-sweep workflow with dry-run/planning before
deletion. Never run GC automatically during import.

User decision: agreed.

### 39. Future `audit`

Question: how should `audit` be framed?

Recommended answer: later consistency checker between SQLite catalog and CAS
filesystem. Report by default, mutate nothing.

User decision: agreed.

### 40. Relationship Schema

Question: should milestone 1 include semantic relationship tables?

Recommended answer: no. Defer until a feature needs them.

User decision: agreed.

### 41. Media Metadata

Question: should milestone 1 store MIME, dimensions, EXIF, duration, or
extension?

Recommended answer: no. Store only content and source observation metadata.

User decision: agreed.

### 42. Non-UTF-8 Paths

Question: how should source path text be stored in SQLite?

Recommended answer: milestone 1 requires UTF-8-representable paths and fails
clearly for non-UTF-8 identities. Do not use lossy conversion.

User decision: agreed.

### 43. Relative Path Separators

Question: should catalog relative paths use platform separators or `/`?

Recommended answer: normalize `relative_path` to `/` in SQLite; keep
`source_root` as an absolute platform path string.

User decision: agreed, with concern about security risks and future hardening
such as chroot-like constraints to avoid writing or symlinking outside intended
trees.

### 44. Path Containment Hardening

Question: how much path hardening should milestone 1 require?

Recommended answer: basic containment now: canonical source root, validated
relative paths, no `..`, no absolute paths, CAS paths constructed only from
trusted hash components. Defer stronger sandboxing/openat/chroot-style
hardening.

User decision: agreed, and suggested leaning into Rust type safety in the later
hardening milestone to make tainting CAS paths with untrusted input impossible.

### 45. Path Newtypes

Question: should the guide introduce path/domain newtypes?

Recommended answer: yes, from the first slice:

- `BlobHash`
- `SourceRoot`
- `SourceRelativePath`
- `StoreRoot`

Do not overdo typestate.

User decision: agreed.

### 46. Newtype Ownership

Question: which module should own path newtypes and construction rules?

Recommended answer: a dedicated `paths` module owns validation, serialization,
and CAS/staging path construction.

User decision: agreed.

### 47. Scanner Return Shape

Question: should scanner return a collected sorted list or iterator?

Recommended answer: collected sorted `Vec<SourceFileCandidate>` in milestone 1.

User decision: agreed.

### 48. Hashing And Staging Interface

Question: should hashing and staging be one operation or separate module calls?

Recommended answer: expose one deep `Store::ingest_file` operation for real
import, internally using hashing/staging/install. Keep pure/read-only hashing
helpers available.

User decision: agreed.

### 49. Dry-Run Hash/Reuse

Question: how should dry-run compute hash/reuse without mutating store?

Recommended answer: separate read-only path using `hash_file` and
`blob_exists_with_size`. Do not call mutating `Store::ingest_file`.

User decision: agreed.

### 50. Pre-Hash Catalog Skip

Question: should real import consult catalog before hashing to skip already-seen
files?

Recommended answer: no. Always hash every regular file in milestone 1.

User decision: agreed.

### 51. Catalog Upsert Outcomes

Question: should catalog methods return inserted/updated outcomes?

Recommended answer: yes, to support accurate `ImportReport` fields without
exposing SQL.

User decision: agreed.

### 52. Transaction Scope

Question: should each file be recorded in its own SQLite transaction or should
the whole import be one transaction?

Recommended answer: transaction per file.

User decision: agreed.

### 53. Timestamp Semantics

Question: should `blobs.created_at_ms` mean first-seen-by-catalog or source
mtime?

Recommended answer: first-seen-by-catalog. Source mtime belongs on
`source_files.modified_at_ms`.

User decision: agreed.

### 54. Clock Abstraction

Question: should milestone 1 inject a clock for deterministic tests?

Recommended answer: yes, a small `Clock` abstraction for timestamp-writing
behavior.

User decision: agreed.

### 55. Filesystem Modified Time

Question: should source mtimes be stored in milliseconds, and how should edge
cases be handled?

Recommended answer: nullable epoch milliseconds. If unavailable or before Unix
epoch, store `None` and trace/debug; metadata read failures still fail import.

User decision: agreed, noting some filesystems do not update mtime as a
performance optimization.

### 56. Mtime As Advisory Metadata

Question: should mtime be documented as advisory only?

Recommended answer: yes.

Question: should future metadata-skip default off?

User correction: default on.

Decision: milestone 1 always hashes. Later metadata-skip may default on but must
be documented as a performance/correctness tradeoff with an opt-out flag.

### 57. Metadata-Skip Inputs

Question: should future metadata-skip use only path + size + mtime, or also
quick fingerprints?

Recommended answer: path + size + mtime only.

User decision: agreed.

### 58. Explicit DB Parent Creation

Question: should real import create parent directories for explicit `--db`?

Recommended answer initially: yes.

User correction: no. If explicit DB parent does not exist, error. Do not guess
permissions or silently rely on umask for important user-selected directories.

Decision: default DB inside store can be created as part of store
initialization; explicit DB parent must already exist.

### 59. Store Root Creation

Question: should real import create `--store` if missing?

Recommended answer: yes, but only that exact tool-owned root and known children.
If the store parent does not exist, fail.

User decision: agreed.

### 60. Single-File Import

Question: should milestone 1 support importing a single file?

Initial recommendation: yes.

User correction: no. Directory-only is simpler and avoids future bugs where a
named-file import might accidentally import the whole parent directory.

Decision: `--source` must be an existing directory.

### 61. Empty Source Directory

Question: should empty source directory import succeed?

Recommended answer: yes, successful no-op with zero counts.

User decision: agreed.

### 62. Source/Store Overlap

Question: should `--store` equal, contain, or be contained by `--source`?

Recommended answer: reject all source/store overlap.

User decision: agreed.

### 63. Explicit DB In Source

Question: if explicit `--db` is outside store, should overlap checks ensure DB
is not inside source?

Recommended answer: yes.

User decision: agreed.

### 64. Store Canonicalization When Missing

Question: should `--store` be canonicalized if it does not exist?

Recommended answer: canonicalize nearest existing parent and append intended
store directory name.

User decision: agreed.

### 65. DB Canonicalization When Missing

Question: should `--db` be canonicalized if the DB file does not exist?

Recommended answer: canonicalize existing parent and append DB filename.

User noted a gotcha: default DB under a not-yet-existing store would otherwise
fail because store does not exist.

Decision: validate `StoreRoot` first. If `--db` is omitted, derive
`StoreRoot/catalog.sqlite` without separately canonicalizing the DB parent.

### 66. Explicit DB Inside Missing Store

Question: should explicit `--db` be allowed inside a not-yet-existing store?

Recommended answer: no. If the user wants default store DB, omit `--db`.

User decision: agreed.

### 67. Overlap With Missing Store

Question: how should overlap validation work when store does not exist?

Recommended answer: use intended absolute `StoreRoot` path derived by
canonicalizing its existing parent.

User decision: agreed.

### 68. Empty Directories In Catalog

Question: should empty source directories be recorded?

Recommended answer: no. Milestone 1 records regular files only.

User decision: agreed.

### 69. Scanner Name-Based Skips

Question: should scanner skip known store/database names if under source?

Recommended answer: no. Structural overlap checks should exclude tool-owned
paths; scanner should avoid ad hoc name-based skips.

User decision: agreed.

### 70. Clippy Warnings

Question: should the guide require `cargo clippy -D warnings` from the first
scaffold slice?

Recommended answer: yes.

User decision: agreed, and said this should be configured in `prek` pre-commit
hooks and possibly coding agent hooks.

### 71. Rust Hooks

Question: how should hooks be configured?

Recommended answer: add local hooks to `.pre-commit-config.yaml`:

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`

Question: should `cargo test --workspace` be in pre-commit?

User decision: yes. It encourages developer focus on maintaining speed.

### 72. AGENTS.md Scope

Question: should Slice 0 modify `AGENTS.md`?

Recommended answer: yes, lightly. Point agents at the Rust rewrite docs and
checks, while keeping detailed preferences in colocated instruction files.

User decision: agreed.

### 73. CI

Question: should the implementation guide require adding CI?

Recommended answer: defer CI.

User decision: agreed.

### 74. README Scope

Question: should the Rust scaffold include README updates?

Initial recommendation: keep README out of scope.

User correction: top-level README should be updated because Python is deprecated.

Decision: README gets a small status update in Slice 0.

### 75. README Detail

Question: how much should README change?

Recommended answer: small prominent status note, Rust dev commands, and mark
Python usage as legacy/deprecated. No full rewrite yet.

User decision: agreed.

### 76. Commit Boundaries

Question: should the guide require committing Slice 0 before Milestone 1?

Recommended answer: recommend separate conventional-commit-friendly slices but
do not assume permission to commit.

User decision: agreed.

### 77. Crate Naming

Question: should crate name be `media-importer` or `media_importer`?

Recommended answer: package and binary `media-importer`; library crate
`media_importer`.

User decision: agreed.

### 78. MSRV

Question: should the guide specify MSRV?

Recommended answer: no formal MSRV yet. Use stable Nixpkgs Rust, Rust 2024, and
avoid nightly-only features.

User decision: agreed.

### 79. Extra Rust Quality Tools

Question: should Slice 0 include `cargo deny`, `cargo nextest`, etc.?

Recommended answer: no, but note coverage and other tooling as future
considerations.

User decision: agreed.

### 80. Future Quality Section

Question: should `IMPLEMENTATION.md` include a future quality/tooling section?

Recommended answer: yes, brief and non-binding.

User decision: agreed.

### 81. fsync Discipline

Question: should milestone 1 fsync staged files or directories?

Recommended answer: do not require full fsync discipline in milestone 1. Defer
to a durability hardening milestone.

User decision: agreed.

### 82. Blob Hash Validation

Question: how strict should `BlobHash` validation be?

Recommended answer: exactly 64 lowercase ASCII hex characters, with validating
constructors and no unchecked public constructors.

User decision: agreed.

### 83. SQLite Hash Constraint

Question: should schema enforce hash format?

Recommended answer: include at least `CHECK(length(hash) = 64)`; enforce
lowercase hex in Rust.

User decision: agreed.

### 84. SQLite Path Constraints

Question: should `source_files` enforce path constraints in SQLite?

Recommended answer: yes, minimal backstops such as non-empty `source_root` and
non-empty non-absolute `relative_path`. Do real path safety in Rust.

User decision: agreed.

### 85. PRAGMAs

Question: should catalog set PRAGMAs every open or only on creation?

Recommended answer: every writable open:

- `foreign_keys = ON`
- `journal_mode = WAL`
- `synchronous = NORMAL`

User decision: agreed.

### 86. Dry-Run Catalog Access

Question: how should dry-run read an existing catalog without mutating SQLite
sidecar files?

Recommended answer: if DB missing, treat as empty; if exists, prefer read-only
mode and avoid mutating PRAGMAs/migrations. If read-only access mutates sidecars
in practice, copy to temp/in-memory.

User decision: agreed.

### 87. Dry-Run Reuse Semantics

Question: in dry-run, should "would reuse" be based on CAS filesystem presence,
catalog presence, or both?

Recommended answer: CAS filesystem determines blob reuse; catalog determines
source-record insert/update reporting.

User decision: agreed.

### 88. Existing CAS Blob Without Catalog Row

Question: should real import treat "CAS blob exists but catalog row missing" as
reusable and insert the missing row?

Recommended answer: yes. This repairs harmless crash leftovers.

User decision: agreed.

### 89. Blob Created Count

Question: how should `blobs_created` be counted if CAS file is created but DB
row already existed?

Recommended answer: `blobs_created` means filesystem CAS blobs created during
this run, from `StoreOutcome::Created`, not DB rows inserted.

User decision: agreed.

### 90. Dependency Versions

Question: should dependency versions be exact or major-compatible?

Recommended answer: major-compatible recommendations, with `Cargo.lock` pinning
resolved versions.

User decision: agreed.

### 91. Source File Changes During Import

Question: how should milestone 1 handle a source file changing during import?

Recommended answer: detect obvious changes and fail. Compare bytes read to
expected size; re-stat after hashing and fail if size or mtime changed.

User decision: agreed.

### 92. Per-File Failure Policy

Question: should failure on one file abort the whole import?

Recommended answer: yes for milestone 1. Do not roll back prior successful
files.

User decision: agreed, adding that future resilient batch import can report
errors, but rollback is never a product feature because rollback is delegated to
ZFS snapshots.

### 93. ZFS Integration

Question: should milestone 1 include ZFS-specific integration?

Recommended answer: no. The design is ZFS-friendly but uses normal filesystem
APIs.

User decision: agreed.

### 94. Rollback Tests

Question: should tests verify no rollback behavior?

Recommended answer: document it and indirectly test actual failure behavior
where practical. Avoid elaborate failure injection in milestone 1.

User decision: agreed.

### 95. Write The Docs

Question: are we ready to write the first draft of the documentation files?

Recommended answer: yes.

User decision: yes.

## Files Created From This Conversation

The assistant created:

- `docs/from-scratch-in-rust/IMPLEMENTATION.md`
- `docs/from-scratch-in-rust/RUST.instructions.md`
- `docs/from-scratch-in-rust/RUST-ARCHITECTURE.instructions.md`
- `docs/from-scratch-in-rust/RUST-TESTING.instructions.md`
- `docs/from-scratch-in-rust/SQLITE.instructions.md`

The assistant then committed those files with:

```text
docs: record rust implementation guidance
```

Commit:

```text
2fd2da8
```

## Follow-Up User Request

After the docs were drafted, the user asked:

> Commit the files, then capture this entire conversation in markdown in a new
> "prompts" subdirectory, then commit that and push.

The first commit was made through `nix develop --command git commit ...` so the
configured pre-commit hooks could find project tooling. The first direct commit
attempt failed because `pytest` was not available in the ambient shell.

