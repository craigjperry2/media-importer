UPDATE blobs
SET deleted_at_ms = NULL
WHERE hash = ?1
  AND size_bytes = ?2;
