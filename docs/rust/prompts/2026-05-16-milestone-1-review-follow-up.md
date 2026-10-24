# 2026-05-16 Milestone 1 Review

> /review the most recent git commit by a coding agent which was tasked with implementing milestone 1 in [IMPLEMENTATION.md](docs/from-scratch-in-rust/IMPLEMENTATION.md)
>
> I've observed a few things that look questionable to me:
>
> - inline sql should live in .sql files and then include_str! them into the source - this allows for syntax highlighting and easier editing by developers. Worthy of updating the [SQLITE.instructions.md](docs/from-scratch-in-rust/SQLITE.instructions.md) too
> - The open_if_exist in ReadOnlyCatalog [catalog.rs](crates/media-importer/src/catalog.rs) code path copies the sqlite db file - is that valid or should it use the sqlite ".backup" functionality? The WAL / SHM copy afterward smells dangerously wrong, this would need a filesystem snapshot to be safe
> - Create an architecture diagram in html, persist in docs/from-scratch-in-rust/architecture.html
> - i deleted some code which was generated but not used, the coding agent had used the allow dead code annotation on the function rather than deleting it. Is this best avoided in future through an update to [AGENTS.md](AGENTS.md)
> - In SourceRelativePath.from_path is there a viable symlink attack here?
> - i think source path clashes should be an error, the test case changed_source_path_updates_catalog_observation seems like unwanted behaviour, was this a miss in my [IMPLEMENTATION.md](docs/from-scratch-in-rust/IMPLEMENTATION.md)  ?

<details><summary>6 previous messages</summary>

> I’ll review the latest commit against the milestone docs and your specific concerns, then separate concrete defects from doc/process updates. I’ll inspect the commit diff first, then the catalog/path code and relevant tests.
>
> <details><summary>Explored 1 search, ran 2 commands</summary>
>
> - Ran `git status --short`
>
> - Ran `git log -1 --stat --oneline`
>
> <details><summary>Explored 1 search</summary>
>
> - Searched for `Milestone 1|source path|path clash|sqlite|SQL|dead code|allow\(dead_code\)|SourceRelativePath|ReadOnlyCatalog|open_if_exist|changed_source_path_updates_catalog_observation` in `from-scratch-in-rust`
>
> </details>
>
> </details>
>
> The latest commit is `b82d865 feat: add catalog and import functionality (milestone 1)`. I’m narrowing now on the parts where the behavior could be unsound: SQLite snapshotting, source-path identity, and relative path validation.
>
> <details><summary>Explored 10 files, 1 search, ran 1 command</summary>
>
> <details><summary>Explored 3 files</summary>
>
> - Read `./crates/media-importer/src/catalog.rs`
> - Read `./crates/media-importer/tests/import_milestone.rs`
> - Read `./crates/media-importer/src/paths.rs`
>
> </details>
>
> - Ran `git show --format=fuller --name-only --stat HEAD`
>
> <details><summary>Explored 7 files, 1 search</summary>
>
> - Read `./crates/media-importer/src/store.rs`
> - Read `./crates/media-importer/src/ingest.rs`
> - Read `./crates/media-importer/src/scanner.rs`
> - Read `./docs/from-scratch-in-rust/IMPLEMENTATION.md`
> - Read `./crates/media-importer/src/cli.rs`
> - Read `./docs/from-scratch-in-rust/SQLITE.instructions.md`
> - Read `./crates/media-importer/src/config.rs`
> - Searched for `allow\(|dead_code|include_str|execute_batch|execute\(|query_row\(` in `media-importer`
>
> </details>
>
> </details>
>
> The implementation does match several explicit milestone bullets, including “changed content at same source path updates the row.” That means one of your concerns is a spec question rather than only an implementation miss; I’m checking whether the current spec creates an unsafe or undesirable behavior in practice.
>
> <details><summary>Explored 2 files, 1 list, ran 4 commands</summary>
>
> <details><summary>Explored 2 files</summary>
>
> - Read `./AGENTS.md`
> - Read `./docs/from-scratch-in-rust/RUST.instructions.md`
>
> </details>
>
> - Ran `cargo test --workspace`
>
> <details><summary>Explored 1 list</summary>
>
> - Listed files in `from-scratch-in-rust`
>
> </details>
>
> - Ran `git show --stat --oneline HEAD^..HEAD`
>
> - Ran `cargo clippy --workspace --all-targets -- -D warnings`
>
> - Ran `cargo fmt --all --check`
>
> </details>
</details>

**Findings**

- **P1** [catalog.rs](./crates/media-importer/src/catalog.rs:303): `ReadOnlyCatalog::open_if_exists` copies `catalog.sqlite`, then separately copies `-wal`/`-shm`. Your concern is valid. That is not a safe SQLite snapshot unless the filesystem snapshot is atomic across all three files. Use SQLite’s backup API from a read-only connection into temp/in-memory DB, or avoid cloning and open read-only without creating sidecars.

- **P1** [scanner.rs](./crates/media-importer/src/scanner.rs:23) / [store.rs](./crates/media-importer/src/store.rs:55): there is a viable TOCTOU symlink attack if the source tree is attacker-controlled or mutable during import. `WalkDir` ignores symlink entries, but after that check the code calls `fs::metadata` and later `File::open`, both path-based and symlink-following. A regular file can be swapped for a symlink between scan/stat/open, causing import of content outside the source root. Fix needs no-follow/openat-style handling or inode/device verification around an already-open file.

- **P2** [catalog.rs](./crates/media-importer/src/catalog.rs:149): same `(source_root, relative_path)` with changed content is silently updated. This is not an implementation miss against the current spec: [IMPLEMENTATION.md](./docs/from-scratch-in-rust/IMPLEMENTATION.md:426) and [IMPLEMENTATION.md](./docs/from-scratch-in-rust/IMPLEMENTATION.md:593) explicitly require this behavior. If path clashes should be errors, update the spec and replace the upsert with “same path + different hash/size => fail”.

- **P3** [catalog.rs](./crates/media-importer/src/catalog.rs:78): inline SQL is widespread. I agree this should move into `.sql` files and be pulled in with `include_str!`, especially the schema/migration SQL. [SQLITE.instructions.md](./docs/from-scratch-in-rust/SQLITE.instructions.md:5) should say that project SQL lives in colocated `.sql` files unless it is a trivial one-liner.

- **P3** `docs/from-scratch-in-rust/architecture.html` is missing. I would not mark this as a milestone-1 regression because `IMPLEMENTATION.md` does not currently require it, but it is a useful follow-up artifact.

**Other Notes**

`#[allow(dead_code)]` is already discouraged by [RUST.instructions.md](./docs/from-scratch-in-rust/RUST.instructions.md:76), but adding a direct AGENTS.md rule like “delete unused generated code; do not use `#[allow(dead_code)]` except for documented FFI/platform boundaries” would make this harder for future agents to miss.

Validation passed locally: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`.

> Address the ReadOnlyCatalog::open_if_exists issue by using the sqlite backup feature
>
> Start a new SECURITY.md file in the repo root and capture the TOCTOU symlink attack possibility as a known issue. Attacker writable source path is not an attack vector we'll address in this project but we should document the behaviour.
>
> Extract the inline sql to .sql files and update the SQLITE.instructions.md
>
> Perform the AGENTS.md update for dead code
>
> Finally, capture this conversation in [prompts](docs/from-scratch-in-rust/prompts/)

<details><summary>14 previous messages</summary>

> I’ll make this as a focused follow-up patch: fix read-only catalog snapshotting first, then move SQL into files and update the docs/process notes. After edits I’ll run the Rust checks so we know the feature flag and backup API are wired correctly.
>
> <details><summary>Explored 2 files, 3 searches, 1 list, ran 2 commands</summary>
>
> <details><summary>Explored 1 file, 1 search</summary>
>
> - Read `./crates/media-importer/Cargo.toml`
> - Searched for `execute_batch\(|execute\(|query_row\(` in `catalog.rs`
>
> </details>
>
> - Ran `git status --short`
>
> <details><summary>Explored 1 search</summary>
>
> - Searched for `pub struct Backup|mod backup|feature.*backup|fn backup|backup::` in `registry`
>
> </details>
>
> - Ran `pgrep -fl "rg -n pub struct Backup"`
>
> <details><summary>Explored 1 file, 1 search, 1 list</summary>
>
> - Read `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/rusqlite-0.39.0/src/backup.rs`
> - Searched for `backup` in `Cargo.toml`
> - Listed files in `media-importer`
>
> </details>
>
> </details>
>
> The backup API is behind rusqlite’s `backup` feature, so this patch will also adjust the crate feature list. I’m going to keep the read-only catalog backed by a temp database, but create that temp DB through SQLite’s online backup API instead of filesystem-copying the DB/WAL/SHM files.
>
> <details><summary>Explored 1 search, ran 1 command</summary>
>
> <details><summary>Explored 1 search</summary>
>
> - Searched for `pub const MAIN_DB|enum DatabaseName|struct DatabaseName|trait Name|impl Name` in `rusqlite-0.39.0`
>
> </details>
>
> - Ran `mkdir -p crates/media-importer/src/catalog/sql`
>
> </details>
>
> I’m adding the SQL files under the catalog module so the Rust source only names query constants. The schema and upserts will be easier to review/edit with SQL tooling, while keeping all raw SQL owned by the catalog boundary.
>
> <details><summary>Created 7 files, edited 2 files</summary>
>
> - Wrote `./crates/media-importer/src/catalog/sql/insert_blob.sql` (+2 -0)
> - Wrote `./crates/media-importer/src/catalog/sql/read_user_version.sql` (+1 -0)
> - Wrote `./crates/media-importer/src/catalog/sql/schema_v1.sql` (+27 -0)
> - Wrote `./crates/media-importer/src/catalog/sql/select_blob_size.sql` (+3 -0)
> - Wrote `./crates/media-importer/src/catalog/sql/select_source_file_id.sql` (+3 -0)
> - Wrote `./crates/media-importer/src/catalog/sql/upsert_source_file.sql` (+17 -0)
> - Wrote `./crates/media-importer/src/catalog/sql/writable_pragmas.sql` (+3 -0)
> - Wrote `./crates/media-importer/Cargo.toml` (+1 -1)
> - Wrote `./crates/media-importer/src/catalog.rs` (+24 -86)
>
> </details>
>
> The code path is now changed to use SQLite’s backup API. Next I’m documenting the symlink race as an explicit known issue/non-goal, then tightening the project instructions so future changes keep SQL out of Rust string literals and avoid dead-code suppressions.
>
> <details><summary>Created 2 files, edited 2 files</summary>
>
> - Wrote `./AGENTS.md` (+2 -1)
> - Wrote `./SECURITY.md` (+20 -0)
> - Wrote `./docs/from-scratch-in-rust/SQLITE.instructions.md` (+7 -3)
> - Wrote `./docs/from-scratch-in-rust/prompts/2026-05-16-milestone-1-review-follow-up.md` (+43 -0)
>
> </details>
>
> Docs and instruction updates are in place. I’m running the checks now; the main thing I expect to shake out is whether rusqlite’s backup feature needs any additional API adjustment beyond the feature flag.
>
> <details><summary>Ran 2 commands</summary>
>
> - Ran `cargo fmt --all --check`
> - Ran `cargo test --workspace`
>
> </details>
>
> The backup API compiled and the test suite passed. Formatting found two style nits in `catalog.rs`, so I’m applying rustfmt and then I’ll run the full lint/test set once more.
>
> <details><summary>Edited 1 file, explored 1 file, 1 search, ran 12 commands</summary>
>
> - Ran `cargo fmt --all`
>
> - Ran `cargo clippy --workspace --all-targets -- -D warnings`
>
> - Ran `cargo fmt --all --check`
>
> - Ran `cargo test --workspace`
>
> - Ran `git diff --stat`
>
> - Ran `git status --short`
>
> - Ran `git diff -- crates/media-importer/src/catalog.rs crates/media-importer/Cargo.toml`
>
> - Ran `git diff -- AGENTS.md docs/from-scratch-in-rust/SQLITE.instructions.md SECURITY.md docs/from-scratch-in-rust/prompts/2026-05-16-milestone-1-review-follow-up.md`
>
> <details><summary>Explored 1 file, 1 search</summary>
>
> - Read `./crates/media-importer/src/catalog.rs`
> - Searched for `SELECT|INSERT|CREATE|PRAGMA|ON CONFLICT|UPDATE|DELETE|sqlite` in `catalog.rs`
>
> </details>
>
> - Ran `git diff --stat -- AGENTS.md crates/media-importer/Cargo.toml crates/media-importer/src/catalog.rs docs/from-scratch-in-rust/SQLITE.instructions.md`
>
> - Wrote `./crates/media-importer/src/catalog.rs` (+17 -8)
>
> - Ran `cargo clippy --workspace --all-targets -- -D warnings`
>
> - Ran `cargo test --workspace`
>
> - Ran `cargo fmt --all --check`
>
> </details>
>
> All Rust checks are green after the cleanup. I’m doing one final diff/status pass now to make sure the new SQL and documentation files are accounted for and there are no unexpected lockfile or generated changes.
>
> <details><summary>Explored 5 files, 1 list, ran 2 commands</summary>
>
> <details><summary>Explored 2 files, 1 list</summary>
>
> - Listed files in `sql`
> - Read `./docs/from-scratch-in-rust/prompts/2026-05-16-milestone-1-review-follow-up.md`
> - Read `./SECURITY.md`
>
> </details>
>
> - Ran `git status --short`
>
> - Ran `git diff --stat -- AGENTS.md crates/media-importer/Cargo.toml crates/media-importer/src/catalog.rs docs/from-scratch-in-rust/SQLITE.instructions.md`
>
> <details><summary>Explored 3 files</summary>
>
> - Read `./docs/from-scratch-in-rust/SQLITE.instructions.md`
> - Read `./AGENTS.md`
> - Read `./crates/media-importer/src/catalog.rs`
>
> </details>
>
> </details>
</details>

Implemented the follow-up patch.

Changed [catalog.rs](./crates/media-importer/src/catalog.rs:255) so dry-run read-only catalog access now snapshots via SQLite’s backup API instead of copying `catalog.sqlite`/WAL/SHM files. Enabled rusqlite’s `backup` feature in [Cargo.toml](./crates/media-importer/Cargo.toml:18).

Extracted catalog SQL into `.sql` files under [crates/media-importer/src/catalog/sql](./crates/media-importer/src/catalog/sql/schema_v1.sql), and updated [SQLITE.instructions.md](./docs/from-scratch-in-rust/SQLITE.instructions.md:10) to require that pattern and prohibit unsafe DB/WAL/SHM copying.

Added [SECURITY.md](./SECURITY.md:5) documenting the attacker-writable source tree TOCTOU symlink race as a known issue/non-goal. Updated [AGENTS.md](./AGENTS.md:43) to prefer deleting unused generated code over `#[allow(dead_code)]`.

Captured the review conversation and remaining open items in [2026-05-16-milestone-1-review-follow-up.md](./docs/from-scratch-in-rust/prompts/2026-05-16-milestone-1-review-follow-up.md:1).

Verification passed:
`cargo fmt --all --check`
`cargo clippy --workspace --all-targets -- -D warnings`
`cargo test --workspace`
