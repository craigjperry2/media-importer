import argparse
import logging
import sys
import time
from dataclasses import replace
from typing import Iterable

from .catalog import Catalog
from .executor import Executor
from .hashing import calculate_hash
from .models import Action, FileObservation
from .planner import Planner
from .scanner import scan_directory

logging.basicConfig(level=logging.INFO, format="%(levelname)s: %(message)s")

_DB_HELP = "Path to SQLite database"
_SCAN_BATCH_BYTES = 256 * 1024 * 1024


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
        return replace(obs, file_hash=existing_obs.file_hash)

    try:
        file_hash = calculate_hash(obs.file_path)
    except OSError:
        return None

    return replace(obs, file_hash=file_hash)


def _blob_exists(catalog: Catalog, file_hash: str, known_blob_hashes: set[str]) -> bool:
    if file_hash in known_blob_hashes:
        return True
    if catalog.get_blob(file_hash) is None:
        return False
    known_blob_hashes.add(file_hash)
    return True


def _iter_source_observations(sources: Iterable[str]) -> Iterable[FileObservation]:
    for source in sources:
        yield from scan_directory(source)


def _plan_scan_actions(
    catalog: Catalog,
    planner: Planner,
    sources: Iterable[str],
    rehash_all: bool = False,
) -> list[Action]:
    actions: list[Action] = []
    known_blob_hashes: set[str] = set()
    now = time.time()

    for obs in _iter_source_observations(sources):
        hashed_obs = _resolve_observation_hash(catalog, obs, rehash_all)
        if hashed_obs is None or hashed_obs.file_hash is None:
            continue

        file_hash = hashed_obs.file_hash
        blob_exists = _blob_exists(catalog, file_hash, known_blob_hashes)
        actions.extend(
            planner.plan_observation(hashed_obs, blob_exists=blob_exists, now=now)
        )
        known_blob_hashes.add(file_hash)

    return actions


def _execute_scan(
    catalog: Catalog,
    planner: Planner,
    executor: Executor,
    sources: Iterable[str],
    rehash_all: bool = False,
    max_batch_bytes: int = _SCAN_BATCH_BYTES,
) -> bool:
    success = True
    known_blob_hashes: set[str] = set()
    batch_blob_hashes: set[str] = set()
    batch_new_hashes: set[str] = set()
    batch_actions: list[Action] = []
    batch_bytes = 0
    now = time.time()

    def flush_batch() -> None:
        nonlocal \
            batch_actions, \
            batch_blob_hashes, \
            batch_new_hashes, \
            batch_bytes, \
            success
        if not batch_actions:
            return

        result = executor.execute_with_result(batch_actions)
        if result.db_committed:
            known_blob_hashes.update(batch_new_hashes - result.failed_hashes)
        success = success and result.success

        batch_actions = []
        batch_blob_hashes = set()
        batch_new_hashes = set()
        batch_bytes = 0

    for obs in _iter_source_observations(sources):
        hashed_obs = _resolve_observation_hash(catalog, obs, rehash_all)
        if hashed_obs is None or hashed_obs.file_hash is None:
            continue

        file_hash = hashed_obs.file_hash
        blob_exists = file_hash in batch_blob_hashes or _blob_exists(
            catalog, file_hash, known_blob_hashes
        )
        batch_actions.extend(
            planner.plan_observation(hashed_obs, blob_exists=blob_exists, now=now)
        )

        if not blob_exists:
            batch_blob_hashes.add(file_hash)
            batch_new_hashes.add(file_hash)

        batch_bytes += obs.size_bytes
        if batch_bytes >= max_batch_bytes:
            flush_batch()

    flush_batch()
    return success


def _handle_scan(args: argparse.Namespace) -> None:
    catalog = Catalog(args.db, read_only=args.dry_run)
    planner = Planner(catalog, args.store)

    if args.dry_run:
        actions = _plan_scan_actions(catalog, planner, args.source, args.rehash_all)
        print(f"Dry run: Planned {len(actions)} actions.")
        for action in actions:
            print(action)
    else:
        executor = Executor(catalog, args.store)
        if not _execute_scan(catalog, planner, executor, args.source, args.rehash_all):
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
    scan_parser.add_argument("--store", required=True, help="Path to store directory")
    scan_parser.add_argument("--db", required=True, help=_DB_HELP)
    scan_parser.add_argument(
        "--source", required=True, action="append", help="Source directories to scan"
    )
    scan_parser.add_argument(
        "--dry-run", action="store_true", help="Plan only, make no changes"
    )
    scan_parser.add_argument(
        "--rehash-all", action="store_true", help="Force rehashing of all files"
    )

    verify_parser = subparsers.add_parser("verify-store")
    verify_parser.add_argument("--store", required=True, help="Path to store directory")
    verify_parser.add_argument("--db", required=True, help=_DB_HELP)
    verify_parser.add_argument(
        "--dry-run", action="store_true", help="Plan only, make no changes"
    )

    query_parser = subparsers.add_parser("query")
    query_parser.add_argument("--db", required=True, help=_DB_HELP)
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
