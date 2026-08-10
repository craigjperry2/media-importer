use assert_fs::TempDir;
use media_importer::catalog::{
    BlobRecord, CatalogWriterConfig, CatalogWriterEvent, CatalogWriterHandle, SourceObservation,
};
use media_importer::paths::{BlobHash, SourceRelativePath};
use rusqlite::{Connection, Error, ErrorCode, params};
use std::num::NonZeroUsize;
use std::time::Duration;

fn create_real_catalog(temp: &TempDir) -> Connection {
    let path = temp.path().join("catalog.sqlite");
    CatalogWriterHandle::spawn(path.clone(), CatalogWriterConfig::default())
        .expect("initialize catalog through its writer API")
        .finish()
        .expect("finish catalog writer");
    Connection::open(path).expect("inspect initialized catalog")
}

#[test]
fn real_catalog_has_versioned_integer_millisecond_timestamps() {
    let temp = TempDir::new().expect("temporary catalog directory");
    let connection = create_real_catalog(&temp);

    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read schema version");
    assert_eq!(version, 1, "catalog PRAGMA user_version must be 1");

    for table in ["blobs", "source_files"] {
        let columns = table_columns(&connection, table);
        for (name, storage_type) in columns {
            if name.contains("_at") {
                assert!(
                    name.ends_with("_at_ms"),
                    "offending timestamp column {table}.{name}: machine timestamp columns must use an `_ms` suffix"
                );
            }
            if name.ends_with("_ms") {
                assert_eq!(
                    storage_type.to_ascii_uppercase(),
                    "INTEGER",
                    "offending timestamp column {table}.{name}: machine timestamps must use SQLite INTEGER storage"
                );
            }
        }
    }
}

#[test]
fn real_catalog_constraints_reject_invalid_identity_values() {
    let temp = TempDir::new().expect("temporary catalog directory");
    let connection = create_real_catalog(&temp);
    let valid_hash = "a".repeat(64);

    assert_constraint(
        connection.execute(
            "INSERT INTO blobs(hash, size_bytes, created_at_ms) VALUES (?1, 1, 1)",
            ["too-short"],
        ),
        "blobs.hash must reject non-64-character BLAKE3 hashes",
    );

    connection
        .execute(
            "INSERT INTO blobs(hash, size_bytes, created_at_ms) VALUES (?1, 1, 1)",
            [&valid_hash],
        )
        .expect("insert prerequisite blob");

    for (source_root, relative_path, expectation) in [
        (
            "",
            "valid.txt",
            "source_files.source_root must reject empty values",
        ),
        (
            "/source",
            "",
            "source_files.relative_path must reject empty values",
        ),
        (
            "/source",
            "/absolute.txt",
            "source_files.relative_path must reject absolute paths",
        ),
    ] {
        assert_constraint(
            insert_source(&connection, source_root, relative_path, &valid_hash),
            expectation,
        );
    }

    insert_source(&connection, "/source", "duplicate.txt", &valid_hash)
        .expect("insert prerequisite source identity");
    assert_constraint(
        insert_source(&connection, "/source", "duplicate.txt", &valid_hash),
        "source_files must reject duplicate (source_root, relative_path) identity",
    );
}

#[test]
fn failed_known_source_resurrection_rolls_back_observation_update() {
    let temp = TempDir::new().expect("temporary catalog directory");
    let path = temp.path().join("catalog.sqlite");
    let writer = CatalogWriterHandle::spawn(path.clone(), CatalogWriterConfig::default())
        .expect("initialize catalog");
    let hash = BlobHash::new("a".repeat(64)).expect("valid hash");
    let relative_path = SourceRelativePath::from_catalog_text("photo.jpg").expect("relative path");
    let initial = SourceObservation {
        source_root: "/source".into(),
        relative_path: relative_path.clone(),
        blob_hash: hash.clone(),
        size_bytes: 5,
        modified_at_ms: Some(10),
        observed_at_ms: 100,
    };
    writer
        .record_imported_file(
            0,
            BlobRecord {
                hash: hash.clone(),
                size_bytes: 5,
                created_at_ms: 100,
            },
            initial,
        )
        .expect("queue source")
        .resolve()
        .expect("record source");

    let failed = writer
        .observe_unchanged_source(
            1,
            SourceObservation {
                source_root: "/source".into(),
                relative_path,
                blob_hash: hash,
                size_bytes: 5,
                modified_at_ms: Some(20),
                observed_at_ms: 200,
            },
            6,
        )
        .and_then(|ticket| ticket.resolve());
    assert!(
        failed.is_err(),
        "wrong expected size must reject resurrection"
    );
    let _ = writer.finish();

    let connection = Connection::open(path).expect("inspect catalog");
    let state: (Option<i64>, i64, i64) = connection
        .query_row(
            "SELECT modified_at_ms, last_seen_at_ms, seen_count
             FROM source_files WHERE source_root = '/source' AND relative_path = 'photo.jpg'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read unchanged observation");
    assert_eq!(state, (Some(10), 100, 1));
}

#[test]
fn writer_events_describe_exact_batch_and_record_sequences() {
    let temp = TempDir::new().expect("temporary catalog directory");
    let path = temp.path().join("catalog.sqlite");
    let writer = CatalogWriterHandle::spawn(
        path,
        CatalogWriterConfig {
            channel_capacity: NonZeroUsize::new(8).expect("non-zero"),
            max_batch_records: NonZeroUsize::new(2).expect("non-zero"),
            max_batch_latency: Duration::from_millis(10),
            checkpoint_every_batches: NonZeroUsize::new(1).expect("non-zero"),
        },
    )
    .expect("start writer");
    let events = writer.events();
    let first = writer
        .record_imported_file(0, record('a', "one.jpg", 1), observation('a', "one.jpg", 1))
        .expect("queue first");
    let second = writer
        .record_imported_file(1, record('b', "two.jpg", 2), observation('b', "two.jpg", 2))
        .expect("queue second");
    first.resolve().expect("commit first");
    second.resolve().expect("commit second");
    let final_ticket = writer
        .record_imported_file(
            2,
            record('c', "three.jpg", 3),
            observation('c', "three.jpg", 3),
        )
        .expect("queue final");
    writer.finish().expect("flush final batch");
    final_ticket.resolve().expect("commit final");

    let events: Vec<_> = events.try_iter().collect();
    assert!(events.contains(&CatalogWriterEvent::BatchOpened {
        size: 2,
        commit_sequence: 1
    }));
    assert!(events.contains(&CatalogWriterEvent::BatchOpened {
        size: 1,
        commit_sequence: 2
    }));
    assert!(events.contains(&CatalogWriterEvent::ShutdownFlush { size: 1 }));
    for sequence in 0..3 {
        assert!(events.iter().any(|event| matches!(event, CatalogWriterEvent::RecordCommitted { sequence: observed, .. } if *observed == sequence)));
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, CatalogWriterEvent::CheckpointCompleted { .. }))
            .count(),
        3
    );
}

#[test]
fn writer_disables_automatic_checkpoints_in_its_startup_pragmas() {
    let temp = TempDir::new().expect("temporary catalog directory");
    let writer = CatalogWriterHandle::spawn(
        temp.path().join("catalog.sqlite"),
        CatalogWriterConfig::default(),
    )
    .expect("start writer with managed checkpoint policy");
    let events = writer.events();
    writer.finish().expect("finish writer");
    assert!(
        events.try_iter().any(|event| matches!(
            event,
            CatalogWriterEvent::Ready {
                wal_autocheckpoint: 0
            }
        )),
        "writer readiness must report the live connection's disabled automatic checkpoint value"
    );
}

#[test]
fn production_catalog_opens_writable_connections_only_during_writer_startup() {
    let source = include_str!("../src/catalog.rs");
    let (_, after_test_support) = source
        .split_once("#[cfg(test)]\npub(crate) mod test_support {")
        .expect("locate explicitly test-only catalog fixtures");
    let (_, production) = after_test_support
        .split_once("\n#[derive(Clone, Debug)]\npub struct BlobRecord")
        .expect("locate production catalog writer implementation");
    assert_eq!(
        production.matches("Connection::open(").count(),
        1,
        "only writer startup may use Connection::open in production catalog code"
    );
    assert!(production.contains("fn open_writable_catalog"));
    let (_, gc_startup) = production
        .split_once("fn open_existing_gc_catalog")
        .expect("locate GC writer startup");
    let (gc_startup, _) = gc_startup
        .split_once("fn writer_loop")
        .expect("bound GC writer startup");
    assert!(
        gc_startup.contains("OpenFlags::SQLITE_OPEN_READ_WRITE"),
        "the sole production read-write flag must remain within GC writer startup"
    );
}

#[test]
fn held_reader_allows_busy_passive_checkpoint_and_durable_batch() {
    let temp = TempDir::new().expect("temporary catalog directory");
    let path = temp.path().join("catalog.sqlite");
    let writer = CatalogWriterHandle::spawn(
        path.clone(),
        CatalogWriterConfig {
            channel_capacity: NonZeroUsize::new(8).expect("non-zero"),
            max_batch_records: NonZeroUsize::new(1).expect("non-zero"),
            max_batch_latency: Duration::from_millis(10),
            checkpoint_every_batches: NonZeroUsize::new(1).expect("non-zero"),
        },
    )
    .expect("start writer");
    let events = writer.events();
    let reader = Connection::open(&path).expect("open held reader");
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM source_files;")
        .expect("hold a WAL read snapshot");

    let ticket = writer
        .record_imported_file(
            0,
            record('a', "held-reader.jpg", 10),
            observation('a', "held-reader.jpg", 10),
        )
        .expect("queue record");
    assert!(
        ticket.resolve().is_ok(),
        "record must commit despite held reader"
    );
    let report = writer
        .finish()
        .expect("busy passive checkpoint is successful");
    reader.execute_batch("COMMIT").expect("release reader");

    assert_eq!((report.committed_batches, report.committed_records), (1, 1));
    let completed: Vec<_> = events
        .try_iter()
        .filter_map(|event| match event {
            CatalogWriterEvent::CheckpointCompleted {
                busy,
                log,
                checkpointed,
            } => Some((busy, log, checkpointed)),
            _ => None,
        })
        .collect();
    assert!(
        !completed.is_empty(),
        "successful work must emit a passive checkpoint completion event"
    );
    assert!(
        completed
            .iter()
            .all(|(busy, log, checkpointed)| *busy >= 0 && *log >= 0 && *checkpointed >= 0)
    );
    assert!(
        completed
            .iter()
            .any(|(_, log, checkpointed)| log > checkpointed),
        "the held read snapshot must constrain passive checkpoint progress: {completed:?}"
    );
    let connection = Connection::open(path).expect("reopen durable catalog");
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM source_files", [], |row| row
                .get::<_, i64>(0))
            .expect("count source rows"),
        1
    );
}

#[test]
fn dropped_record_response_does_not_crash_writer_or_lose_durable_work() {
    let temp = TempDir::new().expect("temporary catalog directory");
    let path = temp.path().join("catalog.sqlite");
    let writer = CatalogWriterHandle::spawn(
        path.clone(),
        CatalogWriterConfig {
            channel_capacity: NonZeroUsize::new(8).expect("non-zero"),
            max_batch_records: NonZeroUsize::new(1).expect("non-zero"),
            max_batch_latency: Duration::from_millis(10),
            checkpoint_every_batches: NonZeroUsize::new(8).expect("non-zero"),
        },
    )
    .expect("start writer");
    drop(
        writer
            .record_imported_file(
                0,
                record('a', "dropped-response.jpg", 1),
                observation('a', "dropped-response.jpg", 1),
            )
            .expect("queue record whose caller drops its response receiver"),
    );
    let report = writer
        .finish()
        .expect("dropped response receiver must not crash writer");
    assert_eq!((report.committed_batches, report.committed_records), (1, 1));
    let connection = Connection::open(path).expect("reopen durable catalog");
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM source_files", [], |row| row
                .get::<_, i64>(0))
            .expect("count durable source rows"),
        1
    );
}

#[test]
fn writer_commits_time_batch_and_mixed_requests_with_exact_outcomes() {
    let temp = TempDir::new().expect("temporary catalog directory");
    let path = temp.path().join("catalog.sqlite");
    let writer = CatalogWriterHandle::spawn(
        path.clone(),
        CatalogWriterConfig {
            channel_capacity: NonZeroUsize::new(8).expect("non-zero"),
            max_batch_records: NonZeroUsize::new(8).expect("non-zero"),
            max_batch_latency: Duration::from_millis(10),
            checkpoint_every_batches: NonZeroUsize::new(8).expect("non-zero"),
        },
    )
    .expect("start writer");
    let events = writer.events();
    let imported = writer
        .record_imported_file(
            0,
            record('a', "one.jpg", 10),
            observation('a', "one.jpg", 10),
        )
        .expect("queue imported record");
    let known = writer
        .observe_unchanged_source(1, observation('a', "one.jpg", 20), 1)
        .expect("queue known observation before resolving either request");
    assert_eq!(
        imported.resolve().expect("mixed batch commit"),
        media_importer::catalog::ImportWriteOutcome::Imported(
            media_importer::catalog::BlobRecordOutcome::Inserted,
            media_importer::catalog::SourceObservationOutcome::Inserted,
        )
    );
    assert_eq!(
        known.resolve().expect("mixed batch commit"),
        media_importer::catalog::ImportWriteOutcome::ObservedKnown
    );
    let report = writer.finish().expect("finish writer");
    assert_eq!((report.committed_batches, report.committed_records), (1, 2));
    let sequences: Vec<_> = events
        .try_iter()
        .filter_map(|event| match event {
            CatalogWriterEvent::RecordCommitted { sequence, .. } => Some(sequence),
            _ => None,
        })
        .collect();
    assert_eq!(
        sequences,
        vec![0, 1],
        "mixed batch outcomes preserve input sequence"
    );

    let connection = Connection::open(path).expect("reopen catalog");
    let state: (i64, i64, Option<i64>, i64) = connection
        .query_row(
            "SELECT seen_count, last_seen_at_ms, modified_at_ms, size_bytes
             FROM source_files WHERE source_root = '/source' AND relative_path = 'one.jpg'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read committed source");
    assert_eq!(state, (2, 20, None, 1));
}

#[test]
fn multiple_successful_batches_reopen_with_exact_observation_state() {
    let temp = TempDir::new().expect("temporary catalog directory");
    let path = temp.path().join("catalog.sqlite");
    let writer = CatalogWriterHandle::spawn(
        path.clone(),
        CatalogWriterConfig {
            channel_capacity: NonZeroUsize::new(8).expect("non-zero"),
            max_batch_records: NonZeroUsize::new(2).expect("non-zero"),
            max_batch_latency: Duration::from_millis(10),
            checkpoint_every_batches: NonZeroUsize::new(8).expect("non-zero"),
        },
    )
    .expect("start writer");
    let first = writer
        .record_imported_file(
            0,
            record('a', "one.jpg", 10),
            observation('a', "one.jpg", 10),
        )
        .expect("queue first");
    let second = writer
        .record_imported_file(
            1,
            record('b', "two.jpg", 20),
            observation('b', "two.jpg", 20),
        )
        .expect("queue second");
    assert!(first.resolve().is_ok());
    assert!(second.resolve().is_ok());
    let observed = writer
        .observe_unchanged_source(2, observation('a', "one.jpg", 30), 1)
        .expect("queue second-batch observation");
    assert_eq!(
        observed.resolve().expect("commit observation"),
        media_importer::catalog::ImportWriteOutcome::ObservedKnown
    );
    let report = writer.finish().expect("finish writer");
    assert_eq!((report.committed_batches, report.committed_records), (2, 3));

    let connection = Connection::open(path).expect("reopen catalog");
    let rows: Vec<_> = connection
        .prepare("SELECT relative_path, blob_hash, size_bytes, first_seen_at_ms, last_seen_at_ms, seen_count FROM source_files ORDER BY relative_path")
        .expect("prepare observation query")
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?, row.get::<_, i64>(4)?, row.get::<_, i64>(5)?,
            ))
        })
        .expect("query observations")
        .collect::<rusqlite::Result<_>>()
        .expect("read observations");
    assert_eq!(
        rows,
        vec![
            ("one.jpg".into(), "a".repeat(64), 1, 10, 30, 2),
            ("two.jpg".into(), "b".repeat(64), 1, 20, 20, 1),
        ]
    );
}

#[test]
fn failing_batch_rolls_back_every_record_after_earlier_batch_is_durable() {
    let temp = TempDir::new().expect("temporary catalog directory");
    let path = temp.path().join("catalog.sqlite");
    let writer = CatalogWriterHandle::spawn(
        path.clone(),
        CatalogWriterConfig {
            channel_capacity: NonZeroUsize::new(8).expect("non-zero"),
            max_batch_records: NonZeroUsize::new(2).expect("non-zero"),
            max_batch_latency: Duration::from_millis(10),
            checkpoint_every_batches: NonZeroUsize::new(8).expect("non-zero"),
        },
    )
    .expect("start writer");
    writer
        .record_imported_file(
            0,
            record('a', "durable.jpg", 1),
            observation('a', "durable.jpg", 1),
        )
        .expect("queue earlier batch")
        .resolve()
        .expect("first batch must commit after timeout");

    let valid = writer
        .record_imported_file(
            1,
            record('b', "rolled-back.jpg", 2),
            observation('b', "rolled-back.jpg", 2),
        )
        .expect("queue valid record in failing batch");
    let invalid = writer
        .observe_unchanged_source(2, observation('a', "durable.jpg", 3), 2)
        .expect("queue invalid record in failing batch");
    assert!(valid.resolve().is_err(), "valid peer must be rolled back");
    assert!(
        invalid.resolve().is_err(),
        "failing record must report failure"
    );
    assert!(writer.finish().is_err(), "writer retains batch failure");

    let connection = Connection::open(path).expect("reopen catalog");
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM source_files", [], |row| row
                .get::<_, i64>(0))
            .expect("count committed sources"),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT seen_count FROM source_files WHERE relative_path = 'durable.jpg'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("read earlier source"),
        1
    );
}

#[test]
fn contradictory_known_source_and_blob_sizes_roll_back_before_any_sql_mutation() {
    let temp = TempDir::new().expect("temporary catalog directory");
    let path = temp.path().join("catalog.sqlite");

    let seed = CatalogWriterHandle::spawn(path.clone(), CatalogWriterConfig::default())
        .expect("start seed writer");
    seed.record_imported_file(
        0,
        record('a', "contradictory.jpg", 1),
        observation('a', "contradictory.jpg", 1),
    )
    .expect("queue contradictory source")
    .resolve()
    .expect("commit contradictory source");
    seed.finish().expect("finish seed writer");
    Connection::open(&path)
        .expect("open seeded catalog")
        .execute(
            "UPDATE blobs SET size_bytes = 2 WHERE hash = ?1",
            ["a".repeat(64)],
        )
        .expect("create independently matching contradictory blob size");

    let writer = CatalogWriterHandle::spawn(
        path.clone(),
        CatalogWriterConfig {
            channel_capacity: NonZeroUsize::new(8).expect("non-zero"),
            max_batch_records: NonZeroUsize::new(2).expect("non-zero"),
            max_batch_latency: Duration::from_millis(10),
            checkpoint_every_batches: NonZeroUsize::new(8).expect("non-zero"),
        },
    )
    .expect("start writer");
    writer
        .record_imported_file(
            1,
            record('b', "durable.jpg", 2),
            observation('b', "durable.jpg", 2),
        )
        .expect("queue earlier durable batch")
        .resolve()
        .expect("commit earlier durable batch");

    let valid = writer
        .record_imported_file(
            2,
            record('c', "rolled-back.jpg", 3),
            observation('c', "rolled-back.jpg", 3),
        )
        .expect("queue valid peer");
    let contradictory = writer
        .observe_unchanged_source(
            3,
            SourceObservation {
                source_root: "/source".into(),
                relative_path: SourceRelativePath::from_catalog_text("contradictory.jpg")
                    .expect("relative path"),
                blob_hash: BlobHash::new("a".repeat(64)).expect("valid hash"),
                size_bytes: 1,
                modified_at_ms: Some(99),
                observed_at_ms: 99,
            },
            2,
        )
        .expect("queue contradictory known source");
    assert!(valid.resolve().is_err(), "valid peer must roll back");
    assert!(
        contradictory.resolve().is_err(),
        "contradictory request must fail before source SQL"
    );
    assert!(writer.finish().is_err(), "writer retains the failed batch");

    let connection = Connection::open(path).expect("reopen catalog");
    let rows: Vec<_> = connection
        .prepare(
            "SELECT relative_path, blob_hash, size_bytes, modified_at_ms, last_seen_at_ms, seen_count \
             FROM source_files ORDER BY relative_path",
        )
        .expect("prepare source query")
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .expect("query source rows")
        .collect::<rusqlite::Result<_>>()
        .expect("read source rows");
    assert_eq!(
        rows,
        vec![
            ("contradictory.jpg".into(), "a".repeat(64), 1, None, 1, 1),
            ("durable.jpg".into(), "b".repeat(64), 1, None, 2, 1),
        ],
        "the prior batch remains durable, while neither the valid peer nor the contradictory observation persist"
    );
}

#[test]
fn mismatched_imported_record_rolls_back_its_batch_after_earlier_batch_is_durable() {
    let temp = TempDir::new().expect("temporary catalog directory");
    let path = temp.path().join("catalog.sqlite");
    let writer = CatalogWriterHandle::spawn(
        path.clone(),
        CatalogWriterConfig {
            channel_capacity: NonZeroUsize::new(8).expect("non-zero"),
            max_batch_records: NonZeroUsize::new(2).expect("non-zero"),
            max_batch_latency: Duration::from_millis(10),
            checkpoint_every_batches: NonZeroUsize::new(8).expect("non-zero"),
        },
    )
    .expect("start writer");
    writer
        .record_imported_file(
            0,
            record('a', "durable.jpg", 1),
            observation('a', "durable.jpg", 1),
        )
        .expect("queue earlier batch")
        .resolve()
        .expect("first batch must commit after timeout");

    let valid = writer
        .record_imported_file(
            1,
            record('b', "rolled-back.jpg", 2),
            observation('b', "rolled-back.jpg", 2),
        )
        .expect("queue valid record in failing batch");
    let mismatched = writer
        .record_imported_file(
            2,
            record('c', "mismatch.jpg", 3),
            observation('d', "mismatch.jpg", 3),
        )
        .expect("queue mismatched imported record");
    assert!(valid.resolve().is_err(), "valid peer must be rolled back");
    assert!(
        mismatched.resolve().is_err(),
        "mismatched imported record must report failure"
    );
    assert!(writer.finish().is_err(), "writer retains batch failure");

    let connection = Connection::open(path).expect("reopen catalog");
    let rows: Vec<_> = connection
        .prepare("SELECT relative_path, blob_hash, size_bytes, seen_count FROM source_files ORDER BY relative_path")
        .expect("prepare durable source query")
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .expect("query durable sources")
        .collect::<rusqlite::Result<_>>()
        .expect("read durable sources");
    assert_eq!(rows, vec![("durable.jpg".into(), "a".repeat(64), 1, 1)]);
}

#[test]
fn malformed_v1_gc_inspection_rolls_back_and_releases_writer_lock() {
    let temp = TempDir::new().expect("temporary catalog directory");
    let path = temp
        .path()
        .canonicalize()
        .expect("canonical temp directory")
        .join("catalog.sqlite");
    CatalogWriterHandle::spawn(path.clone(), CatalogWriterConfig::default())
        .expect("initialize catalog")
        .finish()
        .expect("finish initialization writer");
    let malformed = Connection::open(&path).expect("open catalog for malformed v1 fixture");
    malformed
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; DROP TABLE blobs;")
        .expect("make malformed v1 WAL catalog");
    drop(malformed);

    let writer =
        CatalogWriterHandle::spawn_existing_for_gc(path.clone(), CatalogWriterConfig::default())
            .expect("start GC writer for malformed catalog");
    let error = match writer.begin_gc() {
        Ok(_) => panic!("inspection must fail"),
        Err(error) => error,
    };
    assert!(
        format!("{error:#}").contains("no such table"),
        "inspection error should retain its original SQLite cause: {error:#}"
    );
    assert!(
        writer.finish().is_ok(),
        "writer must shut down after rejected begin"
    );

    let connection = Connection::open(path).expect("reopen after failed inspection");
    connection
        .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
        .expect("failed inspection must not leave a transaction lock behind");
}

fn record(hash: char, _relative_path: &str, observed_at_ms: i64) -> BlobRecord {
    BlobRecord {
        hash: BlobHash::new(hash.to_string().repeat(64)).expect("valid hash"),
        size_bytes: 1,
        created_at_ms: observed_at_ms,
    }
}

fn observation(hash: char, relative_path: &str, observed_at_ms: i64) -> SourceObservation {
    SourceObservation {
        source_root: "/source".into(),
        relative_path: SourceRelativePath::from_catalog_text(relative_path).expect("relative path"),
        blob_hash: BlobHash::new(hash.to_string().repeat(64)).expect("valid hash"),
        size_bytes: 1,
        modified_at_ms: None,
        observed_at_ms,
    }
}

fn table_columns(connection: &Connection, table: &str) -> Vec<(String, String)> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table inspection");
    statement
        .query_map([], |row| Ok((row.get(1)?, row.get(2)?)))
        .expect("query table columns")
        .collect::<Result<Vec<_>, _>>()
        .expect("read table columns")
}

fn insert_source(
    connection: &Connection,
    source_root: &str,
    relative_path: &str,
    hash: &str,
) -> rusqlite::Result<usize> {
    connection.execute(
        "INSERT INTO source_files(
            source_root, relative_path, blob_hash, size_bytes,
            modified_at_ms, first_seen_at_ms, last_seen_at_ms
         ) VALUES (?1, ?2, ?3, 1, NULL, 1, 1)",
        params![source_root, relative_path, hash],
    )
}

fn assert_constraint(result: rusqlite::Result<usize>, expectation: &str) {
    match result {
        Err(Error::SqliteFailure(error, _)) if error.code == ErrorCode::ConstraintViolation => {}
        Err(error) => panic!("{expectation}; got non-constraint error category: {error:?}"),
        Ok(_) => panic!("{expectation}; insert unexpectedly succeeded"),
    }
}
