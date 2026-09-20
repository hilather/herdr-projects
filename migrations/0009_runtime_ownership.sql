CREATE TABLE runtime_ownership (
    binding_id TEXT PRIMARY KEY NOT NULL REFERENCES runtime_bindings(id),
    revision INTEGER NOT NULL CHECK(revision>0),
    binding_revision INTEGER NOT NULL CHECK(binding_revision>0),
    attempt_id TEXT REFERENCES attempts(id),
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK(length(payload_hash)=64)
) STRICT;
UPDATE store_meta SET schema_version=9;
PRAGMA user_version=9;
