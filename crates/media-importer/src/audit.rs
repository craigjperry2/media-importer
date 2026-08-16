use std::fs;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};

use crate::catalog::inspect_catalog_for_audit;
use crate::config::AuditConfig;
use crate::hashing::hash_open_file_with_chunk_observer;
use crate::integrity::{IntegrityFinding, push_finding, sort_and_deduplicate};
use crate::run_lock::{LockMode, StoreRunLock};
use crate::store::{inspect_cas, open_blob_no_follow};
use crate::telemetry::{NoopTelemetrySink, TelemetryEvent, TelemetrySink};

pub type AuditFinding = IntegrityFinding;

#[derive(Debug)]
pub struct AuditReport {
    pub catalog_blobs: u64,
    pub cas_blob_files: u64,
    pub blobs_hashed: u64,
    pub gc_candidates: u64,
    pub findings: Vec<AuditFinding>,
}

impl AuditReport {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

pub fn audit_store(config: AuditConfig) -> Result<AuditReport> {
    audit_store_with_telemetry(config, Arc::new(NoopTelemetrySink))
}

pub fn audit_store_with_telemetry(
    config: AuditConfig,
    telemetry: Arc<dyn TelemetrySink>,
) -> Result<AuditReport> {
    let _lock = StoreRunLock::acquire(&config.store_root, "audit", LockMode::Shared)?;
    let snapshot = inspect_catalog_for_audit(&config.db_path)?;
    telemetry.emit(
        TelemetryEvent::new("audit", "audit_catalog_scanned")
            .field("catalog_blobs", snapshot.blob_rows_seen)
            .field("gc_candidates", snapshot.gc_candidates),
    );
    let cas = inspect_cas(&config.store_root)?;
    let mut findings = snapshot.findings;
    findings
        .try_reserve(cas.findings.len())
        .map_err(|error| eyre!("reserve audit CAS findings: {error}"))?;
    findings.extend(cas.findings);
    let cas_blob_files =
        u64::try_from(cas.valid_blobs.len()).wrap_err("CAS blob count overflow")?;
    telemetry.emit(
        TelemetryEvent::new("audit", "audit_cas_scanned").field("cas_blob_files", cas_blob_files),
    );

    let mut keys = Vec::new();
    keys.try_reserve(
        snapshot
            .valid_blobs
            .len()
            .saturating_add(cas.valid_blobs.len()),
    )
    .map_err(|error| eyre!("reserve reconciliation keys: {error}"))?;
    keys.extend(snapshot.valid_blobs.keys().cloned());
    keys.extend(cas.valid_blobs.iter().cloned());
    keys.sort();
    keys.dedup();

    let mut blobs_hashed = 0_u64;
    for hash in keys {
        if telemetry.failed() {
            return Err(eyre!("telemetry renderer failed"));
        }
        telemetry
            .emit(TelemetryEvent::new("audit", "blob_discovered").field("hash", hash.to_string()));
        let catalog = snapshot.valid_blobs.get(&hash);
        let path = config.store_root.blob_path(&hash);
        let display = crate::integrity::escape_path(
            path.strip_prefix(config.store_root.path()).unwrap_or(&path),
        );
        if !cas.valid_blobs.contains(&hash) {
            match fs::symlink_metadata(&path) {
                Ok(metadata) if !metadata.is_file() => push_finding(
                    &mut findings,
                    AuditFinding::new(
                        "NON_REGULAR_BLOB",
                        hash.to_string(),
                        format!("path={display}"),
                    ),
                )?,
                Ok(_) => push_finding(
                    &mut findings,
                    AuditFinding::new("MISSING_BLOB", hash.to_string(), format!("path={display}")),
                )?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => push_finding(
                    &mut findings,
                    AuditFinding::new("MISSING_BLOB", hash.to_string(), format!("path={display}")),
                )?,
                Err(_) => push_finding(
                    &mut findings,
                    AuditFinding::new(
                        "BLOB_IO_ERROR",
                        hash.to_string(),
                        format!("path={display} reason=stat-failed"),
                    ),
                )?,
            }
            continue;
        }
        if catalog.is_none() {
            push_finding(
                &mut findings,
                AuditFinding::new("ORPHAN_BLOB", hash.to_string(), format!("path={display}")),
            )?;
        }

        match open_blob_no_follow(&config.store_root, &hash) {
            Ok(mut opened) => match hash_open_file_with_chunk_observer(
                &mut opened.file,
                config.chunk_size,
                |_| {
                    if telemetry.failed() {
                        Err(eyre!("telemetry renderer failed"))
                    } else {
                        Ok(())
                    }
                },
            ) {
                Ok(actual) => {
                    blobs_hashed = blobs_hashed
                        .checked_add(1)
                        .ok_or_else(|| eyre!("hashed blob counter overflow"))?;
                    telemetry.emit(
                        TelemetryEvent::new("audit", "blob_hashed")
                            .field("hash", hash.to_string())
                            .field("bytes_read", actual.size_bytes),
                    );
                    let expected_size = catalog
                        .map(|blob| blob.size_bytes)
                        .unwrap_or(opened.identity.len);
                    if expected_size != actual.size_bytes
                        || opened.identity.len != actual.size_bytes
                    {
                        push_finding(
                            &mut findings,
                            AuditFinding::new(
                                "SIZE_MISMATCH",
                                hash.to_string(),
                                format!(
                                    "expected={} metadata={} actual={}",
                                    expected_size, opened.identity.len, actual.size_bytes
                                ),
                            ),
                        )?;
                    }
                    if actual.hash != hash {
                        push_finding(
                            &mut findings,
                            AuditFinding::new(
                                "HASH_MISMATCH",
                                hash.to_string(),
                                format!("expected={hash} actual={}", actual.hash),
                            ),
                        )?;
                    }
                }
                Err(error) if telemetry.failed() => return Err(error),
                Err(_) => push_finding(
                    &mut findings,
                    AuditFinding::new(
                        "BLOB_IO_ERROR",
                        hash.to_string(),
                        format!("path={display} reason=read-failed"),
                    ),
                )?,
            },
            Err(_) => push_finding(
                &mut findings,
                AuditFinding::new(
                    "BLOB_IO_ERROR",
                    hash.to_string(),
                    format!("path={display} reason=open-failed"),
                ),
            )?,
        }
    }

    sort_and_deduplicate(&mut findings);
    for finding in &findings {
        if telemetry.failed() {
            return Err(eyre!("telemetry renderer failed"));
        }
        telemetry.emit(
            TelemetryEvent::new("audit", "finding")
                .field("category", finding.category)
                .field("identity", finding.identity.clone()),
        );
    }
    Ok(AuditReport {
        catalog_blobs: snapshot.blob_rows_seen,
        cas_blob_files,
        blobs_hashed,
        gc_candidates: snapshot.gc_candidates,
        findings,
    })
}
