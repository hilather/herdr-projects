CREATE TABLE coordinator_sessions (
    id TEXT PRIMARY KEY NOT NULL,
    created_unix_ms INTEGER NOT NULL,
    herdr_session TEXT NOT NULL,
    last_checkpoint_id TEXT,
    cursor_seq INTEGER NOT NULL DEFAULT 0 CHECK (cursor_seq>=0),
    UNIQUE (herdr_session)
) STRICT;
CREATE TABLE coordinator_checkpoints (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES coordinator_sessions(id),
    kind TEXT NOT NULL CHECK (kind IN ('full','delta')),
    snapshot_id TEXT NOT NULL REFERENCES memory_snapshots(id),
    from_seq INTEGER NOT NULL CHECK (from_seq>=0),
    through_seq INTEGER NOT NULL CHECK (through_seq>=0),
    manifest_hash TEXT NOT NULL CHECK (length(manifest_hash)=64),
    full_chars INTEGER NOT NULL CHECK (full_chars>=0),
    delta_chars INTEGER NOT NULL CHECK (delta_chars>=0),
    created_unix_ms INTEGER NOT NULL,
    acked INTEGER NOT NULL CHECK (acked IN (0,1))
) STRICT;
CREATE INDEX coordinator_checkpoint_session ON coordinator_checkpoints(session_id, created_unix_ms);
CREATE TRIGGER coordinator_checkpoints_no_delete BEFORE DELETE ON coordinator_checkpoints BEGIN SELECT RAISE(ABORT,'coordinator checkpoint is immutable'); END;
UPDATE store_meta SET schema_version=20;
PRAGMA user_version=20;
