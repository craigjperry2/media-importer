//! Mark-and-sweep garbage collection.
//!
//! Cooperating commands hold the store's exclusive run lock for the complete
//! collection operation. SQLite's reservation still protects the catalog
//! transaction; non-cooperating external writers remain outside that scope.

use std::fs;

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};
use tracing::info;

use crate::catalog::{
    CatalogAuditSnapshot, CatalogBlob, Clock, GcCatalog, GcTransaction, SystemClock,
    inspect_catalog_for_gc_dry_run,
};
use crate::config::GcConfig;
use crate::hashing::hash_open_file;
use crate::integrity::{IntegrityFinding, push_finding, sort_and_deduplicate};
use crate::paths::BlobHash;
use crate::run_lock::{LockMode, StoreRunLock};
use crate::store::{
    BlobFileIdentity, inspect_cas, open_blob_no_follow, remove_blob_file,
    revalidate_blob_for_removal, sync_blob_parent, sync_nearest_existing_blob_parent,
};
use crate::test_probe;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum GcActionKind {
    Mark,
    Resurrect,
    Sweep,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SweepSourceState {
    Present,
    AlreadyAbsent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GcAction {
    pub kind: GcActionKind,
    pub hash: BlobHash,
    pub size_bytes: u64,
    pub sweep_source_state: Option<SweepSourceState>,
}

#[derive(Debug)]
pub struct GcReport {
    pub dry_run: bool,
    pub catalog_blobs: u64,
    pub reachable_blobs: u64,
    pub sweep_candidates_hashed: u64,
    pub planned_marks: u64,
    pub planned_resurrections: u64,
    pub planned_sweeps: u64,
    pub completed_marks: u64,
    pub completed_resurrections: u64,
    pub completed_sweeps: u64,
    pub cas_files_removed: u64,
    pub bytes_reclaimable: u64,
    pub bytes_unlinked: u64,
    pub bytes_reclaimed: u64,
    pub actions: Vec<GcAction>,
    pub findings: Vec<IntegrityFinding>,
}

#[derive(Debug)]
pub enum GcOutcome {
    Complete(GcReport),
    Blocked(GcReport),
    Incomplete {
        report: GcReport,
        error: color_eyre::Report,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlannedKind {
    Unchanged,
    Mark,
    Resurrect,
    Sweep,
}

#[derive(Debug)]
struct GcPlan {
    marks: Vec<CatalogBlob>,
    resurrections: Vec<CatalogBlob>,
    sweeps: Vec<CatalogBlob>,
}

#[derive(Debug)]
struct SweepCandidate {
    blob: CatalogBlob,
    source_state: SweepSourceState,
    identity: Option<BlobFileIdentity>,
}

#[derive(Debug)]
struct Preflight {
    plan: GcPlan,
    sweep_candidates: Vec<SweepCandidate>,
    report: GcReport,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct StagedProgress {
    marks: u64,
    resurrections: u64,
    sweeps: u64,
    reclaimed: u64,
}

impl StagedProgress {
    fn checked_mark(self) -> Result<Self> {
        Ok(Self {
            marks: self
                .marks
                .checked_add(1)
                .ok_or_else(|| eyre!("completed mark counter overflow"))?,
            ..self
        })
    }

    fn checked_resurrection(self) -> Result<Self> {
        Ok(Self {
            resurrections: self
                .resurrections
                .checked_add(1)
                .ok_or_else(|| eyre!("completed resurrection counter overflow"))?,
            ..self
        })
    }

    fn checked_sweep(self, size_bytes: u64, source_state: SweepSourceState) -> Result<Self> {
        let reclaimed = if source_state == SweepSourceState::Present {
            self.reclaimed
                .checked_add(size_bytes)
                .ok_or_else(|| eyre!("reclaimed byte count overflow"))?
        } else {
            self.reclaimed
        };
        Ok(Self {
            sweeps: self
                .sweeps
                .checked_add(1)
                .ok_or_else(|| eyre!("completed sweep counter overflow"))?,
            reclaimed,
            ..self
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PhysicalProgress {
    cas_files_removed: u64,
    bytes_unlinked: u64,
}

impl PhysicalProgress {
    fn checked_after_present_unlink(report: &GcReport, size_bytes: u64) -> Result<Self> {
        Ok(Self {
            cas_files_removed: report
                .cas_files_removed
                .checked_add(1)
                .ok_or_else(|| eyre!("removed CAS file counter overflow"))?,
            bytes_unlinked: report
                .bytes_unlinked
                .checked_add(size_bytes)
                .ok_or_else(|| eyre!("unlinked byte count overflow"))?,
        })
    }

    fn assign_to(self, report: &mut GcReport) {
        report.cas_files_removed = self.cas_files_removed;
        report.bytes_unlinked = self.bytes_unlinked;
    }
}

trait GcStoreMutator {
    fn revalidate_present(
        &self,
        config: &GcConfig,
        hash: &BlobHash,
        identity: &BlobFileIdentity,
    ) -> Result<()>;

    fn unlink_present(&self, config: &GcConfig, hash: &BlobHash) -> Result<()>;

    fn sync_present_parent(&self, config: &GcConfig, hash: &BlobHash) -> Result<()>;

    fn sync_absent(&self, config: &GcConfig, hash: &BlobHash) -> Result<()>;
}

struct FilesystemGcMutator;

impl GcStoreMutator for FilesystemGcMutator {
    fn revalidate_present(
        &self,
        config: &GcConfig,
        hash: &BlobHash,
        identity: &BlobFileIdentity,
    ) -> Result<()> {
        revalidate_blob_for_removal(&config.store_root, hash, identity)
    }

    fn unlink_present(&self, config: &GcConfig, hash: &BlobHash) -> Result<()> {
        remove_blob_file(&config.store_root, hash)
    }

    fn sync_present_parent(&self, config: &GcConfig, hash: &BlobHash) -> Result<()> {
        sync_blob_parent(&config.store_root, hash)
    }

    fn sync_absent(&self, config: &GcConfig, hash: &BlobHash) -> Result<()> {
        sync_nearest_existing_blob_parent(&config.store_root, hash)
    }
}

pub fn collect_garbage(config: GcConfig) -> Result<GcOutcome> {
    collect_garbage_with_dependencies(config, &SystemClock, &FilesystemGcMutator)
}

#[cfg(test)]
fn collect_garbage_with_clock(config: GcConfig, clock: &dyn Clock) -> Result<GcOutcome> {
    collect_garbage_with_dependencies(config, clock, &FilesystemGcMutator)
}

fn collect_garbage_with_dependencies(
    config: GcConfig,
    clock: &dyn Clock,
    mutator: &dyn GcStoreMutator,
) -> Result<GcOutcome> {
    info!(
        store = ?config.store_root.path(),
        catalog = ?config.db_path,
        dry_run = config.dry_run,
        "starting garbage collection; coordinating automatically with cooperating commands"
    );
    let mode = if config.dry_run {
        LockMode::Shared
    } else {
        LockMode::Exclusive
    };
    let _lock = StoreRunLock::acquire(&config.store_root, "gc", mode)?;
    let outcome = if config.dry_run {
        collect_dry_run(config)
    } else {
        collect_real(config, clock, mutator)
    }?;
    test_probe::pause("gc-outcome-constructed")?;
    Ok(outcome)
}

fn collect_dry_run(config: GcConfig) -> Result<GcOutcome> {
    let snapshot = inspect_catalog_for_gc_dry_run(&config.db_path)?;
    let mut preflight = run_preflight(&config, snapshot)?;
    if !preflight.report.findings.is_empty() {
        return Ok(GcOutcome::Blocked(preflight.report));
    }

    let action_count = preflight
        .plan
        .marks
        .len()
        .checked_add(preflight.plan.resurrections.len())
        .and_then(|count| count.checked_add(preflight.sweep_candidates.len()))
        .ok_or_else(|| eyre!("GC action count overflow"))?;
    preflight
        .report
        .actions
        .try_reserve_exact(action_count)
        .map_err(|error| eyre!("reserve GC dry-run actions: {error}"))?;
    for blob in preflight.plan.marks {
        preflight
            .report
            .actions
            .push(action_for(GcActionKind::Mark, &blob, None));
    }
    for blob in preflight.plan.resurrections {
        preflight
            .report
            .actions
            .push(action_for(GcActionKind::Resurrect, &blob, None));
    }
    for candidate in preflight.sweep_candidates {
        preflight.report.actions.push(action_for(
            GcActionKind::Sweep,
            &candidate.blob,
            Some(candidate.source_state),
        ));
    }
    Ok(GcOutcome::Complete(preflight.report))
}

fn collect_real(
    config: GcConfig,
    clock: &dyn Clock,
    mutator: &dyn GcStoreMutator,
) -> Result<GcOutcome> {
    let mut catalog = GcCatalog::open_existing_for_gc(&config.db_path)?;
    let transaction = catalog.begin_immediate()?;
    let snapshot = transaction.inspect_and_snapshot()?;
    let preflight = run_preflight(&config, snapshot)?;
    test_probe::pause("gc-preflight-complete")?;
    if !preflight.report.findings.is_empty() {
        return Ok(GcOutcome::Blocked(preflight.report));
    }
    apply_plan(config, transaction, preflight, clock, mutator)
}

fn run_preflight(config: &GcConfig, snapshot: CatalogAuditSnapshot) -> Result<Preflight> {
    let (plan, reachable_blobs) = build_plan(&snapshot)?;
    let mut report = GcReport {
        dry_run: config.dry_run,
        catalog_blobs: snapshot.blob_rows_seen,
        reachable_blobs,
        sweep_candidates_hashed: 0,
        planned_marks: count(&plan.marks, "planned marks")?,
        planned_resurrections: count(&plan.resurrections, "planned resurrections")?,
        planned_sweeps: count(&plan.sweeps, "planned sweeps")?,
        completed_marks: 0,
        completed_resurrections: 0,
        completed_sweeps: 0,
        cas_files_removed: 0,
        bytes_reclaimable: 0,
        bytes_unlinked: 0,
        bytes_reclaimed: 0,
        actions: Vec::new(),
        findings: snapshot.findings,
    };

    let cas = inspect_cas(&config.store_root)?;
    report
        .findings
        .try_reserve(cas.findings.len())
        .map_err(|error| eyre!("reserve GC CAS findings: {error}"))?;
    report.findings.extend(cas.findings);

    if cas.complete {
        for hash in &cas.valid_blobs {
            if !snapshot.valid_blobs.contains_key(hash) {
                let path = config.store_root.blob_path(hash);
                push_finding(
                    &mut report.findings,
                    IntegrityFinding::new(
                        "ORPHAN_BLOB",
                        hash.to_string(),
                        format!("path={}", display_path(config, &path)),
                    ),
                )?;
            }
        }
    }

    for blob in snapshot.valid_blobs.values() {
        if cas.valid_blobs.contains(&blob.hash) {
            let path = config.store_root.blob_path(&blob.hash);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => {
                    if metadata.len() != blob.size_bytes {
                        push_finding(
                            &mut report.findings,
                            IntegrityFinding::new(
                                "SIZE_MISMATCH",
                                blob.hash.to_string(),
                                format!("expected={} metadata={}", blob.size_bytes, metadata.len()),
                            ),
                        )?;
                    }
                }
                Ok(_) => push_finding(
                    &mut report.findings,
                    IntegrityFinding::new(
                        "NON_REGULAR_BLOB",
                        blob.hash.to_string(),
                        format!("path={}", display_path(config, &path)),
                    ),
                )?,
                Err(_) => push_finding(
                    &mut report.findings,
                    IntegrityFinding::new(
                        "BLOB_IO_ERROR",
                        blob.hash.to_string(),
                        format!("path={} reason=stat-failed", display_path(config, &path)),
                    ),
                )?,
            }
        } else if cas.complete {
            classify_missing_blob(config, blob, &mut report.findings)?;
        }
    }

    let mut sweep_candidates = Vec::new();
    sweep_candidates
        .try_reserve_exact(plan.sweeps.len())
        .map_err(|error| eyre!("reserve GC sweep candidates: {error}"))?;
    for blob in &plan.sweeps {
        if cas.valid_blobs.contains(&blob.hash) {
            let path = config.store_root.blob_path(&blob.hash);
            let display = display_path(config, &path);
            let mut identity = None;
            match open_blob_no_follow(&config.store_root, &blob.hash) {
                Ok(mut opened) => match hash_open_file(&mut opened.file, config.chunk_size) {
                    Ok(actual) => {
                        report.sweep_candidates_hashed = report
                            .sweep_candidates_hashed
                            .checked_add(1)
                            .ok_or_else(|| eyre!("sweep candidate hash counter overflow"))?;
                        let size_matches = opened.identity.len == blob.size_bytes
                            && actual.size_bytes == blob.size_bytes
                            && actual.size_bytes == opened.identity.len;
                        if !size_matches {
                            push_finding(
                                &mut report.findings,
                                IntegrityFinding::new(
                                    "SIZE_MISMATCH",
                                    blob.hash.to_string(),
                                    format!(
                                        "expected={} metadata={} actual={}",
                                        blob.size_bytes, opened.identity.len, actual.size_bytes
                                    ),
                                ),
                            )?;
                        }
                        let hash_matches = actual.hash == blob.hash;
                        if !hash_matches {
                            push_finding(
                                &mut report.findings,
                                IntegrityFinding::new(
                                    "HASH_MISMATCH",
                                    blob.hash.to_string(),
                                    format!("expected={} actual={}", blob.hash, actual.hash),
                                ),
                            )?;
                        }
                        if size_matches && hash_matches {
                            report.bytes_reclaimable = report
                                .bytes_reclaimable
                                .checked_add(blob.size_bytes)
                                .ok_or_else(|| eyre!("reclaimable byte count overflow"))?;
                            identity = Some(opened.identity);
                        }
                    }
                    Err(_) => push_finding(
                        &mut report.findings,
                        IntegrityFinding::new(
                            "BLOB_IO_ERROR",
                            blob.hash.to_string(),
                            format!("path={display} reason=read-failed"),
                        ),
                    )?,
                },
                Err(_) => push_finding(
                    &mut report.findings,
                    IntegrityFinding::new(
                        "BLOB_IO_ERROR",
                        blob.hash.to_string(),
                        format!("path={display} reason=open-failed"),
                    ),
                )?,
            }
            sweep_candidates.push(SweepCandidate {
                blob: blob.clone(),
                source_state: SweepSourceState::Present,
                identity,
            });
        } else if cas.complete && canonical_path_is_absent(config, &blob.hash) {
            sweep_candidates.push(SweepCandidate {
                blob: blob.clone(),
                source_state: SweepSourceState::AlreadyAbsent,
                identity: None,
            });
        }
    }

    sort_and_deduplicate(&mut report.findings);
    Ok(Preflight {
        plan,
        sweep_candidates,
        report,
    })
}

fn build_plan(snapshot: &CatalogAuditSnapshot) -> Result<(GcPlan, u64)> {
    let mut blobs: Vec<_> = snapshot.valid_blobs.values().cloned().collect();
    blobs.sort_by(|left, right| left.hash.cmp(&right.hash));

    let mut plan = GcPlan {
        marks: Vec::new(),
        resurrections: Vec::new(),
        sweeps: Vec::new(),
    };
    plan.marks
        .try_reserve(blobs.len())
        .map_err(|error| eyre!("reserve GC mark plan: {error}"))?;
    plan.resurrections
        .try_reserve(blobs.len())
        .map_err(|error| eyre!("reserve GC resurrection plan: {error}"))?;
    plan.sweeps
        .try_reserve(blobs.len())
        .map_err(|error| eyre!("reserve GC sweep plan: {error}"))?;
    let mut reachable = 0_u64;
    for blob in blobs {
        if blob.referenced {
            reachable = reachable
                .checked_add(1)
                .ok_or_else(|| eyre!("reachable blob counter overflow"))?;
        }
        match classify(blob.referenced, blob.marked_at_ms.is_some()) {
            PlannedKind::Unchanged => {}
            PlannedKind::Mark => plan.marks.push(blob),
            PlannedKind::Resurrect => plan.resurrections.push(blob),
            PlannedKind::Sweep => plan.sweeps.push(blob),
        }
    }
    Ok((plan, reachable))
}

fn apply_plan(
    config: GcConfig,
    transaction: GcTransaction<'_>,
    mut preflight: Preflight,
    clock: &dyn Clock,
    mutator: &dyn GcStoreMutator,
) -> Result<GcOutcome> {
    let total_actions = preflight
        .plan
        .marks
        .len()
        .checked_add(preflight.plan.resurrections.len())
        .and_then(|count| count.checked_add(preflight.sweep_candidates.len()))
        .ok_or_else(|| eyre!("GC action count overflow"))?;
    let mut staged_actions = Vec::new();
    staged_actions
        .try_reserve_exact(total_actions)
        .map_err(|error| eyre!("reserve staged GC actions: {error}"))?;
    preflight
        .report
        .actions
        .try_reserve_exact(total_actions)
        .map_err(|error| eyre!("reserve completed GC actions: {error}"))?;

    let mut progress = StagedProgress::default();
    let marked_at_ms = clock.now_ms();
    for blob in &preflight.plan.marks {
        let next_progress = match progress.checked_mark() {
            Ok(next) => next,
            Err(error) => {
                return Ok(GcOutcome::Incomplete {
                    report: preflight.report,
                    error,
                });
            }
        };
        if let Err(error) = transaction.stage_mark(blob, marked_at_ms) {
            return Ok(GcOutcome::Incomplete {
                report: preflight.report,
                error,
            });
        }
        staged_actions.push(action_for(GcActionKind::Mark, blob, None));
        progress = next_progress;
    }
    for blob in &preflight.plan.resurrections {
        let next_progress = match progress.checked_resurrection() {
            Ok(next) => next,
            Err(error) => {
                return Ok(GcOutcome::Incomplete {
                    report: preflight.report,
                    error,
                });
            }
        };
        if let Err(error) = transaction.stage_resurrection(blob) {
            return Ok(GcOutcome::Incomplete {
                report: preflight.report,
                error,
            });
        }
        staged_actions.push(action_for(GcActionKind::Resurrect, blob, None));
        progress = next_progress;
    }

    for candidate in &preflight.sweep_candidates {
        let next_progress =
            match progress.checked_sweep(candidate.blob.size_bytes, candidate.source_state) {
                Ok(next) => next,
                Err(error) => {
                    return Ok(commit_partial(
                        transaction,
                        preflight.report,
                        staged_actions,
                        progress,
                        error,
                    ));
                }
            };
        let next_physical = if candidate.source_state == SweepSourceState::Present {
            match PhysicalProgress::checked_after_present_unlink(
                &preflight.report,
                candidate.blob.size_bytes,
            ) {
                Ok(next) => Some(next),
                Err(error) => {
                    return Ok(commit_partial(
                        transaction,
                        preflight.report,
                        staged_actions,
                        progress,
                        error,
                    ));
                }
            }
        } else {
            None
        };
        match candidate.source_state {
            SweepSourceState::Present => {
                let Some(identity) = candidate.identity.as_ref() else {
                    return Ok(commit_partial(
                        transaction,
                        preflight.report,
                        staged_actions,
                        progress,
                        eyre!("missing preflight identity for {}", candidate.blob.hash),
                    ));
                };
                if let Err(error) =
                    mutator.revalidate_present(&config, &candidate.blob.hash, identity)
                {
                    return Ok(commit_partial(
                        transaction,
                        preflight.report,
                        staged_actions,
                        progress,
                        error,
                    ));
                }
                if let Err(error) = mutator.unlink_present(&config, &candidate.blob.hash) {
                    return Ok(commit_partial(
                        transaction,
                        preflight.report,
                        staged_actions,
                        progress,
                        error,
                    ));
                }
                next_physical
                    .expect("present sweep candidates have checked physical progress")
                    .assign_to(&mut preflight.report);
                if let Err(error) = mutator.sync_present_parent(&config, &candidate.blob.hash) {
                    return Ok(commit_partial(
                        transaction,
                        preflight.report,
                        staged_actions,
                        progress,
                        error,
                    ));
                }
            }
            SweepSourceState::AlreadyAbsent => {
                if let Err(error) = mutator.sync_absent(&config, &candidate.blob.hash) {
                    return Ok(commit_partial(
                        transaction,
                        preflight.report,
                        staged_actions,
                        progress,
                        error,
                    ));
                }
            }
        }

        if let Err(error) = transaction.stage_sweep(&candidate.blob) {
            return Ok(commit_partial(
                transaction,
                preflight.report,
                staged_actions,
                progress,
                error.wrap_err(format!(
                    "CAS file for {} may already be absent; a later GC can resume the interrupted sweep",
                    candidate.blob.hash
                )),
            ));
        }
        staged_actions.push(action_for(
            GcActionKind::Sweep,
            &candidate.blob,
            Some(candidate.source_state),
        ));
        progress = next_progress;
    }

    if let Err(error) = test_probe::pause_or_fail("gc-before-commit") {
        return Ok(commit_partial(
            transaction,
            preflight.report,
            staged_actions,
            progress,
            error,
        ));
    }

    match transaction.commit() {
        Ok(()) => {
            finish_committed(&mut preflight.report, staged_actions, progress);
            test_probe::pause("gc-commit-complete")?;
            Ok(GcOutcome::Complete(preflight.report))
        }
        Err(error) => Ok(GcOutcome::Incomplete {
            report: preflight.report,
            error,
        }),
    }
}

fn commit_partial(
    transaction: GcTransaction<'_>,
    mut report: GcReport,
    staged_actions: Vec<GcAction>,
    progress: StagedProgress,
    mutation_error: color_eyre::Report,
) -> GcOutcome {
    match transaction.commit() {
        Ok(()) => {
            finish_committed(&mut report, staged_actions, progress);
            GcOutcome::Incomplete {
                report,
                error: mutation_error,
            }
        }
        Err(commit_error) => GcOutcome::Incomplete {
            report,
            error: eyre!(
                "{mutation_error}; additionally failed to commit earlier GC progress: {commit_error}"
            ),
        },
    }
}

fn finish_committed(
    report: &mut GcReport,
    staged_actions: Vec<GcAction>,
    progress: StagedProgress,
) {
    report.completed_marks = progress.marks;
    report.completed_resurrections = progress.resurrections;
    report.completed_sweeps = progress.sweeps;
    report.bytes_reclaimed = progress.reclaimed;
    report.actions = staged_actions;
}

fn classify(referenced: bool, marked: bool) -> PlannedKind {
    match (referenced, marked) {
        (true, false) => PlannedKind::Unchanged,
        (true, true) => PlannedKind::Resurrect,
        (false, false) => PlannedKind::Mark,
        (false, true) => PlannedKind::Sweep,
    }
}

fn classify_missing_blob(
    config: &GcConfig,
    blob: &CatalogBlob,
    findings: &mut Vec<IntegrityFinding>,
) -> Result<()> {
    let path = config.store_root.blob_path(&blob.hash);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_file() => push_finding(
            findings,
            IntegrityFinding::new(
                "NON_REGULAR_BLOB",
                blob.hash.to_string(),
                format!("path={}", display_path(config, &path)),
            ),
        ),
        Ok(_) => push_finding(
            findings,
            IntegrityFinding::new(
                "BLOB_IO_ERROR",
                blob.hash.to_string(),
                format!(
                    "path={} reason=appeared-after-traversal",
                    display_path(config, &path)
                ),
            ),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if blob.marked_at_ms.is_some() && !blob.referenced {
                Ok(())
            } else {
                push_finding(
                    findings,
                    IntegrityFinding::new(
                        "MISSING_BLOB",
                        blob.hash.to_string(),
                        format!("path={}", display_path(config, &path)),
                    ),
                )
            }
        }
        Err(_) => push_finding(
            findings,
            IntegrityFinding::new(
                "BLOB_IO_ERROR",
                blob.hash.to_string(),
                format!("path={} reason=stat-failed", display_path(config, &path)),
            ),
        ),
    }
}

fn canonical_path_is_absent(config: &GcConfig, hash: &BlobHash) -> bool {
    fs::symlink_metadata(config.store_root.blob_path(hash))
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

fn display_path(config: &GcConfig, path: &std::path::Path) -> String {
    crate::integrity::escape_path(path.strip_prefix(config.store_root.path()).unwrap_or(path))
}

fn action_for(
    kind: GcActionKind,
    blob: &CatalogBlob,
    sweep_source_state: Option<SweepSourceState>,
) -> GcAction {
    GcAction {
        kind,
        hash: blob.hash.clone(),
        size_bytes: blob.size_bytes,
        sweep_source_state,
    }
}

fn count<T>(items: &[T], label: &str) -> Result<u64> {
    u64::try_from(items.len()).wrap_err_with(|| format!("{label} count overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::num::NonZeroUsize;

    use assert_fs::TempDir;

    use crate::catalog::{BlobRecord, Catalog, SourceObservation, test_support};
    use crate::paths::{SourceRelativePath, StoreRoot};

    #[test]
    fn classification_covers_all_run_start_states() {
        assert_eq!(classify(true, false), PlannedKind::Unchanged);
        assert_eq!(classify(true, true), PlannedKind::Resurrect);
        assert_eq!(classify(false, false), PlannedKind::Mark);
        assert_eq!(classify(false, true), PlannedKind::Sweep);
    }

    #[test]
    fn checked_staged_progress_reports_counter_specific_overflow() {
        let mark_error = StagedProgress {
            marks: u64::MAX,
            ..StagedProgress::default()
        }
        .checked_mark()
        .expect_err("mark overflow");
        assert!(mark_error.to_string().contains("completed mark counter"));

        let resurrection_error = StagedProgress {
            resurrections: u64::MAX,
            ..StagedProgress::default()
        }
        .checked_resurrection()
        .expect_err("resurrection overflow");
        assert!(
            resurrection_error
                .to_string()
                .contains("completed resurrection counter")
        );

        let sweep_error = StagedProgress {
            sweeps: u64::MAX,
            ..StagedProgress::default()
        }
        .checked_sweep(0, SweepSourceState::AlreadyAbsent)
        .expect_err("sweep overflow");
        assert!(sweep_error.to_string().contains("completed sweep counter"));

        let byte_error = StagedProgress {
            reclaimed: u64::MAX,
            ..StagedProgress::default()
        }
        .checked_sweep(1, SweepSourceState::Present)
        .expect_err("reclaimed byte overflow");
        assert!(byte_error.to_string().contains("reclaimed byte count"));
        assert_eq!(
            StagedProgress {
                reclaimed: u64::MAX,
                ..StagedProgress::default()
            }
            .checked_sweep(1, SweepSourceState::AlreadyAbsent)
            .expect("absent sweep adds no reclaimed bytes")
            .reclaimed,
            u64::MAX
        );
    }

    #[test]
    fn checked_physical_progress_does_not_partially_assign_on_overflow() {
        let mut report = empty_report();
        report.cas_files_removed = 7;
        report.bytes_unlinked = u64::MAX;
        let error =
            PhysicalProgress::checked_after_present_unlink(&report, 1).expect_err("byte overflow");
        assert!(error.to_string().contains("unlinked byte count"));
        assert_eq!(report.cas_files_removed, 7);
        assert_eq!(report.bytes_unlinked, u64::MAX);

        report.cas_files_removed = u64::MAX;
        report.bytes_unlinked = 0;
        let error = PhysicalProgress::checked_after_present_unlink(&report, 0)
            .expect_err("file counter overflow");
        assert!(error.to_string().contains("removed CAS file counter"));
        assert_eq!(report.cas_files_removed, u64::MAX);
        assert_eq!(report.bytes_unlinked, 0);
    }

    #[test]
    fn fixed_clock_value_is_persisted_for_every_mark() {
        let temp = TempDir::new().expect("tempdir");
        let store_path = temp.path().join("store");
        fs::create_dir_all(store_path.join("blobs")).expect("create blobs root");
        let store_root = StoreRoot::validate_existing(&store_path).expect("validate store");
        let db_path = store_root.default_db_path();
        let mut catalog = Catalog::open_or_initialize(&db_path).expect("create catalog");
        add_fixture_blob(&store_root, &mut catalog, b"first");
        add_fixture_blob(&store_root, &mut catalog, b"second");
        drop(catalog);
        test_support::delete_sources(&db_path, None).expect("make fixtures unreachable");

        let config = GcConfig {
            store_root,
            db_path: db_path.clone(),
            dry_run: false,
            chunk_size: NonZeroUsize::new(2).expect("non-zero"),
        };
        let outcome =
            collect_garbage_with_clock(config, &FixedClock).expect("collect with fixed clock");
        let GcOutcome::Complete(report) = outcome else {
            panic!("expected complete mark run");
        };
        assert_eq!(report.completed_marks, 2);

        let marks = test_support::read_marks(&db_path).expect("read marks");
        assert_eq!(marks, vec![123, 123]);
    }

    #[test]
    fn sync_failure_after_unlink_reports_physical_only_progress_and_recovers() {
        let (_temp, config, hash) = marked_unreachable_fixture(b"sync-failure");
        let outcome = collect_garbage_with_dependencies(config.clone(), &FixedClock, &FailSync)
            .expect("GC with injected sync failure");
        let GcOutcome::Incomplete { report, error } = outcome else {
            panic!("expected incomplete outcome");
        };
        assert!(
            error
                .to_string()
                .contains("injected directory sync failure")
        );
        assert_eq!(report.cas_files_removed, 1);
        assert_eq!(report.bytes_unlinked, 12);
        assert_eq!(report.completed_sweeps, 0);
        assert!(report.actions.is_empty());
        assert!(!config.store_root.blob_path(&hash).exists());
        assert_eq!(catalog_blob_count(&config.db_path), 1);

        let recovery = collect_garbage(config.clone()).expect("recover interrupted sweep");
        let GcOutcome::Complete(report) = recovery else {
            panic!("expected complete recovery");
        };
        assert_eq!(report.completed_sweeps, 1);
        assert_eq!(report.cas_files_removed, 0);
        assert_eq!(report.bytes_reclaimed, 0);
        assert_eq!(catalog_blob_count(&config.db_path), 0);
    }

    #[test]
    fn revalidation_and_unlink_failures_stop_before_physical_progress_and_recover() {
        for stage in [FaultStage::Revalidate, FaultStage::Unlink] {
            let (_temp, config, hash) = marked_unreachable_fixture(b"early-failure");
            let outcome = collect_garbage_with_dependencies(
                config.clone(),
                &FixedClock,
                &FailAtStage { stage },
            )
            .expect("GC with injected early failure");
            let GcOutcome::Incomplete { report, error } = outcome else {
                panic!("expected incomplete outcome");
            };
            assert!(error.to_string().contains(stage.error_text()));
            assert_eq!(report.cas_files_removed, 0);
            assert_eq!(report.bytes_unlinked, 0);
            assert_eq!(report.completed_sweeps, 0);
            assert!(report.actions.is_empty());
            assert!(config.store_root.blob_path(&hash).exists());
            assert_eq!(catalog_blob_count(&config.db_path), 1);

            let recovery = collect_garbage(config.clone()).expect("recover candidate");
            let GcOutcome::Complete(report) = recovery else {
                panic!("expected complete recovery");
            };
            assert_eq!(report.completed_sweeps, 1);
            assert_eq!(report.cas_files_removed, 1);
            assert_eq!(catalog_blob_count(&config.db_path), 0);
        }
    }

    #[test]
    fn later_failure_commits_earlier_marks_resurrections_and_sweeps() {
        let temp = TempDir::new().expect("tempdir");
        let store_path = temp.path().join("store");
        fs::create_dir_all(store_path.join("blobs")).expect("create blobs root");
        let store_root = StoreRoot::validate_existing(&store_path).expect("validate store");
        let db_path = store_root.default_db_path();
        let mut catalog = Catalog::open_or_initialize(&db_path).expect("create catalog");
        let sweep_a = add_fixture_blob_at(&store_root, &mut catalog, b"sweep-a", "sweep-a");
        let sweep_b = add_fixture_blob_at(&store_root, &mut catalog, b"sweep-b", "sweep-b");
        let mark = add_fixture_blob_at(&store_root, &mut catalog, b"mark", "mark");
        let resurrect = add_fixture_blob_at(&store_root, &mut catalog, b"resurrect", "resurrect");
        drop(catalog);
        for hash in [&sweep_a, &sweep_b, &mark] {
            test_support::delete_sources(&db_path, Some(hash)).expect("make blob unreachable");
        }
        for hash in [&sweep_a, &sweep_b, &resurrect] {
            test_support::set_mark(&db_path, hash, 1).expect("mark fixture blob");
        }

        let config = GcConfig {
            store_root,
            db_path: db_path.clone(),
            dry_run: false,
            chunk_size: NonZeroUsize::new(2).expect("non-zero"),
        };
        let fault = FailSecondRemoval {
            calls: Cell::new(0),
        };
        let outcome = collect_garbage_with_dependencies(config.clone(), &FixedClock, &fault)
            .expect("GC with later failure");
        let GcOutcome::Incomplete { report, error } = outcome else {
            panic!("expected incomplete outcome");
        };
        assert!(error.to_string().contains("injected deletion failure"));
        assert_eq!(report.planned_marks, 1);
        assert_eq!(report.planned_resurrections, 1);
        assert_eq!(report.planned_sweeps, 2);
        assert_eq!(report.completed_marks, 1);
        assert_eq!(report.completed_resurrections, 1);
        assert_eq!(report.completed_sweeps, 1);
        assert_eq!(report.cas_files_removed, 1);
        assert_eq!(report.actions.len(), 3);
        assert_eq!(catalog_mark(&db_path, &mark), Some(123));
        assert_eq!(catalog_mark(&db_path, &resurrect), None);

        let recovery = collect_garbage(config).expect("recover remaining candidates");
        let GcOutcome::Complete(report) = recovery else {
            panic!("expected complete recovery");
        };
        assert_eq!(report.completed_sweeps, 2);
        assert_eq!(catalog_blob_count(&db_path), 1);
        assert_eq!(catalog_mark(&db_path, &resurrect), None);
    }

    #[test]
    fn injected_second_deletion_failure_commits_earlier_sweep_and_rerun_recovers() {
        let temp = TempDir::new().expect("tempdir");
        let store_path = temp.path().join("store");
        fs::create_dir_all(store_path.join("blobs")).expect("create blobs root");
        let store_root = StoreRoot::validate_existing(&store_path).expect("validate store");
        let db_path = store_root.default_db_path();
        let mut catalog = Catalog::open_or_initialize(&db_path).expect("create catalog");
        let first = add_fixture_blob(&store_root, &mut catalog, b"first");
        let second = add_fixture_blob(&store_root, &mut catalog, b"second");
        let _live = add_fixture_blob(&store_root, &mut catalog, b"live");
        drop(catalog);

        let config = GcConfig {
            store_root,
            db_path,
            dry_run: false,
            chunk_size: NonZeroUsize::new(2).expect("non-zero"),
        };
        let first_run =
            collect_garbage_with_clock(config.clone(), &FixedClock).expect("first GC run");
        assert!(matches!(first_run, GcOutcome::Complete(_)));

        let mut sorted = [first.clone(), second.clone()];
        sorted.sort();
        let fault = FailSecondRemoval {
            calls: Cell::new(0),
        };
        let second_run = collect_garbage_with_dependencies(config.clone(), &FixedClock, &fault)
            .expect("second GC run");
        let GcOutcome::Incomplete { report, error } = second_run else {
            panic!("expected injected incomplete outcome");
        };
        assert!(error.to_string().contains("injected deletion failure"));
        assert_eq!(report.completed_sweeps, 1);
        assert_eq!(report.cas_files_removed, 1);
        assert!(!config.store_root.blob_path(&sorted[0]).exists());
        assert!(config.store_root.blob_path(&sorted[1]).exists());

        let recovery = collect_garbage(config.clone()).expect("recovery GC run");
        let GcOutcome::Complete(report) = recovery else {
            panic!("expected recovery to complete");
        };
        assert_eq!(report.completed_sweeps, 1);
        assert!(!config.store_root.blob_path(&sorted[1]).exists());
    }

    struct FixedClock;

    impl Clock for FixedClock {
        fn now_ms(&self) -> i64 {
            123
        }
    }

    struct FailSecondRemoval {
        calls: Cell<u64>,
    }

    impl GcStoreMutator for FailSecondRemoval {
        fn revalidate_present(
            &self,
            config: &GcConfig,
            hash: &BlobHash,
            identity: &BlobFileIdentity,
        ) -> Result<()> {
            FilesystemGcMutator.revalidate_present(config, hash, identity)
        }

        fn unlink_present(&self, config: &GcConfig, hash: &BlobHash) -> Result<()> {
            let call = self.calls.get() + 1;
            self.calls.set(call);
            if call == 2 {
                return Err(eyre!("injected deletion failure"));
            }
            FilesystemGcMutator.unlink_present(config, hash)
        }

        fn sync_present_parent(&self, config: &GcConfig, hash: &BlobHash) -> Result<()> {
            FilesystemGcMutator.sync_present_parent(config, hash)
        }

        fn sync_absent(&self, config: &GcConfig, hash: &BlobHash) -> Result<()> {
            FilesystemGcMutator.sync_absent(config, hash)
        }
    }

    struct FailSync;

    impl GcStoreMutator for FailSync {
        fn revalidate_present(
            &self,
            config: &GcConfig,
            hash: &BlobHash,
            identity: &BlobFileIdentity,
        ) -> Result<()> {
            FilesystemGcMutator.revalidate_present(config, hash, identity)
        }

        fn unlink_present(&self, config: &GcConfig, hash: &BlobHash) -> Result<()> {
            FilesystemGcMutator.unlink_present(config, hash)
        }

        fn sync_present_parent(&self, _config: &GcConfig, _hash: &BlobHash) -> Result<()> {
            Err(eyre!("injected directory sync failure"))
        }

        fn sync_absent(&self, config: &GcConfig, hash: &BlobHash) -> Result<()> {
            FilesystemGcMutator.sync_absent(config, hash)
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum FaultStage {
        Revalidate,
        Unlink,
    }

    impl FaultStage {
        fn error_text(self) -> &'static str {
            match self {
                Self::Revalidate => "injected revalidation failure",
                Self::Unlink => "injected unlink failure",
            }
        }
    }

    struct FailAtStage {
        stage: FaultStage,
    }

    impl GcStoreMutator for FailAtStage {
        fn revalidate_present(
            &self,
            config: &GcConfig,
            hash: &BlobHash,
            identity: &BlobFileIdentity,
        ) -> Result<()> {
            if matches!(self.stage, FaultStage::Revalidate) {
                return Err(eyre!("{}", self.stage.error_text()));
            }
            FilesystemGcMutator.revalidate_present(config, hash, identity)
        }

        fn unlink_present(&self, config: &GcConfig, hash: &BlobHash) -> Result<()> {
            if matches!(self.stage, FaultStage::Unlink) {
                return Err(eyre!("{}", self.stage.error_text()));
            }
            FilesystemGcMutator.unlink_present(config, hash)
        }

        fn sync_present_parent(&self, config: &GcConfig, hash: &BlobHash) -> Result<()> {
            FilesystemGcMutator.sync_present_parent(config, hash)
        }

        fn sync_absent(&self, config: &GcConfig, hash: &BlobHash) -> Result<()> {
            FilesystemGcMutator.sync_absent(config, hash)
        }
    }

    fn empty_report() -> GcReport {
        GcReport {
            dry_run: false,
            catalog_blobs: 0,
            reachable_blobs: 0,
            sweep_candidates_hashed: 0,
            planned_marks: 0,
            planned_resurrections: 0,
            planned_sweeps: 0,
            completed_marks: 0,
            completed_resurrections: 0,
            completed_sweeps: 0,
            cas_files_removed: 0,
            bytes_reclaimable: 0,
            bytes_unlinked: 0,
            bytes_reclaimed: 0,
            actions: Vec::new(),
            findings: Vec::new(),
        }
    }

    fn marked_unreachable_fixture(content: &[u8]) -> (TempDir, GcConfig, BlobHash) {
        let temp = TempDir::new().expect("tempdir");
        let store_path = temp.path().join("store");
        fs::create_dir_all(store_path.join("blobs")).expect("create blobs root");
        let store_root = StoreRoot::validate_existing(&store_path).expect("validate store");
        let db_path = store_root.default_db_path();
        let mut catalog = Catalog::open_or_initialize(&db_path).expect("create catalog");
        let hash = add_fixture_blob(&store_root, &mut catalog, content);
        drop(catalog);
        test_support::delete_sources(&db_path, None).expect("make fixture unreachable");
        test_support::set_mark(&db_path, &hash, 1).expect("mark fixture");
        (
            temp,
            GcConfig {
                store_root,
                db_path,
                dry_run: false,
                chunk_size: NonZeroUsize::new(2).expect("non-zero"),
            },
            hash,
        )
    }

    fn catalog_blob_count(path: &std::path::Path) -> i64 {
        test_support::blob_count(path).expect("count blobs")
    }

    fn catalog_mark(path: &std::path::Path, hash: &BlobHash) -> Option<i64> {
        test_support::read_mark(path, hash).expect("read mark")
    }

    fn add_fixture_blob(store_root: &StoreRoot, catalog: &mut Catalog, content: &[u8]) -> BlobHash {
        add_fixture_blob_at(store_root, catalog, content, "same.bin")
    }

    fn add_fixture_blob_at(
        store_root: &StoreRoot,
        catalog: &mut Catalog,
        content: &[u8],
        relative_path: &str,
    ) -> BlobHash {
        let hash = BlobHash::new(blake3::hash(content).to_hex().to_string()).expect("valid hash");
        let path = store_root.blob_path(&hash);
        fs::create_dir_all(path.parent().expect("blob parent")).expect("create shard");
        fs::write(&path, content).expect("write fixture blob");
        catalog
            .record_imported_file(
                BlobRecord {
                    hash: hash.clone(),
                    size_bytes: content.len() as u64,
                    created_at_ms: 1,
                },
                SourceObservation {
                    source_root: String::from("/fixture"),
                    relative_path: SourceRelativePath::from_catalog_text(relative_path)
                        .expect("relative path"),
                    blob_hash: hash.clone(),
                    size_bytes: content.len() as u64,
                    modified_at_ms: None,
                    observed_at_ms: 1,
                },
            )
            .expect("record fixture blob");
        hash
    }
}
