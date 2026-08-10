use std::fs;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail, eyre};
use crossbeam_channel::{Receiver, Sender};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::integrity::{IntegrityFinding as AuditFinding, push_finding};
use crate::paths::{BlobHash, SourceRelativePath};
use crate::test_probe;
use rusqlite::types::ValueRef;
use std::collections::{BTreeSet, HashMap};

const CURRENT_SCHEMA_VERSION: i64 = 1;
// Telemetry is advisory. Keep it bounded so a caller that does not subscribe
// cannot make a long import retain one event per record indefinitely.
const EVENT_CHANNEL_CAPACITY: usize = 256;
const AUDIT_BEGIN_SQL: &str = include_str!("catalog/sql/audit_begin.sql");
const AUDIT_BLOBS_SQL: &str = include_str!("catalog/sql/audit_blobs.sql");
const AUDIT_COMMIT_SQL: &str = include_str!("catalog/sql/audit_commit.sql");
const AUDIT_FOREIGN_KEY_CHECK_SQL: &str = include_str!("catalog/sql/audit_foreign_key_check.sql");
const AUDIT_INTEGRITY_CHECK_SQL: &str = include_str!("catalog/sql/audit_integrity_check.sql");
const AUDIT_SCHEMA_OBJECTS_SQL: &str = include_str!("catalog/sql/audit_schema_objects.sql");
const AUDIT_TABLE_INFO_SQL: &str = include_str!("catalog/sql/audit_table_info.sql");
const AUDIT_FOREIGN_KEYS_SQL: &str = include_str!("catalog/sql/audit_foreign_keys.sql");
const AUDIT_INDEXES_SQL: &str = include_str!("catalog/sql/audit_indexes.sql");
const AUDIT_INDEX_COLUMNS_SQL: &str = include_str!("catalog/sql/audit_index_columns.sql");
const AUDIT_SOURCE_FILES_SQL: &str = include_str!("catalog/sql/audit_source_files.sql");
const GC_CONNECTION_PRAGMAS_SQL: &str = include_str!("catalog/sql/gc_connection_pragmas.sql");
const GC_MARK_SQL: &str = include_str!("catalog/sql/gc_mark.sql");
const GC_RESURRECT_SQL: &str = include_str!("catalog/sql/gc_resurrect.sql");
const GC_SNAPSHOT_SQL: &str = include_str!("catalog/sql/gc_snapshot.sql");
const GC_SWEEP_SQL: &str = include_str!("catalog/sql/gc_sweep.sql");
const GC_BEGIN_SQL: &str = include_str!("catalog/sql/gc_begin.sql");
const GC_COMMIT_SQL: &str = include_str!("catalog/sql/gc_commit.sql");
const GC_ROLLBACK_SQL: &str = include_str!("catalog/sql/gc_rollback.sql");
const WAL_CHECKPOINT_PASSIVE_SQL: &str = include_str!("catalog/sql/wal_checkpoint_passive.sql");
const TEST_INVALID_CHECKPOINT_SQL: &str = include_str!("catalog/sql/test_invalid_checkpoint.sql");
const READ_JOURNAL_MODE_SQL: &str = include_str!("catalog/sql/read_journal_mode.sql");
const READ_WAL_AUTOCHECKPOINT_SQL: &str = include_str!("catalog/sql/read_wal_autocheckpoint.sql");

#[derive(Clone, Debug)]
pub struct CatalogBlob {
    pub hash: BlobHash,
    pub size_bytes: u64,
    pub marked_at_ms: Option<i64>,
    pub referenced: bool,
}

pub struct CatalogAuditSnapshot {
    pub blob_rows_seen: u64,
    pub gc_candidates: u64,
    pub valid_blobs: HashMap<BlobHash, CatalogBlob>,
    pub findings: Vec<AuditFinding>,
}

struct SharedReadConnection {
    connection: Connection,
    temp_dir: Option<PathBuf>,
}

impl Deref for SharedReadConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

impl Drop for SharedReadConnection {
    fn drop(&mut self) {
        if let Some(temp_dir) = self.temp_dir.take() {
            let _ = fs::remove_dir_all(temp_dir);
        }
    }
}

pub fn inspect_catalog_for_audit(path: &Path) -> Result<CatalogAuditSnapshot> {
    let connection = open_existing_read_only(path, "auditing")?;
    require_current_schema(&connection)?;
    connection
        .execute_batch(AUDIT_BEGIN_SQL)
        .wrap_err("begin audit read transaction")?;
    let snapshot = inspect_catalog_connection(&connection, AUDIT_BLOBS_SQL)?;
    connection
        .execute_batch(AUDIT_COMMIT_SQL)
        .wrap_err("finish audit read transaction")?;
    Ok(snapshot)
}

pub fn inspect_catalog_for_gc_dry_run(path: &Path) -> Result<CatalogAuditSnapshot> {
    let connection = open_existing_read_only(path, "garbage collection dry run")?;
    require_current_schema(&connection)?;
    connection
        .execute_batch(AUDIT_BEGIN_SQL)
        .wrap_err("begin GC dry-run read transaction")?;
    let snapshot = inspect_catalog_connection(&connection, GC_SNAPSHOT_SQL)?;
    connection
        .execute_batch(AUDIT_COMMIT_SQL)
        .wrap_err("finish GC dry-run read transaction")?;
    Ok(snapshot)
}

fn open_existing_read_only(path: &Path, operation: &str) -> Result<SharedReadConnection> {
    let (open_path, temp_dir, immutable) = snapshot_catalog_for_shared_read(path, operation)?;
    // `immutable=1` is safe only in the no-WAL branch: audit requires a
    // quiescent catalog, and there are no committed frames outside the main
    // database to ignore. It also prevents SQLite from creating sidecars.
    let uri = sqlite_read_only_uri(&open_path, immutable)?;
    let connection = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .wrap_err_with(|| format!("open read-only catalog database for {operation} {:?}", path))?;
    Ok(SharedReadConnection {
        connection,
        temp_dir: Some(temp_dir),
    })
}

fn require_current_schema(connection: &Connection) -> Result<()> {
    let version: i64 = connection
        .query_row(READ_USER_VERSION_SQL, [], |row| row.get(0))
        .wrap_err("read catalog schema version")?;
    if version != CURRENT_SCHEMA_VERSION {
        bail!("catalog schema version {version} is unsupported; expected {CURRENT_SCHEMA_VERSION}");
    }
    Ok(())
}

fn inspect_catalog_connection(
    connection: &Connection,
    blob_query: &str,
) -> Result<CatalogAuditSnapshot> {
    let mut findings = Vec::new();
    {
        let mut stmt = connection
            .prepare(AUDIT_INTEGRITY_CHECK_SQL)
            .wrap_err("prepare integrity check")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .wrap_err("run integrity check")?;
        for row in rows {
            let message = row.wrap_err("read integrity result")?;
            if message != "ok" {
                push_finding(
                    &mut findings,
                    AuditFinding::new(
                        "CATALOG_INTEGRITY",
                        "integrity_check",
                        format!("reason=integrity-check detail={}", bounded(&message)),
                    ),
                )?;
            }
        }
    }
    {
        let mut stmt = connection
            .prepare(AUDIT_FOREIGN_KEY_CHECK_SQL)
            .wrap_err("prepare foreign key check")?;
        let mut rows = stmt.query([]).wrap_err("run foreign key check")?;
        while let Some(row) = rows.next().wrap_err("read foreign key result")? {
            let table: String = row.get(0)?;
            let rowid: Option<i64> = row.get(1)?;
            let parent: String = row.get(2)?;
            let fk: i64 = row.get(3)?;
            push_finding(
                &mut findings,
                AuditFinding::new(
                    "CATALOG_INTEGRITY",
                    format!(
                        "{table}:{}",
                        rowid.map_or_else(|| "null".into(), |v| v.to_string())
                    ),
                    format!("reason=foreign-key parent={parent} fk={fk}"),
                ),
            )?;
        }
    }
    validate_schema(connection, &mut findings)?;
    let mut valid_blobs = HashMap::new();
    let mut blob_rows_seen = 0_u64;
    let mut gc_candidates = 0_u64;
    {
        let mut stmt = connection
            .prepare(blob_query)
            .wrap_err("prepare blob audit query")?;
        let mut rows = stmt.query([]).wrap_err("query blobs for audit")?;
        while let Some(row) = rows.next().wrap_err("enumerate blob rows")? {
            blob_rows_seen = blob_rows_seen
                .checked_add(1)
                .ok_or_else(|| eyre!("blob row counter overflow"))?;
            let rowid = row.get::<_, i64>(0).unwrap_or(-1);
            let mut reasons = Vec::new();
            let hash = match row.get_ref(1)? {
                ValueRef::Text(v) => std::str::from_utf8(v)
                    .ok()
                    .and_then(|s| BlobHash::new(s.to_owned()).ok()),
                _ => None,
            };
            if hash.is_none() {
                reasons.push("invalid-hash");
            }
            let size = match row.get_ref(2)? {
                ValueRef::Integer(v) if v >= 0 => Some(v as u64),
                _ => None,
            };
            if size.is_none() {
                reasons.push("invalid-size");
            }
            if !matches!(row.get_ref(3)?, ValueRef::Integer(_)) {
                reasons.push("invalid-created-at");
            }
            let marked_at_ms = match row.get_ref(4)? {
                ValueRef::Null => None,
                ValueRef::Integer(value) => {
                    gc_candidates = gc_candidates
                        .checked_add(1)
                        .ok_or_else(|| eyre!("GC counter overflow"))?;
                    Some(value)
                }
                _ => {
                    reasons.push("invalid-deleted-at");
                    None
                }
            };
            let referenced = match row.get_ref(5)? {
                ValueRef::Integer(0) => Some(false),
                ValueRef::Integer(1) => Some(true),
                _ => {
                    reasons.push("invalid-reachability");
                    None
                }
            };
            if reasons.is_empty() {
                let hash = hash.expect("validated");
                valid_blobs
                    .try_reserve(1)
                    .map_err(|error| eyre!("reserve catalog blob index: {error}"))?;
                valid_blobs.insert(
                    hash.clone(),
                    CatalogBlob {
                        hash,
                        size_bytes: size.expect("validated"),
                        marked_at_ms,
                        referenced: referenced.expect("validated"),
                    },
                );
            } else {
                push_finding(
                    &mut findings,
                    AuditFinding::new(
                        "INVALID_BLOB_ROW",
                        rowid.to_string(),
                        format!("reason={}", reasons.join(",")),
                    ),
                )?;
            }
        }
    }
    {
        let mut stmt = connection
            .prepare(AUDIT_SOURCE_FILES_SQL)
            .wrap_err("prepare source audit query")?;
        let mut rows = stmt.query([]).wrap_err("query sources for audit")?;
        while let Some(row) = rows.next().wrap_err("enumerate source rows")? {
            let id = row.get::<_, i64>(0).unwrap_or(-1);
            let mut reasons = Vec::new();
            if !matches!(row.get_ref(1)?, ValueRef::Text(v) if !v.is_empty()) {
                reasons.push("invalid-source-root");
            }
            let relative_ok = matches!(row.get_ref(2)?, ValueRef::Text(v) if std::str::from_utf8(v).ok().is_some_and(|s| SourceRelativePath::from_catalog_text(s).is_ok()));
            if !relative_ok {
                reasons.push("invalid-relative-path");
            }
            let blob = match row.get_ref(3)? {
                ValueRef::Text(v) => std::str::from_utf8(v)
                    .ok()
                    .and_then(|s| BlobHash::new(s.to_owned()).ok()),
                _ => None,
            };
            if blob.is_none() {
                reasons.push("invalid-blob-hash");
            }
            let size = match row.get_ref(4)? {
                ValueRef::Integer(v) if v >= 0 => Some(v as u64),
                _ => None,
            };
            if size.is_none() {
                reasons.push("invalid-size");
            }
            if !matches!(row.get_ref(5)?, ValueRef::Null | ValueRef::Integer(_)) {
                reasons.push("invalid-modified-at");
            }
            let first = match row.get_ref(6)? {
                ValueRef::Integer(v) => Some(v),
                _ => None,
            };
            let last = match row.get_ref(7)? {
                ValueRef::Integer(v) => Some(v),
                _ => None,
            };
            if first.is_none() {
                reasons.push("invalid-first-seen");
            }
            if last.is_none() {
                reasons.push("invalid-last-seen");
            }
            if matches!((first,last),(Some(a),Some(b)) if a>b) {
                reasons.push("reversed-seen-times");
            }
            if !matches!(row.get_ref(8)?, ValueRef::Integer(v) if v > 0) {
                reasons.push("invalid-seen-count");
            }
            if let (Some(hash), Some(source_size)) = (&blob, size) {
                match valid_blobs.get(hash) {
                    Some(b) if b.size_bytes != source_size => {
                        reasons.push("blob-size-disagreement")
                    }
                    None => reasons.push("missing-valid-blob"),
                    _ => {}
                }
            }
            if !reasons.is_empty() {
                push_finding(
                    &mut findings,
                    AuditFinding::new(
                        "INVALID_SOURCE_ROW",
                        id.to_string(),
                        format!("reason={}", reasons.join(",")),
                    ),
                )?;
            }
        }
    }
    Ok(CatalogAuditSnapshot {
        blob_rows_seen,
        gc_candidates,
        valid_blobs,
        findings,
    })
}

fn bounded(value: &str) -> String {
    value.chars().take(256).collect()
}

fn sidecar_metadata(path: &Path, label: &str) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!(
                "catalog {label} sidecar must be a real regular file: {:?}",
                path
            )
        }
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).wrap_err_with(|| format!("stat catalog {label} sidecar {:?}", path))
        }
    }
}

/// Make a private SQLite snapshot for a same-store-lock-protected shared read.
///
/// The caller's shared store lock prevents cooperating commands from changing
/// the source while the main database and any committed WAL are copied. SHM is
/// deliberately never copied: it is SQLite coordination state, not durable
/// database content, and SQLite may create it only beside this temporary copy.
/// Direct external writers remain unsupported by this narrow snapshot policy.
fn snapshot_catalog_for_shared_read(
    path: &Path,
    operation: &str,
) -> Result<(PathBuf, PathBuf, bool)> {
    if !path.exists() {
        bail!("catalog database does not exist: {:?}", path);
    }
    let metadata =
        fs::symlink_metadata(path).wrap_err_with(|| format!("stat catalog database {:?}", path))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("catalog database must be a real regular file: {:?}", path);
    }

    let wal = sidecar_path(path, "-wal");
    let wal_metadata = sidecar_metadata(&wal, "WAL")?;
    let nonempty_wal = wal_metadata.is_some_and(|metadata| metadata.len() > 0);
    copy_catalog_snapshot(path, nonempty_wal, operation)
}

fn copy_catalog_snapshot(
    path: &Path,
    copy_wal: bool,
    operation: &str,
) -> Result<(PathBuf, PathBuf, bool)> {
    let temp_dir = std::env::temp_dir().join(format!(
        "media-importer-shared-read-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&temp_dir)
        .wrap_err_with(|| format!("create shared-read catalog temp directory {:?}", temp_dir))?;
    let result = (|| {
        let file_name = path
            .file_name()
            .ok_or_else(|| eyre!("catalog path must name a file: {:?}", path))?;
        let copied_db = temp_dir.join(file_name);
        fs::copy(path, &copied_db)
            .wrap_err_with(|| format!("copy catalog database {:?} to {:?}", path, copied_db))?;
        if copy_wal {
            let source = sidecar_path(path, "-wal");
            let destination = sidecar_path(&copied_db, "-wal");
            fs::copy(&source, &destination).wrap_err_with(|| {
                format!(
                    "copy catalog WAL for {operation} from {:?} to {:?}",
                    source, destination
                )
            })?;
        }
        Ok((copied_db, temp_dir.clone(), !copy_wal))
    })();
    match result {
        Ok(snapshot) => Ok(snapshot),
        Err(error) => {
            let _ = fs::remove_dir_all(&temp_dir);
            Err(error)
        }
    }
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn sqlite_read_only_uri(path: &Path, immutable: bool) -> Result<String> {
    let absolute = path
        .canonicalize()
        .wrap_err_with(|| format!("canonicalize catalog path {:?}", path))?;
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        absolute.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = absolute.to_string_lossy().into_owned().into_bytes();

    let mut uri = String::from("file:");
    uri.try_reserve(bytes.len().saturating_mul(3).saturating_add(32))
        .map_err(|error| eyre!("reserve SQLite URI: {error}"))?;
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(uri, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    uri.push_str("?mode=ro");
    if immutable {
        uri.push_str("&immutable=1");
    }
    Ok(uri)
}

#[derive(Debug, Eq, PartialEq)]
struct ColumnShape {
    name: String,
    declared_type: String,
    not_null: bool,
    primary_key: i64,
}

fn validate_schema(connection: &Connection, findings: &mut Vec<AuditFinding>) -> Result<()> {
    let expected_objects = BTreeSet::from([
        ("index".to_owned(), "idx_source_files_blob_hash".to_owned()),
        ("table".to_owned(), "blobs".to_owned()),
        ("table".to_owned(), "source_files".to_owned()),
    ]);
    let mut actual_objects = Vec::new();
    let mut table_sql = HashMap::new();
    {
        let mut statement = connection.prepare(AUDIT_SCHEMA_OBJECTS_SQL)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let kind: String = row.get(0)?;
            let name: String = row.get(1)?;
            let sql: Option<String> = row.get(2)?;
            actual_objects
                .try_reserve(1)
                .map_err(|error| eyre!("reserve schema object list: {error}"))?;
            actual_objects.push((kind.clone(), name.clone()));
            if kind == "table" {
                table_sql
                    .try_reserve(1)
                    .map_err(|error| eyre!("reserve schema SQL index: {error}"))?;
                table_sql.insert(name, sql.unwrap_or_default());
            }
        }
    }
    actual_objects.sort();
    actual_objects.dedup();
    for object in &expected_objects {
        if !actual_objects.contains(object) {
            schema_finding(findings, &object.1)?;
        }
    }
    for object in &actual_objects {
        if !expected_objects.contains(object) {
            schema_finding(findings, &object.1)?;
        }
    }

    let expected_blobs = [
        ("hash", "TEXT", false, 1),
        ("size_bytes", "INTEGER", true, 0),
        ("created_at_ms", "INTEGER", true, 0),
        ("deleted_at_ms", "INTEGER", false, 0),
    ];
    let expected_sources = [
        ("id", "INTEGER", false, 1),
        ("source_root", "TEXT", true, 0),
        ("relative_path", "TEXT", true, 0),
        ("blob_hash", "TEXT", true, 0),
        ("size_bytes", "INTEGER", true, 0),
        ("modified_at_ms", "INTEGER", false, 0),
        ("first_seen_at_ms", "INTEGER", true, 0),
        ("last_seen_at_ms", "INTEGER", true, 0),
        ("seen_count", "INTEGER", true, 0),
    ];
    validate_columns(connection, "blobs", &expected_blobs, findings)?;
    validate_columns(connection, "source_files", &expected_sources, findings)?;

    let normalized_blobs = normalize_sql(table_sql.get("blobs").map_or("", String::as_str));
    if !normalized_blobs.contains("check(length(hash)=64)") {
        schema_finding(findings, "blobs.check.hash-length")?;
    }
    let normalized_sources =
        normalize_sql(table_sql.get("source_files").map_or("", String::as_str));
    if !normalized_sources.contains("check(length(source_root)>0)")
        || !normalized_sources.contains("length(relative_path)>0")
        || !normalized_sources.contains("substr(relative_path,1,1)!='/")
    {
        schema_finding(findings, "source_files.checks")?;
    }

    let foreign_keys = read_string_rows(connection, AUDIT_FOREIGN_KEYS_SQL, "source_files", 5)?;
    if foreign_keys
        != vec![vec![
            String::from("blobs"),
            String::from("blob_hash"),
            String::from("hash"),
            String::from("NO ACTION"),
            String::from("NO ACTION"),
        ]]
    {
        schema_finding(findings, "source_files.foreign-key")?;
    }
    let index_columns = read_string_rows(
        connection,
        AUDIT_INDEX_COLUMNS_SQL,
        "idx_source_files_blob_hash",
        1,
    )?;
    if index_columns != vec![vec![String::from("blob_hash")]] {
        schema_finding(findings, "idx_source_files_blob_hash.columns")?;
    }
    let indexes = read_string_rows(connection, AUDIT_INDEXES_SQL, "source_files", 3)?;
    let has_required_index = indexes
        .iter()
        .any(|row| row == &["idx_source_files_blob_hash", "0", "c"]);
    let has_unique_identity = indexes.iter().any(|row| {
        row.get(1).is_some_and(|value| value == "1")
            && row.get(2).is_some_and(|value| value == "u")
            && read_string_rows(connection, AUDIT_INDEX_COLUMNS_SQL, &row[0], 1).is_ok_and(
                |columns| {
                    columns
                        == vec![
                            vec![String::from("source_root")],
                            vec![String::from("relative_path")],
                        ]
                },
            )
    });
    if !has_required_index {
        schema_finding(findings, "idx_source_files_blob_hash")?;
    }
    if !has_unique_identity {
        schema_finding(findings, "source_files.unique-identity")?;
    }
    Ok(())
}

fn validate_columns(
    connection: &Connection,
    table: &str,
    expected: &[(&str, &str, bool, i64)],
    findings: &mut Vec<AuditFinding>,
) -> Result<()> {
    let mut statement = connection.prepare(AUDIT_TABLE_INFO_SQL)?;
    let rows = statement.query_map([table], |row| {
        Ok(ColumnShape {
            name: row.get(0)?,
            declared_type: row.get(1)?,
            not_null: row.get::<_, i64>(2)? != 0,
            primary_key: row.get(3)?,
        })
    })?;
    let mut actual = Vec::new();
    actual
        .try_reserve(expected.len())
        .map_err(|error| eyre!("reserve schema columns: {error}"))?;
    for row in rows {
        actual.push(row?);
    }
    let expected: Vec<_> = expected
        .iter()
        .map(|(name, ty, not_null, pk)| ColumnShape {
            name: (*name).into(),
            declared_type: (*ty).into(),
            not_null: *not_null,
            primary_key: *pk,
        })
        .collect();
    if actual != expected {
        schema_finding(findings, &format!("{table}.columns"))?;
    }
    Ok(())
}

fn read_string_rows(
    connection: &Connection,
    sql: &str,
    argument: &str,
    columns: usize,
) -> Result<Vec<Vec<String>>> {
    let mut statement = connection.prepare(sql)?;
    let mut rows = statement.query([argument])?;
    let mut output = Vec::new();
    while let Some(row) = rows.next()? {
        output
            .try_reserve(1)
            .map_err(|error| eyre!("reserve schema result: {error}"))?;
        let mut values = Vec::new();
        values
            .try_reserve_exact(columns)
            .map_err(|error| eyre!("reserve schema row: {error}"))?;
        for index in 0..columns {
            values.push(row.get::<_, String>(index)?);
        }
        output.push(values);
    }
    Ok(output)
}

fn schema_finding(findings: &mut Vec<AuditFinding>, identity: &str) -> Result<()> {
    findings
        .try_reserve(1)
        .map_err(|error| eyre!("reserve schema finding: {error}"))?;
    push_finding(
        findings,
        AuditFinding::new("CATALOG_INTEGRITY", identity, "reason=schema-mismatch"),
    )
}

fn normalize_sql(sql: &str) -> String {
    sql.to_ascii_lowercase()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}
const INSERT_BLOB_SQL: &str = include_str!("catalog/sql/insert_blob.sql");
const READ_USER_VERSION_SQL: &str = include_str!("catalog/sql/read_user_version.sql");
const CONFIRM_WAL_MODE_SQL: &str = include_str!("catalog/sql/confirm_wal_mode.sql");
const RESURRECT_IMPORTED_BLOB_SQL: &str = include_str!("catalog/sql/resurrect_imported_blob.sql");
const SCHEMA_V1_SQL: &str = include_str!("catalog/sql/schema_v1.sql");
const SELECT_BLOB_SIZE_SQL: &str = include_str!("catalog/sql/select_blob_size.sql");
const SELECT_LIVE_MATERIALIZATION_ENTRIES_SQL: &str =
    include_str!("catalog/sql/select_live_materialization_entries.sql");
const SELECT_SOURCE_FILE_ID_SQL: &str = include_str!("catalog/sql/select_source_file_id.sql");
const UPSERT_SOURCE_FILE_SQL: &str = include_str!("catalog/sql/upsert_source_file.sql");
const SELECT_KNOWN_SOURCE_FILE_SQL: &str = include_str!("catalog/sql/select_known_source_file.sql");
const OBSERVE_KNOWN_SOURCE_FILE_SQL: &str =
    include_str!("catalog/sql/observe_known_source_file.sql");
const RESURRECT_KNOWN_SOURCE_BLOB_SQL: &str =
    include_str!("catalog/sql/resurrect_known_source_blob.sql");
const WRITABLE_PRAGMAS_SQL: &str = include_str!("catalog/sql/writable_pragmas.sql");

pub trait Clock {
    fn now_ms(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis().try_into().unwrap_or(i64::MAX))
            .unwrap_or(0)
    }
}

fn require_one_gc_row(operation: &str, blob: &CatalogBlob, affected: usize) -> Result<()> {
    if affected != 1 {
        bail!(
            "garbage collection {operation} for {} expected one matching row, affected {affected}",
            blob.hash
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::{BlobHash, Connection, Path, Result};

    const DELETE_SOURCES_SQL: &str = include_str!("catalog/sql/test_delete_sources.sql");
    const SET_MARK_SQL: &str = include_str!("catalog/sql/test_set_mark.sql");
    const READ_MARKS_SQL: &str = include_str!("catalog/sql/test_read_marks.sql");
    const BLOB_COUNT_SQL: &str = include_str!("catalog/sql/test_blob_count.sql");
    const READ_MARK_SQL: &str = include_str!("catalog/sql/test_read_mark.sql");

    pub(crate) fn delete_sources(path: &Path, hash: Option<&BlobHash>) -> Result<()> {
        Connection::open(path)?.execute(
            DELETE_SOURCES_SQL,
            [hash.map(crate::paths::BlobHash::as_str)],
        )?;
        Ok(())
    }

    pub(crate) fn set_mark(path: &Path, hash: &BlobHash, value: i64) -> Result<()> {
        Connection::open(path)?.execute(SET_MARK_SQL, (hash.as_str(), value))?;
        Ok(())
    }

    pub(crate) fn read_marks(path: &Path) -> Result<Vec<i64>> {
        let connection = Connection::open(path)?;
        let mut statement = connection.prepare(READ_MARKS_SQL)?;
        let values = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(values)
    }

    pub(crate) fn blob_count(path: &Path) -> Result<i64> {
        Ok(Connection::open(path)?.query_row(BLOB_COUNT_SQL, [], |row| row.get(0))?)
    }

    pub(crate) fn read_mark(path: &Path, hash: &BlobHash) -> Result<Option<i64>> {
        Ok(Connection::open(path)?.query_row(READ_MARK_SQL, [hash.as_str()], |row| row.get(0))?)
    }
}

#[derive(Clone, Debug)]
pub struct BlobRecord {
    pub hash: BlobHash,
    pub size_bytes: u64,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug)]
pub struct SourceObservation {
    pub source_root: String,
    pub relative_path: SourceRelativePath,
    pub blob_hash: BlobHash,
    pub size_bytes: u64,
    pub modified_at_ms: Option<i64>,
    pub observed_at_ms: i64,
}

/// A previously recorded source identity and the blob it references.
#[derive(Clone, Debug)]
pub struct KnownSourceFile {
    pub blob_hash: BlobHash,
    pub source_size_bytes: u64,
    pub modified_at_ms: Option<i64>,
    pub blob_size_bytes: u64,
    pub blob_deleted_at_ms: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct LiveMaterializationEntry {
    pub relative_path: SourceRelativePath,
    pub blob_hash: BlobHash,
    pub blob_size_bytes: u64,
}

// BlobRecord     | SourceObservation | Scenario Description
// ---------------|-------------------|----------------------
// Inserted       | Inserted          | New file ingested
// AlreadyPresent | Inserted          | File already present, de-dup in action!
// AlreadyPresent | Updated           | Already seen, update observed_at/mtime
// Inserted       | Updated           | WARNING! Clashing source path with different file content

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlobRecordOutcome {
    Inserted,
    AlreadyPresent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceObservationOutcome {
    Inserted,
    Updated,
}

#[derive(Clone)]
pub struct CatalogWriterConfig {
    pub channel_capacity: NonZeroUsize,
    pub max_batch_records: NonZeroUsize,
    pub max_batch_latency: Duration,
    pub checkpoint_every_batches: NonZeroUsize,
    #[cfg(test)]
    batch_deadline_hook: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    batch_receipt_hook: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for CatalogWriterConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CatalogWriterConfig")
            .field("channel_capacity", &self.channel_capacity)
            .field("max_batch_records", &self.max_batch_records)
            .field("max_batch_latency", &self.max_batch_latency)
            .field("checkpoint_every_batches", &self.checkpoint_every_batches)
            .finish_non_exhaustive()
    }
}

impl Default for CatalogWriterConfig {
    fn default() -> Self {
        Self {
            channel_capacity: NonZeroUsize::new(64).expect("non-zero"),
            max_batch_records: NonZeroUsize::new(32).expect("non-zero"),
            max_batch_latency: Duration::from_millis(25),
            checkpoint_every_batches: NonZeroUsize::new(8).expect("non-zero"),
            #[cfg(test)]
            batch_deadline_hook: None,
            #[cfg(test)]
            batch_receipt_hook: None,
        }
    }
}

#[cfg(test)]
impl CatalogWriterConfig {
    fn with_batch_deadline_hook(mut self, hook: std::sync::Arc<dyn Fn() + Send + Sync>) -> Self {
        self.batch_deadline_hook = Some(hook);
        self
    }

    fn with_batch_receipt_hook(mut self, hook: std::sync::Arc<dyn Fn() + Send + Sync>) -> Self {
        self.batch_receipt_hook = Some(hook);
        self
    }
}

pub struct CatalogWriterHandle {
    request_tx: Sender<CatalogWriteRequest>,
    join_handle: Option<thread::JoinHandle<Result<CatalogWriterReport>>>,
    path: PathBuf,
    command: &'static str,
    events: Receiver<CatalogWriterEvent>,
}

#[derive(Clone, Debug, Default)]
pub struct CatalogWriterReport {
    pub committed_batches: u64,
    pub committed_records: u64,
    pub committed_gc_sessions: u64,
    pub checkpoints: u64,
}

pub struct ImportWriteTicket {
    response: Receiver<Result<ImportWriteOutcome>>,
    path: PathBuf,
    command: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportWriteOutcome {
    Imported(BlobRecordOutcome, SourceObservationOutcome),
    ObservedKnown,
}

/// Behavior-level writer telemetry without leaking database or channel internals.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogWriterEvent {
    Started,
    Ready {
        wal_autocheckpoint: i64,
    },
    Failed {
        context: String,
    },
    Stopped,
    BatchOpened {
        size: usize,
        commit_sequence: u64,
    },
    BatchCommitted {
        size: usize,
        commit_sequence: u64,
        elapsed_ms: u128,
    },
    BatchRolledBack {
        size: usize,
        sequence: u64,
    },
    RecordCommitted {
        sequence: u64,
        outcome: ImportWriteOutcome,
    },
    CheckpointRequested {
        commit_sequence: u64,
        shutdown: bool,
    },
    CheckpointCompleted {
        busy: i64,
        log: i64,
        checkpointed: i64,
    },
    GcBegun,
    GcCommitted,
    GcPartiallyCommitted,
    GcRolledBack,
    ShutdownFlush {
        size: usize,
    },
}

pub struct GcWriteSession<'a> {
    writer: &'a CatalogWriterHandle,
}

enum CatalogWriteRequest {
    Imported {
        sequence: u64,
        blob: BlobRecord,
        observation: SourceObservation,
        response: Sender<Result<ImportWriteOutcome>>,
    },
    Known {
        sequence: u64,
        observation: SourceObservation,
        expected_blob_size_bytes: u64,
        response: Sender<Result<ImportWriteOutcome>>,
    },
    GcBegin(Sender<Result<CatalogAuditSnapshot>>),
    GcMark {
        blob: CatalogBlob,
        marked_at_ms: i64,
        response: Sender<Result<()>>,
    },
    GcResurrect {
        blob: CatalogBlob,
        response: Sender<Result<()>>,
    },
    GcSweep {
        blob: CatalogBlob,
        response: Sender<Result<()>>,
    },
    GcFinish {
        commit: bool,
        partial: bool,
        response: Sender<Result<()>>,
    },
}

struct PendingImport {
    sequence: u64,
    request: PendingImportRequest,
    response: Sender<Result<ImportWriteOutcome>>,
}

enum PendingImportRequest {
    Imported(BlobRecord, SourceObservation),
    Known(SourceObservation, u64),
}

impl CatalogWriterHandle {
    pub fn spawn(path: PathBuf, config: CatalogWriterConfig) -> Result<Self> {
        Self::spawn_inner(path, config, false)
    }

    pub fn spawn_existing_for_gc(path: PathBuf, config: CatalogWriterConfig) -> Result<Self> {
        Self::spawn_inner(path, config, true)
    }

    fn spawn_inner(path: PathBuf, config: CatalogWriterConfig, gc_only: bool) -> Result<Self> {
        let (request_tx, request_rx) = crossbeam_channel::bounded(config.channel_capacity.get());
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let (event_tx, events) = crossbeam_channel::bounded(EVENT_CHANNEL_CAPACITY);
        let thread_path = path.clone();
        let command = if gc_only { "gc" } else { "import" };
        let join_handle = thread::Builder::new()
            .name("catalog-writer".into())
            .spawn(move || {
                emit(&event_tx, CatalogWriterEvent::Started);
                test_probe::pause_or_panic("catalog-writer-before-ready")?;
                test_probe::pause_or_fail("catalog-writer-before-ready")?;
                let startup = if gc_only {
                    open_existing_gc_catalog(&thread_path)
                } else {
                    open_writable_catalog(&thread_path)
                };
                match startup {
                    Ok(connection) => {
                        let wal_autocheckpoint = writer_wal_autocheckpoint(&connection)?;
                        let _ = ready_tx.send(Ok(()));
                        emit(&event_tx, CatalogWriterEvent::Ready { wal_autocheckpoint });
                        let result = writer_loop(connection, request_rx, config, &event_tx);
                        match &result {
                            Ok(_) => emit(&event_tx, CatalogWriterEvent::Stopped),
                            Err(error) => emit(
                                &event_tx,
                                CatalogWriterEvent::Failed {
                                    context: format!("{error:#}"),
                                },
                            ),
                        }
                        result
                    }
                    Err(error) => {
                        // The joining handle retains the original report. The ready
                        // channel is only a notification and must not stringify it.
                        let _ = ready_tx.send(Err(()));
                        emit(
                            &event_tx,
                            CatalogWriterEvent::Failed {
                                context: format!("{error:#}"),
                            },
                        );
                        Err(error)
                    }
                }
            })
            .wrap_err("spawn catalog writer thread")?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                request_tx,
                join_handle: Some(join_handle),
                path,
                command,
                events,
            }),
            Ok(Err(())) => {
                match join_handle.join() {
                    Ok(Err(startup_error)) => Err(startup_error
                        .wrap_err(format!("start {command} catalog writer for {path:?}"))),
                    Ok(Ok(_)) => Err(eyre!(
                        "{command} catalog writer for {path:?} reported startup failure"
                    )),
                    Err(_) => Err(eyre!(
                        "{command} catalog writer thread panicked during startup for {path:?}"
                    )),
                }
            }
            Err(ready_error) => match join_handle.join() {
                Ok(Err(startup_error)) => Err(startup_error.wrap_err(format!(
                    "{command} catalog writer for {path:?} closed its startup-ready channel: {ready_error}"
                ))),
                Ok(Ok(_)) => Err(eyre!(
                    "{command} catalog writer for {path:?} closed its startup-ready channel before reporting readiness: {ready_error}"
                )),
                Err(_) => Err(eyre!(
                    "{command} catalog writer thread panicked before startup readiness for {path:?}: {ready_error}"
                )),
            },
        }
    }

    pub fn events(&self) -> Receiver<CatalogWriterEvent> {
        self.events.clone()
    }

    pub fn record_imported_file(
        &self,
        sequence: u64,
        blob: BlobRecord,
        observation: SourceObservation,
    ) -> Result<ImportWriteTicket> {
        self.submit(|response| CatalogWriteRequest::Imported {
            sequence,
            blob,
            observation,
            response,
        })
    }

    pub fn observe_unchanged_source(
        &self,
        sequence: u64,
        observation: SourceObservation,
        expected_blob_size_bytes: u64,
    ) -> Result<ImportWriteTicket> {
        self.submit(|response| CatalogWriteRequest::Known {
            sequence,
            observation,
            expected_blob_size_bytes,
            response,
        })
    }

    fn submit(
        &self,
        build: impl FnOnce(Sender<Result<ImportWriteOutcome>>) -> CatalogWriteRequest,
    ) -> Result<ImportWriteTicket> {
        let (response_tx, response) = crossbeam_channel::bounded(1);
        self.request_tx.send(build(response_tx)).wrap_err_with(|| {
            format!(
                "send {} request to catalog writer for {:?}",
                self.command, self.path
            )
        })?;
        Ok(ImportWriteTicket {
            response,
            path: self.path.clone(),
            command: self.command,
        })
    }

    pub fn begin_gc(&self) -> Result<(GcWriteSession<'_>, CatalogAuditSnapshot)> {
        let (response_tx, response) = crossbeam_channel::bounded(1);
        self.request_tx
            .send(CatalogWriteRequest::GcBegin(response_tx))
            .wrap_err_with(|| {
                format!(
                    "send GC begin request to catalog writer for {:?}",
                    self.path
                )
            })?;
        let snapshot = response.recv().wrap_err_with(|| {
            format!(
                "wait for GC begin response from catalog writer for {:?}",
                self.path
            )
        })??;
        Ok((GcWriteSession { writer: self }, snapshot))
    }

    pub fn finish(mut self) -> Result<CatalogWriterReport> {
        let (replacement_tx, _) = crossbeam_channel::bounded(1);
        let request_tx = std::mem::replace(&mut self.request_tx, replacement_tx);
        drop(request_tx);
        let join = self.join_handle.take().expect("writer join handle present");
        join.join()
            .map_err(|_| {
                eyre!(
                    "{} catalog writer thread panicked for {:?}",
                    self.command,
                    self.path
                )
            })?
            .wrap_err_with(|| format!("finish {} catalog writer for {:?}", self.command, self.path))
    }
}

impl Drop for CatalogWriterHandle {
    fn drop(&mut self) {
        // A JoinHandle detaches on drop. Commands call `finish` to propagate
        // errors; this fallback still closes and joins the owned worker when a
        // caller is unwinding so it cannot outlive the command or its lock.
        if let Some(join) = self.join_handle.take() {
            let (replacement_tx, _) = crossbeam_channel::bounded(1);
            let request_tx = std::mem::replace(&mut self.request_tx, replacement_tx);
            drop(request_tx);
            if let Err(error) = join.join() {
                tracing::error!(catalog = ?self.path, ?error, "catalog writer panicked while joining during cleanup");
            }
        }
    }
}

impl ImportWriteTicket {
    pub fn resolve(self) -> Result<ImportWriteOutcome> {
        self.response.recv().wrap_err_with(|| {
            format!(
                "wait for {} catalog writer record outcome for {:?}",
                self.command, self.path
            )
        })?
    }
}

impl GcWriteSession<'_> {
    pub fn stage_mark(&self, blob: &CatalogBlob, marked_at_ms: i64) -> Result<()> {
        self.request(|response| CatalogWriteRequest::GcMark {
            blob: blob.clone(),
            marked_at_ms,
            response,
        })
    }
    pub fn stage_resurrection(&self, blob: &CatalogBlob) -> Result<()> {
        self.request(|response| CatalogWriteRequest::GcResurrect {
            blob: blob.clone(),
            response,
        })
    }
    pub fn stage_sweep(&self, blob: &CatalogBlob) -> Result<()> {
        self.request(|response| CatalogWriteRequest::GcSweep {
            blob: blob.clone(),
            response,
        })
    }
    pub fn commit(self) -> Result<()> {
        self.commit_inner(false)
    }
    pub(crate) fn commit_partial(self) -> Result<()> {
        self.commit_inner(true)
    }
    fn commit_inner(&self, partial: bool) -> Result<()> {
        if let Err(commit_error) = self.finish(true, partial) {
            // SQLite leaves a transaction active after many commit failures.
            // Always drive the protocol to a terminal state before releasing
            // the writer, retaining the commit error as the canonical cause.
            return match self.finish(false, false) {
                Ok(()) => {
                    Err(commit_error.wrap_err("GC commit failed; rolled back active session"))
                }
                Err(rollback_error) => Err(commit_error.wrap_err(format!(
                    "GC commit failed and rollback also failed: {rollback_error:#}"
                ))),
            };
        }
        Ok(())
    }
    pub fn rollback(self) -> Result<()> {
        self.finish(false, false)
    }
    fn finish(&self, commit: bool, partial: bool) -> Result<()> {
        self.request(|response| CatalogWriteRequest::GcFinish {
            commit,
            partial,
            response,
        })
    }
    fn request(&self, build: impl FnOnce(Sender<Result<()>>) -> CatalogWriteRequest) -> Result<()> {
        let (response_tx, response) = crossbeam_channel::bounded(1);
        self.writer
            .request_tx
            .send(build(response_tx))
            .wrap_err_with(|| {
                format!(
                    "send GC request to catalog writer for {:?}",
                    self.writer.path
                )
            })?;
        response.recv().wrap_err_with(|| {
            format!(
                "wait for GC response from catalog writer for {:?}",
                self.writer.path
            )
        })?
    }
}

pub struct ReadOnlyCatalog {
    connection: SharedReadConnection,
}

// Legacy fixture convenience for unit tests. Production code can only mutate
// through `CatalogWriterHandle`.
#[cfg(test)]
pub(crate) struct Catalog {
    writer: Option<CatalogWriterHandle>,
    sequence: u64,
}

#[cfg(test)]
impl Catalog {
    pub(crate) fn open_or_initialize(path: &Path) -> Result<Self> {
        Ok(Self {
            writer: Some(CatalogWriterHandle::spawn(
                path.to_path_buf(),
                CatalogWriterConfig::default(),
            )?),
            sequence: 0,
        })
    }
    pub(crate) fn record_imported_file(
        &mut self,
        blob: BlobRecord,
        observation: SourceObservation,
    ) -> Result<(BlobRecordOutcome, SourceObservationOutcome)> {
        let outcome = self
            .writer
            .as_ref()
            .expect("test catalog writer")
            .record_imported_file(self.sequence, blob, observation)?
            .resolve()?;
        self.sequence += 1;
        match outcome {
            ImportWriteOutcome::Imported(blob, source) => Ok((blob, source)),
            ImportWriteOutcome::ObservedKnown => unreachable!(),
        }
    }
}

#[cfg(test)]
impl Drop for Catalog {
    fn drop(&mut self) {
        if let Some(writer) = self.writer.take() {
            let _ = writer.finish();
        }
    }
}

fn record_imported_file_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    blob: BlobRecord,
    observation: SourceObservation,
) -> Result<(BlobRecordOutcome, SourceObservationOutcome)> {
    validate_imported_record(&blob, &observation)?;
    let inserted_blobs = tx
        .execute(
            INSERT_BLOB_SQL,
            params![
                blob.hash.as_str(),
                sqlite_u64(blob.size_bytes, "blob size")?,
                blob.created_at_ms
            ],
        )
        .wrap_err("upsert blob record")?;
    let blob_outcome = if inserted_blobs == 1 {
        BlobRecordOutcome::Inserted
    } else {
        BlobRecordOutcome::AlreadyPresent
    };
    let expected_size = sqlite_u64(blob.size_bytes, "blob size")?;
    let existing_size: i64 = tx
        .query_row(SELECT_BLOB_SIZE_SQL, params![blob.hash.as_str()], |row| {
            row.get(0)
        })
        .wrap_err("read existing blob record")?;
    if existing_size != expected_size {
        bail!(
            "catalog blob {} exists with size {}, imported size {}",
            blob.hash,
            existing_size,
            blob.size_bytes
        );
    }
    let resurrected = tx
        .execute(
            RESURRECT_IMPORTED_BLOB_SQL,
            params![blob.hash.as_str(), expected_size],
        )
        .wrap_err("clear imported blob deletion mark")?;
    if resurrected != 1 {
        bail!(
            "expected one matching blob row while resurrecting {}, updated {}",
            blob.hash,
            resurrected
        );
    }

    let existing_source: Option<i64> = tx
        .query_row(
            SELECT_SOURCE_FILE_ID_SQL,
            params![
                observation.source_root.as_str(),
                observation.relative_path.as_str()
            ],
            |row| row.get(0),
        )
        .optional()
        .wrap_err("read existing source observation")?;
    let source_outcome = if existing_source.is_some() {
        SourceObservationOutcome::Updated
    } else {
        SourceObservationOutcome::Inserted
    };

    let source_rows = tx
        .execute(
            UPSERT_SOURCE_FILE_SQL,
            params![
                observation.source_root.as_str(),
                observation.relative_path.as_str(),
                observation.blob_hash.as_str(),
                sqlite_u64(observation.size_bytes, "source file size")?,
                observation.modified_at_ms,
                observation.observed_at_ms,
            ],
        )
        .wrap_err("upsert source observation")?;
    if source_rows != 1 {
        bail!(
            "imported source observation for {:?} expected one affected source row, affected {source_rows}",
            observation.relative_path
        );
    }

    Ok((blob_outcome, source_outcome))
}

/// Ensure the two halves of an imported record describe the same durable blob
/// before this record can mutate its transaction.
fn validate_imported_record(blob: &BlobRecord, observation: &SourceObservation) -> Result<()> {
    if blob.hash != observation.blob_hash {
        bail!(
            "imported record for {:?} has blob hash {} but source observation references {}",
            observation.relative_path,
            blob.hash,
            observation.blob_hash
        );
    }
    if blob.size_bytes != observation.size_bytes {
        bail!(
            "imported record for {:?} has blob size {} but source observation has size {}",
            observation.relative_path,
            blob.size_bytes,
            observation.size_bytes
        );
    }
    // Check conversion before any statement as well, so an out-of-range size
    // cannot leave a partially-mutated transaction if this code changes later.
    sqlite_u64(blob.size_bytes, "blob size")?;
    sqlite_u64(observation.size_bytes, "source file size")?;
    Ok(())
}

/// Record a repeat observation without changing its blob identity.
///
/// Both updates are in one transaction so a deletion-marked blob is
/// resurrected exactly when its source observation is refreshed.
fn observe_known_source_file_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    observation: SourceObservation,
    expected_blob_size_bytes: u64,
) -> Result<()> {
    if observation.size_bytes != expected_blob_size_bytes {
        bail!(
            "known-source observation for {:?} has source size {} but expected blob size {}",
            observation.relative_path,
            observation.size_bytes,
            expected_blob_size_bytes
        );
    }
    // Validate both values before issuing SQL. This preserves the atomic batch
    // contract even if a catalog has independently inconsistent source/blob
    // size fields from an older or externally-corrupted database.
    let source_size = sqlite_u64(observation.size_bytes, "source file size")?;
    let expected_blob_size = sqlite_u64(expected_blob_size_bytes, "expected blob size")?;
    let source_rows = tx
        .execute(
            OBSERVE_KNOWN_SOURCE_FILE_SQL,
            params![
                observation.source_root.as_str(),
                observation.relative_path.as_str(),
                observation.blob_hash.as_str(),
                source_size,
                observation.modified_at_ms,
                observation.observed_at_ms,
            ],
        )
        .wrap_err("refresh known source observation")?;
    if source_rows != 1 {
        bail!(
            "known-source observation for {:?} expected one matching source row, affected {source_rows}",
            observation.relative_path
        );
    }
    let blob_rows = tx
        .execute(
            RESURRECT_KNOWN_SOURCE_BLOB_SQL,
            params![observation.blob_hash.as_str(), expected_blob_size,],
        )
        .wrap_err("resurrect known source blob")?;
    if blob_rows != 1 {
        bail!(
            "known-source observation for blob {} expected one matching blob row, affected {blob_rows}",
            observation.blob_hash
        );
    }
    Ok(())
}

fn sqlite_u64(value: u64, label: &str) -> Result<i64> {
    value
        .try_into()
        .map_err(|_| eyre!("{label} exceeds SQLite INTEGER range: {value}"))
}

fn known_source_file(
    connection: &Connection,
    source_root: &str,
    relative_path: &SourceRelativePath,
) -> Result<Option<KnownSourceFile>> {
    type KnownSourceRow = (String, i64, Option<i64>, i64, Option<i64>);
    let row: Option<KnownSourceRow> = connection
        .query_row(
            SELECT_KNOWN_SOURCE_FILE_SQL,
            params![source_root, relative_path.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .wrap_err("read known source file")?;
    row.map(
        |(blob_hash, source_size_bytes, modified_at_ms, blob_size_bytes, blob_deleted_at_ms)| {
            let source_size_bytes: u64 = source_size_bytes
                .try_into()
                .map_err(|_| eyre!("catalog source size is negative for hash {blob_hash}"))?;
            let blob_size_bytes: u64 = blob_size_bytes
                .try_into()
                .map_err(|_| eyre!("catalog blob size is negative for hash {blob_hash}"))?;
            Ok(KnownSourceFile {
                blob_hash: BlobHash::new(blob_hash)?,
                source_size_bytes,
                modified_at_ms,
                blob_size_bytes,
                blob_deleted_at_ms,
            })
        },
    )
    .transpose()
}

impl ReadOnlyCatalog {
    pub fn open_for_materialization(path: &Path) -> Result<Self> {
        let connection = open_existing_read_only(path, "build-tree materialization")?;
        let version: i64 = connection
            .query_row(READ_USER_VERSION_SQL, [], |row| row.get(0))
            .wrap_err("read catalog schema version")?;
        if version == 0 {
            bail!("catalog database is uninitialized: {:?}", path);
        }
        if version > CURRENT_SCHEMA_VERSION {
            bail!(
                "catalog schema version {} is newer than supported version {}",
                version,
                CURRENT_SCHEMA_VERSION
            );
        }
        if version != CURRENT_SCHEMA_VERSION {
            bail!(
                "catalog schema version {} is unsupported; expected {}",
                version,
                CURRENT_SCHEMA_VERSION
            );
        }
        Ok(Self { connection })
    }

    pub fn open_if_exists(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let connection = open_existing_read_only(path, "import dry-run catalog snapshot")?;
        let version: i64 = connection
            .query_row(READ_USER_VERSION_SQL, [], |row| row.get(0))
            .wrap_err("read catalog schema version")?;
        if version > CURRENT_SCHEMA_VERSION {
            bail!(
                "catalog schema version {} is newer than supported version {}",
                version,
                CURRENT_SCHEMA_VERSION
            );
        }
        if version == 0 {
            drop(connection);
            return Ok(None);
        }
        Ok(Some(Self { connection }))
    }

    pub fn source_observation_outcome(
        &self,
        source_root: &str,
        relative_path: &SourceRelativePath,
    ) -> Result<SourceObservationOutcome> {
        let existing: Option<i64> = self
            .connection
            .query_row(
                SELECT_SOURCE_FILE_ID_SQL,
                params![source_root, relative_path.as_str()],
                |row| row.get(0),
            )
            .optional()
            .wrap_err("read source observation for dry-run")?;
        Ok(if existing.is_some() {
            SourceObservationOutcome::Updated
        } else {
            SourceObservationOutcome::Inserted
        })
    }

    pub fn known_source_file(
        &self,
        source_root: &str,
        relative_path: &SourceRelativePath,
    ) -> Result<Option<KnownSourceFile>> {
        known_source_file(&self.connection, source_root, relative_path)
    }

    pub fn live_materialization_entries(&self) -> Result<Vec<LiveMaterializationEntry>> {
        let mut statement = self
            .connection
            .prepare(SELECT_LIVE_MATERIALIZATION_ENTRIES_SQL)
            .wrap_err("prepare live materialization query")?;
        let rows = statement
            .query_map([], |row| {
                let relative_path: String = row.get(0)?;
                let blob_hash: String = row.get(1)?;
                let size_bytes: i64 = row.get(2)?;
                Ok((relative_path, blob_hash, size_bytes))
            })
            .wrap_err("query live materialization entries")?;

        let mut entries = Vec::new();
        for row in rows {
            let (relative_path, blob_hash, size_bytes) =
                row.wrap_err("read live materialization entry")?;
            if size_bytes < 0 {
                bail!("catalog blob size is negative for hash {blob_hash}");
            }
            entries.push(LiveMaterializationEntry {
                relative_path: SourceRelativePath::from_catalog_text(&relative_path)?,
                blob_hash: BlobHash::new(blob_hash)?,
                blob_size_bytes: size_bytes as u64,
            });
        }
        Ok(entries)
    }
}

fn migrate(connection: &mut Connection) -> Result<()> {
    let version: i64 = connection
        .query_row(READ_USER_VERSION_SQL, [], |row| row.get(0))
        .wrap_err("read catalog schema version")?;

    if version > CURRENT_SCHEMA_VERSION {
        bail!(
            "catalog schema version {} is newer than supported version {}",
            version,
            CURRENT_SCHEMA_VERSION
        );
    }
    if version == CURRENT_SCHEMA_VERSION {
        return Ok(());
    }

    let tx = connection
        .transaction()
        .wrap_err("begin catalog schema migration")?;
    tx.execute_batch(SCHEMA_V1_SQL)
        .wrap_err("create catalog schema version 1")?;
    tx.commit().wrap_err("commit catalog schema migration")?;
    Ok(())
}

fn open_writable_catalog(path: &Path) -> Result<Connection> {
    let mut connection = Connection::open(path)
        .wrap_err_with(|| format!("open catalog database in writer thread {path:?}"))?;
    connection
        .execute_batch(WRITABLE_PRAGMAS_SQL)
        .wrap_err("enable catalog writer PRAGMAs")?;
    migrate(&mut connection)?;
    require_current_schema(&connection)?;
    tracing::trace!(catalog = ?path, "catalog writer ready");
    Ok(connection)
}

/// Verify the connection-local automatic-checkpoint policy on the actual
/// connection that will enter the writer loop. Reopening the database would
/// inspect a different connection and cannot establish this invariant.
fn writer_wal_autocheckpoint(connection: &Connection) -> Result<i64> {
    let value: i64 = connection
        .query_row(READ_WAL_AUTOCHECKPOINT_SQL, [], |row| row.get(0))
        .wrap_err("read catalog writer wal_autocheckpoint PRAGMA")?;
    if value != 0 {
        bail!("catalog writer wal_autocheckpoint must be 0 after writer PRAGMAs; found {value}");
    }
    Ok(value)
}

fn open_existing_gc_catalog(path: &Path) -> Result<Connection> {
    let metadata = fs::symlink_metadata(path)
        .wrap_err_with(|| format!("stat catalog database for garbage collection {path:?}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("catalog database must be an existing regular file, not a symlink: {path:?}");
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .wrap_err_with(|| format!("open existing catalog database for garbage collection {path:?}"))?;
    let version: i64 = connection
        .query_row(READ_USER_VERSION_SQL, [], |row| row.get(0))
        .wrap_err("read catalog schema version for garbage collection")?;
    if version != CURRENT_SCHEMA_VERSION {
        bail!("catalog schema version {version} is unsupported; expected {CURRENT_SCHEMA_VERSION}");
    }
    let journal_mode: String = connection
        .query_row(READ_JOURNAL_MODE_SQL, [], |row| row.get(0))
        .wrap_err("read catalog journal mode for garbage collection")?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        bail!(
            "garbage collection requires an existing WAL-mode catalog; found journal_mode={journal_mode}"
        );
    }
    connection
        .busy_timeout(Duration::from_secs(1))
        .wrap_err("set garbage collection SQLite busy timeout")?;
    connection
        .execute_batch(GC_CONNECTION_PRAGMAS_SQL)
        .wrap_err("enable garbage collection catalog PRAGMAs")?;
    connection
        .execute_batch(WRITABLE_PRAGMAS_SQL)
        .wrap_err("enable catalog writer PRAGMAs for garbage collection")?;
    let confirmed_mode: String = connection
        .query_row(CONFIRM_WAL_MODE_SQL, [], |row| row.get(0))
        .wrap_err("confirm WAL journal mode for garbage collection")?;
    if !confirmed_mode.eq_ignore_ascii_case("wal") {
        bail!("catalog did not remain in WAL mode for garbage collection");
    }
    Ok(connection)
}

fn writer_loop(
    mut connection: Connection,
    request_rx: Receiver<CatalogWriteRequest>,
    config: CatalogWriterConfig,
    events: &Sender<CatalogWriterEvent>,
) -> Result<CatalogWriterReport> {
    let result = writer_loop_inner(&mut connection, &request_rx, config, events);
    if result.is_err() {
        // Do not leave producer tickets waiting for a receiver that has exited.
        // The joined writer report remains the canonical failure with its source.
        for request in request_rx.try_iter() {
            reject_terminal_request(request);
        }
    }
    result
}

fn writer_loop_inner(
    connection: &mut Connection,
    request_rx: &Receiver<CatalogWriteRequest>,
    config: CatalogWriterConfig,
    events: &Sender<CatalogWriterEvent>,
) -> Result<CatalogWriterReport> {
    let mut pending = Vec::new();
    let mut first_pending_at: Option<Instant> = None;
    let mut report = CatalogWriterReport::default();
    let mut gc_active = false;
    loop {
        test_probe::pause_or_panic("catalog-writer-before-request")?;
        test_probe::pause_or_fail("catalog-writer-before-request")?;
        if let Some(first) = first_pending_at {
            // Check the deadline before receiving again. `recv_timeout(Duration::ZERO)`
            // may otherwise dequeue a request already queued after the deadline,
            // incorrectly placing it in the expired batch.
            #[cfg(test)]
            if let Some(hook) = &config.batch_deadline_hook {
                hook();
            }
            if first.elapsed() >= config.max_batch_latency {
                flush_imports(connection, &mut pending, &mut report, &config, events)?;
                first_pending_at = None;
                continue;
            }
        }
        let received = if let Some(first) = first_pending_at {
            request_rx.recv_timeout(config.max_batch_latency.saturating_sub(first.elapsed()))
        } else {
            request_rx
                .recv()
                .map_err(|_| crossbeam_channel::RecvTimeoutError::Disconnected)
        };
        match received {
            Ok(
                request
                @ (CatalogWriteRequest::Imported { .. } | CatalogWriteRequest::Known { .. }),
            ) => {
                // A request can be dequeued just as the deadline expires. Check
                // again before assigning it to the open batch, so the deadline is
                // absolute rather than merely the receive timeout's best effort.
                #[cfg(test)]
                if let Some(hook) = &config.batch_receipt_hook {
                    hook();
                }
                if first_pending_at.is_some_and(|first| first.elapsed() >= config.max_batch_latency)
                {
                    flush_imports(connection, &mut pending, &mut report, &config, events)?;
                    first_pending_at = None;
                }
                if gc_active {
                    reject_import(
                        request,
                        "catalog writer rejects import records during an active GC session",
                    );
                    continue;
                }
                pending.push(pending_from(request));
                // A private lifecycle seam for the CLI test that distinguishes
                // an accepted request losing its response channel from a send
                // attempted after the request receiver has already closed.
                test_probe::pause_or_panic("catalog-writer-after-request-accepted")?;
                test_probe::pause_or_fail("catalog-writer-after-request-accepted")?;
                first_pending_at.get_or_insert_with(Instant::now);
                if pending.len() >= config.max_batch_records.get() {
                    flush_imports(connection, &mut pending, &mut report, &config, events)?;
                    first_pending_at = None;
                }
            }
            Ok(request) => {
                if !pending.is_empty() {
                    flush_imports(connection, &mut pending, &mut report, &config, events)?;
                    first_pending_at = None;
                }
                match request {
                    CatalogWriteRequest::GcBegin(response) => {
                        let outcome = if gc_active {
                            Err(eyre!("GC session already active"))
                        } else {
                            let result = (|| {
                                connection
                                    .execute_batch(GC_BEGIN_SQL)
                                    .wrap_err("begin immediate garbage collection transaction")?;
                                // Validate under the write reservation as well as at
                                // startup: an external writer could otherwise change
                                // user_version between open and the authoritative
                                // reachability snapshot.
                                require_current_schema(connection).wrap_err(
                                    "validate catalog schema version inside GC transaction",
                                )?;
                                test_probe::pause_or_fail("catalog-gc-after-begin")?;
                                let snapshot =
                                    inspect_catalog_connection(connection, GC_SNAPSHOT_SQL)?;
                                gc_active = true;
                                emit(events, CatalogWriterEvent::GcBegun);
                                tracing::trace!("catalog writer GC session begun");
                                Ok(snapshot)
                            })();
                            if let Err(error) = result {
                                // Inspection runs inside BEGIN IMMEDIATE. It must not
                                // leave that transaction holding the writer connection
                                // after a malformed catalog or probe failure.
                                return_gc_begin_failure(connection, error)
                            } else {
                                result
                            }
                        };
                        let _ = response.send(outcome);
                    }
                    CatalogWriteRequest::GcMark {
                        blob,
                        marked_at_ms,
                        response,
                    } => {
                        let result = stage_gc_mark(connection, &blob, marked_at_ms, gc_active);
                        let _ = response.send(result);
                    }
                    CatalogWriteRequest::GcResurrect { blob, response } => {
                        let result = stage_gc_resurrect(connection, &blob, gc_active);
                        let _ = response.send(result);
                    }
                    CatalogWriteRequest::GcSweep { blob, response } => {
                        let result = stage_gc_sweep(connection, &blob, gc_active);
                        let _ = response.send(result);
                    }
                    CatalogWriteRequest::GcFinish {
                        commit,
                        partial,
                        response,
                    } => {
                        let result = if !gc_active {
                            Err(eyre!("no active GC session"))
                        } else if commit {
                            connection
                                .execute_batch(GC_COMMIT_SQL)
                                .wrap_err("commit garbage collection transaction")
                        } else {
                            connection
                                .execute_batch(GC_ROLLBACK_SQL)
                                .wrap_err("rollback garbage collection transaction")
                        };
                        if result.is_ok() {
                            gc_active = false;
                            if commit {
                                report.committed_gc_sessions = report
                                    .committed_gc_sessions
                                    .checked_add(1)
                                    .ok_or_else(|| eyre!("catalog GC session counter overflow"))?;
                                emit(
                                    events,
                                    if partial {
                                        CatalogWriterEvent::GcPartiallyCommitted
                                    } else {
                                        CatalogWriterEvent::GcCommitted
                                    },
                                );
                                if report
                                    .committed_gc_sessions
                                    .is_multiple_of(config.checkpoint_every_batches.get() as u64)
                                {
                                    emit(
                                        events,
                                        CatalogWriterEvent::CheckpointRequested {
                                            commit_sequence: report.committed_batches,
                                            shutdown: false,
                                        },
                                    );
                                    checkpoint(connection, &mut report, events)?;
                                }
                            } else {
                                emit(events, CatalogWriterEvent::GcRolledBack);
                            }
                        }
                        let _ = response.send(result);
                    }
                    CatalogWriteRequest::Imported { .. } | CatalogWriteRequest::Known { .. } => {
                        unreachable!()
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                flush_imports(connection, &mut pending, &mut report, &config, events)?;
                first_pending_at = None;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                if gc_active {
                    let _ = connection.execute_batch(GC_ROLLBACK_SQL);
                    bail!("catalog writer shut down with an active GC session");
                }
                if !pending.is_empty() {
                    let size = pending.len();
                    flush_imports(connection, &mut pending, &mut report, &config, events)?;
                    // This terminal event means the durable flush succeeded.
                    emit(events, CatalogWriterEvent::ShutdownFlush { size });
                }
                emit(
                    events,
                    CatalogWriterEvent::CheckpointRequested {
                        commit_sequence: report.committed_batches,
                        shutdown: true,
                    },
                );
                checkpoint(connection, &mut report, events)?;
                tracing::trace!(?report, "catalog writer stopped");
                return Ok(report);
            }
        }
    }
}

fn reject_terminal_request(request: CatalogWriteRequest) {
    match request {
        CatalogWriteRequest::Imported { response, .. }
        | CatalogWriteRequest::Known { response, .. } => {
            let _ = response.send(Err(eyre!(
                "catalog writer terminated before completing request"
            )));
        }
        CatalogWriteRequest::GcBegin(response) => {
            let _ = response.send(Err(eyre!("catalog writer terminated before beginning GC")));
        }
        CatalogWriteRequest::GcMark { response, .. }
        | CatalogWriteRequest::GcResurrect { response, .. }
        | CatalogWriteRequest::GcSweep { response, .. }
        | CatalogWriteRequest::GcFinish { response, .. } => {
            let _ = response.send(Err(eyre!(
                "catalog writer terminated before completing GC request"
            )));
        }
    }
}

fn pending_from(request: CatalogWriteRequest) -> PendingImport {
    match request {
        CatalogWriteRequest::Imported {
            sequence,
            blob,
            observation,
            response,
        } => PendingImport {
            sequence,
            request: PendingImportRequest::Imported(blob, observation),
            response,
        },
        CatalogWriteRequest::Known {
            sequence,
            observation,
            expected_blob_size_bytes,
            response,
        } => PendingImport {
            sequence,
            request: PendingImportRequest::Known(observation, expected_blob_size_bytes),
            response,
        },
        _ => unreachable!(),
    }
}

fn reject_import(request: CatalogWriteRequest, reason: &str) {
    match request {
        CatalogWriteRequest::Imported { response, .. }
        | CatalogWriteRequest::Known { response, .. } => {
            let _ = response.send(Err(eyre!(reason.to_owned())));
        }
        _ => unreachable!(),
    }
}

fn flush_imports(
    connection: &mut Connection,
    pending: &mut Vec<PendingImport>,
    report: &mut CatalogWriterReport,
    config: &CatalogWriterConfig,
    events: &Sender<CatalogWriterEvent>,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let started = Instant::now();
    let batch_size = pending.len();
    let commit_sequence = report.committed_batches + 1;
    emit(
        events,
        CatalogWriterEvent::BatchOpened {
            size: batch_size,
            commit_sequence,
        },
    );
    let tx = match connection.transaction() {
        Ok(tx) => tx,
        Err(error) => {
            emit(
                events,
                CatalogWriterEvent::BatchRolledBack {
                    size: batch_size,
                    sequence: pending[0].sequence,
                },
            );
            return Err(eyre!(error).wrap_err("begin catalog import batch transaction"));
        }
    };
    let mut outcomes = Vec::with_capacity(pending.len());
    for item in pending.iter() {
        let outcome = match &item.request {
            PendingImportRequest::Imported(blob, observation) => {
                record_imported_file_in_transaction(&tx, blob.clone(), observation.clone())
                    .map(|(blob, source)| ImportWriteOutcome::Imported(blob, source))
            }
            PendingImportRequest::Known(observation, size) => {
                observe_known_source_file_in_transaction(&tx, observation.clone(), *size)
                    .map(|()| ImportWriteOutcome::ObservedKnown)
            }
        };
        match outcome {
            Ok(value) => outcomes.push(value),
            Err(error) => {
                let failed_sequence = item.sequence;
                emit(
                    events,
                    CatalogWriterEvent::BatchRolledBack {
                        size: batch_size,
                        sequence: failed_sequence,
                    },
                );
                for pending in pending.drain(..) {
                    let _ = pending
                        .response
                        .send(Err(eyre!("catalog import batch rolled back")));
                }
                return Err(error.wrap_err(format!(
                    "catalog import batch failed at sequence {}",
                    failed_sequence
                )));
            }
        }
    }
    if let Err(error) = tx.commit() {
        emit(
            events,
            CatalogWriterEvent::BatchRolledBack {
                size: batch_size,
                sequence: pending[0].sequence,
            },
        );
        for pending in pending.drain(..) {
            let _ = pending
                .response
                .send(Err(eyre!("catalog import batch rolled back")));
        }
        return Err(eyre!(error).wrap_err("commit catalog import batch"));
    }
    report.committed_batches = report
        .committed_batches
        .checked_add(1)
        .ok_or_else(|| eyre!("catalog batch counter overflow"))?;
    report.committed_records = report
        .committed_records
        .checked_add(outcomes.len() as u64)
        .ok_or_else(|| eyre!("catalog record counter overflow"))?;
    for (item, outcome) in pending.drain(..).zip(outcomes) {
        emit(
            events,
            CatalogWriterEvent::RecordCommitted {
                sequence: item.sequence,
                outcome: outcome.clone(),
            },
        );
        let _ = item.response.send(Ok(outcome));
    }
    tracing::trace!(
        batch_size,
        batches = report.committed_batches,
        elapsed_ms = started.elapsed().as_millis(),
        "catalog writer batch committed"
    );
    emit(
        events,
        CatalogWriterEvent::BatchCommitted {
            size: batch_size,
            commit_sequence,
            elapsed_ms: started.elapsed().as_millis(),
        },
    );
    if report
        .committed_batches
        .is_multiple_of(config.checkpoint_every_batches.get() as u64)
    {
        emit(
            events,
            CatalogWriterEvent::CheckpointRequested {
                commit_sequence,
                shutdown: false,
            },
        );
        checkpoint(connection, report, events)?;
    }
    Ok(())
}

fn checkpoint(
    connection: &Connection,
    report: &mut CatalogWriterReport,
    events: &Sender<CatalogWriterEvent>,
) -> Result<()> {
    test_probe::pause_or_fail("catalog-writer-before-checkpoint")?;
    // Exercise the same rusqlite error path as an operational checkpoint
    // failure without exposing a production test switch. SQLite rejects this
    // invalid checkpoint target before returning any checkpoint result row.
    if test_probe::enabled("catalog-writer-checkpoint-sqlite-error")? {
        connection
            .execute_batch(TEST_INVALID_CHECKPOINT_SQL)
            .wrap_err("run injected SQLite checkpoint error")?;
    }
    let (busy, log, checkpointed): (i64, i64, i64) = connection
        .query_row(WAL_CHECKPOINT_PASSIVE_SQL, [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .wrap_err("run passive WAL checkpoint")?;
    if busy < 0 || log < 0 || checkpointed < 0 {
        bail!(
            "invalid passive checkpoint result busy={busy} log={log} checkpointed={checkpointed}"
        );
    }
    report.checkpoints = report
        .checkpoints
        .checked_add(1)
        .ok_or_else(|| eyre!("catalog checkpoint counter overflow"))?;
    tracing::trace!(
        busy,
        log,
        checkpointed,
        "catalog writer passive checkpoint complete"
    );
    emit(
        events,
        CatalogWriterEvent::CheckpointCompleted {
            busy,
            log,
            checkpointed,
        },
    );
    Ok(())
}

fn return_gc_begin_failure<T>(connection: &Connection, error: color_eyre::Report) -> Result<T> {
    match connection
        .execute_batch(GC_ROLLBACK_SQL)
        .wrap_err("rollback garbage collection transaction after inspection failure")
    {
        Ok(()) => Err(error),
        Err(cleanup_error) => Err(error.wrap_err(format!(
            "GC inspection failed and rollback cleanup also failed: {cleanup_error:#}"
        ))),
    }
}

fn emit(events: &Sender<CatalogWriterEvent>, event: CatalogWriterEvent) {
    if events.try_send(event).is_err() {
        tracing::trace!("catalog writer telemetry receiver is saturated or closed");
    }
}

fn stage_gc_mark(
    connection: &Connection,
    blob: &CatalogBlob,
    marked_at_ms: i64,
    active: bool,
) -> Result<()> {
    if !active {
        bail!("no active GC session");
    }
    let affected = connection
        .execute(
            GC_MARK_SQL,
            params![
                blob.hash.as_str(),
                sqlite_u64(blob.size_bytes, "blob size")?,
                marked_at_ms
            ],
        )
        .wrap_err_with(|| format!("mark unreachable blob {}", blob.hash))?;
    require_one_gc_row("mark", blob, affected)
}
fn stage_gc_resurrect(connection: &Connection, blob: &CatalogBlob, active: bool) -> Result<()> {
    if !active {
        bail!("no active GC session");
    }
    let affected = connection
        .execute(
            GC_RESURRECT_SQL,
            params![
                blob.hash.as_str(),
                sqlite_u64(blob.size_bytes, "blob size")?
            ],
        )
        .wrap_err_with(|| format!("resurrect referenced blob {}", blob.hash))?;
    require_one_gc_row("resurrect", blob, affected)
}
fn stage_gc_sweep(connection: &Connection, blob: &CatalogBlob, active: bool) -> Result<()> {
    if !active {
        bail!("no active GC session");
    }
    let affected = connection
        .execute(
            GC_SWEEP_SQL,
            params![
                blob.hash.as_str(),
                sqlite_u64(blob.size_bytes, "blob size")?
            ],
        )
        .wrap_err_with(|| format!("delete swept blob row {}", blob.hash))?;
    require_one_gc_row("sweep", blob, affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::TempDir;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    #[test]
    fn elapsed_batch_deadline_flushes_before_a_queued_later_request() {
        let temp = TempDir::new().expect("temporary catalog directory");
        let (deadline_entered_tx, deadline_entered_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let pause_once = Arc::new(AtomicBool::new(true));
        let hook = {
            let pause_once = Arc::clone(&pause_once);
            Arc::new(move || {
                if pause_once.swap(false, Ordering::SeqCst) {
                    deadline_entered_tx.send(()).expect("notify deadline entry");
                    release_rx.recv().expect("release deadline check");
                }
            })
        };

        let path = temp.path().join("catalog.sqlite");
        let writer = CatalogWriterHandle::spawn(
            path,
            CatalogWriterConfig {
                channel_capacity: NonZeroUsize::new(4).expect("non-zero"),
                max_batch_records: NonZeroUsize::new(4).expect("non-zero"),
                max_batch_latency: Duration::from_millis(10),
                checkpoint_every_batches: NonZeroUsize::new(8).expect("non-zero"),
                batch_deadline_hook: None,
                batch_receipt_hook: None,
            }
            .with_batch_deadline_hook(hook),
        )
        .expect("start writer");
        let events = writer.events();
        let first = writer
            .record_imported_file(
                0,
                BlobRecord {
                    hash: BlobHash::new("a".repeat(64)).expect("valid hash"),
                    size_bytes: 1,
                    created_at_ms: 1,
                },
                SourceObservation {
                    source_root: "/source".into(),
                    relative_path: SourceRelativePath::from_catalog_text("first.jpg")
                        .expect("relative path"),
                    blob_hash: BlobHash::new("a".repeat(64)).expect("valid hash"),
                    size_bytes: 1,
                    modified_at_ms: None,
                    observed_at_ms: 1,
                },
            )
            .expect("queue first record");
        deadline_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer reaches deadline check");
        thread::sleep(Duration::from_millis(15));
        let second = writer
            .record_imported_file(
                1,
                BlobRecord {
                    hash: BlobHash::new("b".repeat(64)).expect("valid hash"),
                    size_bytes: 2,
                    created_at_ms: 2,
                },
                SourceObservation {
                    source_root: "/source".into(),
                    relative_path: SourceRelativePath::from_catalog_text("second.jpg")
                        .expect("relative path"),
                    blob_hash: BlobHash::new("b".repeat(64)).expect("valid hash"),
                    size_bytes: 2,
                    modified_at_ms: None,
                    observed_at_ms: 2,
                },
            )
            .expect("queue request after deadline");
        release_tx.send(()).expect("release writer");

        first.resolve().expect("first batch commits");
        let report = writer.finish().expect("finish writer");
        second.resolve().expect("second batch commits");

        assert_eq!((report.committed_batches, report.committed_records), (2, 2));
        let batches: Vec<_> = events
            .try_iter()
            .filter_map(|event| match event {
                CatalogWriterEvent::BatchCommitted {
                    size,
                    commit_sequence,
                    ..
                } => Some((size, commit_sequence)),
                _ => None,
            })
            .collect();
        assert_eq!(batches, vec![(1, 1), (1, 2)]);
    }

    #[test]
    fn deadline_expiring_during_receipt_starts_a_new_batch() {
        let temp = TempDir::new().expect("temporary catalog directory");
        let (receipt_entered_tx, receipt_entered_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let receipt_count = Arc::new(AtomicUsize::new(0));
        let hook = {
            let receipt_count = Arc::clone(&receipt_count);
            Arc::new(move || {
                if receipt_count.fetch_add(1, Ordering::SeqCst) == 1 {
                    receipt_entered_tx.send(()).expect("notify second receipt");
                    release_rx.recv().expect("release second receipt");
                }
            })
        };

        let writer = CatalogWriterHandle::spawn(
            temp.path().join("catalog.sqlite"),
            CatalogWriterConfig {
                channel_capacity: NonZeroUsize::new(4).expect("non-zero"),
                max_batch_records: NonZeroUsize::new(4).expect("non-zero"),
                max_batch_latency: Duration::from_millis(10),
                checkpoint_every_batches: NonZeroUsize::new(8).expect("non-zero"),
                batch_deadline_hook: None,
                batch_receipt_hook: None,
            }
            .with_batch_receipt_hook(hook),
        )
        .expect("start writer");
        let events = writer.events();
        let first = writer
            .record_imported_file(
                0,
                BlobRecord {
                    hash: BlobHash::new("a".repeat(64)).expect("valid hash"),
                    size_bytes: 1,
                    created_at_ms: 1,
                },
                SourceObservation {
                    source_root: "/source".into(),
                    relative_path: SourceRelativePath::from_catalog_text("first.jpg")
                        .expect("relative path"),
                    blob_hash: BlobHash::new("a".repeat(64)).expect("valid hash"),
                    size_bytes: 1,
                    modified_at_ms: None,
                    observed_at_ms: 1,
                },
            )
            .expect("queue first record");
        let second = writer
            .record_imported_file(
                1,
                BlobRecord {
                    hash: BlobHash::new("b".repeat(64)).expect("valid hash"),
                    size_bytes: 2,
                    created_at_ms: 2,
                },
                SourceObservation {
                    source_root: "/source".into(),
                    relative_path: SourceRelativePath::from_catalog_text("second.jpg")
                        .expect("relative path"),
                    blob_hash: BlobHash::new("b".repeat(64)).expect("valid hash"),
                    size_bytes: 2,
                    modified_at_ms: None,
                    observed_at_ms: 2,
                },
            )
            .expect("queue second record");
        receipt_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer dequeues second record");
        thread::sleep(Duration::from_millis(15));
        release_tx.send(()).expect("release writer");

        first.resolve().expect("first batch commits");
        let report = writer.finish().expect("finish writer");
        second.resolve().expect("second batch commits");

        assert_eq!((report.committed_batches, report.committed_records), (2, 2));
        let batches: Vec<_> = events
            .try_iter()
            .filter_map(|event| match event {
                CatalogWriterEvent::BatchCommitted {
                    size,
                    commit_sequence,
                    ..
                } => Some((size, commit_sequence)),
                _ => None,
            })
            .collect();
        assert_eq!(batches, vec![(1, 1), (1, 2)]);
    }
}
