use color_eyre::Result;

use crate::catalog::{
    BlobRecord, Catalog, Clock, ReadOnlyCatalog, SourceObservation, SourceObservationOutcome,
    SystemClock,
};
use crate::config::ImportConfig;
use crate::run_lock::{LockMode, StoreRunLock};
use crate::scanner::{SourceFileCandidate, scan_source};
use crate::store::{Store, StoreOutcome};

#[derive(Clone, Debug, Default)]
pub struct ImportReport {
    pub dry_run: bool,
    pub files_seen: u64,
    pub blobs_created: u64,
    pub blobs_reused: u64,
    pub bytes_seen: u64,
    pub bytes_written: u64,
    pub source_records_inserted: u64,
    pub source_records_updated: u64,
}

pub fn import_source(config: ImportConfig) -> Result<ImportReport> {
    import_source_with_clock(config, &SystemClock)
}

pub fn import_source_with_clock(config: ImportConfig, clock: &impl Clock) -> Result<ImportReport> {
    if config.dry_run {
        if config.store_root.path().exists() {
            let _lock = StoreRunLock::acquire(&config.store_root, "import", LockMode::Shared)?;
            dry_run_import(config, clock)
        } else {
            tracing::trace!(store = ?config.store_root.path(), command = "import", "skipping store coordination for missing-store dry run");
            dry_run_import(config, clock)
        }
    } else {
        match std::fs::create_dir(config.store_root.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let _lock = StoreRunLock::acquire(&config.store_root, "import", LockMode::Exclusive)?;
        real_import(config, clock)
    }
}

fn real_import(config: ImportConfig, clock: &impl Clock) -> Result<ImportReport> {
    let store = Store::new(config.store_root.clone());
    store.prepare_for_import()?;
    let mut catalog = Catalog::open_or_initialize(&config.db_path)?;
    let source_root_text = config.source_root.durable_text()?;
    let candidates = scan_source(&config.source_root)?;
    let mut report = ImportReport {
        dry_run: false,
        ..ImportReport::default()
    };

    for candidate in candidates {
        report_observed_file(&mut report, &candidate);
        let blob = store.ingest_file(
            &candidate.absolute_path,
            candidate.size_bytes,
            config.chunk_size,
        )?;
        report_store_outcome(&mut report, &blob.outcome, blob.size_bytes);

        let now_ms = clock.now_ms();
        let (_, source_outcome) = catalog.record_imported_file(
            BlobRecord {
                hash: blob.hash.clone(),
                size_bytes: blob.size_bytes,
                created_at_ms: now_ms,
            },
            SourceObservation {
                source_root: source_root_text.clone(),
                relative_path: candidate.relative_path,
                blob_hash: blob.hash,
                size_bytes: blob.size_bytes,
                modified_at_ms: candidate.modified_at_ms,
                observed_at_ms: now_ms,
            },
        )?;
        report_source_outcome(&mut report, &source_outcome);
    }

    Ok(report)
}

fn dry_run_import(config: ImportConfig, _clock: &impl Clock) -> Result<ImportReport> {
    let store = Store::new(config.store_root);
    let source_root_text = config.source_root.durable_text()?;
    let read_only_catalog = ReadOnlyCatalog::open_if_exists(&config.db_path)?;
    let candidates = scan_source(&config.source_root)?;
    let mut report = ImportReport {
        dry_run: true,
        ..ImportReport::default()
    };

    for candidate in candidates {
        report_observed_file(&mut report, &candidate);
        let blob = store.hash_file_read_only(&candidate.absolute_path, config.chunk_size)?;
        report_store_outcome(&mut report, &blob.outcome, blob.size_bytes);

        let source_outcome = match &read_only_catalog {
            Some(catalog) => {
                catalog.source_observation_outcome(&source_root_text, &candidate.relative_path)?
            }
            None => SourceObservationOutcome::Inserted,
        };
        report_source_outcome(&mut report, &source_outcome);
    }

    Ok(report)
}

fn report_observed_file(report: &mut ImportReport, candidate: &SourceFileCandidate) {
    report.files_seen += 1;
    report.bytes_seen += candidate.size_bytes;
}

fn report_store_outcome(report: &mut ImportReport, outcome: &StoreOutcome, size_bytes: u64) {
    match outcome {
        StoreOutcome::Created => {
            report.blobs_created += 1;
            report.bytes_written += size_bytes;
        }
        StoreOutcome::Reused => report.blobs_reused += 1,
    }
}

fn report_source_outcome(report: &mut ImportReport, outcome: &SourceObservationOutcome) {
    match outcome {
        SourceObservationOutcome::Inserted => report.source_records_inserted += 1,
        SourceObservationOutcome::Updated => report.source_records_updated += 1,
    }
}
