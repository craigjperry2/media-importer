# Agent Directives: Media Importer

## Authoritative Docs

This repository is in a clean-slate Rust rewrite. Treat these files as the
source of truth for current implementation work:

- `docs/from-scratch-in-rust/IMPLEMENTATION.md`
- `docs/from-scratch-in-rust/SPEC.md`
- `docs/from-scratch-in-rust/RUST.instructions.md`
- `docs/from-scratch-in-rust/RUST-ARCHITECTURE.instructions.md`
- `docs/from-scratch-in-rust/RUST-TESTING.instructions.md`
- `docs/from-scratch-in-rust/SQLITE.instructions.md`

The Python implementation is deprecated historical context. Material under
`docs/old_python/` may explain past decisions, but it is not a behavior oracle
for Rust work.

## Commands

Execute these from the project root after entering the dev environment
(`direnv allow` or `nix develop`):

- **Format:** `cargo fmt --all --check`
- **Lint:** `cargo clippy --workspace --all-targets -- -D warnings`
- **Test:** `cargo test --workspace`
- **Hooks:** `prek run --all-files`
- **Run CLI:** `cargo run -p media-importer --`

## Boundaries

- Prefer a functional core with side effects at the edges.
- Keep CLI parsing, validation, dispatch, and user-facing output in CLI-facing
  modules.
- Keep source scanning separate from hashing, blob writes, and catalog writes.
- Keep content-addressed store mutation in store-focused modules.
- Keep SQLite connection setup, migrations, PRAGMAs, transactions, and raw SQL
  in catalog-focused modules.
- Keep ingest orchestration behind behavior-level interfaces.
- Do not use ORMs.
- Do not expose placeholder commands before their milestone.
- Maintain type safety and do not suppress linting broadly.

## Project Structure

- `crates/media-importer/src/main.rs`: binary entrypoint.
- `crates/media-importer/src/lib.rs`: library crate root.
- Future internal modules should follow the Rust rewrite docs, starting with
  responsibilities such as `cli`, `config`, `paths`, `scanner`, `hashing`,
  `store`, `catalog`, `ingest`, and `telemetry` or `reporting`.

## Git

Use Conventional Commits for commit messages when asked to commit:

- `feat: behavior added, changed or removed`
- `refactor: why thing needed to change`
- `test: behavior covered by tests`

Do not assume permission to commit unless explicitly asked.

