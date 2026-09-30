-- TM5.1 (docs/telemetry/certificate-scale.md §5): read indexes for lookups
-- that ran once per rollout source or per attempt and scanned a whole table
-- each time, quadratic in collected history: `codex_usage` by `path_digest`
-- (the usage metrics' accepted count and `cli_version_uncertified` test, the
-- collector's re-read check; proposed by TM4.1, contracts-analytics.md §6),
-- `rollout_sources` by `attempt_id` (each attempt's bound usage in the
-- attempt projection), and two per-record lookups over the record's session:
-- `codex_usage` by turn (the ledger sync's mixed-model turn test) and by
-- response (the repeated-response exclusion of the usage sums,
-- certificate-core.md R3). Re-runnable.
CREATE INDEX IF NOT EXISTS codex_usage_by_path ON codex_usage(path_digest, accepted, reason);
CREATE INDEX IF NOT EXISTS rollout_sources_by_attempt ON rollout_sources(attempt_id, binding);
CREATE INDEX IF NOT EXISTS codex_usage_by_turn ON codex_usage(session_id, turn_id, model);
CREATE INDEX IF NOT EXISTS codex_usage_by_response ON codex_usage(session_id, response_id, payload_digest, accepted, ordinal);
PRAGMA user_version = 3;
