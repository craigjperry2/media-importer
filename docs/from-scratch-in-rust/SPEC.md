# Mission Brief: Content Addressable Media Store (CAMS)

## 1. High-Level Objective

Build a high-velocity, deduplicating media ingestion and management CLI tool in Rust. The system must ingest mass media from disparate sources into a content-addressable store (CAS) on ZFS, using a SQLite sidecar for metadata and symlink-based presentation.

## 2. Core Functional Outcomes (Success Metrics)

- **Idempotent Ingestion:** Re-running an import on an 8TB drive must complete in minutes (metadata-check speed), not hours.
- **Mechanical Sympathy:** The source disk must be read exactly once. No reliance on VFS page cache for large-file hashing.
- **Zero-Collision Storage:** Use BLAKE3 for addressing. Store blobs in a 2-level hex-prefix directory (`16^2 x 16^2` fan-out).
- **Relational Integrity:** Maintain a SQLite DB that tracks every unique blob and every original `source_path`.
- **Semantic Linking:** Support many-to-many relationships between blobs (e.g., `thumbnail_of`, `transcode_of`).
- **Non-Destructive Management:** Deletion is a "Mark-and-Sweep" process; symlink generation is an offline "Materialization" process.

## 3. Architectural Constraints

### The I/O Pipeline (The Shell)

- **Mount-Point Workers:** Concurrency is constrained by physical block device. 1 worker for HDDs; configurable N for SSDs.
- **Streaming Buffer:** Use a configurable chunk-based read/hash/write loop.
- **Staging Area:** All writes go to a `.tmp` file in a ZFS staging directory. Only on EOF and successful DB commit is the file atomically `mv`'d to the CAS.
- **Purge on Boot:** The application must clear the staging directory upon startup to recover from crashes.

### The Database (The Persistence)

- **WAL Mode:** Initialize SQLite with `PRAGMA journal_mode=WAL;` and `synchronous=NORMAL;`.
- **Single-Writer Thread:** All database mutations must flow through a single thread via a `crossbeam-channel`.
- **Managed Checkpoints:** The writer thread must batch transactions (by count or time) and explicitly trigger `PASSIVE` checkpoints to prevent WAL ballooning.

### The Interface (Deep & Narrow)

- **CLI Surface:** Expose a narrow CLI surface: `import`, `build-tree`, `gc`, `audit`.
- **Reporting:** Detect TTY. Use `indicatif` for an in-place 4-line dashboard (Ingest speed, Hash speed, Progress). If non-TTY, emit structured, append-only logs to stdout.

## 4. Engineering & Implementation Guidelines

### Rust Design Patterns

- **Isolate Side Effects:** Keep the "Core" (hashing, relationship logic, path sharding) as pure functions. Push the `std::fs` and `rusqlite` calls to the absolute edges of the call stack.
- **Deep Interfaces:** The Ingestor and Store modules should hide their internal threading and batching complexity. The caller should simply provide a source path and a configuration.
- **Immutability:** Force `chmod 444` on all blobs post-ingestion.

### Testing Strategy

- **Integration-First:** Tests must verify outcomes (e.g., "Is the file in the store? Is the DB record correct?") without knowing if the worker used a 4MB or 8MB buffer.
- **Implementation Agnostic:** You should be able to swap the hashing crate or the DB driver without breaking the test suite.
- **Observable Behavior:** Use tracing for deep instrumentation. Integration tests are encouraged to assert against trace logs for verifying internal state transitions that are not exposed via the public API.

## A Final Note on Testing

If the agent is unsure how to test a deep interface, like verifying the checkpointing logic of the writer thread, instruct it to use a "Probe" approach: expose a Trace or Telemetry stream that the test can subscribe to, ensuring the test remains a consumer of behavior rather than a micro-manager of implementation.
