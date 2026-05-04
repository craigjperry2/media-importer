import os
import sqlite3
from collections.abc import Callable
from pathlib import Path
from typing import List, Optional

from .models import Blob, FileObservation


class Catalog:
    
    def __init__(self, db_path: str, read_only: bool = False):
        self.db_path = db_path
    
        path = Path(db_path).resolve()
        db_exists = path.exists()
        use_in_memory = read_only and not db_exists

        if read_only and db_exists:
            uri = f"file:{path}?mode=ro"
        elif use_in_memory:
            uri = "file::memory:?cache=shared"
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            uri = f"file:{path}"

        self.conn = sqlite3.connect(uri, uri=True)
        self.conn.row_factory = sqlite3.Row

        if use_in_memory or not read_only:
            self._init_db()

    def _init_db(self):
        self.conn.execute("PRAGMA foreign_keys = ON")
        self.conn.execute("PRAGMA journal_mode = WAL")

        self.conn.execute("""
            CREATE TABLE IF NOT EXISTS blobs (
                file_hash TEXT PRIMARY KEY,
                size_bytes INTEGER,
                store_path TEXT UNIQUE,
                first_seen_at REAL
            )
        """)

        self.conn.execute("""
            CREATE TABLE IF NOT EXISTS source_files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_path TEXT UNIQUE,
                file_name TEXT,
                file_format TEXT,
                size_bytes INTEGER,
                mtime REAL,
                file_hash TEXT,
                last_seen_at REAL,
                FOREIGN KEY(file_hash) REFERENCES blobs(file_hash) ON DELETE CASCADE
            )
        """)

        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_file_name ON source_files(file_name)"
        )
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_file_format ON source_files(file_format)"
        )
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_file_hash ON source_files(file_hash)"
        )

        self.conn.commit()

    def get_observation(self, file_path: str) -> Optional[FileObservation]:
        row = self.conn.execute(
            "SELECT * FROM source_files WHERE file_path = ?", (file_path,)
        ).fetchone()
        return FileObservation(**dict(row)) if row else None


    def get_blob(self, file_hash: str) -> Optional[Blob]:
        row = self.conn.execute(
            "SELECT * FROM blobs WHERE file_hash = ?", (file_hash,)
        ).fetchone()
        return Blob(**dict(row)) if row else None

    def get_all_blobs(self) -> List[Blob]:
        cursor = self.conn.execute("SELECT * FROM blobs")
        return [Blob(
                    file_hash=row["file_hash"],
                    size_bytes=row["size_bytes"],
                    store_path=row["store_path"],
                    first_seen_at=row["first_seen_at"],
                ) for row in cursor]

    def execute_in_transaction(
        self, func: Callable[[sqlite3.Connection], None]
    ) -> None:
        try:
            self.conn.execute("BEGIN TRANSACTION")
            func(self.conn)
            self.conn.commit()
        except Exception as e:
            self.conn.rollback()
            raise e

    def close(self):
        self.conn.close()
