-- Session totals are replaced only when P1 replays that session, in the same
-- transaction as the ledger and committed source frontier.
CREATE TABLE IF NOT EXISTS accounting_usage_totals (
 session_id TEXT PRIMARY KEY,
 input_tokens INTEGER NOT NULL,
 output_tokens INTEGER NOT NULL,
 reasoning_tokens INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS accounting_source_summary (
 path_digest TEXT PRIMARY KEY,
 session_id TEXT NOT NULL,
 uncertified INTEGER NOT NULL,
 quarantined INTEGER NOT NULL,
 rejected INTEGER NOT NULL,
 accepted_records INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS accounting_source_summary_session ON accounting_source_summary(session_id);
UPDATE accounting_stream SET invalidated='schema_upgrade' WHERE singleton=1;

CREATE TABLE IF NOT EXISTS accounting_tool_summary (
 session_id TEXT PRIMARY KEY,
 tally TEXT NOT NULL CHECK(json_valid(tally))
) STRICT;
CREATE TABLE IF NOT EXISTS accounting_tool_frontier (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 canonical TEXT NOT NULL,
 inputs TEXT NOT NULL
) STRICT;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_tool_calls_INSERT AFTER INSERT ON codex_tool_calls BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_tool_calls_UPDATE AFTER UPDATE ON codex_tool_calls BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_tool_calls_DELETE AFTER DELETE ON codex_tool_calls BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_exec_items_INSERT AFTER INSERT ON codex_exec_items BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_exec_items_UPDATE AFTER UPDATE ON codex_exec_items BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_exec_items_DELETE AFTER DELETE ON codex_exec_items BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_mcp_calls_INSERT AFTER INSERT ON codex_mcp_calls BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_mcp_calls_UPDATE AFTER UPDATE ON codex_mcp_calls BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_mcp_calls_DELETE AFTER DELETE ON codex_mcp_calls BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_turn_aborts_INSERT AFTER INSERT ON codex_turn_aborts BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_turn_aborts_UPDATE AFTER UPDATE ON codex_turn_aborts BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_turn_aborts_DELETE AFTER DELETE ON codex_turn_aborts BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_tool_namespaces_INSERT AFTER INSERT ON codex_tool_namespaces BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_tool_namespaces_UPDATE AFTER UPDATE ON codex_tool_namespaces BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_tool_namespaces_DELETE AFTER DELETE ON codex_tool_namespaces BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_agent_items_INSERT AFTER INSERT ON codex_agent_items BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_agent_items_UPDATE AFTER UPDATE ON codex_agent_items BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_codex_agent_items_DELETE AFTER DELETE ON codex_agent_items BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_attention_INSERT AFTER INSERT ON attention_samples BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE attempt_id=NEW.attempt_id ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_attention_UPDATE AFTER UPDATE ON attention_samples BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE attempt_id=OLD.attempt_id ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE attempt_id=NEW.attempt_id ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_tools_attention_DELETE AFTER DELETE ON attention_samples BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE attempt_id=OLD.attempt_id ON CONFLICT(session_id) DO NOTHING;
END;

CREATE TABLE IF NOT EXISTS accounting_dispatch_headroom (
 attempt_id TEXT PRIMARY KEY,
 home_digest TEXT NOT NULL,
 decided_unix_ms INTEGER NOT NULL,
 body TEXT NOT NULL CHECK(json_valid(body))
) STRICT;
CREATE TABLE IF NOT EXISTS accounting_dispatch_frontier (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 canonical TEXT NOT NULL
) STRICT;

-- Lifecycle inputs only; open interval ends are supplied at read time.
CREATE TABLE IF NOT EXISTS accounting_fleet_snapshot (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 canonical TEXT NOT NULL,
 body TEXT NOT NULL CHECK(json_valid(body))
) STRICT;

-- Central usage/model coverage retains its native acceptance semantics,
-- separately from normalized lane counters (which may refuse a record).
CREATE TABLE IF NOT EXISTS accounting_native_totals (
 session_id TEXT PRIMARY KEY,
 input_tokens INTEGER NOT NULL,
 output_tokens INTEGER NOT NULL,
 reasoning_tokens INTEGER NOT NULL,
 records INTEGER NOT NULL,
 models INTEGER NOT NULL
) STRICT;

-- Exact unbounded cost metric bodies, committed with their valuation revision.
CREATE TABLE IF NOT EXISTS accounting_cost_aggregates (
 revision INTEGER PRIMARY KEY,
 body TEXT NOT NULL CHECK(json_valid(body))
) STRICT;

-- Canonical lifecycle watermarks and report diagnostics, validated separately
-- from usage frontiers so a live query never substitutes stale evidence.
CREATE TABLE IF NOT EXISTS accounting_canonical_watermark (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 canonical TEXT NOT NULL,
 body TEXT NOT NULL CHECK(json_valid(body))
) STRICT;
CREATE TABLE IF NOT EXISTS accounting_termination_summary (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 inputs TEXT NOT NULL,
 body TEXT NOT NULL CHECK(json_valid(body))
) STRICT;

CREATE TRIGGER IF NOT EXISTS accounting_tools_claude_messages_INSERT AFTER INSERT ON claude_messages BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;

CREATE TRIGGER IF NOT EXISTS accounting_tools_claude_messages_UPDATE AFTER UPDATE ON claude_messages BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;

CREATE TRIGGER IF NOT EXISTS accounting_tools_claude_messages_DELETE AFTER DELETE ON claude_messages BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
END;

CREATE TRIGGER IF NOT EXISTS accounting_tools_claude_tool_results_INSERT AFTER INSERT ON claude_tool_results BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;

CREATE TRIGGER IF NOT EXISTS accounting_tools_claude_tool_results_UPDATE AFTER UPDATE ON claude_tool_results BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;

CREATE TRIGGER IF NOT EXISTS accounting_tools_claude_tool_results_DELETE AFTER DELETE ON claude_tool_results BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
