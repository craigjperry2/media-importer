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
    FileObservation,
    InsertObservationAction,
    MarkStaleAction,
)
from .scanner import scan_directory


class Planner:
    def __init__(self, catalog: Catalog, store_dir: Path):
        self.catalog = catalog
        self.store_dir = store_dir

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
