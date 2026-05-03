import sqlite3
import os
from typing import List, Optional
from .models import Blob, FileObservation

class Catalog:
    def __init__(self, db_path: str, read_only: bool = False):
        self.db_path = db_path
        
        if read_only:
            # When db does not exist and read_only is True, SQLite will raise an error with ?mode=ro.
            # We can use an in-memory clone or simply allow opening. Let's use in-memory clone for safety if requested.
            # Actually, `?mode=ro` URI requires the DB to exist. If it doesn't, we can just connect to :memory:
            if not os.path.exists(db_path):
                uri = "file::memory:?cache=shared"
            else:
                uri = f"file:{os.path.abspath(db_path)}?mode=ro"
        else:
            # Ensure directory exists
            os.makedirs(os.path.dirname(os.path.abspath(db_path)), exist_ok=True)
            uri = f"file:{os.path.abspath(db_path)}"
            
        self.conn = sqlite3.connect(uri, uri=True)
        self.conn.row_factory = sqlite3.Row
        
        if not read_only or not os.path.exists(db_path):
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
        
        self.conn.execute("CREATE INDEX IF NOT EXISTS idx_file_name ON source_files(file_name)")
        self.conn.execute("CREATE INDEX IF NOT EXISTS idx_file_format ON source_files(file_format)")
        self.conn.execute("CREATE INDEX IF NOT EXISTS idx_file_hash ON source_files(file_hash)")
        
        self.conn.commit()

    def get_observation(self, file_path: str) -> Optional[FileObservation]:
        cursor = self.conn.execute(
            "SELECT * FROM source_files WHERE file_path = ?", (file_path,)
        )
        row = cursor.fetchone()
        if row:
            return FileObservation(
                file_path=row['file_path'],
                file_name=row['file_name'],
                file_format=row['file_format'],
                size_bytes=row['size_bytes'],
                mtime=row['mtime'],
                file_hash=row['file_hash'],
                last_seen_at=row['last_seen_at']
            )
        return None

    def get_blob(self, file_hash: str) -> Optional[Blob]:
        cursor = self.conn.execute(
            "SELECT * FROM blobs WHERE file_hash = ?", (file_hash,)
        )
        row = cursor.fetchone()
        if row:
            return Blob(
                file_hash=row['file_hash'],
                size_bytes=row['size_bytes'],
                store_path=row['store_path'],
                first_seen_at=row['first_seen_at']
            )
        return None
        
    def get_all_blobs(self) -> List[Blob]:
        cursor = self.conn.execute("SELECT * FROM blobs")
        blobs = []
        for row in cursor:
            blobs.append(Blob(
                file_hash=row['file_hash'],
                size_bytes=row['size_bytes'],
                store_path=row['store_path'],
                first_seen_at=row['first_seen_at']
            ))
        return blobs

    def execute_in_transaction(self, func):
        try:
            self.conn.execute("BEGIN TRANSACTION")
            func(self.conn)
            self.conn.commit()
        except Exception as e:
            self.conn.rollback()
            raise e

    def close(self):
        self.conn.close()
