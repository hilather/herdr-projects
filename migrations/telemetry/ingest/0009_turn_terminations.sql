-- F4 (docs/telemetry/certificate-live.md §5, §10): Codex writes neither
-- `task_complete` nor `turn_aborted` for a turn the product ends, so a bound
-- rollout's last turn that was open when its canonical attempt's
-- `runtime.worker_terminated` receipt (cause `cancellation` or `completion`)
-- was observed, and was opened at or before it, ended by termination: its
-- final event is `ended_by_termination`, never a `final_event_missing` gap.
-- One row per source, for its last turn (`turn_offset` =
-- `rollout_ingest_state.last_turn_offset`); recomputed on every collect from
-- the canonical receipt (read-only) and reset by a re-read from byte 0.
-- Metadata only. Re-runnable: `IF NOT EXISTS`.
CREATE TABLE IF NOT EXISTS rollout_turn_terminations (
    path_digest TEXT PRIMARY KEY,
    turn_offset INTEGER NOT NULL CHECK (turn_offset >= 0),
    attempt_id TEXT NOT NULL,
    cause TEXT NOT NULL CHECK (cause IN ('cancellation', 'completion')),
    terminated_unix_ms INTEGER NOT NULL
) STRICT;
