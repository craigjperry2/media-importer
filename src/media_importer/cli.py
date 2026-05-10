import argparse
import logging
import sys
import time
from collections.abc import Callable, Iterable, Iterator
from dataclasses import dataclass, field, replace
from pathlib import Path

from .catalog import Catalog
from .executor import ExecutionResult, Executor
from .hashing import calculate_hash
from .models import (
    Action,
    CopyFileAction,
    FileObservation,
    InsertObservationAction,
    RecordSourceRootAction,
)
from .planner import Planner
from .scanner import scan_directory

logging.basicConfig(level=logging.INFO, format="%(levelname)s: %(message)s")

_DB_HELP = "Path to SQLite database"
_SCAN_BATCH_BYTES = 256 * 1024 * 1024
_SCAN_PROGRESS_EVERY = 100
_COPY_PROGRESS_EVERY = 25


@dataclass
class ScanProgress:
    processed_files: int = 0
    skipped_files: int = 0
    planned_actions: int = 0
    new_files_to_copy: int = 0
    observations_to_record: int = 0


class ScanProgressReporter:
    def __init__(
        self,
        plan_progress_every: int = _SCAN_PROGRESS_EVERY,
        copy_progress_every: int = _COPY_PROGRESS_EVERY,
    ):
        self.plan_progress_every = plan_progress_every
        self.copy_progress_every = copy_progress_every

    def planning_started(self, sources: list[Path], dry_run: bool) -> None:
        mode = "dry-run planning" if dry_run else "scan"
        self._emit(f"Starting {mode} across {len(sources)} source(s)")

    def source_started(self, source: Path) -> None:
        self._emit(f"Scanning source: {source}")

    def planning_progress(self, progress: ScanProgress) -> None:
        if progress.processed_files % self.plan_progress_every != 0:
            return
        self._emit(f"Planning progress: {_format_progress(progress)}")

    def planning_complete(self, progress: ScanProgress) -> None:
        self._emit(f"Planning complete: {_format_progress(progress)}")

    def execution_started(self) -> None:
        self._emit("Executing scan batches")

    def copy_progress(self, processed_copy_actions: int) -> None:
        if processed_copy_actions % self.copy_progress_every != 0:
            return
        self._emit(
            f"Execution progress: processed {processed_copy_actions} file copies"
        )

    def execution_complete(
        self,
        progress: ScanProgress,
        processed_copy_actions: int,
        result: ExecutionResult,
    ) -> None:
        if result.success:
            self._emit(
                f"Scan complete: {_format_progress(progress)}; copied "
                f"{processed_copy_actions} new files"
            )
            return

        self._emit(
            f"Scan finished with errors: {_format_progress(progress)}; "
            f"processed {processed_copy_actions} of {progress.new_files_to_copy} "
            "file copies"
        )

    def dry_run_complete(self, progress: ScanProgress) -> None:
        self._emit(f"Dry run complete: {_format_progress(progress)}")

    def _emit(self, message: str) -> None:
        print(message, file=sys.stderr)


def _format_progress(progress: ScanProgress) -> str:
    summary = (
        f"{progress.processed_files} files processed, "
        f"{progress.new_files_to_copy} new files to copy, "
        f"{progress.observations_to_record} observations to record, "
        f"{progress.planned_actions} planned actions"
    )
    if progress.skipped_files:
        summary += f", {progress.skipped_files} skipped"
    return summary


def _count_scan_actions(actions: list[Action]) -> tuple[int, int]:
    copy_actions = 0
    observation_actions = 0
    for action in actions:
        if isinstance(action, CopyFileAction):
            copy_actions += 1
        elif isinstance(action, InsertObservationAction):
            observation_actions += 1
    return copy_actions, observation_actions


def _resolve_observation_hash(
    catalog: Catalog, obs: FileObservation, rehash_all: bool
) -> FileObservation | None:
    existing_obs = catalog.get_observation(obs.file_path)
    if (
        existing_obs
        and existing_obs.file_hash is not None
        and not rehash_all
        and existing_obs.size_bytes == obs.size_bytes
        and existing_obs.mtime == obs.mtime
    ):
        return replace(
            obs,
            file_hash=existing_obs.file_hash,
            browse_root=obs.browse_root or existing_obs.browse_root,
            browse_rel_path=existing_obs.browse_rel_path,
        )

    try:
        file_hash = calculate_hash(obs.file_path)
    except OSError:
        return None

    return replace(
        obs,
        file_hash=file_hash,
        browse_root=obs.browse_root
        or (existing_obs.browse_root if existing_obs else None),
        browse_rel_path=existing_obs.browse_rel_path if existing_obs else None,
    )


def _blob_exists(catalog: Catalog, file_hash: str, known_blob_hashes: set[str]) -> bool:
    if file_hash in known_blob_hashes:
        return True
    if catalog.get_blob(file_hash) is None:
        return False
    known_blob_hashes.add(file_hash)
    return True


def _iter_source_observations(
    sources: Iterable[Path],
) -> Iterator[tuple[Path, FileObservation]]:
    for source in sources:
        resolved_source = source.resolve()
        for observation in scan_directory(resolved_source):
            yield source, observation


def _prepare_scan_sources(planner: Planner, sources: Iterable[Path]) -> list[Path]:
    source_list = list(sources)
    if planner.browse_root is None:
        return source_list
    return planner.validate_source_roots(source_list)


def _enrich_observation(
    obs: FileObservation, source_root: Path, browse_root: Path | None
) -> FileObservation:
    resolved_source_root = source_root.resolve()
    resolved_file_path = obs.file_path.resolve()
    return replace(
        obs,
        file_path=resolved_file_path,
        source_root=resolved_source_root,
        source_rel_path=resolved_file_path.relative_to(resolved_source_root),
        browse_root=browse_root,
    )


@dataclass
class _ScanBatch:
    actions: list[Action] = field(default_factory=list)  # pyright: ignore[reportUnknownVariableType]
    blob_hashes: set[str] = field(default_factory=set)  # pyright: ignore[reportUnknownVariableType]
    new_hashes: set[str] = field(default_factory=set)  # pyright: ignore[reportUnknownVariableType]
    bytes: int = 0


def _flush_scan_batch(
    batch: _ScanBatch,
    executor: Executor,
    known_blob_hashes: set[str],
    failed_hashes: set[str],
    on_copy_processed: Callable[[], None],
) -> bool:
    if not batch.actions:
        return True
    result = executor.execute_with_result(
        batch.actions, on_copy_processed=on_copy_processed
    )
    if result.db_committed:
        known_blob_hashes.update(batch.new_hashes - result.failed_hashes)
    failed_hashes.update(result.failed_hashes)
    batch.actions = []
    batch.blob_hashes = set()
    batch.new_hashes = set()
    batch.bytes = 0
    return result.db_committed


def _add_obs_to_batch(
    obs: FileObservation,
    catalog: Catalog,
    planner: Planner,
    rehash_all: bool,
    batch: _ScanBatch,
    known_blob_hashes: set[str],
    progress: ScanProgress,
    now: float,
) -> FileObservation | None:
    hashed_obs = _resolve_observation_hash(catalog, obs, rehash_all)
    if hashed_obs is None or hashed_obs.file_hash is None:
        progress.skipped_files += 1
        return None
    file_hash = hashed_obs.file_hash
    blob_exists = file_hash in batch.blob_hashes or _blob_exists(
        catalog, file_hash, known_blob_hashes
    )
    observation_actions = planner.plan_observation(
        hashed_obs, blob_exists=blob_exists, now=now
    )
    batch.actions.extend(observation_actions)
    copy_count, insert_count = _count_scan_actions(observation_actions)
    progress.planned_actions += len(observation_actions)
    progress.new_files_to_copy += copy_count
    progress.observations_to_record += insert_count
    if not blob_exists:
        batch.blob_hashes.add(file_hash)
        batch.new_hashes.add(file_hash)
    batch.bytes += obs.size_bytes
    return replace(hashed_obs, last_seen_at=now)


def _plan_scan_actions_with_progress(
    catalog: Catalog,
    planner: Planner,
    sources: Iterable[Path],
    rehash_all: bool = False,
    reporter: ScanProgressReporter | None = None,
) -> tuple[list[Action], ScanProgress]:
    actions: list[Action] = []
    known_blob_hashes: set[str] = set()
    progress = ScanProgress()
    now = time.time()
    current_source: Path | None = None
    resolved_sources = _prepare_scan_sources(planner, sources)
    current_scan_observations: list[FileObservation] = []
    if planner.browse_root is not None:
        root_actions: list[Action] = [
            RecordSourceRootAction(source_root=source.resolve())
            for source in resolved_sources
        ]
        actions.extend(root_actions)
        progress.planned_actions += len(root_actions)

    for source, obs in _iter_source_observations(resolved_sources):
        if reporter is not None and source != current_source:
            reporter.source_started(source)
            current_source = source

        progress.processed_files += 1
        enriched_obs = _enrich_observation(obs, source, planner.browse_root)
        hashed_obs = _resolve_observation_hash(catalog, enriched_obs, rehash_all)
        if hashed_obs is None or hashed_obs.file_hash is None:
            progress.skipped_files += 1
            if reporter is not None:
                reporter.planning_progress(progress)
            continue

        file_hash = hashed_obs.file_hash
        blob_exists = _blob_exists(catalog, file_hash, known_blob_hashes)
        observation_actions = planner.plan_observation(
            hashed_obs, blob_exists=blob_exists, now=now
        )
        actions.extend(observation_actions)
        current_scan_observations.append(replace(hashed_obs, last_seen_at=now))
        copy_actions, insert_actions = _count_scan_actions(observation_actions)
        progress.planned_actions += len(observation_actions)
        progress.new_files_to_copy += copy_actions
        progress.observations_to_record += insert_actions
        known_blob_hashes.add(file_hash)
        if reporter is not None:
            reporter.planning_progress(progress)

    if planner.browse_root is not None:
        browse_actions = planner.plan_browse_reconciliation(
            resolved_sources, now, current_scan_observations
        )
        actions.extend(browse_actions)
        progress.planned_actions += len(browse_actions)

    if reporter is not None:
        reporter.planning_complete(progress)
    return actions, progress


def _plan_scan_actions(  # pyright: ignore[reportUnusedFunction] used in test_planner.py
    catalog: Catalog,
    planner: Planner,
    sources: Iterable[Path],
    rehash_all: bool = False,
    reporter: ScanProgressReporter | None = None,
) -> list[Action]:
    actions, _ = _plan_scan_actions_with_progress(
        catalog,
        planner,
        sources,
        rehash_all,
        reporter=reporter,
    )
    return actions


def _execute_scan(
    catalog: Catalog,
    planner: Planner,
    executor: Executor,
    sources: Iterable[Path],
    rehash_all: bool = False,
    max_batch_bytes: int = _SCAN_BATCH_BYTES,
    reporter: ScanProgressReporter | None = None,
) -> tuple[ExecutionResult, ScanProgress, int]:
    known_blob_hashes: set[str] = set()
    batch = _ScanBatch()
    copy_actions_processed = 0
    progress = ScanProgress()
    now = time.time()
    current_source: Path | None = None
    db_committed = True
    failed_hashes: set[str] = set()
    resolved_sources = _prepare_scan_sources(planner, sources)
    current_scan_observations: list[FileObservation] = []
    if planner.browse_root is not None:
        root_actions: list[Action] = [
            RecordSourceRootAction(source_root=source.resolve())
            for source in resolved_sources
        ]
        db_committed = db_committed and executor.execute(root_actions)
        progress.planned_actions += len(root_actions)

    def on_copy_processed() -> None:
        nonlocal copy_actions_processed
        copy_actions_processed += 1
        if reporter is not None:
            reporter.copy_progress(copy_actions_processed)

    if reporter is not None:
        reporter.execution_started()

    for source, obs in _iter_source_observations(resolved_sources):
        if reporter is not None and source != current_source:
            reporter.source_started(source)
            current_source = source

        progress.processed_files += 1
        enriched_obs = _enrich_observation(obs, source, planner.browse_root)
        planned_obs = _add_obs_to_batch(
            enriched_obs,
            catalog,
            planner,
            rehash_all,
            batch,
            known_blob_hashes,
            progress,
            now,
        )
        if planned_obs is not None:
            current_scan_observations.append(planned_obs)
        if reporter is not None:
            reporter.planning_progress(progress)
        if planned_obs is not None and batch.bytes >= max_batch_bytes:
            db_committed = db_committed and _flush_scan_batch(
                batch, executor, known_blob_hashes, failed_hashes, on_copy_processed
            )

    db_committed = db_committed and _flush_scan_batch(
        batch, executor, known_blob_hashes, failed_hashes, on_copy_processed
    )
    result = ExecutionResult(
        failed_hashes=frozenset(failed_hashes),
        db_committed=db_committed,
    )
    if result.db_committed and planner.browse_root is not None:
        browse_observations = [
            observation
            for observation in current_scan_observations
            if observation.file_hash not in failed_hashes
        ]
        browse_actions = planner.plan_browse_reconciliation(
            resolved_sources, now, browse_observations
        )
        progress.planned_actions += len(browse_actions)
        browse_result = executor.execute_with_result(browse_actions)
        result = ExecutionResult(
            failed_hashes=result.failed_hashes | browse_result.failed_hashes,
            db_committed=result.db_committed and browse_result.db_committed,
        )
    if reporter is not None:
        reporter.planning_complete(progress)
        reporter.execution_complete(progress, copy_actions_processed, result)
    return result, progress, copy_actions_processed


def _handle_scan(args: argparse.Namespace) -> None:
    catalog = Catalog(args.db, read_only=args.dry_run)
    planner = Planner(catalog, args.store, browse_root=args.browse_root)
    reporter = ScanProgressReporter()
    reporter.planning_started(args.source, dry_run=args.dry_run)

    if args.dry_run:
        actions, progress = _plan_scan_actions_with_progress(
            catalog,
            planner,
            args.source,
            args.rehash_all,
            reporter=reporter,
        )
        reporter.dry_run_complete(progress)
        print(f"Dry run: Planned {len(actions)} actions.")
        for action in actions:
            print(action)
    else:
        executor = Executor(catalog, args.store, browse_root=args.browse_root)
        result, _, _ = _execute_scan(
            catalog,
            planner,
            executor,
            args.source,
            args.rehash_all,
            reporter=reporter,
        )
        if not result.success:
            sys.exit(1)


def _handle_verify_store(args: argparse.Namespace) -> None:
    catalog = Catalog(args.db, read_only=args.dry_run)
    planner = Planner(catalog, args.store)
    actions = planner.plan_verify_store()

    if args.dry_run:
        print(f"Dry run: Planned {len(actions)} actions.")
        for action in actions:
            print(action)
    else:
        executor = Executor(catalog, args.store)
        if not executor.execute(actions):
            sys.exit(1)


def _handle_query(args: argparse.Namespace) -> None:
    catalog = Catalog(args.db, read_only=True)
    query = "SELECT * FROM source_files WHERE 1=1"
    params: list[str] = []
    if args.ext:
        query += " AND file_format = ?"
        params.append(args.ext)
    if args.name:
        query += " AND file_name LIKE ?"
        params.append(f"%{args.name}%")
    if args.hash:
        query += " AND file_hash = ?"
        params.append(args.hash)

    cursor = catalog.conn.execute(query, params)
    for row in cursor:
        print(dict(row))


def main() -> None:
    parser = argparse.ArgumentParser(description="Media Consolidator & Cataloger")
    subparsers = parser.add_subparsers(dest="command", required=True)

    scan_parser = subparsers.add_parser("scan")
    scan_parser.add_argument(
        "--store", type=Path, required=True, help="Path to store directory"
    )
    scan_parser.add_argument("--db", type=Path, required=True, help=_DB_HELP)
    scan_parser.add_argument(
        "--source",
        type=Path,
        required=True,
        action="append",
        help="Source directories to scan",
    )
    scan_parser.add_argument(
        "--dry-run", action="store_true", help="Plan only, make no changes"
    )
    scan_parser.add_argument(
        "--browse-root",
        type=Path,
        help="Optional root for a source-relative symlink browse tree",
    )
    scan_parser.add_argument(
        "--rehash-all", action="store_true", help="Force rehashing of all files"
    )

    verify_parser = subparsers.add_parser("verify-store")
    verify_parser.add_argument(
        "--store", type=Path, required=True, help="Path to store directory"
    )
    verify_parser.add_argument("--db", type=Path, required=True, help=_DB_HELP)
    verify_parser.add_argument(
        "--dry-run", action="store_true", help="Plan only, make no changes"
    )

    query_parser = subparsers.add_parser("query")
    query_parser.add_argument("--db", type=Path, required=True, help=_DB_HELP)
    query_parser.add_argument("--ext", help="Filter by extension")
    query_parser.add_argument("--name", help="Filter by file name")
    query_parser.add_argument("--hash", help="Filter by file hash")

    args = parser.parse_args()

    if args.command == "scan":
        _handle_scan(args)
    elif args.command == "verify-store":
        _handle_verify_store(args)
    elif args.command == "query":
        _handle_query(args)


if __name__ == "__main__":
    main()
