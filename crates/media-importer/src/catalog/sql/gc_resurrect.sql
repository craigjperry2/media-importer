UPDATE blobs
SET deleted_at_ms = NULL
WHERE hash = ?1
  AND size_bytes = ?2
  AND deleted_at_ms IS NOT NULL
  AND EXISTS (
      SELECT 1
      FROM source_files
      WHERE source_files.blob_hash = blobs.hash
  );
