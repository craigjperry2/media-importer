import hashlib


def calculate_hash(file_path: str, chunk_size: int = 8192) -> str:
    hasher = hashlib.blake2b()
    with open(file_path, "rb") as f:
        while chunk := f.read(chunk_size):
            hasher.update(chunk)
    return hasher.hexdigest()
