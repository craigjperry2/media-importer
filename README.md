# Robust Media Consolidator & Cataloger

A production-grade command-line tool written in Python to incrementally consolidate media from multiple input directories into a centralized, deduplicated "store". It uses a persistent SQLite database as a hashing cache and catalog.

## Architecture

The logic is strictly divided into modules:
* `cli.py`: Argparse entrypoint.
* `models.py`: Dataclasses modeling domain objects.
* `hashing.py`: Cryptographic content hashing.
* `scanner.py`: Filesystem traversal ignoring symlinks.
* `catalog.py`: SQLite connection management and deterministic schema.
* `planner.py`: Purely functional state comparison. Yields execution plans.
* `executor.py`: Effectful executor that processes action plans (atomic copying, DB transactions).

## Requirements & Environment

This tool uses a Nix Flake for fully reproducible dependencies and `uv` for Python virtual environment management.

### Prerequisites

- [Nix](https://nixos.org/download/) with flakes enabled
- [direnv](https://direnv.net/) (optional, but recommended)

### Setup

**Option A — direnv (recommended):** The repo includes a `.envrc` that activates the flake automatically. After installing direnv:

```sh
direnv allow
```

This drops you into a shell with Python 3.13, `uv`, Nix-provided native CLI tools such as `prek` and `ruff`, and an activated `.venv` with all dependencies installed.

**Option B — manual:**

```sh
nix develop
```

In both cases the `shellHook` runs `uv sync --dev` to create `.venv/` and install dependencies, then activates the virtualenv. The shell then prefers Nix-provided native CLI tools such as `prek` and `ruff`, which keeps the setup working on both nix-darwin and NixOS. Subsequent entries are fast because `uv` is incremental.

**Option C — without nix (not recommended):** You are responsible for providing a suitable uv and python version (NB: uv can provide the python version). You can then manually create the venv, sync the dependencies, and install the pre-commit hook:

```sh
uv sync --dev
source .venv/bin/activate
prek install
```

### Running the app

With the venv active (either via direnv or after `nix develop`):

```sh
media-importer --help
```

## Usage

### Scanning and Consolidating

```sh
media-importer scan \
  --store /path/to/store \
  --db /path/to/catalog.db \
  --source /path/to/source1 \
  --source /path/to/source2
```

During `scan`, the CLI reports count-based progress as it walks sources, plans work, and processes file copies so long-running runs stay visible.

To maintain a human-browseable symlink overlay alongside the hash-based store,
pass `--browse-root`. The browse tree mirrors each source-relative path, but
the file name is suffixed with the first seven characters of the content hash to
keep paths stable and collision-free:

```sh
media-importer scan \
  --store /path/to/store \
  --browse-root /path/to/browse \
  --db /path/to/catalog.db \
  --source /path/to/source
```

The real files remain in the canonical store. The browse root contains only
symlinks, and rescans remove stale links for files that disappeared from the
scanned source roots.

Use `--dry-run` to observe planned changes without writing to disk or database:
```sh
media-importer scan --store store --db catalog.db --source src_dir --dry-run
```

Non-dry-run scans process files incrementally in bounded batches so newly hashed files are copied while they are still likely to be resident in the page cache. `--dry-run` still computes the full action list up front.

### Verifying Store State

To find missing or unindexed files in the store:
```sh
media-importer verify-store --store /path/to/store --db /path/to/catalog.db
```

### Querying the Catalog

Search the database:
```sh
media-importer query --db /path/to/catalog.db --ext .jpg --name "vacation"
```

## Running Tests

```sh
pytest
```

(`pytest.ini_options` in `pyproject.toml` sets `testpaths = ["tests"]` so no path argument is needed.)
