CREATE TABLE runtime_observations (
    binding_id TEXT PRIMARY KEY NOT NULL REFERENCES runtime_bindings(id),
    binding_revision INTEGER NOT NULL CHECK (binding_revision > 0),
    task_revision INTEGER,
    observed_unix_ms INTEGER NOT NULL CHECK (observed_unix_ms >= 0),
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK (length(payload_hash) = 64)
) STRICT;
UPDATE store_meta SET schema_version=6;
PRAGMA user_version=6;
