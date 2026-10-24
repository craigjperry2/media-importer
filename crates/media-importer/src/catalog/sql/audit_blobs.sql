SELECT
    rowid,
    hash,
    size_bytes,
    created_at_ms,
    deleted_at_ms,
    EXISTS (
        SELECT 1
        FROM source_files
        WHERE source_files.blob_hash = blobs.hash
    ) AS referenced
FROM blobs
ORDER BY hash, rowid;
