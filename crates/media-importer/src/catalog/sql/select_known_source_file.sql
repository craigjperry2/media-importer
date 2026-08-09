SELECT
    source_files.blob_hash,
    source_files.size_bytes,
    source_files.modified_at_ms,
    blobs.size_bytes,
    blobs.deleted_at_ms
FROM source_files
JOIN blobs ON blobs.hash = source_files.blob_hash
WHERE source_files.source_root = ?1
  AND source_files.relative_path = ?2;
