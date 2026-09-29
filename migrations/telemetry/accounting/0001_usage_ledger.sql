-- Stream `accounting` 1 (docs/telemetry/contracts-accounting.md): the usage
-- ledger derived from Codex sidecar tables by read-only SQL, rebuilt whole by
-- each sync. Counters only; no content (contracts §7).
-- `IF NOT EXISTS`: a sidecar whose stream table was lost keeps its ledger tables.
CREATE TABLE IF NOT EXISTS usage_entries (
    entry_id TEXT PRIMARY KEY,
    source TEXT NOT NULL CHECK (source = 'codex'),
    session_id TEXT NOT NULL,
    basis TEXT NOT NULL CHECK (basis IN ('delta', 'cumulative')),
    scope TEXT NOT NULL CHECK (scope IN ('request', 'thread')),
    normalization_version TEXT NOT NULL,
    precedence INTEGER NOT NULL CHECK (precedence > 0),
    position INTEGER NOT NULL CHECK (position >= 0),
    response_id TEXT,
    model TEXT,
    native TEXT NOT NULL CHECK (json_valid(native)),
    input_tokens INTEGER,
    cache_read_tokens INTEGER,
    new_input_tokens INTEGER,
    cache_write_tokens INTEGER,
    output_tokens INTEGER,
    reasoning_tokens INTEGER,
    total_tokens INTEGER,
    CHECK (total_tokens IS NULL OR total_tokens = input_tokens + output_tokens),
    CHECK (new_input_tokens IS NULL OR new_input_tokens = input_tokens - cache_read_tokens)
) STRICT;
CREATE TABLE IF NOT EXISTS usage_dispositions (
    entry_id TEXT NOT NULL REFERENCES usage_entries(entry_id),
    path_digest TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK (disposition IN ('accepted', 'duplicate', 'conflict', 'unresolved')),
    reason TEXT,
    PRIMARY KEY (entry_id, path_digest)
) STRICT;
CREATE TABLE IF NOT EXISTS usage_ledger (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    normalization_version TEXT NOT NULL,
    synced_unix_ms INTEGER NOT NULL
) STRICT;
