use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use predicates::prelude::*;
use rusqlite::Connection;
use std::fs;
use std::path::Path;

fn import(temp: &TempDir) {
    temp.child("source").create_dir_all().unwrap();
    temp.child("source/a.txt").write_str("abcdef").unwrap();
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["import", "--store"])
        .arg(temp.child("store").path())
        .arg("--source")
        .arg(temp.child("source").path())
        .assert()
        .success();
}

#[test]
fn help_exposes_audit_but_not_gc() {
    Command::cargo_bin("media-importer")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("audit"))
        .stdout(predicate::str::contains("gc").not());
}

#[test]
fn clean_imported_store_audits_with_exact_counters() {
    let temp = TempDir::new().unwrap();
    import(&temp);
    Command::cargo_bin("media-importer").unwrap().args(["audit", "--store"])
        .arg(temp.child("store").path()).arg("--chunk-size").arg("2").assert().success()
        .stdout(predicate::str::contains("Audit clean\nCatalog blobs: 1\nCAS blob files: 1\nBlobs hashed: 1\nGC candidates: 0\nFindings: 0"));
}

#[test]
fn missing_blob_is_finding_and_exit_two() {
    let temp = TempDir::new().unwrap();
    import(&temp);
    let blobs = temp.child("store/blobs");
    let first = std::fs::read_dir(blobs.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let second = std::fs::read_dir(first)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let blob = std::fs::read_dir(second)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::remove_file(blob).unwrap();
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["audit", "--store"])
        .arg(temp.child("store").path())
        .assert()
        .code(2)
        .stdout(predicate::str::contains("MISSING_BLOB"))
        .stdout(predicate::str::contains("Findings: 1"));
}

#[test]
fn operational_failure_is_exit_one_and_zero_chunk_is_rejected() {
    let temp = TempDir::new().unwrap();
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["audit", "--store"])
        .arg(temp.child("missing").path())
        .assert()
        .code(1);
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["audit", "--store"])
        .arg(temp.path())
        .args(["--chunk-size", "0"])
        .assert()
        .code(2);
}

#[test]
fn audit_without_wal_does_not_create_sidecars_or_change_durable_files() {
    let temp = TempDir::new().unwrap();
    import(&temp);
    let db = temp.child("store/catalog.sqlite");
    let blob = only_blob(temp.child("store/blobs").path());
    let before_db = snapshot(db.path());
    let before_blob = snapshot(&blob);
    let wal = format!("{}-wal", db.path().display());
    let shm = format!("{}-shm", db.path().display());
    assert!(!Path::new(&wal).exists());
    assert!(!Path::new(&shm).exists());

    audit(&temp).success();

    assert!(!Path::new(&wal).exists());
    assert!(!Path::new(&shm).exists());
    assert_eq!(before_db, snapshot(db.path()));
    assert_eq!(before_blob, snapshot(&blob));
}

#[test]
fn immutable_uri_percent_encodes_explicit_catalog_paths() {
    let temp = TempDir::new().unwrap();
    import(&temp);
    let original = temp.child("store/catalog.sqlite");
    let explicit = temp.child("catalog with % and ? characters.sqlite");
    fs::rename(original.path(), explicit.path()).unwrap();
    let mut command = Command::cargo_bin("media-importer").unwrap();
    command
        .args(["audit", "--store"])
        .arg(temp.child("store").path())
        .arg("--db")
        .arg(explicit.path())
        .assert()
        .success();
}

#[test]
fn audit_observes_committed_wal_and_rejects_wal_without_shm() {
    let temp = TempDir::new().unwrap();
    import(&temp);
    let db = temp.child("store/catalog.sqlite");
    let connection = Connection::open(db.path()).unwrap();
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    let content = b"wal-only";
    let hash = blake3::hash(content).to_hex().to_string();
    let path = temp.child(format!(
        "store/blobs/{}/{}/{}",
        &hash[..2],
        &hash[2..4],
        hash
    ));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    path.write_binary(content).unwrap();
    connection
        .execute(
            "INSERT INTO blobs(hash,size_bytes,created_at_ms) VALUES (?1,?2,1)",
            (&hash, content.len() as i64),
        )
        .unwrap();
    assert!(
        Path::new(&format!("{}-wal", db.path().display()))
            .metadata()
            .unwrap()
            .len()
            > 0
    );
    audit(&temp)
        .success()
        .stdout(predicate::str::contains("Catalog blobs: 2"));
    drop(connection);

    fs::write(format!("{}-wal", db.path().display()), b"not-a-real-wal").unwrap();
    let _ = fs::remove_file(format!("{}-shm", db.path().display()));
    audit(&temp)
        .code(1)
        .stderr(predicate::str::contains("without a usable SHM"));
}

#[test]
fn semantic_schema_impostors_and_extra_objects_are_findings() {
    for mutation in [
        "ALTER TABLE blobs RENAME COLUMN created_at_ms TO made_at_ms",
        "DROP TABLE source_files",
        "DROP INDEX idx_source_files_blob_hash",
        "CREATE TABLE unexpected(value TEXT)",
    ] {
        let temp = TempDir::new().unwrap();
        import(&temp);
        let connection = Connection::open(temp.child("store/catalog.sqlite").path()).unwrap();
        connection.execute_batch(mutation).unwrap();
        drop(connection);
        if mutation.contains("RENAME COLUMN") || mutation.contains("DROP TABLE") {
            audit(&temp).code(1);
        } else {
            audit(&temp)
                .code(2)
                .stdout(predicate::str::contains("CATALOG_INTEGRITY"));
        }
    }
}

#[test]
fn semantic_schema_validation_rejects_wrong_fk_and_missing_checks() {
    for schema in [
        include_str!("../src/catalog/sql/schema_v1.sql").replace(
            "REFERENCES blobs(hash)",
            "REFERENCES blobs(hash) ON DELETE CASCADE",
        ),
        include_str!("../src/catalog/sql/schema_v1.sql").replace(" CHECK(length(hash) = 64)", ""),
    ] {
        let temp = TempDir::new().unwrap();
        temp.child("store/blobs").create_dir_all().unwrap();
        let connection = Connection::open(temp.child("store/catalog.sqlite").path()).unwrap();
        connection.execute_batch(&schema).unwrap();
        drop(connection);
        audit(&temp)
            .code(2)
            .stdout(predicate::str::contains("reason=schema-mismatch"));
    }
}

#[test]
fn malformed_catalog_storage_and_domain_values_accumulate() {
    let temp = TempDir::new().unwrap();
    import(&temp);
    let connection = Connection::open(temp.child("store/catalog.sqlite").path()).unwrap();
    connection
        .execute_batch("PRAGMA ignore_check_constraints=ON;")
        .unwrap();
    connection
        .execute(
            "UPDATE blobs SET size_bytes='wrong', created_at_ms=x'00'",
            [],
        )
        .unwrap();
    connection.execute("UPDATE source_files SET relative_path='/absolute', seen_count=0, first_seen_at_ms=9, last_seen_at_ms=1", []).unwrap();
    drop(connection);
    audit(&temp)
        .code(2)
        .stdout(predicate::str::contains("INVALID_BLOB_ROW"))
        .stdout(predicate::str::contains("INVALID_SOURCE_ROW"));
}

#[test]
fn foreign_key_and_source_domain_failures_are_distinct_findings() {
    let temp = TempDir::new().unwrap();
    import(&temp);
    let connection = Connection::open(temp.child("store/catalog.sqlite").path()).unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys=OFF;")
        .unwrap();
    connection
        .execute(
            "UPDATE source_files SET blob_hash=?1, size_bytes=-1, seen_count=0",
            ["ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"],
        )
        .unwrap();
    drop(connection);
    audit(&temp)
        .code(2)
        .stdout(predicate::str::contains("CATALOG_INTEGRITY"))
        .stdout(predicate::str::contains("INVALID_SOURCE_ROW"));
}

#[test]
fn deleted_blobs_remain_required_and_count_as_gc_candidates() {
    let temp = TempDir::new().unwrap();
    import(&temp);
    let connection = Connection::open(temp.child("store/catalog.sqlite").path()).unwrap();
    connection
        .execute("UPDATE blobs SET deleted_at_ms=1", [])
        .unwrap();
    drop(connection);
    audit(&temp)
        .success()
        .stdout(predicate::str::contains("GC candidates: 1"));

    fs::remove_file(only_blob(temp.child("store/blobs").path())).unwrap();
    audit(&temp)
        .code(2)
        .stdout(predicate::str::contains("MISSING_BLOB"))
        .stdout(predicate::str::contains("GC candidates: 1"));
}

#[test]
fn size_mismatch_still_hashes_and_reports_independent_corruption() {
    let temp = TempDir::new().unwrap();
    import(&temp);
    let blob = only_blob(temp.child("store/blobs").path());
    fs::remove_file(&blob).unwrap();
    fs::write(&blob, b"different-length-and-content").unwrap();
    audit(&temp)
        .code(2)
        .stdout(predicate::str::contains("HASH_MISMATCH"))
        .stdout(predicate::str::contains("SIZE_MISMATCH"))
        .stdout(predicate::str::contains("Blobs hashed: 1"));
}

#[test]
fn orphan_corruption_and_invalid_cas_layout_are_reported_once_and_separately() {
    let temp = TempDir::new().unwrap();
    import(&temp);
    let orphan_hash = "0000000000000000000000000000000000000000000000000000000000000000";
    let orphan = temp.child(format!("store/blobs/00/00/{orphan_hash}"));
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    orphan.write_str("wrong-content").unwrap();
    temp.child("store/blobs/AA").create_dir_all().unwrap();
    temp.child("store/blobs/root-file")
        .write_str("bad")
        .unwrap();
    temp.child("store/blobs/01/23/too/deep")
        .create_dir_all()
        .unwrap();

    audit(&temp)
        .code(2)
        .stdout(predicate::str::contains("HASH_MISMATCH 0000"))
        .stdout(predicate::str::contains("ORPHAN_BLOB 0000"))
        .stdout(predicate::str::contains("INVALID_CAS_ENTRY AA"))
        .stdout(predicate::str::contains("CAS blob files: 2"))
        .stdout(predicate::str::contains("Blobs hashed: 2"));
}

#[cfg(unix)]
#[test]
fn symlinks_and_non_utf8_cas_names_are_not_followed_and_are_escaped() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    let temp = TempDir::new().unwrap();
    import(&temp);
    symlink("/", temp.child("store/blobs/link").path()).unwrap();
    let name = OsStr::from_bytes(b"bad\xff");
    if fs::write(temp.child("store/blobs").path().join(name), b"bad").is_err() {
        // Some macOS filesystems reject non-UTF-8 names at creation time.
        audit(&temp)
            .code(2)
            .stdout(predicate::str::contains("INVALID_CAS_ENTRY link"));
        return;
    }
    audit(&temp)
        .code(2)
        .stdout(predicate::str::contains("INVALID_CAS_ENTRY bad\\xff"))
        .stdout(predicate::str::contains("INVALID_CAS_ENTRY link"));
}

#[test]
fn empty_valid_shards_are_clean_and_findings_are_category_sorted() {
    let temp = TempDir::new().unwrap();
    import(&temp);
    temp.child("store/blobs/aa/bb").create_dir_all().unwrap();
    audit(&temp).success();

    let blob = only_blob(temp.child("store/blobs").path());
    fs::remove_file(&blob).unwrap();
    fs::write(&blob, b"xxxxxx").unwrap();
    temp.child("store/blobs/root").write_str("invalid").unwrap();
    let output = audit(&temp).code(2).get_output().stdout.clone();
    let output = String::from_utf8(output).unwrap();
    assert!(output.find("HASH_MISMATCH").unwrap() < output.find("INVALID_CAS_ENTRY").unwrap());
}

fn audit(temp: &TempDir) -> assert_cmd::assert::Assert {
    let mut command = Command::cargo_bin("media-importer").unwrap();
    command
        .args(["audit", "--store"])
        .arg(temp.child("store").path())
        .assert()
}

fn only_blob(root: &Path) -> std::path::PathBuf {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.is_file() {
                return path;
            }
        }
    }
    panic!("expected a blob file")
}

fn snapshot(path: &Path) -> (Vec<u8>, u64, std::time::SystemTime) {
    let metadata = fs::metadata(path).unwrap();
    (
        fs::read(path).unwrap(),
        metadata.permissions().readonly() as u64,
        metadata.modified().unwrap(),
    )
}
