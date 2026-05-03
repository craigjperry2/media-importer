from dataclasses import dataclass
from typing import Optional

@dataclass
class Blob:
    file_hash: str
    size_bytes: int
    store_path: str
    first_seen_at: float

@dataclass
class FileObservation:
    file_path: str
    file_name: str
    file_format: str
    size_bytes: int
    mtime: float
    file_hash: Optional[str]
    last_seen_at: float

@dataclass
class Action:
    pass

@dataclass
class CopyFileAction(Action):
    source_path: str
    store_path: str
    file_hash: str
    size_bytes: int

@dataclass
class InsertObservationAction(Action):
    observation: FileObservation

@dataclass
class MarkStaleAction(Action):
    file_path: str

@dataclass
class AddBlobAction(Action):
    blob: Blob
