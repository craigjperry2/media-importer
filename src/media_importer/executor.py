import logging
import os
import shutil
import sqlite3
from collections.abc import Callable
from dataclasses import dataclass
from typing import List, Set

from .catalog import Catalog
from .models import (
    Action,
    AddBlobAction,
    CopyFileAction,
    InsertObservationAction,
    MarkStaleAction,
)

logger = logging.getLogger(__name__)


@dataclass(frozen=True)
class ExecutionResult:
    failed_hashes: frozenset[str]
    db_committed: bool

    @property
    def success(self) -> bool:
        return self.db_committed and not self.failed_hashes


class Executor:
    def __init__(self, catalog: Catalog, store_dir: str):
        self.catalog = catalog
        self.store_dir = store_dir

    def _copy_files(
        self,
        actions: List[Action],
        on_copy_processed: Callable[[], None] | None = None,
    ) -> Set[str]:
        failed_hashes: Set[str] = set()
        for action in actions:
            if not isinstance(action, CopyFileAction):
                continue
            full_store_path = os.path.join(self.store_dir, action.store_path)
            try:
                os.makedirs(os.path.dirname(full_store_path), exist_ok=True)
                tmp_path = full_store_path + ".tmp"
                shutil.copy2(action.source_path, tmp_path)
                with open(tmp_path, "ab") as f:
                    os.fsync(f.fileno())
                os.replace(tmp_path, full_store_path)
            except Exception as e:
                logger.error(
                    f"Failed to copy {action.source_path} to {full_store_path}: {e}"
                )
                failed_hashes.add(action.file_hash)
            finally:
                if on_copy_processed is not None:
                    on_copy_processed()
        return failed_hashes

    def _db_operations(
        self,
        conn: sqlite3.Connection,
        actions: List[Action],
        failed_hashes: Set[str],
    ) -> None:
        for action in actions:
            if isinstance(action, AddBlobAction):
                if action.blob.file_hash in failed_hashes:
                    continue
                conn.execute(
                    "INSERT OR IGNORE INTO blobs (file_hash, size_bytes, store_path, first_seen_at) VALUES (?, ?, ?, ?)",
                    (
                        action.blob.file_hash,
                        action.blob.size_bytes,
                        action.blob.store_path,
                        action.blob.first_seen_at,
                    ),
                )
            elif isinstance(action, InsertObservationAction):
                obs = action.observation
                if obs.file_hash in failed_hashes:
                    continue
                conn.execute(
                    """
                    INSERT INTO source_files (file_path, file_name, file_format, size_bytes, mtime, file_hash, last_seen_at)
                    VALUES (?, ?, ?, ?, ?, ?, ?)
                    ON CONFLICT(file_path) DO UPDATE SET
                        file_name=excluded.file_name,
                        file_format=excluded.file_format,
                        size_bytes=excluded.size_bytes,
                        mtime=excluded.mtime,
                        file_hash=excluded.file_hash,
                        last_seen_at=excluded.last_seen_at
                """,
                    (
                        obs.file_path,
                        obs.file_name,
                        obs.file_format,
                        obs.size_bytes,
                        obs.mtime,
                        obs.file_hash,
                        obs.last_seen_at,
                    ),
                )
            elif isinstance(action, MarkStaleAction):
                conn.execute(
                    "DELETE FROM blobs WHERE store_path = ?", (action.file_path,)
                )

    def execute_with_result(
        self,
        actions: List[Action],
        on_copy_processed: Callable[[], None] | None = None,
    ) -> ExecutionResult:
        failed_hashes = self._copy_files(actions, on_copy_processed=on_copy_processed)
        db_committed = False
        try:
            self.catalog.execute_in_transaction(
                lambda conn: self._db_operations(conn, actions, failed_hashes)
            )
            db_committed = True
        except Exception as e:
            logger.error(f"Failed to update database: {e}")
        return ExecutionResult(
            failed_hashes=frozenset(failed_hashes),
            db_committed=db_committed,
        )

    def execute(self, actions: List[Action]) -> bool:
        return self.execute_with_result(actions).success
