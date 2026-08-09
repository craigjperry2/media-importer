use color_eyre::Result;

use crate::catalog::{
    BlobRecord, Catalog, Clock, KnownSourceFile, ReadOnlyCatalog, SourceObservation,
    SourceObservationOutcome, SystemClock,
};
use crate::config::ImportConfig;
use crate::paths::BlobHash;
use crate::run_lock::{LockMode, StoreRunLock};
use crate::scanner::{SourceFileCandidate, scan_source};
use crate::store::{CasMetadataCheck, Store, StoreOutcome};

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
    pub files_skipped: u64,
    pub bytes_skipped: u64,
    pub files_hashed: u64,
    pub bytes_hashed: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HashReason {
    MetadataSkippingDisabled,
    NoCatalogObservation,
    SizeChanged,
    ModifiedTimeUnavailable,
    ModifiedTimeChanged,
    CatalogSourceInconsistency,
    CasEntryMissingOrInvalid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportDisposition {
    Skip {
        blob_hash: BlobHash,
        size_bytes: u64,
        resurrection_required: bool,
    },
    Hash {
        reason: HashReason,
    },
}

/// Classify source metadata against a known catalog observation. This is an
/// optimization only: callers must subsequently validate the CAS file.
pub fn classify_import(
    metadata_skip: bool,
    candidate: &SourceFileCandidate,
    known: Option<&KnownSourceFile>,
) -> ImportDisposition {
    if !metadata_skip {
        return ImportDisposition::Hash {
            reason: HashReason::MetadataSkippingDisabled,
        };
    }
    let Some(known) = known else {
        return ImportDisposition::Hash {
            reason: HashReason::NoCatalogObservation,
        };
    };
    if known.source_size_bytes != candidate.size_bytes {
        return ImportDisposition::Hash {
            reason: HashReason::SizeChanged,
        };
    }
    let (Some(recorded_mtime), Some(observed_mtime)) =
        (known.modified_at_ms, candidate.modified_at_ms)
    else {
        return ImportDisposition::Hash {
            reason: HashReason::ModifiedTimeUnavailable,
        };
    };
    if recorded_mtime != observed_mtime {
        return ImportDisposition::Hash {
            reason: HashReason::ModifiedTimeChanged,
        };
    }
    if known.blob_size_bytes != known.source_size_bytes {
        return ImportDisposition::Hash {
            reason: HashReason::CatalogSourceInconsistency,
        };
    }
    ImportDisposition::Skip {
        blob_hash: known.blob_hash.clone(),
        size_bytes: known.source_size_bytes,
        resurrection_required: known.blob_deleted_at_ms.is_some(),
    }
}

pub fn import_source(config: ImportConfig) -> Result<ImportReport> {
    import_source_with_clock(config, &SystemClock)
}

pub fn import_source_with_clock(config: ImportConfig, clock: &impl Clock) -> Result<ImportReport> {
    if config.dry_run {
        if config.store_root.path().exists() {
            let _lock = StoreRunLock::acquire(&config.store_root, "import", LockMode::Shared)?;
            dry_run_import(config)
        } else {
            tracing::trace!(store = ?config.store_root.path(), command = "import", "skipping store coordination for missing-store dry run");
            dry_run_import(config)
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
        let known = catalog.known_source_file(&source_root_text, &candidate.relative_path)?;
        let disposition = validated_disposition(
            &store,
            classify_import(config.metadata_skip, &candidate, known.as_ref()),
        )?;
        tracing::trace!(path = ?candidate.relative_path, disposition = ?disposition, "import metadata decision");
        match disposition {
            ImportDisposition::Skip {
                blob_hash,
                size_bytes,
                resurrection_required,
            } => {
                tracing::trace!(path = ?candidate.relative_path, bytes = size_bytes, "skipping source content read");
                let now_ms = clock.now_ms();
                catalog.observe_known_source_file(
                    source_observation(&source_root_text, &candidate, blob_hash, now_ms),
                    size_bytes,
                )?;
                report.files_skipped += 1;
                report.bytes_skipped += size_bytes;
                report.blobs_reused += 1;
                report.source_records_updated += 1;
                if resurrection_required {
                    tracing::trace!(path = ?candidate.relative_path, "resurrected metadata-skipped blob");
                }
            }
            ImportDisposition::Hash { reason } => {
                tracing::trace!(path = ?candidate.relative_path, ?reason, "reading source content for import");
                let blob = store.ingest_file(
                    &candidate.absolute_path,
                    candidate.size_bytes,
                    config.chunk_size,
                )?;
                report.files_hashed += 1;
                report.bytes_hashed += blob.size_bytes;
                report_store_outcome(&mut report, &blob.outcome, blob.size_bytes);
                let now_ms = clock.now_ms();
                let (_, source_outcome) = catalog.record_imported_file(
                    BlobRecord {
                        hash: blob.hash.clone(),
                        size_bytes: blob.size_bytes,
                        created_at_ms: now_ms,
                    },
                    source_observation(&source_root_text, &candidate, blob.hash, now_ms),
                )?;
                report_source_outcome(&mut report, &source_outcome);
            }
        }
    }
    Ok(report)
}

fn dry_run_import(config: ImportConfig) -> Result<ImportReport> {
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
        let known = match &read_only_catalog {
            Some(catalog) => {
                catalog.known_source_file(&source_root_text, &candidate.relative_path)?
            }
            None => None,
        };
        let disposition = validated_disposition(
            &store,
            classify_import(config.metadata_skip, &candidate, known.as_ref()),
        )?;
        tracing::trace!(path = ?candidate.relative_path, disposition = ?disposition, dry_run = true, "import metadata decision");
        match disposition {
            ImportDisposition::Skip { size_bytes, .. } => {
                tracing::trace!(path = ?candidate.relative_path, bytes = size_bytes, dry_run = true, "skipping source content read");
                report.files_skipped += 1;
                report.bytes_skipped += size_bytes;
                report.blobs_reused += 1;
                report.source_records_updated += 1;
            }
            ImportDisposition::Hash { reason } => {
                tracing::trace!(path = ?candidate.relative_path, ?reason, dry_run = true, "reading source content for import");
                let blob =
                    store.hash_file_read_only(&candidate.absolute_path, config.chunk_size)?;
                report.files_hashed += 1;
                report.bytes_hashed += blob.size_bytes;
                report_store_outcome(&mut report, &blob.outcome, blob.size_bytes);
                let source_outcome = match &read_only_catalog {
                    Some(catalog) => catalog
                        .source_observation_outcome(&source_root_text, &candidate.relative_path)?,
                    None => SourceObservationOutcome::Inserted,
                };
                report_source_outcome(&mut report, &source_outcome);
            }
        }
    }
    Ok(report)
}

fn validated_disposition(
    store: &Store,
    disposition: ImportDisposition,
) -> Result<ImportDisposition> {
    let ImportDisposition::Skip {
        blob_hash,
        size_bytes,
        ..
    } = &disposition
    else {
        return Ok(disposition);
    };
    match store.check_blob_metadata(blob_hash, *size_bytes)? {
        CasMetadataCheck::Present => Ok(disposition),
        CasMetadataCheck::Missing | CasMetadataCheck::SizeMismatch | CasMetadataCheck::Invalid => {
            Ok(ImportDisposition::Hash {
                reason: HashReason::CasEntryMissingOrInvalid,
            })
        }
    }
}

fn source_observation(
    source_root: &str,
    candidate: &SourceFileCandidate,
    blob_hash: BlobHash,
    observed_at_ms: i64,
) -> SourceObservation {
    SourceObservation {
        source_root: source_root.to_owned(),
        relative_path: candidate.relative_path.clone(),
        blob_hash,
        size_bytes: candidate.size_bytes,
        modified_at_ms: candidate.modified_at_ms,
        observed_at_ms,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{BlobHash, SourceRelativePath};
    use std::path::PathBuf;

    fn candidate(size_bytes: u64, modified_at_ms: Option<i64>) -> SourceFileCandidate {
        SourceFileCandidate {
            absolute_path: PathBuf::from("source-file"),
            relative_path: SourceRelativePath::from_catalog_text("file").unwrap(),
            size_bytes,
            modified_at_ms,
        }
    }
    fn known(size_bytes: u64, modified_at_ms: Option<i64>) -> KnownSourceFile {
        KnownSourceFile {
            blob_hash: BlobHash::new("a".repeat(64)).unwrap(),
            source_size_bytes: size_bytes,
            modified_at_ms,
            blob_size_bytes: size_bytes,
            blob_deleted_at_ms: None,
        }
    }

    #[test]
    fn classifies_only_matching_complete_metadata_as_skip() {
        assert!(matches!(
            classify_import(true, &candidate(2, Some(7)), Some(&known(2, Some(7)))),
            ImportDisposition::Skip { .. }
        ));
        assert_eq!(
            classify_import(false, &candidate(2, Some(7)), Some(&known(2, Some(7)))),
            ImportDisposition::Hash {
                reason: HashReason::MetadataSkippingDisabled
            }
        );
        assert_eq!(
            classify_import(true, &candidate(3, Some(7)), Some(&known(2, Some(7)))),
            ImportDisposition::Hash {
                reason: HashReason::SizeChanged
            }
        );
        assert_eq!(
            classify_import(true, &candidate(2, None), Some(&known(2, Some(7)))),
            ImportDisposition::Hash {
                reason: HashReason::ModifiedTimeUnavailable
            }
        );
        assert_eq!(
            classify_import(true, &candidate(2, Some(8)), Some(&known(2, Some(7)))),
            ImportDisposition::Hash {
                reason: HashReason::ModifiedTimeChanged
            }
        );
    }
}
