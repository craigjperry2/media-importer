use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use predicates::prelude::*;
use rusqlite::Connection;

#[test]
fn gc_help_documents_lifecycle_hashing_dry_run_and_automatic_waiting() {
    let output = Command::cargo_bin("media-importer")
        .unwrap()
        .args(["gc", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("catalog source records"))
        .stdout(predicate::str::contains("marked before this run"))
        .stdout(predicate::str::contains("fully hashed"))
        .stdout(predicate::str::contains("Dry-run"))
        .stdout(predicate::str::contains("waits indefinitely"))
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();
    for non_goal in [
        "--mark-only",
        "--sweep-only",
        "--force",
        "--yes",
        "--grace-period",
        "--json",
        "--workers",
    ] {
        assert!(!output.contains(non_goal), "unexpected GC flag {non_goal}");
    }
}

#[test]
fn zero_gc_chunk_size_is_an_operational_validation_error_without_mutation() {
    let temp = imported("zero");
    let before = durable_state(&temp);

    gc(&temp, &["--chunk-size", "0"])
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--chunk-size must be non-zero"));

    assert_eq!(durable_state(&temp), before);
}

#[test]
fn clean_referenced_store_is_an_exact_noop_even_when_source_file_is_deleted() {
    let temp = imported("reachable");
    fs::remove_file(temp.child("source/file.bin").path()).unwrap();
    let before = durable_state(&temp);

    gc(&temp, &[])
        .success()
        .stdout(predicate::str::contains(
            "GC complete\nBlobs marked: 0\nBlobs resurrected: 0\nBlobs swept: 0\nCAS files removed: 0\nBytes reclaimed: 0\nCatalog blobs: 1\nReachable blobs: 1\nSweep candidates hashed: 0\nFindings: 0",
        ));

    assert_eq!(durable_state(&temp), before);
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
    let (_, blob) = only_catalog_blob(&temp);
    let before_db_metadata = metadata_state(db.path());
    let before_blob_metadata = metadata_state(&blob);
    let before_blobs_metadata = metadata_state(temp.child("store/blobs").path());
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
    assert_eq!(before_db_metadata, metadata_state(db.path()));
    assert_eq!(before_blob_metadata, metadata_state(&blob));
    assert_eq!(
        before_blobs_metadata,
        metadata_state(temp.child("store/blobs").path())
    );
    assert!(!wal.exists());
    assert!(!shm.exists());
    let (hash, blob) = only_catalog_blob(&temp);
    assert!(blob.exists());
    assert_eq!(deleted_at(&temp, &hash), None);
}

#[test]
fn replacing_a_source_path_collects_only_the_old_blob_after_two_gc_runs() {
    let temp = imported("old-content");
    let (old_hash, old_blob) = only_catalog_blob(&temp);
    writable(temp.child("source/file.bin").path());
    temp.child("source/file.bin")
        .write_str("new-content")
        .unwrap();
    import_existing_source(&temp);
    let new_hash = blake3::hash(b"new-content").to_hex().to_string();
    let new_blob = blob_path(&temp, &new_hash);

    gc(&temp, &[])
        .success()
        .stdout(predicate::str::contains(format!("MARK {old_hash}")));
    assert!(old_blob.exists());
    assert!(new_blob.exists());
    gc(&temp, &[])
        .success()
        .stdout(predicate::str::contains(format!("SWEEP {old_hash}")));

    assert!(!old_blob.exists());
    assert!(new_blob.exists());
    assert_eq!(blob_count(&temp), 1);
    assert_eq!(deleted_at(&temp, &new_hash), None);
}

#[test]
fn all_source_references_must_move_before_a_shared_blob_becomes_unreachable() {
    let temp = TempDir::new().unwrap();
    temp.child("source").create_dir_all().unwrap();
    temp.child("source/a.bin").write_str("shared").unwrap();
    temp.child("source/b.bin").write_str("shared").unwrap();
    import_existing_source(&temp);
    let shared_hash = blake3::hash(b"shared").to_hex().to_string();

    temp.child("source/a.bin").write_str("first-new").unwrap();
    import_existing_source(&temp);
    gc(&temp, &[])
        .success()
        .stdout(predicate::str::contains("Blobs marked: 0"));
    assert_eq!(
        deleted_at(&temp, &shared_hash),
        None,
        "the mark belongs to the source content replaced on the second import"
    );

    temp.child("source/b.bin").write_str("second-new").unwrap();
    import_existing_source(&temp);
    gc(&temp, &[])
        .success()
        .stdout(predicate::str::contains(format!("MARK {shared_hash}")));
    assert!(blob_path(&temp, &shared_hash).exists());
    gc(&temp, &[])
        .success()
        .stdout(predicate::str::contains(format!("SWEEP {shared_hash}")));
    assert!(!blob_path(&temp, &shared_hash).exists());
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
fn representative_cas_findings_block_marks_and_preserve_the_invalid_state() {
    let malformed = imported("malformed");
    make_unreachable(&malformed);
    let malformed_entry = malformed.child("store/blobs/not-a-shard");
    malformed_entry.write_str("invalid").unwrap();
    let before = catalog_state(&malformed);
    gc(&malformed, &[])
        .code(2)
        .stdout(predicate::str::contains("INVALID_CAS_ENTRY"))
        .stdout(predicate::str::contains("GC blocked"));
    assert_eq!(catalog_state(&malformed), before);
    malformed_entry.assert("invalid");

    let missing_live = imported("missing-live");
    let (hash, blob) = only_catalog_blob(&missing_live);
    fs::remove_file(&blob).unwrap();
    let before = catalog_state(&missing_live);
    gc(&missing_live, &[])
        .code(2)
        .stdout(predicate::str::contains(format!("MISSING_BLOB {hash}")));
    assert_eq!(catalog_state(&missing_live), before);

    let missing_markable = imported("missing-markable");
    make_unreachable(&missing_markable);
    let (hash, blob) = only_catalog_blob(&missing_markable);
    fs::remove_file(&blob).unwrap();
    let before = catalog_state(&missing_markable);
    gc(&missing_markable, &[])
        .code(2)
        .stdout(predicate::str::contains(format!("MISSING_BLOB {hash}")));
    assert_eq!(catalog_state(&missing_markable), before);
    assert_eq!(deleted_at(&missing_markable, &hash), None);

    let size_mismatch = imported("short");
    make_unreachable(&size_mismatch);
    let (_, blob) = only_catalog_blob(&size_mismatch);
    fs::remove_file(&blob).unwrap();
    fs::write(&blob, b"substantially-longer").unwrap();
    let before = catalog_state(&size_mismatch);
    gc(&size_mismatch, &[])
        .code(2)
        .stdout(predicate::str::contains("SIZE_MISMATCH"));
    assert_eq!(catalog_state(&size_mismatch), before);
    assert_eq!(fs::read(&blob).unwrap(), b"substantially-longer");
}

#[cfg(unix)]
#[test]
fn symlinked_and_nonregular_expected_cas_entries_are_never_interrupted_sweeps() {
    use std::os::unix::fs::symlink;

    let symlink_case = imported("symlink-candidate");
    make_unreachable(&symlink_case);
    let (hash, blob) = only_catalog_blob(&symlink_case);
    set_mark(&symlink_case, &hash, 1);
    fs::remove_file(&blob).unwrap();
    symlink("/does/not/exist", &blob).unwrap();
    let before = catalog_state(&symlink_case);
    gc(&symlink_case, &[])
        .code(2)
        .stdout(predicate::str::contains("INVALID_CAS_ENTRY"));
    assert_eq!(catalog_state(&symlink_case), before);
    assert!(
        fs::symlink_metadata(&blob)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let directory_case = imported("directory-candidate");
    make_unreachable(&directory_case);
    let (hash, blob) = only_catalog_blob(&directory_case);
    set_mark(&directory_case, &hash, 1);
    fs::remove_file(&blob).unwrap();
    fs::create_dir(&blob).unwrap();
    let before = catalog_state(&directory_case);
    gc(&directory_case, &[])
        .code(2)
        .stdout(predicate::str::contains(format!("NON_REGULAR_BLOB {hash}")));
    assert_eq!(catalog_state(&directory_case), before);
    assert!(blob.is_dir());
}

#[test]
fn catalog_foreign_key_and_domain_findings_block_all_gc_mutation() {
    let foreign_key = imported("foreign-key");
    let (hash, blob) = only_catalog_blob(&foreign_key);
    let foreign_connection = connection(&foreign_key);
    foreign_connection
        .execute_batch("PRAGMA foreign_keys=OFF")
        .unwrap();
    foreign_connection
        .execute(
            "UPDATE source_files SET blob_hash=?1",
            ["ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"],
        )
        .unwrap();
    drop(foreign_connection);
    let before_blob = fs::read(&blob).unwrap();
    let before_deleted = deleted_at(&foreign_key, &hash);
    gc(&foreign_key, &[])
        .code(2)
        .stdout(predicate::str::contains("CATALOG_INTEGRITY"))
        .stdout(predicate::str::contains("INVALID_SOURCE_ROW"));
    assert_eq!(fs::read(&blob).unwrap(), before_blob);
    assert_eq!(deleted_at(&foreign_key, &hash), before_deleted);

    let invalid_blob = imported("invalid-blob");
    let (hash, blob) = only_catalog_blob(&invalid_blob);
    let invalid_connection = connection(&invalid_blob);
    invalid_connection
        .execute_batch("PRAGMA ignore_check_constraints=ON")
        .unwrap();
    invalid_connection
        .execute("UPDATE blobs SET size_bytes='wrong'", [])
        .unwrap();
    drop(invalid_connection);
    let raw_before: String = connection(&invalid_blob)
        .query_row("SELECT quote(size_bytes) FROM blobs", [], |row| row.get(0))
        .unwrap();
    let before_blob = fs::read(&blob).unwrap();
    gc(&invalid_blob, &[])
        .code(2)
        .stdout(predicate::str::contains("INVALID_BLOB_ROW"))
        .stdout(predicate::str::contains("GC blocked"));
    let raw_after: String = connection(&invalid_blob)
        .query_row("SELECT quote(size_bytes) FROM blobs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(raw_after, raw_before);
    assert_eq!(fs::read(&blob).unwrap(), before_blob);
    assert!(blob_path(&invalid_blob, &hash).exists());
}

#[test]
fn mixed_dry_run_and_real_run_have_exact_ordered_actions_and_counters() {
    let temp = TempDir::new().unwrap();
    temp.child("source").create_dir_all().unwrap();
    let fixtures = [
        ("unchanged", "u"),
        ("mark-b", "mb"),
        ("mark-a", "ma"),
        ("resurrect-b", "rb"),
        ("resurrect-a", "ra"),
        ("sweep-b", "sb"),
        ("sweep-a", "sa"),
    ];
    for (path, content) in fixtures {
        temp.child(format!("source/{path}"))
            .write_str(content)
            .unwrap();
    }
    import_existing_source(&temp);
    let hash = |content: &str| blake3::hash(content.as_bytes()).to_hex().to_string();
    let mut marks = [hash("mb"), hash("ma")];
    let mut resurrections = [hash("rb"), hash("ra")];
    let mut sweeps = [hash("sb"), hash("sa")];
    marks.sort();
    resurrections.sort();
    sweeps.sort();
    let connection = connection(&temp);
    for value in marks.iter().chain(sweeps.iter()) {
        connection
            .execute("DELETE FROM source_files WHERE blob_hash=?1", [value])
            .unwrap();
    }
    for value in resurrections.iter().chain(sweeps.iter()) {
        connection
            .execute("UPDATE blobs SET deleted_at_ms=7 WHERE hash=?1", [value])
            .unwrap();
    }
    drop(connection);

    let output = gc(&temp, &["--dry-run"])
        .success()
        .stdout(predicate::str::contains(
            "Blobs that would be marked: 2\nBlobs that would be resurrected: 2\nBlobs that would be swept: 2\nBytes that would be reclaimed: 4\nCatalog blobs: 7\nReachable blobs: 3\nSweep candidates hashed: 2\nFindings: 0",
        ))
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();
    assert_action_order(
        &output,
        &[
            format!("WOULD_MARK {}", marks[0]),
            format!("WOULD_MARK {}", marks[1]),
            format!("WOULD_RESURRECT {}", resurrections[0]),
            format!("WOULD_RESURRECT {}", resurrections[1]),
            format!("WOULD_SWEEP {}", sweeps[0]),
            format!("WOULD_SWEEP {}", sweeps[1]),
        ],
    );

    let output = gc(&temp, &[])
        .success()
        .stdout(predicate::str::contains(
            "GC complete\nBlobs marked: 2\nBlobs resurrected: 2\nBlobs swept: 2\nCAS files removed: 2\nBytes reclaimed: 4\nCatalog blobs: 7\nReachable blobs: 3\nSweep candidates hashed: 2\nFindings: 0",
        ))
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();
    assert_action_order(
        &output,
        &[
            format!("MARK {}", marks[0]),
            format!("MARK {}", marks[1]),
            format!("RESURRECT {}", resurrections[0]),
            format!("RESURRECT {}", resurrections[1]),
            format!("SWEEP {}", sweeps[0]),
            format!("SWEEP {}", sweeps[1]),
        ],
    );
}

#[test]
fn present_and_already_absent_sweeps_use_distinct_physical_and_byte_counters() {
    let temp = TempDir::new().unwrap();
    temp.child("source").create_dir_all().unwrap();
    temp.child("source/present").write_str("present").unwrap();
    temp.child("source/absent").write_str("absent").unwrap();
    import_existing_source(&temp);
    make_unreachable(&temp);
    connection(&temp)
        .execute("UPDATE blobs SET deleted_at_ms=1", [])
        .unwrap();
    let absent_hash = blake3::hash(b"absent").to_hex().to_string();
    fs::remove_file(blob_path(&temp, &absent_hash)).unwrap();

    gc(&temp, &[])
        .success()
        .stdout(predicate::str::contains(
            "Blobs swept: 2\nCAS files removed: 1\nBytes reclaimed: 7",
        ))
        .stdout(predicate::str::contains(format!(
            "SWEEP {absent_hash} bytes=6 state=already-absent"
        )));
    assert_eq!(blob_count(&temp), 0);
}

#[test]
fn missing_store_blobs_and_catalog_are_validation_errors_and_are_never_created() {
    let temp = TempDir::new().unwrap();
    let missing_store = temp.child("missing-store");
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["gc", "--store"])
        .arg(missing_store.path())
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty());
    assert!(!missing_store.exists());

    let store_without_blobs = temp.child("store-without-blobs");
    store_without_blobs.create_dir_all().unwrap();
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["gc", "--store"])
        .arg(store_without_blobs.path())
        .assert()
        .code(1);
    assert!(!store_without_blobs.child("blobs").exists());
    assert!(!store_without_blobs.child("catalog.sqlite").exists());

    let store_without_catalog = temp.child("store-without-catalog");
    store_without_catalog
        .child("blobs")
        .create_dir_all()
        .unwrap();
    store_without_catalog
        .child("blobs/sentinel")
        .write_str("unchanged")
        .unwrap();
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["gc", "--store"])
        .arg(store_without_catalog.path())
        .assert()
        .code(1);
    assert!(!store_without_catalog.child("catalog.sqlite").exists());
    store_without_catalog
        .child("blobs/sentinel")
        .assert("unchanged");
}

#[cfg(unix)]
#[test]
fn symlinked_store_blobs_and_catalog_are_rejected_without_application_mutation() {
    use std::os::unix::fs::symlink;

    let store_case = imported("store-link");
    let before = durable_state(&store_case);
    symlink(
        store_case.child("store").path(),
        store_case.child("store-link").path(),
    )
    .unwrap();
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["gc", "--store"])
        .arg(store_case.child("store-link").path())
        .assert()
        .code(1);
    assert_eq!(durable_state(&store_case), before);

    let blobs_case = imported("blobs-link");
    let blobs = blobs_case.child("store/blobs");
    let real_blobs = blobs_case.child("store/real-blobs");
    fs::rename(blobs.path(), real_blobs.path()).unwrap();
    symlink(real_blobs.path(), blobs.path()).unwrap();
    let before_catalog = catalog_state(&blobs_case);
    let before_blob = fs::read(only_blob(real_blobs.path())).unwrap();
    gc(&blobs_case, &[]).code(1);
    assert_eq!(catalog_state(&blobs_case), before_catalog);
    assert_eq!(fs::read(only_blob(real_blobs.path())).unwrap(), before_blob);

    let catalog_case = imported("catalog-link");
    let catalog = catalog_case.child("store/catalog.sqlite");
    let real_catalog = catalog_case.child("store/real-catalog.sqlite");
    fs::rename(catalog.path(), real_catalog.path()).unwrap();
    symlink(real_catalog.path(), catalog.path()).unwrap();
    let blob = only_blob(catalog_case.child("store/blobs").path());
    let before_blob = fs::read(&blob).unwrap();
    let before_catalog = fs::read(real_catalog.path()).unwrap();
    gc(&catalog_case, &[]).code(1);
    assert_eq!(fs::read(&blob).unwrap(), before_blob);
    assert_eq!(fs::read(real_catalog.path()).unwrap(), before_catalog);
}

#[test]
fn real_gc_rejects_uninitialized_older_and_newer_catalogs_without_migration() {
    for (name, initialize_schema, version) in [
        ("uninitialized", false, 0_i64),
        ("older", true, 0_i64),
        ("newer", true, 2_i64),
    ] {
        let temp = TempDir::new().unwrap();
        temp.child("store/blobs").create_dir_all().unwrap();
        let db = temp.child("store/catalog.sqlite");
        let connection = Connection::open(db.path()).unwrap();
        if initialize_schema {
            connection
                .execute_batch(include_str!("../src/catalog/sql/schema_v1.sql"))
                .unwrap();
        }
        connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        let mode: String = connection
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        drop(connection);
        let before = fs::read(db.path()).unwrap();

        gc(&temp, &[])
            .code(1)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("catalog schema version"));

        assert_eq!(fs::read(db.path()).unwrap(), before, "{name}");
        let version_after: i64 = Connection::open(db.path())
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version_after, version, "{name}");
    }
}

#[test]
fn enumerable_semantic_schema_finding_blocks_all_gc_mutation() {
    let temp = imported("schema");
    let connection = connection(&temp);
    connection
        .execute_batch("DROP INDEX idx_source_files_blob_hash")
        .unwrap();
    drop(connection);
    let before = durable_state(&temp);

    gc(&temp, &[])
        .code(2)
        .stdout(predicate::str::contains("CATALOG_INTEGRITY"))
        .stdout(predicate::str::contains("GC blocked"));

    assert_eq!(durable_state(&temp), before);
}

#[test]
fn dry_run_observes_committed_wal_without_changing_source_sidecars() {
    let wal_case = imported("main");
    let db = wal_case.child("store/catalog.sqlite");
    let connection = Connection::open(db.path()).unwrap();
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    let content = b"committed-in-wal";
    let hash = blake3::hash(content).to_hex().to_string();
    let path = blob_path(&wal_case, &hash);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, content).unwrap();
    connection
        .execute(
            "INSERT INTO blobs(hash,size_bytes,created_at_ms) VALUES (?1,?2,1)",
            (&hash, content.len() as i64),
        )
        .unwrap();
    gc(&wal_case, &["--dry-run"])
        .success()
        .stdout(predicate::str::contains(format!("WOULD_MARK {hash}")))
        .stdout(predicate::str::contains("Catalog blobs: 2"));
    drop(connection);
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
    import_existing_source(&temp);
    temp
}

fn import_existing_source(temp: &TempDir) {
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["import", "--store"])
        .arg(temp.child("store").path())
        .arg("--source")
        .arg(temp.child("source").path())
        .assert()
        .success();
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

#[derive(Debug, Eq, PartialEq)]
struct DurableState {
    catalog: Vec<(String, i64, Option<i64>, i64)>,
    cas: Vec<(PathBuf, Vec<u8>)>,
}

fn durable_state(temp: &TempDir) -> DurableState {
    let blobs = temp.child("store/blobs");
    let mut cas = Vec::new();
    for entry in walkdir::WalkDir::new(blobs.path())
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file() {
            cas.push((
                entry
                    .path()
                    .strip_prefix(blobs.path())
                    .unwrap()
                    .to_path_buf(),
                fs::read(entry.path()).unwrap(),
            ));
        }
    }
    cas.sort_by(|left, right| left.0.cmp(&right.0));
    DurableState {
        catalog: catalog_state(temp),
        cas,
    }
}

fn catalog_state(temp: &TempDir) -> Vec<(String, i64, Option<i64>, i64)> {
    let connection = connection(temp);
    let mut statement = connection
        .prepare(
            "SELECT hash, size_bytes, deleted_at_ms, \
             (SELECT count(*) FROM source_files WHERE blob_hash=blobs.hash) \
             FROM blobs ORDER BY hash",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[derive(Debug, Eq, PartialEq)]
struct MetadataState {
    len: u64,
    readonly: bool,
    modified: SystemTime,
}

fn metadata_state(path: &Path) -> MetadataState {
    let metadata = fs::metadata(path).unwrap();
    MetadataState {
        len: metadata.len(),
        readonly: metadata.permissions().readonly(),
        modified: metadata.modified().unwrap(),
    }
}

fn only_blob(blobs: &Path) -> PathBuf {
    walkdir::WalkDir::new(blobs)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| entry.file_type().is_file())
        .expect("one CAS blob")
        .into_path()
}

fn assert_action_order(output: &str, actions: &[String]) {
    let mut previous = None;
    for action in actions {
        let position = output.find(action).expect("expected action in output");
        if let Some(previous) = previous {
            assert!(previous < position, "actions out of order: {actions:?}");
        }
        previous = Some(position);
    }
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
