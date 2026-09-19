-- Rebuild the parent and its only dependent table with foreign keys enabled.
-- Imported provenance remains unique; newly created records have no source file.
CREATE TEMP TABLE saved_runtime_observations AS SELECT * FROM runtime_observations;
DROP TABLE runtime_observations;
CREATE TABLE next_runtime_bindings (
    id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT REFERENCES tasks(id),
    revision INTEGER NOT NULL CHECK (revision > 0),
    source_path TEXT UNIQUE REFERENCES legacy_sources(path),
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK (length(payload_hash) = 64)
) STRICT;
INSERT INTO next_runtime_bindings SELECT * FROM runtime_bindings;
DROP TABLE runtime_bindings;
ALTER TABLE next_runtime_bindings RENAME TO runtime_bindings;
CREATE TABLE runtime_observations (
    binding_id TEXT PRIMARY KEY NOT NULL REFERENCES runtime_bindings(id),
    binding_revision INTEGER NOT NULL CHECK (binding_revision > 0),
    task_revision INTEGER,
    observed_unix_ms INTEGER NOT NULL CHECK (observed_unix_ms >= 0),
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK (length(payload_hash) = 64)
) STRICT;
INSERT INTO runtime_observations SELECT * FROM saved_runtime_observations;
DROP TABLE saved_runtime_observations;
UPDATE store_meta SET schema_version=8;
PRAGMA user_version=8;
