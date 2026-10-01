-- P5: preserve every native field/check, remove redundant rowid btrees.
CREATE TABLE codex_turns_0004 (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    model TEXT,
    effort TEXT,
    duration_ms INTEGER,
    time_to_first_token_ms INTEGER,
    PRIMARY KEY (session_id, turn_id)
) STRICT, WITHOUT ROWID;
INSERT INTO codex_turns_0004 SELECT * FROM codex_turns;
DROP TABLE codex_turns;
ALTER TABLE codex_turns_0004 RENAME TO codex_turns;
CREATE TABLE codex_rate_limits_0004 (
    session_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    limit_id TEXT,
    used_percent TEXT,
    window_minutes INTEGER,
    resets_at INTEGER,
    plan_type TEXT,
    observed_ts INTEGER NOT NULL,
    PRIMARY KEY (session_id, ordinal)
) STRICT, WITHOUT ROWID;
INSERT INTO codex_rate_limits_0004 SELECT * FROM codex_rate_limits;
DROP TABLE codex_rate_limits;
ALTER TABLE codex_rate_limits_0004 RENAME TO codex_rate_limits;
-- The native text digest stays unchanged. The lookup key stores its hex
-- bytes; queries also compare the original text, including malformed legacy
-- values, so this narrows the index without changing duplicate exclusion.
DROP INDEX codex_usage_by_response;
CREATE INDEX codex_usage_by_response ON codex_usage(session_id,response_id,unhex(substr(payload_digest,8)),accepted,ordinal);
PRAGMA user_version = 4;
