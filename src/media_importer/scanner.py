from pathlib import Path
from typing import Iterator

from .models import FileObservation


def scan_directory(path: Path) -> Iterator[FileObservation]:
    root_path = path.resolve()
    for dirpath, _, filenames in root_path.walk(follow_symlinks=False):
        for filename in filenames:
            file_path = dirpath / filename
            if file_path.is_symlink():
                continue

            try:
                stat = file_path.stat()
            except OSError:
                continue

            yield FileObservation(
                file_path=file_path,
                file_name=filename,
                file_format=file_path.suffix.lower(),
                size_bytes=stat.st_size,
                mtime=stat.st_mtime,
                file_hash=None,
                last_seen_at=0.0,
            )
