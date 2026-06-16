SELECT
    source_files.relative_path,
    source_files.blob_hash,
    blobs.size_bytes
FROM source_files
JOIN blobs ON blobs.hash = source_files.blob_hash
WHERE blobs.deleted_at_ms IS NULL
GROUP BY source_files.relative_path, source_files.blob_hash
ORDER BY source_files.relative_path, source_files.blob_hash;
