-- Add lossless legacy provenance; memory stays file-authoritative.
ALTER TABLE store_meta RENAME TO store_meta_v1;
CREATE TABLE store_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version INTEGER NOT NULL CHECK (schema_version > 0)
) STRICT;
INSERT INTO store_meta VALUES (1, 2);
DROP TABLE store_meta_v1;
CREATE TABLE legacy_sources (
    path TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('task','thread','runtime','inbox')),
    digest TEXT NOT NULL CHECK (length(digest) = 64),
    bytes BLOB NOT NULL
) STRICT;
CREATE TABLE migration_receipt (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    source_digest TEXT NOT NULL,
    source_count INTEGER NOT NULL,
    task_count INTEGER NOT NULL,
    reconciliation_required INTEGER NOT NULL CHECK (reconciliation_required = 1)
) STRICT;
PRAGMA user_version = 2;
