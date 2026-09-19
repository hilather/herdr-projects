-- Imported execution identities are evidence, never automatic ownership grants.
CREATE TABLE runtime_bindings (
    id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT REFERENCES tasks(id),
    revision INTEGER NOT NULL CHECK (revision > 0),
    source_path TEXT NOT NULL UNIQUE REFERENCES legacy_sources(path),
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK (length(payload_hash) = 64)
) STRICT;
UPDATE store_meta SET schema_version = 5;
PRAGMA user_version = 5;
