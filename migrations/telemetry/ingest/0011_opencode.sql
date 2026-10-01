-- DG4d metadata only; follows normalized session retention/tombstones and backups.
CREATE TABLE IF NOT EXISTS opencode_messages (
    session_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    path_digest TEXT NOT NULL,
    source_revision INTEGER NOT NULL,
    model_id TEXT,
    provider_id TEXT,
    cost REAL CHECK (cost >= 0),
    created_unix_ms INTEGER,
    completed_unix_ms INTEGER,
    PRIMARY KEY (session_id,message_id),
    UNIQUE (session_id,ordinal)
) STRICT;
CREATE TABLE IF NOT EXISTS opencode_tools (
    session_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    part_id TEXT NOT NULL,
    tool TEXT,
    status TEXT CHECK (status IN ('pending','running','completed','error')),
    is_error INTEGER CHECK (is_error IN (0,1)),
    created_unix_ms INTEGER,
    completed_unix_ms INTEGER,
    PRIMARY KEY (session_id,part_id)
) STRICT;
