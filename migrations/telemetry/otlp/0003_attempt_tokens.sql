-- Only hashes are durable; bearer credentials are returned once to the launcher.
CREATE TABLE otlp_attempt_tokens (
    token_hash TEXT PRIMARY KEY,
    project_hash TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    expires_unix_ms INTEGER NOT NULL,
    revoked_unix_ms INTEGER,
    created_unix_ms INTEGER NOT NULL
) STRICT;
