-- Local verifier and integrator outcomes only. A pull-request poll, CI check,
-- or closed PR is not a row here and cannot satisfy a dependency. Ack records a
-- replan proposal id; it does not reserve an attempt.
CREATE TABLE feedback_items (
    feedback_id TEXT PRIMARY KEY CHECK (length(feedback_id) = 64),
    operation_id TEXT NOT NULL CHECK (length(operation_id) BETWEEN 1 AND 128),
    outcome_revision INTEGER NOT NULL CHECK (outcome_revision > 0),
    category TEXT NOT NULL CHECK (category IN (
        'verifier_rejection',
        'integrator_rejection',
        'integrator_conflict',
        'invalidation'
    )),
    task_id TEXT NOT NULL REFERENCES tasks(id),
    reason TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 128),
    state TEXT NOT NULL CHECK (state IN ('open', 'claimed', 'acked')),
    replan_proposal_id TEXT,
    created_unix_ms INTEGER NOT NULL,
    UNIQUE (operation_id, outcome_revision, category),
    CHECK (
        (state = 'acked' AND replan_proposal_id IS NOT NULL AND length(replan_proposal_id) BETWEEN 1 AND 128)
        OR
        (state != 'acked' AND replan_proposal_id IS NULL)
    )
) STRICT;
CREATE TABLE feedback_claims (
    feedback_id TEXT NOT NULL REFERENCES feedback_items(feedback_id),
    claim_epoch INTEGER NOT NULL CHECK (claim_epoch > 0),
    owner TEXT NOT NULL CHECK (length(owner) BETWEEN 1 AND 128),
    lease_until_ms INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('active', 'expired', 'acked')),
    claimed_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (feedback_id, claim_epoch)
) STRICT;
CREATE INDEX feedback_items_by_task_state ON feedback_items(task_id, state, created_unix_ms);
-- One live lease. Retake expires the previous row before inserting the next epoch.
CREATE UNIQUE INDEX feedback_claims_one_active ON feedback_claims(feedback_id) WHERE state = 'active';
CREATE TRIGGER feedback_items_no_relabel
BEFORE UPDATE OF feedback_id, operation_id, outcome_revision, category, task_id, reason, created_unix_ms ON feedback_items
BEGIN SELECT RAISE(ABORT, 'feedback identity is immutable'); END;
CREATE TRIGGER feedback_items_no_delete BEFORE DELETE ON feedback_items
BEGIN SELECT RAISE(ABORT, 'feedback item is immutable'); END;
CREATE TRIGGER feedback_items_no_rewind BEFORE UPDATE OF state ON feedback_items
WHEN OLD.state = 'acked'
  OR (OLD.state = 'claimed' AND NEW.state = 'open')
  OR (OLD.state = 'open' AND NEW.state = 'acked')
BEGIN SELECT RAISE(ABORT, 'feedback state cannot move backwards'); END;
CREATE TRIGGER feedback_items_ack_once BEFORE UPDATE OF replan_proposal_id ON feedback_items
WHEN OLD.replan_proposal_id IS NOT NULL
BEGIN SELECT RAISE(ABORT, 'feedback ack is immutable'); END;
CREATE TRIGGER feedback_claims_no_relabel
BEFORE UPDATE OF feedback_id, claim_epoch, owner, lease_until_ms, claimed_unix_ms ON feedback_claims
BEGIN SELECT RAISE(ABORT, 'feedback claim identity is immutable'); END;
CREATE TRIGGER feedback_claims_no_reopen BEFORE UPDATE OF state ON feedback_claims
WHEN OLD.state != 'active'
BEGIN SELECT RAISE(ABORT, 'feedback claim decision is immutable'); END;
CREATE TRIGGER feedback_claims_no_delete BEFORE DELETE ON feedback_claims
BEGIN SELECT RAISE(ABORT, 'feedback claim is immutable'); END;
UPDATE store_meta SET schema_version = 29;
PRAGMA user_version = 29;
