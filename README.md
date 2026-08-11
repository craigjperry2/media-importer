# Media Importer

`media-importer` is a Rust CLI for importing files into a content-addressed
store backed by a SQLite catalog, materializing a browse tree, auditing catalog
and CAS integrity, and explicitly collecting unreachable blobs.

The current implementation exposes `import`, `build-tree`, `audit`, and `gc`.
The Rust rewrite documentation under `docs/from-scratch-in-rust/` is
authoritative. Material under `docs/old_python/` is historical context, not a
behavior oracle.

## Commands

Import a source directory into the store and catalog:

```text
media-importer import \
  --store STORE_ROOT \
  --source SOURCE_ROOT \
  [--db DB_PATH] \
  [--dry-run] \
  [--chunk-size BYTES] \
  [--workers-per-mount N] \
  [--no-metadata-skip]
```

Build a browse tree of links from live catalog entries:

```text
media-importer build-tree \
  --store STORE_ROOT \
  --browse-tree BROWSE_TREE_ROOT \
  [--db DB_PATH] \
  [--dry-run] \
  [--hash-digits DIGITS]
```

Audit the catalog and CAS without modifying them:

```text
media-importer audit \
  --store STORE_ROOT \
  [--db DB_PATH] \
  [--chunk-size BYTES]
```

Run mark-and-sweep garbage collection:

```text
media-importer gc \
  --store STORE_ROOT \
  [--db DB_PATH] \
  [--dry-run] \
  [--chunk-size BYTES]
```

When `--db` is omitted, commands use `STORE_ROOT/catalog.sqlite`.

`import` reuses a previously cataloged blob without reading source content when
the canonical source path, size, and available modified time match. This is an
optimization that relies on the source filesystem faithfully updating modified
times when content changes; use `--no-metadata-skip` to hash every regular
source file when that assumption is unsuitable.

`--workers-per-mount` defaults to `1`. It limits concurrent source-content
reads separately for every source filesystem (`st_dev`), including nested
mounts discovered beneath the source root. Increase it for SSD-backed sources;
the importer intentionally does not guess disk type.

Imports expose five bounded high-water measurements: 64 scanner-to-scheduler
candidates; per-mount work, including both scheduler backlog and work admitted
to the shared worker queue/active readers (at most 64 plus the smaller of that
mount's configured reader limit and the eight-worker process cap); 16 completed
blobs awaiting catalog submission (8 in the completion channel plus at most 8
held by workers while that channel is full); 64 catalog-writer requests; and
32 pending catalog outcomes. Queue occupancy events carry the stage, current
occupancy, and capacity for deterministic test probes. The fixed pipeline
capacities avoid configuration multiplication; the per-mount bound is computed
with checked arithmetic from the validated non-zero reader limit.
The scheduler skips mount groups at their configured reader limit and chooses
the next eligible queued candidate, so excess mount identities cannot create
threads or prevent an eligible mount from making progress. At most eight source
worker threads are live for a run.

### Single-node command coordination

Commands that use the same canonical store root coordinate automatically with
an operating-system advisory lock on that directory. Conflicting commands wait
indefinitely for the current cooperating command to finish; the operating
system releases the lock if its process exits. There is no lock file or
catalog lock row.

| Command | Mode |
| --- | --- |
| `import`, `build-tree`, `gc` | exclusive |
| `import --dry-run` (existing store), `build-tree --dry-run`, `audit`, `gc --dry-run` | shared |

Shared commands may overlap each other. An `import --dry-run` whose store does
not exist is deliberately unlocked and creates nothing. This is cooperative,
single-node coordination only: manual filesystem edits, direct SQLite clients,
and other non-cooperating programs remain unsafe. Different stores that share
an external catalog or browse-tree path are unsupported and uncoordinated.

### Catalog writer operation

Each mutating import or GC command owns one catalog-writer thread for its full
run. Import observations commit in bounded batches; the writer disables SQLite
automatic WAL checkpoints and performs explicit passive checkpoints after
configured batches and during clean shutdown. No command-line tuning flags are
exposed for these internal limits. Read-only commands and all dry runs do not
start a writer or checkpoint the catalog.

### Audit contract

Audit validates the SQLite schema, catalog domain values, foreign keys, and
both directions of the catalog/CAS relationship. It fully rehashes every
canonical CAS blob and can therefore be I/O intensive.

Cooperating `media-importer` commands using the same store coordinate
automatically. External catalog and filesystem writers remain unsafe during an
audit.

Audit only reports problems. It never creates, repairs, deletes, migrates, or
checkpoints durable store or catalog state. Catalog blobs with `deleted_at_ms`
set remain expected in the CAS and are counted as GC candidates.

Audit exit statuses are:

- `0`: audit completed with no integrity findings;
- `1`: invalid configuration or an operational error prevented completion;
- `2`: audit completed with one or more integrity findings.

### Garbage collection contract

GC treats schema-v1 `source_files` records as its reachability roots. It does
not inspect whether the original files still exist: a missing source file
remains a durable reference until a future explicit catalog-management feature
removes or replaces that record. Browse-tree links are not reachability roots.

Collection uses two committed catalog transitions:

1. A run marks an unreachable, unmarked blob by setting `deleted_at_ms`.
2. A later invocation may sweep that previously marked blob if it is still
   unreachable.

The boundary is committed blob state, not process exit status or elapsed time.
A mark committed during an otherwise incomplete run counts as the first
transition. Newly marked blobs are never swept in the same invocation. If a
marked blob becomes referenced, GC clears the mark instead of sweeping it.
Import also clears a matching blob's mark atomically with recording its source
observation, so rerunning a failed or repeated import normally resurrects that
content.

Before any mutation, GC validates catalog integrity and the strict CAS layout,
reconciles catalog and CAS membership, checks every cataloged blob's size, and
fully hashes every present sweep candidate. This can be I/O intensive.
Orphans, corrupt candidates, missing live blobs, malformed entries, and other
integrity findings block all application-row and CAS mutation. GC never adopts,
repairs, or deletes an orphan.

`--dry-run` performs that complete preflight and prints `WOULD_*` actions
without creating, changing, or deleting catalog, SQLite sidecar, CAS, staging,
permission, or timestamp state (ordinary reads may update access times
according to the filesystem's mount policy).

GC requires a non-zero `--chunk-size`. A zero value is invalid GC
configuration and returns exit status `1` without producing a GC summary or
mutating durable state.

Filesystem removal and the SQLite commit cannot be globally atomic. GC removes
and directory-syncs a blob before deleting its catalog row. If interrupted
between those steps, a later run recognizes a previously marked, unreachable,
already-absent blob and finalizes its catalog row. Empty CAS shard directories
are deliberately retained. “Bytes reclaimed” is the sum of cataloged logical
sizes for finalized present sweeps; ZFS compression, snapshots, deduplication,
sparse allocation, reflinks, or hard links can make physical space recovery
different.

Cooperating `media-importer` commands using the same store coordinate
automatically. Do not run external catalog/CAS writers against that store
concurrently. SQLite's write reservation remains part of GC's catalog
transaction semantics.

GC exit statuses are:

- `0`: GC or GC dry-run completed with no safety findings;
- `1`: invalid configuration or an operational/mutation error prevented full
  completion; an incomplete report may show already committed or physical
  progress;
- `2`: the safety preflight found integrity problems and blocked all
  application-row and CAS mutation.

After GC, rerun `build-tree` when appropriate. An existing browse tree may
contain stale links because it is an offline materialization, not a
reachability root. If an import was interrupted after installing a CAS blob but
before catalog commit, rerunning that import can normally adopt the orphan;
audit the store before retrying GC.

## Setup

Use the Nix development shell from the repository root:

```sh
nix develop
```

With `direnv`, allow the repository once and let it enter the same flake shell
automatically:

```sh
direnv allow
```

The shell provides Cargo, rustc, rustfmt, clippy, rust-analyzer, SQLite, bash,
and `prek`. Entering the shell installs the pre-commit hooks.

## Checks

Run the same checks configured in pre-commit:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Or run all hooks:

```sh
prek run --all-files
```

## Repository Layout

- `Cargo.toml`: root Cargo workspace.
- `crates/media-importer/`: Rust application package, binary, and library.
- `docs/from-scratch-in-rust/`: active product, architecture, testing, and
  implementation guidance for the rewrite.
- `docs/old_python/`: archived Python-era notes and feature material. Do not
  use this archive as a behavior oracle for the Rust implementation.
