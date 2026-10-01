-- DG4i: preserve existing native ledger rows while admitting Muse
-- and its inclusive input/cache-read/cache-write convention. Derived projections
-- retain their existing follows_sources retention/backup classification.
CREATE TEMP TABLE dg4i_entries AS SELECT * FROM usage_entries;
CREATE TEMP TABLE dg4i_dispositions AS SELECT * FROM usage_dispositions;
DELETE FROM usage_dispositions;
DROP TABLE usage_entries;
CREATE TABLE IF NOT EXISTS usage_entries (
    entry_id TEXT PRIMARY KEY,
    source TEXT NOT NULL CHECK (source IN ('codex', 'claude-code', 'opencode', 'muse')),
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
    CHECK (new_input_tokens IS NULL OR new_input_tokens = input_tokens - cache_read_tokens - CASE WHEN source IN ('claude-code','opencode','muse') THEN cache_write_tokens ELSE 0 END)
) STRICT;
INSERT INTO usage_entries SELECT * FROM dg4i_entries;
INSERT INTO usage_dispositions SELECT * FROM dg4i_dispositions;
DROP TABLE dg4i_entries;
DROP TABLE dg4i_dispositions;
CREATE INDEX IF NOT EXISTS usage_entries_session ON usage_entries(session_id);
UPDATE accounting_stream SET invalidated='schema_upgrade' WHERE singleton=1;
