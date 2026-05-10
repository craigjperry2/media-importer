import hashlib
from pathlib import Path


def calculate_hash(file_path: Path | str, chunk_size: int = 8192) -> str:
    hasher = hashlib.blake2b()
    with Path(file_path).open("rb") as f:
        while chunk := f.read(chunk_size):
            hasher.update(chunk)
    return hasher.hexdigest()
