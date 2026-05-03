import os
import time
from typing import Iterable, List

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
    def __init__(self, catalog: Catalog, store_dir: str):
        self.catalog = catalog
        self.store_dir = store_dir

    def _process_observation(
        self, obs: FileObservation, rehash_all: bool, now: float
    ) -> List[Action] | None:
        existing_obs = self.catalog.get_observation(obs.file_path)

        file_hash: str | None
        if (
            existing_obs
            and not rehash_all
            and existing_obs.size_bytes == obs.size_bytes
            and existing_obs.mtime == obs.mtime
        ):
            file_hash = existing_obs.file_hash
        else:
            try:
                file_hash = calculate_hash(obs.file_path)
            except OSError:
                return None

        assert file_hash is not None
        obs.file_hash = file_hash
        obs.last_seen_at = now

        actions: List[Action] = []
        if not self.catalog.get_blob(file_hash):
            shard = file_hash[:2]
            store_path = os.path.join(shard, f"{file_hash}{obs.file_format}")
            blob = Blob(
                file_hash=file_hash,
                size_bytes=obs.size_bytes,
                store_path=store_path,
                first_seen_at=now,
            )
            actions.append(AddBlobAction(blob=blob))
            actions.append(
                CopyFileAction(
                    source_path=obs.file_path,
                    store_path=store_path,
                    file_hash=file_hash,
                    size_bytes=obs.size_bytes,
                )
            )
        actions.append(InsertObservationAction(observation=obs))
        return actions

    def plan_scan(
        self, sources: Iterable[str], rehash_all: bool = False
    ) -> List[Action]:
        actions: List[Action] = []
        now = time.time()
        for source in sources:
            for obs in scan_directory(source):
                result = self._process_observation(obs, rehash_all, now)
                if result is not None:
                    actions.extend(result)
        return actions

    def plan_verify_store(self) -> List[Action]:
        actions: List[Action] = []
        blobs = self.catalog.get_all_blobs()

        for blob in blobs:
            full_path = os.path.join(self.store_dir, blob.store_path)
            if not os.path.exists(full_path):
                actions.append(MarkStaleAction(file_path=blob.store_path))

        store_files: set[str] = set()
        if os.path.exists(self.store_dir):
            for obs in scan_directory(self.store_dir):
                store_files.add(obs.file_path)

        indexed_files = {
            os.path.normpath(os.path.join(self.store_dir, b.store_path)) for b in blobs
        }
        unindexed = store_files - indexed_files

        now = time.time()
        for unindexed_file in unindexed:
            try:
                file_hash = calculate_hash(unindexed_file)
                stat = os.stat(unindexed_file)
            except OSError:
                continue

            existing_blob = self.catalog.get_blob(file_hash)
            if not existing_blob:
                rel_path = os.path.relpath(unindexed_file, self.store_dir)
                blob = Blob(
                    file_hash=file_hash,
                    size_bytes=stat.st_size,
                    store_path=rel_path,
                    first_seen_at=now,
                )
                actions.append(AddBlobAction(blob=blob))

        return actions
