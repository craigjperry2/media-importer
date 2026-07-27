//! Mark-and-sweep garbage collection.
//!
//! Correctness requires the selected store and catalog to remain quiescent
//! from path validation until this operation returns. The SQLite reservation
//! protects the catalog snapshot, but milestone 4 deliberately has no CAS run
//! lock.

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
use crate::store::{
    BlobFileIdentity, inspect_cas, open_blob_no_follow, remove_revalidated_blob,
    sync_nearest_existing_blob_parent,
};

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

#[derive(Default)]
struct StagedProgress {
    marks: u64,
    resurrections: u64,
    sweeps: u64,
    reclaimed: u64,
}

trait GcStoreMutator {
    fn remove_present(
        &self,
        config: &GcConfig,
        hash: &BlobHash,
        identity: &BlobFileIdentity,
    ) -> Result<()>;

    fn sync_absent(&self, config: &GcConfig, hash: &BlobHash) -> Result<()>;
}

struct FilesystemGcMutator;

impl GcStoreMutator for FilesystemGcMutator {
    fn remove_present(
        &self,
        config: &GcConfig,
        hash: &BlobHash,
        identity: &BlobFileIdentity,
    ) -> Result<()> {
        remove_revalidated_blob(&config.store_root, hash, identity)
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
        "starting garbage collection; store and catalog must remain quiescent"
    );
    if config.dry_run {
        collect_dry_run(config)
    } else {
        collect_real(config, clock, mutator)
    }
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
        if let Err(error) = transaction.stage_mark(blob, marked_at_ms) {
            return Ok(GcOutcome::Incomplete {
                report: preflight.report,
                error,
            });
        }
        staged_actions.push(action_for(GcActionKind::Mark, blob, None));
        progress.marks += 1;
    }
    for blob in &preflight.plan.resurrections {
        if let Err(error) = transaction.stage_resurrection(blob) {
            return Ok(GcOutcome::Incomplete {
                report: preflight.report,
                error,
            });
        }
        staged_actions.push(action_for(GcActionKind::Resurrect, blob, None));
        progress.resurrections += 1;
    }

    for candidate in &preflight.sweep_candidates {
        let mutation = match candidate.source_state {
            SweepSourceState::Present => {
                let identity = candidate.identity.as_ref().ok_or_else(|| {
                    eyre!("missing preflight identity for {}", candidate.blob.hash)
                })?;
                mutator.remove_present(&config, &candidate.blob.hash, identity)
            }
            SweepSourceState::AlreadyAbsent => mutator.sync_absent(&config, &candidate.blob.hash),
        };
        if let Err(error) = mutation {
            return Ok(commit_partial(
                transaction,
                preflight.report,
                staged_actions,
                progress,
                error,
            ));
        }

        if candidate.source_state == SweepSourceState::Present {
            preflight.report.cas_files_removed = preflight
                .report
                .cas_files_removed
                .checked_add(1)
                .ok_or_else(|| eyre!("removed CAS file counter overflow"))?;
            preflight.report.bytes_unlinked = preflight
                .report
                .bytes_unlinked
                .checked_add(candidate.blob.size_bytes)
                .ok_or_else(|| eyre!("unlinked byte count overflow"))?;
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
        progress.sweeps += 1;
        if candidate.source_state == SweepSourceState::Present {
            progress.reclaimed = progress
                .reclaimed
                .checked_add(candidate.blob.size_bytes)
                .ok_or_else(|| eyre!("reclaimed byte count overflow"))?;
        }
    }

    match transaction.commit() {
        Ok(()) => {
            finish_committed(&mut preflight.report, staged_actions, progress);
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

    use crate::catalog::{BlobRecord, Catalog, SourceObservation};
    use crate::paths::{SourceRelativePath, StoreRoot};

    #[test]
    fn classification_covers_all_run_start_states() {
        assert_eq!(classify(true, false), PlannedKind::Unchanged);
        assert_eq!(classify(true, true), PlannedKind::Resurrect);
        assert_eq!(classify(false, false), PlannedKind::Mark);
        assert_eq!(classify(false, true), PlannedKind::Sweep);
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
        fn remove_present(
            &self,
            config: &GcConfig,
            hash: &BlobHash,
            identity: &BlobFileIdentity,
        ) -> Result<()> {
            let call = self.calls.get() + 1;
            self.calls.set(call);
            if call == 2 {
                return Err(eyre!("injected deletion failure"));
            }
            FilesystemGcMutator.remove_present(config, hash, identity)
        }

        fn sync_absent(&self, config: &GcConfig, hash: &BlobHash) -> Result<()> {
            FilesystemGcMutator.sync_absent(config, hash)
        }
    }

    fn add_fixture_blob(store_root: &StoreRoot, catalog: &mut Catalog, content: &[u8]) -> BlobHash {
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
                    relative_path: SourceRelativePath::from_catalog_text("same.bin")
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
