import logging
import os
import shutil
import sqlite3
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import List, Set

from .catalog import Catalog
from .models import (
    Action,
    AddBlobAction,
    CopyFileAction,
    CreateOrUpdateBrowseSymlinkAction,
    DeleteObservationAction,
    InsertObservationAction,
    MarkStaleAction,
    RecordSourceRootAction,
    RemoveBrowseSymlinkAction,
    UpdateBrowsePathAction,
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
    def __init__(
        self, catalog: Catalog, store_dir: Path, browse_root: Path | None = None
    ):
        self.catalog = catalog
        self.store_dir = store_dir.resolve()
        self.browse_root = browse_root.resolve() if browse_root is not None else None

    def _copy_files(
        self,
        actions: List[Action],
        on_copy_processed: Callable[[], None] | None = None,
    ) -> Set[str]:
        failed_hashes: Set[str] = set()
        for action in actions:
            if not isinstance(action, CopyFileAction):
                continue
            full_store_path = self.store_dir / action.store_path
            try:
                full_store_path.parent.mkdir(parents=True, exist_ok=True)
                tmp_path = Path(f"{full_store_path}.tmp")
                shutil.copy2(action.source_path, tmp_path)
                with tmp_path.open("ab") as f:
                    os.fsync(f.fileno())
                tmp_path.replace(full_store_path)
            except Exception as e:
                logger.error(
                    f"Failed to copy {action.source_path} to {full_store_path}: {e}"
                )
                failed_hashes.add(action.file_hash)
            finally:
                if on_copy_processed is not None:
                    on_copy_processed()
        return failed_hashes

    def _apply_browse_filesystem_actions(self, actions: List[Action]) -> None:
        for action in actions:
            if isinstance(action, RemoveBrowseSymlinkAction):
                self._remove_browse_symlink(action)
            elif isinstance(action, CreateOrUpdateBrowseSymlinkAction):
                self._create_or_update_browse_symlink(action)

    def _create_or_update_browse_symlink(
        self, action: CreateOrUpdateBrowseSymlinkAction
    ) -> None:
        link_path = self._resolve_browse_link_path(
            action.browse_root, action.browse_rel_path
        )
        target_path = action.target_store_path.resolve()
        link_path.parent.mkdir(parents=True, exist_ok=True)
        relative_target = Path(os.path.relpath(target_path, link_path.parent))

        if link_path.is_symlink():
            if Path(os.readlink(link_path)) == relative_target:
                return
            link_path.unlink()
        elif link_path.exists():
            raise FileExistsError(
                f"Refusing to replace non-symlink browse entry: {link_path}"
            )

        link_path.symlink_to(relative_target)

    def _remove_browse_symlink(self, action: RemoveBrowseSymlinkAction) -> None:
        link_path = self._resolve_browse_link_path(
            action.browse_root, action.browse_rel_path
        )
        if link_path.is_symlink():
            link_path.unlink()
            self._prune_empty_browse_dirs(link_path.parent, action.browse_root)
        elif link_path.exists():
            raise FileExistsError(
                f"Refusing to remove non-symlink browse entry: {link_path}"
            )

    def _resolve_browse_link_path(
        self, browse_root: Path, browse_rel_path: Path
    ) -> Path:
        if browse_rel_path.is_absolute():
            raise ValueError(
                f"Unsafe browse_rel_path (must be relative): {browse_rel_path}"
            )
        if any(part == ".." for part in browse_rel_path.parts):
            raise ValueError(
                f"Unsafe browse_rel_path (must not contain '..'): {browse_rel_path}"
            )

        resolved_root = browse_root.resolve()
        unresolved_link_path = resolved_root / browse_rel_path
        resolved_parent = unresolved_link_path.parent.resolve(strict=False)
        resolved_link_path = resolved_parent / unresolved_link_path.name
        try:
            resolved_link_path.relative_to(resolved_root)
        except ValueError as e:
            raise ValueError(
                f"Unsafe browse_rel_path escapes browse_root: {browse_rel_path}"
            ) from e

        return resolved_link_path

    def _prune_empty_browse_dirs(self, path: Path, browse_root: Path) -> None:
        current = path
        root = browse_root.resolve()
        while current.resolve() != root:
            try:
                current.rmdir()
            except OSError:
                return
            current = current.parent

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
                        str(action.blob.store_path),
                        action.blob.first_seen_at,
                    ),
                )
            elif isinstance(action, InsertObservationAction):
                obs = action.observation
                if obs.file_hash in failed_hashes:
                    continue
                conn.execute(
                    """
                    INSERT INTO source_files (
                        file_path, file_name, file_format, size_bytes, mtime,
                        file_hash, last_seen_at, source_root, source_rel_path,
                        browse_root, browse_rel_path
                    )
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    ON CONFLICT(file_path) DO UPDATE SET
                        file_name=excluded.file_name,
                        file_format=excluded.file_format,
                        size_bytes=excluded.size_bytes,
                        mtime=excluded.mtime,
                        file_hash=excluded.file_hash,
                        last_seen_at=excluded.last_seen_at,
                        source_root=excluded.source_root,
                        source_rel_path=excluded.source_rel_path,
                        browse_root=excluded.browse_root,
                        browse_rel_path=excluded.browse_rel_path
                """,
                    (
                        str(obs.file_path),
                        obs.file_name,
                        obs.file_format,
                        obs.size_bytes,
                        obs.mtime,
                        obs.file_hash,
                        obs.last_seen_at,
                        str(obs.source_root) if obs.source_root else None,
                        str(obs.source_rel_path) if obs.source_rel_path else None,
                        str(obs.browse_root) if obs.browse_root else None,
                        str(obs.browse_rel_path) if obs.browse_rel_path else None,
                    ),
                )
            elif isinstance(action, MarkStaleAction):
                conn.execute(
                    "DELETE FROM blobs WHERE store_path = ?", (str(action.file_path),)
                )
            elif isinstance(action, RecordSourceRootAction):
                conn.execute(
                    "INSERT OR IGNORE INTO source_roots (source_root) VALUES (?)",
                    (str(action.source_root),),
                )
            elif isinstance(action, UpdateBrowsePathAction):
                conn.execute(
                    """
                    UPDATE source_files
                    SET browse_root = ?, browse_rel_path = ?
                    WHERE file_path = ?
                    """,
                    (
                        str(action.browse_root) if action.browse_root else None,
                        str(action.browse_rel_path) if action.browse_rel_path else None,
                        str(action.file_path),
                    ),
                )
            elif isinstance(action, DeleteObservationAction):
                conn.execute(
                    "DELETE FROM source_files WHERE file_path = ?",
                    (str(action.file_path),),
                )

    def execute_with_result(
        self,
        actions: List[Action],
        on_copy_processed: Callable[[], None] | None = None,
    ) -> ExecutionResult:
        failed_hashes = self._copy_files(actions, on_copy_processed=on_copy_processed)
        db_committed = False
        try:
            self._apply_browse_filesystem_actions(actions)
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
