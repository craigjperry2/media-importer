SELECT
    rowid,
    hash,
    size_bytes,
    created_at_ms,
    deleted_at_ms
FROM blobs
ORDER BY hash, rowid;
