-- P5: lossless envelope metadata interning. Logical columns stay in the view.
CREATE TABLE IF NOT EXISTS source_observation_strings (id INTEGER PRIMARY KEY, value TEXT NOT NULL UNIQUE) STRICT;
CREATE TABLE source_observation_payloads (
 id INTEGER PRIMARY KEY, payload TEXT NOT NULL, payload_digest TEXT NOT NULL,
 UNIQUE(payload,payload_digest)
) STRICT;
INSERT OR IGNORE INTO source_observation_strings(value) SELECT producer_epoch FROM source_observations;
INSERT OR IGNORE INTO source_observation_strings(value) SELECT producer_id FROM source_observations;
INSERT OR IGNORE INTO source_observation_strings(value) SELECT event_kind FROM source_observations;
INSERT OR IGNORE INTO source_observation_strings(value) SELECT identity FROM source_observations;
INSERT OR IGNORE INTO source_observation_strings(value) SELECT provenance FROM source_observations;
INSERT OR IGNORE INTO source_observation_strings(value) SELECT measurement FROM source_observations;
INSERT OR IGNORE INTO source_observation_payloads(payload,payload_digest) SELECT payload,payload_digest FROM source_observations WHERE event_kind IN ('codex.custom_tool_call.v1','codex.custom_tool_call_output.v1','codex.task_complete.v1','codex.turn_context.v1');
CREATE TABLE source_observation_rows (
 source_id INTEGER NOT NULL REFERENCES source_observation_strings(id),
 producer_sequence INTEGER NOT NULL CHECK(producer_sequence >= 0),
 schema_version INTEGER NOT NULL,
 producer_id INTEGER NOT NULL REFERENCES source_observation_strings(id),
 kind_id INTEGER NOT NULL REFERENCES source_observation_strings(id),
 occurred_unix_ms INTEGER,
 observed_unix_ms INTEGER NOT NULL,
 identity_id INTEGER NOT NULL REFERENCES source_observation_strings(id),
 provenance_id INTEGER NOT NULL REFERENCES source_observation_strings(id),
 measurement_id INTEGER NOT NULL REFERENCES source_observation_strings(id),
 payload TEXT, payload_digest ANY,
 payload_id INTEGER REFERENCES source_observation_payloads(id),
 envelope_bytes INTEGER NOT NULL CHECK(envelope_bytes BETWEEN 1 AND 65536),
 event_id TEXT,
 CHECK((payload_id IS NULL AND payload IS NOT NULL AND payload_digest IS NOT NULL)
    OR (payload_id IS NOT NULL AND payload IS NULL AND payload_digest IS NULL)),
 PRIMARY KEY(source_id,producer_sequence)
) STRICT, WITHOUT ROWID;
INSERT INTO source_observation_rows(source_id,producer_sequence,schema_version,producer_id,kind_id,occurred_unix_ms,observed_unix_ms,identity_id,provenance_id,measurement_id,payload,payload_digest,payload_id,envelope_bytes,event_id)
 SELECT (SELECT id FROM source_observation_strings WHERE value=o.producer_epoch),
 o.producer_sequence,
 o.schema_version,
 (SELECT id FROM source_observation_strings WHERE value=o.producer_id),
 (SELECT id FROM source_observation_strings WHERE value=o.event_kind),
 o.occurred_unix_ms,
 o.observed_unix_ms,
 (SELECT id FROM source_observation_strings WHERE value=o.identity),
 (SELECT id FROM source_observation_strings WHERE value=o.provenance),
 (SELECT id FROM source_observation_strings WHERE value=o.measurement),
 CASE WHEN o.event_kind IN ('codex.custom_tool_call.v1','codex.custom_tool_call_output.v1','codex.task_complete.v1','codex.turn_context.v1') THEN NULL ELSE o.payload END,
 CASE WHEN o.event_kind IN ('codex.custom_tool_call.v1','codex.custom_tool_call_output.v1','codex.task_complete.v1','codex.turn_context.v1') THEN NULL WHEN 'sha256:'||lower(hex(unhex(substr(o.payload_digest,8))))=o.payload_digest THEN unhex(substr(o.payload_digest,8)) ELSE o.payload_digest END,
 CASE WHEN o.event_kind IN ('codex.custom_tool_call.v1','codex.custom_tool_call_output.v1','codex.task_complete.v1','codex.turn_context.v1') THEN (SELECT id FROM source_observation_payloads WHERE payload=o.payload AND payload_digest=o.payload_digest) ELSE NULL END,
 o.envelope_bytes,
 CASE WHEN o.event_id=substr(o.event_kind,1,instr(o.event_kind,'.')-1)||':'||o.producer_epoch||':'||o.producer_sequence THEN NULL ELSE o.event_id END FROM source_observations o;
DROP TABLE source_observations;
CREATE VIEW source_observations AS SELECT
 coalesce(r.event_id,substr(k.value,1,instr(k.value,'.')-1)||':'||s.value||':'||r.producer_sequence) AS event_id,
 r.schema_version,p.value AS producer_id,s.value AS producer_epoch,r.producer_sequence,k.value AS event_kind,
 r.occurred_unix_ms,r.observed_unix_ms,i.value AS identity,v.value AS provenance,m.value AS measurement,
 coalesce(r.payload,x.payload) AS payload,coalesce(CASE WHEN typeof(r.payload_digest)='blob' THEN 'sha256:'||lower(hex(r.payload_digest)) ELSE r.payload_digest END,x.payload_digest) AS payload_digest,r.envelope_bytes
 FROM source_observation_rows r
 JOIN source_observation_strings s ON s.id=r.source_id
 JOIN source_observation_strings p ON p.id=r.producer_id
 JOIN source_observation_strings k ON k.id=r.kind_id
 JOIN source_observation_strings i ON i.id=r.identity_id
 JOIN source_observation_strings v ON v.id=r.provenance_id
 JOIN source_observation_strings m ON m.id=r.measurement_id
 LEFT JOIN source_observation_payloads x ON x.id=r.payload_id;
CREATE TRIGGER source_observations_insert INSTEAD OF INSERT ON source_observations BEGIN
INSERT OR IGNORE INTO source_observation_payloads(payload,payload_digest) SELECT NEW.payload,NEW.payload_digest WHERE NEW.event_kind IN ('codex.custom_tool_call.v1','codex.custom_tool_call_output.v1','codex.task_complete.v1','codex.turn_context.v1');
INSERT OR IGNORE INTO source_observation_strings(value) VALUES(NEW.producer_epoch);
INSERT OR IGNORE INTO source_observation_strings(value) VALUES(NEW.producer_id);
INSERT OR IGNORE INTO source_observation_strings(value) VALUES(NEW.event_kind);
INSERT OR IGNORE INTO source_observation_strings(value) VALUES(NEW.identity);
INSERT OR IGNORE INTO source_observation_strings(value) VALUES(NEW.provenance);
INSERT OR IGNORE INTO source_observation_strings(value) VALUES(NEW.measurement);
INSERT INTO source_observation_rows(source_id,producer_sequence,schema_version,producer_id,kind_id,occurred_unix_ms,observed_unix_ms,identity_id,provenance_id,measurement_id,payload,payload_digest,payload_id,envelope_bytes,event_id)
 VALUES((SELECT id FROM source_observation_strings WHERE value=NEW.producer_epoch),
 NEW.producer_sequence,
 NEW.schema_version,
 (SELECT id FROM source_observation_strings WHERE value=NEW.producer_id),
 (SELECT id FROM source_observation_strings WHERE value=NEW.event_kind),
 NEW.occurred_unix_ms,
 NEW.observed_unix_ms,
 (SELECT id FROM source_observation_strings WHERE value=NEW.identity),
 (SELECT id FROM source_observation_strings WHERE value=NEW.provenance),
 (SELECT id FROM source_observation_strings WHERE value=NEW.measurement),
 CASE WHEN NEW.event_kind IN ('codex.custom_tool_call.v1','codex.custom_tool_call_output.v1','codex.task_complete.v1','codex.turn_context.v1') THEN NULL ELSE NEW.payload END,
 CASE WHEN NEW.event_kind IN ('codex.custom_tool_call.v1','codex.custom_tool_call_output.v1','codex.task_complete.v1','codex.turn_context.v1') THEN NULL WHEN 'sha256:'||lower(hex(unhex(substr(NEW.payload_digest,8))))=NEW.payload_digest THEN unhex(substr(NEW.payload_digest,8)) ELSE NEW.payload_digest END,
 CASE WHEN NEW.event_kind IN ('codex.custom_tool_call.v1','codex.custom_tool_call_output.v1','codex.task_complete.v1','codex.turn_context.v1') THEN (SELECT id FROM source_observation_payloads WHERE payload=NEW.payload AND payload_digest=NEW.payload_digest) ELSE NULL END,
 NEW.envelope_bytes,
 CASE WHEN NEW.event_id=substr(NEW.event_kind,1,instr(NEW.event_kind,'.')-1)||':'||NEW.producer_epoch||':'||NEW.producer_sequence THEN NULL ELSE NEW.event_id END);
END;
CREATE TRIGGER source_observations_update INSTEAD OF UPDATE ON source_observations BEGIN
INSERT OR IGNORE INTO source_observation_payloads(payload,payload_digest) SELECT NEW.payload,NEW.payload_digest WHERE NEW.event_kind IN ('codex.custom_tool_call.v1','codex.custom_tool_call_output.v1','codex.task_complete.v1','codex.turn_context.v1');
INSERT OR IGNORE INTO source_observation_strings(value) VALUES(NEW.producer_epoch);
INSERT OR IGNORE INTO source_observation_strings(value) VALUES(NEW.producer_id);
INSERT OR IGNORE INTO source_observation_strings(value) VALUES(NEW.event_kind);
INSERT OR IGNORE INTO source_observation_strings(value) VALUES(NEW.identity);
INSERT OR IGNORE INTO source_observation_strings(value) VALUES(NEW.provenance);
INSERT OR IGNORE INTO source_observation_strings(value) VALUES(NEW.measurement);
UPDATE source_observation_rows SET source_id=(SELECT id FROM source_observation_strings WHERE value=NEW.producer_epoch),
 producer_sequence=NEW.producer_sequence,
 schema_version=NEW.schema_version,
 producer_id=(SELECT id FROM source_observation_strings WHERE value=NEW.producer_id),
 kind_id=(SELECT id FROM source_observation_strings WHERE value=NEW.event_kind),
 occurred_unix_ms=NEW.occurred_unix_ms,
 observed_unix_ms=NEW.observed_unix_ms,
 identity_id=(SELECT id FROM source_observation_strings WHERE value=NEW.identity),
 provenance_id=(SELECT id FROM source_observation_strings WHERE value=NEW.provenance),
 measurement_id=(SELECT id FROM source_observation_strings WHERE value=NEW.measurement),
 payload=CASE WHEN NEW.event_kind IN ('codex.custom_tool_call.v1','codex.custom_tool_call_output.v1','codex.task_complete.v1','codex.turn_context.v1') THEN NULL ELSE NEW.payload END,
 payload_digest=CASE WHEN NEW.event_kind IN ('codex.custom_tool_call.v1','codex.custom_tool_call_output.v1','codex.task_complete.v1','codex.turn_context.v1') THEN NULL WHEN 'sha256:'||lower(hex(unhex(substr(NEW.payload_digest,8))))=NEW.payload_digest THEN unhex(substr(NEW.payload_digest,8)) ELSE NEW.payload_digest END,
 payload_id=CASE WHEN NEW.event_kind IN ('codex.custom_tool_call.v1','codex.custom_tool_call_output.v1','codex.task_complete.v1','codex.turn_context.v1') THEN (SELECT id FROM source_observation_payloads WHERE payload=NEW.payload AND payload_digest=NEW.payload_digest) ELSE NULL END,
 envelope_bytes=NEW.envelope_bytes,
 event_id=CASE WHEN NEW.event_id=substr(NEW.event_kind,1,instr(NEW.event_kind,'.')-1)||':'||NEW.producer_epoch||':'||NEW.producer_sequence THEN NULL ELSE NEW.event_id END
 WHERE source_id=(SELECT id FROM source_observation_strings WHERE value=OLD.producer_epoch) AND producer_sequence=OLD.producer_sequence;
END;
CREATE TRIGGER source_observations_delete INSTEAD OF DELETE ON source_observations BEGIN
 DELETE FROM source_observation_rows WHERE source_id=(SELECT id FROM source_observation_strings WHERE value=OLD.producer_epoch) AND producer_sequence=OLD.producer_sequence; END;

-- Remove duplicate primary-key btrees; no public rowid is used here.
CREATE TABLE codex_usage_times_0012 (
    session_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    record_unix_ms INTEGER,
    PRIMARY KEY (session_id, ordinal)
) STRICT, WITHOUT ROWID;
INSERT INTO codex_usage_times_0012 SELECT * FROM codex_usage_times;
DROP TABLE codex_usage_times;
ALTER TABLE codex_usage_times_0012 RENAME TO codex_usage_times;
CREATE TABLE codex_rate_limit_windows_0012 (
    session_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    secondary_used_percent TEXT,
    secondary_window_minutes INTEGER,
    secondary_resets_at INTEGER,
    rate_limit_reached_type TEXT,
    PRIMARY KEY (session_id, ordinal)
) STRICT, WITHOUT ROWID;
INSERT INTO codex_rate_limit_windows_0012 SELECT * FROM codex_rate_limit_windows;
DROP TABLE codex_rate_limit_windows;
ALTER TABLE codex_rate_limit_windows_0012 RENAME TO codex_rate_limit_windows;
CREATE TABLE codex_tool_calls_0012 (
    session_id TEXT NOT NULL,
    call_id TEXT NOT NULL,
    call_kind TEXT CHECK (call_kind IN ('custom_tool_call', 'function_call')),
    name TEXT,
    status TEXT,
    turn_id TEXT,
    called_unix_ms INTEGER,
    output_kind TEXT CHECK (output_kind IN ('custom_tool_call_output', 'function_call_output')),
    output_unix_ms INTEGER,
    PRIMARY KEY (session_id, call_id)
) STRICT, WITHOUT ROWID;
INSERT INTO codex_tool_calls_0012 SELECT * FROM codex_tool_calls;
DROP TABLE codex_tool_calls;
ALTER TABLE codex_tool_calls_0012 RENAME TO codex_tool_calls;
CREATE TABLE codex_exec_items_0012 (
    session_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    thread_id TEXT,
    turn_id TEXT,
    status TEXT,
    source TEXT,
    exit_code INTEGER,
    startup_duration_secs INTEGER,
    startup_duration_nanos INTEGER,
    completed_unix_ms INTEGER,
    PRIMARY KEY (session_id, item_id)
) STRICT, WITHOUT ROWID;
INSERT INTO codex_exec_items_0012 SELECT * FROM codex_exec_items;
DROP TABLE codex_exec_items;
ALTER TABLE codex_exec_items_0012 RENAME TO codex_exec_items;
CREATE TABLE codex_turn_aborts_0012 (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    reason TEXT,
    duration_ms INTEGER,
    aborted_unix_ms INTEGER,
    PRIMARY KEY (session_id, turn_id)
) STRICT, WITHOUT ROWID;
INSERT INTO codex_turn_aborts_0012 SELECT * FROM codex_turn_aborts;
DROP TABLE codex_turn_aborts;
ALTER TABLE codex_turn_aborts_0012 RENAME TO codex_turn_aborts;
CREATE TABLE codex_mcp_calls_0012 (
    session_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    thread_id TEXT,
    turn_id TEXT,
    server TEXT,
    tool TEXT,
    status TEXT,
    read_only_hint INTEGER CHECK (read_only_hint IS NULL OR read_only_hint IN (0, 1)),
    is_error INTEGER CHECK (is_error IS NULL OR is_error IN (0, 1)),
    duration_secs INTEGER,
    duration_nanos INTEGER,
    completed_unix_ms INTEGER,
    PRIMARY KEY (session_id, item_id)
) STRICT, WITHOUT ROWID;
INSERT INTO codex_mcp_calls_0012 SELECT * FROM codex_mcp_calls;
DROP TABLE codex_mcp_calls;
ALTER TABLE codex_mcp_calls_0012 RENAME TO codex_mcp_calls;
CREATE TABLE codex_agent_items_0012 (
    session_id TEXT NOT NULL,
    item_type TEXT NOT NULL CHECK (item_type IN ('SubAgentActivity', 'CollabAgentToolCall')),
    item_id TEXT NOT NULL,
    thread_id TEXT,
    turn_id TEXT,
    status TEXT,
    agent_thread_id TEXT,
    sender_thread_id TEXT,
    receiver_thread_ids TEXT CHECK (receiver_thread_ids IS NULL OR json_valid(receiver_thread_ids)),
    completed_unix_ms INTEGER,
    PRIMARY KEY (session_id, item_type, item_id)
) STRICT, WITHOUT ROWID;
INSERT INTO codex_agent_items_0012 SELECT * FROM codex_agent_items;
DROP TABLE codex_agent_items;
ALTER TABLE codex_agent_items_0012 RENAME TO codex_agent_items;
CREATE TABLE codex_tool_namespaces_0012 (
    session_id TEXT NOT NULL,
    call_id TEXT NOT NULL,
    namespace TEXT NOT NULL,
    PRIMARY KEY (session_id, call_id)
) STRICT, WITHOUT ROWID;
INSERT INTO codex_tool_namespaces_0012 SELECT * FROM codex_tool_namespaces;
DROP TABLE codex_tool_namespaces;
ALTER TABLE codex_tool_namespaces_0012 RENAME TO codex_tool_namespaces;
CREATE TABLE rollout_metadata_0012 (
    path_digest TEXT PRIMARY KEY,
    model_provider TEXT,
    forked_from_id TEXT,
    subagent_kind TEXT,
    subagent_parent_thread_id TEXT,
    subagent_depth INTEGER
) STRICT, WITHOUT ROWID;
INSERT INTO rollout_metadata_0012 SELECT * FROM rollout_metadata;
DROP TABLE rollout_metadata;
ALTER TABLE rollout_metadata_0012 RENAME TO rollout_metadata;
CREATE TABLE rollout_threads_0012 (
    path_digest TEXT PRIMARY KEY,
    parent_thread_id TEXT,
    session_id TEXT,
    thread_source TEXT
) STRICT, WITHOUT ROWID;
INSERT INTO rollout_threads_0012 SELECT * FROM rollout_threads;
DROP TABLE rollout_threads;
ALTER TABLE rollout_threads_0012 RENAME TO rollout_threads;
CREATE TABLE rollout_subagents_0012 (
    path_digest TEXT PRIMARY KEY,
    subagent_detail TEXT
) STRICT, WITHOUT ROWID;
INSERT INTO rollout_subagents_0012 SELECT * FROM rollout_subagents;
DROP TABLE rollout_subagents;
ALTER TABLE rollout_subagents_0012 RENAME TO rollout_subagents;
CREATE TABLE rollout_ingest_state_0012 (
    path_digest TEXT PRIMARY KEY,
    uncertified_envelopes INTEGER NOT NULL CHECK (uncertified_envelopes IN (0, 1)),
    last_turn_offset INTEGER CHECK (last_turn_offset IS NULL OR last_turn_offset >= 0),
    last_turn_id TEXT,
    last_turn_completed INTEGER NOT NULL CHECK (last_turn_completed IN (0, 1))
) STRICT, WITHOUT ROWID;
INSERT INTO rollout_ingest_state_0012 SELECT * FROM rollout_ingest_state;
DROP TABLE rollout_ingest_state;
ALTER TABLE rollout_ingest_state_0012 RENAME TO rollout_ingest_state;
CREATE TABLE rollout_forks_0012 (
    path_digest TEXT PRIMARY KEY,
    forked_from_ordinal_exclusive INTEGER,
    base_thread_id TEXT,
    base_end_ordinal_exclusive INTEGER,
    base_end_byte_offset INTEGER
) STRICT, WITHOUT ROWID;
INSERT INTO rollout_forks_0012 SELECT * FROM rollout_forks;
DROP TABLE rollout_forks;
ALTER TABLE rollout_forks_0012 RENAME TO rollout_forks;
CREATE TABLE rollout_turn_ends_0012 (
    path_digest TEXT PRIMARY KEY,
    last_turn_aborted INTEGER NOT NULL CHECK (last_turn_aborted IN (0, 1))
) STRICT, WITHOUT ROWID;
INSERT INTO rollout_turn_ends_0012 SELECT * FROM rollout_turn_ends;
DROP TABLE rollout_turn_ends;
ALTER TABLE rollout_turn_ends_0012 RENAME TO rollout_turn_ends;
CREATE TABLE rollout_turn_terminations_0012 (
    path_digest TEXT PRIMARY KEY,
    turn_offset INTEGER NOT NULL CHECK (turn_offset >= 0),
    attempt_id TEXT NOT NULL,
    cause TEXT NOT NULL CHECK (cause IN ('cancellation', 'completion')),
    terminated_unix_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
INSERT INTO rollout_turn_terminations_0012 SELECT * FROM rollout_turn_terminations;
DROP TABLE rollout_turn_terminations;
ALTER TABLE rollout_turn_terminations_0012 RENAME TO rollout_turn_terminations;
CREATE TABLE codex_tool_sources_0012 (
    path_digest TEXT PRIMARY KEY
) STRICT, WITHOUT ROWID;
INSERT INTO codex_tool_sources_0012 SELECT * FROM codex_tool_sources;
DROP TABLE codex_tool_sources;
ALTER TABLE codex_tool_sources_0012 RENAME TO codex_tool_sources;
CREATE TABLE source_bindings_0012 (
    path_digest TEXT PRIMARY KEY,
    basis TEXT NOT NULL CHECK (basis IN ('collector_binding', 'predates_binding', 'binding_revoked', 'no_binding', 'no_match', 'ambiguous'))
) STRICT, WITHOUT ROWID;
INSERT INTO source_bindings_0012 SELECT * FROM source_bindings;
DROP TABLE source_bindings;
ALTER TABLE source_bindings_0012 RENAME TO source_bindings;
