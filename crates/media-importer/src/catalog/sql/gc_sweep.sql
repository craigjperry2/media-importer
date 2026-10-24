DELETE FROM blobs
WHERE hash = ?1
  AND size_bytes = ?2
  AND deleted_at_ms IS NOT NULL
  AND NOT EXISTS (
      SELECT 1
      FROM source_files
      WHERE source_files.blob_hash = blobs.hash
  );
