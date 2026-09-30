-- Durable change frontier; source mutations and sync completion commit atomically.
CREATE TABLE IF NOT EXISTS accounting_stream (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 sequence INTEGER NOT NULL DEFAULT 0,
 watermark INTEGER NOT NULL DEFAULT -1,
 tombstones INTEGER NOT NULL DEFAULT 0,
 quota_rebuild INTEGER NOT NULL DEFAULT 1,
 invalidated TEXT,
 last_mode TEXT,
 last_reason TEXT
) STRICT;
INSERT OR IGNORE INTO accounting_stream(singleton,invalidated) VALUES(1,'schema_upgrade');
UPDATE accounting_stream SET invalidated='schema_upgrade' WHERE singleton=1;
CREATE TABLE IF NOT EXISTS accounting_dirty_sessions(session_id TEXT PRIMARY KEY) STRICT;
CREATE INDEX IF NOT EXISTS usage_entries_session ON usage_entries(session_id);
CREATE INDEX IF NOT EXISTS session_graph_nodes_session ON session_graph_nodes(session_id);
CREATE INDEX IF NOT EXISTS rollout_sources_session ON rollout_sources(session_id);
CREATE TRIGGER IF NOT EXISTS accounting_codex_usage_insert AFTER INSERT ON codex_usage BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_usage_update AFTER UPDATE ON codex_usage BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_usage_delete AFTER DELETE ON codex_usage BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
UPDATE accounting_stream SET invalidated=coalesce(invalidated,'source_rows_deleted');
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_quarantine_insert AFTER INSERT ON codex_quarantine BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_quarantine_update AFTER UPDATE ON codex_quarantine BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_quarantine_delete AFTER DELETE ON codex_quarantine BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
UPDATE accounting_stream SET invalidated=coalesce(invalidated,'source_rows_deleted');
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_turns_insert AFTER INSERT ON codex_turns BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_turns_update AFTER UPDATE ON codex_turns BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_turns_delete AFTER DELETE ON codex_turns BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
UPDATE accounting_stream SET invalidated=coalesce(invalidated,'source_rows_deleted');
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_rate_limits_insert AFTER INSERT ON codex_rate_limits BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_rate_limits_update AFTER UPDATE ON codex_rate_limits BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_rate_limits_delete AFTER DELETE ON codex_rate_limits BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
UPDATE accounting_stream SET invalidated=coalesce(invalidated,'source_rows_deleted');
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_rate_limit_windows_insert AFTER INSERT ON codex_rate_limit_windows BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_rate_limit_windows_update AFTER UPDATE ON codex_rate_limit_windows BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_rate_limit_windows_delete AFTER DELETE ON codex_rate_limit_windows BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
UPDATE accounting_stream SET invalidated=coalesce(invalidated,'source_rows_deleted');
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_fork_reconciliation_insert AFTER INSERT ON codex_fork_reconciliation BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_fork_reconciliation_update AFTER UPDATE ON codex_fork_reconciliation BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_codex_fork_reconciliation_delete AFTER DELETE ON codex_fork_reconciliation BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
UPDATE accounting_stream SET invalidated=coalesce(invalidated,'source_rows_deleted');
END;
CREATE TRIGGER IF NOT EXISTS accounting_rollout_sources_insert AFTER INSERT ON rollout_sources BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_rollout_sources_update AFTER UPDATE ON rollout_sources BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
INSERT INTO accounting_dirty_sessions VALUES(NEW.session_id) ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_rollout_sources_delete AFTER DELETE ON rollout_sources BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions VALUES(OLD.session_id) ON CONFLICT(session_id) DO NOTHING;
UPDATE accounting_stream SET invalidated=coalesce(invalidated,'source_rows_deleted');
END;
CREATE TRIGGER IF NOT EXISTS accounting_rollout_metadata_insert AFTER INSERT ON rollout_metadata BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE path_digest=NEW.path_digest ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_rollout_metadata_update AFTER UPDATE ON rollout_metadata BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE path_digest=NEW.path_digest ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_rollout_metadata_delete AFTER DELETE ON rollout_metadata BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE path_digest=OLD.path_digest ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_rollout_threads_insert AFTER INSERT ON rollout_threads BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE path_digest=NEW.path_digest ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_rollout_threads_update AFTER UPDATE ON rollout_threads BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE path_digest=NEW.path_digest ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_rollout_threads_delete AFTER DELETE ON rollout_threads BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE path_digest=OLD.path_digest ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_rollout_forks_insert AFTER INSERT ON rollout_forks BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE path_digest=NEW.path_digest ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_rollout_forks_update AFTER UPDATE ON rollout_forks BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE path_digest=NEW.path_digest ON CONFLICT(session_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS accounting_rollout_forks_delete AFTER DELETE ON rollout_forks BEGIN
UPDATE accounting_stream SET sequence=sequence+1;
INSERT INTO accounting_dirty_sessions SELECT session_id FROM rollout_sources WHERE path_digest=OLD.path_digest ON CONFLICT(session_id) DO NOTHING;
END;
-- An externally changed projection cannot be trusted as an incremental base.
CREATE TRIGGER IF NOT EXISTS accounting_dispositions_update AFTER UPDATE ON usage_dispositions BEGIN
UPDATE accounting_stream SET invalidated='projection_changed'; END;
CREATE TRIGGER IF NOT EXISTS accounting_ledger_delete AFTER DELETE ON usage_ledger BEGIN
UPDATE accounting_stream SET invalidated='projection_deleted'; END;

-- Ordinary ordered snapshots extend their persisted windows. Corrections and
-- account reassignment replay the affected accounts instead.
CREATE INDEX IF NOT EXISTS quota_windows_current ON quota_windows(account,limit_id,window_kind,resets_unix_ms);
CREATE INDEX IF NOT EXISTS quota_observations_order ON quota_window_observations(observed_unix_ms,session_id,ordinal);
CREATE TRIGGER IF NOT EXISTS accounting_quota_limits_update AFTER UPDATE ON codex_rate_limits BEGIN
UPDATE accounting_stream SET quota_rebuild=1; END;
CREATE TRIGGER IF NOT EXISTS accounting_quota_secondary_insert AFTER INSERT ON codex_rate_limit_windows
WHEN EXISTS(SELECT 1 FROM quota_window_observations WHERE session_id=NEW.session_id AND ordinal=NEW.ordinal) BEGIN
UPDATE accounting_stream SET quota_rebuild=1; END;
CREATE TRIGGER IF NOT EXISTS accounting_quota_secondary_update AFTER UPDATE ON codex_rate_limit_windows BEGIN
UPDATE accounting_stream SET quota_rebuild=1; END;
CREATE TRIGGER IF NOT EXISTS accounting_quota_source_insert AFTER INSERT ON rollout_sources
WHEN EXISTS(SELECT 1 FROM quota_window_observations WHERE session_id=NEW.session_id) BEGIN
UPDATE accounting_stream SET quota_rebuild=1; END;
CREATE TRIGGER IF NOT EXISTS accounting_quota_source_update AFTER UPDATE ON rollout_sources
WHEN OLD.home_digest<>NEW.home_digest OR OLD.session_id<>NEW.session_id BEGIN
UPDATE accounting_stream SET quota_rebuild=1; END;
