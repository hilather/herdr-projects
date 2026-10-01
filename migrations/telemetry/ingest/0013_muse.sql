-- Muse model_completed identity, retained/backed up with its native session.
CREATE TABLE IF NOT EXISTS muse_events (
    session_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    path_digest TEXT NOT NULL,
    byte_offset INTEGER NOT NULL CHECK (byte_offset >= 0),
    PRIMARY KEY (session_id, event_id),
    UNIQUE (session_id, ordinal)
) STRICT;

-- Exact parent file identity, independent of copied sessions with the same id.
CREATE TABLE IF NOT EXISTS muse_parents (
    path_digest TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    parent_path_digest TEXT NOT NULL
) STRICT;
