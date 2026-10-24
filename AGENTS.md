# Agent Directives: Media Importer

## Authoritative Docs

`media-importer` is implemented in Rust. Treat these files as the source of
truth for implementation work:

- `docs/rust/SPEC.md`
- `docs/rust/RUST.instructions.md`
- `docs/rust/RUST-ARCHITECTURE.instructions.md`
- `docs/rust/RUST-TESTING.instructions.md`
- `docs/rust/SQLITE.instructions.md`

The Rust code replaced an earlier Python implementation. Material under
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
- **Validate before finishing a turn:** If you changed any files, run
  `prek run --all-files` and fix every failure before you end your turn or
  commit. Git hooks may not run for you; Amp orbs, for example, use a global
  `core.hooksPath` that disables the repo's pre-commit hook. The format hook
  only checks, so run `cargo fmt --all` to fix formatting, then rerun prek.

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
- Do not expose placeholder commands for unimplemented features.
- Maintain type safety and do not suppress linting broadly.
- Delete unused generated code instead of hiding it with `#[allow(dead_code)]`.
  Use local lint allowances only for deliberate, documented exceptions.

## Project Structure

All modules live in `crates/media-importer/src/`:

- `main.rs`: binary entrypoint and command dispatch.
- `lib.rs`: library crate root.
- `cli.rs`: clap argument parsing.
- `config.rs`: validated command configuration.
- `paths.rs`: path and identity newtypes (store, source, browse tree, hashes).
- `scanner.rs`: source tree traversal (does not follow symlinks).
- `hashing.rs`: BLAKE3 hashing over byte streams.
- `store.rs`: content-addressed store layout, staging, and blob installation.
- `catalog.rs` and `catalog/sql/`: SQLite connections, schema, writer thread,
  and raw SQL.
- `ingest.rs`: mount-aware parallel import orchestration.
- `materialize.rs`: `build-tree` browse-tree materialization.
- `audit.rs`: read-only catalog and CAS integrity audit.
- `gc.rs`: mark-and-sweep garbage collection.
- `integrity.rs`: shared integrity findings and safe path escaping.
- `run_lock.rs`: cooperative advisory lock on the store root.
- `telemetry.rs`: telemetry facts and the human and JSON Lines renderers.
- `test_probe.rs`: private handshakes for integration-test child processes.

Integration tests live in `crates/media-importer/tests/`.

## Specialized TDD Agents

- Custom Copilot agents for the red-green-refactor loop live in
  `.github/agents/`.
- `tdd-judge`: verifies each completed stage in a fresh context window, then
  either forwards it or rejects it with concrete rework instructions.
- `tdd-requirement`: chooses the next observable outcome, gathers research, and
  prepares a handoff for judge review.
- `tdd-red`: writes a small failing test slice that captures the outcome, then
  sends it to the judge.
- `tdd-green`: makes those exact tests pass without changing them, then sends
  the result to the judge.
- `tdd-refactor`: refactors either tests or implementation in one pass, never
  both, then sends the result to the judge.
- The workflow is `requirement -> judge -> red -> judge -> green -> judge ->
  refactor -> judge`, then either stops or loops back to `requirement`.
- Agent-to-agent handoffs must stay in the foreground and use self-contained
  prompts so the next agent can continue in a fresh context without losing
  routing or verification details.
- These agents are intentionally language-agnostic: they must infer the active
  language, tooling, and test commands from the repository rather than assuming
  a stack.

## Git

Use Conventional Commits for commit messages when asked to commit:

- `feat: behavior added, changed or removed`
- `refactor: why thing needed to change`
- `test: behavior covered by tests`

Do not assume permission to commit unless explicitly asked.
