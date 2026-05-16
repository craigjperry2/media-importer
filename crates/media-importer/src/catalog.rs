use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail, eyre};
use rusqlite::{Connection, MAIN_DB, OpenFlags, OptionalExtension, params};

use crate::paths::{BlobHash, SourceRelativePath};

const CURRENT_SCHEMA_VERSION: i64 = 1;
const INSERT_BLOB_SQL: &str = include_str!("catalog/sql/insert_blob.sql");
const READ_USER_VERSION_SQL: &str = include_str!("catalog/sql/read_user_version.sql");
const SCHEMA_V1_SQL: &str = include_str!("catalog/sql/schema_v1.sql");
const SELECT_BLOB_SIZE_SQL: &str = include_str!("catalog/sql/select_blob_size.sql");
const SELECT_SOURCE_FILE_ID_SQL: &str = include_str!("catalog/sql/select_source_file_id.sql");
const UPSERT_SOURCE_FILE_SQL: &str = include_str!("catalog/sql/upsert_source_file.sql");
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
    connection: Connection,
    temp_dir: Option<PathBuf>,
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
            let existing_size: i64 = tx
                .query_row(SELECT_BLOB_SIZE_SQL, params![blob.hash.as_str()], |row| {
                    row.get(0)
                })
                .wrap_err("read existing blob record")?;
            if existing_size != sqlite_u64(blob.size_bytes, "blob size")? {
                bail!(
                    "catalog blob {} exists with size {}, imported size {}",
                    blob.hash,
                    existing_size,
                    blob.size_bytes
                );
            }
            BlobRecordOutcome::AlreadyPresent
        };

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
}

fn sqlite_u64(value: u64, label: &str) -> Result<i64> {
    value
        .try_into()
        .map_err(|_| eyre!("{label} exceeds SQLite INTEGER range: {value}"))
}

impl ReadOnlyCatalog {
    pub fn open_if_exists(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let (open_path, temp_dir) = backup_catalog_for_read_only_access(path)?;
        let connection = Connection::open_with_flags(&open_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .wrap_err_with(|| format!("open read-only catalog database {:?}", path))?;
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
            let _ = fs::remove_dir_all(temp_dir);
            return Ok(None);
        }
        Ok(Some(Self {
            connection,
            temp_dir: Some(temp_dir),
        }))
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
}

impl Drop for ReadOnlyCatalog {
    fn drop(&mut self) {
        if let Some(temp_dir) = self.temp_dir.take() {
            let _ = fs::remove_dir_all(temp_dir);
        }
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

fn backup_catalog_for_read_only_access(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let temp_dir =
        std::env::temp_dir().join(format!("media-importer-dry-run-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&temp_dir)
        .wrap_err_with(|| format!("create dry-run catalog temp directory {:?}", temp_dir))?;

    let file_name = path
        .file_name()
        .ok_or_else(|| eyre!("catalog path must name a file: {:?}", path))?;
    let copied_db = temp_dir.join(file_name);

    let source = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(source) => source,
        Err(error) => {
            let _ = fs::remove_dir_all(&temp_dir);
            return Err(error)
                .wrap_err_with(|| format!("open source catalog database for backup {:?}", path));
        }
    };
    if let Err(error) = source.backup(MAIN_DB, &copied_db, None) {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(error).wrap_err_with(|| {
            format!(
                "backup catalog database {:?} to dry-run temp path {:?}",
                path, copied_db
            )
        });
    }

    Ok((copied_db, temp_dir))
}
