import os
from pathlib import Path
from typing import Iterator
from .models import FileObservation

def scan_directory(path: str) -> Iterator[FileObservation]:
    root_path = Path(path).resolve()
    for dirpath, dirnames, filenames in os.walk(root_path, followlinks=False):
        for filename in filenames:
            file_path = Path(dirpath) / filename
            if file_path.is_symlink():
                continue
            
            try:
                stat = file_path.stat()
            except OSError:
                continue

            yield FileObservation(
                file_path=str(file_path),
                file_name=filename,
                file_format=file_path.suffix.lower(),
                size_bytes=stat.st_size,
                mtime=stat.st_mtime,
                file_hash=None,
                last_seen_at=0.0
            )
