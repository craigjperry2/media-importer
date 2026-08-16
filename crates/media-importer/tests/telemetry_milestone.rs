use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use media_importer::config::{ImportConfig, ImportOptions};
use media_importer::ingest::import_source_with_telemetry;
use media_importer::telemetry::{TelemetryEvent, TelemetrySink};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::fs;
use std::io::Read;
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn parse_jsonl(output: Vec<u8>) -> Vec<serde_json::Value> {
    String::from_utf8(output)
        .expect("UTF-8 JSON Lines")
        .lines()
        .map(|line| serde_json::from_str(line).expect("one complete JSON object"))
        .collect()
}

fn assert_jsonl_envelope(records: &[serde_json::Value], command: &str) {
    assert!(!records.is_empty(), "{command} emitted no telemetry");
    assert!(records.iter().all(|record| {
        record["schema_version"] == 1
            && record["command"] == command
            && record["event"].as_str().is_some_and(|event| {
                event
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character == '_')
            })
    }));
    assert_eq!(
        records.last().and_then(|record| record["event"].as_str()),
        Some("command_summary"),
        "summary must be the final append-only record"
    );
}

fn durable_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry
                    .path()
                    .strip_prefix(root)
                    .expect("root-relative durable path")
                    .to_string_lossy()
                    .into_owned(),
                fs::read(entry.path()).expect("durable file bytes"),
            )
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

#[test]
fn captured_stdout_uses_independently_parseable_json_lines_with_a_summary() {
    let temp = TempDir::new().expect("temporary directory");
    temp.child("source")
        .create_dir_all()
        .expect("source directory");
    temp.child("source/file.txt")
        .write_str("hello")
        .expect("source file");

    let output = Command::cargo_bin("media-importer")
        .expect("binary")
        .args(["import", "--store"])
        .arg(temp.child("store").path())
        .arg("--source")
        .arg(temp.child("source").path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let records = parse_jsonl(output);

    assert_jsonl_envelope(&records, "import");
    let summary = records.last().expect("summary event");
    assert_eq!(summary["event"], "command_summary");
    assert_eq!(summary["files_seen"], 1);
    assert_eq!(summary["bytes_hashed"], 5);
    assert_eq!(summary["dry_run"], false);

    let discovered = records
        .iter()
        .find(|record| record["event"] == "file_discovered")
        .expect("file discovery event");
    assert_eq!(discovered["path"], "file.txt");
    let stored = records
        .iter()
        .find(|record| record["event"] == "cas_blob_created")
        .expect("CAS creation event");
    let hash = stored["hash"].as_str().expect("string hash");
    assert!(
        hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    );
    assert!(stored["path"].is_string());
    assert!(stored["dry_run"].is_boolean());
}

#[test]
fn jsonl_operational_failure_ends_with_command_failed() {
    let temp = TempDir::new().expect("temporary directory");
    let source = temp.child("source");
    source.create_dir_all().expect("source directory");
    source
        .child("file.txt")
        .write_str("hello")
        .expect("source file");
    let probe = temp.child("failure-probe");
    probe.create_dir_all().expect("probe directory");
    let mut command = ProcessCommand::new(assert_cmd::cargo::cargo_bin("media-importer"));
    command
        .args(["--output", "jsonl", "import", "--store"])
        .arg(temp.child("store").path())
        .arg("--source")
        .arg(source.path())
        .env(
            "MEDIA_IMPORTER_TEST_LIFECYCLE_PROBE",
            format!("{}|scanner-before-next", probe.path().display()),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("spawn injected import");
    release_failed_probe(probe.path(), "scanner-before-next");
    let output = child.wait_with_output().expect("wait for injected import");
    assert_eq!(output.status.code(), Some(1));
    let records = parse_jsonl(output.stdout);
    assert_jsonl_envelope_failure(&records, "import");
}

fn release_failed_probe(probe: &Path, stage: &str) {
    let ready = probe.join(format!("{stage}.ready"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "command never reached {stage}");
        std::thread::sleep(Duration::from_millis(5));
    }
    fs::write(probe.join(format!("{stage}.fail")), "fail").expect("inject failure");
    fs::write(probe.join(format!("{stage}.release")), "release").expect("release command");
}

fn assert_jsonl_envelope_failure(records: &[serde_json::Value], command: &str) {
    assert!(!records.is_empty(), "{command} emitted no telemetry");
    assert!(
        records
            .iter()
            .all(|record| { record["schema_version"] == 1 && record["command"] == command })
    );
    assert_eq!(
        records.last().and_then(|record| record["event"].as_str()),
        Some("command_failed"),
        "operational failure must be the terminal append-only record"
    );
}

#[test]
fn explicit_output_modes_override_captured_stdout_detection() {
    let temp = TempDir::new().expect("temporary directory");
    temp.child("source")
        .create_dir_all()
        .expect("source directory");
    temp.child("source/file.txt")
        .write_str("hello")
        .expect("source file");

    Command::cargo_bin("media-importer")
        .expect("binary")
        .args(["--output", "human", "import", "--store"])
        .arg(temp.child("store").path())
        .arg("--source")
        .arg(temp.child("source").path())
        .assert()
        .success()
        .stdout(predicates::str::contains("Import complete"));

    Command::cargo_bin("media-importer")
        .expect("binary")
        .args(["--output", "jsonl", "audit", "--store"])
        .arg(temp.child("store").path())
        .assert()
        .success()
        .stdout(predicates::str::contains("\"event\":\"command_summary\""));
}

#[test]
fn auto_mode_uses_the_human_renderer_when_stdout_is_a_pty() {
    let temp = TempDir::new().expect("temporary directory");
    temp.child("source")
        .create_dir_all()
        .expect("source directory");
    temp.child("source/file.txt")
        .write_str("hello")
        .expect("source file");

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open test PTY");
    let mut command = CommandBuilder::new(assert_cmd::cargo::cargo_bin("media-importer"));
    let probe = temp.child("pty-probe");
    probe.create_dir_all().expect("PTY probe directory");
    command.env(
        "MEDIA_IMPORTER_TEST_LIFECYCLE_PROBE",
        format!("{}|scanner-before-next", probe.path().display()),
    );
    command.env("TERM", "xterm-256color");
    command.args([
        "import",
        "--store",
        temp.child("store")
            .path()
            .to_str()
            .expect("UTF-8 store path"),
        "--source",
        temp.child("source")
            .path()
            .to_str()
            .expect("UTF-8 source path"),
    ]);
    let mut child = pair.slave.spawn_command(command).expect("spawn in PTY");
    let probe_path = probe.path().to_owned();
    let release = std::thread::spawn(move || {
        let ready = probe_path.join("scanner-before-next.ready");
        for _ in 0..200 {
            if ready.exists() {
                std::thread::sleep(Duration::from_millis(150));
                fs::write(probe_path.join("scanner-before-next.release"), "release")
                    .expect("release PTY probe");
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("PTY dashboard probe never became ready");
    });
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let read_output = std::thread::spawn(move || {
        let mut output = String::new();
        reader
            .read_to_string(&mut output)
            .expect("read PTY command output");
        output
    });
    let status = child.wait().expect("wait for PTY command");
    release.join().expect("join PTY probe releaser");
    drop(pair.master);
    let output = read_output.join().expect("join PTY reader");

    assert!(status.success(), "PTY import failed: {output}");
    for line in ["Progress  ", "Ingest    ", "Hash      ", "Catalog   "] {
        assert!(
            output.contains(line),
            "PTY dashboard omitted {line:?}: {output}"
        );
    }
    assert!(output.contains("Import complete"), "{output}");
    assert_no_cursor_control_after(&output, "Import complete");
    assert!(
        !output.contains("\"event\":\"command_summary\""),
        "PTY selected JSON Lines instead of the terminal renderer: {output}"
    );
}

#[test]
fn jsonl_command_table_keeps_schema_summaries_and_statuses_stable() {
    let temp = TempDir::new().expect("temporary directory");
    temp.child("source")
        .create_dir_all()
        .expect("source directory");
    temp.child("source/file.txt")
        .write_str("hello")
        .expect("source file");
    let store = temp.child("store");
    let browse = temp.child("browse");

    let cases: Vec<(&str, Vec<String>, i32, &str)> = vec![
        (
            "import",
            vec![
                "import".into(),
                "--store".into(),
                store.path().display().to_string(),
                "--source".into(),
                temp.child("source").path().display().to_string(),
            ],
            0,
            "files_seen",
        ),
        (
            "audit",
            vec![
                "audit".into(),
                "--store".into(),
                store.path().display().to_string(),
            ],
            0,
            "clean",
        ),
        (
            "build_tree",
            vec![
                "build-tree".into(),
                "--store".into(),
                store.path().display().to_string(),
                "--browse-tree".into(),
                browse.path().display().to_string(),
            ],
            0,
            "desired_links",
        ),
        (
            "gc",
            vec![
                "gc".into(),
                "--dry-run".into(),
                "--store".into(),
                store.path().display().to_string(),
            ],
            0,
            "outcome",
        ),
    ];
    for (command, args, status, summary_field) in cases {
        let output = Command::cargo_bin("media-importer")
            .expect("binary")
            .arg("--output")
            .arg("jsonl")
            .args(args)
            .assert()
            .code(status)
            .get_output()
            .stdout
            .clone();
        let records = parse_jsonl(output);
        assert_jsonl_envelope(&records, command);
        assert!(
            records.last().expect("summary")[summary_field].is_boolean()
                || records.last().expect("summary")[summary_field].is_number()
                || records.last().expect("summary")[summary_field].is_string(),
            "{command} summary omitted typed {summary_field}"
        );
    }

    let blob = fs::read_dir(store.child("blobs").path())
        .expect("first shard")
        .next()
        .expect("first shard entry")
        .expect("first shard entry value")
        .path();
    let blob = fs::read_dir(blob)
        .expect("second shard")
        .next()
        .expect("second shard entry")
        .expect("second shard entry value")
        .path();
    let blob = fs::read_dir(blob)
        .expect("blob shard")
        .next()
        .expect("blob entry")
        .expect("blob entry value")
        .path();
    fs::remove_file(blob).expect("corrupt blob");
    let output = Command::cargo_bin("media-importer")
        .expect("binary")
        .args(["--output", "jsonl", "audit", "--store"])
        .arg(store.path())
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let records = parse_jsonl(output);
    assert_jsonl_envelope(&records, "audit");
    assert_eq!(records.last().expect("summary")["clean"], false);
    assert!(records.iter().any(|record| record["event"] == "finding"));
}

#[test]
fn gc_jsonl_preserves_blocked_and_incomplete_outcomes() {
    let temp = TempDir::new().expect("temporary directory");
    temp.child("source")
        .create_dir_all()
        .expect("source directory");
    temp.child("source/file.txt")
        .write_str("hello")
        .expect("source file");
    let store = temp.child("store");
    Command::cargo_bin("media-importer")
        .expect("binary")
        .args(["--output", "human", "import", "--store"])
        .arg(store.path())
        .arg("--source")
        .arg(temp.child("source").path())
        .assert()
        .success();

    let blob = only_blob_path(store.path());
    fs::remove_file(blob).expect("remove referenced blob");
    let blocked = Command::cargo_bin("media-importer")
        .expect("binary")
        .args(["--output", "jsonl", "gc", "--store"])
        .arg(store.path())
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let blocked = parse_jsonl(blocked);
    assert_jsonl_envelope(&blocked, "gc");
    let blocked_summary = blocked.last().expect("blocked summary");
    assert_eq!(blocked_summary["outcome"], "blocked");
    assert_eq!(blocked_summary["incomplete"], false);
    assert!(blocked.iter().any(|event| event["event"] == "finding"));

    // Recreate a clean store with one unreachable row, then fail at the
    // private post-preflight lifecycle seam. This reaches the CLI's
    // constructed-incomplete-report branch rather than a pre-report error.
    fs::remove_dir_all(store.path()).expect("remove test store");
    Command::cargo_bin("media-importer")
        .expect("binary")
        .args(["--output", "human", "import", "--store"])
        .arg(store.path())
        .arg("--source")
        .arg(temp.child("source").path())
        .assert()
        .success();
    rusqlite::Connection::open(store.path().join("catalog.sqlite"))
        .expect("open catalog")
        .execute("DELETE FROM source_files", [])
        .expect("make blob unreachable");
    let probe = temp.child("gc-probe");
    probe.create_dir_all().expect("probe directory");
    let mut command = ProcessCommand::new(assert_cmd::cargo::cargo_bin("media-importer"));
    command
        .args(["--output", "jsonl", "gc", "--store"])
        .arg(store.path())
        .env(
            "MEDIA_IMPORTER_TEST_LIFECYCLE_PROBE",
            format!("{}|gc-before-commit", probe.path().display()),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("spawn injected GC");
    let ready = probe.path().join("gc-before-commit.ready");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "GC never reached mutation seam");
        std::thread::sleep(Duration::from_millis(5));
    }
    fs::write(probe.path().join("gc-before-commit.fail"), "fail").expect("inject failure");
    fs::write(probe.path().join("gc-before-commit.release"), "release").expect("release GC");
    let output = child.wait_with_output().expect("wait for injected GC");
    assert_eq!(output.status.code(), Some(1));
    let incomplete = parse_jsonl(output.stdout);
    assert_jsonl_envelope(&incomplete, "gc");
    let incomplete_summary = incomplete.last().expect("incomplete summary");
    assert_eq!(incomplete_summary["outcome"], "incomplete");
    assert_eq!(incomplete_summary["incomplete"], true);
}

fn only_blob_path(store: &Path) -> std::path::PathBuf {
    let hash: String = rusqlite::Connection::open(store.join("catalog.sqlite"))
        .expect("open catalog")
        .query_row("SELECT hash FROM blobs", [], |row| row.get(0))
        .expect("catalog blob hash");
    store
        .join("blobs")
        .join(&hash[..2])
        .join(&hash[2..4])
        .join(hash)
}

#[test]
fn presentation_modes_preserve_dry_run_store_state() {
    let temp = TempDir::new().expect("temporary directory");
    temp.child("source")
        .create_dir_all()
        .expect("source directory");
    temp.child("source/file.txt")
        .write_str("hello")
        .expect("source file");
    let store = temp.child("store");
    Command::cargo_bin("media-importer")
        .expect("binary")
        .args(["--output", "human", "import", "--store"])
        .arg(store.path())
        .arg("--source")
        .arg(temp.child("source").path())
        .assert()
        .success();
    let before = durable_files(store.path());
    for mode in ["human", "jsonl"] {
        Command::cargo_bin("media-importer")
            .expect("binary")
            .args([
                "--output",
                mode,
                "import",
                "--dry-run",
                "--no-metadata-skip",
                "--store",
            ])
            .arg(store.path())
            .arg("--source")
            .arg(temp.child("source").path())
            .assert()
            .success();
        assert_eq!(
            durable_files(store.path()),
            before,
            "{mode} dry run mutated store"
        );
    }
}

#[test]
fn failing_renderer_cancels_concurrent_import_joins_workers_and_allows_rerun() {
    struct FailingAfterFirstHash(AtomicBool);
    impl TelemetrySink for FailingAfterFirstHash {
        fn emit(&self, event: TelemetryEvent) {
            if event.event == "file_hashed" {
                self.0.store(true, Ordering::Release);
            }
        }
        fn failed(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }

    let temp = TempDir::new().expect("temporary directory");
    temp.child("source")
        .create_dir_all()
        .expect("source directory");
    for number in 0..8 {
        temp.child(format!("source/file-{number}.bin"))
            .write_str(&format!("content-{number}"))
            .expect("source file");
    }
    let store = temp.child("store");
    let config = ImportConfig::from_options(ImportOptions {
        store: store.path().to_owned(),
        source: temp.child("source").path().to_owned(),
        db: None,
        dry_run: false,
        metadata_skip: false,
        chunk_size: NonZeroUsize::new(2).expect("non-zero chunk size"),
        workers_per_mount: NonZeroUsize::new(8).expect("non-zero workers"),
    })
    .expect("valid import configuration");
    let error = import_source_with_telemetry(
        config,
        Arc::new(FailingAfterFirstHash(AtomicBool::new(false))),
    )
    .expect_err("renderer failure must cancel import");
    assert!(error.to_string().contains("telemetry renderer failed"));

    Command::cargo_bin("media-importer")
        .expect("binary")
        .args(["--output", "jsonl", "import", "--store"])
        .arg(store.path())
        .arg("--source")
        .arg(temp.child("source").path())
        .assert()
        .success();
}

#[test]
fn pty_audit_finding_finishes_with_a_human_report() {
    let temp = TempDir::new().expect("temporary directory");
    temp.child("source")
        .create_dir_all()
        .expect("source directory");
    temp.child("source/file.txt")
        .write_str("hello")
        .expect("source file");
    Command::cargo_bin("media-importer")
        .expect("binary")
        .args(["--output", "human", "import", "--store"])
        .arg(temp.child("store").path())
        .arg("--source")
        .arg(temp.child("source").path())
        .assert()
        .success();
    let first_shard = fs::read_dir(temp.child("store/blobs").path())
        .expect("first shard")
        .next()
        .expect("first shard entry")
        .expect("first shard directory")
        .path();
    let second_shard = fs::read_dir(first_shard)
        .expect("second shard")
        .next()
        .expect("second shard entry")
        .expect("second shard directory")
        .path();
    let blob = fs::read_dir(second_shard)
        .expect("blob shard")
        .next()
        .expect("blob entry")
        .expect("blob file")
        .path();
    fs::remove_file(blob).expect("remove blob for audit finding");
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open test PTY");
    let mut command = CommandBuilder::new(assert_cmd::cargo::cargo_bin("media-importer"));
    command.args(["audit", "--store"]);
    command.arg(
        temp.child("store")
            .path()
            .to_str()
            .expect("UTF-8 store path"),
    );
    let mut child = pair.slave.spawn_command(command).expect("spawn in PTY");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let read_output = std::thread::spawn(move || {
        let mut output = String::new();
        reader
            .read_to_string(&mut output)
            .expect("read PTY command output");
        output
    });
    let status = child.wait().expect("wait for PTY command");
    drop(pair.master);
    let output = read_output.join().expect("join PTY reader");

    assert!(!status.success(), "audit unexpectedly succeeded: {output}");
    assert!(
        output.contains("Audit complete"),
        "audit did not leave its final human report after the dashboard: {output}"
    );
    assert_no_cursor_control_after(&output, "Audit complete");
    assert!(
        !output.contains("\"event\":\"command_summary\""),
        "PTY audit selected JSONL instead of the human renderer: {output}"
    );
}

fn assert_no_cursor_control_after(output: &str, final_heading: &str) {
    let heading = output
        .rfind(final_heading)
        .expect("final human report heading");
    assert!(
        !output[heading..].contains('\u{1b}'),
        "cursor control remained after the final human report: {}",
        &output[heading..]
    );
}

#[test]
fn import_jsonl_reconciles_real_staging_dry_run_and_writer_shutdown_checkpoint() {
    let temp = TempDir::new().expect("temporary directory");
    temp.child("source")
        .create_dir_all()
        .expect("source directory");
    temp.child("source/file.txt")
        .write_str("hello")
        .expect("source file");

    let real = Command::cargo_bin("media-importer")
        .expect("binary")
        .args(["--output", "jsonl", "import", "--store"])
        .arg(temp.child("store").path())
        .arg("--source")
        .arg(temp.child("source").path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let real: Vec<serde_json::Value> = String::from_utf8(real)
        .expect("UTF-8 JSON Lines")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON record"))
        .collect();
    assert!(
        real.iter()
            .any(|record| record["event"] == "staging_bytes_delta")
    );
    assert!(real.iter().any(|record| {
        record["event"] == "catalog_final_checkpoint" && record["checkpoints"].as_u64() >= Some(1)
    }));

    let dry_run = Command::cargo_bin("media-importer")
        .expect("binary")
        .args([
            "--output",
            "jsonl",
            "import",
            "--dry-run",
            "--no-metadata-skip",
            "--store",
        ])
        .arg(temp.child("store").path())
        .arg("--source")
        .arg(temp.child("source").path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let dry_run: Vec<serde_json::Value> = String::from_utf8(dry_run)
        .expect("UTF-8 JSON Lines")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON record"))
        .collect();
    assert!(
        dry_run
            .iter()
            .any(|record| record["event"] == "source_bytes_delta")
    );
    assert!(
        !dry_run
            .iter()
            .any(|record| record["event"] == "staging_bytes_delta")
    );
}
