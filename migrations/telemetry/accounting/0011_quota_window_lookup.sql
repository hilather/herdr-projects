-- Stream `accounting` 11 (TM5.1, docs/telemetry/certificate-scale.md §5):
-- M40 reads, for every dispatch decision, each window kind's latest trusted
-- observation and the other homes that reported the same window (limit,
-- kind, length and reset within the jitter tolerance). A kind with no
-- trusted observation (Codex reports no secondary window) and the
-- shared-window lookup each scanned the account's (or every) observation
-- once per decision. Re-runnable.
CREATE INDEX IF NOT EXISTS quota_window_observations_trusted ON quota_window_observations(account, limit_id, window_kind, trust, observed_unix_ms);
CREATE INDEX IF NOT EXISTS quota_window_observations_window ON quota_window_observations(limit_id, window_kind, window_minutes, resets_unix_ms);
