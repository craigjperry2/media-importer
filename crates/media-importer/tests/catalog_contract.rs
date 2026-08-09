use assert_fs::TempDir;
use media_importer::catalog::{BlobRecord, Catalog, SourceObservation};
use media_importer::paths::{BlobHash, SourceRelativePath};
use rusqlite::{Connection, Error, ErrorCode, params};

fn create_real_catalog(temp: &TempDir) -> Connection {
    let path = temp.path().join("catalog.sqlite");
    let catalog = Catalog::open_or_initialize(&path).expect("initialize catalog through its API");
    drop(catalog);
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
    let mut catalog = Catalog::open_or_initialize(&path).expect("initialize catalog");
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
    catalog
        .record_imported_file(
            BlobRecord {
                hash: hash.clone(),
                size_bytes: 5,
                created_at_ms: 100,
            },
            initial,
        )
        .expect("record source");

    let failed = catalog.observe_known_source_file(
        SourceObservation {
            source_root: "/source".into(),
            relative_path,
            blob_hash: hash,
            size_bytes: 5,
            modified_at_ms: Some(20),
            observed_at_ms: 200,
        },
        6,
    );
    assert!(
        failed.is_err(),
        "wrong expected size must reject resurrection"
    );
    drop(catalog);

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
