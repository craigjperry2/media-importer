from pathlib import Path

from media_importer.hashing import calculate_hash


def test_large_file_chunked_hashing(workspace: Path) -> None:
    file_path = workspace / "large.bin"
    # create a file larger than chunk size
    with file_path.open("wb") as f:
        f.write(b"0" * 10000)

    h1 = calculate_hash(file_path, chunk_size=1024)
    h2 = calculate_hash(file_path, chunk_size=8192)
    assert h1 == h2
