-- DG4j: OTLP projection uses existing normalized-session tables and ledger.
-- No new retained tables: projections follow sidecar.otlp source retention.
CREATE TEMP TABLE dg4j_entries AS SELECT * FROM usage_entries;
CREATE TEMP TABLE dg4j_dispositions AS SELECT * FROM usage_dispositions;
DELETE FROM usage_dispositions;
DROP TABLE usage_entries;
CREATE TABLE IF NOT EXISTS usage_entries (
    entry_id TEXT PRIMARY KEY,
    source TEXT NOT NULL CHECK (source IN ('codex', 'claude-code', 'opencode', 'muse', 'otlp:grok', 'otlp:claude-code', 'otlp:muse')),
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
    CHECK (new_input_tokens IS NULL OR new_input_tokens = input_tokens - cache_read_tokens - CASE WHEN source IN ('claude-code','opencode','muse','otlp:grok','otlp:claude-code','otlp:muse') THEN cache_write_tokens ELSE 0 END)
) STRICT;
INSERT INTO usage_entries SELECT * FROM dg4j_entries;
INSERT INTO usage_dispositions SELECT * FROM dg4j_dispositions;
DROP TABLE dg4j_entries;
DROP TABLE dg4j_dispositions;
CREATE INDEX IF NOT EXISTS usage_entries_session ON usage_entries(session_id);
UPDATE accounting_stream SET invalidated='schema_upgrade' WHERE singleton=1;
-- Sanitized native observations only; no cross-surface aggregation.
CREATE TABLE IF NOT EXISTS otlp_records (
    identity TEXT PRIMARY KEY,
    adapter TEXT NOT NULL,
    attempt_id TEXT,
    binding TEXT NOT NULL CHECK(binding IN ('exact','unbound','unknown_attempt')),
    kind TEXT NOT NULL CHECK(kind IN ('usage','tool','unmapped')),
    source_trust TEXT NOT NULL CHECK(source_trust='collector_observed'),
    certified TEXT NOT NULL CHECK(certified='fixture'),
    record TEXT NOT NULL CHECK(json_valid(record)),
    observed_unix_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS otlp_records_attempt ON otlp_records(attempt_id,kind);

CREATE INDEX IF NOT EXISTS rollout_sources_surface ON rollout_sources(originator,attempt_id,binding);
-- One synthetic session per stable record identity, with no invented home/cwd.
CREATE VIEW IF NOT EXISTS otlp_ledger_sources AS
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
       AND json_extract(record,'$.native_name')='model_call'))
 AND json_extract(record,'$.attributes.total_tokens') BETWEEN 0 AND 9007199254740992
 AND json_extract(record,'$.attributes.cached_input_tokens') + json_extract(record,'$.attributes.cache_write_input_tokens') <= json_extract(record,'$.attributes.input_tokens')
 AND json_extract(record,'$.attributes.reasoning_output_tokens') <= json_extract(record,'$.attributes.output_tokens');
CREATE TRIGGER IF NOT EXISTS accounting_otlp_insert AFTER INSERT ON otlp_records
WHEN NEW.identity IN (SELECT path_digest FROM otlp_ledger_sources) BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT OR IGNORE INTO accounting_dirty_sessions SELECT session_id FROM otlp_ledger_sources WHERE path_digest=NEW.identity;
INSERT OR IGNORE INTO rollout_sources(path_digest,home_digest,session_id,session_unix_ms,cwd,cli_version,originator,source,records,binding,attempt_id,observed_unix_ms)
SELECT path_digest,'otlp',session_id,session_unix_ms,'',cli_version,adapter,adapter,1,'bound',attempt_id,observed_unix_ms FROM otlp_ledger_sources WHERE path_digest=NEW.identity;
INSERT OR IGNORE INTO codex_usage(session_id,ordinal,path_digest,response_id,model,payload_digest,input_tokens,cached_input_tokens,cache_write_input_tokens,output_tokens,reasoning_output_tokens,total_tokens,accepted,observed_unix_ms)
SELECT session_id,1,path_digest,path_digest,model,path_digest,input_tokens,cached_input_tokens,cache_write_input_tokens,output_tokens,reasoning_output_tokens,total_tokens,1,observed_unix_ms FROM otlp_ledger_sources WHERE path_digest=NEW.identity;
INSERT OR IGNORE INTO codex_usage_times(session_id,ordinal,record_unix_ms) SELECT session_id,1,session_unix_ms FROM otlp_ledger_sources WHERE path_digest=NEW.identity;
END;
CREATE TRIGGER IF NOT EXISTS accounting_otlp_update AFTER UPDATE ON otlp_records
WHEN NEW.identity IN (SELECT path_digest FROM otlp_ledger_sources)
 OR OLD.identity IN (SELECT path_digest FROM rollout_sources WHERE originator LIKE 'otlp:%') BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT OR IGNORE INTO accounting_dirty_sessions VALUES(OLD.adapter || ':' || OLD.identity);
DELETE FROM codex_usage_times WHERE session_id=OLD.adapter || ':' || OLD.identity;
DELETE FROM codex_usage WHERE path_digest=OLD.identity;
DELETE FROM rollout_sources WHERE path_digest=OLD.identity;
INSERT OR IGNORE INTO accounting_dirty_sessions SELECT session_id FROM otlp_ledger_sources WHERE path_digest=NEW.identity;
INSERT OR IGNORE INTO rollout_sources(path_digest,home_digest,session_id,session_unix_ms,cwd,cli_version,originator,source,records,binding,attempt_id,observed_unix_ms)
SELECT path_digest,'otlp',session_id,session_unix_ms,'',cli_version,adapter,adapter,1,'bound',attempt_id,observed_unix_ms FROM otlp_ledger_sources WHERE path_digest=NEW.identity;
INSERT OR IGNORE INTO codex_usage(session_id,ordinal,path_digest,response_id,model,payload_digest,input_tokens,cached_input_tokens,cache_write_input_tokens,output_tokens,reasoning_output_tokens,total_tokens,accepted,observed_unix_ms)
SELECT session_id,1,path_digest,path_digest,model,path_digest,input_tokens,cached_input_tokens,cache_write_input_tokens,output_tokens,reasoning_output_tokens,total_tokens,1,observed_unix_ms FROM otlp_ledger_sources WHERE path_digest=NEW.identity;
INSERT OR IGNORE INTO codex_usage_times(session_id,ordinal,record_unix_ms) SELECT session_id,1,session_unix_ms FROM otlp_ledger_sources WHERE path_digest=NEW.identity;
END;
CREATE TRIGGER IF NOT EXISTS accounting_otlp_delete AFTER DELETE ON otlp_records
WHEN OLD.identity IN (SELECT path_digest FROM rollout_sources WHERE originator LIKE 'otlp:%') BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT OR IGNORE INTO accounting_dirty_sessions VALUES(OLD.adapter || ':' || OLD.identity);
DELETE FROM codex_usage_times WHERE session_id=OLD.adapter || ':' || OLD.identity;
DELETE FROM codex_usage WHERE path_digest=OLD.identity;
DELETE FROM rollout_sources WHERE path_digest=OLD.identity;
END;
CREATE TRIGGER IF NOT EXISTS accounting_native_otlp_insert AFTER INSERT ON rollout_sources BEGIN
INSERT OR IGNORE INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE attempt_id=NEW.attempt_id AND originator='otlp:' || NEW.originator;
END;
CREATE TRIGGER IF NOT EXISTS accounting_native_otlp_update AFTER UPDATE ON rollout_sources BEGIN
INSERT OR IGNORE INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE attempt_id=OLD.attempt_id AND originator='otlp:' || OLD.originator;
INSERT OR IGNORE INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE attempt_id=NEW.attempt_id AND originator='otlp:' || NEW.originator;
END;
CREATE TRIGGER IF NOT EXISTS accounting_native_otlp_delete AFTER DELETE ON rollout_sources BEGIN
INSERT OR IGNORE INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE attempt_id=OLD.attempt_id AND originator='otlp:' || OLD.originator;
END;
INSERT OR IGNORE INTO rollout_sources(path_digest,home_digest,session_id,session_unix_ms,cwd,cli_version,originator,source,records,binding,attempt_id,observed_unix_ms)
SELECT path_digest,'otlp',session_id,session_unix_ms,'',cli_version,adapter,adapter,1,'bound',attempt_id,observed_unix_ms FROM otlp_ledger_sources;
INSERT OR IGNORE INTO codex_usage(session_id,ordinal,path_digest,response_id,model,payload_digest,input_tokens,cached_input_tokens,cache_write_input_tokens,output_tokens,reasoning_output_tokens,total_tokens,accepted,observed_unix_ms)
SELECT session_id,1,path_digest,path_digest,model,path_digest,input_tokens,cached_input_tokens,cache_write_input_tokens,output_tokens,reasoning_output_tokens,total_tokens,1,observed_unix_ms FROM otlp_ledger_sources;
INSERT OR IGNORE INTO codex_usage_times(session_id,ordinal,record_unix_ms) SELECT session_id,1,session_unix_ms FROM otlp_ledger_sources;
