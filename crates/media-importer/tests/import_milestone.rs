use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use predicates::prelude::*;
use rusqlite::{Connection, params};

#[test]
fn help_exposes_import_command() {
    let mut cmd = Command::cargo_bin("media-importer").expect("binary exists");

    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("import"));
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
        .stdout(predicate::str::contains("Source records inserted: 0"))
        .stdout(predicate::str::contains("Source records updated: 3"));

    assert_source_row(store.path(), "a.txt", 2, "same");
    assert_catalog_counts(store.path(), 2, 3);
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
    source
        .child("photo.txt")
        .write_str("new")
        .expect("write new");
    run_import(source.path(), store.path()).success();

    assert_catalog_counts(store.path(), 2, 1);
    assert_source_row(store.path(), "photo.txt", 2, "new");
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
