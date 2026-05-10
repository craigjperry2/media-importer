import sqlite3
from collections.abc import Callable
from pathlib import Path
from typing import List, Optional

from .models import Blob, FileObservation


class Catalog:
    def __init__(self, db_path: Path, read_only: bool = False):
        path = db_path.resolve()
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
                source_root TEXT,
                source_rel_path TEXT,
                browse_root TEXT,
                browse_rel_path TEXT,
                FOREIGN KEY(file_hash) REFERENCES blobs(file_hash) ON DELETE CASCADE
            )
        """)
        self.conn.execute("""
            CREATE TABLE IF NOT EXISTS source_roots (
                source_root TEXT PRIMARY KEY
            )
        """)
        self._ensure_source_file_columns()

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

    def _ensure_source_file_columns(self) -> None:
        columns = {
            row["name"]
            for row in self.conn.execute("PRAGMA table_info(source_files)").fetchall()
        }
        for name in (
            "source_root",
            "source_rel_path",
            "browse_root",
            "browse_rel_path",
        ):
            if name not in columns:
                self.conn.execute(f"ALTER TABLE source_files ADD COLUMN {name} TEXT")

    def _row_to_observation(self, row: sqlite3.Row) -> FileObservation:
        return FileObservation(
            file_path=Path(row["file_path"]),
            file_name=row["file_name"],
            file_format=row["file_format"],
            size_bytes=row["size_bytes"],
            mtime=row["mtime"],
            file_hash=row["file_hash"],
            last_seen_at=row["last_seen_at"],
            source_root=Path(row["source_root"]) if row["source_root"] else None,
            source_rel_path=Path(row["source_rel_path"])
            if row["source_rel_path"]
            else None,
            browse_root=Path(row["browse_root"]) if row["browse_root"] else None,
            browse_rel_path=Path(row["browse_rel_path"])
            if row["browse_rel_path"]
            else None,
        )

    def get_observation(self, file_path: Path) -> Optional[FileObservation]:
        row = self.conn.execute(
            """
            SELECT file_path, file_name, file_format, size_bytes, mtime, file_hash,
                   last_seen_at, source_root, source_rel_path, browse_root,
                   browse_rel_path
            FROM source_files WHERE file_path = ?
            """,
            (str(file_path),),
        ).fetchone()
        if row is None:
            return None
        return self._row_to_observation(row)

    def get_blob(self, file_hash: str) -> Optional[Blob]:
        row = self.conn.execute(
            "SELECT * FROM blobs WHERE file_hash = ?", (file_hash,)
        ).fetchone()
        if row is None:
            return None
        return Blob(
            file_hash=row["file_hash"],
            size_bytes=row["size_bytes"],
            store_path=Path(row["store_path"]),
            first_seen_at=row["first_seen_at"],
        )

    def get_all_blobs(self) -> List[Blob]:
        cursor = self.conn.execute("SELECT * FROM blobs")
        return [
            Blob(
                file_hash=row["file_hash"],
                size_bytes=row["size_bytes"],
                store_path=Path(row["store_path"]),
                first_seen_at=row["first_seen_at"],
            )
            for row in cursor
        ]

    def get_observations_for_hash(self, file_hash: str) -> list[FileObservation]:
        cursor = self.conn.execute(
            """
            SELECT file_path, file_name, file_format, size_bytes, mtime, file_hash,
                   last_seen_at, source_root, source_rel_path, browse_root,
                   browse_rel_path
            FROM source_files
            WHERE file_hash = ?
            """,
            (file_hash,),
        )
        return [self._row_to_observation(row) for row in cursor]

    def get_all_live_observations(self) -> list[FileObservation]:
        cursor = self.conn.execute(
            """
            SELECT file_path, file_name, file_format, size_bytes, mtime, file_hash,
                   last_seen_at, source_root, source_rel_path, browse_root,
                   browse_rel_path
            FROM source_files
            WHERE file_hash IS NOT NULL
            """
        )
        return [self._row_to_observation(row) for row in cursor]

    def get_stale_observations(
        self, source_roots: list[Path], last_seen_at: float
    ) -> list[FileObservation]:
        roots = [root.resolve() for root in source_roots]
        stale: list[FileObservation] = []
        for observation in self.get_all_live_observations():
            if observation.last_seen_at >= last_seen_at:
                continue
            if any(
                _is_relative_to(observation.file_path.resolve(), root) for root in roots
            ):
                stale.append(observation)
        return stale

    def get_source_roots(self) -> list[Path]:
        source_file_rows = self.conn.execute(
            "SELECT DISTINCT source_root FROM source_files WHERE source_root IS NOT NULL"
        ).fetchall()
        root_rows = self.conn.execute("SELECT source_root FROM source_roots").fetchall()
        roots = {
            Path(row["source_root"])
            for row in [*source_file_rows, *root_rows]
            if row["source_root"] is not None
        }
        return sorted(roots)

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


def _is_relative_to(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False
