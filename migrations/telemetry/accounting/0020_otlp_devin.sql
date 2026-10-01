-- DG4k: admit certified Devin OTLP `api_request` usage (inclusive read/write
-- normalization derived from Devin's cache-exclusive input) into the ledger.
-- No new retained tables: projections follow sidecar.otlp retention.
CREATE TEMP TABLE dg4k_entries AS SELECT * FROM usage_entries;
CREATE TEMP TABLE dg4k_dispositions AS SELECT * FROM usage_dispositions;
DELETE FROM usage_dispositions;
DROP TABLE usage_entries;
CREATE TABLE IF NOT EXISTS usage_entries (
    entry_id TEXT PRIMARY KEY,
    source TEXT NOT NULL CHECK (source IN ('codex', 'claude-code', 'opencode', 'muse', 'otlp:grok', 'otlp:claude-code', 'otlp:muse', 'otlp:devin')),
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
    CHECK (new_input_tokens IS NULL OR new_input_tokens = input_tokens - cache_read_tokens - CASE WHEN source IN ('claude-code','opencode','muse','otlp:grok','otlp:claude-code','otlp:muse', 'otlp:devin') THEN cache_write_tokens ELSE 0 END)
) STRICT;
INSERT INTO usage_entries SELECT * FROM dg4k_entries;
INSERT INTO usage_dispositions SELECT * FROM dg4k_dispositions;
DROP TABLE dg4k_entries;
DROP TABLE dg4k_dispositions;
CREATE INDEX IF NOT EXISTS usage_entries_session ON usage_entries(session_id);
UPDATE accounting_stream SET invalidated='schema_upgrade' WHERE singleton=1;
DROP VIEW IF EXISTS otlp_ledger_sources;
CREATE VIEW otlp_ledger_sources AS
SELECT identity AS path_digest, adapter || ':' || identity AS session_id,
 adapter, attempt_id, observed_unix_ms,
 CAST(json_extract(record,'$.timeUnixNano') AS INTEGER)/1000000 AS session_unix_ms,
 substr(adapter,6) || '/' || json_extract(record,'$.cli_version') AS cli_version,
 json_extract(record,'$.attributes.model') AS model,
 json_extract(record,'$.attributes.input_tokens') AS input_tokens,
 json_extract(record,'$.attributes.cached_input_tokens') AS cached_input_tokens,
 json_extract(record,'$.attributes.cache_write_input_tokens') AS cache_write_input_tokens,
 json_extract(record,'$.attributes.output_tokens') AS output_tokens,
 json_extract(record,'$.attributes.reasoning_output_tokens') AS reasoning_output_tokens,
 json_extract(record,'$.attributes.total_tokens') AS total_tokens
FROM otlp_records
WHERE binding='exact' AND attempt_id IS NOT NULL AND kind='usage'
 AND json_extract(record,'$.usage_authority') IN ('api_request','model_call')
 AND coalesce(json_extract(record,'$.mapping_certified'),'fixture') <> 'none'
 AND ((adapter='otlp:grok' AND json_extract(record,'$.cli_version')='1.0.46'
       AND json_extract(record,'$.native_name')='grok_code.api_request'
       AND json_extract(record,'$.usage_authority')='api_request'
       AND json_extract(record,'$.usage_source_key') IS NOT NULL)
   OR (adapter='otlp:claude-code' AND json_extract(record,'$.cli_version') IN ('2.1.3','2.1.286')
       AND json_extract(record,'$.native_name')='claude_code.api_request')
   OR (adapter='otlp:muse' AND json_extract(record,'$.cli_version')='1.4.0-R4161.1'
       AND json_extract(record,'$.native_name')='model_call')
   OR (adapter='otlp:devin' AND json_extract(record,'$.cli_version')='3000.11.3'
       AND json_extract(record,'$.native_name')='api_request'
       AND json_extract(record,'$.usage_authority')='api_request'
       AND json_extract(record,'$.usage_source_key') IS NOT NULL))
 AND json_extract(record,'$.attributes.total_tokens') BETWEEN 0 AND 9007199254740992
 AND json_extract(record,'$.attributes.cached_input_tokens') + json_extract(record,'$.attributes.cache_write_input_tokens') <= json_extract(record,'$.attributes.input_tokens')
 AND json_extract(record,'$.attributes.reasoning_output_tokens') <= json_extract(record,'$.attributes.output_tokens');
