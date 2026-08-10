use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use assert_fs::TempDir;
use media_importer::audit::audit_store;
use media_importer::config::{AuditConfig, AuditOptions, DEFAULT_CHUNK_SIZE};
use media_importer::paths::StoreRoot;
use media_importer::run_lock::{LockMode, StoreRunLock};
use rusqlite::Connection;
use walkdir::WalkDir;

const HELPER_ENV: &str = "MEDIA_IMPORTER_RUN_LOCK_HELPER";
const WAIT: Duration = Duration::from_secs(5);

#[test]
fn helper_holds_store_lock() {
    let Ok(spec) = std::env::var(HELPER_ENV) else {
        return;
    };
    let mut fields = spec.splitn(4, '|');
    let store = PathBuf::from(fields.next().expect("store path"));
    let mode = match fields.next().expect("mode") {
        "shared" => LockMode::Shared,
        "exclusive" => LockMode::Exclusive,
        other => panic!("unknown lock mode {other}"),
    };
    let ready = PathBuf::from(fields.next().expect("ready path"));
    let release = PathBuf::from(fields.next().expect("release path"));
    let root = StoreRoot::validate(&store).expect("validate store");
    let _lock = StoreRunLock::acquire(&root, "test-helper", mode).expect("acquire lock");
    fs::write(&ready, "acquired").expect("write ready sentinel");
    wait_for(&release, "release sentinel");
}

#[test]
fn shared_holders_overlap_and_shared_blocks_exclusive_until_release() {
    let fixture = StoreFixture::new("store");
    let first = fixture.spawn("first", LockMode::Shared);
    fixture.wait_ready("first");
    let second = fixture.spawn("second", LockMode::Shared);
    fixture.wait_ready("second");
    let exclusive = fixture.spawn("exclusive", LockMode::Exclusive);
    fixture.assert_not_ready("exclusive");
    fixture.release("first");
    fixture.release("second");
    wait_child(first);
    wait_child(second);
    fixture.wait_ready("exclusive");
    fixture.release("exclusive");
    wait_child(exclusive);
}

#[test]
fn exclusive_blocks_shared_and_exclusive_until_normal_exit() {
    let fixture = StoreFixture::new("store");
    let holder = fixture.spawn("holder", LockMode::Exclusive);
    fixture.wait_ready("holder");
    let shared = fixture.spawn("shared", LockMode::Shared);
    fixture.assert_not_ready("shared");
    fixture.release("holder");
    wait_child(holder);
    fixture.wait_ready("shared");
    fixture.release("shared");
    wait_child(shared);

    let holder = fixture.spawn("second-holder", LockMode::Exclusive);
    fixture.wait_ready("second-holder");
    let exclusive = fixture.spawn("exclusive", LockMode::Exclusive);
    fixture.assert_not_ready("exclusive");
    fixture.release("second-holder");
    wait_child(holder);
    fixture.wait_ready("exclusive");
    fixture.release("exclusive");
    wait_child(exclusive);
}

#[test]
fn termination_releases_lock_and_aliases_coordinate() {
    let fixture = StoreFixture::new("store");
    let holder = fixture.spawn("holder", LockMode::Exclusive);
    fixture.wait_ready("holder");
    let alias = fixture
        .store
        .parent()
        .expect("store parent")
        .join("store/../store");
    let blocked = fixture.spawn_for(&alias, "alias", LockMode::Shared);
    fixture.assert_not_ready("alias");
    let mut holder = holder;
    holder.kill().expect("terminate holder");
    assert!(!holder.wait().expect("wait for terminated holder").success());
    fixture.wait_ready("alias");
    fixture.release("alias");
    wait_child(blocked);
}

#[test]
fn distinct_store_roots_do_not_block_each_other() {
    let first = StoreFixture::new("first");
    let second = StoreFixture::new("second");
    let holder = first.spawn("holder", LockMode::Exclusive);
    first.wait_ready("holder");
    let independent = second.spawn("independent", LockMode::Exclusive);
    second.wait_ready("independent");
    first.release("holder");
    second.release("independent");
    wait_child(holder);
    wait_child(independent);
}

#[test]
fn replaced_regular_file_and_symlink_fail_acquisition_with_context_before_audit_body() {
    let fixture = CommandFixture::imported();
    let config = audit_config(&fixture.store);
    let original = fixture.store.with_extension("original");
    fs::rename(&fixture.store, &original).expect("move store aside");
    fs::write(&fixture.store, "not a directory").expect("replace store with file");
    let error = audit_store(config.clone()).expect_err("regular file must not be locked");
    let rendered = format!("{error:?}");
    assert!(rendered.contains("audit"));
    assert!(rendered.contains("shared"));
    assert!(rendered.contains(&fixture.store.display().to_string()));
    assert!(rendered.contains("not a directory"));
    fs::remove_file(&fixture.store).expect("remove replacement file");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&original, &fixture.store).expect("replace store with symlink");
        let error = audit_store(config).expect_err("symlink must not be followed");
        let rendered = format!("{error:?}");
        assert!(rendered.contains("audit"));
        assert!(rendered.contains("shared"));
        assert!(rendered.contains(&fixture.store.display().to_string()));
        assert!(rendered.contains("open store directory"));
    }
}

#[test]
fn early_operational_error_releases_a_command_lock() {
    let fixture = CommandFixture::imported();
    let config = audit_config(&fixture.store);
    fs::remove_file(fixture.store.join("catalog.sqlite")).expect("remove catalog after validation");
    let _ = audit_store(config).expect_err("audit must fail after acquiring its lock");
    let holder = fixture.lock_holder("after-error", LockMode::Exclusive);
    fixture.wait_ready("after-error");
    fixture.release("after-error");
    wait_child(holder);
}

#[test]
fn exclusive_lock_prevents_new_import_bootstrap_until_release() {
    let fixture = CommandFixture::new();
    fs::create_dir(&fixture.store).expect("create empty store root");
    let holder = fixture.lock_holder("holder", LockMode::Exclusive);
    fixture.wait_ready("holder");
    let mut import = fixture.command(&[
        "import",
        "--store",
        fixture.store_str(),
        "--source",
        fixture.source_str(),
    ]);
    fixture.assert_running(&mut import, "import");
    assert!(
        fs::read_dir(&fixture.store)
            .expect("read empty root")
            .next()
            .is_none()
    );
    fixture.release("holder");
    wait_child(holder);
    wait_command(import);
    assert!(fixture.store.join("blobs").is_dir());
    assert!(fixture.store.join("staging").is_dir());
    assert!(fixture.store.join("catalog.sqlite").is_file());
}

#[test]
fn exclusive_lock_blocks_all_existing_store_readers_before_their_work() {
    let fixture = CommandFixture::imported();
    for (name, arguments) in [
        ("audit", vec!["audit", "--store", fixture.store_str()]),
        (
            "import-dry",
            vec![
                "import",
                "--store",
                fixture.store_str(),
                "--source",
                fixture.source_str(),
                "--dry-run",
            ],
        ),
        (
            "tree-dry",
            vec![
                "build-tree",
                "--store",
                fixture.store_str(),
                "--browse-tree",
                fixture.browse_str(),
                "--dry-run",
            ],
        ),
        (
            "gc-dry",
            vec!["gc", "--store", fixture.store_str(), "--dry-run"],
        ),
    ] {
        let holder = fixture.lock_holder(name, LockMode::Exclusive);
        fixture.wait_ready(name);
        let mut command = fixture.command(&arguments);
        fixture.assert_running(&mut command, name);
        fixture.release(name);
        wait_child(holder);
        wait_command(command);
    }
}

#[test]
fn shared_lock_allows_readers_and_blocks_all_real_writers() {
    let fixture = CommandFixture::imported();
    let holder = fixture.lock_holder("shared", LockMode::Shared);
    fixture.wait_ready("shared");
    for arguments in [
        vec!["audit", "--store", fixture.store_str()],
        vec![
            "import",
            "--store",
            fixture.store_str(),
            "--source",
            fixture.source_str(),
            "--dry-run",
        ],
        vec![
            "build-tree",
            "--store",
            fixture.store_str(),
            "--browse-tree",
            fixture.browse_str(),
            "--dry-run",
        ],
        vec!["gc", "--store", fixture.store_str(), "--dry-run"],
    ] {
        wait_command(fixture.command(&arguments));
    }
    let mut import = fixture.command(&[
        "import",
        "--store",
        fixture.store_str(),
        "--source",
        fixture.source_str(),
    ]);
    let mut tree = fixture.command(&[
        "build-tree",
        "--store",
        fixture.store_str(),
        "--browse-tree",
        fixture.browse_str(),
    ]);
    let mut gc = fixture.command(&["gc", "--store", fixture.store_str()]);
    fixture.assert_running(&mut import, "real import");
    fixture.assert_running(&mut tree, "real build-tree");
    fixture.assert_running(&mut gc, "real gc");
    fixture.release("shared");
    wait_child(holder);
    wait_command(import);
    wait_command(tree);
    wait_command(gc);
}

#[test]
fn dry_runs_preserve_existing_state_and_missing_store_import_stays_unlocked() {
    let fixture = CommandFixture::imported();
    let before = tree_state(&fixture.store);
    for (name, arguments) in [
        ("audit", vec!["audit", "--store", fixture.store_str()]),
        (
            "import",
            vec![
                "import",
                "--store",
                fixture.store_str(),
                "--source",
                fixture.source_str(),
                "--dry-run",
            ],
        ),
        (
            "build-tree",
            vec![
                "build-tree",
                "--store",
                fixture.store_str(),
                "--browse-tree",
                fixture.browse_str(),
                "--dry-run",
            ],
        ),
        (
            "gc",
            vec!["gc", "--store", fixture.store_str(), "--dry-run"],
        ),
    ] {
        wait_command(fixture.command(&arguments));
        assert_eq!(
            tree_state(&fixture.store),
            before,
            "{name} dry-run changed durable store state"
        );
    }
    assert!(!fixture.store.join(".media-importer.lock").exists());

    let missing = fixture.temp.path().join("missing-store");
    wait_command(fixture.command(&[
        "import",
        "--store",
        missing.to_str().expect("utf-8 path"),
        "--source",
        fixture.source_str(),
        "--dry-run",
    ]));
    assert!(!missing.exists());
}

#[test]
fn every_catalog_reader_uses_a_wal_snapshot_without_mutating_source_files() {
    let fixture = CommandFixture::imported();
    let db_path = fixture.store.join("catalog.sqlite");
    let connection = Connection::open(&db_path).expect("open catalog");
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .expect("enable WAL");
    let content = b"snapshot-only";
    let hash = blake3::hash(content).to_hex().to_string();
    let blob = fixture
        .store
        .join("blobs")
        .join(&hash[..2])
        .join(&hash[2..4])
        .join(&hash);
    fs::create_dir_all(blob.parent().expect("blob parent")).expect("create blob parent");
    fs::write(&blob, content).expect("write blob");
    connection
        .execute(
            "INSERT INTO blobs(hash,size_bytes,created_at_ms) VALUES (?1,?2,1)",
            (&hash, content.len() as i64),
        )
        .expect("write committed WAL row");
    let before = tree_state(&fixture.store);

    for arguments in [
        vec!["audit", "--store", fixture.store_str()],
        vec![
            "import",
            "--store",
            fixture.store_str(),
            "--source",
            fixture.source_str(),
            "--dry-run",
        ],
        vec![
            "build-tree",
            "--store",
            fixture.store_str(),
            "--browse-tree",
            fixture.browse_str(),
            "--dry-run",
        ],
        vec!["gc", "--store", fixture.store_str(), "--dry-run"],
        vec![
            "build-tree",
            "--store",
            fixture.store_str(),
            "--browse-tree",
            fixture.browse_str(),
        ],
    ] {
        wait_command(fixture.command(&arguments));
        assert_eq!(
            tree_state(&fixture.store),
            before,
            "shared snapshot changed source state"
        );
    }
    drop(connection);
}

#[test]
fn racing_new_store_imports_serialize_to_one_valid_catalog() {
    let fixture = CommandFixture::new();
    let first = fixture.command(&[
        "import",
        "--store",
        fixture.store_str(),
        "--source",
        fixture.source_str(),
    ]);
    let second = fixture.command(&[
        "import",
        "--store",
        fixture.store_str(),
        "--source",
        fixture.source_str(),
    ]);
    wait_command(first);
    wait_command(second);
    let connection = Connection::open(fixture.store.join("catalog.sqlite")).expect("open catalog");
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM blobs", [], |row| row.get::<_, i64>(0))
            .expect("blob count"),
        1
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM source_files", [], |row| row
                .get::<_, i64>(0))
            .expect("source count"),
        1
    );
}

#[cfg(unix)]
#[test]
fn import_accepts_a_store_symlink_alias_and_coordinates_on_its_target_inode() {
    let fixture = CommandFixture::new();
    fs::create_dir(&fixture.store).expect("store root");
    let alias = fixture.temp.path().join("store-alias");
    std::os::unix::fs::symlink(&fixture.store, &alias).expect("create store alias");
    let holder = fixture.lock_holder("holder", LockMode::Exclusive);
    fixture.wait_ready("holder");
    let mut import = fixture.command(&[
        "import",
        "--store",
        alias.to_str().expect("utf-8 alias"),
        "--source",
        fixture.source_str(),
    ]);
    fixture.assert_running(&mut import, "import through symlink alias");
    assert!(
        fs::read_dir(&fixture.store)
            .expect("read store")
            .next()
            .is_none(),
        "import body began before target inode lock was available"
    );
    fixture.release("holder");
    wait_child(holder);
    wait_command(import);
    assert!(fixture.store.join("catalog.sqlite").is_file());
}

#[test]
fn real_build_tree_retains_exclusive_lock_through_planning_application_and_report_construction() {
    let fixture = CommandFixture::imported();

    let planned_probe = TempDir::new().expect("planning probe directory");
    let planned = fixture.probed_command(
        &[
            "build-tree",
            "--store",
            fixture.store_str(),
            "--browse-tree",
            fixture.browse_str(),
        ],
        planned_probe.path(),
        "build-tree-planned",
    );
    wait_probe(planned_probe.path(), "build-tree-planned");
    assert!(
        !fixture.browse.exists(),
        "planning probe fired after browse-tree application"
    );
    let planning_holder = assert_command_holds_exclusive_lock(&fixture, "planning-holder");
    release_probe(planned_probe.path(), "build-tree-planned");
    wait_command(planned);
    fixture.wait_ready("planning-holder");
    fixture.release("planning-holder");
    wait_child(planning_holder);

    let applied_probe = TempDir::new().expect("application probe directory");
    let applied = fixture.probed_command(
        &[
            "build-tree",
            "--store",
            fixture.store_str(),
            "--browse-tree",
            fixture.browse_str(),
        ],
        applied_probe.path(),
        "build-tree-applied-report-constructed",
    );
    wait_probe(
        applied_probe.path(),
        "build-tree-applied-report-constructed",
    );
    assert!(
        WalkDir::new(&fixture.browse)
            .into_iter()
            .filter_map(Result::ok)
            .any(|entry| entry.file_type().is_symlink()),
        "application/report probe fired before a browse-tree link existed"
    );
    let application_holder = assert_command_holds_exclusive_lock(&fixture, "application-holder");
    release_probe(
        applied_probe.path(),
        "build-tree-applied-report-constructed",
    );
    wait_command(applied);
    fixture.wait_ready("application-holder");
    fixture.release("application-holder");
    wait_child(application_holder);
}

#[test]
fn real_gc_retains_exclusive_lock_through_preflight_commit_and_incomplete_outcome_construction() {
    let fixture = CommandFixture::imported();
    Connection::open(fixture.store.join("catalog.sqlite"))
        .expect("open catalog")
        .execute("DELETE FROM source_files", [])
        .expect("make blob unreachable");

    let preflight_probe = TempDir::new().expect("preflight probe directory");
    let preflight = fixture.probed_command(
        &["gc", "--store", fixture.store_str()],
        preflight_probe.path(),
        "gc-preflight-complete",
    );
    wait_probe(preflight_probe.path(), "gc-preflight-complete");
    let preflight_holder = assert_command_holds_exclusive_lock(&fixture, "preflight-holder");
    release_probe(preflight_probe.path(), "gc-preflight-complete");
    wait_command(preflight);
    fixture.wait_ready("preflight-holder");
    fixture.release("preflight-holder");
    wait_child(preflight_holder);

    let commit_probe = TempDir::new().expect("commit probe directory");
    let commit = fixture.probed_command(
        &["gc", "--store", fixture.store_str()],
        commit_probe.path(),
        "gc-commit-complete",
    );
    wait_probe(commit_probe.path(), "gc-commit-complete");
    let commit_holder = assert_command_holds_exclusive_lock(&fixture, "commit-holder");
    release_probe(commit_probe.path(), "gc-commit-complete");
    wait_command(commit);
    fixture.wait_ready("commit-holder");
    fixture.release("commit-holder");
    wait_child(commit_holder);

    fs::write(fixture.source.join("second.jpg"), b"second image").expect("add second source");
    wait_command(fixture.command(&[
        "import",
        "--store",
        fixture.store_str(),
        "--source",
        fixture.source_str(),
    ]));
    Connection::open(fixture.store.join("catalog.sqlite"))
        .expect("open catalog")
        .execute("DELETE FROM source_files", [])
        .expect("make second blob unreachable");

    let incomplete_probe = TempDir::new().expect("incomplete probe directory");
    let mut incomplete = fixture.probed_command(
        &["gc", "--store", fixture.store_str()],
        incomplete_probe.path(),
        "gc-before-commit,gc-outcome-constructed",
    );
    wait_probe(incomplete_probe.path(), "gc-before-commit");
    fs::write(
        incomplete_probe.path().join("gc-before-commit.fail"),
        "fail",
    )
    .expect("inject incomplete GC outcome");
    release_probe(incomplete_probe.path(), "gc-before-commit");
    wait_probe(incomplete_probe.path(), "gc-outcome-constructed");
    let incomplete_holder =
        assert_command_holds_exclusive_lock(&fixture, "incomplete-outcome-holder");
    release_probe(incomplete_probe.path(), "gc-outcome-constructed");
    assert!(
        !incomplete.wait().expect("wait for incomplete GC").success(),
        "injected mutation failure must reach the CLI incomplete outcome path"
    );
    fixture.wait_ready("incomplete-outcome-holder");
    fixture.release("incomplete-outcome-holder");
    wait_child(incomplete_holder);
}

#[test]
fn cli_lock_acquisition_failure_exits_one_without_rendering_a_report() {
    let fixture = CommandFixture::imported();
    let probe = TempDir::new().expect("lock probe directory");
    let command = fixture.probed_command(
        &["audit", "--store", fixture.store_str()],
        probe.path(),
        "lock-before-open",
    );
    wait_probe(probe.path(), "lock-before-open");
    let original = fixture.store.with_extension("original");
    fs::rename(&fixture.store, &original).expect("move store aside after CLI validation");
    fs::write(&fixture.store, "not a directory").expect("replace store before lock open");
    release_probe(probe.path(), "lock-before-open");
    let output = command.wait_with_output().expect("wait for lock failure");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        output.stdout.is_empty(),
        "lock failure rendered a command report"
    );
}

#[test]
fn catalog_writer_startup_failure_is_bounded_contextual_and_releases_store_lock() {
    assert_writer_cli_failure(
        "startup",
        "catalog-writer-before-ready",
        |_| {},
        |probe| {
            wait_probe(probe, "catalog-writer-before-ready");
            inject_probe_marker(probe, "catalog-writer-before-ready", "fail");
            release_probe(probe, "catalog-writer-before-ready");
        },
    );
}

#[test]
fn catalog_writer_send_after_receiver_closed_is_bounded_contextual_and_releases_store_lock() {
    assert_writer_cli_failure(
        "send-after-receiver-closed",
        "catalog-writer-after-request-accepted",
        |fixture| {
            fs::write(
                fixture.source.join("z-second-photo.jpg"),
                b"second test image",
            )
            .expect("second source file");
        },
        |probe| {
            wait_probe(probe, "catalog-writer-after-request-accepted");
            inject_probe_marker(probe, "catalog-writer-after-request-accepted", "fail");
            release_probe(probe, "catalog-writer-after-request-accepted");
        },
    );
}

#[test]
fn catalog_writer_accepted_request_response_closure_is_bounded_contextual_and_releases_store_lock()
{
    assert_writer_cli_failure(
        "accepted-request-response-closure",
        "catalog-writer-after-request-accepted",
        |_| {},
        |probe| {
            wait_probe(probe, "catalog-writer-after-request-accepted");
            inject_probe_marker(probe, "catalog-writer-after-request-accepted", "fail");
            release_probe(probe, "catalog-writer-after-request-accepted");
        },
    );
}

#[test]
fn catalog_writer_post_readiness_panic_is_joined_contextually_and_releases_store_lock() {
    assert_writer_cli_failure(
        "post-readiness-panic",
        "catalog-writer-after-request-accepted",
        |_| {},
        |probe| {
            wait_probe(probe, "catalog-writer-after-request-accepted");
            inject_probe_marker(probe, "catalog-writer-after-request-accepted", "panic");
            release_probe(probe, "catalog-writer-after-request-accepted");
        },
    );
}

#[test]
fn sqlite_checkpoint_failure_is_contextual_and_releases_the_store_lock() {
    let fixture = CommandFixture::new();
    let probe = TempDir::new().expect("checkpoint probe directory");
    let command = fixture.probed_command_with_output(
        &[
            "import",
            "--store",
            fixture.store_str(),
            "--source",
            fixture.source_str(),
        ],
        probe.path(),
        "catalog-writer-checkpoint-sqlite-error",
    );
    let output = wait_failed_command_bounded(command, "SQLite checkpoint");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("checkpoint"),
        "missing checkpoint context: {stderr}"
    );
    assert!(
        stderr.contains("SQL logic error") || stderr.contains("syntax error"),
        "missing SQLite cause: {stderr}"
    );
    let holder = fixture.lock_holder("checkpoint-released", LockMode::Exclusive);
    fixture.wait_ready("checkpoint-released");
    fixture.release("checkpoint-released");
    wait_child(holder);
}

struct StoreFixture {
    temp: TempDir,
    store: PathBuf,
}

impl StoreFixture {
    fn new(name: &str) -> Self {
        let temp = TempDir::new().expect("temporary directory");
        let store = temp.path().join(name);
        fs::create_dir(&store).expect("store root");
        Self { temp, store }
    }

    fn spawn(&self, name: &str, mode: LockMode) -> Child {
        self.spawn_for(&self.store, name, mode)
    }
    fn spawn_for(&self, store: &Path, name: &str, mode: LockMode) -> Child {
        spawn_lock_holder(store, &self.temp, name, mode)
    }
    fn wait_ready(&self, name: &str) {
        wait_for(
            &self.temp.path().join(format!("{name}.ready")),
            "ready sentinel",
        );
    }
    fn assert_not_ready(&self, name: &str) {
        assert_not_ready(&self.temp.path().join(format!("{name}.ready")), name);
    }
    fn release(&self, name: &str) {
        release(&self.temp, name);
    }
}

struct CommandFixture {
    temp: TempDir,
    store: PathBuf,
    source: PathBuf,
    browse: PathBuf,
}

impl CommandFixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temporary directory");
        let source = temp.path().join("source");
        fs::create_dir(&source).expect("source directory");
        fs::write(source.join("photo.jpg"), b"test image").expect("source file");
        Self {
            store: temp.path().join("store"),
            browse: temp.path().join("browse"),
            temp,
            source,
        }
    }
    fn imported() -> Self {
        let fixture = Self::new();
        wait_command(fixture.command(&[
            "import",
            "--store",
            fixture.store_str(),
            "--source",
            fixture.source_str(),
        ]));
        fixture
    }
    fn command(&self, arguments: &[&str]) -> Child {
        Command::new(env!("CARGO_BIN_EXE_media-importer"))
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn CLI")
    }
    fn probed_command(&self, arguments: &[&str], probe: &Path, stages: &str) -> Child {
        Command::new(env!("CARGO_BIN_EXE_media-importer"))
            .args(arguments)
            .env(
                "MEDIA_IMPORTER_TEST_LIFECYCLE_PROBE",
                format!("{}|{stages}", probe.display()),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn probed CLI")
    }
    fn probed_command_with_output(&self, arguments: &[&str], probe: &Path, stages: &str) -> Child {
        Command::new(env!("CARGO_BIN_EXE_media-importer"))
            .args(arguments)
            .env(
                "MEDIA_IMPORTER_TEST_LIFECYCLE_PROBE",
                format!("{}|{stages}", probe.display()),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn probed CLI")
    }
    fn lock_holder(&self, name: &str, mode: LockMode) -> Child {
        spawn_lock_holder(&self.store, &self.temp, name, mode)
    }
    fn wait_ready(&self, name: &str) {
        wait_for(
            &self.temp.path().join(format!("{name}.ready")),
            "ready sentinel",
        );
    }
    fn release(&self, name: &str) {
        release(&self.temp, name);
    }
    fn assert_not_ready(&self, name: &str) {
        assert_not_ready(&self.temp.path().join(format!("{name}.ready")), name);
    }
    fn assert_running(&self, child: &mut Child, description: &str) {
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            assert!(
                child.try_wait().expect("check command").is_none(),
                "{description} completed while lock was held"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn store_str(&self) -> &str {
        self.store.to_str().expect("utf-8 path")
    }
    fn source_str(&self) -> &str {
        self.source.to_str().expect("utf-8 path")
    }
    fn browse_str(&self) -> &str {
        self.browse.to_str().expect("utf-8 path")
    }
}

fn audit_config(store: &Path) -> AuditConfig {
    AuditConfig::from_options(AuditOptions {
        store: store.to_path_buf(),
        db: None,
        chunk_size: DEFAULT_CHUNK_SIZE,
    })
    .expect("audit config")
}

fn spawn_lock_holder(store: &Path, temp: &TempDir, name: &str, mode: LockMode) -> Child {
    let ready = temp.path().join(format!("{name}.ready"));
    let release_path = temp.path().join(format!("{name}.release"));
    let mode = match mode {
        LockMode::Shared => "shared",
        LockMode::Exclusive => "exclusive",
    };
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "helper_holds_store_lock", "--nocapture"])
        .env(
            HELPER_ENV,
            format!(
                "{}|{mode}|{}|{}",
                store.display(),
                ready.display(),
                release_path.display()
            ),
        )
        .spawn()
        .expect("spawn lock helper")
}

fn tree_state(root: &Path) -> Vec<(PathBuf, Vec<u8>, bool, std::time::SystemTime)> {
    let mut state = WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .map(|entry| {
            let entry = entry.expect("walk store");
            let relative = entry
                .path()
                .strip_prefix(root)
                .expect("relative store path")
                .to_path_buf();
            let content = if entry.file_type().is_file() {
                fs::read(entry.path()).expect("read store file")
            } else {
                Vec::new()
            };
            let metadata = fs::symlink_metadata(entry.path()).expect("stat store entry");
            (
                relative,
                content,
                metadata.permissions().readonly(),
                metadata.modified().expect("store entry modified time"),
            )
        })
        .collect::<Vec<_>>();
    state.sort();
    state
}

fn release(temp: &TempDir, name: &str) {
    fs::write(temp.path().join(format!("{name}.release")), "release")
        .expect("write release sentinel");
}
fn wait_probe(probe: &Path, stage: &str) {
    wait_for(&probe.join(format!("{stage}.ready")), "lifecycle probe");
}
fn release_probe(probe: &Path, stage: &str) {
    fs::write(probe.join(format!("{stage}.release")), "release").expect("release lifecycle probe");
}
fn assert_command_holds_exclusive_lock(fixture: &CommandFixture, name: &str) -> Child {
    let holder = fixture.lock_holder(name, LockMode::Exclusive);
    fixture.assert_not_ready(name);
    holder
}
fn assert_not_ready(path: &Path, name: &str) {
    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        assert!(!path.exists(), "{name} acquired a conflicting lock");
        thread::sleep(Duration::from_millis(10));
    }
}
fn wait_for(path: &Path, description: &str) {
    let deadline = Instant::now() + WAIT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}: {path:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn wait_child(mut child: Child) {
    assert!(
        child.wait().expect("wait for helper").success(),
        "lock helper failed"
    );
}
fn wait_command(mut child: Child) {
    assert!(
        child.wait().expect("wait for command").success(),
        "command failed"
    );
}

fn assert_writer_cli_failure(
    name: &str,
    stages: &str,
    setup: impl FnOnce(&CommandFixture),
    drive_failure: impl FnOnce(&Path),
) {
    let fixture = CommandFixture::new();
    setup(&fixture);
    let probe = TempDir::new().expect("writer lifecycle probe directory");
    let command = fixture.probed_command_with_output(
        &[
            "import",
            "--store",
            fixture.store_str(),
            "--source",
            fixture.source_str(),
        ],
        probe.path(),
        stages,
    );
    drive_failure(probe.path());
    let output = wait_failed_command_bounded(command, name);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("catalog writer"),
        "{name} omitted writer context: {stderr}"
    );
    assert!(
        stderr.contains("catalog.sqlite") || stderr.contains("catalog"),
        "{name} omitted catalog context: {stderr}"
    );

    let lock_name = format!("{name}-released");
    let holder = fixture.lock_holder(&lock_name, LockMode::Exclusive);
    fixture.wait_ready(&lock_name);
    fixture.release(&lock_name);
    wait_child(holder);
}

fn inject_probe_marker(probe: &Path, stage: &str, marker: &str) {
    fs::write(probe.join(format!("{stage}.{marker}")), marker)
        .expect("inject writer lifecycle failure");
}

fn wait_failed_command_bounded(mut child: Child, description: &str) -> std::process::Output {
    let deadline = Instant::now() + WAIT;
    loop {
        if child.try_wait().expect("check failed command").is_some() {
            let output = child
                .wait_with_output()
                .expect("collect failed command output");
            assert!(
                !output.status.success(),
                "{description} writer failure unexpectedly succeeded"
            );
            return output;
        }
        assert!(
            Instant::now() < deadline,
            "{description} writer failure hung"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
