UPDATE blobs
SET deleted_at_ms = ?2
WHERE hash = ?1;
