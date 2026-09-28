-- Telemetry sidecar (contracts §5). Analytics only: never read to grant launch.
-- Canonical IDs are referenced by value; no content is stored (contracts §7).
-- Paths are digests; `cwd` keeps the home prefix as `~`.
CREATE TABLE collect_offsets (
    path_digest TEXT PRIMARY KEY,
    device INTEGER NOT NULL,
    inode INTEGER NOT NULL,
    byte_offset INTEGER NOT NULL CHECK (byte_offset >= 0),
    records INTEGER NOT NULL CHECK (records >= 0),
    rate_limits INTEGER NOT NULL CHECK (rate_limits >= 0),
    model TEXT,
    effort TEXT,
    updated_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE rollout_sources (
    path_digest TEXT PRIMARY KEY,
    home_digest TEXT NOT NULL,
    session_id TEXT NOT NULL,
    session_unix_ms INTEGER,
    cwd TEXT NOT NULL,
    cwd_attempt TEXT,
    cli_version TEXT NOT NULL,
    originator TEXT,
    source TEXT,
    records INTEGER NOT NULL CHECK (records >= 0),
    thread_usage TEXT CHECK (thread_usage IS NULL OR json_valid(thread_usage)),
    token_count_usage TEXT CHECK (token_count_usage IS NULL OR json_valid(token_count_usage)),
    binding TEXT NOT NULL CHECK (binding IN ('bound', 'unbound', 'ambiguous')),
    attempt_id TEXT CHECK ((binding = 'bound') = (attempt_id IS NOT NULL)),
    observed_unix_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX rollout_sources_session ON rollout_sources(session_id);
CREATE TABLE codex_usage (
    session_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    path_digest TEXT NOT NULL,
    response_id TEXT,
    turn_id TEXT,
    model TEXT,
    effort TEXT,
    payload_digest TEXT NOT NULL,
    input_tokens INTEGER,
    cached_input_tokens INTEGER,
    cache_write_input_tokens INTEGER,
    output_tokens INTEGER,
    reasoning_output_tokens INTEGER,
    total_tokens INTEGER,
    accepted INTEGER NOT NULL CHECK (accepted IN (0, 1)),
    reason TEXT CHECK (reason IN ('invariant_violation', 'cli_version_uncertified')),
    observed_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (session_id, ordinal),
    CHECK ((accepted = 1) = (reason IS NULL)),
    CHECK (accepted = 1 OR total_tokens IS NULL)
) STRICT;
CREATE TABLE codex_quarantine (
    session_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    first_digest TEXT NOT NULL,
    new_digest TEXT NOT NULL,
    observed_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (session_id, ordinal, new_digest)
) STRICT;
CREATE TABLE codex_discrepancy (
    session_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('thread_total', 'token_count_total')),
    summed_total INTEGER NOT NULL,
    reported_total INTEGER NOT NULL,
    fields TEXT NOT NULL CHECK (json_valid(fields)),
    observed_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (session_id, kind)
) STRICT;
CREATE TABLE codex_rate_limits (
    session_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    limit_id TEXT,
    used_percent TEXT,
    window_minutes INTEGER,
    resets_at INTEGER,
    plan_type TEXT,
    observed_ts INTEGER NOT NULL,
    PRIMARY KEY (session_id, ordinal)
) STRICT;
CREATE TABLE codex_turns (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    model TEXT,
    effort TEXT,
    duration_ms INTEGER,
    time_to_first_token_ms INTEGER,
    PRIMARY KEY (session_id, turn_id)
) STRICT;
PRAGMA user_version = 1;
