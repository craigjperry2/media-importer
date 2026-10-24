//! Command-neutral telemetry facts and the CLI-owned renderers for them.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, TrySendError};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle, TermLike};
use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};

pub const SCHEMA_VERSION: u8 = 1;

/// Stable output identity escaping for paths and other filesystem-derived
/// labels. JSON escaping alone is insufficient because it would preserve
/// control characters after decoding; this representation is safe to display
/// and remains one line in every consumer.
pub fn escape_output_identity(value: &str) -> String {
    use std::fmt::Write;

    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() {
            let _ = write!(escaped, "\\x{:02x}", character as u32);
        } else {
            escaped.push(character);
        }
    }
    escaped
}

/// A versioned domain fact. Values are deliberately limited to the scalar
/// vocabulary used by this command interface, rather than arbitrary JSON.
/// This prevents core code from accidentally exporting diagnostic structures.
#[derive(Clone, Debug)]
pub struct TelemetryEvent {
    pub schema_version: u8,
    pub command: String,
    pub event: String,
    fields: Vec<TelemetryField>,
}

#[derive(Clone, Debug)]
struct TelemetryField {
    name: &'static str,
    value: TelemetryValue,
}

#[derive(Clone, Debug)]
pub enum TelemetryValue {
    Boolean(bool),
    Count(u64),
    Text(String),
}

impl TelemetryValue {
    fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Count(value) => Some(*value),
            Self::Boolean(_) | Self::Text(_) => None,
        }
    }
}

impl From<bool> for TelemetryValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}
impl From<u64> for TelemetryValue {
    fn from(value: u64) -> Self {
        Self::Count(value)
    }
}
impl From<String> for TelemetryValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}
impl From<&str> for TelemetryValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl TelemetryEvent {
    pub fn new(command: &str, event: &str) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            command: command.to_owned(),
            event: event.to_owned(),
            fields: Vec::new(),
        }
    }

    pub fn field(mut self, name: &'static str, value: impl Into<TelemetryValue>) -> Self {
        self.fields.push(TelemetryField {
            name,
            value: value.into(),
        });
        self
    }

    #[cfg(test)]
    pub(crate) fn boolean_field(&self, name: &str) -> Option<bool> {
        self.fields
            .iter()
            .find(|field| field.name == name)
            .and_then(|field| match &field.value {
                TelemetryValue::Boolean(value) => Some(*value),
                TelemetryValue::Count(_) | TelemetryValue::Text(_) => None,
            })
    }
}

impl Serialize for TelemetryEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut object = serializer.serialize_map(Some(3 + self.fields.len()))?;
        object.serialize_entry("schema_version", &self.schema_version)?;
        object.serialize_entry("command", &self.command)?;
        object.serialize_entry("event", &self.event)?;
        for field in &self.fields {
            match &field.value {
                TelemetryValue::Boolean(value) => object.serialize_entry(field.name, value)?,
                TelemetryValue::Count(value) => object.serialize_entry(field.name, value)?,
                TelemetryValue::Text(value) => object.serialize_entry(field.name, value)?,
            }
        }
        object.end()
    }
}

/// Narrow behavior-to-presentation boundary.  A failed renderer becomes a
/// cancellation signal; callers must check it at their normal safe points.
pub trait TelemetrySink: Send + Sync {
    fn emit(&self, event: TelemetryEvent);
    fn failed(&self) -> bool {
        false
    }
    fn finish(&self) {}
}

/// The only bridge from command workers to a renderer.  It deliberately has a
/// small, bounded inbox: producers never wait on a terminal or pipe write.
/// High-frequency counter samples are coalesced when the inbox is full.
/// Domain facts are lossless: a full inbox applies bounded backpressure to the
/// producer instead of silently discarding a finding, action, state
/// transition, or durable catalog outcome.  In particular, queue saturation
/// is not a renderer failure and must never turn a healthy command into a
/// cancellation.  The queue remains bounded; only renderer I/O failure is a
/// cancellation signal.
pub struct RendererTransport {
    sender: Mutex<Option<Sender<TelemetryEvent>>>,
    failed: ArcFailure,
    join: Mutex<Option<JoinHandle<()>>>,
    pending_deltas: Mutex<BTreeMap<(String, String, &'static str), u64>>,
    // Gauges describe the most recent state, so replacing an older sample is
    // exact. Keeping them outside the renderer inbox prevents content workers
    // from ever waiting on a slow terminal merely to report queue pressure.
    pending_gauges: Mutex<BTreeMap<(String, String, String), TelemetryEvent>>,
    // A source worker can publish at most one terminal failure. There are at
    // most eight source workers, so this bounded side buffer preserves safe
    // failure routing without making the worker wait for renderer I/O.
    pending_worker_failures: Mutex<Vec<TelemetryEvent>>,
}

#[derive(Clone)]
struct ArcFailure(std::sync::Arc<AtomicBool>);

impl RendererTransport {
    pub fn new(renderer: std::sync::Arc<dyn TelemetrySink>) -> Self {
        const CAPACITY: usize = 256;
        let (sender, receiver) = crossbeam_channel::bounded(CAPACITY);
        let failed = ArcFailure(std::sync::Arc::new(AtomicBool::new(false)));
        let thread_failed = failed.clone();
        let join = std::thread::Builder::new()
            .name("telemetry-renderer".to_owned())
            .spawn(move || render_events(receiver, renderer, thread_failed))
            .expect("spawn telemetry renderer thread");
        Self {
            sender: Mutex::new(Some(sender)),
            failed,
            join: Mutex::new(Some(join)),
            pending_deltas: Mutex::new(BTreeMap::new()),
            pending_gauges: Mutex::new(BTreeMap::new()),
            pending_worker_failures: Mutex::new(Vec::with_capacity(8)),
        }
    }
}

fn render_events(
    receiver: Receiver<TelemetryEvent>,
    renderer: std::sync::Arc<dyn TelemetrySink>,
    failed: ArcFailure,
) {
    for event in receiver {
        renderer.emit(event);
        if renderer.failed() {
            failed.0.store(true, Ordering::Release);
            break;
        }
    }
    renderer.finish();
    if renderer.failed() {
        failed.0.store(true, Ordering::Release);
    }
}

impl TelemetrySink for RendererTransport {
    fn emit(&self, event: TelemetryEvent) {
        if self.failed() {
            return;
        }
        let sender = match self.sender.lock() {
            Ok(sender) => sender.clone(),
            Err(_) => {
                self.failed.0.store(true, Ordering::Release);
                return;
            }
        };
        let Some(sender) = sender else { return };
        if matches!(event.event.as_str(), "command_summary" | "command_failed") {
            // These are terminal command-owner events. They may wait only
            // after command work has completed, so drain coalesced counters
            // first and make the terminal state the final JSON record. In
            // particular, an operational failure must not be followed by a
            // counter which happened to be coalesced while the renderer was
            // congested.
            self.drain_pending(&sender);
            if !self.failed() && sender.send(event).is_err() {
                self.failed.0.store(true, Ordering::Release);
            }
            return;
        }
        self.flush_pending(&sender);
        match sender.try_send(event.clone()) {
            Ok(()) => {}
            Err(TrySendError::Full(event)) => {
                if self.coalesce_delta(event.clone()) {
                    return;
                }
                if self.coalesce_worker_fact(&event) {
                    return;
                }
                // A non-counter domain fact has no safe coalescing rule.  It
                // is therefore sent losslessly. These facts are emitted by
                // command coordination, never the source-content workers;
                // workers use the exact counter route above and consequently
                // never wait for a terminal or pipe renderer.
                if sender.send(event).is_err() {
                    self.failed.0.store(true, Ordering::Release);
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                self.failed.0.store(true, Ordering::Release);
            }
        }
    }

    fn failed(&self) -> bool {
        self.failed.0.load(Ordering::Acquire)
    }

    fn finish(&self) {
        self.close();
    }
}

impl RendererTransport {
    fn coalesce_delta(&self, event: TelemetryEvent) -> bool {
        if !event.event.ends_with("_delta") {
            return false;
        }
        let Ok(mut pending) = self.pending_deltas.lock() else {
            self.failed.0.store(true, Ordering::Release);
            return false;
        };
        let mut has_count = false;
        for field in event.fields {
            if let TelemetryValue::Count(value) = field.value {
                has_count = true;
                let key = (event.command.clone(), event.event.clone(), field.name);
                let Some(total) = pending.get_mut(&key) else {
                    pending.insert(key, value);
                    continue;
                };
                let Some(next) = total.checked_add(value) else {
                    self.failed.0.store(true, Ordering::Release);
                    return false;
                };
                *total = next;
            }
        }
        has_count
    }

    /// Convert high-frequency worker facts into exact, additive counters when
    /// the bounded renderer inbox is saturated.  The normal path retains the
    /// per-file record (including its path); this fallback deliberately trades
    /// that advisory detail for bounded, nonblocking workers while preserving
    /// every dashboard and summary total.
    fn coalesce_worker_fact(&self, event: &TelemetryEvent) -> bool {
        let add = |name: &'static str, field: &'static str, value: u64| {
            let Ok(mut pending) = self.pending_deltas.lock() else {
                self.failed.0.store(true, Ordering::Release);
                return false;
            };
            let key = (event.command.clone(), name.to_owned(), field);
            let Some(total) = pending.get_mut(&key) else {
                pending.insert(key, value);
                return true;
            };
            let Some(next) = total.checked_add(value) else {
                self.failed.0.store(true, Ordering::Release);
                return false;
            };
            *total = next;
            true
        };
        match event.event.as_str() {
            "file_discovered" => add("file_discovered_delta", "count", 1),
            "file_skipped" => add("file_skipped_delta", "count", 1),
            "file_hashed" => {
                // Source and staging byte totals are independently emitted as
                // delta events. Do not infer them from file completion: that
                // would duplicate counters when the inbox is saturated.
                add("file_hashed_delta", "count", 1)
            }
            // Audit hashes existing CAS blobs.  Preserve that public domain
            // vocabulary when a slow renderer forces coalescing: it is not an
            // import file hash and it never represents source/staging I/O.
            "blob_hashed" => {
                let bytes_read = count_field(event, "bytes_read");
                add("blob_hashed_delta", "count", 1)
                    && (bytes_read == 0 || add("audit_bytes_delta", "bytes", bytes_read))
            }
            "cas_blob_created" => add("cas_blob_created_delta", "count", 1),
            "cas_blob_reused" => add("cas_blob_reused_delta", "count", 1),
            "active_content_workers" | "queue_occupancy" | "queue_backpressure" => {
                self.coalesce_gauge(event.clone())
            }
            "operational_failure" => self.coalesce_worker_failure(event.clone()),
            // These are already additive metrics and retain their existing
            // stable event names in the normal path.
            "source_bytes_delta" | "staging_bytes_delta" => self.coalesce_delta(event.clone()),
            _ => false,
        }
    }

    fn coalesce_gauge(&self, event: TelemetryEvent) -> bool {
        let discriminator = event
            .fields
            .iter()
            .find(|field| field.name == "stage")
            .and_then(|field| match &field.value {
                TelemetryValue::Text(value) => Some(value.clone()),
                TelemetryValue::Boolean(_) | TelemetryValue::Count(_) => None,
            })
            .unwrap_or_default();
        let key = (event.command.clone(), event.event.clone(), discriminator);
        match self.pending_gauges.lock() {
            Ok(mut pending) => {
                pending.insert(key, event);
                true
            }
            Err(_) => {
                self.failed.0.store(true, Ordering::Release);
                false
            }
        }
    }

    fn coalesce_worker_failure(&self, event: TelemetryEvent) -> bool {
        match self.pending_worker_failures.lock() {
            Ok(mut pending) if pending.len() < 8 => {
                pending.push(event);
                true
            }
            Ok(_) => {
                // More than one failure per bounded worker set would violate
                // the executor contract. Cancellation is safer than allowing
                // an unbounded fallback or blocking a worker indefinitely.
                self.failed.0.store(true, Ordering::Release);
                false
            }
            Err(_) => {
                self.failed.0.store(true, Ordering::Release);
                false
            }
        }
    }

    fn flush_deltas(&self, sender: &Sender<TelemetryEvent>) {
        let Ok(mut pending) = self.pending_deltas.lock() else {
            self.failed.0.store(true, Ordering::Release);
            return;
        };
        while let Some(((command, event, name), value)) = pending.pop_first() {
            let delta = TelemetryEvent::new(&command, &event).field(name, value);
            match sender.try_send(delta) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    pending.insert((command, event, name), value);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.failed.0.store(true, Ordering::Release);
                    break;
                }
            }
        }
    }

    fn flush_pending(&self, sender: &Sender<TelemetryEvent>) {
        self.flush_deltas(sender);
        if self.failed() {
            return;
        }
        let Ok(mut failures) = self.pending_worker_failures.lock() else {
            self.failed.0.store(true, Ordering::Release);
            return;
        };
        while !failures.is_empty() {
            let event = failures.remove(0);
            match sender.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    failures.insert(0, event);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.failed.0.store(true, Ordering::Release);
                    return;
                }
            }
        }
        drop(failures);
        let Ok(mut gauges) = self.pending_gauges.lock() else {
            self.failed.0.store(true, Ordering::Release);
            return;
        };
        while let Some((key, event)) = gauges.pop_first() {
            match sender.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    gauges.insert(key, event);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.failed.0.store(true, Ordering::Release);
                    return;
                }
            }
        }
    }

    /// Close the bounded transport after the terminal event has been queued
    /// and wait for the renderer.  CLI calls this on every command outcome.
    pub fn close(&self) {
        // A worker never waits for rendering, but the command owner may wait
        // here while shutting down.  Drain every coalesced delta before
        // dropping the sender so an otherwise successful command never loses
        // exact counter totals at the renderer boundary.
        if let Ok(sender) = self.sender.lock() {
            if let Some(sender) = sender.as_ref() {
                self.drain_pending(sender);
            }
        } else {
            self.failed.0.store(true, Ordering::Release);
        }
        // Dropping the last sender lets the renderer finish its already queued
        // records, clear a dashboard, and then join normally.
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        } else {
            self.failed.0.store(true, Ordering::Release);
        }
        if let Ok(mut join) = self.join.lock()
            && let Some(handle) = join.take()
            && handle.join().is_err()
        {
            self.failed.0.store(true, Ordering::Release);
        }
    }

    fn drain_pending(&self, sender: &Sender<TelemetryEvent>) {
        let Ok(mut pending) = self.pending_deltas.lock() else {
            self.failed.0.store(true, Ordering::Release);
            return;
        };
        while let Some(((command, event, name), value)) = pending.pop_first() {
            let delta = TelemetryEvent::new(&command, &event).field(name, value);
            if sender.send(delta).is_err() {
                self.failed.0.store(true, Ordering::Release);
                return;
            }
        }
        drop(pending);
        let Ok(mut failures) = self.pending_worker_failures.lock() else {
            self.failed.0.store(true, Ordering::Release);
            return;
        };
        for event in failures.drain(..) {
            if sender.send(event).is_err() {
                self.failed.0.store(true, Ordering::Release);
                return;
            }
        }
        drop(failures);
        let Ok(mut gauges) = self.pending_gauges.lock() else {
            self.failed.0.store(true, Ordering::Release);
            return;
        };
        for (_, event) in std::mem::take(&mut *gauges) {
            if sender.send(event).is_err() {
                self.failed.0.store(true, Ordering::Release);
                return;
            }
        }
    }
}

pub struct NoopTelemetrySink;
impl TelemetrySink for NoopTelemetrySink {
    fn emit(&self, _: TelemetryEvent) {}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputMode {
    Auto,
    Human,
    Jsonl,
}
impl OutputMode {
    pub fn uses_jsonl(self, stdout_is_terminal: bool) -> bool {
        matches!(self, Self::Jsonl) || matches!(self, Self::Auto) && !stdout_is_terminal
    }
}
pub fn stdout_is_terminal() -> bool {
    std::io::stdout().is_terminal()
}

/// One locked write per record prevents concurrent workers from interleaving
/// JSON objects.  A broken pipe is retained as state instead of being silently
/// discarded, allowing ingest to cancel and join its workers.
pub struct JsonlTelemetrySink {
    output: Mutex<Box<dyn Write + Send>>,
    failed: AtomicBool,
}
impl JsonlTelemetrySink {
    pub fn new() -> Self {
        Self {
            output: Mutex::new(Box::new(std::io::stdout())),
            failed: AtomicBool::new(false),
        }
    }

    /// Constructor for renderer tests and embedders. Output errors are kept as
    /// renderer state so command orchestration can cancel and join normally.
    pub fn with_writer(output: impl Write + Send + 'static) -> Self {
        Self {
            output: Mutex::new(Box::new(output)),
            failed: AtomicBool::new(false),
        }
    }
}
impl Default for JsonlTelemetrySink {
    fn default() -> Self {
        Self::new()
    }
}
impl TelemetrySink for JsonlTelemetrySink {
    fn emit(&self, event: TelemetryEvent) {
        let Ok(mut output) = self.output.lock() else {
            self.failed.store(true, Ordering::Release);
            return;
        };
        if serde_json::to_writer(&mut *output, &event).is_err()
            || output.write_all(b"\n").is_err()
            || output.flush().is_err()
        {
            self.failed.store(true, Ordering::Release);
        }
    }
    fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}

/// Exactly four coalesced terminal lines.  Counters remain exact in the state;
/// indicatif redraws are limited to ten per second.
pub struct HumanTelemetrySink {
    progress: MultiProgress,
    lines: [ProgressBar; 4],
    state: Mutex<DashboardState>,
    failed: Arc<AtomicBool>,
}
struct DashboardState {
    command: String,
    files: u64,
    completed: u64,
    skipped: u64,
    read: u64,
    written: u64,
    created: u64,
    reused: u64,
    active: u64,
    mounts: u64,
    committed: u64,
    batches: u64,
    checkpoints: u64,
    queue: u64,
    gc_candidates: u64,
    reclaimable: u64,
    unlinked: u64,
    tree_directories: u64,
    started: Instant,
    last_draw: Option<Instant>,
}
impl DashboardState {
    fn new(command: &str) -> Self {
        Self {
            command: command.to_owned(),
            files: 0,
            completed: 0,
            skipped: 0,
            read: 0,
            written: 0,
            created: 0,
            reused: 0,
            active: 0,
            mounts: 0,
            committed: 0,
            batches: 0,
            checkpoints: 0,
            queue: 0,
            gc_candidates: 0,
            reclaimable: 0,
            unlinked: 0,
            tree_directories: 0,
            started: Instant::now(),
            last_draw: None,
        }
    }
}
impl HumanTelemetrySink {
    pub fn new(terminal: bool, command: &str) -> Self {
        let failed = Arc::new(AtomicBool::new(false));
        let progress = MultiProgress::with_draw_target(if terminal {
            ProgressDrawTarget::term_like_with_hz(
                Box::new(FailureTrackingTerm::new(
                    Box::new(StdoutTerm::default()),
                    Arc::clone(&failed),
                )),
                10,
            )
        } else {
            ProgressDrawTarget::hidden()
        });
        let style =
            ProgressStyle::with_template("{msg}").expect("fixed dashboard template is valid");
        let lines = std::array::from_fn(|_| {
            let bar = progress.add(ProgressBar::new_spinner());
            bar.set_style(style.clone());
            bar.enable_steady_tick(std::time::Duration::from_millis(100));
            bar
        });
        let sink = Self {
            progress,
            lines,
            state: Mutex::new(DashboardState::new(command)),
            failed,
        };
        sink.draw(&DashboardState::new(command));
        sink
    }

    /// Construct a dashboard with an injectable terminal target.  The sink
    /// records every drawing error from the target and exposes it through the
    /// normal telemetry cancellation boundary.
    pub fn with_term_like(command: &str, terminal: Box<dyn TermLike>) -> Self {
        let failed = Arc::new(AtomicBool::new(false));
        let progress = MultiProgress::with_draw_target(ProgressDrawTarget::term_like_with_hz(
            Box::new(FailureTrackingTerm::new(terminal, Arc::clone(&failed))),
            10,
        ));
        let style =
            ProgressStyle::with_template("{msg}").expect("fixed dashboard template is valid");
        let lines = std::array::from_fn(|_| {
            let bar = progress.add(ProgressBar::new_spinner());
            bar.set_style(style.clone());
            bar.enable_steady_tick(std::time::Duration::from_millis(100));
            bar
        });
        let sink = Self {
            progress,
            lines,
            state: Mutex::new(DashboardState::new(command)),
            failed,
        };
        sink.draw(&DashboardState::new(command));
        sink
    }
    fn draw(&self, state: &DashboardState) {
        let elapsed_duration = state.started.elapsed();
        let elapsed = elapsed_duration.as_secs();
        // A zero-duration observation has no measurable rate.  Render zero
        // rather than inventing a denominator; subsequent redraws use the
        // monotonic elapsed time captured above.
        let elapsed_seconds = elapsed_duration.as_secs_f64();
        let write_rate = if elapsed_seconds > 0.0 {
            (state.written as f64 / elapsed_seconds) as u64
        } else {
            0
        };
        let read_rate = if elapsed_seconds > 0.0 {
            (state.read as f64 / elapsed_seconds) as u64
        } else {
            0
        };
        let messages = match state.command.as_str() {
            "audit" => [
                format!(
                    "Progress  blobs={} hashed={} findings={} elapsed={}s",
                    state.files, state.completed, state.skipped, elapsed
                ),
                format!(
                    "Audit     catalog-blobs={} gc-candidates={}",
                    state.committed, state.gc_candidates
                ),
                format!(
                    "Hash      read={} rate={} cas-files={}",
                    state.read, read_rate, state.created
                ),
                format!(
                    "Integrity findings={} catalog-checkpoints={}",
                    state.skipped, state.checkpoints
                ),
            ],
            "gc" => [
                format!(
                    "Progress  planned={} completed={} findings={} elapsed={}s",
                    state.files, state.completed, state.skipped, elapsed
                ),
                format!(
                    "GC        candidates={} hashed={} reclaimable={}",
                    state.gc_candidates, state.created, state.reclaimable
                ),
                format!(
                    "Hash      read={} rate={} unlinked={}",
                    state.read, read_rate, state.unlinked
                ),
                format!(
                    "Catalog   blob-rows={} transactions={} checkpoints={}",
                    state.committed, state.batches, state.checkpoints
                ),
            ],
            "build_tree" => [
                format!(
                    "Progress  entries={} applied={} skipped={} elapsed={}s",
                    state.files, state.completed, state.skipped, elapsed
                ),
                format!(
                    "Tree      links-created={} links-reused={} links-replaced={}",
                    state.created, state.reused, state.written
                ),
                format!(
                    "Tree      stale-links={} directories={}",
                    state.skipped, state.tree_directories
                ),
                format!(
                    "Catalog   desired-entries={} applied-links={}",
                    state.files, state.completed
                ),
            ],
            _ => [
                format!(
                    "Progress  files={} completed={} skipped={} elapsed={}s",
                    state.files, state.completed, state.skipped, elapsed
                ),
                format!(
                    "Ingest    written={} rate={} blobs-created={} blobs-reused={}",
                    state.written, write_rate, state.created, state.reused
                ),
                format!(
                    "Hash      read={} rate={} active={} mounts={}",
                    state.read, read_rate, state.active, state.mounts
                ),
                format!(
                    "Catalog   committed={} batches={} checkpoints={} queue={}",
                    state.committed, state.batches, state.checkpoints, state.queue
                ),
            ],
        };
        for (line, message) in self.lines.iter().zip(messages) {
            line.set_message(message);
        }
    }
}

/// Minimal stdout target used instead of indicatif's opaque terminal target,
/// so a failed write is observable by the command's cancellation boundary.
#[derive(Debug)]
struct StdoutTerm {
    output: Mutex<std::io::Stdout>,
}

impl Default for StdoutTerm {
    fn default() -> Self {
        Self {
            output: Mutex::new(std::io::stdout()),
        }
    }
}

impl StdoutTerm {
    fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut output = self
            .output
            .lock()
            .map_err(|_| std::io::Error::other("stdout terminal lock poisoned"))?;
        output.write_all(bytes)
    }
}

impl TermLike for StdoutTerm {
    fn width(&self) -> u16 {
        80
    }
    fn height(&self) -> u16 {
        24
    }
    fn move_cursor_up(&self, n: usize) -> std::io::Result<()> {
        self.write(format!("\u{1b}[{n}A").as_bytes())
    }
    fn move_cursor_down(&self, n: usize) -> std::io::Result<()> {
        self.write(format!("\u{1b}[{n}B").as_bytes())
    }
    fn move_cursor_right(&self, n: usize) -> std::io::Result<()> {
        self.write(format!("\u{1b}[{n}C").as_bytes())
    }
    fn move_cursor_left(&self, n: usize) -> std::io::Result<()> {
        self.write(format!("\u{1b}[{n}D").as_bytes())
    }
    fn write_line(&self, line: &str) -> std::io::Result<()> {
        self.write(line.as_bytes())?;
        self.write(b"\n")
    }
    fn write_str(&self, value: &str) -> std::io::Result<()> {
        self.write(value.as_bytes())
    }
    fn clear_line(&self) -> std::io::Result<()> {
        self.write(b"\r\x1b[2K")
    }
    fn flush(&self) -> std::io::Result<()> {
        let mut output = self
            .output
            .lock()
            .map_err(|_| std::io::Error::other("stdout terminal lock poisoned"))?;
        output.flush()
    }
}

#[derive(Debug)]
struct FailureTrackingTerm {
    inner: Box<dyn TermLike>,
    failed: Arc<AtomicBool>,
}

impl FailureTrackingTerm {
    fn new(inner: Box<dyn TermLike>, failed: Arc<AtomicBool>) -> Self {
        Self { inner, failed }
    }

    fn record(&self, result: std::io::Result<()>) -> std::io::Result<()> {
        if result.is_err() {
            self.failed.store(true, Ordering::Release);
        }
        result
    }
}

impl TermLike for FailureTrackingTerm {
    fn width(&self) -> u16 {
        self.inner.width()
    }
    fn height(&self) -> u16 {
        self.inner.height()
    }
    fn move_cursor_up(&self, n: usize) -> std::io::Result<()> {
        self.record(self.inner.move_cursor_up(n))
    }
    fn move_cursor_down(&self, n: usize) -> std::io::Result<()> {
        self.record(self.inner.move_cursor_down(n))
    }
    fn move_cursor_right(&self, n: usize) -> std::io::Result<()> {
        self.record(self.inner.move_cursor_right(n))
    }
    fn move_cursor_left(&self, n: usize) -> std::io::Result<()> {
        self.record(self.inner.move_cursor_left(n))
    }
    fn write_line(&self, value: &str) -> std::io::Result<()> {
        self.record(self.inner.write_line(value))
    }
    fn write_str(&self, value: &str) -> std::io::Result<()> {
        self.record(self.inner.write_str(value))
    }
    fn clear_line(&self) -> std::io::Result<()> {
        self.record(self.inner.clear_line())
    }
    fn flush(&self) -> std::io::Result<()> {
        self.record(self.inner.flush())
    }
}

fn count_field(event: &TelemetryEvent, name: &str) -> u64 {
    event
        .fields
        .iter()
        .find(|field| field.name == name)
        .and_then(|field| field.value.as_u64())
        .unwrap_or(0)
}
impl TelemetrySink for HumanTelemetrySink {
    fn emit(&self, event: TelemetryEvent) {
        let Ok(mut state) = self.state.lock() else {
            self.failed.store(true, Ordering::Release);
            return;
        };
        match event.event.as_str() {
            "command_started" => state.command = event.command,
            "file_discovered" | "blob_discovered" => state.files += 1,
            "file_discovered_delta" => state.files += count_field(&event, "count"),
            "file_skipped" => {
                state.skipped += 1;
                state.completed += 1;
            }
            "file_skipped_delta" => {
                let count = count_field(&event, "count");
                state.skipped += count;
                state.completed += count;
            }
            "file_hashed" | "blob_hashed" => {
                state.completed += 1;
                state.read += count_field(&event, "bytes_read");
                state.written += count_field(&event, "staging_bytes");
            }
            "file_hashed_delta" | "blob_hashed_delta" => {
                state.completed += count_field(&event, "count");
            }
            "cas_blob_created" => state.created += 1,
            "cas_blob_created_delta" => state.created += count_field(&event, "count"),
            "cas_blob_reused" => state.reused += 1,
            "cas_blob_reused_delta" => state.reused += count_field(&event, "count"),
            "catalog_record_committed" => state.committed += 1,
            "catalog_batch_committed" => state.batches += 1,
            "catalog_checkpoint_completed" => state.checkpoints += 1,
            // The last writer checkpoint is emitted while its shutdown is
            // joining, after normal event draining has ended.  This aggregate
            // reconciles the live counter with durable writer state.
            "catalog_final_checkpoint" => state.checkpoints = count_field(&event, "checkpoints"),
            "source_bytes_delta" => state.read += count_field(&event, "bytes"),
            "audit_bytes_delta" => state.read += count_field(&event, "bytes"),
            "staging_bytes_delta" => state.written += count_field(&event, "bytes"),
            "active_content_workers" => {
                state.active = count_field(&event, "active");
                state.mounts = count_field(&event, "mounts");
            }
            "queue_occupancy" => state.queue = count_field(&event, "occupancy"),
            "audit_catalog_scanned" => {
                state.committed = count_field(&event, "catalog_blobs");
                state.gc_candidates = count_field(&event, "gc_candidates");
            }
            "audit_cas_scanned" => state.created = count_field(&event, "cas_blob_files"),
            "finding" => state.skipped += 1,
            // This is a final report fact. Individual `gc_blob_hashed`
            // events already updated the live counter, so counting it again
            // would double the dashboard total.
            "gc_preflight_hashed" => {}
            "gc_preflight_started" => {
                // Hashing happens before actions. Until the preflight is
                // complete, progress has an honest candidate denominator.
                state.files = count_field(&event, "sweep_candidates");
                state.gc_candidates = state.files;
                state.completed = 0;
            }
            "gc_blob_hashed" => {
                // Hash completion is distinct from completed GC actions;
                // retaining it in the read counter keeps both dashboard
                // phases truthful when the denominator changes below.
                state.read += count_field(&event, "bytes_read");
                state.created += 1;
            }
            "gc_preflight_complete" => {
                state.files = count_field(&event, "planned_actions");
                state.skipped = count_field(&event, "findings");
                state.committed = count_field(&event, "catalog_blobs");
                state.completed = 0;
                state.reclaimable = count_field(&event, "bytes_reclaimable");
            }
            "gc_action" => state.completed += 1,
            "gc_catalog_committed" => {
                state.batches += 1;
                state.unlinked += count_field(&event, "bytes_unlinked");
            }
            "tree_entry_planned" => state.files += 1,
            "tree_link_applied" => state.completed += 1,
            "tree_directory_applied" => state.tree_directories += 1,
            "tree_planned" => state.files = count_field(&event, "entries"),
            "tree_applied" => {
                state.completed = count_field(&event, "links_created")
                    + count_field(&event, "links_replaced")
                    + count_field(&event, "stale_links_removed");
                state.created = count_field(&event, "links_created");
                state.written = count_field(&event, "links_replaced");
                state.skipped = count_field(&event, "stale_links_removed");
                state.tree_directories = count_field(&event, "directories_created")
                    + count_field(&event, "directories_pruned");
            }
            _ => {}
        }
        let now = Instant::now();
        if state
            .last_draw
            .is_none_or(|last| now.duration_since(last).as_millis() >= 100)
        {
            state.last_draw = Some(now);
            self.draw(&state);
        }
    }
    fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
    fn finish(&self) {
        // A command can complete inside the refresh coalescing window. Draw
        // once unconditionally before clearing the live area so the final
        // dashboard state is truthful even for short commands.
        if let Ok(state) = self.state.lock() {
            self.draw(&state);
        } else {
            self.failed.store(true, Ordering::Release);
            return;
        }
        if self.progress.clear().is_err() {
            self.failed.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::Arc;

    #[derive(Default)]
    struct CollectingSink {
        events: Mutex<Vec<TelemetryEvent>>,
        fail_on_finish: AtomicBool,
        finished: AtomicBool,
    }

    impl TelemetrySink for CollectingSink {
        fn emit(&self, event: TelemetryEvent) {
            self.events.lock().expect("event lock").push(event);
        }

        fn failed(&self) -> bool {
            self.finished.load(Ordering::Acquire) && self.fail_on_finish.load(Ordering::Acquire)
        }

        fn finish(&self) {
            self.finished.store(true, Ordering::Release);
        }
    }
    #[test]
    fn mode_selection_honours_explicit_overrides() {
        assert!(OutputMode::Auto.uses_jsonl(false));
        assert!(!OutputMode::Auto.uses_jsonl(true));
        assert!(!OutputMode::Human.uses_jsonl(false));
        assert!(OutputMode::Jsonl.uses_jsonl(true));
    }

    #[test]
    fn dashboard_state_starts_with_the_selected_command() {
        assert_eq!(DashboardState::new("audit").command, "audit");
        assert_eq!(DashboardState::new("gc").command, "gc");
    }

    #[derive(Debug)]
    struct FailingTerminal;

    impl TermLike for FailingTerminal {
        fn width(&self) -> u16 {
            80
        }
        fn move_cursor_up(&self, _: usize) -> io::Result<()> {
            Ok(())
        }
        fn move_cursor_down(&self, _: usize) -> io::Result<()> {
            Ok(())
        }
        fn move_cursor_right(&self, _: usize) -> io::Result<()> {
            Ok(())
        }
        fn move_cursor_left(&self, _: usize) -> io::Result<()> {
            Ok(())
        }
        fn write_line(&self, _: &str) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injected broken pipe",
            ))
        }
        fn write_str(&self, _: &str) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injected broken pipe",
            ))
        }
        fn clear_line(&self) -> io::Result<()> {
            Ok(())
        }
        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn terminal_draw_failures_are_exposed_for_cooperative_cancellation() {
        let sink = HumanTelemetrySink::with_term_like("import", Box::new(FailingTerminal));
        sink.emit(TelemetryEvent::new("import", "command_started"));
        assert!(sink.failed(), "terminal write failure must be observable");
    }
    #[test]
    fn event_schema_has_stable_required_fields() {
        let value = serde_json::to_value(
            TelemetryEvent::new("import", "file_skipped").field("bytes", 4_u64),
        )
        .unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["command"], "import");
        assert_eq!(value["event"], "file_skipped");
    }

    #[test]
    fn close_drains_coalesced_deltas_without_losing_totals() {
        let renderer = Arc::new(CollectingSink::default());
        let transport = RendererTransport::new(renderer.clone());
        for _ in 0..300 {
            transport
                .emit(TelemetryEvent::new("import", "source_bytes_delta").field("bytes", 1_u64));
        }
        transport.close();

        let total: u64 = renderer
            .events
            .lock()
            .expect("event lock")
            .iter()
            .filter(|event| event.event == "source_bytes_delta")
            .map(|event| count_field(event, "bytes"))
            .sum();
        assert_eq!(total, 300);
        assert!(!transport.failed());
    }

    #[test]
    fn command_failed_is_terminal_after_coalesced_counters() {
        let renderer = Arc::new(CollectingSink::default());
        let transport = RendererTransport::new(renderer.clone());
        for _ in 0..300 {
            transport
                .emit(TelemetryEvent::new("import", "source_bytes_delta").field("bytes", 1_u64));
        }
        transport.emit(TelemetryEvent::new("import", "command_failed"));
        transport.close();

        let events = renderer.events.lock().expect("event lock");
        assert_eq!(
            events.last().map(|event| event.event.as_str()),
            Some("command_failed")
        );
        let total: u64 = events
            .iter()
            .filter(|event| event.event == "source_bytes_delta")
            .map(|event| count_field(event, "bytes"))
            .sum();
        assert_eq!(total, 300);
    }

    #[test]
    fn queue_saturation_preserves_non_coalescible_facts_without_cancellation() {
        struct SlowCollectingSink(Mutex<Vec<TelemetryEvent>>);
        impl TelemetrySink for SlowCollectingSink {
            fn emit(&self, event: TelemetryEvent) {
                std::thread::sleep(std::time::Duration::from_millis(1));
                self.0.lock().expect("event lock").push(event);
            }
        }

        let renderer = Arc::new(SlowCollectingSink(Mutex::new(Vec::new())));
        let transport = RendererTransport::new(renderer.clone());
        for number in 0..300_u64 {
            transport.emit(
                TelemetryEvent::new("audit", "finding").field("identity", number.to_string()),
            );
        }
        transport.close();
        assert!(!transport.failed());
        assert_eq!(renderer.0.lock().expect("event lock").len(), 300);
    }

    #[test]
    fn saturated_renderer_coalesces_worker_facts_without_blocking_or_losing_totals() {
        struct SlowCollectingSink(Mutex<Vec<TelemetryEvent>>);
        impl TelemetrySink for SlowCollectingSink {
            fn emit(&self, event: TelemetryEvent) {
                std::thread::sleep(std::time::Duration::from_millis(2));
                self.0.lock().expect("event lock").push(event);
            }
        }

        let renderer = Arc::new(SlowCollectingSink(Mutex::new(Vec::new())));
        let transport = RendererTransport::new(renderer.clone());
        let started = Instant::now();
        for _ in 0..400 {
            transport
                .emit(TelemetryEvent::new("import", "file_hashed").field("path", "source/file"));
            transport
                .emit(TelemetryEvent::new("import", "source_bytes_delta").field("bytes", 3_u64));
        }
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "source workers must not wait for the slow renderer"
        );
        transport.close();

        let events = renderer.0.lock().expect("event lock");
        let files: u64 = events
            .iter()
            .map(|event| match event.event.as_str() {
                "file_hashed" => 1,
                "file_hashed_delta" => count_field(event, "count"),
                _ => 0,
            })
            .sum();
        let bytes: u64 = events
            .iter()
            .map(|event| match event.event.as_str() {
                "file_hashed" => count_field(event, "bytes_read"),
                "source_bytes_delta" => count_field(event, "bytes"),
                _ => 0,
            })
            .sum();
        assert_eq!(files, 400);
        assert_eq!(bytes, 1_200);
        assert!(!transport.failed());
    }

    #[test]
    fn saturated_audit_hashes_keep_audit_event_names_and_byte_meaning() {
        struct SlowCollectingSink(Mutex<Vec<TelemetryEvent>>);
        impl TelemetrySink for SlowCollectingSink {
            fn emit(&self, event: TelemetryEvent) {
                std::thread::sleep(std::time::Duration::from_millis(2));
                self.0.lock().expect("event lock").push(event);
            }
        }

        let renderer = Arc::new(SlowCollectingSink(Mutex::new(Vec::new())));
        let transport = RendererTransport::new(renderer.clone());
        for _ in 0..400 {
            transport.emit(
                TelemetryEvent::new("audit", "blob_hashed")
                    .field("hash", "a".repeat(64))
                    .field("bytes_read", 3_u64),
            );
        }
        transport.close();

        let events = renderer.0.lock().expect("event lock");
        assert!(events.iter().all(|event| {
            !matches!(
                event.event.as_str(),
                "file_hashed_delta" | "source_bytes_delta" | "staging_bytes_delta"
            )
        }));
        let hashes: u64 = events
            .iter()
            .map(|event| match event.event.as_str() {
                "blob_hashed" => 1,
                "blob_hashed_delta" => count_field(event, "count"),
                _ => 0,
            })
            .sum();
        let bytes: u64 = events
            .iter()
            .map(|event| match event.event.as_str() {
                "blob_hashed" => count_field(event, "bytes_read"),
                "audit_bytes_delta" => count_field(event, "bytes"),
                _ => 0,
            })
            .sum();
        assert_eq!(hashes, 400);
        assert_eq!(bytes, 1_200);
        assert!(!transport.failed());
    }

    #[test]
    fn renderer_failure_during_finish_is_visible_to_command_owner() {
        let renderer = Arc::new(CollectingSink::default());
        renderer.fail_on_finish.store(true, Ordering::Release);
        let transport = RendererTransport::new(renderer);
        transport.emit(TelemetryEvent::new("audit", "command_started"));
        transport.close();
        assert!(transport.failed());
    }

    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "closed test pipe",
            ))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn json_renderer_turns_a_broken_pipe_into_cancellation_state() {
        let sink = JsonlTelemetrySink::with_writer(FailingWriter);
        sink.emit(TelemetryEvent::new("import", "command_started"));
        assert!(sink.failed());
    }
}
