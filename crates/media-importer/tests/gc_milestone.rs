use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use predicates::prelude::*;
use rusqlite::Connection;

#[test]
fn gc_help_documents_lifecycle_hashing_dry_run_and_quiescence() {
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["gc", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("catalog source records"))
        .stdout(predicate::str::contains("marked before this run"))
        .stdout(predicate::str::contains("fully hashed"))
        .stdout(predicate::str::contains("Dry-run"))
        .stdout(predicate::str::contains("quiescent"));
}

#[test]
fn two_real_runs_mark_then_sweep_without_a_time_delay() {
    let temp = imported("collect-me");
    make_unreachable(&temp);
    let (hash, blob) = only_catalog_blob(&temp);

    gc(&temp, &[])
        .success()
        .stdout(predicate::str::contains(format!("MARK {hash} bytes=10")))
        .stdout(predicate::str::contains("Blobs marked: 1"))
        .stdout(predicate::str::contains("Blobs swept: 0"));
    assert!(blob.exists());
    assert!(deleted_at(&temp, &hash).is_some());

    gc(&temp, &["--chunk-size", "3"])
        .success()
        .stdout(predicate::str::contains(format!("SWEEP {hash} bytes=10")))
        .stdout(predicate::str::contains("Sweep candidates hashed: 1"))
        .stdout(predicate::str::contains("Bytes reclaimed: 10"));
    assert!(!blob.exists());
    assert_eq!(blob_count(&temp), 0);
    assert!(blob.parent().unwrap().exists(), "empty shard remains");

    gc(&temp, &[])
        .success()
        .stdout(predicate::str::contains("Catalog blobs: 0"))
        .stdout(predicate::str::contains("Blobs swept: 0"));
}

#[test]
fn repeated_dry_runs_preserve_database_and_report_the_same_fixed_plan() {
    let temp = imported("dry-run");
    make_unreachable(&temp);
    let db = temp.child("store/catalog.sqlite");
    let before = fs::read(db.path()).unwrap();
    let wal = sidecar(db.path(), "-wal");
    let shm = sidecar(db.path(), "-shm");
    assert!(!wal.exists());
    assert!(!shm.exists());

    let first = gc(&temp, &["--dry-run"])
        .success()
        .stdout(predicate::str::contains("WOULD_MARK"))
        .get_output()
        .stdout
        .clone();
    let second = gc(&temp, &["--dry-run"])
        .success()
        .stdout(predicate::str::contains("WOULD_MARK"))
        .get_output()
        .stdout
        .clone();

    assert_eq!(first, second);
    assert_eq!(before, fs::read(db.path()).unwrap());
    assert!(!wal.exists());
    assert!(!shm.exists());
    let (hash, blob) = only_catalog_blob(&temp);
    assert!(blob.exists());
    assert_eq!(deleted_at(&temp, &hash), None);
}

#[test]
fn marked_referenced_blob_is_resurrected_without_hashing() {
    let temp = imported("live");
    let (hash, blob) = only_catalog_blob(&temp);
    set_mark(&temp, &hash, 7);

    gc(&temp, &[])
        .success()
        .stdout(predicate::str::contains(format!("RESURRECT {hash}")))
        .stdout(predicate::str::contains("Sweep candidates hashed: 0"));
    assert!(blob.exists());
    assert_eq!(deleted_at(&temp, &hash), None);
}

#[test]
fn reimport_resurrects_atomically_and_restores_build_tree_visibility() {
    let temp = imported("reimported");
    let (hash, _) = only_catalog_blob(&temp);
    set_mark(&temp, &hash, 9);

    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["import", "--store"])
        .arg(temp.child("store").path())
        .arg("--source")
        .arg(temp.child("source").path())
        .assert()
        .success();
    assert_eq!(deleted_at(&temp, &hash), None);

    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["build-tree", "--store"])
        .arg(temp.child("store").path())
        .arg("--browse-tree")
        .arg(temp.child("browse").path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Desired links: 1"));
}

#[test]
fn all_new_marks_share_one_run_timestamp_and_render_in_hash_order() {
    let temp = TempDir::new().unwrap();
    temp.child("source").create_dir_all().unwrap();
    temp.child("source/a").write_str("first").unwrap();
    temp.child("source/b").write_str("second").unwrap();
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["import", "--store"])
        .arg(temp.child("store").path())
        .arg("--source")
        .arg(temp.child("source").path())
        .assert()
        .success();
    make_unreachable(&temp);

    let output = gc(&temp, &[]).success().get_output().stdout.clone();
    let output = String::from_utf8(output).unwrap();
    let database = connection(&temp);
    let mut statement = database
        .prepare("SELECT hash FROM blobs ORDER BY hash")
        .unwrap();
    let hashes: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(output.find(&hashes[0]).unwrap() < output.find(&hashes[1]).unwrap());
    let distinct_marks: i64 = connection(&temp)
        .query_row(
            "SELECT count(DISTINCT deleted_at_ms) FROM blobs",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(distinct_marks, 1);
}

#[test]
fn dry_run_of_prior_mark_hashes_candidate_but_does_not_sweep() {
    let temp = imported("chunked-candidate");
    make_unreachable(&temp);
    let (hash, blob) = only_catalog_blob(&temp);
    set_mark(&temp, &hash, 1);

    gc(&temp, &["--dry-run", "--chunk-size", "2"])
        .success()
        .stdout(predicate::str::contains(format!("WOULD_SWEEP {hash}")))
        .stdout(predicate::str::contains("Sweep candidates hashed: 1"))
        .stdout(predicate::str::contains(
            "Bytes that would be reclaimed: 17",
        ));
    assert!(blob.exists());
    assert_eq!(deleted_at(&temp, &hash), Some(1));
}

#[test]
fn same_size_corruption_of_a_non_candidate_is_not_hashed_by_gc() {
    let temp = imported("abcdef");
    let (hash, blob) = only_catalog_blob(&temp);
    writable(&blob);
    fs::write(&blob, b"ghijkl").unwrap();

    gc(&temp, &[])
        .success()
        .stdout(predicate::str::contains("Sweep candidates hashed: 0"))
        .stdout(predicate::str::contains("Findings: 0"));
    assert_eq!(deleted_at(&temp, &hash), None);
}

#[test]
fn interrupted_sweep_finalizes_catalog_without_claiming_reclaimed_bytes() {
    let temp = imported("gone");
    make_unreachable(&temp);
    let (hash, blob) = only_catalog_blob(&temp);
    set_mark(&temp, &hash, 1);
    fs::remove_file(&blob).unwrap();

    gc(&temp, &[])
        .success()
        .stdout(predicate::str::contains(format!(
            "SWEEP {hash} bytes=4 state=already-absent"
        )))
        .stdout(predicate::str::contains("CAS files removed: 0"))
        .stdout(predicate::str::contains("Bytes reclaimed: 0"));
    assert_eq!(blob_count(&temp), 0);
}

#[test]
fn orphan_and_candidate_hash_mismatch_block_every_mutation() {
    let orphan_case = imported("reachable");
    let orphan_content = b"orphan";
    let orphan_hash = blake3::hash(orphan_content).to_hex().to_string();
    let orphan_path = blob_path(&orphan_case, &orphan_hash);
    fs::create_dir_all(orphan_path.parent().unwrap()).unwrap();
    fs::write(&orphan_path, orphan_content).unwrap();

    gc(&orphan_case, &[])
        .code(2)
        .stdout(predicate::str::contains(format!(
            "ORPHAN_BLOB {orphan_hash}"
        )))
        .stdout(predicate::str::contains("GC blocked"));
    assert!(orphan_path.exists());
    let (live_hash, _) = only_catalog_blob(&orphan_case);
    assert_eq!(deleted_at(&orphan_case, &live_hash), None);

    let corrupt_case = imported("abcdef");
    make_unreachable(&corrupt_case);
    let (hash, blob) = only_catalog_blob(&corrupt_case);
    set_mark(&corrupt_case, &hash, 1);
    writable(&blob);
    fs::write(&blob, b"ghijkl").unwrap();

    gc(&corrupt_case, &[])
        .code(2)
        .stdout(predicate::str::contains(format!("HASH_MISMATCH {hash}")))
        .stdout(predicate::str::contains("GC blocked"));
    assert!(blob.exists());
    assert_eq!(deleted_at(&corrupt_case, &hash), Some(1));
}

#[test]
fn real_gc_rejects_non_wal_catalog_without_changing_its_mode() {
    let temp = imported("mode");
    let db = temp.child("store/catalog.sqlite");
    let connection = Connection::open(db.path()).unwrap();
    let mode: String = connection
        .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
    drop(connection);

    gc(&temp, &[]).code(1).stderr(predicate::str::contains(
        "requires an existing WAL-mode catalog",
    ));
    let connection = Connection::open(db.path()).unwrap();
    let mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
}

fn imported(content: &str) -> TempDir {
    let temp = TempDir::new().unwrap();
    temp.child("source").create_dir_all().unwrap();
    temp.child("source/file.bin").write_str(content).unwrap();
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["import", "--store"])
        .arg(temp.child("store").path())
        .arg("--source")
        .arg(temp.child("source").path())
        .assert()
        .success();
    temp
}

fn gc(temp: &TempDir, extra: &[&str]) -> assert_cmd::assert::Assert {
    let mut command = Command::cargo_bin("media-importer").unwrap();
    command
        .args(["gc", "--store"])
        .arg(temp.child("store").path())
        .args(extra);
    command.assert()
}

fn connection(temp: &TempDir) -> Connection {
    Connection::open(temp.child("store/catalog.sqlite").path()).unwrap()
}

fn make_unreachable(temp: &TempDir) {
    connection(temp)
        .execute("DELETE FROM source_files", [])
        .unwrap();
}

fn set_mark(temp: &TempDir, hash: &str, value: i64) {
    connection(temp)
        .execute(
            "UPDATE blobs SET deleted_at_ms=?2 WHERE hash=?1",
            (hash, value),
        )
        .unwrap();
}

fn deleted_at(temp: &TempDir, hash: &str) -> Option<i64> {
    connection(temp)
        .query_row(
            "SELECT deleted_at_ms FROM blobs WHERE hash=?1",
            [hash],
            |row| row.get(0),
        )
        .unwrap()
}

fn blob_count(temp: &TempDir) -> i64 {
    connection(temp)
        .query_row("SELECT count(*) FROM blobs", [], |row| row.get(0))
        .unwrap()
}

fn only_catalog_blob(temp: &TempDir) -> (String, PathBuf) {
    let hash: String = connection(temp)
        .query_row("SELECT hash FROM blobs", [], |row| row.get(0))
        .unwrap();
    let path = blob_path(temp, &hash);
    (hash, path)
}

fn blob_path(temp: &TempDir, hash: &str) -> PathBuf {
    temp.path()
        .join("store/blobs")
        .join(&hash[..2])
        .join(&hash[2..4])
        .join(hash)
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(unix)]
fn writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
}

#[cfg(not(unix))]
fn writable(path: &Path) {
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions).unwrap();
}
