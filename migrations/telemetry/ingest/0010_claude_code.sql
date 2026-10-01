-- DG4b: metadata-only native Claude Code identity and reported tool outcomes.
-- Both tables follow their normalized session's retention and backup lifecycle.
CREATE TABLE IF NOT EXISTS claude_messages (
    session_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    path_digest TEXT NOT NULL,
    byte_offset INTEGER NOT NULL CHECK (byte_offset >= 0),
    PRIMARY KEY (session_id, message_id),
    UNIQUE (session_id, ordinal)
) STRICT;
CREATE TABLE IF NOT EXISTS claude_tool_results (
    session_id TEXT NOT NULL,
    call_id TEXT NOT NULL,
    is_error INTEGER CHECK (is_error IN (0, 1)),
    completed_unix_ms INTEGER,
    PRIMARY KEY (session_id, call_id)
) STRICT;
