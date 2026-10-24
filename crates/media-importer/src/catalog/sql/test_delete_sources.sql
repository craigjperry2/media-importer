DELETE FROM source_files
WHERE ?1 IS NULL OR blob_hash = ?1;
