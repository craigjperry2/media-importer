# Media Importer

`media-importer` is being rewritten from scratch in Rust. The Rust rewrite docs
in `docs/from-scratch-in-rust/` are authoritative for current implementation
work; the previous Python codebase has been archived under `docs/old_python/`
for historical context only.

The current branch contains the initial Cargo workspace scaffold. Milestone 1
will add a single end-to-end `import` command that imports a source directory
into a content-addressed store and records catalog state in SQLite:

```sh
media-importer import \
  --store /path/to/store \
  --source /path/to/source \
  [--db /path/to/catalog.sqlite] \
  [--dry-run] \
  [--chunk-size <bytes>]
```

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

