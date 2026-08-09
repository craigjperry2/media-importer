use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail, eyre};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use crate::integrity::{IntegrityFinding as AuditFinding, push_finding};
use crate::paths::{BlobHash, SourceRelativePath};
use rusqlite::types::ValueRef;
use std::collections::{BTreeSet, HashMap};

const CURRENT_SCHEMA_VERSION: i64 = 1;
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
const READ_JOURNAL_MODE_SQL: &str = include_str!("catalog/sql/read_journal_mode.sql");

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

pub struct GcCatalog {
    connection: Connection,
}

pub struct GcTransaction<'connection> {
    transaction: Transaction<'connection>,
}

impl GcCatalog {
    pub fn open_existing_for_gc(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)
            .wrap_err_with(|| format!("stat catalog database for garbage collection {:?}", path))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "catalog database must be an existing regular file, not a symlink: {:?}",
                path
            );
        }

        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .wrap_err_with(|| {
            format!("open existing catalog database for garbage collection {path:?}")
        })?;
        connection
            .busy_timeout(Duration::from_secs(1))
            .wrap_err("set garbage collection SQLite busy timeout")?;

        let version: i64 = connection
            .query_row(READ_USER_VERSION_SQL, [], |row| row.get(0))
            .wrap_err("read catalog schema version for garbage collection")?;
        if version != CURRENT_SCHEMA_VERSION {
            bail!(
                "catalog schema version {version} is unsupported; expected {CURRENT_SCHEMA_VERSION}"
            );
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
            .execute_batch(GC_CONNECTION_PRAGMAS_SQL)
            .wrap_err("enable garbage collection catalog PRAGMAs")?;
        let confirmed_mode: String = connection
            .query_row(CONFIRM_WAL_MODE_SQL, [], |row| row.get(0))
            .wrap_err("confirm WAL journal mode for garbage collection")?;
        if !confirmed_mode.eq_ignore_ascii_case("wal") {
            bail!("catalog did not remain in WAL mode for garbage collection");
        }

        Ok(Self { connection })
    }

    pub fn begin_immediate(&mut self) -> Result<GcTransaction<'_>> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .wrap_err("begin immediate garbage collection transaction")?;
        Ok(GcTransaction { transaction })
    }
}

impl GcTransaction<'_> {
    pub fn inspect_and_snapshot(&self) -> Result<CatalogAuditSnapshot> {
        let version: i64 = self
            .transaction
            .query_row(READ_USER_VERSION_SQL, [], |row| row.get(0))
            .wrap_err("re-read catalog schema version inside garbage collection transaction")?;
        if version != CURRENT_SCHEMA_VERSION {
            bail!("catalog schema version changed to {version}; expected {CURRENT_SCHEMA_VERSION}");
        }
        inspect_catalog_connection(&self.transaction, GC_SNAPSHOT_SQL)
    }

    pub fn stage_mark(&self, blob: &CatalogBlob, marked_at_ms: i64) -> Result<()> {
        let affected = self
            .transaction
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

    pub fn stage_resurrection(&self, blob: &CatalogBlob) -> Result<()> {
        let affected = self
            .transaction
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

    pub fn stage_sweep(&self, blob: &CatalogBlob) -> Result<()> {
        let affected = self
            .transaction
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

    pub fn commit(self) -> Result<()> {
        self.transaction
            .commit()
            .wrap_err("commit garbage collection transaction")
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

pub struct Catalog {
    connection: Connection,
}

pub struct ReadOnlyCatalog {
    connection: SharedReadConnection,
}

impl Catalog {
    pub fn open_or_initialize(path: &Path) -> Result<Self> {
        let mut connection =
            Connection::open(path).wrap_err_with(|| format!("open catalog database {:?}", path))?;
        connection
            .execute_batch(WRITABLE_PRAGMAS_SQL)
            .wrap_err("enable catalog PRAGMAs")?;
        migrate(&mut connection)?;
        Ok(Self { connection })
    }

    pub fn record_imported_file(
        &mut self,
        blob: BlobRecord,
        observation: SourceObservation,
    ) -> Result<(BlobRecordOutcome, SourceObservationOutcome)> {
        let tx = self
            .connection
            .transaction()
            .wrap_err("begin catalog import transaction")?;

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

        tx.execute(
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

        tx.commit().wrap_err("commit catalog import transaction")?;
        Ok((blob_outcome, source_outcome))
    }

    pub fn known_source_file(
        &self,
        source_root: &str,
        relative_path: &SourceRelativePath,
    ) -> Result<Option<KnownSourceFile>> {
        known_source_file(&self.connection, source_root, relative_path)
    }

    /// Record a repeat observation without changing its blob identity.
    ///
    /// Both updates are in one transaction so a deletion-marked blob is
    /// resurrected exactly when its source observation is refreshed.
    pub fn observe_known_source_file(
        &mut self,
        observation: SourceObservation,
        expected_blob_size_bytes: u64,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction()
            .wrap_err("begin known-source observation transaction")?;
        let source_rows = tx
            .execute(
                OBSERVE_KNOWN_SOURCE_FILE_SQL,
                params![
                    observation.source_root.as_str(),
                    observation.relative_path.as_str(),
                    observation.blob_hash.as_str(),
                    sqlite_u64(observation.size_bytes, "source file size")?,
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
                params![
                    observation.blob_hash.as_str(),
                    sqlite_u64(expected_blob_size_bytes, "expected blob size")?,
                ],
            )
            .wrap_err("resurrect known source blob")?;
        if blob_rows != 1 {
            bail!(
                "known-source observation for blob {} expected one matching blob row, affected {blob_rows}",
                observation.blob_hash
            );
        }
        tx.commit()
            .wrap_err("commit known-source observation transaction")
    }
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
