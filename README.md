# Media Importer

`media-importer` is a Rust CLI for importing files into a content-addressed
store backed by a SQLite catalog, materializing a browse tree, and auditing
catalog and CAS integrity.

The current implementation exposes `import`, `build-tree`, and `audit`. The
Rust rewrite documentation under `docs/from-scratch-in-rust/` is authoritative.
Material under `docs/old_python/` is historical context, not a behavior oracle.

## Commands

Import a source directory into the store and catalog:

```text
media-importer import \
  --store STORE_ROOT \
  --source SOURCE_ROOT \
  [--db DB_PATH] \
  [--dry-run] \
  [--chunk-size BYTES]
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

When `--db` is omitted, commands use `STORE_ROOT/catalog.sqlite`.

### Audit contract

Audit validates the SQLite schema, catalog domain values, foreign keys, and
both directions of the catalog/CAS relationship. It fully rehashes every
canonical CAS blob and can therefore be I/O intensive.

The store and catalog must remain quiescent throughout an audit. Do not run
`import` or another store-mutating process concurrently.

Audit only reports problems. It never creates, repairs, deletes, migrates, or
checkpoints durable store or catalog state. Catalog blobs with `deleted_at_ms`
set remain expected in the CAS and are counted as GC candidates.

Audit exit statuses are:

- `0`: audit completed with no integrity findings;
- `1`: invalid configuration or an operational error prevented completion;
- `2`: audit completed with one or more integrity findings.

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
