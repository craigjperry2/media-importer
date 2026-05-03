import os
import shutil
import logging
from typing import List, Set
from .models import Action, CopyFileAction, InsertObservationAction, MarkStaleAction, AddBlobAction
from .catalog import Catalog

logger = logging.getLogger(__name__)

class Executor:
    def __init__(self, catalog: Catalog, store_dir: str):
        self.catalog = catalog
        self.store_dir = store_dir

    def execute(self, actions: List[Action]) -> bool:
        success = True
        failed_hashes: Set[str] = set()
        
        # Execute copies first
        for action in actions:
            if isinstance(action, CopyFileAction):
                full_store_path = os.path.join(self.store_dir, action.store_path)
                try:
                    os.makedirs(os.path.dirname(full_store_path), exist_ok=True)
                    tmp_path = full_store_path + '.tmp'
                    shutil.copy2(action.source_path, tmp_path)
                    
                    # fsync tmp_path
                    with open(tmp_path, 'ab') as f:
                        os.fsync(f.fileno())
                        
                    os.replace(tmp_path, full_store_path)
                except Exception as e:
                    logger.error(f"Failed to copy {action.source_path} to {full_store_path}: {e}")
                    failed_hashes.add(action.file_hash)
                    success = False
                    
        # Update DB transactionally
        def db_operations(conn):
            for action in actions:
                if isinstance(action, AddBlobAction):
                    if action.blob.file_hash in failed_hashes:
                        continue
                        
                    conn.execute(
                        "INSERT OR IGNORE INTO blobs (file_hash, size_bytes, store_path, first_seen_at) VALUES (?, ?, ?, ?)",
                        (action.blob.file_hash, action.blob.size_bytes, action.blob.store_path, action.blob.first_seen_at)
                    )
                elif isinstance(action, InsertObservationAction):
                    obs = action.observation
                    conn.execute("""
                        INSERT INTO source_files (file_path, file_name, file_format, size_bytes, mtime, file_hash, last_seen_at)
                        VALUES (?, ?, ?, ?, ?, ?, ?)
                        ON CONFLICT(file_path) DO UPDATE SET
                            file_name=excluded.file_name,
                            file_format=excluded.file_format,
                            size_bytes=excluded.size_bytes,
                            mtime=excluded.mtime,
                            file_hash=excluded.file_hash,
                            last_seen_at=excluded.last_seen_at
                    """, (obs.file_path, obs.file_name, obs.file_format, obs.size_bytes, obs.mtime, obs.file_hash, obs.last_seen_at))
                elif isinstance(action, MarkStaleAction):
                    conn.execute("DELETE FROM blobs WHERE store_path = ?", (action.file_path,))
                    
        try:
            self.catalog.execute_in_transaction(db_operations)
        except Exception as e:
            logger.error(f"Failed to update database: {e}")
            success = False
            
        return success
