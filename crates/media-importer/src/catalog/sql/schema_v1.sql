CREATE TABLE blobs (
    hash TEXT PRIMARY KEY CHECK(length(hash) = 64),
    size_bytes INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    deleted_at_ms INTEGER
);

CREATE TABLE source_files (
    id INTEGER PRIMARY KEY,
    source_root TEXT NOT NULL CHECK(length(source_root) > 0),
    relative_path TEXT NOT NULL CHECK(
        length(relative_path) > 0
        AND substr(relative_path, 1, 1) != '/'
    ),
    blob_hash TEXT NOT NULL REFERENCES blobs(hash),
    size_bytes INTEGER NOT NULL,
    modified_at_ms INTEGER,
    first_seen_at_ms INTEGER NOT NULL,
    last_seen_at_ms INTEGER NOT NULL,
    seen_count INTEGER NOT NULL DEFAULT 1,
    UNIQUE(source_root, relative_path)
);

CREATE INDEX idx_source_files_blob_hash
ON source_files(blob_hash);

PRAGMA user_version = 1;
