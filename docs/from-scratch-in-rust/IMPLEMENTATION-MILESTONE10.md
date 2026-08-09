# Rust Implementation Guide: Milestone 10

This guide records implementation decisions for milestone 10. `SPEC.md` remains
the source of product intent. Milestones 6 through 9 provide the performance and
catalog events needed for a real reporting layer. Milestone 10 adds terminal
dashboards and structured append-only non-terminal output.

The Python implementation is deprecated historical context and is not a
behavior oracle.

## Companion Instructions

- `RUST.instructions.md`
- `RUST-ARCHITECTURE.instructions.md`
- `RUST-TESTING.instructions.md`
- `SQLITE.instructions.md`

## Milestone 10: Structured Telemetry And Adaptive Rendering

Add `indicatif`, `serde` with derive support, and `serde_json` as runtime
dependencies. Use `std::io::IsTerminal` to choose the default stdout renderer.

Preserve the four commands. Add one global presentation option:

```text
media-importer [--output <auto|human|jsonl>] <COMMAND> ...
```

- `auto` is the default: terminal stdout uses the dashboard/human renderer;
  non-terminal stdout uses JSON Lines.
- `human` forces the terminal-style final human report but does not emit cursor
  control when stdout is not a terminal.
- `jsonl` forces structured append-only records and never emits cursor control.

Output selection changes presentation only. It must not change command work,
locking, dry-run, transactions, findings, or exit status.

## Telemetry Boundary

Create a command-neutral structured event model owned by a `telemetry` or
`reporting` boundary. Deep behavior APIs receive a narrow sink:

```rust
pub trait TelemetrySink: Send + Sync {
    fn emit(&self, event: TelemetryEvent);
}
```

Events should cover stable domain facts, not private implementation choreography:

- command started/finished;
- files discovered, skipped, and hashed;
- source bytes read and staging bytes written;
- CAS blob created/reused;
- active content workers and queue backpressure;
- catalog records committed and batches committed;
- passive checkpoint outcomes;
- audit/GC blobs hashed;
- GC actions/findings;
- build-tree plan/application progress; and
- cancellation or operational failure context safe for output.

Keep tracing on stderr for diagnostic detail. Telemetry is not a substitute for
contextual `color-eyre` errors.

The CLI constructs the sink and owns rendering. Scanner, store, catalog,
executor, audit, GC, and materialize modules must not call `println!`, write
cursor controls, serialize JSON, or depend on `indicatif`.

## Event Schema

JSON Lines output is a versioned public machine interface. Every line is one
complete UTF-8 JSON object with at least:

```json
{"schema_version":1,"command":"import","event":"file_skipped", ...}
```

Requirements:

- one object per line, flushed promptly enough for long-running pipelines;
- no pretty printing, multiline objects, banners, blank separators, or ANSI;
- stable snake_case event and field names;
- integer byte/count/time values, not localized strings;
- paths encoded through the repository's safe escaped identity policy;
- hashes remain lowercase full BLAKE3 text when present;
- dry-run state included where relevant;
- a final `command_summary` event always follows a successfully constructed
  report, including findings/blocked/incomplete outcomes;
- operational errors remain stderr reports and process exit 1, but may also emit
  a final safe `command_failed` event when the renderer was initialized;
- never serialize arbitrary `color-eyre` debug chains or sensitive environment
  values to stdout.

Document schema compatibility: additive fields are allowed within version 1;
renaming/removing fields or changing meanings requires a new schema version.

## Four-Line TTY Dashboard

For long-running commands on terminal stdout, use one `indicatif::MultiProgress`
with exactly four live lines:

```text
Progress  files=... completed=... skipped=... elapsed=...
Ingest    written=... rate=... blobs-created=... blobs-reused=...
Hash      read=... rate=... active=... mounts=...
Catalog   committed=... batches=... checkpoints=... queue=...
```

Adapt labels for audit, GC, and build-tree while retaining four physical lines.
Unknown totals must render honestly; do not pre-scan the entire source merely to
obtain a denominator. Rates use monotonic elapsed time and handle zero duration
without division errors.

On completion, clear or finish the live bars cleanly and print the existing
human final report below them. Findings and GC action lines remain deterministic
human output after the dashboard is finished. Never leave a partially drawn
dashboard on a handled failure.

Do not update the display for every byte. Coalesce refreshes to a bounded rate
such as 10 Hz while counters remain exact.

## Command Coverage

### Import

Show discovery/completion, metadata skips, one-pass hash rate, staging/CAS write
rate, active workers/mounts, catalog batches, and checkpoints.

### Audit

Show catalog/CAS reconciliation and blob hashing progress. Audit remains fully
read-only; reporting must not create application files.

### Garbage Collection

Show preflight hashing, planned/completed actions, and catalog commit state.
Preserve incomplete reports and exit status behavior.

### Build Tree

Show entries planned, links applied, and directories processed. Do not add an
extra catalog scan just to determine totals.

Short commands may render the same four lines briefly or only the final human
report when no progress event was emitted. The non-TTY JSON Lines contract
applies regardless of duration.

## Concurrency And Backpressure

Worker threads must never block on terminal rendering. Send events through a
bounded reporting channel with a coalescing policy for high-frequency counter
deltas. Never drop findings, actions, command state transitions, batch commits,
checkpoint outcomes, or the final summary.

It is acceptable to combine adjacent byte-delta events before rendering. Event
coalescing must preserve exact cumulative totals. Renderer failure, including a
broken pipe, triggers cooperative command cancellation and normal thread joins;
do not panic or detach work.

## Existing Human Output

Retain the existing human headings, findings, actions, counters, and exit-status
meanings under terminal `auto` and forced `human`. Update wording only where new
milestone counters require it.

Tests that expect human strings must explicitly request `--output human` when
their stdout is captured. New default non-TTY tests must parse JSON Lines rather
than search free-form text.

## Testing Requirements

Add tests proving:

- auto selection chooses JSON Lines for piped/captured stdout;
- auto selection chooses the terminal renderer under a PTY;
- explicit `human` and `jsonl` override detection;
- JSON Lines contains only independently parseable one-line objects;
- schema version, command, event names, counters, hashes, paths, and dry-run
  fields are typed and stable;
- every successful, blocked, and incomplete report emits a final summary;
- exit statuses remain 0/1/2 as previously specified;
- the TTY dashboard uses exactly four live lines and leaves no cursor artifacts
  after completion;
- refresh coalescing does not lose bytes or counts;
- findings/action ordering remains deterministic;
- concurrent import events do not corrupt JSON lines;
- a slow renderer cannot cause unbounded memory growth;
- broken-pipe/render failure cancels and joins all command workers;
- dry-run/read-only commands remain durably mutation-free in both modes;
- no production stdout writes exist outside CLI/reporting modules;
- no core module depends on `indicatif`, `serde_json`, or terminal detection.

Use a PTY-capable test harness for dashboard behavior and an in-memory writer for
exact renderer tests. Do not assert wall-clock rates exactly.

## Recommended Implementation Sequence

1. Define stable telemetry events and sink traits without changing output.
2. Thread sinks through behavior-level APIs and reconcile counters.
3. Implement and test JSON Lines serialization.
4. Implement output selection and forced modes.
5. Implement the four-line terminal state model and `indicatif` renderer.
6. Add bounded/coalescing event transport and failure propagation.
7. Convert existing captured-output tests to explicit human or parsed JSONL.
8. Document the event schema and examples in README.
9. Run format, clippy, workspace tests, hooks, and PTY integration tests.

## Explicit Non-Goals

- a GUI or web dashboard;
- remote telemetry export;
- Prometheus/OpenTelemetry protocols;
- persistence of progress events;
- logging source file contents;
- changing command semantics based on terminal presence; or
- repair/quarantine workflows.

## Definition Of Done

Milestone 10 is complete when terminal users receive a clean four-line live
dashboard, non-terminal users receive versioned JSON Lines, deep modules emit
structured events without rendering concerns, failures cancel safely, existing
human output remains available, and all repository checks pass.
