SELECT
    id,
    source_root,
    relative_path,
    blob_hash,
    size_bytes,
    modified_at_ms,
    first_seen_at_ms,
    last_seen_at_ms,
    seen_count
FROM source_files
ORDER BY id;
