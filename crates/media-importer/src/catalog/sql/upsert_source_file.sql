INSERT INTO source_files (
    source_root,
    relative_path,
    blob_hash,
    size_bytes,
    modified_at_ms,
    first_seen_at_ms,
    last_seen_at_ms,
    seen_count
)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 1)
ON CONFLICT(source_root, relative_path) DO UPDATE SET
    blob_hash = excluded.blob_hash,
    size_bytes = excluded.size_bytes,
    modified_at_ms = excluded.modified_at_ms,
    last_seen_at_ms = excluded.last_seen_at_ms,
    seen_count = source_files.seen_count + 1
