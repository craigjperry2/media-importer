from dataclasses import dataclass
from typing import Optional
from pathlib import Path


@dataclass
class Blob:
    file_hash: str
    size_bytes: int
    store_path: Path
    first_seen_at: float


@dataclass
class FileObservation:
    file_path: Path
    file_name: str
    file_format: str
    size_bytes: int
    mtime: float
    file_hash: Optional[str]
    last_seen_at: float
    source_root: Path | None = None
    source_rel_path: Path | None = None
    browse_root: Path | None = None
    browse_rel_path: Path | None = None


@dataclass
class Action:
    pass


@dataclass
class CopyFileAction(Action):
    source_path: Path
    store_path: Path
    file_hash: str
    size_bytes: int


@dataclass
class InsertObservationAction(Action):
    observation: FileObservation


@dataclass
class MarkStaleAction(Action):
    file_path: Path


@dataclass
class AddBlobAction(Action):
    blob: Blob


@dataclass
class RecordSourceRootAction(Action):
    source_root: Path


@dataclass
class CreateOrUpdateBrowseSymlinkAction(Action):
    browse_root: Path
    browse_rel_path: Path
    target_store_path: Path


@dataclass
class RemoveBrowseSymlinkAction(Action):
    browse_root: Path
    browse_rel_path: Path


@dataclass
class UpdateBrowsePathAction(Action):
    file_path: Path
    browse_root: Path | None
    browse_rel_path: Path | None


@dataclass
class DeleteObservationAction(Action):
    file_path: Path
