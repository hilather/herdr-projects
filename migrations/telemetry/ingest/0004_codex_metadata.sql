-- A4 (TM1.3 remainder, contracts-collection.md "A4 proposed contracts.md §5/§7
-- revision"): Codex metadata beside the `codex` stream's tables, keyed like
-- them and written in the same sidecar transaction. Metadata only:
-- identifiers, enum-like excerpts, numbers and times. `IF NOT EXISTS`: a
-- sidecar whose stream table was lost re-runs this migration.

-- Per rollout source, from its first `session_meta`: `model_provider`
-- (excerpt), `forked_from_id`, and the subagent source's variant (`review`,
-- `thread_spawn`, ...) with, for `thread_spawn`, the parent thread id and
-- depth. A source with a `rollout_sources` row but none here was read before
-- A4; the next collect reads it again from byte 0.
CREATE TABLE IF NOT EXISTS rollout_metadata (
    path_digest TEXT PRIMARY KEY,
    model_provider TEXT,
    forked_from_id TEXT,
    subagent_kind TEXT,
    subagent_parent_thread_id TEXT,
    subagent_depth INTEGER
) STRICT;

-- Per `codex_usage` row: the token_usage_record line's `timestamp` (Unix ms,
-- `NULL` when the line has none). Outside the payload digest; the first stays.
CREATE TABLE IF NOT EXISTS codex_usage_times (
    session_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    record_unix_ms INTEGER,
    PRIMARY KEY (session_id, ordinal)
) STRICT;

-- Per `codex_rate_limits` row: `rate_limits.secondary.{used_percent (decimal
-- text), window_minutes, resets_at}` and `rate_limits.rate_limit_reached_type`
-- (excerpt). The first stays.
CREATE TABLE IF NOT EXISTS codex_rate_limit_windows (
    session_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    secondary_used_percent TEXT,
    secondary_window_minutes INTEGER,
    secondary_resets_at INTEGER,
    rate_limit_reached_type TEXT,
    PRIMARY KEY (session_id, ordinal)
) STRICT;
