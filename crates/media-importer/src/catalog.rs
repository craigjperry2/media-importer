use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail, eyre};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::paths::{BlobHash, SourceRelativePath};

const CURRENT_SCHEMA_VERSION: i64 = 1;

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
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;",
            )
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
                "INSERT OR IGNORE INTO blobs (hash, size_bytes, created_at_ms, deleted_at_ms)
                 VALUES (?1, ?2, ?3, NULL)",
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
                .query_row(
                    "SELECT size_bytes FROM blobs WHERE hash = ?1",
                    params![blob.hash.as_str()],
                    |row| row.get(0),
                )
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
                "SELECT id
                 FROM source_files
                 WHERE source_root = ?1 AND relative_path = ?2",
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
            "INSERT INTO source_files (
                source_root,
                relative_path,
                blob_hash,
                size_bytes,
                modified_at_ms,
                first_seen_at_ms,
                last_seen_at_ms,
                seen_count
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 1)
             ON CONFLICT(source_root, relative_path) DO UPDATE SET
                blob_hash = excluded.blob_hash,
                size_bytes = excluded.size_bytes,
                modified_at_ms = excluded.modified_at_ms,
                last_seen_at_ms = excluded.last_seen_at_ms,
                seen_count = source_files.seen_count + 1",
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
        let (open_path, temp_dir) = copy_catalog_for_read_only_access(path)?;
        let connection = Connection::open_with_flags(&open_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .wrap_err_with(|| format!("open read-only catalog database {:?}", path))?;
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
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
                "SELECT id
                 FROM source_files
                 WHERE source_root = ?1 AND relative_path = ?2",
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
        .query_row("PRAGMA user_version", [], |row| row.get(0))
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
    tx.execute_batch(
        "CREATE TABLE blobs (
            hash TEXT PRIMARY KEY CHECK(length(hash) = 64),
            size_bytes INTEGER NOT NULL,
            created_at_ms INTEGER NOT NULL,
            deleted_at_ms INTEGER
        );

        CREATE TABLE source_files (
            id INTEGER PRIMARY KEY,
            source_root TEXT NOT NULL CHECK(length(source_root) > 0),
            relative_path TEXT NOT NULL CHECK(
                length(relative_path) > 0
                AND substr(relative_path, 1, 1) != '/'
            ),
            blob_hash TEXT NOT NULL REFERENCES blobs(hash),
            size_bytes INTEGER NOT NULL,
            modified_at_ms INTEGER,
            first_seen_at_ms INTEGER NOT NULL,
            last_seen_at_ms INTEGER NOT NULL,
            seen_count INTEGER NOT NULL DEFAULT 1,
            UNIQUE(source_root, relative_path)
        );

        CREATE INDEX idx_source_files_blob_hash
        ON source_files(blob_hash);

        PRAGMA user_version = 1;",
    )
    .wrap_err("create catalog schema version 1")?;
    tx.commit().wrap_err("commit catalog schema migration")?;
    Ok(())
}

fn copy_catalog_for_read_only_access(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let temp_dir =
        std::env::temp_dir().join(format!("media-importer-dry-run-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&temp_dir)
        .wrap_err_with(|| format!("create dry-run catalog temp directory {:?}", temp_dir))?;

    let file_name = path
        .file_name()
        .ok_or_else(|| eyre!("catalog path must name a file: {:?}", path))?;
    let copied_db = temp_dir.join(file_name);
    fs::copy(path, &copied_db).wrap_err_with(|| {
        format!(
            "copy catalog database {:?} to dry-run temp path {:?}",
            path, copied_db
        )
    })?;

    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(path, suffix);
        if sidecar.exists() {
            let copied_sidecar = sidecar_path(&copied_db, suffix);
            fs::copy(&sidecar, &copied_sidecar).wrap_err_with(|| {
                format!(
                    "copy catalog sidecar {:?} to dry-run temp path {:?}",
                    sidecar, copied_sidecar
                )
            })?;
        }
    }

    Ok((copied_db, temp_dir))
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}
