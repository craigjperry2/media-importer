use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use media_importer::catalog::Clock;
use media_importer::config::{DEFAULT_CHUNK_SIZE, ImportConfig, ImportOptions};
use media_importer::ingest::import_source_with_clock;
use predicates::prelude::*;
use rusqlite::{Connection, params};

#[test]
fn help_exposes_import_command() {
    let mut cmd = Command::cargo_bin("media-importer").expect("binary exists");

    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("import"))
        .stdout(predicate::str::contains("build-tree"));
}

#[test]
fn real_import_creates_cas_and_catalog_then_rerun_reuses_blobs() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source.child("a.txt").write_str("same").expect("write a");
    source.child("nested").create_dir_all().expect("nested dir");
    source
        .child("nested/b.txt")
        .write_str("same")
        .expect("write b");
    source
        .child(".hidden")
        .write_str("hidden")
        .expect("write hidden");
    #[cfg(unix)]
    symlink(
        source.child("a.txt").path(),
        source.child("linked.txt").path(),
    )
    .expect("symlink");

    let store = temp.child("store");
    store
        .child("staging")
        .create_dir_all()
        .expect("staging dir");
    store
        .child("staging/stale.tmp")
        .write_str("stale")
        .expect("stale staging");

    run_import(source.path(), store.path())
        .success()
        .stdout(predicate::str::contains("Import complete"))
        .stdout(predicate::str::contains("Files seen: 3"))
        .stdout(predicate::str::contains("Blobs created: 2"))
        .stdout(predicate::str::contains("Blobs reused: 1"))
        .stdout(predicate::str::contains("Source records inserted: 3"))
        .stdout(predicate::str::contains("Bytes written: 10"));

    assert!(!store.child("staging/stale.tmp").path().exists());
    assert_blob(store.path(), "same", true);
    assert_blob(store.path(), "hidden", true);
    assert_schema_version(&store.child("catalog.sqlite").path().to_path_buf(), 1);
    assert_catalog_counts(store.path(), 2, 3);
    assert_source_row(store.path(), "a.txt", 1, "same");
    assert_source_row(store.path(), "nested/b.txt", 1, "same");
    assert_source_row(store.path(), ".hidden", 1, "hidden");

    run_import(source.path(), store.path())
        .success()
        .stdout(predicate::str::contains("Files seen: 3"))
        .stdout(predicate::str::contains("Blobs created: 0"))
        .stdout(predicate::str::contains("Blobs reused: 3"))
        .stdout(predicate::str::contains("Files skipped: 3"))
        .stdout(predicate::str::contains("Bytes skipped: 14"))
        .stdout(predicate::str::contains("Files hashed: 0"))
        .stdout(predicate::str::contains("Source records inserted: 0"))
        .stdout(predicate::str::contains("Source records updated: 3"));

    assert_source_row(store.path(), "a.txt", 2, "same");
    assert_catalog_counts(store.path(), 2, 3);
}

#[test]
fn unchanged_repeat_import_skips_source_reads_and_refreshes_observation() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source
        .child("a.txt")
        .write_str("alpha")
        .expect("write file");
    let store = temp.child("store");

    run_import(source.path(), store.path()).success();
    let before = source_observation_state(store.path(), "a.txt");
    let config = import_config(source.path(), store.path());
    let report =
        import_source_with_clock(config, &FixedClock(9_999_999_999_999)).expect("repeat import");

    assert_eq!(report.files_skipped, 1);
    assert_eq!(report.bytes_skipped, 5);
    assert_eq!(report.files_hashed, 0);
    assert_eq!(report.bytes_hashed, 0, "skip must not read source content");
    assert_eq!(
        source_observation_state(store.path(), "a.txt"),
        (9_999_999_999_999, 2)
    );
    assert!(
        before.0 < 9_999_999_999_999,
        "fixed clock must prove last_seen refresh"
    );
}

#[test]
fn changed_source_path_updates_catalog_observation() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source
        .child("photo.txt")
        .write_str("old")
        .expect("write old");
    let store = temp.child("store");

    run_import(source.path(), store.path()).success();
    thread::sleep(Duration::from_millis(5));
    source
        .child("photo.txt")
        .write_str("new")
        .expect("write new");
    run_import(source.path(), store.path()).success();

    assert_catalog_counts(store.path(), 2, 1);
    assert_source_row(store.path(), "photo.txt", 2, "new");
}

#[test]
fn changed_source_size_forces_content_hashing() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source.child("a.txt").write_str("old").expect("write file");
    let store = temp.child("store");

    run_import(source.path(), store.path()).success();
    source
        .child("a.txt")
        .write_str("larger")
        .expect("change size");
    run_import(source.path(), store.path())
        .success()
        .stdout(predicate::str::contains("Files skipped: 0"))
        .stdout(predicate::str::contains("Files hashed: 1"))
        .stdout(predicate::str::contains("Bytes hashed: 6"));
    assert_source_row(store.path(), "a.txt", 2, "larger");
}

#[test]
fn unavailable_catalog_mtime_forces_content_hashing() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source
        .child("a.txt")
        .write_str("alpha")
        .expect("write file");
    let store = temp.child("store");

    run_import(source.path(), store.path()).success();
    let connection = Connection::open(store.child("catalog.sqlite").path()).expect("open db");
    connection
        .execute("UPDATE source_files SET modified_at_ms = NULL", [])
        .expect("clear advisory mtime");
    drop(connection);

    run_import(source.path(), store.path())
        .success()
        .stdout(predicate::str::contains("Files skipped: 0"))
        .stdout(predicate::str::contains("Files hashed: 1"))
        .stdout(predicate::str::contains("Bytes hashed: 5"));
}

#[test]
fn no_metadata_skip_forces_content_hashing_of_an_unchanged_file() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source
        .child("a.txt")
        .write_str("alpha")
        .expect("write file");
    let store = temp.child("store");

    run_import(source.path(), store.path()).success();
    run_import_args(source.path(), store.path(), &["--no-metadata-skip"])
        .success()
        .stdout(predicate::str::contains("Files skipped: 0"))
        .stdout(predicate::str::contains("Files hashed: 1"))
        .stdout(predicate::str::contains("Bytes hashed: 5"));
    assert_source_row(store.path(), "a.txt", 2, "alpha");
}

#[test]
fn changed_same_size_content_is_hashed_after_mtime_changes() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source
        .child("a.txt")
        .write_str("alpha")
        .expect("write file");
    let store = temp.child("store");

    run_import(source.path(), store.path()).success();
    thread::sleep(Duration::from_millis(5));
    source
        .child("a.txt")
        .write_str("bravo")
        .expect("replace file");
    run_import(source.path(), store.path())
        .success()
        .stdout(predicate::str::contains("Files skipped: 0"))
        .stdout(predicate::str::contains("Files hashed: 1"));
    assert_source_row(store.path(), "a.txt", 2, "bravo");
}

#[test]
fn missing_cataloged_cas_blob_falls_back_to_hash_and_recovers() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source
        .child("a.txt")
        .write_str("alpha")
        .expect("write file");
    let store = temp.child("store");

    run_import(source.path(), store.path()).success();
    fs::remove_file(blob_path(store.path(), "alpha")).expect("remove CAS blob");
    run_import(source.path(), store.path())
        .success()
        .stdout(predicate::str::contains("Files skipped: 0"))
        .stdout(predicate::str::contains("Files hashed: 1"))
        .stdout(predicate::str::contains("Blobs created: 1"));
    assert_blob(store.path(), "alpha", true);
}

#[test]
fn wrong_size_cataloged_cas_blob_is_hashed_then_fails_safely() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source
        .child("a.txt")
        .write_str("alpha")
        .expect("write file");
    let store = temp.child("store");

    run_import(source.path(), store.path()).success();
    let blob = blob_path(store.path(), "alpha");
    fs::remove_file(&blob).expect("remove valid blob");
    fs::write(&blob, "bad").expect("install wrong-size blob");

    run_import(source.path(), store.path())
        .failure()
        .stderr(predicate::str::contains(
            "CAS blob exists with unexpected size",
        ));
    assert_source_row(store.path(), "a.txt", 1, "alpha");
}

#[cfg(unix)]
#[test]
fn source_root_alias_reuses_one_identity_without_hashing() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source
        .child("a.txt")
        .write_str("alpha")
        .expect("write file");
    let alias = temp.child("source-alias");
    symlink(source.path(), alias.path()).expect("create source alias");
    let store = temp.child("store");

    run_import(source.path(), store.path()).success();
    run_import(alias.path(), store.path())
        .success()
        .stdout(predicate::str::contains("Files skipped: 1"))
        .stdout(predicate::str::contains("Files hashed: 0"));

    let connection = Connection::open(store.child("catalog.sqlite").path()).expect("open db");
    let (rows, seen_count): (i64, i64) = connection
        .query_row(
            "SELECT COUNT(*), MAX(seen_count) FROM source_files WHERE relative_path = 'a.txt'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read canonical source identity");
    assert_eq!(rows, 1);
    assert_eq!(seen_count, 2);
}

#[test]
fn metadata_skip_resurrects_a_marked_blob_without_hashing() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source
        .child("a.txt")
        .write_str("alpha")
        .expect("write file");
    let store = temp.child("store");

    run_import(source.path(), store.path()).success();
    let hash = blake3::hash(b"alpha").to_hex().to_string();
    let connection = Connection::open(store.child("catalog.sqlite").path()).expect("open db");
    connection
        .execute(
            "UPDATE blobs SET deleted_at_ms = 1 WHERE hash = ?1",
            [&hash],
        )
        .expect("mark blob");
    drop(connection);

    run_import(source.path(), store.path())
        .success()
        .stdout(predicate::str::contains("Files skipped: 1"))
        .stdout(predicate::str::contains("Files hashed: 0"));
    let connection = Connection::open(store.child("catalog.sqlite").path()).expect("open db");
    let mark: Option<i64> = connection
        .query_row(
            "SELECT deleted_at_ms FROM blobs WHERE hash = ?1",
            [&hash],
            |row| row.get(0),
        )
        .expect("read mark");
    assert_eq!(mark, None);
}

#[test]
fn dry_run_missing_store_creates_nothing() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source
        .child("a.txt")
        .write_str("alpha")
        .expect("write file");
    let store = temp.child("missing-store");

    run_import_args(source.path(), store.path(), &["--dry-run"])
        .success()
        .stdout(predicate::str::contains("Dry run complete"))
        .stdout(predicate::str::contains("Files seen: 1"))
        .stdout(predicate::str::contains("Blobs that would be created: 1"));

    assert!(!store.path().exists());
}

#[test]
fn dry_run_existing_store_does_not_mutate_catalog_or_staging() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source
        .child("a.txt")
        .write_str("alpha")
        .expect("write file");
    let store = temp.child("store");

    run_import(source.path(), store.path()).success();
    store
        .child("staging/stale.tmp")
        .write_str("stale")
        .expect("write stale");
    let db = store.child("catalog.sqlite").path().to_path_buf();
    let before = fs::metadata(&db).expect("db metadata").modified().ok();

    run_import_args(source.path(), store.path(), &["--dry-run"])
        .success()
        .stdout(predicate::str::contains("Blobs that would be reused: 1"))
        .stdout(predicate::str::contains(
            "Files that would skip content reads: 1",
        ))
        .stdout(predicate::str::contains("Files that would be hashed: 0"))
        .stdout(predicate::str::contains(
            "Source records that would be updated: 1",
        ));

    assert!(store.child("staging/stale.tmp").path().exists());
    assert_eq!(
        before,
        fs::metadata(&db).expect("db metadata").modified().ok()
    );
    assert_source_row(store.path(), "a.txt", 1, "alpha");
}

#[test]
fn empty_source_directory_succeeds() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    let store = temp.child("store");

    run_import(source.path(), store.path())
        .success()
        .stdout(predicate::str::contains("Files seen: 0"));

    assert_catalog_counts(store.path(), 0, 0);
}

#[test]
fn rejects_source_store_overlap_and_db_inside_source() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");

    run_import(source.path(), source.child("store").path())
        .failure()
        .stderr(predicate::str::contains(
            "source and store paths must not overlap",
        ));

    let outside_store = temp.child("store");
    let db_inside_source = source.child("catalog.sqlite");
    run_import_args(
        source.path(),
        outside_store.path(),
        &["--db", db_inside_source.path().to_str().unwrap()],
    )
    .failure()
    .stderr(predicate::str::contains(
        "database path must not be inside source directory",
    ));
}

#[test]
fn newer_schema_version_fails_clearly() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source
        .child("a.txt")
        .write_str("alpha")
        .expect("write file");
    let store = temp.child("store");
    store.create_dir_all().expect("store dir");
    let db = store.child("catalog.sqlite");
    let connection = Connection::open(db.path()).expect("open db");
    connection
        .execute_batch("PRAGMA user_version = 2;")
        .expect("set version");
    drop(connection);

    run_import(source.path(), store.path())
        .failure()
        .stderr(predicate::str::contains("newer than supported version 1"));
}

#[cfg(unix)]
#[test]
fn build_tree_creates_relative_symlinks_and_rerun_is_unchanged() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source.child("Movies").create_dir_all().expect("movies dir");
    source
        .child("Movies/clip.mov")
        .write_str("clip")
        .expect("clip");
    source.child(".env").write_str("env").expect("env");
    source.child("README").write_str("readme").expect("readme");
    let store = temp.child("store");
    let browse = temp.child("browse");

    run_import(source.path(), store.path()).success();
    run_build_tree(store.path(), browse.path(), &[])
        .success()
        .stdout(predicate::str::contains("Build tree complete"))
        .stdout(predicate::str::contains("Desired links: 3"))
        .stdout(predicate::str::contains("Links created: 3"))
        .stdout(predicate::str::contains("Directories created: 2"));

    assert_materialized_link(
        browse.path(),
        store.path(),
        "Movies/clip.mov",
        "Movies/clip_",
        ".mov",
        "clip",
    );
    assert_materialized_link(browse.path(), store.path(), ".env", ".env_", "", "env");
    assert_materialized_link(
        browse.path(),
        store.path(),
        "README",
        "README_",
        "",
        "readme",
    );

    run_build_tree(store.path(), browse.path(), &[])
        .success()
        .stdout(predicate::str::contains("Links created: 0"))
        .stdout(predicate::str::contains("Links unchanged: 3"));
}

#[cfg(unix)]
#[test]
fn build_tree_dry_run_reports_without_mutating() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source.child("a.txt").write_str("alpha").expect("alpha");
    let store = temp.child("store");
    let browse = temp.child("browse");

    run_import(source.path(), store.path()).success();
    run_build_tree(store.path(), browse.path(), &["--dry-run"])
        .success()
        .stdout(predicate::str::contains("Dry run complete"))
        .stdout(predicate::str::contains("Links that would be created: 1"))
        .stdout(predicate::str::contains(
            "Directories that would be created: 1",
        ));

    assert!(!browse.path().exists());
}

#[cfg(unix)]
#[test]
fn build_tree_rejects_missing_catalog_and_db_inside_browse_tree() {
    let temp = TempDir::new().expect("tempdir");
    let store = temp.child("store");
    store.child("blobs").create_dir_all().expect("blobs dir");
    let browse = temp.child("browse");
    browse.create_dir_all().expect("browse dir");

    run_build_tree(store.path(), browse.path(), &[])
        .failure()
        .stderr(predicate::str::contains("catalog database does not exist"));

    run_build_tree(
        store.path(),
        browse.path(),
        &[
            "--db",
            browse.child("catalog.sqlite").path().to_str().unwrap(),
        ],
    )
    .failure()
    .stderr(predicate::str::contains(
        "database path must not be inside browse tree",
    ));
}

#[cfg(unix)]
#[test]
fn build_tree_removes_stale_owned_symlinks_without_pruning_nonempty_dirs() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source.child("a.txt").write_str("alpha").expect("alpha");
    let store = temp.child("store");
    let browse = temp.child("browse");

    run_import(source.path(), store.path()).success();
    let stale_dir = browse.child("stale/sub");
    stale_dir.create_dir_all().expect("stale dir");
    stale_dir
        .child("keep.txt")
        .write_str("keep")
        .expect("keep file");
    let blobs = store
        .child("blobs")
        .path()
        .canonicalize()
        .expect("canonical blobs dir");
    let stale_link = stale_dir.child("old_link");
    symlink(blobs.join("missing-owned-blob"), stale_link.path()).expect("stale symlink");

    run_build_tree(store.path(), browse.path(), &[])
        .success()
        .stdout(predicate::str::contains("Stale links removed: 1"))
        .stdout(predicate::str::contains("Directories pruned: 0"));

    assert!(
        fs::symlink_metadata(stale_link.path()).is_err(),
        "stale symlink should be removed"
    );
    assert!(stale_dir.child("keep.txt").path().exists());
    assert!(stale_dir.path().is_dir());
}

#[cfg(unix)]
#[test]
fn build_tree_rejects_cas_blob_paths_that_are_symlinks() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source.child("a.txt").write_str("alpha").expect("alpha");
    let store = temp.child("store");
    let browse = temp.child("browse");

    run_import(source.path(), store.path()).success();
    let blob = blob_path(store.path(), "alpha");
    let real_blob = blob.with_extension("real");
    fs::rename(&blob, &real_blob).expect("move blob aside");
    symlink(&real_blob, &blob).expect("replace blob path with symlink");

    run_build_tree(store.path(), browse.path(), &[])
        .failure()
        .stderr(predicate::str::contains("CAS blob is not a regular file"));
    assert!(!browse.path().exists());
}

#[cfg(unix)]
#[test]
fn build_tree_replaces_owned_absolute_symlinks_and_preserves_user_symlinks() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.child("source");
    source.create_dir_all().expect("source dir");
    source.child("a.txt").write_str("alpha").expect("alpha");
    let store = temp.child("store");
    let browse = temp.child("browse");

    run_import(source.path(), store.path()).success();
    run_build_tree(store.path(), browse.path(), &[]).success();

    let output = materialized_path(browse.path(), "a_", ".txt", "alpha");
    fs::remove_file(&output).expect("remove materialized link");
    symlink(
        blob_path(store.path(), "alpha")
            .canonicalize()
            .expect("canonical blob"),
        &output,
    )
    .expect("absolute owned symlink");

    let user_target = temp.child("user-target");
    user_target.write_str("user").expect("user target");
    let user_link = browse.child("user-link");
    symlink(user_target.path(), user_link.path()).expect("user symlink");

    run_build_tree(store.path(), browse.path(), &[])
        .success()
        .stdout(predicate::str::contains("Links replaced: 1"))
        .stdout(predicate::str::contains("Stale links removed: 0"));

    let target = fs::read_link(&output).expect("read replaced link");
    assert!(
        !target.is_absolute(),
        "target should be relative: {target:?}"
    );
    assert_eq!(
        output
            .parent()
            .expect("link parent")
            .join(&target)
            .canonicalize()
            .expect("canonical symlink target"),
        blob_path(store.path(), "alpha")
            .canonicalize()
            .expect("canonical blob path")
    );
    assert!(user_link.path().is_symlink());
}

fn run_import(source: &Path, store: &Path) -> assert_cmd::assert::Assert {
    run_import_args(source, store, &[])
}

fn run_import_args(source: &Path, store: &Path, extra: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("media-importer").expect("binary exists");
    cmd.arg("import")
        .arg("--store")
        .arg(store)
        .arg("--source")
        .arg(source)
        .args(extra)
        .assert()
}

struct FixedClock(i64);

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

fn import_config(source: &Path, store: &Path) -> ImportConfig {
    ImportConfig::from_options(ImportOptions {
        store: store.to_path_buf(),
        source: source.to_path_buf(),
        db: None,
        dry_run: false,
        metadata_skip: true,
        chunk_size: DEFAULT_CHUNK_SIZE,
    })
    .expect("import config")
}

fn source_observation_state(store: &Path, relative_path: &str) -> (i64, i64) {
    let connection = Connection::open(store.join("catalog.sqlite")).expect("open db");
    connection
        .query_row(
            "SELECT last_seen_at_ms, seen_count FROM source_files WHERE relative_path = ?1",
            params![relative_path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("source observation")
}

fn run_build_tree(store: &Path, browse: &Path, extra: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("media-importer").expect("binary exists");
    cmd.arg("build-tree")
        .arg("--store")
        .arg(store)
        .arg("--browse-tree")
        .arg(browse)
        .args(extra)
        .assert()
}

#[cfg(unix)]
fn assert_materialized_link(
    browse: &Path,
    store: &Path,
    source_relative_path: &str,
    expected_prefix: &str,
    expected_suffix: &str,
    contents: &str,
) {
    let output = materialized_path(browse, expected_prefix, expected_suffix, contents);
    assert!(output.is_symlink(), "expected symlink at {output:?}");
    let target = fs::read_link(&output).expect("read symlink");
    assert!(
        !target.is_absolute(),
        "target should be relative: {target:?}"
    );
    assert_eq!(
        output
            .parent()
            .expect("link parent")
            .join(&target)
            .canonicalize()
            .expect("canonical symlink target"),
        blob_path(store, contents)
            .canonicalize()
            .expect("canonical blob path")
    );

    let connection = Connection::open(store.join("catalog.sqlite")).expect("open db");
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM source_files WHERE relative_path = ?1",
            params![source_relative_path],
            |row| row.get(0),
        )
        .expect("source row count");
    assert_eq!(count, 1);
}

#[cfg(unix)]
fn materialized_path(
    browse: &Path,
    expected_prefix: &str,
    expected_suffix: &str,
    contents: &str,
) -> PathBuf {
    let hash = blake3::hash(contents.as_bytes()).to_hex().to_string();
    browse.join(format!("{expected_prefix}{}{expected_suffix}", &hash[..6]))
}

fn assert_blob(store: &Path, contents: &str, read_only: bool) {
    let path = blob_path(store, contents);
    assert!(path.exists(), "blob exists at {path:?}");
    assert_eq!(fs::read_to_string(&path).expect("read blob"), contents);
    #[cfg(unix)]
    if read_only {
        assert_eq!(
            fs::metadata(&path)
                .expect("blob metadata")
                .permissions()
                .mode()
                & 0o777,
            0o444
        );
    }
}

fn blob_path(store: &Path, contents: &str) -> PathBuf {
    let hash = blake3::hash(contents.as_bytes()).to_hex().to_string();
    store
        .join("blobs")
        .join(&hash[0..2])
        .join(&hash[2..4])
        .join(hash)
}

fn assert_schema_version(db_path: &PathBuf, expected: i64) {
    let connection = Connection::open(db_path).expect("open db");
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("schema version");
    assert_eq!(version, expected);
}

fn assert_catalog_counts(store: &Path, blobs: i64, source_files: i64) {
    let connection = Connection::open(store.join("catalog.sqlite")).expect("open db");
    let blob_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get(0))
        .expect("blob count");
    let source_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM source_files", [], |row| row.get(0))
        .expect("source count");
    assert_eq!(blob_count, blobs);
    assert_eq!(source_count, source_files);
}

fn assert_source_row(store: &Path, relative_path: &str, seen_count: i64, contents: &str) {
    let connection = Connection::open(store.join("catalog.sqlite")).expect("open db");
    let (blob_hash, stored_seen_count): (String, i64) = connection
        .query_row(
            "SELECT blob_hash, seen_count
             FROM source_files
             WHERE relative_path = ?1",
            params![relative_path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("source row");
    assert_eq!(stored_seen_count, seen_count);
    assert_eq!(
        blob_hash,
        blake3::hash(contents.as_bytes()).to_hex().to_string()
    );
}
