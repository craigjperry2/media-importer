import time
from dataclasses import replace
from pathlib import Path
from typing import List

from .catalog import Catalog
from .hashing import calculate_hash
from .models import (
    Action,
    AddBlobAction,
    Blob,
    CopyFileAction,
    CreateOrUpdateBrowseSymlinkAction,
    DeleteObservationAction,
    FileObservation,
    InsertObservationAction,
    MarkStaleAction,
    RemoveBrowseSymlinkAction,
    UpdateBrowsePathAction,
)
from .scanner import scan_directory


class Planner:
    def __init__(
        self, catalog: Catalog, store_dir: Path, browse_root: Path | None = None
    ):
        self.catalog = catalog
        self.store_dir = store_dir.resolve()
        self.browse_root = browse_root.resolve() if browse_root is not None else None

    def validate_source_roots(self, source_roots: list[Path]) -> list[Path]:
        resolved_roots = [source.resolve() for source in source_roots]
        self._raise_for_overlaps(resolved_roots)
        existing_roots = self.catalog.get_source_roots()
        for root in resolved_roots:
            for existing_root in existing_roots:
                if root == existing_root:
                    continue
                if _paths_overlap(root, existing_root):
                    raise ValueError(
                        f"Source roots overlap: {root} and {existing_root}"
                    )
        return resolved_roots

    def plan_observation(
        self, obs: FileObservation, blob_exists: bool, now: float
    ) -> List[Action]:
        file_hash = obs.file_hash
        assert file_hash is not None
        observation = replace(obs, last_seen_at=now)
        actions: List[Action] = []
        if not blob_exists:
            shard = file_hash[:2]
            store_path = Path(shard) / f"{file_hash}{observation.file_format}"
            blob = Blob(
                file_hash=file_hash,
                size_bytes=observation.size_bytes,
                store_path=store_path,
                first_seen_at=now,
            )
            actions.append(AddBlobAction(blob=blob))
            actions.append(
                CopyFileAction(
                    source_path=observation.file_path,
                    store_path=store_path,
                    file_hash=file_hash,
                    size_bytes=observation.size_bytes,
                )
            )
        actions.append(InsertObservationAction(observation=observation))
        return actions

    def plan_verify_store(self) -> List[Action]:
        actions: List[Action] = []
        blobs = self.catalog.get_all_blobs()

        for blob in blobs:
            full_path = self.store_dir / blob.store_path
            if not full_path.exists():
                for observation in self.catalog.get_observations_for_hash(
                    blob.file_hash
                ):
                    if (
                        observation.browse_root is not None
                        and observation.browse_rel_path is not None
                    ):
                        actions.append(
                            RemoveBrowseSymlinkAction(
                                browse_root=observation.browse_root,
                                browse_rel_path=observation.browse_rel_path,
                            )
                        )
                actions.append(MarkStaleAction(file_path=blob.store_path))

        store_files: set[Path] = set()
        if self.store_dir.exists():
            for obs in scan_directory(self.store_dir):
                store_files.add(Path(obs.file_path).resolve())

        indexed_files = {(self.store_dir / b.store_path).resolve() for b in blobs}
        unindexed = store_files - indexed_files

        now = time.time()
        for unindexed_file in unindexed:
            try:
                file_hash = calculate_hash(unindexed_file)
                stat = unindexed_file.stat()
            except OSError:
                continue

            existing_blob = self.catalog.get_blob(file_hash)
            if not existing_blob:
                rel_path = unindexed_file.relative_to(self.store_dir)
                blob = Blob(
                    file_hash=file_hash,
                    size_bytes=stat.st_size,
                    store_path=rel_path,
                    first_seen_at=now,
                )
                actions.append(AddBlobAction(blob=blob))

        return actions

    def plan_browse_reconciliation(
        self,
        source_roots: list[Path],
        scanned_at: float,
        current_scan_observations: list[FileObservation],
    ) -> list[Action]:
        actions: list[Action] = []
        resolved_roots = [root.resolve() for root in source_roots]
        stale_observations = self.catalog.get_stale_observations(
            resolved_roots, scanned_at
        )
        current_paths = {
            observation.file_path for observation in current_scan_observations
        }
        stale_observations = [
            observation
            for observation in stale_observations
            if observation.file_path not in current_paths
        ]

        stale_paths = {observation.file_path for observation in stale_observations}
        for observation in stale_observations:
            if (
                observation.browse_root is not None
                and observation.browse_rel_path is not None
            ):
                actions.append(
                    RemoveBrowseSymlinkAction(
                        browse_root=observation.browse_root,
                        browse_rel_path=observation.browse_rel_path,
                    )
                )
            actions.append(DeleteObservationAction(file_path=observation.file_path))

        merged: dict[Path, FileObservation] = {
            observation.file_path: observation
            for observation in self.catalog.get_all_live_observations()
            if observation.file_path not in stale_paths
        }
        for observation in current_scan_observations:
            merged[observation.file_path] = observation

        for observation in merged.values():
            if observation.file_hash is None or observation.source_rel_path is None:
                continue
            browse_root = observation.browse_root
            if browse_root is None:
                continue
            desired_rel_path = _browse_rel_path(
                observation.source_rel_path, observation.file_hash
            )
            full_store_path = self.store_dir / _store_path_for_observation(observation)
            if (
                observation.browse_rel_path != desired_rel_path
                or observation.browse_root != browse_root
            ):
                if observation.browse_rel_path is not None:
                    actions.append(
                        RemoveBrowseSymlinkAction(
                            browse_root=browse_root,
                            browse_rel_path=observation.browse_rel_path,
                        )
                    )
                actions.append(
                    UpdateBrowsePathAction(
                        file_path=observation.file_path,
                        browse_root=browse_root,
                        browse_rel_path=desired_rel_path,
                    )
                )
            actions.append(
                CreateOrUpdateBrowseSymlinkAction(
                    browse_root=browse_root,
                    browse_rel_path=desired_rel_path,
                    target_store_path=full_store_path,
                )
            )

        return actions

    def _raise_for_overlaps(self, source_roots: list[Path]) -> None:
        for index, root in enumerate(source_roots):
            for other in source_roots[index + 1 :]:
                if root == other:
                    continue
                if _paths_overlap(root, other):
                    raise ValueError(f"Source roots overlap: {root} and {other}")


def _paths_overlap(first: Path, second: Path) -> bool:
    return _is_relative_to(first, second) or _is_relative_to(second, first)


def _is_relative_to(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False


def _browse_rel_path(source_rel_path: Path, file_hash: str) -> Path:
    suffix = file_hash[:7]
    return source_rel_path.with_name(
        f"{source_rel_path.stem}_{suffix}{source_rel_path.suffix}"
    )


def _store_path_for_observation(observation: FileObservation) -> Path:
    assert observation.file_hash is not None
    return Path(observation.file_hash[:2]) / (
        f"{observation.file_hash}{observation.file_format}"
    )
