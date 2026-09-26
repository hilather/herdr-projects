-- Waits and replans. The cursor is written in the same transaction as the wait.
-- Replay is once: a wake asks for reevaluation and is not proof.
-- Only a schema 29 feedback row can request a replan. Two automatic replans
-- for one blocker inside a plan revision, then one inbox escalation.
-- A new plan revision records an explicit budget reset. Infrastructure retries
-- of the same attempt are not attempt rows, so they do not consume
-- max_attempts_per_task. Verification history is not rewritten here.
CREATE TABLE wait_conditions (
    wait_id TEXT PRIMARY KEY CHECK (length(wait_id) = 64),
    task_id TEXT NOT NULL REFERENCES tasks(id),
    attempt_id TEXT,
    condition TEXT NOT NULL CHECK (condition IN (
        'dependency_evidence',
        'user_decision',
        'resource_availability',
        'adapter_recovery',
        'validation_completion'
    )),
    plan_revision INTEGER NOT NULL CHECK (plan_revision >= 0),
    cursor_sequence INTEGER NOT NULL CHECK (cursor_sequence >= 0),
    state TEXT NOT NULL CHECK (state IN ('waiting', 'replayed')),
    replayed_through INTEGER CHECK (replayed_through IS NULL OR replayed_through >= cursor_sequence),
    wake_requested INTEGER NOT NULL CHECK (wake_requested IN (0, 1)),
    created_unix_ms INTEGER NOT NULL,
    FOREIGN KEY (attempt_id, task_id) REFERENCES attempts(id, task_id),
    CHECK (
        (state = 'waiting' AND replayed_through IS NULL AND wake_requested = 0)
        OR
        (state = 'replayed' AND replayed_through IS NOT NULL)
    )
) STRICT;
CREATE INDEX wait_conditions_by_task ON wait_conditions(task_id, state, cursor_sequence);
CREATE TABLE wait_replay_events (
    wait_id TEXT NOT NULL REFERENCES wait_conditions(wait_id),
    event_sequence INTEGER NOT NULL REFERENCES events(sequence),
    kind TEXT NOT NULL CHECK (length(kind) BETWEEN 1 AND 128),
    wake INTEGER NOT NULL CHECK (wake IN (0, 1)),
    PRIMARY KEY (wait_id, event_sequence)
) STRICT;
CREATE TABLE replan_budget_resets (
    plan_revision INTEGER PRIMARY KEY CHECK (plan_revision >= 0),
    reset_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE replan_requests (
    replan_id TEXT PRIMARY KEY CHECK (length(replan_id) = 64),
    feedback_id TEXT NOT NULL UNIQUE REFERENCES feedback_items(feedback_id),
    plan_revision INTEGER NOT NULL CHECK (plan_revision >= 0),
    blocker_fingerprint TEXT NOT NULL CHECK (length(blocker_fingerprint) = 64),
    outcome TEXT NOT NULL CHECK (outcome IN ('automatic', 'escalated')),
    proposal_id TEXT,
    inbox_id TEXT,
    created_unix_ms INTEGER NOT NULL,
    CHECK (
        (outcome = 'automatic' AND proposal_id IS NOT NULL AND length(proposal_id) BETWEEN 1 AND 128 AND inbox_id IS NULL)
        OR
        (outcome = 'escalated' AND proposal_id IS NULL AND inbox_id IS NOT NULL AND length(inbox_id) BETWEEN 1 AND 512)
    )
) STRICT;
CREATE INDEX replan_requests_by_blocker ON replan_requests(plan_revision, blocker_fingerprint, outcome);
-- One escalation for a blocker inside a plan revision. A later feedback must not open another proposal.
CREATE UNIQUE INDEX replan_one_escalation ON replan_requests(plan_revision, blocker_fingerprint) WHERE outcome = 'escalated';
CREATE TABLE attempt_infrastructure_retries (
    retry_id TEXT PRIMARY KEY CHECK (length(retry_id) = 64),
    attempt_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    retry_ordinal INTEGER NOT NULL CHECK (retry_ordinal > 0),
    created_unix_ms INTEGER NOT NULL,
    UNIQUE (attempt_id, retry_ordinal),
    FOREIGN KEY (attempt_id, task_id) REFERENCES attempts(id, task_id)
) STRICT;
CREATE TRIGGER wait_conditions_no_delete BEFORE DELETE ON wait_conditions
BEGIN SELECT RAISE(ABORT, 'wait condition is immutable'); END;
CREATE TRIGGER wait_conditions_cursor_fixed
BEFORE UPDATE OF wait_id, task_id, attempt_id, condition, plan_revision, cursor_sequence, created_unix_ms ON wait_conditions
BEGIN SELECT RAISE(ABORT, 'wait cursor is immutable'); END;
CREATE TRIGGER wait_conditions_replay_once
BEFORE UPDATE OF state, replayed_through, wake_requested ON wait_conditions
WHEN OLD.state = 'replayed'
BEGIN SELECT RAISE(ABORT, 'wait cursor replays once'); END;
CREATE TRIGGER wait_replay_events_no_update BEFORE UPDATE ON wait_replay_events
BEGIN SELECT RAISE(ABORT, 'wait replay is immutable'); END;
CREATE TRIGGER wait_replay_events_no_delete BEFORE DELETE ON wait_replay_events
BEGIN SELECT RAISE(ABORT, 'wait replay is immutable'); END;
CREATE TRIGGER replan_requests_no_update BEFORE UPDATE ON replan_requests
BEGIN SELECT RAISE(ABORT, 'replan request is immutable'); END;
CREATE TRIGGER replan_requests_no_delete BEFORE DELETE ON replan_requests
BEGIN SELECT RAISE(ABORT, 'replan request is immutable'); END;
CREATE TRIGGER replan_budget_resets_no_update BEFORE UPDATE ON replan_budget_resets
BEGIN SELECT RAISE(ABORT, 'replan budget reset is immutable'); END;
CREATE TRIGGER replan_budget_resets_no_delete BEFORE DELETE ON replan_budget_resets
BEGIN SELECT RAISE(ABORT, 'replan budget reset is immutable'); END;
CREATE TRIGGER attempt_infrastructure_retries_no_update BEFORE UPDATE ON attempt_infrastructure_retries
BEGIN SELECT RAISE(ABORT, 'infrastructure retry is immutable'); END;
CREATE TRIGGER attempt_infrastructure_retries_no_delete BEFORE DELETE ON attempt_infrastructure_retries
BEGIN SELECT RAISE(ABORT, 'infrastructure retry is immutable'); END;
UPDATE store_meta SET schema_version = 36;
PRAGMA user_version = 36;
