CREATE TABLE inbox_items (
    id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK (length(payload_hash)=64),
    seen INTEGER NOT NULL CHECK (seen IN (0,1)),
    done INTEGER NOT NULL CHECK (done IN (0,1))
) STRICT;
UPDATE store_meta SET schema_version=4 WHERE singleton=1;
PRAGMA user_version=4;
