SELECT
    blobs.rowid,
    blobs.hash,
    blobs.size_bytes,
    blobs.created_at_ms,
    blobs.deleted_at_ms,
    EXISTS (
        SELECT 1
        FROM source_files
        WHERE source_files.blob_hash = blobs.hash
    ) AS referenced
FROM blobs
ORDER BY blobs.hash, blobs.rowid;
