-- Stream `accounting` 5 (docs/telemetry/contracts-accounting.md §6): human
-- attention samples. One row per launched, unterminated canonical attempt per
-- observation pass: the agent state label stock Herdr's `agent list` reported
-- for the pane recorded in the attempt's `runtime.launch_started` receipt, or
-- the reason no label was observed. Append-only; intervals, gaps and M31–M33
-- are derived from it at read time. State labels, reason codes and timestamps
-- only: no pane content, screen text, terminal title, cwd or agent name is
-- stored (contracts §7).
CREATE TABLE IF NOT EXISTS attention_samples (
    attempt_id TEXT NOT NULL CHECK (length(attempt_id) > 0),
    observed_unix_ms INTEGER NOT NULL,
    state TEXT CHECK (state IN ('blocked', 'working', 'idle', 'done')),
    gap TEXT CHECK (gap IN ('herdr_unreachable', 'herdr_error', 'herdr_reply_invalid', 'agent_absent', 'identity_mismatch',
        'state_unknown', 'state_unrecognized', 'remote_route', 'route_unrecorded', 'budget_exhausted')),
    interval_ms INTEGER NOT NULL CHECK (interval_ms > 0),
    source TEXT NOT NULL CHECK (source = 'herdr-agent-list-v1'),
    CHECK ((state IS NULL) <> (gap IS NULL))
) STRICT;
CREATE INDEX IF NOT EXISTS attention_samples_attempt ON attention_samples(attempt_id, observed_unix_ms);
