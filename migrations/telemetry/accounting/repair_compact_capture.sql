-- Restore 0012 capture lost when older compaction upgrades rebuilt native tables.
-- Idempotent installation also repairs stores already at current stream versions.
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
CREATE TRIGGER IF NOT EXISTS accounting_quota_limits_update AFTER UPDATE ON codex_rate_limits BEGIN
UPDATE accounting_stream SET quota_rebuild=1; END;
CREATE TRIGGER IF NOT EXISTS accounting_quota_secondary_insert AFTER INSERT ON codex_rate_limit_windows
WHEN EXISTS(SELECT 1 FROM quota_window_observations WHERE session_id=NEW.session_id AND ordinal=NEW.ordinal) BEGIN
UPDATE accounting_stream SET quota_rebuild=1; END;
CREATE TRIGGER IF NOT EXISTS accounting_quota_secondary_update AFTER UPDATE ON codex_rate_limit_windows BEGIN
UPDATE accounting_stream SET quota_rebuild=1; END;

-- Also recover stores compacted before P5b restored projection invalidation.
CREATE TRIGGER IF NOT EXISTS accounting_dispositions_update AFTER UPDATE ON usage_dispositions BEGIN
UPDATE accounting_stream SET invalidated='projection_changed'; END;
