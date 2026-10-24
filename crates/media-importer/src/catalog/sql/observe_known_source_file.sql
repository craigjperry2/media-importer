UPDATE source_files
SET modified_at_ms = ?5,
    last_seen_at_ms = ?6,
    seen_count = seen_count + 1
WHERE source_root = ?1
  AND relative_path = ?2
  AND blob_hash = ?3
  AND size_bytes = ?4;
