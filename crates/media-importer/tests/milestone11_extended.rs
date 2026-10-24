//! Release-qualification, subprocess-based M11 resilience campaign.
//!
//! Run with `cargo test --workspace --test milestone11_extended -- --ignored`.
//! It intentionally reports application stream facts; it makes no physical
//! device-read or machine-specific throughput claim.

use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use rusqlite::{Connection, OpenFlags};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn import_jsonl(source: &Path, store: &Path, extra: &[&str]) -> Vec<serde_json::Value> {
    let output = Command::cargo_bin("media-importer")
        .expect("binary")
        .args(["--output", "jsonl", "import", "--store"])
        .arg(store)
        .arg("--source")
        .arg(source)
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output)
        .expect("JSON Lines UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("complete JSON line"))
        .collect()
}

fn source_bytes(records: &[serde_json::Value]) -> u64 {
    records
        .iter()
        .filter(|r| r["event"] == "source_bytes_delta")
        .map(|r| r["bytes"].as_u64().expect("source byte count"))
        .sum()
}

fn summary_bytes_hashed(records: &[serde_json::Value]) -> u64 {
    records
        .iter()
        .find(|record| record["event"] == "command_summary")
        .and_then(|record| record["bytes_hashed"].as_u64())
        .expect("import command summary with bytes_hashed")
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn spawn_import_probe(
    source: &Path,
    store: &Path,
    probe: &Path,
    stage: &str,
) -> std::process::Child {
    ProcessCommand::new(assert_cmd::cargo::cargo_bin("media-importer"))
        .args(["import", "--store"])
        .arg(store)
        .arg("--source")
        .arg(source)
        .args(["--no-metadata-skip", "--workers-per-mount", "2"])
        .env(
            "MEDIA_IMPORTER_TEST_LIFECYCLE_PROBE",
            format!("{}|{stage}", probe.display()),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fault child")
}

fn audit_clean(store: &Path) {
    Command::cargo_bin("media-importer")
        .expect("binary")
        .args(["audit", "--store"])
        .arg(store)
        .assert()
        .success();
}

/// Each import seam is killed (not merely returned from) in an isolated
/// process.  Its immediately durable state is then observed before a
/// read-only command and dry run prove they did not repair it; the next real
/// import performs permitted recovery and the final audit proves lock release.
#[test]
#[ignore = "release qualification: extended I/O and crash-recovery campaign"]
fn m11_extended_io_and_exact_import_fault_campaign() {
    let temp = TempDir::new().expect("temporary directory");
    let source = temp.child("source");
    source.create_dir_all().unwrap();
    let bytes = vec![0x5a; 256 * 1024];
    fs::write(source.child("repeat.bin").path(), &bytes).unwrap();
    let store = temp.child("store");

    let first = import_jsonl(source.path(), store.path(), &[]);
    assert_eq!(source_bytes(&first), bytes.len() as u64);
    assert_eq!(source_bytes(&first), summary_bytes_hashed(&first));
    let unchanged = import_jsonl(source.path(), store.path(), &[]);
    assert_eq!(
        source_bytes(&unchanged),
        0,
        "metadata skip reads no content"
    );
    assert_eq!(source_bytes(&unchanged), summary_bytes_hashed(&unchanged));
    let forced = import_jsonl(source.path(), store.path(), &["--no-metadata-skip"]);
    assert_eq!(source_bytes(&forced), bytes.len() as u64);
    assert_eq!(source_bytes(&forced), summary_bytes_hashed(&forced));

    for stage in [
        "store-after-staging-created",
        "store-during-source-copy",
        "ingest-after-cas-install",
        "ingest-after-catalog-submission",
        "catalog-writer-before-batch-commit",
        "catalog-writer-after-batch-commit",
        "catalog-writer-before-checkpoint",
        "catalog-writer-during-passive-checkpoint",
        "source-worker-before-read",
    ] {
        let case = TempDir::new().unwrap();
        let case_source = case.child("source");
        case_source.create_dir_all().unwrap();
        case_source.child("file.bin").write_binary(&bytes).unwrap();
        let case_store = case.child("store");
        let probe = case.child("probe");
        probe.create_dir_all().unwrap();
        if stage == "source-worker-before-read" {
            case_source
                .child("second.bin")
                .write_binary(&bytes)
                .unwrap();
            probe
                .child(format!("{stage}.participants"))
                .write_str("2")
                .unwrap();
        }
        let stage_spec = if stage == "ingest-after-catalog-submission" {
            // Hold the writer before it commits so this seam has the exact
            // intended durable state: installed CAS, but no catalog rows.
            "ingest-after-catalog-submission,catalog-writer-before-batch-commit"
        } else if stage == "catalog-writer-before-checkpoint" {
            // Enable the SQLite VFS checkpoint-start seam as well. The Rust
            // pre-checkpoint seam must win first; if the post-entry marker
            // were emitted here, this assertion would fail.
            "catalog-writer-before-checkpoint,catalog-writer-during-passive-checkpoint"
        } else {
            stage
        };
        let mut child = spawn_import_probe(
            case_source.path(),
            case_store.path(),
            probe.path(),
            stage_spec,
        );
        wait_for(probe.child(format!("{stage}.ready")).path());
        if stage == "catalog-writer-before-checkpoint" {
            assert!(
                !probe
                    .child("catalog-writer-during-passive-checkpoint.ready")
                    .path()
                    .exists(),
                "the SQLite checkpoint-entry seam must not fire at the Rust pre-checkpoint seam"
            );
        }
        child.kill().expect("kill at exact lifecycle boundary");
        child.wait().expect("reap killed child");

        // Read-only commands and dry runs have no repair authority. Snapshot
        // every durable component, including the recursive staging tree.
        let before = durable_state(case_store.path());
        assert_import_boundary_state(stage, &before, &bytes);
        let audit = Command::cargo_bin("media-importer")
            .unwrap()
            .args(["audit", "--store"])
            .arg(case_store.path())
            .output()
            .expect("run read-only audit");
        assert!(
            matches!(audit.status.code(), Some(0 | 2)),
            "{stage}: audit must report state, not repair it: {audit:?}"
        );
        Command::cargo_bin("media-importer")
            .unwrap()
            .args(["import", "--store"])
            .arg(case_store.path())
            .arg("--source")
            .arg(case_source.path())
            .arg("--dry-run")
            .assert()
            .success();
        assert_eq!(
            durable_state(case_store.path()),
            before,
            "{stage}: read-only state changed"
        );

        import_jsonl(
            case_source.path(),
            case_store.path(),
            &["--no-metadata-skip"],
        );
        assert!(
            snapshot_tree(case_store.child("staging").path()).is_empty(),
            "{stage}: real recovery leaves staging empty"
        );
        if stage == "catalog-writer-during-passive-checkpoint" {
            let recovered = durable_state(case_store.path());
            assert!(
                recovered.catalog_blobs.len() == 1 && recovered.source_files.len() == 1,
                "{stage}: recovery retains the batch committed before SQLite checkpoint entry"
            );
            assert!(
                !recovered.wal_exists || recovered.wal_bytes.len() <= before.wal_bytes.len(),
                "{stage}: recovery's later managed checkpoint does not expand the interrupted WAL"
            );
        }
        audit_clean(case_store.path());
    }
}

/// The non-import seams have distinct durable-state rules. Each test kills a
/// process at the actual mutation point, proves audit/dry-run are observers,
/// then runs the permitted recovery and checks lock release via a final audit.
#[test]
#[ignore = "release qualification: extended I/O and crash-recovery campaign"]
fn m11_extended_gc_build_tree_and_reporting_fault_boundaries() {
    let bytes = vec![0x31; 32 * 1024];

    // GC: after unlink but before the sweep transaction can commit, the CAS
    // file is absent while the catalog still retains its row. A later GC
    // safely completes that interrupted sweep.
    let gc_case = TempDir::new().unwrap();
    let gc_source = gc_case.child("source");
    gc_source.create_dir_all().unwrap();
    gc_source.child("orphan.bin").write_binary(&bytes).unwrap();
    let gc_store = gc_case.child("store");
    import_jsonl(gc_source.path(), gc_store.path(), &[]);
    let database = Connection::open(gc_store.child("catalog.sqlite").path()).unwrap();
    database.execute("DELETE FROM source_files", []).unwrap();
    drop(database);
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["gc", "--store"])
        .arg(gc_store.path())
        .assert()
        .success();
    let probe = gc_case.child("gc-probe");
    probe.create_dir_all().unwrap();
    let mut child = ProcessCommand::new(assert_cmd::cargo::cargo_bin("media-importer"))
        .args(["gc", "--store"])
        .arg(gc_store.path())
        .env(
            "MEDIA_IMPORTER_TEST_LIFECYCLE_PROBE",
            format!(
                "{}|gc-after-unlink-before-sweep-commit",
                probe.path().display()
            ),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(
        probe
            .child("gc-after-unlink-before-sweep-commit.ready")
            .path(),
    );
    child.kill().unwrap();
    child.wait().unwrap();
    let gc_before = durable_state(gc_store.path());
    assert_eq!(
        gc_before.catalog_blobs.len(),
        1,
        "uncommitted sweep retains catalog row"
    );
    assert_eq!(
        gc_before.cas.len(),
        0,
        "post-unlink seam removes the CAS file"
    );
    assert_read_only_preserves(gc_store.path(), None, &gc_before, "gc interrupted sweep");
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["gc", "--store"])
        .arg(gc_store.path())
        .assert()
        .success();
    audit_clean(gc_store.path());

    // Build-tree: replacement is atomic. Killing before rename preserves the
    // old link and any temporary entry; a rerun removes the temp by replacing
    // the old owned link with the catalog's desired target.
    let tree_case = TempDir::new().unwrap();
    let tree_source = tree_case.child("source");
    tree_source.create_dir_all().unwrap();
    tree_source.child("same.bin").write_binary(b"old").unwrap();
    let tree_store = tree_case.child("store");
    let browse = tree_case.child("browse");
    import_jsonl(tree_source.path(), tree_store.path(), &[]);
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["build-tree", "--store"])
        .arg(tree_store.path())
        .arg("--browse-tree")
        .arg(browse.path())
        .assert()
        .success();
    let existing = walkdir::WalkDir::new(browse.path())
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| entry.file_type().is_symlink())
        .expect("materialized symlink")
        .into_path();
    tree_source
        .child("other.bin")
        .write_binary(b"other")
        .unwrap();
    import_jsonl(tree_source.path(), tree_store.path(), &[]);
    let original_target = fs::read_link(&existing).unwrap();
    let original_blob =
        fs::canonicalize(existing.parent().unwrap().join(&original_target)).unwrap();
    let replacement_blob = walkdir::WalkDir::new(tree_store.child("blobs").path())
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .find(|path| path.is_file() && fs::canonicalize(path).unwrap() != original_blob)
        .expect("second CAS blob");
    fs::remove_file(&existing).unwrap();
    std::os::unix::fs::symlink(
        media_importer::paths::relative_symlink_target(
            existing.parent().unwrap(),
            &replacement_blob,
        )
        .unwrap(),
        &existing,
    )
    .unwrap();
    let tree_probe = tree_case.child("tree-probe");
    tree_probe.create_dir_all().unwrap();
    let mut tree = ProcessCommand::new(assert_cmd::cargo::cargo_bin("media-importer"))
        .args(["build-tree", "--store"])
        .arg(tree_store.path())
        .arg("--browse-tree")
        .arg(browse.path())
        .env(
            "MEDIA_IMPORTER_TEST_LIFECYCLE_PROBE",
            format!(
                "{}|build-tree-before-replacement",
                tree_probe.path().display()
            ),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(
        tree_probe
            .child("build-tree-before-replacement.ready")
            .path(),
    );
    tree.kill().unwrap();
    tree.wait().unwrap();
    let tree_before = durable_state(tree_store.path());
    let browse_before = snapshot_tree(browse.path());
    assert!(
        browse_before.iter().any(|entry| entry.kind == "symlink"),
        "replacement seam preserves the existing browse link"
    );
    assert!(
        browse_before
            .iter()
            .filter(|entry| entry.kind == "symlink")
            .count()
            >= 2,
        "replacement seam leaves the unrenamed temporary browse link observable"
    );
    assert_read_only_preserves(
        tree_store.path(),
        Some(browse.path()),
        &tree_before,
        "build-tree replacement",
    );
    Command::cargo_bin("media-importer")
        .unwrap()
        .args(["build-tree", "--store"])
        .arg(tree_store.path())
        .arg("--browse-tree")
        .arg(browse.path())
        .assert()
        .success();
    audit_clean(tree_store.path());

    // A closed JSONL reader is a real broken-pipe reporting shutdown. The
    // command must terminate and release the store lock; its next real run is
    // allowed to reconcile any work committed before cancellation.
    let pipe_case = TempDir::new().unwrap();
    let pipe_source = pipe_case.child("source");
    pipe_source.create_dir_all().unwrap();
    for i in 0..96 {
        pipe_source
            .child(format!("{i}.bin"))
            .write_binary(&bytes)
            .unwrap();
    }
    let pipe_store = pipe_case.child("store");
    let mut pipe = ProcessCommand::new(assert_cmd::cargo::cargo_bin("media-importer"))
        .args(["--output", "jsonl", "import", "--store"])
        .arg(pipe_store.path())
        .arg("--source")
        .arg(pipe_source.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(pipe.stdout.take());
    assert!(
        pipe.wait().unwrap().code().is_some(),
        "broken-pipe child terminates"
    );
    let pipe_before = durable_state(pipe_store.path());
    assert_read_only_preserves(
        pipe_store.path(),
        None,
        &pipe_before,
        "broken-pipe reporting shutdown",
    );
    import_jsonl(pipe_source.path(), pipe_store.path(), &[]);
    audit_clean(pipe_store.path());
}

/// Production-style generated records exercise bounded queues, repeated
/// writer batches/checkpoint attempts, and sustained parseable JSONL plus the
/// human terminal renderer. Scheduler-specific active-reader and queue bounds
/// are asserted by the deterministic normal-suite probe tests named in M11's
/// conformance matrix.
#[test]
#[ignore = "release qualification: extended I/O and crash-recovery campaign"]
fn m11_extended_many_records_checkpoints_and_sustained_rendering() {
    let temp = TempDir::new().unwrap();
    let source = temp.child("source");
    source.create_dir_all().unwrap();
    for i in 0..384 {
        source
            .child(format!("record-{i:03}.bin"))
            .write_binary(&[i as u8; 4096])
            .unwrap();
    }
    let store = temp.child("store");
    let records = import_jsonl(source.path(), store.path(), &[]);
    assert!(
        records
            .iter()
            .filter(|r| r["event"] == "catalog_batch_committed")
            .count()
            > 1,
        "many records cross writer batches"
    );
    assert!(
        records
            .iter()
            .filter(|r| r["event"] == "catalog_checkpoint_requested")
            .count()
            >= 1,
        "multiple default writer batches trigger a periodic checkpoint attempt"
    );
    assert!(
        records
            .iter()
            .filter(|r| r["event"] == "catalog_checkpoint_completed")
            .count()
            >= 1,
        "periodic passive checkpoint completes"
    );
    let final_checkpoints = records
        .iter()
        .find(|r| r["event"] == "catalog_final_checkpoint")
        .and_then(|r| r["checkpoints"].as_u64())
        .expect("final checkpoint count");
    assert!(
        final_checkpoints >= 2,
        "periodic plus shutdown checkpoint complete"
    );
    let wal = store.child("catalog.sqlite-wal");
    let wal_bytes = fs::metadata(wal.path())
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    assert!(
        wal_bytes <= 2 * 1024 * 1024,
        "managed checkpoints bound generated WAL: {wal_bytes}"
    );
    assert!(
        records
            .iter()
            .any(|r| r["event"] == "catalog_final_checkpoint"),
        "shutdown checkpoint attempted"
    );
    let human = Command::cargo_bin("media-importer")
        .unwrap()
        .args(["--output", "human", "import", "--store"])
        .arg(store.path())
        .arg("--source")
        .arg(source.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(
        String::from_utf8(human)
            .unwrap()
            .contains("Import complete")
    );
    let pty_store = temp.child("pty-store");
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(assert_cmd::cargo::cargo_bin("media-importer"));
    command.env("TERM", "xterm-256color");
    command.args([
        "import",
        "--store",
        pty_store.path().to_str().unwrap(),
        "--source",
        source.path().to_str().unwrap(),
    ]);
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let output = thread::spawn(move || {
        let mut output = String::new();
        reader.read_to_string(&mut output).unwrap();
        output
    });
    let status = child.wait().unwrap();
    drop(pair.master);
    let output = output.join().unwrap();
    assert!(status.success(), "sustained PTY import failed: {output}");
    for line in ["Progress  ", "Ingest    ", "Hash      ", "Catalog   "] {
        assert!(
            output.contains(line),
            "PTY dashboard omitted {line:?}: {output}"
        );
    }
    assert!(output.contains("Import complete"));
    audit_clean(store.path());
    audit_clean(pty_store.path());
}

#[derive(Debug, Eq, PartialEq)]
struct DurableState {
    staging: Vec<TreeEntry>,
    cas: Vec<CasEntry>,
    catalog_blobs: Vec<CatalogBlob>,
    source_files: Vec<CatalogSource>,
    relationship_tables: Vec<String>,
    catalog_bytes: Vec<u8>,
    wal_exists: bool,
    wal_bytes: Vec<u8>,
}
#[derive(Debug, Eq, PartialEq)]
struct CasEntry {
    hash: String,
    size_bytes: u64,
    mode: u32,
}
#[derive(Debug, Eq, PartialEq)]
struct CatalogBlob {
    hash: String,
    size_bytes: i64,
    deleted_at_ms: Option<i64>,
}
#[derive(Debug, Eq, PartialEq)]
struct CatalogSource {
    relative_path: String,
    blob_hash: String,
    size_bytes: i64,
}
#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TreeEntry {
    relative: String,
    kind: String,
    bytes: Vec<u8>,
    mode: u32,
    modified_ns: u128,
}

fn durable_state(store: &Path) -> DurableState {
    let database = store.join("catalog.sqlite");
    let (catalog_blobs, source_files, relationship_tables) = Connection::open_with_flags(
        &database,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
        .ok()
        .map(|connection| {
            (
                connection
                    .prepare("SELECT hash, size_bytes, deleted_at_ms FROM blobs ORDER BY hash")
                    .and_then(|mut statement| {
                        statement
                            .query_map([], |row| {
                                Ok(CatalogBlob {
                                    hash: row.get(0)?,
                                    size_bytes: row.get(1)?,
                                    deleted_at_ms: row.get(2)?,
                                })
                            })?
                            .collect()
                    })
                    .unwrap_or_default(),
                connection
                    .prepare(
                        "SELECT relative_path, blob_hash, size_bytes FROM source_files ORDER BY relative_path",
                    )
                    .and_then(|mut statement| {
                        statement
                            .query_map([], |row| {
                                Ok(CatalogSource {
                                    relative_path: row.get(0)?,
                                    blob_hash: row.get(1)?,
                                    size_bytes: row.get(2)?,
                                })
                            })?
                            .collect()
                    })
                    .unwrap_or_default(),
                connection
                    .prepare(
                        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name LIKE '%relationship%' ORDER BY name",
                    )
                    .and_then(|mut statement| {
                        statement.query_map([], |row| row.get(0))?.collect()
                    })
                    .unwrap_or_default(),
            )
        })
        .unwrap_or_default();
    let wal = store.join("catalog.sqlite-wal");
    let mut cas = walkdir::WalkDir::new(store.join("blobs"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            let metadata = entry.metadata().expect("CAS entry metadata");
            CasEntry {
                hash: entry.file_name().to_string_lossy().into_owned(),
                size_bytes: metadata.len(),
                mode: metadata.permissions().mode(),
            }
        })
        .collect::<Vec<_>>();
    cas.sort_by(|left, right| left.hash.cmp(&right.hash));
    DurableState {
        staging: snapshot_tree(&store.join("staging")),
        cas,
        catalog_blobs,
        source_files,
        relationship_tables,
        catalog_bytes: fs::read(&database).unwrap_or_default(),
        wal_exists: wal.exists(),
        wal_bytes: fs::read(wal).unwrap_or_default(),
    }
}

fn assert_import_boundary_state(stage: &str, state: &DurableState, bytes: &[u8]) {
    let hash = blake3::hash(bytes).to_hex().to_string();
    let expected_cas = vec![CasEntry {
        hash: hash.clone(),
        size_bytes: bytes.len() as u64,
        mode: 0o100444,
    }];
    let expected_blobs = vec![CatalogBlob {
        hash: hash.clone(),
        size_bytes: bytes.len() as i64,
        deleted_at_ms: None,
    }];
    let expected_sources = vec![CatalogSource {
        relative_path: "file.bin".to_owned(),
        blob_hash: hash,
        size_bytes: bytes.len() as i64,
    }];
    assert!(
        state.relationship_tables.is_empty(),
        "{stage}: relationship persistence is deferred and must remain absent"
    );
    match stage {
        "store-after-staging-created" => {
            assert_eq!(state.cas, [], "{stage}: no CAS install before staging");
            assert_eq!(
                state.catalog_blobs,
                [],
                "{stage}: no catalog row before staging"
            );
            assert_eq!(
                state.source_files,
                [],
                "{stage}: no source row before staging"
            );
            assert_eq!(
                state.staging.len(),
                1,
                "{stage}: exactly one private staging file"
            );
            let staging = &state.staging[0];
            assert_eq!(staging.kind, "file");
            assert!(staging.relative.ends_with("2e746d70"));
            assert_eq!(staging.bytes, Vec::<u8>::new());
            assert_eq!(staging.mode, 0o100644);
        }
        "store-during-source-copy" => {
            assert_eq!(state.cas, [], "{stage}: no CAS install during copy");
            assert_eq!(
                state.catalog_blobs,
                [],
                "{stage}: no catalog row during copy"
            );
            assert_eq!(state.source_files, [], "{stage}: no source row during copy");
            assert_eq!(
                state.staging.len(),
                1,
                "{stage}: exactly one private staging file"
            );
            let staging = &state.staging[0];
            assert_eq!(staging.kind, "file");
            assert!(staging.relative.ends_with("2e746d70"));
            assert_eq!(staging.bytes, bytes);
            assert_eq!(staging.mode, 0o100644);
        }
        "source-worker-before-read" => {
            assert!(state.staging.is_empty(), "{stage}: no per-file staging yet");
            assert_eq!(state.cas, [], "{stage}: no CAS before source reads");
            assert_eq!(
                state.catalog_blobs,
                [],
                "{stage}: no catalog rows before source reads"
            );
            assert_eq!(
                state.source_files,
                [],
                "{stage}: no source rows before source reads"
            );
        }
        "ingest-after-cas-install"
        | "ingest-after-catalog-submission"
        | "catalog-writer-before-batch-commit" => {
            assert!(
                state.staging.is_empty(),
                "{stage}: installed staging is absent"
            );
            assert_eq!(
                state.cas, expected_cas,
                "{stage}: exact immutable CAS entry"
            );
            assert_eq!(
                state.catalog_blobs,
                [],
                "{stage}: uncommitted batch has no blob row"
            );
            assert_eq!(
                state.source_files,
                [],
                "{stage}: uncommitted batch has no source row"
            );
        }
        "catalog-writer-after-batch-commit"
        | "catalog-writer-before-checkpoint"
        | "catalog-writer-during-passive-checkpoint" => {
            assert!(state.staging.is_empty(), "{stage}: no stale staging");
            assert_eq!(
                state.cas, expected_cas,
                "{stage}: exact immutable CAS entry"
            );
            assert_eq!(
                state.catalog_blobs, expected_blobs,
                "{stage}: committed blob row"
            );
            assert_eq!(
                state.source_files, expected_sources,
                "{stage}: committed source row"
            );
            assert!(
                state.wal_exists,
                "{stage}: writer's WAL is durable at checkpoint seam"
            );
            assert!(
                !state.wal_bytes.is_empty(),
                "{stage}: checkpoint seam retains a non-empty WAL snapshot"
            );
        }
        _ => panic!("uncovered import lifecycle stage {stage}"),
    }
}

fn assert_read_only_preserves(
    store: &Path,
    browse: Option<&Path>,
    before: &DurableState,
    boundary: &str,
) {
    let audit = Command::cargo_bin("media-importer")
        .unwrap()
        .args(["audit", "--store"])
        .arg(store)
        .output()
        .unwrap();
    assert!(
        matches!(audit.status.code(), Some(0 | 2)),
        "{boundary}: audit only reports state"
    );
    let mut command = Command::cargo_bin("media-importer").unwrap();
    if let Some(browse) = browse {
        command
            .args(["build-tree", "--store"])
            .arg(store)
            .arg("--browse-tree")
            .arg(browse);
    } else {
        command.args(["gc", "--store"]).arg(store);
    }
    command.arg("--dry-run").assert().success();
    assert_eq!(
        &durable_state(store),
        before,
        "{boundary}: audit/dry-run repaired durable state"
    );
}

fn snapshot_tree(root: &Path) -> Vec<TreeEntry> {
    let mut result = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return result;
    };
    for entry in entries.filter_map(Result::ok) {
        snapshot_entry(root, &entry.path(), &mut result);
    }
    result.sort();
    result
}

fn snapshot_entry(root: &Path, path: &Path, result: &mut Vec<TreeEntry>) {
    let metadata = fs::symlink_metadata(path).unwrap();
    let kind = if metadata.file_type().is_dir() {
        "dir"
    } else if metadata.file_type().is_symlink() {
        "symlink"
    } else {
        "file"
    };
    let bytes = if metadata.file_type().is_file() {
        fs::read(path).unwrap()
    } else if metadata.file_type().is_symlink() {
        fs::read_link(path)
            .unwrap()
            .as_os_str()
            .as_encoded_bytes()
            .to_vec()
    } else {
        Vec::new()
    };
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    result.push(TreeEntry {
        relative: path
            .strip_prefix(root)
            .unwrap()
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        kind: kind.to_owned(),
        bytes,
        mode: metadata.permissions().mode(),
        modified_ns,
    });
    if metadata.file_type().is_dir() {
        for entry in fs::read_dir(path).unwrap().filter_map(Result::ok) {
            snapshot_entry(root, &entry.path(), result);
        }
    }
}
