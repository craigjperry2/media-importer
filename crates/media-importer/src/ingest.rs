use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};
use crossbeam_channel::{Receiver, Sender};

use crate::catalog::{
    BlobRecord, CatalogWriterConfig, CatalogWriterHandle, Clock, ImportWriteOutcome,
    ImportWriteTicket, KnownSourceFile, ReadOnlyCatalog, SourceObservation,
    SourceObservationOutcome, SystemClock,
};
use crate::config::ImportConfig;
use crate::paths::BlobHash;
use crate::run_lock::{LockMode, StoreRunLock};
use crate::scanner::{MountId, SourceFileCandidate, scan_source};
use crate::store::{CasMetadataCheck, Store, StoreOutcome, StoredBlob};

const MAX_LIVE_SOURCE_WORKERS: usize = 8;
const SCHEDULER_QUEUE_CAPACITY: usize = 64;
const WORKER_QUEUE_CAPACITY: usize = 8;
const COMPLETION_QUEUE_CAPACITY: usize = 8;
/// Completed work can be retained in the channel or held by any source
/// worker while that channel is full. This is the total bounded footprint.
const COMPLETED_WORK_RETAINED_CAPACITY: usize = COMPLETION_QUEUE_CAPACITY + MAX_LIVE_SOURCE_WORKERS;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
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

/// Stable, behavior-level import instrumentation. No channel or thread identity
/// is exposed, so callers can prove source I/O and mount limits without taking
/// a dependency on executor mechanics.
#[derive(Clone, Eq, PartialEq)]
pub enum IngestEvent {
    CandidateDiscovered {
        sequence: u64,
        source_path: crate::paths::SourceRelativePath,
        mount_id: MountId,
    },
    MetadataSkipped {
        sequence: u64,
        source_path: crate::paths::SourceRelativePath,
        size_bytes: u64,
    },
    WorkQueued {
        sequence: u64,
        source_path: crate::paths::SourceRelativePath,
        mount_id: MountId,
    },
    WorkDequeued {
        sequence: u64,
        source_path: crate::paths::SourceRelativePath,
        mount_id: MountId,
    },
    SourceReadStarted {
        sequence: u64,
        source_path: crate::paths::SourceRelativePath,
        mount_id: MountId,
    },
    SourceReadCompleted {
        sequence: u64,
        source_path: crate::paths::SourceRelativePath,
        mount_id: MountId,
        size_bytes: u64,
    },
    WorkCancelled {
        sequence: u64,
        source_path: crate::paths::SourceRelativePath,
        mount_id: MountId,
    },
    WorkFailed {
        sequence: u64,
        source_path: crate::paths::SourceRelativePath,
        mount_id: MountId,
        context: String,
    },
    CasStored {
        sequence: u64,
        source_path: crate::paths::SourceRelativePath,
        outcome: StoreOutcome,
    },
    ActiveWorkers {
        mount_id: MountId,
        active: usize,
    },
    /// Exact occupancy sampled at a bounded pipeline boundary.  `mount_id`
    /// is present only for the per-mount scheduler backlog.
    QueueOccupancy {
        stage: QueueStage,
        mount_id: Option<MountId>,
        occupancy: usize,
        capacity: usize,
    },
    QueueBackpressure,
    CatalogSubmitted {
        sequence: u64,
        source_path: crate::paths::SourceRelativePath,
    },
    CatalogCommitted {
        sequence: u64,
        source_path: crate::paths::SourceRelativePath,
    },
    Cancellation,
    Shutdown,
}

impl fmt::Debug for IngestEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Events carry source-relative paths for observers that need to
        // correlate behavior. Their debug representation is intentionally
        // content-free so logging an event does not disclose source names or
        // worker failure context by accident.
        match self {
            Self::CandidateDiscovered {
                sequence, mount_id, ..
            } => formatter
                .debug_struct("CandidateDiscovered")
                .field("sequence", sequence)
                .field("mount_id", mount_id)
                .finish_non_exhaustive(),
            Self::MetadataSkipped {
                sequence,
                size_bytes,
                ..
            } => formatter
                .debug_struct("MetadataSkipped")
                .field("sequence", sequence)
                .field("size_bytes", size_bytes)
                .finish_non_exhaustive(),
            Self::WorkQueued {
                sequence, mount_id, ..
            }
            | Self::WorkDequeued {
                sequence, mount_id, ..
            }
            | Self::SourceReadStarted {
                sequence, mount_id, ..
            }
            | Self::WorkCancelled {
                sequence, mount_id, ..
            } => formatter
                .debug_struct(self.kind())
                .field("sequence", sequence)
                .field("mount_id", mount_id)
                .finish_non_exhaustive(),
            Self::SourceReadCompleted {
                sequence,
                mount_id,
                size_bytes,
                ..
            } => formatter
                .debug_struct("SourceReadCompleted")
                .field("sequence", sequence)
                .field("mount_id", mount_id)
                .field("size_bytes", size_bytes)
                .finish_non_exhaustive(),
            Self::WorkFailed {
                sequence, mount_id, ..
            } => formatter
                .debug_struct("WorkFailed")
                .field("sequence", sequence)
                .field("mount_id", mount_id)
                .finish_non_exhaustive(),
            Self::CasStored {
                sequence, outcome, ..
            } => formatter
                .debug_struct("CasStored")
                .field("sequence", sequence)
                .field("outcome", outcome)
                .finish_non_exhaustive(),
            Self::ActiveWorkers { mount_id, active } => formatter
                .debug_struct("ActiveWorkers")
                .field("mount_id", mount_id)
                .field("active", active)
                .finish(),
            Self::QueueOccupancy {
                stage,
                mount_id,
                occupancy,
                capacity,
            } => formatter
                .debug_struct("QueueOccupancy")
                .field("stage", stage)
                .field("mount_id", mount_id)
                .field("occupancy", occupancy)
                .field("capacity", capacity)
                .finish(),
            Self::QueueBackpressure => formatter.write_str("QueueBackpressure"),
            Self::CatalogSubmitted { sequence, .. } => formatter
                .debug_struct("CatalogSubmitted")
                .field("sequence", sequence)
                .finish_non_exhaustive(),
            Self::CatalogCommitted { sequence, .. } => formatter
                .debug_struct("CatalogCommitted")
                .field("sequence", sequence)
                .finish_non_exhaustive(),
            Self::Cancellation => formatter.write_str("Cancellation"),
            Self::Shutdown => formatter.write_str("Shutdown"),
        }
    }
}

impl IngestEvent {
    fn kind(&self) -> &'static str {
        match self {
            Self::WorkQueued { .. } => "WorkQueued",
            Self::WorkDequeued { .. } => "WorkDequeued",
            Self::SourceReadStarted { .. } => "SourceReadStarted",
            Self::WorkCancelled { .. } => "WorkCancelled",
            _ => unreachable!("kind is called only for event variants with a mount"),
        }
    }
}

/// Stable names for the five bounded executor queues/bookkeeping stages.
/// `PerMountWork` counts scheduler backlog plus work already admitted to the
/// bounded shared worker queue or an active reader for that mount.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueStage {
    ScannerToScheduler,
    PerMountWork,
    CompletedBlobs,
    CatalogRequests,
    CatalogOutcomes,
}

pub trait IngestObserver: Send + Sync {
    fn observe(&self, event: IngestEvent);
}

struct NoopObserver;
impl IngestObserver for NoopObserver {
    fn observe(&self, _: IngestEvent) {}
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
    import_source_with_clock_and_observer(config, clock, Arc::new(NoopObserver))
}

/// Import through the real filesystem, store, catalog writer, and run-lock
/// boundary while publishing behavior-level events to `observer`.
///
/// The convenience import entry points deliberately retain their no-op
/// observer. Callers that need to prove I/O or scheduler behavior can use this
/// deep interface without recreating CLI orchestration.
pub fn import_source_with_clock_and_observer(
    config: ImportConfig,
    clock: &dyn Clock,
    observer: Arc<dyn IngestObserver>,
) -> Result<ImportReport> {
    if config.dry_run {
        if config.store_root.path().exists() {
            let _lock = StoreRunLock::acquire(&config.store_root, "import", LockMode::Shared)?;
            dry_run_import(config, observer.as_ref())
        } else {
            dry_run_import(config, observer.as_ref())
        }
    } else {
        match std::fs::create_dir(config.store_root.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let _lock = StoreRunLock::acquire(&config.store_root, "import", LockMode::Exclusive)?;
        real_import(config, clock, observer)
    }
}

fn real_import(
    config: ImportConfig,
    clock: &dyn Clock,
    observer: Arc<dyn IngestObserver>,
) -> Result<ImportReport> {
    let store = Store::new(config.store_root.clone());
    store.prepare_for_import()?;
    let catalog = ReadOnlyCatalog::open_if_exists(&config.db_path)?;
    let writer_config = CatalogWriterConfig::default();
    let max_outstanding = writer_config.max_batch_records.get();
    let writer = CatalogWriterHandle::spawn(config.db_path.clone(), writer_config)?;
    // Catch coordinator panics as operational errors so the writer is always
    // finished before the outer run lock is released.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        execute_import(
            scan_source(&config.source_root),
            ExecutorContext {
                config,
                clock,
                store: &store,
                catalog: catalog.as_ref(),
                writer: &writer,
                max_outstanding,
                observer: Arc::clone(&observer),
            },
        )
    }))
    .unwrap_or_else(|payload| {
        Err(eyre!(
            "import coordinator panicked: {}",
            panic_message(payload)
        ))
    });
    let finish = writer.finish();
    match (result, finish) {
        (Ok(report), Ok(_)) => Ok(report),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Ok(_)) => Err(error),
        (Err(operation), Err(writer)) => Err(writer.wrap_err(format!(
            "import operation also failed while catalog writer terminated: {operation:#}"
        ))),
    }
}

/// Executes bounded streaming import work. At most 64 candidates wait in the
/// scheduler, eight source workers exist, and the existing catalog writer has
/// its own bounded request queue. Arithmetic and all capacities are constants
/// validated at compile time; worker configuration is a non-zero CLI type.
struct ExecutorContext<'a> {
    config: ImportConfig,
    clock: &'a dyn Clock,
    store: &'a Store,
    catalog: Option<&'a ReadOnlyCatalog>,
    writer: &'a CatalogWriterHandle,
    max_outstanding: usize,
    observer: Arc<dyn IngestObserver>,
}

fn execute_import(
    candidates: impl Iterator<Item = Result<SourceFileCandidate>>,
    context: ExecutorContext<'_>,
) -> Result<ImportReport> {
    let ExecutorContext {
        config,
        clock,
        store,
        catalog,
        writer,
        max_outstanding,
        observer,
    } = context;
    let source_root_text = config.source_root.durable_text()?;
    let mut scanner = candidates;
    let mut report = ImportReport {
        dry_run: false,
        ..ImportReport::default()
    };
    let mut tickets = BTreeMap::new();
    let mut pending = VecDeque::new();
    let mut active = HashMap::<MountId, usize>::new();
    let mut last_dispatched_mount = None;
    let mut sequence = 0_u64;
    let mut scanner_done = false;
    // The configured value controls a mount's share of the fixed source-worker
    // pool. A mount cannot have more admitted work than the pool can ever
    // execute, otherwise its bounded footprint would exceed the reported
    // per-mount capacity before workers apply backpressure.
    let effective_workers_per_mount = effective_workers_per_mount(config.workers_per_mount);
    let mut workers = SourceWorkers::start(
        config.store_root.clone(),
        config.chunk_size,
        Arc::clone(&observer),
    )?;
    // This boundary owns the source workers.  Scanner, scheduler, coordinator,
    // and observer callbacks are all allowed to fail operationally, but none of
    // them may unwind past this point and detach a worker while it still owns a
    // staging file or CAS mutation.
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        loop {
            poll_terminal_failures(&workers, writer)?;
            dispatch_ready(
                &mut pending,
                &mut active,
                effective_workers_per_mount,
                &workers,
                observer.as_ref(),
                &mut last_dispatched_mount,
            )?;
            // A worker or writer can fail while dispatch was considering a
            // bounded queue. Do not consume another scanner item in that case.
            poll_terminal_failures(&workers, writer)?;
            if !scanner_done && pending.len() < SCHEDULER_QUEUE_CAPACITY {
                crate::test_probe::pause_or_panic("scanner-before-next")?;
                crate::test_probe::pause_or_fail("scanner-before-next")?;
                poll_terminal_failures(&workers, writer)?;
                match scanner.next() {
                    Some(Ok(candidate)) => {
                        observer.observe(IngestEvent::CandidateDiscovered {
                            sequence,
                            source_path: candidate.relative_path.clone(),
                            mount_id: candidate.mount_id,
                        });
                        report_observed_file(&mut report, &candidate);
                        let known = match catalog {
                            Some(catalog) => catalog
                                .known_source_file(&source_root_text, &candidate.relative_path)?,
                            None => None,
                        };
                        match validated_disposition(
                            store,
                            classify_import(config.metadata_skip, &candidate, known.as_ref()),
                        )? {
                            ImportDisposition::Skip {
                                blob_hash,
                                size_bytes,
                                resurrection_required,
                            } => {
                                let ticket_sequence = sequence;
                                let ticket = writer.observe_unchanged_source(
                                    ticket_sequence,
                                    source_observation(
                                        &source_root_text,
                                        &candidate,
                                        blob_hash,
                                        clock.now_ms(),
                                    ),
                                    size_bytes,
                                )?;
                                observer.observe(IngestEvent::MetadataSkipped {
                                    sequence: ticket_sequence,
                                    source_path: candidate.relative_path.clone(),
                                    size_bytes,
                                });
                                observer.observe(IngestEvent::CatalogSubmitted {
                                    sequence: ticket_sequence,
                                    source_path: candidate.relative_path.clone(),
                                });
                                sequence = next_sequence(sequence)?;
                                tickets.insert(ticket_sequence, PendingTicket {
                                    sequence: ticket_sequence,
                                    source_path: candidate.relative_path.clone(),
                                    ticket,
                                });
                                observe_catalog_occupancy(
                                    writer,
                                    tickets.len(),
                                    max_outstanding,
                                    observer.as_ref(),
                                );
                                drain_outcomes_to_limit(
                                    &mut tickets,
                                    &mut report,
                                    max_outstanding,
                                    writer,
                                    observer.as_ref(),
                                )?;
                                report.files_skipped += 1;
                                report.bytes_skipped += size_bytes;
                                report.blobs_reused += 1;
                                report.source_records_updated += 1;
                                if resurrection_required {
                                    tracing::trace!(path=?candidate.relative_path, "resurrected metadata-skipped blob");
                                }
                            }
                            ImportDisposition::Hash { reason } => {
                                tracing::trace!(path=?candidate.relative_path, ?reason, "queued source content read");
                                pending.push_back(Work {
                                    sequence,
                                    candidate,
                                });
                                observe_work_occupancy(
                                    &pending,
                                    &active,
                                    effective_workers_per_mount,
                                    observer.as_ref(),
                                )?;
                                sequence = next_sequence(sequence)?;
                            }
                        }
                    }
                    Some(Err(error)) => return Err(error),
                    None => scanner_done = true,
                }
                continue;
            }
            if !scanner_done && pending.len() >= SCHEDULER_QUEUE_CAPACITY {
                observer.observe(IngestEvent::QueueBackpressure);
            }
            if active.is_empty() && pending.is_empty() && scanner_done {
                break;
            }
            let completed = workers.recv(writer)?;
            complete_work(
                completed,
                &mut active,
                &mut report,
                CompletionContext {
                    source_root: &source_root_text,
                    clock,
                    writer,
                    tickets: &mut tickets,
                    max_outstanding,
                    pending: &pending,
                    workers_per_mount: effective_workers_per_mount,
                    observer: observer.as_ref(),
                },
            )?;
        }
        while let Some((sequence, ticket)) = tickets.pop_first() {
            let source_path = ticket.source_path.clone();
            let outcome = resolve_ticket(ticket, &tickets)?;
            observer.observe(IngestEvent::CatalogCommitted {
                sequence,
                source_path,
            });
            apply_outcome(&mut report, outcome);
        }
        Ok(report)
    }))
    .unwrap_or_else(|payload| {
        Err(eyre!(
            "import executor coordinator panicked: {}",
            panic_message(payload)
        ))
    });
    if run.is_err() {
        // Stop workers before notifying application-provided instrumentation.
        // An observer may block or panic, but neither may leave queued work
        // able to begin source I/O, staging, or CAS mutation.
        workers.cancel();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observer.observe(IngestEvent::Cancellation);
        }));
    }
    let shutdown = workers.finish();
    // Ditto for the terminal event: cleanup is more important than telemetry.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        observer.observe(IngestEvent::Shutdown);
    }));
    match (run, shutdown) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(join)) => {
            Err(error.wrap_err(format!("source worker shutdown also failed: {join:#}")))
        }
    }
}

/// Terminal worker and catalog status takes precedence over source admission.
/// Cancellation is set by the failing worker before it publishes its bounded
/// control message; cancelling here also stops sibling workers for catalog
/// failures before the coordinator begins cleanup.
fn poll_terminal_failures(workers: &SourceWorkers, writer: &CatalogWriterHandle) -> Result<()> {
    if let Some(failure) = workers.try_terminal_failure() {
        workers.cancel();
        return Err(eyre!(failure));
    }
    if let Some(failure) = writer.try_terminal_failure() {
        workers.cancel();
        return Err(eyre!("catalog writer failed: {failure}"));
    }
    Ok(())
}

fn next_sequence(sequence: u64) -> Result<u64> {
    sequence
        .checked_add(1)
        .ok_or_else(|| eyre!("catalog import sequence overflow"))
}

fn effective_workers_per_mount(workers_per_mount: NonZeroUsize) -> NonZeroUsize {
    NonZeroUsize::new(workers_per_mount.get().min(MAX_LIVE_SOURCE_WORKERS))
        .expect("a non-zero worker configuration remains non-zero when capped")
}

fn dispatch_ready(
    pending: &mut VecDeque<Work>,
    active: &mut HashMap<MountId, usize>,
    limit: NonZeroUsize,
    workers: &SourceWorkers,
    observer: &dyn IngestObserver,
    last_dispatched_mount: &mut Option<MountId>,
) -> Result<()> {
    loop {
        let eligible =
            |work: &Work| active.get(&work.candidate.mount_id).copied().unwrap_or(0) < limit.get();
        // Choose an eligible mount other than the previous dispatch whenever
        // one exists. This is round-robin across mount identities without
        // allocating a worker or queue for an idle mount.
        let index = pending
            .iter()
            .position(|work| {
                eligible(work) && Some(work.candidate.mount_id) != *last_dispatched_mount
            })
            .or_else(|| pending.iter().position(eligible));
        let Some(index) = index else {
            return Ok(());
        };
        let work = pending.remove(index).expect("pending work index");
        let mount_id = work.candidate.mount_id;
        let sequence = work.sequence;
        let source_path = work.candidate.relative_path.clone();
        if let Some(work) = workers.try_submit(work)? {
            // Do not block the coordinator while every worker is reporting to
            // its bounded completion queue. It must remain able to receive
            // completions (or observe cancellation) before attempting more work.
            pending.insert(index, work);
            observer.observe(IngestEvent::QueueBackpressure);
            return Ok(());
        }
        let count = active.entry(mount_id).or_default();
        *count = count
            .checked_add(1)
            .ok_or_else(|| eyre!("active mount worker counter overflow"))?;
        observer.observe(IngestEvent::WorkQueued {
            sequence,
            source_path,
            mount_id,
        });
        observe_work_occupancy(pending, active, limit, observer)?;
        *last_dispatched_mount = Some(mount_id);
    }
}

/// Report the full scheduler footprint. Per-mount accounting includes both
/// candidates still waiting in the scheduler and work already admitted to the
/// bounded shared worker queue (including a worker currently reading it).
fn observe_work_occupancy(
    pending: &VecDeque<Work>,
    active: &HashMap<MountId, usize>,
    workers_per_mount: NonZeroUsize,
    observer: &dyn IngestObserver,
) -> Result<()> {
    observer.observe(IngestEvent::QueueOccupancy {
        stage: QueueStage::ScannerToScheduler,
        mount_id: None,
        occupancy: pending.len(),
        capacity: SCHEDULER_QUEUE_CAPACITY,
    });
    let capacity = SCHEDULER_QUEUE_CAPACITY
        .checked_add(workers_per_mount.get().min(MAX_LIVE_SOURCE_WORKERS))
        .ok_or_else(|| eyre!("per-mount work queue capacity overflow"))?;
    let mounts = pending
        .iter()
        .map(|work| work.candidate.mount_id)
        .chain(active.keys().copied())
        .collect::<BTreeSet<_>>();
    for mount_id in mounts {
        let pending_count = pending
            .iter()
            .filter(|work| work.candidate.mount_id == mount_id)
            .count();
        observer.observe(IngestEvent::QueueOccupancy {
            stage: QueueStage::PerMountWork,
            mount_id: Some(mount_id),
            occupancy: pending_count + active.get(&mount_id).copied().unwrap_or_default(),
            capacity,
        });
    }
    Ok(())
}

fn observe_catalog_occupancy(
    writer: &CatalogWriterHandle,
    pending_outcomes: usize,
    pending_outcomes_capacity: usize,
    observer: &dyn IngestObserver,
) {
    let (occupancy, capacity) = writer.request_queue_occupancy();
    observer.observe(IngestEvent::QueueOccupancy {
        stage: QueueStage::CatalogRequests,
        mount_id: None,
        occupancy,
        capacity,
    });
    observer.observe(IngestEvent::QueueOccupancy {
        stage: QueueStage::CatalogOutcomes,
        mount_id: None,
        occupancy: pending_outcomes,
        capacity: pending_outcomes_capacity,
    });
}

struct CompletionContext<'a> {
    source_root: &'a str,
    clock: &'a dyn Clock,
    writer: &'a CatalogWriterHandle,
    tickets: &'a mut BTreeMap<u64, PendingTicket>,
    max_outstanding: usize,
    pending: &'a VecDeque<Work>,
    workers_per_mount: NonZeroUsize,
    observer: &'a dyn IngestObserver,
}

fn complete_work(
    completed: CompletedWork,
    active: &mut HashMap<MountId, usize>,
    report: &mut ImportReport,
    context: CompletionContext<'_>,
) -> Result<()> {
    let mount_id = completed.work.candidate.mount_id;
    let count = active
        .get_mut(&mount_id)
        .ok_or_else(|| eyre!("completed work without active mount accounting"))?;
    *count = count
        .checked_sub(1)
        .ok_or_else(|| eyre!("active mount worker counter underflow"))?;
    let became_idle = *count == 0;
    observe_work_occupancy(
        context.pending,
        active,
        context.workers_per_mount,
        context.observer,
    )?;
    if became_idle {
        active.remove(&mount_id);
    }
    let blob = match completed.result {
        Ok(blob) => blob,
        Err(error) => {
            context.observer.observe(IngestEvent::WorkFailed {
                sequence: completed.work.sequence,
                source_path: completed.work.candidate.relative_path.clone(),
                mount_id,
                context: format!("{error:#}"),
            });
            return Err(error.wrap_err(format!(
                "source worker failed for {:?}",
                completed.work.candidate.relative_path
            )));
        }
    };
    context.observer.observe(IngestEvent::CasStored {
        sequence: completed.work.sequence,
        source_path: completed.work.candidate.relative_path.clone(),
        outcome: blob.outcome.clone(),
    });
    report.files_hashed += 1;
    report.bytes_hashed += blob.size_bytes;
    report_store_outcome(report, &blob.outcome, blob.size_bytes);
    let sequence = completed.work.sequence;
    context.tickets.insert(
        sequence,
        PendingTicket {
            sequence,
            source_path: completed.work.candidate.relative_path.clone(),
            ticket: context.writer.record_imported_file(
                sequence,
                BlobRecord {
                    hash: blob.hash.clone(),
                    size_bytes: blob.size_bytes,
                    created_at_ms: context.clock.now_ms(),
                },
                source_observation(
                    context.source_root,
                    &completed.work.candidate,
                    blob.hash,
                    context.clock.now_ms(),
                ),
            )?,
        },
    );
    observe_catalog_occupancy(
        context.writer,
        context.tickets.len(),
        context.max_outstanding,
        context.observer,
    );
    context.observer.observe(IngestEvent::CatalogSubmitted {
        sequence,
        source_path: completed.work.candidate.relative_path.clone(),
    });
    drain_outcomes_to_limit(
        context.tickets,
        report,
        context.max_outstanding,
        context.writer,
        context.observer,
    )
}

struct Work {
    sequence: u64,
    candidate: SourceFileCandidate,
}
struct CompletedWork {
    work: Work,
    result: Result<StoredBlob>,
}
struct PendingTicket {
    sequence: u64,
    source_path: crate::paths::SourceRelativePath,
    ticket: ImportWriteTicket,
}
struct SourceWorkers {
    jobs: Option<Sender<Work>>,
    completed: Receiver<CompletedWork>,
    joins: Vec<thread::JoinHandle<()>>,
    cancelled: Arc<AtomicBool>,
    terminal_failures: Receiver<SourceWorkerFailure>,
    completed_retained: Arc<AtomicUsize>,
    observer: Arc<dyn IngestObserver>,
}

#[derive(Debug)]
struct SourceWorkerFailure {
    sequence: Option<u64>,
    source_path: Option<crate::paths::SourceRelativePath>,
    mount_id: Option<MountId>,
    context: String,
}

impl fmt::Display for SourceWorkerFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.sequence, &self.source_path, &self.mount_id) {
            (Some(sequence), Some(source_path), Some(mount_id)) => write!(
                formatter,
                "source worker/store failure at sequence {sequence} source {source_path:?} mount {mount_id:?}: {}",
                self.context
            ),
            _ => write!(formatter, "source worker failure: {}", self.context),
        }
    }
}

impl SourceWorkers {
    fn start(
        store_root: crate::paths::StoreRoot,
        chunk_size: NonZeroUsize,
        observer: Arc<dyn IngestObserver>,
    ) -> Result<Self> {
        let (jobs_tx, jobs_rx) = crossbeam_channel::bounded::<Work>(WORKER_QUEUE_CAPACITY);
        let (completed_tx, completed) =
            crossbeam_channel::bounded::<CompletedWork>(COMPLETION_QUEUE_CAPACITY);
        let (failure_tx, terminal_failures) = crossbeam_channel::bounded::<SourceWorkerFailure>(1);
        let mut joins = Vec::with_capacity(MAX_LIVE_SOURCE_WORKERS);
        let cancelled = Arc::new(AtomicBool::new(false));
        let completed_retained = Arc::new(AtomicUsize::new(0));
        let active_readers = Arc::new(Mutex::new(HashMap::<MountId, usize>::new()));
        for worker_number in 0..MAX_LIVE_SOURCE_WORKERS {
            let worker_jobs_rx = jobs_rx.clone();
            let completed_tx = completed_tx.clone();
            let root = store_root.clone();
            let cancelled = Arc::clone(&cancelled);
            let observer = Arc::clone(&observer);
            let failure_tx = failure_tx.clone();
            let active_readers = Arc::clone(&active_readers);
            let completed_retained = Arc::clone(&completed_retained);
            let spawned = thread::Builder::new()
                .name(format!("source-worker-{worker_number}"))
                .spawn(move || {
                    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let store = Store::new(root);
                        while let Ok(work) = worker_jobs_rx.recv() {
                            if cancelled.load(Ordering::Acquire) {
                                observer.observe(IngestEvent::WorkCancelled {
                                    sequence: work.sequence,
                                    source_path: work.candidate.relative_path,
                                    mount_id: work.candidate.mount_id,
                                });
                                continue;
                            }
                            let mount_id = work.candidate.mount_id;
                            observer.observe(IngestEvent::WorkDequeued {
                                sequence: work.sequence,
                                source_path: work.candidate.relative_path.clone(),
                                mount_id,
                            });
                            let active = increment_readers(&active_readers, mount_id)?;
                            observer.observe(IngestEvent::ActiveWorkers { mount_id, active });
                            observer.observe(IngestEvent::SourceReadStarted {
                                sequence: work.sequence,
                                source_path: work.candidate.relative_path.clone(),
                                mount_id,
                            });
                            let sequence = work.sequence;
                            let source_path = work.candidate.relative_path.clone();
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    crate::test_probe::pause_or_panic("source-worker-before-read")?;
                                    crate::test_probe::pause_or_fail("source-worker-before-read")?;
                                    store.ingest_file_with_read_observer(
                                        &work.candidate.absolute_path,
                                        work.candidate.size_bytes,
                                        chunk_size,
                                        |size_bytes| {
                                            observer.observe(IngestEvent::SourceReadCompleted {
                                                sequence,
                                                source_path,
                                                mount_id,
                                                size_bytes,
                                            });
                                        },
                                    )
                                }))
                                .unwrap_or_else(|payload| {
                                    Err(eyre!(
                                        "source worker panicked while reading {:?}: {}",
                                        work.candidate.absolute_path,
                                        panic_message(payload)
                                    ))
                                });
                            if let Err(error) = result {
                                let failure = SourceWorkerFailure {
                                    sequence: Some(work.sequence),
                                    source_path: Some(work.candidate.relative_path.clone()),
                                    mount_id: Some(mount_id),
                                    context: format!("{error:#}"),
                                };
                                observer.observe(IngestEvent::WorkFailed {
                                    sequence: work.sequence,
                                    source_path: work.candidate.relative_path.clone(),
                                    mount_id,
                                    context: failure.context.clone(),
                                });
                                // Publish cancellation before the terminal
                                // control signal and return instead of taking
                                // another job. The first failure is the one
                                // with stable source context.
                                signal_terminal_worker_failure(&cancelled, &failure_tx, failure)?;
                                // Tests can hold a worker after its terminal
                                // state is visible, proving the run lock waits
                                // for every active sibling during shutdown.
                                crate::test_probe::pause("source-worker-after-failure")?;
                                let active = decrement_readers(&active_readers, mount_id)?;
                                observer.observe(IngestEvent::ActiveWorkers { mount_id, active });
                                return Ok::<(), color_eyre::Report>(());
                            }
                            let active = decrement_readers(&active_readers, mount_id)?;
                            observer.observe(IngestEvent::ActiveWorkers { mount_id, active });
                            let mut completed = CompletedWork { work, result };
                            let retained = completed_retained.fetch_add(1, Ordering::AcqRel) + 1;
                            observer.observe(IngestEvent::QueueOccupancy {
                                stage: QueueStage::CompletedBlobs,
                                mount_id: None,
                                occupancy: retained,
                                capacity: COMPLETED_WORK_RETAINED_CAPACITY,
                            });
                            loop {
                                if cancelled.load(Ordering::Acquire) {
                                    completed_retained.fetch_sub(1, Ordering::AcqRel);
                                    observer.observe(IngestEvent::WorkCancelled {
                                        sequence: completed.work.sequence,
                                        source_path: completed.work.candidate.relative_path.clone(),
                                        mount_id,
                                    });
                                    break;
                                }
                                match completed_tx
                                    .send_timeout(completed, Duration::from_millis(10))
                                {
                                    Ok(()) => {
                                        break;
                                    }
                                    Err(crossbeam_channel::SendTimeoutError::Timeout(value)) => {
                                        completed = value
                                    }
                                    Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                                        completed_retained.fetch_sub(1, Ordering::AcqRel);
                                        return Ok::<(), color_eyre::Report>(());
                                    }
                                }
                            }
                        }
                        Ok::<(), color_eyre::Report>(())
                    }));
                    match run {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            let _ = signal_terminal_worker_failure(
                                &cancelled,
                                &failure_tx,
                                SourceWorkerFailure {
                                    sequence: None,
                                    source_path: None,
                                    mount_id: None,
                                    context: format!("source worker failed: {error:#}"),
                                },
                            );
                        }
                        Err(payload) => {
                            let _ = signal_terminal_worker_failure(
                                &cancelled,
                                &failure_tx,
                                SourceWorkerFailure {
                                    sequence: None,
                                    source_path: None,
                                    mount_id: None,
                                    context: format!(
                                        "source worker panicked: {}",
                                        panic_message(payload)
                                    ),
                                },
                            );
                        }
                    }
                });
            match spawned {
                Ok(join) => joins.push(join),
                Err(error) => {
                    drop(jobs_tx);
                    drop(jobs_rx);
                    let mut join_errors = Vec::new();
                    for join in joins {
                        if let Err(payload) = join.join() {
                            join_errors.push(panic_message(payload));
                        }
                    }
                    let detail = if join_errors.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "; already-started source workers also panicked: {}",
                            join_errors.join("; ")
                        )
                    };
                    return Err(error).wrap_err(format!("spawn source worker thread{detail}"));
                }
            }
        }
        Ok(Self {
            jobs: Some(jobs_tx),
            completed,
            joins,
            cancelled,
            terminal_failures,
            completed_retained,
            observer,
        })
    }
    fn try_submit(&self, work: Work) -> Result<Option<Work>> {
        let jobs = self.jobs.as_ref().expect("source workers live");
        match jobs.try_send(work) {
            Ok(()) => Ok(None),
            Err(crossbeam_channel::TrySendError::Full(work)) => Ok(Some(work)),
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                Err(eyre!("source worker queue closed while scheduling work"))
            }
        }
    }
    fn recv(&self, writer: &CatalogWriterHandle) -> Result<CompletedWork> {
        let catalog_terminal_failures = writer.terminal_failure_receiver();
        crossbeam_channel::select! {
            recv(self.completed) -> completed => {
                let completed = completed.wrap_err("wait for source worker completion")?;
                let retained = self.completed_retained.fetch_sub(1, Ordering::AcqRel) - 1;
                self.observer.observe(IngestEvent::QueueOccupancy {
                    stage: QueueStage::CompletedBlobs,
                    mount_id: None,
                    occupancy: retained,
                    capacity: COMPLETED_WORK_RETAINED_CAPACITY,
                });
                Ok(completed)
            },
            recv(self.terminal_failures) -> failure => Err(eyre!("{}", failure.map_err(|_| eyre!("source worker terminal failure channel closed"))?)),
            recv(catalog_terminal_failures) -> failure => {
                // The coordinator announces cancellation only after `recv`
                // returns its error. Set the shared cancellation state first,
                // so queued work cannot begin while that observer callback is
                // running (or before outer cleanup reaches `cancel`).
                self.cancel();
                Err(eyre!(
                    "catalog writer failed: {}",
                    failure.map_err(|_| eyre!("catalog writer terminal failure channel closed"))?
                ))
            },
        }
    }
    fn finish(&mut self) -> Result<()> {
        self.cancel();
        drop(self.jobs.take());
        let mut errors = Vec::new();
        for join in self.joins.drain(..) {
            if let Err(payload) = join.join() {
                errors.push(format!(
                    "source worker thread panicked: {}",
                    panic_message(payload)
                ));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(eyre!(errors.join("; ")))
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn try_terminal_failure(&self) -> Option<SourceWorkerFailure> {
        match self.terminal_failures.try_recv() {
            Ok(failure) => Some(failure),
            Err(crossbeam_channel::TryRecvError::Empty)
            | Err(crossbeam_channel::TryRecvError::Disconnected) => None,
        }
    }
}

fn signal_terminal_worker_failure(
    cancelled: &AtomicBool,
    failures: &Sender<SourceWorkerFailure>,
    failure: SourceWorkerFailure,
) -> Result<()> {
    if cancelled
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        failures
            .send(failure)
            .map_err(|_| eyre!("source worker terminal failure receiver closed"))?;
    }
    Ok(())
}

fn increment_readers(active: &Mutex<HashMap<MountId, usize>>, mount_id: MountId) -> Result<usize> {
    let mut active = active
        .lock()
        .map_err(|_| eyre!("source reader accounting mutex poisoned"))?;
    let count = active.entry(mount_id).or_default();
    *count = count
        .checked_add(1)
        .ok_or_else(|| eyre!("active source reader counter overflow"))?;
    Ok(*count)
}

fn decrement_readers(active: &Mutex<HashMap<MountId, usize>>, mount_id: MountId) -> Result<usize> {
    let mut active = active
        .lock()
        .map_err(|_| eyre!("source reader accounting mutex poisoned"))?;
    let count = active
        .get_mut(&mount_id)
        .ok_or_else(|| eyre!("source reader finished without start accounting"))?;
    *count = count
        .checked_sub(1)
        .ok_or_else(|| eyre!("active source reader counter underflow"))?;
    let now = *count;
    if now == 0 {
        active.remove(&mount_id);
    }
    Ok(now)
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

fn drain_outcomes_to_limit(
    tickets: &mut BTreeMap<u64, PendingTicket>,
    report: &mut ImportReport,
    max: usize,
    _writer: &CatalogWriterHandle,
    observer: &dyn IngestObserver,
) -> Result<()> {
    while tickets.len() >= max {
        let (sequence, ticket) = tickets.pop_first().expect("non-empty tickets");
        let source_path = ticket.source_path.clone();
        let outcome = resolve_ticket(ticket, tickets)?;
        observer.observe(IngestEvent::CatalogCommitted {
            sequence,
            source_path,
        });
        apply_outcome(report, outcome);
    }
    Ok(())
}
fn apply_outcome(report: &mut ImportReport, outcome: ImportWriteOutcome) {
    if let ImportWriteOutcome::Imported(_, source) = outcome {
        report_source_outcome(report, &source);
    }
}

/// A catalog ticket is the durable boundary for one discovered source. Keep
/// that identity on every asynchronous writer error: the writer may batch
/// records, but callers must not lose the affected sequence/path.
fn resolve_ticket(
    ticket: PendingTicket,
    pending: &BTreeMap<u64, PendingTicket>,
) -> Result<ImportWriteOutcome> {
    let sequence = ticket.sequence;
    let source_path = ticket.source_path;
    ticket.ticket.resolve_detailed().map_err(|error| {
        let failed_sequence = error.sequence;
        let failed_path = if error.sequence == sequence {
            source_path.clone()
        } else {
            pending
                .get(&error.sequence)
                .map(|pending| pending.source_path.clone())
                .unwrap_or_else(|| source_path.clone())
        };
        eyre!(error).wrap_err(format!(
            "resolve catalog outcome for sequence {} source {:?} (awaiting sequence {} source {:?})",
            failed_sequence, failed_path, sequence, source_path
        ))
    })
}

fn dry_run_import(config: ImportConfig, observer: &dyn IngestObserver) -> Result<ImportReport> {
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let store = Store::new(config.store_root);
        let source_root = config.source_root.durable_text()?;
        let catalog = ReadOnlyCatalog::open_if_exists(&config.db_path)?;
        let mut report = ImportReport {
            dry_run: true,
            ..ImportReport::default()
        };
        let mut sequence = 0_u64;
        for candidate in scan_source(&config.source_root) {
            let candidate = candidate?;
            observer.observe(IngestEvent::CandidateDiscovered {
                sequence,
                source_path: candidate.relative_path.clone(),
                mount_id: candidate.mount_id,
            });
            report_observed_file(&mut report, &candidate);
            let known = match catalog.as_ref() {
                Some(catalog) => {
                    catalog.known_source_file(&source_root, &candidate.relative_path)?
                }
                None => None,
            };
            match validated_disposition(
                &store,
                classify_import(config.metadata_skip, &candidate, known.as_ref()),
            )? {
                ImportDisposition::Skip { size_bytes, .. } => {
                    observer.observe(IngestEvent::MetadataSkipped {
                        sequence,
                        source_path: candidate.relative_path.clone(),
                        size_bytes,
                    });
                    report.files_skipped += 1;
                    report.bytes_skipped += size_bytes;
                    report.blobs_reused += 1;
                    report.source_records_updated += 1;
                }
                ImportDisposition::Hash { .. } => {
                    observer.observe(IngestEvent::WorkQueued {
                        sequence,
                        source_path: candidate.relative_path.clone(),
                        mount_id: candidate.mount_id,
                    });
                    observer.observe(IngestEvent::WorkDequeued {
                        sequence,
                        source_path: candidate.relative_path.clone(),
                        mount_id: candidate.mount_id,
                    });
                    observer.observe(IngestEvent::SourceReadStarted {
                        sequence,
                        source_path: candidate.relative_path.clone(),
                        mount_id: candidate.mount_id,
                    });
                    let blob =
                        store.hash_file_read_only(&candidate.absolute_path, config.chunk_size)?;
                    observer.observe(IngestEvent::SourceReadCompleted {
                        sequence,
                        source_path: candidate.relative_path.clone(),
                        mount_id: candidate.mount_id,
                        size_bytes: blob.size_bytes,
                    });
                    report.files_hashed += 1;
                    report.bytes_hashed += blob.size_bytes;
                    report_store_outcome(&mut report, &blob.outcome, blob.size_bytes);
                    report_source_outcome(
                        &mut report,
                        &catalog
                            .as_ref()
                            .map(|c| {
                                c.source_observation_outcome(&source_root, &candidate.relative_path)
                            })
                            .transpose()?
                            .unwrap_or(SourceObservationOutcome::Inserted),
                    );
                }
            }
            sequence = next_sequence(sequence)?;
        }
        Ok(report)
    }))
    .unwrap_or_else(|payload| {
        Err(eyre!(
            "dry-run import coordinator panicked: {}",
            panic_message(payload)
        ))
    });
    if run.is_err() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observer.observe(IngestEvent::Cancellation);
        }));
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        observer.observe(IngestEvent::Shutdown);
    }));
    run
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
fn report_store_outcome(report: &mut ImportReport, outcome: &StoreOutcome, size: u64) {
    match outcome {
        StoreOutcome::Created => {
            report.blobs_created += 1;
            report.bytes_written += size;
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
    use crate::config::{DEFAULT_CHUNK_SIZE, ImportOptions};
    use crate::paths::{BlobHash, SourceRelativePath};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::sync::mpsc;

    static TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct RecordingObserver(Mutex<Vec<IngestEvent>>);

    impl RecordingObserver {
        fn events(&self) -> Vec<IngestEvent> {
            self.0.lock().expect("recording observer lock").clone()
        }

        fn maximum_readers(&self, mount: MountId) -> usize {
            self.events()
                .into_iter()
                .filter_map(|event| match event {
                    IngestEvent::ActiveWorkers { mount_id, active } if mount_id == mount => {
                        Some(active)
                    }
                    _ => None,
                })
                .max()
                .unwrap_or_default()
        }
    }

    impl IngestObserver for RecordingObserver {
        fn observe(&self, event: IngestEvent) {
            self.0.lock().expect("recording observer lock").push(event);
        }
    }

    #[test]
    fn ingest_event_debug_redacts_source_content_and_failure_context() {
        let event = IngestEvent::WorkFailed {
            sequence: 9,
            source_path: SourceRelativePath::from_catalog_text("private/holiday.jpg")
                .expect("relative path"),
            mount_id: MountId::for_test(42),
            context: "secret source bytes".to_owned(),
        };
        let rendered = format!("{event:?}");
        assert!(rendered.contains("WorkFailed"));
        assert!(rendered.contains("sequence: 9"));
        assert!(rendered.contains("MountId(<redacted>)"));
        assert!(!rendered.contains("holiday.jpg"));
        assert!(!rendered.contains("secret source bytes"));
        assert!(!rendered.contains("42"));
    }

    /// A test-only completion gate.  It blocks at the actual source-read
    /// boundary, so tests can prove parallel admission with injected mount
    /// identities without depending on physical disks or scheduler timing.
    struct BarrierObserver {
        recorder: RecordingObserver,
        started: std::sync::Barrier,
    }

    impl BarrierObserver {
        fn new(readers: usize) -> Self {
            Self {
                recorder: RecordingObserver::default(),
                started: std::sync::Barrier::new(readers),
            }
        }
    }

    impl IngestObserver for BarrierObserver {
        fn observe(&self, event: IngestEvent) {
            if matches!(event, IngestEvent::SourceReadStarted { .. }) {
                self.started.wait();
            }
            self.recorder.observe(event);
        }
    }

    /// Forces sequence zero to finish after sequence one at the actual read
    /// boundary. The two barriers make the ordering a synchronization fact,
    /// not a scheduler-timing assumption.
    struct CompletionOrderObserver {
        recorder: RecordingObserver,
        first_started: std::sync::Barrier,
        release_first: std::sync::Barrier,
        second_completed: mpsc::Sender<()>,
    }

    impl IngestObserver for CompletionOrderObserver {
        fn observe(&self, event: IngestEvent) {
            match &event {
                IngestEvent::SourceReadStarted { sequence: 0, .. } => {
                    self.first_started.wait();
                    self.release_first.wait();
                }
                IngestEvent::SourceReadCompleted { sequence: 1, .. } => {
                    let _ = self.second_completed.send(());
                }
                _ => {}
            }
            self.recorder.observe(event);
        }
    }

    /// Coordinates a failed source read with the coordinator's scanner loop.
    /// The barriers make the assertion below independent of worker scheduling:
    /// candidates are pending, the coordinator is blocked after discovery, and
    /// only then is the failed read allowed to publish terminal cancellation.
    struct TerminalFailureObserver {
        recorder: RecordingObserver,
        worker_started: std::sync::Barrier,
        release_worker: std::sync::Barrier,
        coordinator_blocked: std::sync::Barrier,
        release_coordinator: std::sync::Barrier,
        worker_failed: mpsc::Sender<()>,
    }

    /// Holds one admitted reader while the catalog writer terminates. This
    /// makes the coordinator's blocking completion wait observable: terminal
    /// cancellation must arrive before that reader is released.
    struct CatalogTerminationObserver {
        recorder: RecordingObserver,
        blocked_reader_started: std::sync::Barrier,
        release_blocked_reader: std::sync::Barrier,
        cancellation_started: std::sync::Barrier,
        release_cancellation: std::sync::Barrier,
        active_reader_completed: mpsc::Sender<()>,
    }

    /// Holds the cancellation callback while the admitted readers are released.
    /// The ninth candidate is queued behind eight readers, so this makes it
    /// deterministic that cancellation must be visible before workers are
    /// allowed to take another job.
    struct BlockingScannerCancellationObserver {
        recorder: RecordingObserver,
        reader_admission: Arc<std::sync::Barrier>,
        reader_started: mpsc::Sender<()>,
        release_reader: Mutex<mpsc::Receiver<()>>,
        ninth_candidate_discovered: mpsc::Sender<()>,
        admitted_cancelled: mpsc::Sender<()>,
        cancellation_started: mpsc::Sender<()>,
        release_cancellation: Mutex<mpsc::Receiver<()>>,
    }

    impl IngestObserver for BlockingScannerCancellationObserver {
        fn observe(&self, event: IngestEvent) {
            match event {
                IngestEvent::SourceReadStarted { sequence, .. }
                    if sequence < MAX_LIVE_SOURCE_WORKERS as u64 =>
                {
                    self.reader_admission.wait();
                    let _ = self.reader_started.send(());
                    self.release_reader
                        .lock()
                        .expect("reader release receiver")
                        .recv()
                        .expect("release admitted reader");
                }
                IngestEvent::Cancellation => {
                    let _ = self.cancellation_started.send(());
                    self.release_cancellation
                        .lock()
                        .expect("cancellation release receiver")
                        .recv()
                        .expect("release cancellation observer");
                }
                IngestEvent::CandidateDiscovered { sequence, .. }
                    if sequence == MAX_LIVE_SOURCE_WORKERS as u64 =>
                {
                    let _ = self.ninth_candidate_discovered.send(());
                }
                IngestEvent::WorkCancelled { sequence, .. }
                    if sequence < MAX_LIVE_SOURCE_WORKERS as u64 =>
                {
                    let _ = self.admitted_cancelled.send(());
                }
                _ => {}
            }
            self.recorder.observe(event);
        }
    }

    /// Blocks the eight live source workers so a high requested per-mount
    /// value must fill only the documented scheduler backlog plus the capped
    /// worker admission budget.
    struct HighNAdmissionObserver {
        recorder: RecordingObserver,
        mount_id: MountId,
        reader_admission: Arc<std::sync::Barrier>,
        reader_started: mpsc::Sender<()>,
        release_reader: Mutex<mpsc::Receiver<()>>,
        saturated: mpsc::Sender<()>,
        gated_readers: AtomicUsize,
        saturation_sent: AtomicBool,
    }

    impl IngestObserver for HighNAdmissionObserver {
        fn observe(&self, event: IngestEvent) {
            match &event {
                IngestEvent::SourceReadStarted { mount_id, .. } if *mount_id == self.mount_id => {
                    if self.gated_readers.fetch_add(1, Ordering::AcqRel) < MAX_LIVE_SOURCE_WORKERS {
                        let _ = self.reader_started.send(());
                        self.reader_admission.wait();
                        self.release_reader
                            .lock()
                            .expect("high-N reader release receiver")
                            .recv()
                            .expect("release high-N reader");
                    }
                }
                IngestEvent::QueueOccupancy {
                    stage: QueueStage::PerMountWork,
                    mount_id: Some(mount_id),
                    occupancy,
                    capacity,
                } if *mount_id == self.mount_id
                    && occupancy == capacity
                    && !self.saturation_sent.swap(true, Ordering::AcqRel) =>
                {
                    let _ = self.saturated.send(());
                }
                _ => {}
            }
            self.recorder.observe(event);
        }
    }

    /// Produces the scanner failure only after every live worker has reached
    /// the source-read observer. This avoids relying on scanner/worker timing
    /// when testing cancellation of already-queued work.
    struct GatedScannerFailure {
        candidates: std::vec::IntoIter<Result<SourceFileCandidate>>,
        reader_admission: Arc<std::sync::Barrier>,
        failure_emitted: bool,
    }

    impl Iterator for GatedScannerFailure {
        type Item = Result<SourceFileCandidate>;

        fn next(&mut self) -> Option<Self::Item> {
            if let Some(candidate) = self.candidates.next() {
                return Some(candidate);
            }
            if self.failure_emitted {
                return None;
            }
            self.failure_emitted = true;
            self.reader_admission.wait();
            Some(Err(eyre!("injected scanner failure")))
        }
    }

    impl IngestObserver for CatalogTerminationObserver {
        fn observe(&self, event: IngestEvent) {
            self.recorder.observe(event.clone());
            match event {
                IngestEvent::SourceReadStarted { sequence: 1, .. } => {
                    self.blocked_reader_started.wait();
                    self.release_blocked_reader.wait();
                }
                IngestEvent::WorkCancelled { sequence: 1, .. } => {
                    let _ = self.active_reader_completed.send(());
                }
                IngestEvent::Cancellation => {
                    self.cancellation_started.wait();
                    self.release_cancellation.wait();
                }
                _ => {}
            }
        }
    }

    impl IngestObserver for TerminalFailureObserver {
        fn observe(&self, event: IngestEvent) {
            self.recorder.observe(event.clone());
            match &event {
                IngestEvent::SourceReadStarted { sequence: 0, .. } => {
                    self.worker_started.wait();
                    self.release_worker.wait();
                }
                IngestEvent::CandidateDiscovered { sequence: 4, .. } => {
                    self.coordinator_blocked.wait();
                    self.release_coordinator.wait();
                }
                IngestEvent::WorkFailed { sequence: 0, .. } => {
                    let _ = self.worker_failed.send(());
                }
                _ => {}
            }
        }
    }

    struct SaturationObserver {
        recorder: RecordingObserver,
        mount_work_saturated: mpsc::Sender<()>,
        outcomes_saturated: mpsc::Sender<()>,
        completions_saturated: mpsc::Sender<()>,
        sent_mount_work: AtomicBool,
        sent_outcomes: AtomicBool,
        sent_completions: AtomicBool,
    }

    impl IngestObserver for SaturationObserver {
        fn observe(&self, event: IngestEvent) {
            match &event {
                IngestEvent::QueueOccupancy {
                    stage: QueueStage::PerMountWork,
                    occupancy,
                    capacity,
                    ..
                } if occupancy == capacity
                    && !self.sent_mount_work.swap(true, Ordering::AcqRel) =>
                {
                    let _ = self.mount_work_saturated.send(());
                }
                IngestEvent::QueueOccupancy {
                    stage: QueueStage::CatalogOutcomes,
                    occupancy: 32,
                    capacity: 32,
                    ..
                } if !self.sent_outcomes.swap(true, Ordering::AcqRel) => {
                    let _ = self.outcomes_saturated.send(());
                }
                IngestEvent::QueueOccupancy {
                    stage: QueueStage::CompletedBlobs,
                    occupancy,
                    capacity,
                    ..
                } if self.sent_outcomes.load(Ordering::Acquire)
                    && *occupancy > 0
                    && *capacity == COMPLETED_WORK_RETAINED_CAPACITY
                    && !self.sent_completions.swap(true, Ordering::AcqRel) =>
                {
                    let _ = self.completions_saturated.send(());
                }
                _ => {}
            }
            self.recorder.observe(event);
        }
    }

    struct ExecutorHarness {
        root: PathBuf,
        source: PathBuf,
        store: PathBuf,
    }

    impl ExecutorHarness {
        fn new() -> Self {
            let id = TEST_DIRECTORY.fetch_add(1, AtomicOrdering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "media-importer-ingest-test-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(root.join("source")).expect("create test source");
            Self {
                source: root.join("source"),
                store: root.join("store"),
                root,
            }
        }

        fn candidates(&self, files: &[(&str, &[u8], u64)]) -> Vec<Result<SourceFileCandidate>> {
            files
                .iter()
                .map(|(name, contents, mount)| {
                    let path = self.source.join(name);
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent).expect("create source parent");
                    }
                    fs::write(&path, contents).expect("write source fixture");
                    Ok(SourceFileCandidate {
                        absolute_path: path,
                        relative_path: SourceRelativePath::from_catalog_text(name)
                            .expect("relative path"),
                        size_bytes: contents.len() as u64,
                        modified_at_ms: Some(1),
                        mount_id: MountId::for_test(*mount),
                    })
                })
                .collect()
        }

        fn run<I>(
            &self,
            candidates: I,
            workers_per_mount: usize,
            observer: Arc<dyn IngestObserver>,
        ) -> Result<ImportReport>
        where
            I: IntoIterator<Item = Result<SourceFileCandidate>>,
        {
            self.run_with_writer_config(
                candidates,
                workers_per_mount,
                observer,
                CatalogWriterConfig::default(),
            )
        }

        fn run_default(
            &self,
            candidates: Vec<Result<SourceFileCandidate>>,
            observer: Arc<dyn IngestObserver>,
        ) -> Result<ImportReport> {
            self.run(
                candidates,
                crate::config::DEFAULT_WORKERS_PER_MOUNT.get(),
                observer,
            )
        }

        fn run_with_writer_config<I>(
            &self,
            candidates: I,
            workers_per_mount: usize,
            observer: Arc<dyn IngestObserver>,
            writer_config: CatalogWriterConfig,
        ) -> Result<ImportReport>
        where
            I: IntoIterator<Item = Result<SourceFileCandidate>>,
        {
            fs::create_dir_all(&self.store).expect("create store");
            let config = ImportConfig::from_options(ImportOptions {
                store: self.store.clone(),
                source: self.source.clone(),
                db: None,
                dry_run: false,
                metadata_skip: false,
                chunk_size: DEFAULT_CHUNK_SIZE,
                workers_per_mount: NonZeroUsize::new(workers_per_mount).expect("non-zero workers"),
            })?;
            let store = Store::new(config.store_root.clone());
            store.prepare_for_import()?;
            let writer = CatalogWriterHandle::spawn(config.db_path.clone(), writer_config)?;
            let result = execute_import(
                candidates.into_iter(),
                ExecutorContext {
                    config,
                    clock: &FixedClock(1),
                    store: &store,
                    catalog: None,
                    writer: &writer,
                    max_outstanding: 32,
                    observer,
                },
            );
            let finish = writer.finish();
            match (result, finish) {
                (Ok(report), Ok(_)) => Ok(report),
                (Ok(_), Err(error)) | (Err(error), Ok(_)) => Err(error),
                (Err(operation), Err(writer)) => Err(writer.wrap_err(format!(
                    "executor operation also failed while catalog writer terminated: {operation:#}"
                ))),
            }
        }
    }

    impl Drop for ExecutorHarness {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct FixedClock(i64);
    impl Clock for FixedClock {
        fn now_ms(&self) -> i64 {
            self.0
        }
    }

    fn candidate(size_bytes: u64, modified_at_ms: Option<i64>) -> SourceFileCandidate {
        SourceFileCandidate {
            absolute_path: PathBuf::from("source-file"),
            relative_path: SourceRelativePath::from_catalog_text("file").unwrap(),
            size_bytes,
            modified_at_ms,
            mount_id: MountId::for_test(1),
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
    }

    #[test]
    fn default_configuration_and_configured_n_are_enforced_per_injected_mount() {
        let harness = ExecutorHarness::new();
        let observer = Arc::new(RecordingObserver::default());
        harness
            .run_default(
                harness.candidates(&[("a", b"a", 7), ("b", b"b", 7), ("c", b"c", 7)]),
                observer.clone(),
            )
            .expect("default import");
        assert!(observer.maximum_readers(MountId::for_test(7)) <= 1);

        let harness = ExecutorHarness::new();
        let observer = Arc::new(RecordingObserver::default());
        harness
            .run(
                harness.candidates(&[
                    ("a", b"a", 7),
                    ("b", b"b", 7),
                    ("c", b"c", 7),
                    ("d", b"d", 7),
                ]),
                3,
                observer.clone(),
            )
            .expect("parallel import");
        assert!(observer.maximum_readers(MountId::for_test(7)) <= 3);
    }

    #[test]
    fn independent_and_nested_injected_mounts_are_classified_and_make_progress() {
        let harness = ExecutorHarness::new();
        let observer = Arc::new(RecordingObserver::default());
        harness
            .run(
                harness.candidates(&[("top/a", b"alpha", 11), ("nested/b", b"beta", 22)]),
                1,
                observer.clone(),
            )
            .expect("mount-aware import");
        let discovered: Vec<_> = observer
            .events()
            .into_iter()
            .filter_map(|event| match event {
                IngestEvent::CandidateDiscovered { mount_id, .. } => Some(mount_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            discovered,
            vec![MountId::for_test(11), MountId::for_test(22)]
        );
        assert!(observer.maximum_readers(MountId::for_test(11)) >= 1);
        assert!(observer.maximum_readers(MountId::for_test(22)) >= 1);
    }

    #[test]
    fn barrier_harness_proves_parallel_read_admission_for_independent_mounts_and_n() {
        let harness = ExecutorHarness::new();
        let observer = Arc::new(BarrierObserver::new(2));
        harness
            .run(
                harness.candidates(&[("a", b"alpha", 31), ("b", b"beta", 32)]),
                1,
                observer.clone(),
            )
            .expect("independent mounts meet read barrier");
        assert!(observer.recorder.maximum_readers(MountId::for_test(31)) >= 1);
        assert!(observer.recorder.maximum_readers(MountId::for_test(32)) >= 1);

        let harness = ExecutorHarness::new();
        let observer = Arc::new(BarrierObserver::new(2));
        harness
            .run(
                harness.candidates(&[("a", b"alpha", 33), ("b", b"beta", 33)]),
                2,
                observer.clone(),
            )
            .expect("two permits meet read barrier");
        assert_eq!(observer.recorder.maximum_readers(MountId::for_test(33)), 2);
    }

    #[test]
    fn exact_one_pass_read_events_are_emitted_before_post_read_failure_and_staging_is_cleaned() {
        let harness = ExecutorHarness::new();
        let observer = Arc::new(RecordingObserver::default());
        let mut candidates = harness.candidates(&[("bad", b"bytes", 4)]);
        candidates[0].as_mut().expect("candidate").size_bytes = 99;
        assert!(harness.run(candidates, 1, observer.clone()).is_err());
        let completions: Vec<_> = observer
            .events()
            .into_iter()
            .filter_map(|event| match event {
                IngestEvent::SourceReadCompleted { size_bytes, .. } => Some(size_bytes),
                _ => None,
            })
            .collect();
        assert_eq!(
            completions,
            vec![5],
            "source bytes are emitted at the read boundary"
        );
        assert_eq!(
            fs::read_dir(harness.store.join("staging"))
                .expect("staging directory")
                .count(),
            0,
            "post-read failure removes staging files"
        );
    }

    #[test]
    fn parallel_same_content_deduplicates_and_queue_high_water_never_exceeds_documented_bounds() {
        let harness = ExecutorHarness::new();
        let observer = Arc::new(RecordingObserver::default());
        let report = harness
            .run(
                harness.candidates(&[("a", b"same", 1), ("b", b"same", 2), ("c", b"other", 3)]),
                2,
                observer.clone(),
            )
            .expect("deduplicating import");
        assert_eq!(report.files_hashed, 3);
        assert_eq!(report.blobs_created, 2);
        assert_eq!(report.blobs_reused, 1);
        assert!(observer.events().into_iter().all(|event| match event {
            IngestEvent::QueueOccupancy {
                occupancy,
                capacity,
                ..
            } => occupancy <= capacity,
            _ => true,
        }));
        assert_eq!(
            report.source_records_inserted, 3,
            "one catalog row per source"
        );
    }

    #[test]
    fn opposite_discovery_and_completion_orders_have_identical_durable_outcomes() {
        let first = ExecutorHarness::new();
        let second = ExecutorHarness::new();
        let first_report = first
            .run(
                first.candidates(&[("a", b"same", 1), ("b", b"other", 2), ("c", b"same", 3)]),
                2,
                Arc::new(RecordingObserver::default()),
            )
            .expect("first import");
        let candidates =
            second.candidates(&[("c", b"same", 3), ("b", b"other", 2), ("a", b"same", 1)]);
        let (second_completed_tx, second_completed_rx) = mpsc::channel();
        let observer = Arc::new(CompletionOrderObserver {
            recorder: RecordingObserver::default(),
            first_started: std::sync::Barrier::new(2),
            release_first: std::sync::Barrier::new(2),
            second_completed: second_completed_tx,
        });
        let run_observer = Arc::clone(&observer);
        let second_ref = &second;
        let second_report = thread::scope(|scope| {
            let runner = scope.spawn(move || second_ref.run(candidates, 2, run_observer));
            observer.first_started.wait();
            second_completed_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("second source read completes while first is held");
            observer.release_first.wait();
            runner
                .join()
                .expect("opposite completion worker join")
                .expect("opposite discovery import")
        });
        let completion_order: Vec<_> = observer
            .recorder
            .events()
            .into_iter()
            .filter_map(|event| match event {
                IngestEvent::SourceReadCompleted { sequence, .. } => Some(sequence),
                _ => None,
            })
            .collect();
        let sequence_one = completion_order
            .iter()
            .position(|sequence| *sequence == 1)
            .expect("sequence one completion");
        let sequence_zero = completion_order
            .iter()
            .position(|sequence| *sequence == 0)
            .expect("sequence zero completion");
        assert!(
            sequence_one < sequence_zero,
            "barrier must force sequence one to complete before sequence zero: {completion_order:?}"
        );
        assert_eq!(first_report, second_report);
        let mut snapshots = Vec::new();
        for harness in [&first, &second] {
            let (rows, blobs) = crate::catalog::test_support::source_blob_snapshot(
                &harness.store.join("catalog.sqlite"),
            )
            .expect("catalog snapshot");
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0].1, rows[2].1, "same content shares a blob");
            assert_eq!(blobs, 2);
            snapshots.push(rows);
        }
        assert_eq!(snapshots[0], snapshots[1]);
    }

    #[test]
    fn blocked_writer_keeps_all_five_documented_high_water_bounds() {
        let harness = ExecutorHarness::new();
        let mut candidates = Vec::new();
        // One mount deterministically fills its 64-entry scheduler backlog
        // plus all eight worker admissions. Extra files ensure the blocked
        // catalog writer applies backpressure while source completions remain
        // bounded in flight.
        for number in 0..96 {
            let name = format!("file-{number}");
            let path = harness.source.join(&name);
            fs::write(&path, b"content").expect("write source");
            candidates.push(Ok(SourceFileCandidate {
                absolute_path: path,
                relative_path: SourceRelativePath::from_catalog_text(&name).expect("relative"),
                size_bytes: 7,
                modified_at_ms: Some(1),
                mount_id: MountId::for_test(1),
            }));
        }
        let (mount_work_saturated_tx, mount_work_saturated_rx) = mpsc::channel();
        let (outcomes_saturated_tx, outcomes_saturated_rx) = mpsc::channel();
        let (completions_saturated_tx, completions_saturated_rx) = mpsc::channel();
        let observer = Arc::new(SaturationObserver {
            recorder: RecordingObserver::default(),
            mount_work_saturated: mount_work_saturated_tx,
            outcomes_saturated: outcomes_saturated_tx,
            completions_saturated: completions_saturated_tx,
            sent_mount_work: AtomicBool::new(false),
            sent_outcomes: AtomicBool::new(false),
            sent_completions: AtomicBool::new(false),
        });
        let writer_entered = Arc::new(std::sync::Barrier::new(2));
        let writer_release = Arc::new(std::sync::Barrier::new(2));
        let entered = Arc::clone(&writer_entered);
        let release = Arc::clone(&writer_release);
        let first_receipt = Arc::new(AtomicBool::new(true));
        let receipt = Arc::clone(&first_receipt);
        let writer_config =
            CatalogWriterConfig::default().with_batch_receipt_hook(Arc::new(move || {
                if receipt.swap(false, Ordering::AcqRel) {
                    entered.wait();
                    release.wait();
                }
            }));
        let run_observer = observer.clone();
        let run = thread::spawn(move || {
            harness.run_with_writer_config(
                candidates,
                MAX_LIVE_SOURCE_WORKERS,
                run_observer,
                writer_config,
            )
        });
        mount_work_saturated_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("single mount saturates queued and admitted work accounting");
        writer_entered.wait();
        outcomes_saturated_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("blocked writer saturates pending catalog outcomes");
        completions_saturated_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("blocked coordinator retains completed work within its bound");
        let events = observer.recorder.events();
        for stage in [
            QueueStage::ScannerToScheduler,
            QueueStage::PerMountWork,
            QueueStage::CompletedBlobs,
            QueueStage::CatalogRequests,
            QueueStage::CatalogOutcomes,
        ] {
            assert!(events.iter().any(|event| matches!(event, IngestEvent::QueueOccupancy { stage: observed, .. } if *observed == stage)));
        }
        assert!(events.iter().all(|event| match event {
            IngestEvent::QueueOccupancy {
                occupancy,
                capacity,
                ..
            } => occupancy <= capacity,
            _ => true,
        }));
        writer_release.wait();
        run.join()
            .expect("executor thread join")
            .expect("complete import");
    }

    #[test]
    fn high_requested_per_mount_limit_is_capped_to_live_worker_admission_bound() {
        let harness = ExecutorHarness::new();
        let mount_id = MountId::for_test(91);
        let mut candidates = Vec::new();
        for number in 0..(SCHEDULER_QUEUE_CAPACITY + MAX_LIVE_SOURCE_WORKERS) {
            let name = format!("high-n-{number}");
            let path = harness.source.join(&name);
            fs::write(&path, b"content").expect("write high-N source fixture");
            candidates.push(Ok(SourceFileCandidate {
                absolute_path: path,
                relative_path: SourceRelativePath::from_catalog_text(&name)
                    .expect("high-N relative path"),
                size_bytes: 7,
                modified_at_ms: Some(1),
                mount_id,
            }));
        }
        let (reader_started_tx, reader_started_rx) = mpsc::channel();
        let (release_reader_tx, release_reader_rx) = mpsc::channel();
        let (saturated_tx, saturated_rx) = mpsc::channel();
        let reader_admission = Arc::new(std::sync::Barrier::new(MAX_LIVE_SOURCE_WORKERS));
        let observer = Arc::new(HighNAdmissionObserver {
            recorder: RecordingObserver::default(),
            mount_id,
            reader_admission: Arc::clone(&reader_admission),
            reader_started: reader_started_tx,
            release_reader: Mutex::new(release_reader_rx),
            saturated: saturated_tx,
            gated_readers: AtomicUsize::new(0),
            saturation_sent: AtomicBool::new(false),
        });
        let run_observer = Arc::clone(&observer);
        let requested_limit = MAX_LIVE_SOURCE_WORKERS * 4;
        let run = thread::spawn(move || harness.run(candidates, requested_limit, run_observer));
        for _ in 0..MAX_LIVE_SOURCE_WORKERS {
            reader_started_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("all capped reader admissions reach the deterministic gate");
        }
        saturated_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("high-N request saturates the capped per-mount footprint");
        let events = observer.recorder.events();
        let expected_capacity = SCHEDULER_QUEUE_CAPACITY + MAX_LIVE_SOURCE_WORKERS;
        assert!(events.iter().any(|event| matches!(
            event,
            IngestEvent::QueueOccupancy {
                stage: QueueStage::PerMountWork,
                mount_id: Some(observed_mount),
                occupancy,
                capacity,
            } if *observed_mount == mount_id
                && *occupancy == expected_capacity
                && *capacity == expected_capacity
        )));
        assert!(events.iter().all(|event| match event {
            IngestEvent::QueueOccupancy {
                stage: QueueStage::PerMountWork,
                mount_id: Some(observed_mount),
                occupancy,
                capacity,
            } if *observed_mount == mount_id => occupancy <= capacity,
            _ => true,
        }));
        assert!(observer.recorder.maximum_readers(mount_id) <= MAX_LIVE_SOURCE_WORKERS);
        for _ in 0..MAX_LIVE_SOURCE_WORKERS {
            release_reader_tx
                .send(())
                .expect("release capped high-N reader");
        }
        run.join()
            .expect("high-N executor thread join")
            .expect("complete high-N import");
    }

    #[test]
    fn scanner_and_worker_failures_cancel_join_and_allow_clean_rerun() {
        let harness = ExecutorHarness::new();
        let observer = Arc::new(RecordingObserver::default());
        let mut candidates = harness.candidates(&[("a", b"a", 1)]);
        candidates.push(Err(eyre!("injected scanner failure")));
        assert!(harness.run(candidates, 1, observer.clone()).is_err());
        assert!(observer.events().contains(&IngestEvent::Cancellation));
        assert!(observer.events().contains(&IngestEvent::Shutdown));
        let observer = Arc::new(RecordingObserver::default());
        harness
            .run(harness.candidates(&[("a", b"a", 1)]), 1, observer)
            .expect("rerun after cancellation");
    }

    #[test]
    fn scanner_failure_cancels_before_a_blocking_observer_can_release_queued_work() {
        let harness = ExecutorHarness::new();
        let mut candidates = Vec::new();
        for sequence in 0..=MAX_LIVE_SOURCE_WORKERS {
            let name = format!("scanner-{sequence}");
            let path = harness.source.join(&name);
            fs::write(&path, b"scanner failure fixture").expect("write scanner fixture");
            candidates.push(Ok(SourceFileCandidate {
                absolute_path: path,
                relative_path: SourceRelativePath::from_catalog_text(&name)
                    .expect("scanner relative path"),
                size_bytes: 23,
                modified_at_ms: Some(1),
                // The ninth candidate is eligible despite the first mount
                // being saturated, so it is discovered before scanner failure.
                mount_id: MountId::for_test(if sequence == MAX_LIVE_SOURCE_WORKERS {
                    2
                } else {
                    1
                }),
            }));
        }
        let reader_admission = Arc::new(std::sync::Barrier::new(MAX_LIVE_SOURCE_WORKERS + 1));
        let (reader_started_tx, reader_started_rx) = mpsc::channel();
        let (release_reader_tx, release_reader_rx) = mpsc::channel();
        let (ninth_candidate_discovered_tx, ninth_candidate_discovered_rx) = mpsc::channel();
        let (admitted_cancelled_tx, admitted_cancelled_rx) = mpsc::channel();
        let (cancellation_started_tx, cancellation_started_rx) = mpsc::channel();
        let (release_cancellation_tx, release_cancellation_rx) = mpsc::channel();
        let observer = Arc::new(BlockingScannerCancellationObserver {
            recorder: RecordingObserver::default(),
            reader_admission: Arc::clone(&reader_admission),
            reader_started: reader_started_tx,
            release_reader: Mutex::new(release_reader_rx),
            ninth_candidate_discovered: ninth_candidate_discovered_tx,
            admitted_cancelled: admitted_cancelled_tx,
            cancellation_started: cancellation_started_tx,
            release_cancellation: Mutex::new(release_cancellation_rx),
        });
        let run_observer = Arc::clone(&observer);
        let harness_ref = &harness;
        let result = thread::scope(|scope| {
            let scanner = GatedScannerFailure {
                candidates: candidates.into_iter(),
                reader_admission,
                failure_emitted: false,
            };
            let runner = scope.spawn(move || harness_ref.run(scanner, 8, run_observer));
            for _ in 0..MAX_LIVE_SOURCE_WORKERS {
                reader_started_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("all admitted readers reach the deterministic gate");
            }
            ninth_candidate_discovered_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("ninth candidate is discovered before scanner failure is emitted");
            cancellation_started_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("scanner failure reaches the blocking cancellation observer");
            for _ in 0..MAX_LIVE_SOURCE_WORKERS {
                release_reader_tx.send(()).expect("release admitted reader");
            }
            for _ in 0..MAX_LIVE_SOURCE_WORKERS {
                admitted_cancelled_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("admitted reader finishes after cancellation");
            }
            // Cancellation remains blocked while workers become available. A
            // ninth candidate is either pending in the scheduler or admitted
            // to the shared worker queue, but must never dequeue, read, stage,
            // or install into CAS.
            assert_eq!(
                blob_file_count(&harness.store),
                1,
                "only admitted same-content readers may reach CAS"
            );
            assert_eq!(
                fs::read_dir(harness.store.join("staging"))
                    .expect("read staging during blocked cancellation")
                    .count(),
                0,
                "completed admitted readers leave no staging artifacts"
            );
            release_cancellation_tx
                .send(())
                .expect("release cancellation observer");
            runner.join().expect("scanner failure executor join")
        });
        assert!(result.is_err(), "scanner failure must fail import");
        let events = observer.recorder.events();
        let queued_sequence = MAX_LIVE_SOURCE_WORKERS as u64;
        assert!(events.iter().any(|event| matches!(
            event,
            IngestEvent::CandidateDiscovered { sequence, .. } if *sequence == queued_sequence
        )));
        assert!(
            !events.iter().any(|event| matches!(
                event,
                IngestEvent::WorkDequeued { sequence, .. }
                    | IngestEvent::SourceReadStarted { sequence, .. }
                    | IngestEvent::SourceReadCompleted { sequence, .. }
                    | IngestEvent::CasStored { sequence, .. }
                    if *sequence == queued_sequence
            )),
            "discovered scanner work must not start while cancellation observer is blocked: {events:?}"
        );
        assert_eq!(
            fs::read_dir(harness.store.join("staging"))
                .expect("read staging after joined scanner failure")
                .count(),
            0,
            "joined scanner failure leaves staging empty"
        );
        harness
            .run(
                harness.candidates(&[("rerun", b"clean", 1)]),
                1,
                Arc::new(RecordingObserver::default()),
            )
            .expect("clean rerun after scanner cancellation");
    }

    #[test]
    fn terminal_worker_failure_stops_pending_candidate_consumption_and_new_source_reads() {
        let harness = ExecutorHarness::new();
        let mut candidates = vec![Ok(SourceFileCandidate {
            absolute_path: harness.source.join("missing.txt"),
            relative_path: SourceRelativePath::from_catalog_text("missing.txt")
                .expect("relative path"),
            size_bytes: 7,
            modified_at_ms: Some(1),
            mount_id: MountId::for_test(1),
        })];
        candidates.extend(harness.candidates(&[
            ("pending-1", b"one", 1),
            ("pending-2", b"two", 1),
            ("pending-3", b"three", 1),
            ("pending-4", b"four", 1),
            ("must-not-consume", b"five", 1),
        ]));
        let (failed_tx, failed_rx) = mpsc::channel();
        let observer = Arc::new(TerminalFailureObserver {
            recorder: RecordingObserver::default(),
            worker_started: std::sync::Barrier::new(2),
            release_worker: std::sync::Barrier::new(2),
            coordinator_blocked: std::sync::Barrier::new(2),
            release_coordinator: std::sync::Barrier::new(2),
            worker_failed: failed_tx,
        });
        let run_observer = Arc::clone(&observer);
        let harness_ref = &harness;
        let result = thread::scope(|scope| {
            let runner = scope.spawn(move || harness_ref.run(candidates, 1, run_observer));
            observer.worker_started.wait();
            observer.coordinator_blocked.wait();
            observer.release_worker.wait();
            failed_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("failed source read publishes terminal control before coordinator resumes");
            observer.release_coordinator.wait();
            runner.join().expect("import thread join")
        });
        let error = result.expect_err("missing source must fail import");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("missing.txt"),
            "source context: {rendered}"
        );
        let events = observer.recorder.events();
        let failure = events
            .iter()
            .position(|event| matches!(event, IngestEvent::WorkFailed { sequence: 0, .. }))
            .expect("failed worker event");
        assert!(
            events[..failure]
                .iter()
                .any(|event| matches!(event, IngestEvent::CandidateDiscovered { sequence: 4, .. })),
            "test must establish pending candidates before release"
        );
        assert!(
            !events[failure + 1..].iter().any(|event| matches!(
                event,
                IngestEvent::CandidateDiscovered { .. }
                    | IngestEvent::WorkDequeued { .. }
                    | IngestEvent::SourceReadStarted { .. }
            )),
            "terminal cancellation must stop scanner consumption and further source starts: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, IngestEvent::Shutdown)),
            "workers must join before returning"
        );
    }

    #[test]
    fn catalog_termination_cancels_queued_work_while_waiting_for_a_worker() {
        for panic_writer in [false, true] {
            let harness = ExecutorHarness::new();
            let mut candidates = harness.candidates(&[
                ("first-active", b"first active source", 1),
                ("held-active", b"held active source", 2),
            ]);
            for sequence in 0..78 {
                let name = format!("queued-{sequence}");
                let path = harness.source.join(&name);
                fs::write(&path, format!("queued source {sequence}")).expect("write source");
                candidates.push(Ok(SourceFileCandidate {
                    absolute_path: path,
                    relative_path: SourceRelativePath::from_catalog_text(&name)
                        .expect("relative path"),
                    size_bytes: (14 + sequence.to_string().len()) as u64,
                    modified_at_ms: Some(1),
                    // The queued candidates share the held reader's mount.
                    // A per-mount limit of one keeps them queued while the
                    // first mount supplies the catalog request that fails.
                    mount_id: MountId::for_test(2),
                }));
            }
            let (active_reader_completed_tx, active_reader_completed_rx) = mpsc::channel();
            let observer = Arc::new(CatalogTerminationObserver {
                recorder: RecordingObserver::default(),
                blocked_reader_started: std::sync::Barrier::new(2),
                release_blocked_reader: std::sync::Barrier::new(2),
                cancellation_started: std::sync::Barrier::new(2),
                release_cancellation: std::sync::Barrier::new(2),
                active_reader_completed: active_reader_completed_tx,
            });
            let writer_entered = Arc::new(std::sync::Barrier::new(2));
            let release_writer = Arc::new(std::sync::Barrier::new(2));
            let writer_entered_hook = Arc::clone(&writer_entered);
            let release_writer_hook = Arc::clone(&release_writer);
            let writer_hook = Arc::new(move || -> Result<()> {
                writer_entered_hook.wait();
                release_writer_hook.wait();
                if panic_writer {
                    panic!("injected catalog writer panic");
                }
                Err(eyre!("injected catalog writer failure"))
            });
            let config = CatalogWriterConfig::default().with_batch_terminal_hook(writer_hook);
            let run_observer = Arc::clone(&observer);
            let harness_ref = &harness;
            let result = thread::scope(|scope| {
                let runner = scope.spawn(move || {
                    harness_ref.run_with_writer_config(candidates, 1, run_observer, config)
                });
                observer.blocked_reader_started.wait();
                writer_entered.wait();
                release_writer.wait();
                // Keep the cancellation callback occupied. `SourceWorkers::recv`
                // must already have published cancellation, so no queued work
                // may start while this observer callback is held.
                observer.cancellation_started.wait();
                // Already-admitted work may finish safely after cancellation;
                // this reader cannot submit a catalog outcome but can finish
                // its source read and CAS mutation before worker shutdown.
                observer.release_blocked_reader.wait();
                active_reader_completed_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("active reader completes after catalog cancellation");
                let events = observer.recorder.events();
                let cancellation = events
                    .iter()
                    .position(|event| matches!(event, IngestEvent::Cancellation))
                    .expect("cancellation event");
                assert!(
                    !events[cancellation + 1..].iter().any(|event| matches!(
                        event,
                        IngestEvent::WorkDequeued { sequence, .. }
                            | IngestEvent::SourceReadStarted { sequence, .. }
                            | IngestEvent::SourceReadCompleted { sequence, .. }
                            | IngestEvent::CasStored { sequence, .. }
                            if *sequence >= 2
                    )),
                    "catalog cancellation admitted queued source I/O or CAS work: {events:?}"
                );
                assert!(
                    events.iter().any(|event| matches!(
                        event,
                        IngestEvent::SourceReadCompleted { sequence: 1, .. }
                    )),
                    "already-admitted reader must be allowed to complete"
                );
                assert!(
                    events.iter().any(|event| matches!(
                        event,
                        IngestEvent::WorkCancelled { sequence: 1, .. }
                    )),
                    "completed active reader must not submit work after cancellation"
                );
                assert_eq!(
                    blob_file_count(&harness.store),
                    2,
                    "only the first and already-active readers may reach CAS"
                );
                assert_eq!(
                    fs::read_dir(harness.store.join("staging"))
                        .expect("read staging directory while cancellation is held")
                        .count(),
                    0,
                    "queued work must not leave staging files"
                );
                observer.release_cancellation.wait();
                runner.join().expect("executor thread join")
            });
            let error = result.expect_err("catalog termination must fail import");
            let rendered = format!("{error:#}");
            assert!(
                rendered.contains("catalog writer"),
                "writer context: {rendered}"
            );
            assert!(
                rendered.contains(if panic_writer {
                    "injected catalog writer panic"
                } else {
                    "injected catalog writer failure"
                }),
                "terminal payload: {rendered}"
            );
            harness
                .run(
                    harness.candidates(&[("rerun", b"clean", 1)]),
                    1,
                    Arc::new(RecordingObserver::default()),
                )
                .expect("clean rerun after catalog termination");
        }
    }

    fn blob_file_count(store: &std::path::Path) -> usize {
        fs::read_dir(store.join("blobs"))
            .expect("read blob root")
            .flat_map(|first| {
                fs::read_dir(first.expect("first blob shard").path())
                    .expect("read first blob shard")
            })
            .flat_map(|second| {
                fs::read_dir(second.expect("second blob shard").path())
                    .expect("read second blob shard")
            })
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().expect("blob entry type").is_file())
            .count()
    }
}
