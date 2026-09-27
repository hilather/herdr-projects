-- Scheduling scans queued work, never retired task/attempt/binding history.
CREATE INDEX tasks_ready_for_admission ON tasks(id) WHERE state='queued' AND active_attempt IS NULL;
CREATE INDEX attempts_by_task ON attempts(task_id, id);
CREATE INDEX attempts_retained_by_task ON attempts(task_id, id) WHERE termination_observed=0;
CREATE INDEX events_by_kind_entity ON events(kind,entity,sequence);
CREATE INDEX events_by_kind_sequence ON events(kind,sequence);
CREATE INDEX events_by_entity_kind ON events(entity,kind,sequence);
CREATE INDEX worker_briefs_by_attempt ON operations(json_extract(payload,'$.attempt'),id) WHERE kind='runtime.worker_brief';
CREATE INDEX ownership_by_attempt ON runtime_ownership(attempt_id,binding_id);
CREATE INDEX runtime_bindings_by_task ON runtime_bindings(task_id, id);
CREATE INDEX consumer_bindings_live_task ON consumer_bindings(task_id,binding_id) WHERE retired=0;
CREATE INDEX approvals_by_action ON approval_grants(json_extract(payload,'$.scope.action_digest'),id);
-- Rebuildable pending verifier work; successful and rejected runs both finish
-- a verifier job. Result and run rows remain the immutable source of truth.
CREATE TABLE pending_verification_work (
    submission_id TEXT PRIMARY KEY REFERENCES result_submissions(submission_id),
    created_unix_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX verification_runs_by_submission ON verification_runs(submission_id);
-- A submission stays pending until every acceptance policy of its contract
-- revision has a run; one policy's run must not hide the others.
INSERT INTO pending_verification_work
SELECT s.submission_id,s.created_unix_ms FROM result_submissions s
WHERE NOT EXISTS (SELECT 1 FROM verification_runs r WHERE r.submission_id=s.submission_id)
   OR EXISTS (SELECT 1 FROM acceptance_policies p
              WHERE p.task_id=s.task_id AND p.contract_revision=s.contract_revision
                AND NOT EXISTS (SELECT 1 FROM verification_runs r
                                WHERE r.submission_id=s.submission_id AND r.policy_id=p.policy_id));
CREATE INDEX pending_verification_oldest ON pending_verification_work(created_unix_ms,submission_id);
CREATE TRIGGER pending_verification_on_submission AFTER INSERT ON result_submissions
BEGIN INSERT INTO pending_verification_work VALUES(NEW.submission_id,NEW.created_unix_ms); END;
CREATE TRIGGER pending_verification_on_run AFTER INSERT ON verification_runs
BEGIN DELETE FROM pending_verification_work WHERE submission_id=NEW.submission_id
  AND NOT EXISTS (SELECT 1 FROM acceptance_policies p
                  WHERE p.task_id=NEW.task_id AND p.contract_revision=NEW.contract_revision
                    AND NOT EXISTS (SELECT 1 FROM verification_runs r
                                    WHERE r.submission_id=NEW.submission_id AND r.policy_id=p.policy_id)); END;
CREATE INDEX integration_pending_oldest ON integration_operations(created_unix_ms)
WHERE state IN ('effect_pending','candidate_prepared','validating');
-- Recheck only routine operations whose delivery/cleanup evidence changed.
-- This projection grants no cleanup authority: the scheduler validates the
-- exact occurrence, delivery and completion receipt before removing a row.
CREATE TABLE routine_overlap_work (
    operation_id TEXT PRIMARY KEY REFERENCES routine_occurrences(operation_id),
    name TEXT NOT NULL
) STRICT;
CREATE INDEX routine_overlap_by_name ON routine_overlap_work(name,operation_id);
INSERT INTO routine_overlap_work SELECT operation_id,name FROM routine_occurrences WHERE operation_id IS NOT NULL;
CREATE TRIGGER routine_overlap_on_occurrence AFTER INSERT ON routine_occurrences
WHEN NEW.operation_id IS NOT NULL
BEGIN INSERT INTO routine_overlap_work VALUES(NEW.operation_id,NEW.name); END;
CREATE TRIGGER routine_overlap_on_delivery AFTER UPDATE ON operation_delivery
BEGIN
    INSERT OR IGNORE INTO routine_overlap_work SELECT operation_id,name FROM routine_occurrences WHERE operation_id=NEW.operation_id;
END;
CREATE TRIGGER routine_overlap_on_receipt AFTER INSERT ON events WHEN NEW.kind='routine.completed'
BEGIN
    INSERT OR IGNORE INTO routine_overlap_work SELECT operation_id,name FROM routine_occurrences WHERE operation_id=NEW.entity;
END;
CREATE TRIGGER routine_overlap_on_receipt_update AFTER UPDATE ON events
WHEN OLD.kind='routine.completed' OR NEW.kind='routine.completed'
BEGIN
    INSERT OR IGNORE INTO routine_overlap_work SELECT operation_id,name FROM routine_occurrences WHERE operation_id=OLD.entity OR operation_id=NEW.entity;
END;
CREATE TRIGGER routine_overlap_on_receipt_delete AFTER DELETE ON events WHEN OLD.kind='routine.completed'
BEGIN
    INSERT OR IGNORE INTO routine_overlap_work SELECT operation_id,name FROM routine_occurrences WHERE operation_id=OLD.entity;
END;
-- Advisory scheduling cursor. Reservation still revalidates the current head.
CREATE TABLE admission_scan_cursor (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    control_epoch INTEGER NOT NULL,
    policy_revision INTEGER NOT NULL,
    rank_time INTEGER NOT NULL,
    score INTEGER NOT NULL,
    enqueue_sequence INTEGER NOT NULL,
    task_id TEXT NOT NULL
) STRICT;
-- Repair the omission in previously published schema-34 denial constraints.
DROP TRIGGER IF EXISTS authority_denials_no_update;
DROP TRIGGER IF EXISTS authority_denials_no_delete;
CREATE TABLE authority_denials_v43 (
    id TEXT PRIMARY KEY NOT NULL,
    unix_ms INTEGER NOT NULL,
    class TEXT NOT NULL CHECK (class IN ('approval','budget','routine-store','memory','contract','admission','delegation')),
    command TEXT NOT NULL,
    actor_channel TEXT NOT NULL CHECK (actor_channel IN ('cli-owner','unknown-rejected')),
    reason_code TEXT NOT NULL,
    policy_digest TEXT NOT NULL CHECK (length(policy_digest) = 64),
    expected_head INTEGER,
    actual_head INTEGER
) STRICT;
INSERT INTO authority_denials_v43(id, unix_ms, class, command, actor_channel, reason_code, policy_digest, expected_head, actual_head)
SELECT id, unix_ms, class, command, actor_channel, reason_code, policy_digest, expected_head, actual_head FROM authority_denials;
DROP TABLE authority_denials;
ALTER TABLE authority_denials_v43 RENAME TO authority_denials;
CREATE TRIGGER authority_denials_no_update BEFORE UPDATE ON authority_denials BEGIN SELECT RAISE(ABORT,'authority denial is immutable'); END;
CREATE TRIGGER authority_denials_no_delete BEFORE DELETE ON authority_denials BEGIN SELECT RAISE(ABORT,'authority denial is immutable'); END;

UPDATE store_meta SET schema_version = 43;
PRAGMA user_version = 43;

CREATE INDEX wait_conditions_pending_plan ON wait_conditions(plan_revision,wait_id) WHERE wake_requested=0;
ALTER TABLE wait_conditions ADD COLUMN deadline_unix_ms INTEGER CHECK(deadline_unix_ms IS NULL OR deadline_unix_ms>=0);
CREATE TRIGGER wait_deadline_immutable BEFORE UPDATE OF deadline_unix_ms ON wait_conditions BEGIN SELECT RAISE(ABORT,'wait deadline is immutable'); END;
CREATE TABLE wait_service_cursor (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    plan_revision INTEGER NOT NULL CHECK(plan_revision>=0),
    wait_id TEXT NOT NULL
) STRICT;

CREATE INDEX verification_runs_wait_task ON verification_runs(task_id,contract_revision,attempt_id,created_unix_ms);

CREATE TABLE replan_responses (
    replan_id TEXT PRIMARY KEY REFERENCES replan_requests(replan_id),
    proposal_id TEXT NOT NULL UNIQUE REFERENCES plan_proposals(proposal_id),
    created_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TRIGGER replan_responses_no_update BEFORE UPDATE ON replan_responses BEGIN SELECT RAISE(ABORT,'replan response is immutable'); END;
CREATE TRIGGER replan_responses_no_delete BEFORE DELETE ON replan_responses BEGIN SELECT RAISE(ABORT,'replan response is immutable'); END;

-- Membership needed to rebuild the active projection without visiting retired bindings.
-- Backfill is an explicit migration cost; lifecycle mutations maintain it atomically.
CREATE TABLE active_work_candidates (
    binding_id TEXT PRIMARY KEY NOT NULL REFERENCES runtime_bindings(id) ON DELETE CASCADE
) STRICT;
INSERT INTO active_work_candidates
SELECT b.id FROM runtime_bindings b WHERE b.task_id IS NULL
       OR EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id AND a.termination_observed=0)
       OR NOT EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id)
       OR EXISTS(SELECT 1 FROM runtime_ownership o WHERE o.binding_id=b.id);
UPDATE active_work_meta SET projection_revision=printf('%064d',0) WHERE singleton=1;

CREATE TRIGGER active_candidates_runtime_bindings_insert AFTER INSERT ON runtime_bindings BEGIN
    DELETE FROM active_work_candidates WHERE binding_id=NEW.id;
    INSERT OR IGNORE INTO active_work_candidates
    SELECT b.id FROM runtime_bindings b WHERE b.id=NEW.id AND (b.task_id IS NULL
       OR EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id AND a.termination_observed=0)
       OR NOT EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id)
       OR EXISTS(SELECT 1 FROM runtime_ownership o WHERE o.binding_id=b.id));
    UPDATE active_work_meta SET projection_revision=printf('%064d',0) WHERE singleton=1;
END;

CREATE TRIGGER active_candidates_runtime_bindings_update AFTER UPDATE ON runtime_bindings BEGIN
    DELETE FROM active_work_candidates WHERE binding_id=OLD.id;
    INSERT OR IGNORE INTO active_work_candidates
    SELECT b.id FROM runtime_bindings b WHERE b.id=OLD.id AND (b.task_id IS NULL
       OR EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id AND a.termination_observed=0)
       OR NOT EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id)
       OR EXISTS(SELECT 1 FROM runtime_ownership o WHERE o.binding_id=b.id));
    DELETE FROM active_work_candidates WHERE binding_id=NEW.id;
    INSERT OR IGNORE INTO active_work_candidates
    SELECT b.id FROM runtime_bindings b WHERE b.id=NEW.id AND (b.task_id IS NULL
       OR EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id AND a.termination_observed=0)
       OR NOT EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id)
       OR EXISTS(SELECT 1 FROM runtime_ownership o WHERE o.binding_id=b.id));
    UPDATE active_work_meta SET projection_revision=printf('%064d',0) WHERE singleton=1;
END;

CREATE TRIGGER active_candidates_runtime_bindings_delete AFTER DELETE ON runtime_bindings BEGIN
    DELETE FROM active_work_candidates WHERE binding_id=OLD.id;
    INSERT OR IGNORE INTO active_work_candidates
    SELECT b.id FROM runtime_bindings b WHERE b.id=OLD.id AND (b.task_id IS NULL
       OR EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id AND a.termination_observed=0)
       OR NOT EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id)
       OR EXISTS(SELECT 1 FROM runtime_ownership o WHERE o.binding_id=b.id));
    UPDATE active_work_meta SET projection_revision=printf('%064d',0) WHERE singleton=1;
END;

CREATE TRIGGER active_candidates_attempts_insert AFTER INSERT ON attempts BEGIN
    DELETE FROM active_work_candidates WHERE binding_id IN (SELECT id FROM runtime_bindings WHERE task_id=NEW.task_id);
    INSERT OR IGNORE INTO active_work_candidates
    SELECT b.id FROM runtime_bindings b WHERE b.task_id=NEW.task_id AND (b.task_id IS NULL
       OR EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id AND a.termination_observed=0)
       OR NOT EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id)
       OR EXISTS(SELECT 1 FROM runtime_ownership o WHERE o.binding_id=b.id));
    UPDATE active_work_meta SET projection_revision=printf('%064d',0) WHERE singleton=1;
END;

CREATE TRIGGER active_candidates_attempts_update AFTER UPDATE ON attempts BEGIN
    DELETE FROM active_work_candidates WHERE binding_id IN (SELECT id FROM runtime_bindings WHERE task_id=OLD.task_id);
    INSERT OR IGNORE INTO active_work_candidates
    SELECT b.id FROM runtime_bindings b WHERE b.task_id=OLD.task_id AND (b.task_id IS NULL
       OR EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id AND a.termination_observed=0)
       OR NOT EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id)
       OR EXISTS(SELECT 1 FROM runtime_ownership o WHERE o.binding_id=b.id));
    DELETE FROM active_work_candidates WHERE binding_id IN (SELECT id FROM runtime_bindings WHERE task_id=NEW.task_id);
    INSERT OR IGNORE INTO active_work_candidates
    SELECT b.id FROM runtime_bindings b WHERE b.task_id=NEW.task_id AND (b.task_id IS NULL
       OR EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id AND a.termination_observed=0)
       OR NOT EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id)
       OR EXISTS(SELECT 1 FROM runtime_ownership o WHERE o.binding_id=b.id));
    UPDATE active_work_meta SET projection_revision=printf('%064d',0) WHERE singleton=1;
END;

CREATE TRIGGER active_candidates_attempts_delete AFTER DELETE ON attempts BEGIN
    DELETE FROM active_work_candidates WHERE binding_id IN (SELECT id FROM runtime_bindings WHERE task_id=OLD.task_id);
    INSERT OR IGNORE INTO active_work_candidates
    SELECT b.id FROM runtime_bindings b WHERE b.task_id=OLD.task_id AND (b.task_id IS NULL
       OR EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id AND a.termination_observed=0)
       OR NOT EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id)
       OR EXISTS(SELECT 1 FROM runtime_ownership o WHERE o.binding_id=b.id));
    UPDATE active_work_meta SET projection_revision=printf('%064d',0) WHERE singleton=1;
END;

CREATE TRIGGER active_candidates_runtime_ownership_insert AFTER INSERT ON runtime_ownership BEGIN
    DELETE FROM active_work_candidates WHERE binding_id IN (SELECT id FROM runtime_bindings WHERE id=NEW.binding_id);
    INSERT OR IGNORE INTO active_work_candidates
    SELECT b.id FROM runtime_bindings b WHERE b.id=NEW.binding_id AND (b.task_id IS NULL
       OR EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id AND a.termination_observed=0)
       OR NOT EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id)
       OR EXISTS(SELECT 1 FROM runtime_ownership o WHERE o.binding_id=b.id));
    UPDATE active_work_meta SET projection_revision=printf('%064d',0) WHERE singleton=1;
END;

CREATE TRIGGER active_candidates_runtime_ownership_update AFTER UPDATE ON runtime_ownership BEGIN
    DELETE FROM active_work_candidates WHERE binding_id IN (SELECT id FROM runtime_bindings WHERE id=OLD.binding_id);
    INSERT OR IGNORE INTO active_work_candidates
    SELECT b.id FROM runtime_bindings b WHERE b.id=OLD.binding_id AND (b.task_id IS NULL
       OR EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id AND a.termination_observed=0)
       OR NOT EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id)
       OR EXISTS(SELECT 1 FROM runtime_ownership o WHERE o.binding_id=b.id));
    DELETE FROM active_work_candidates WHERE binding_id IN (SELECT id FROM runtime_bindings WHERE id=NEW.binding_id);
    INSERT OR IGNORE INTO active_work_candidates
    SELECT b.id FROM runtime_bindings b WHERE b.id=NEW.binding_id AND (b.task_id IS NULL
       OR EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id AND a.termination_observed=0)
       OR NOT EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id)
       OR EXISTS(SELECT 1 FROM runtime_ownership o WHERE o.binding_id=b.id));
    UPDATE active_work_meta SET projection_revision=printf('%064d',0) WHERE singleton=1;
END;

CREATE TRIGGER active_candidates_runtime_ownership_delete AFTER DELETE ON runtime_ownership BEGIN
    DELETE FROM active_work_candidates WHERE binding_id IN (SELECT id FROM runtime_bindings WHERE id=OLD.binding_id);
    INSERT OR IGNORE INTO active_work_candidates
    SELECT b.id FROM runtime_bindings b WHERE b.id=OLD.binding_id AND (b.task_id IS NULL
       OR EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id AND a.termination_observed=0)
       OR NOT EXISTS(SELECT 1 FROM attempts a WHERE a.task_id=b.task_id)
       OR EXISTS(SELECT 1 FROM runtime_ownership o WHERE o.binding_id=b.id));
    UPDATE active_work_meta SET projection_revision=printf('%064d',0) WHERE singleton=1;
END;

CREATE TRIGGER active_candidates_task_attempt AFTER UPDATE OF active_attempt ON tasks BEGIN
    UPDATE active_work_meta SET projection_revision=printf('%064d',0) WHERE singleton=1;
END;

-- Exact delegated actions and their lifetime quota debit commit with reservation.
-- No historical grants acquire authority or quota usage through this backfill.
CREATE TABLE delegated_reservations (
    grant_id TEXT NOT NULL REFERENCES delegation_grants(id),
    idempotency_key TEXT NOT NULL,
    request_digest TEXT NOT NULL CHECK(length(request_digest)=64),
    request_bytes BLOB NOT NULL CHECK(length(request_bytes)<=65536),
    approval_id TEXT NOT NULL UNIQUE REFERENCES approval_grants(id),
    attempt_id TEXT NOT NULL UNIQUE REFERENCES attempts(id),
    response TEXT NOT NULL CHECK(json_valid(response)),
    PRIMARY KEY(grant_id,idempotency_key)
) STRICT;
CREATE TRIGGER delegated_reservations_no_update BEFORE UPDATE ON delegated_reservations BEGIN SELECT RAISE(ABORT,'delegated reservation is immutable'); END;
CREATE TRIGGER delegated_reservations_no_delete BEFORE DELETE ON delegated_reservations BEGIN SELECT RAISE(ABORT,'delegated reservation is immutable'); END;

-- Supporting worker declarations are retained separately from package coverage.
-- Historical receipts are not retroactively certified during upgrade.
CREATE TABLE worker_package_acknowledgments (
    package_id TEXT NOT NULL REFERENCES update_packages(package_id),
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    disposition TEXT NOT NULL CHECK(disposition IN ('seen','applied')),
    package_receipt_sequence INTEGER NOT NULL REFERENCES events(sequence),
    evidence_sequence INTEGER NOT NULL UNIQUE REFERENCES events(sequence),
    PRIMARY KEY(package_id,attempt_id,disposition)
) STRICT;
CREATE TRIGGER worker_package_acknowledgments_no_update BEFORE UPDATE ON worker_package_acknowledgments BEGIN SELECT RAISE(ABORT,'worker package evidence is immutable'); END;
CREATE TRIGGER worker_package_acknowledgments_no_delete BEFORE DELETE ON worker_package_acknowledgments BEGIN SELECT RAISE(ABORT,'worker package evidence is immutable'); END;

-- Repacking preserves each logical receipt's original package and event.
-- Existing historical retargets are retained verbatim; future receipts cannot
-- be moved, even when the same change is acknowledged in another package.
DROP TRIGGER memory_change_receipts_no_update;
CREATE TRIGGER memory_change_receipts_no_update BEFORE UPDATE ON memory_change_receipts
BEGIN SELECT RAISE(ABORT,'memory change receipt is immutable'); END;

-- Optional delivery retirement has distinct evidence; it is never an applied
-- receipt for the obsolete change. No historical obligation is backfilled.
CREATE TABLE memory_update_supersessions (
    binding_id TEXT NOT NULL REFERENCES consumer_bindings(binding_id),
    change_id TEXT NOT NULL REFERENCES memory_delivery_intents(id),
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    replacement_change_id TEXT NOT NULL REFERENCES memory_delivery_intents(id),
    replacement_receipt_sequence INTEGER NOT NULL REFERENCES events(sequence),
    sequence INTEGER NOT NULL UNIQUE REFERENCES events(sequence),
    receipt TEXT NOT NULL CHECK(json_valid(receipt)),
    PRIMARY KEY(binding_id,change_id),
    FOREIGN KEY(binding_id,change_id) REFERENCES consumer_binding_obligations(binding_id,delivery_id),
    FOREIGN KEY(binding_id,replacement_change_id) REFERENCES consumer_binding_obligations(binding_id,delivery_id),
    CHECK(change_id<>replacement_change_id)
) STRICT;
CREATE TRIGGER memory_update_supersessions_no_update BEFORE UPDATE ON memory_update_supersessions
BEGIN SELECT RAISE(ABORT,'memory supersession is immutable'); END;
CREATE TRIGGER memory_update_supersessions_no_delete BEFORE DELETE ON memory_update_supersessions
BEGIN SELECT RAISE(ABORT,'memory supersession is immutable'); END;

-- New barriers bind their complete memory read set. Keep legacy headers and
-- release history verbatim; do not invent a v2 read set for an old freeze.
CREATE TABLE barrier_memory_read_sets (
    barrier_id TEXT PRIMARY KEY REFERENCES barrier_revisions(barrier_id),
    schema_version INTEGER NOT NULL CHECK(schema_version=2),
    payload TEXT NOT NULL CHECK(json_valid(payload) AND octet_length(payload)<=8388608)
) STRICT;
CREATE TRIGGER barrier_memory_read_sets_no_update BEFORE UPDATE ON barrier_memory_read_sets
BEGIN SELECT RAISE(ABORT,'barrier memory read set is immutable'); END;
CREATE TRIGGER barrier_memory_read_sets_no_delete BEFORE DELETE ON barrier_memory_read_sets
BEGIN SELECT RAISE(ABORT,'barrier memory read set is immutable'); END;

-- Exact signed release bytes are distinct from the deterministic release token.
-- Historical releases acquire no invented authorization on upgrade.
CREATE TABLE barrier_release_authorizations (
    barrier_id TEXT PRIMARY KEY REFERENCES barrier_revisions(barrier_id),
    authorization_digest TEXT NOT NULL UNIQUE CHECK(length(authorization_digest)=64),
    raw BLOB NOT NULL CHECK(length(raw)<=65536),
    sequence INTEGER NOT NULL UNIQUE REFERENCES events(sequence)
) STRICT;
CREATE TRIGGER barrier_release_authorizations_no_update BEFORE UPDATE ON barrier_release_authorizations
BEGIN SELECT RAISE(ABORT,'barrier release authorization is immutable'); END;
CREATE TRIGGER barrier_release_authorizations_no_delete BEFORE DELETE ON barrier_release_authorizations
BEGIN SELECT RAISE(ABORT,'barrier release authorization is immutable'); END;

-- Release is historical evidence, not permanent applicability. Preserve the
-- original schema-40 headers and append revocation of an already released wave.
CREATE TABLE barrier_release_revocations (
    barrier_id TEXT PRIMARY KEY REFERENCES barrier_revisions(barrier_id),
    sequence INTEGER NOT NULL REFERENCES events(sequence)
) STRICT;
CREATE TRIGGER barrier_release_revocations_require_release BEFORE INSERT ON barrier_release_revocations
WHEN NOT EXISTS(SELECT 1 FROM barrier_revisions WHERE barrier_id=NEW.barrier_id AND released_seq IS NOT NULL AND released_seq<NEW.sequence AND revoked_seq IS NULL)
  OR NOT EXISTS(SELECT 1 FROM events WHERE sequence=NEW.sequence AND kind='barrier.revoked' AND entity=NEW.barrier_id)
BEGIN SELECT RAISE(ABORT,'released barrier required'); END;
CREATE TRIGGER barrier_release_revocations_no_update BEFORE UPDATE ON barrier_release_revocations
BEGIN SELECT RAISE(ABORT,'barrier release revocation is immutable'); END;
CREATE TRIGGER barrier_release_revocations_no_delete BEFORE DELETE ON barrier_release_revocations
BEGIN SELECT RAISE(ABORT,'barrier release revocation is immutable'); END;
CREATE VIEW barrier_current_status AS
SELECT b.barrier_id,b.required_set_generation,b.memory_manifest_digest,b.release_token,
       b.released_seq,coalesce(b.revoked_seq,r.sequence) AS revoked_seq,b.created_seq
FROM barrier_revisions b LEFT JOIN barrier_release_revocations r ON r.barrier_id=b.barrier_id;

-- Only still-applicable memberships participate in invalidation routing.
-- Backfill a projection of retained facts, never a release or revocation.
CREATE TABLE barrier_open_members (
    barrier_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    PRIMARY KEY(barrier_id,task_id),
    FOREIGN KEY(barrier_id,task_id) REFERENCES barrier_members(barrier_id,task_id)
) STRICT;
CREATE INDEX barrier_open_members_by_task ON barrier_open_members(task_id,barrier_id);
INSERT INTO barrier_open_members
SELECT m.barrier_id,m.task_id FROM barrier_members m
JOIN barrier_current_status b ON b.barrier_id=m.barrier_id WHERE b.revoked_seq IS NULL;
CREATE TRIGGER barrier_open_members_valid_insert BEFORE INSERT ON barrier_open_members
WHEN NOT EXISTS(SELECT 1 FROM barrier_members m JOIN barrier_current_status b ON b.barrier_id=m.barrier_id
                WHERE m.barrier_id=NEW.barrier_id AND m.task_id=NEW.task_id AND b.revoked_seq IS NULL)
BEGIN SELECT RAISE(ABORT,'applicable barrier membership required'); END;
CREATE TRIGGER barrier_open_members_no_update BEFORE UPDATE ON barrier_open_members
BEGIN SELECT RAISE(ABORT,'barrier routing identity is immutable'); END;
CREATE TRIGGER barrier_open_members_no_live_delete BEFORE DELETE ON barrier_open_members
WHEN EXISTS(SELECT 1 FROM barrier_current_status WHERE barrier_id=OLD.barrier_id AND revoked_seq IS NULL)
BEGIN SELECT RAISE(ABORT,'applicable barrier membership cannot be removed'); END;
CREATE TRIGGER barrier_open_members_on_member AFTER INSERT ON barrier_members
BEGIN
    INSERT INTO barrier_open_members SELECT NEW.barrier_id,NEW.task_id
    WHERE EXISTS(SELECT 1 FROM barrier_current_status WHERE barrier_id=NEW.barrier_id AND revoked_seq IS NULL);
END;
CREATE TRIGGER barrier_open_members_on_pending_revocation AFTER UPDATE OF revoked_seq ON barrier_revisions
WHEN NEW.revoked_seq IS NOT NULL
BEGIN DELETE FROM barrier_open_members WHERE barrier_id=NEW.barrier_id; END;
CREATE TRIGGER barrier_open_members_on_release_revocation AFTER INSERT ON barrier_release_revocations
BEGIN DELETE FROM barrier_open_members WHERE barrier_id=NEW.barrier_id; END;

-- A revocation event and its applicability transition are one statement. This
-- also covers revocations emitted while promoting/routing memory changes.
CREATE TRIGGER barrier_revocation_on_event AFTER INSERT ON events WHEN NEW.kind='barrier.revoked'
BEGIN
    SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM barrier_current_status WHERE barrier_id=NEW.entity)
        THEN RAISE(ABORT,'known barrier required for revocation') END;
    -- A materialized multi-barrier invalidation can also contain a descendant
    -- already withdrawn by an earlier row's ancestry propagation. Retain that
    -- additional cause event without replacing the first applicability decision.
    INSERT INTO barrier_release_revocations
        SELECT barrier_id,NEW.sequence FROM barrier_current_status
        WHERE barrier_id=NEW.entity AND released_seq IS NOT NULL AND revoked_seq IS NULL;
    UPDATE barrier_revisions SET revoked_seq=NEW.sequence
        WHERE barrier_id=NEW.entity AND released_seq IS NULL AND revoked_seq IS NULL
          AND EXISTS(SELECT 1 FROM barrier_current_status WHERE barrier_id=NEW.entity AND revoked_seq IS NULL);
END;

CREATE TRIGGER barrier_revoke_on_memory_invalidation AFTER INSERT ON memory_invalidations
WHEN NEW.severity!='informational' AND NEW.resolved_seq IS NULL
BEGIN
    SELECT CASE WHEN (SELECT count(*) FROM (
        SELECT barrier_id FROM barrier_open_members WHERE task_id=NEW.task_id
        UNION ALL
        SELECT barrier_id FROM barrier_open_members WHERE NEW.task_id IS NULL GROUP BY barrier_id
        LIMIT 1001
    ))>1000 THEN RAISE(ABORT,'barrier invalidation exceeds 1000 affected barriers') END;
    INSERT INTO events(kind,entity,revision,payload_version,payload)
    SELECT 'barrier.revoked',barrier_id,1,1,
           json_object('invalidation_id',NEW.id,'triggering_seq',NEW.triggering_seq)
    FROM (
        -- Revocation removes routing rows. Materialize the complete bounded
        -- candidate set before any event trigger changes that projection.
        WITH affected AS MATERIALIZED (
            SELECT barrier_id FROM barrier_open_members WHERE task_id=NEW.task_id
            UNION ALL
            SELECT barrier_id FROM barrier_open_members WHERE NEW.task_id IS NULL GROUP BY barrier_id
            LIMIT 1001
        ) SELECT barrier_id FROM affected
    ) ORDER BY barrier_id;
END;

-- Upgrade is an administrative full-state boundary. Previously unresolved
-- invalidations must not leave historical releases applicable after upgrade.
-- Record a new migration-time decision with its actual retained cause; never
-- rewrite the old release or manufacture an old revocation timestamp.
INSERT INTO events(kind,entity,revision,payload_version,payload)
SELECT 'barrier.revoked',a.barrier_id,1,1,
       json_object('invalidation_id',i.id,'triggering_seq',i.triggering_seq,'reason','schema43_unresolved_invalidation')
FROM (
    WITH affected AS MATERIALIZED (
        SELECT m.barrier_id,min(i.id) AS invalidation_id
        FROM barrier_open_members m JOIN memory_invalidations i ON i.task_id=m.task_id OR i.task_id IS NULL
        WHERE i.resolved_seq IS NULL AND i.severity!='informational'
        GROUP BY m.barrier_id
    ) SELECT * FROM affected
) a JOIN memory_invalidations i ON i.id=a.invalidation_id
ORDER BY a.barrier_id;

-- Frozen memory dependencies outlive worker delivery bindings. These are
-- rebuildable routing projections of immutable read sets, not new evidence.
CREATE TABLE barrier_open_memory_records (
    barrier_id TEXT NOT NULL REFERENCES barrier_memory_read_sets(barrier_id),
    record_id TEXT NOT NULL REFERENCES memory_records(id),
    PRIMARY KEY(barrier_id,record_id)
) STRICT;
CREATE INDEX barrier_open_memory_records_by_record ON barrier_open_memory_records(record_id,barrier_id);
CREATE TABLE barrier_open_memory_unknown (
    barrier_id TEXT PRIMARY KEY REFERENCES barrier_revisions(barrier_id)
) STRICT;
INSERT INTO barrier_open_memory_records
SELECT s.barrier_id,json_extract(c.value,'$[0]')
FROM barrier_memory_read_sets s JOIN barrier_current_status b ON b.barrier_id=s.barrier_id,
     json_each(s.payload,'$.members') m,json_each(m.value,'$.consumed') c
WHERE b.revoked_seq IS NULL
UNION
SELECT s.barrier_id,json_extract(r.value,'$[0]')
FROM barrier_memory_read_sets s JOIN barrier_current_status b ON b.barrier_id=s.barrier_id,
     json_each(s.payload,'$.required') r WHERE b.revoked_seq IS NULL;
INSERT INTO barrier_open_memory_unknown
SELECT DISTINCT m.barrier_id FROM barrier_open_members m
WHERE NOT EXISTS(SELECT 1 FROM barrier_memory_read_sets s WHERE s.barrier_id=m.barrier_id);

CREATE TRIGGER barrier_open_memory_records_valid_insert BEFORE INSERT ON barrier_open_memory_records
WHEN NOT EXISTS(SELECT 1 FROM barrier_current_status WHERE barrier_id=NEW.barrier_id AND revoked_seq IS NULL)
BEGIN SELECT RAISE(ABORT,'applicable memory read set required'); END;
CREATE TRIGGER barrier_open_memory_records_no_update BEFORE UPDATE ON barrier_open_memory_records
BEGIN SELECT RAISE(ABORT,'barrier memory routing identity is immutable'); END;
CREATE TRIGGER barrier_open_memory_records_no_live_delete BEFORE DELETE ON barrier_open_memory_records
WHEN EXISTS(SELECT 1 FROM barrier_current_status WHERE barrier_id=OLD.barrier_id AND revoked_seq IS NULL)
BEGIN SELECT RAISE(ABORT,'applicable memory routing cannot be removed'); END;
CREATE TRIGGER barrier_open_memory_unknown_valid_insert BEFORE INSERT ON barrier_open_memory_unknown
WHEN NOT EXISTS(SELECT 1 FROM barrier_current_status WHERE barrier_id=NEW.barrier_id AND revoked_seq IS NULL)
  OR EXISTS(SELECT 1 FROM barrier_memory_read_sets WHERE barrier_id=NEW.barrier_id)
BEGIN SELECT RAISE(ABORT,'applicable legacy barrier required'); END;
CREATE TRIGGER barrier_open_memory_unknown_no_update BEFORE UPDATE ON barrier_open_memory_unknown
BEGIN SELECT RAISE(ABORT,'legacy barrier routing identity is immutable'); END;
CREATE TRIGGER barrier_open_memory_unknown_no_live_delete BEFORE DELETE ON barrier_open_memory_unknown
WHEN EXISTS(SELECT 1 FROM barrier_current_status WHERE barrier_id=OLD.barrier_id AND revoked_seq IS NULL)
 AND NOT EXISTS(SELECT 1 FROM barrier_memory_read_sets WHERE barrier_id=OLD.barrier_id)
BEGIN SELECT RAISE(ABORT,'applicable legacy routing cannot be removed'); END;

CREATE TRIGGER barrier_memory_routing_on_read_set AFTER INSERT ON barrier_memory_read_sets
BEGIN
    INSERT INTO barrier_open_memory_records
    SELECT NEW.barrier_id,json_extract(c.value,'$[0]')
    FROM json_each(NEW.payload,'$.members') m,json_each(m.value,'$.consumed') c
    WHERE EXISTS(SELECT 1 FROM barrier_current_status WHERE barrier_id=NEW.barrier_id AND revoked_seq IS NULL)
    UNION
    SELECT NEW.barrier_id,json_extract(r.value,'$[0]') FROM json_each(NEW.payload,'$.required') r
    WHERE EXISTS(SELECT 1 FROM barrier_current_status WHERE barrier_id=NEW.barrier_id AND revoked_seq IS NULL);
    DELETE FROM barrier_open_memory_unknown WHERE barrier_id=NEW.barrier_id;
END;
CREATE TRIGGER barrier_memory_routing_on_legacy_member AFTER INSERT ON barrier_members
BEGIN
    INSERT OR IGNORE INTO barrier_open_memory_unknown SELECT NEW.barrier_id
    WHERE EXISTS(SELECT 1 FROM barrier_current_status WHERE barrier_id=NEW.barrier_id AND revoked_seq IS NULL)
      AND NOT EXISTS(SELECT 1 FROM barrier_memory_read_sets WHERE barrier_id=NEW.barrier_id);
END;
CREATE TRIGGER barrier_memory_routing_on_pending_revocation AFTER UPDATE OF revoked_seq ON barrier_revisions
WHEN NEW.revoked_seq IS NOT NULL
BEGIN
    DELETE FROM barrier_open_memory_records WHERE barrier_id=NEW.barrier_id;
    DELETE FROM barrier_open_memory_unknown WHERE barrier_id=NEW.barrier_id;
END;
CREATE TRIGGER barrier_memory_routing_on_release_revocation AFTER INSERT ON barrier_release_revocations
BEGIN
    DELETE FROM barrier_open_memory_records WHERE barrier_id=NEW.barrier_id;
    DELETE FROM barrier_open_memory_unknown WHERE barrier_id=NEW.barrier_id;
END;

-- Retain each attempt's exact prerequisite independently of its task's later
-- contracts. Route withdrawal only through live, not yet invalidated consumers.
CREATE TABLE attempt_required_releases (
    attempt_id TEXT PRIMARY KEY REFERENCES attempt_inputs(attempt_id),
    barrier_id TEXT NOT NULL REFERENCES barrier_release_authorizations(barrier_id),
    release_sequence INTEGER NOT NULL CHECK(release_sequence>0),
    authorization_digest TEXT NOT NULL CHECK(length(authorization_digest)=64)
) STRICT;
INSERT INTO attempt_required_releases
SELECT i.attempt_id,json_extract(CAST(c.raw_bytes AS TEXT),'$.required_barrier.barrier_id'),
       json_extract(CAST(c.raw_bytes AS TEXT),'$.required_barrier.release_sequence'),
       json_extract(CAST(c.raw_bytes AS TEXT),'$.required_barrier.authorization_digest')
FROM attempt_inputs i JOIN task_contracts c
 ON c.task_id=json_extract(i.payload,'$.inputs.task_contract.id')
 AND c.task_id=json_extract(i.payload,'$.inputs.task')
 AND c.contract_revision=json_extract(i.payload,'$.inputs.task_contract.revision')
 AND c.raw_digest=json_extract(i.payload,'$.inputs.task_contract.digest')
WHERE CASE WHEN json_valid(CAST(c.raw_bytes AS TEXT)) THEN json_extract(CAST(c.raw_bytes AS TEXT),'$.version') END=2;
CREATE TRIGGER attempt_required_releases_valid_insert BEFORE INSERT ON attempt_required_releases
WHEN NOT EXISTS(
    SELECT 1 FROM attempt_inputs i JOIN task_contracts c
      ON c.task_id=json_extract(i.payload,'$.inputs.task_contract.id')
      AND c.task_id=json_extract(i.payload,'$.inputs.task')
      AND c.contract_revision=json_extract(i.payload,'$.inputs.task_contract.revision')
      AND c.raw_digest=json_extract(i.payload,'$.inputs.task_contract.digest')
    JOIN barrier_release_authorizations a ON a.barrier_id=NEW.barrier_id
      AND a.sequence=NEW.release_sequence AND a.authorization_digest=NEW.authorization_digest
    JOIN barrier_current_status b ON b.barrier_id=a.barrier_id AND b.revoked_seq IS NULL
    WHERE i.attempt_id=NEW.attempt_id AND json_extract(CAST(c.raw_bytes AS TEXT),'$.version')=2
      AND json_extract(CAST(c.raw_bytes AS TEXT),'$.required_barrier.barrier_id')=NEW.barrier_id
      AND json_extract(CAST(c.raw_bytes AS TEXT),'$.required_barrier.release_sequence')=NEW.release_sequence
      AND json_extract(CAST(c.raw_bytes AS TEXT),'$.required_barrier.authorization_digest')=NEW.authorization_digest
)
BEGIN SELECT RAISE(ABORT,'exact live reserved barrier release required'); END;
CREATE TRIGGER attempt_required_releases_no_update BEFORE UPDATE ON attempt_required_releases
BEGIN SELECT RAISE(ABORT,'attempt barrier requirement is immutable'); END;
CREATE TRIGGER attempt_required_releases_no_delete BEFORE DELETE ON attempt_required_releases
BEGIN SELECT RAISE(ABORT,'attempt barrier requirement is immutable'); END;
CREATE TRIGGER attempt_required_releases_on_inputs AFTER INSERT ON attempt_inputs
BEGIN
    INSERT INTO attempt_required_releases
    SELECT NEW.attempt_id,json_extract(CAST(c.raw_bytes AS TEXT),'$.required_barrier.barrier_id'),
           json_extract(CAST(c.raw_bytes AS TEXT),'$.required_barrier.release_sequence'),
           json_extract(CAST(c.raw_bytes AS TEXT),'$.required_barrier.authorization_digest')
    FROM task_contracts c
    WHERE c.task_id=json_extract(NEW.payload,'$.inputs.task_contract.id')
      AND c.task_id=json_extract(NEW.payload,'$.inputs.task')
      AND c.contract_revision=json_extract(NEW.payload,'$.inputs.task_contract.revision')
      AND c.raw_digest=json_extract(NEW.payload,'$.inputs.task_contract.digest')
      AND CASE WHEN json_valid(CAST(c.raw_bytes AS TEXT)) THEN json_extract(CAST(c.raw_bytes AS TEXT),'$.version') END=2;
END;

CREATE TABLE attempt_barrier_invalidations (
    attempt_id TEXT PRIMARY KEY REFERENCES attempt_required_releases(attempt_id),
    revocation_sequence INTEGER NOT NULL REFERENCES events(sequence),
    sequence INTEGER NOT NULL UNIQUE REFERENCES events(sequence),
    CHECK(sequence>revocation_sequence)
) STRICT;
CREATE TRIGGER attempt_barrier_invalidations_valid_insert BEFORE INSERT ON attempt_barrier_invalidations
WHEN NOT EXISTS(
    SELECT 1 FROM attempt_required_releases r JOIN barrier_current_status b ON b.barrier_id=r.barrier_id
    JOIN events e ON e.sequence=NEW.sequence AND e.kind='attempt.barrier_invalidated' AND e.entity=r.attempt_id
    WHERE r.attempt_id=NEW.attempt_id AND b.revoked_seq=NEW.revocation_sequence
      AND json_extract(e.payload,'$.barrier_id')=r.barrier_id
      AND json_extract(e.payload,'$.revocation_sequence')=NEW.revocation_sequence
)
BEGIN SELECT RAISE(ABORT,'exact barrier revocation event required'); END;
CREATE TRIGGER attempt_barrier_invalidations_no_update BEFORE UPDATE ON attempt_barrier_invalidations
BEGIN SELECT RAISE(ABORT,'attempt barrier invalidation is immutable'); END;
CREATE TRIGGER attempt_barrier_invalidations_no_delete BEFORE DELETE ON attempt_barrier_invalidations
BEGIN SELECT RAISE(ABORT,'attempt barrier invalidation is immutable'); END;

CREATE TABLE barrier_live_consumers (
    barrier_id TEXT NOT NULL REFERENCES barrier_release_authorizations(barrier_id),
    attempt_id TEXT NOT NULL UNIQUE REFERENCES attempt_required_releases(attempt_id),
    PRIMARY KEY(barrier_id,attempt_id)
) STRICT;
CREATE TRIGGER barrier_live_consumers_valid_insert BEFORE INSERT ON barrier_live_consumers
WHEN NOT EXISTS(SELECT 1 FROM attempt_required_releases r JOIN attempts a ON a.id=r.attempt_id
     JOIN barrier_current_status b ON b.barrier_id=r.barrier_id
     WHERE r.attempt_id=NEW.attempt_id AND r.barrier_id=NEW.barrier_id AND a.termination_observed=0
       AND b.revoked_seq IS NULL AND NOT EXISTS(SELECT 1 FROM attempt_barrier_invalidations x WHERE x.attempt_id=r.attempt_id))
BEGIN SELECT RAISE(ABORT,'live applicable attempt barrier required'); END;
-- Bound admission rather than allowing an admitted population that cannot be
-- invalidated atomically within the corresponding routing bound.
CREATE TRIGGER barrier_live_consumers_limit BEFORE INSERT ON barrier_live_consumers
WHEN (SELECT count(*) FROM (SELECT 1 FROM barrier_live_consumers WHERE barrier_id=NEW.barrier_id LIMIT 1000))>=1000
BEGIN SELECT RAISE(ABORT,'barrier exceeds 1000 live consumers'); END;
CREATE TRIGGER barrier_live_consumers_no_update BEFORE UPDATE ON barrier_live_consumers
BEGIN SELECT RAISE(ABORT,'barrier consumer routing identity is immutable'); END;
CREATE TRIGGER barrier_live_consumers_no_live_delete BEFORE DELETE ON barrier_live_consumers
WHEN EXISTS(SELECT 1 FROM attempts a WHERE a.id=OLD.attempt_id AND a.termination_observed=0)
 AND NOT EXISTS(SELECT 1 FROM attempt_barrier_invalidations x WHERE x.attempt_id=OLD.attempt_id)
BEGIN SELECT RAISE(ABORT,'live barrier consumer cannot be removed'); END;
INSERT INTO barrier_live_consumers
SELECT r.barrier_id,r.attempt_id FROM attempt_required_releases r JOIN attempts a ON a.id=r.attempt_id
JOIN barrier_current_status b ON b.barrier_id=r.barrier_id WHERE a.termination_observed=0 AND b.revoked_seq IS NULL;
CREATE TRIGGER barrier_live_consumers_on_requirement AFTER INSERT ON attempt_required_releases
BEGIN
    INSERT INTO barrier_live_consumers SELECT NEW.barrier_id,NEW.attempt_id
    WHERE EXISTS(SELECT 1 FROM attempts WHERE id=NEW.attempt_id AND termination_observed=0);
END;
CREATE TRIGGER barrier_live_consumers_on_termination AFTER UPDATE OF termination_observed ON attempts
WHEN NEW.termination_observed=1
BEGIN DELETE FROM barrier_live_consumers WHERE attempt_id=NEW.id; END;
CREATE TRIGGER barrier_live_consumers_on_invalidation AFTER INSERT ON attempt_barrier_invalidations
BEGIN DELETE FROM barrier_live_consumers WHERE attempt_id=NEW.attempt_id; END;

CREATE TRIGGER attempt_barrier_invalidation_on_event AFTER INSERT ON events
WHEN NEW.kind='attempt.barrier_invalidated'
BEGIN
    INSERT INTO attempt_barrier_invalidations(attempt_id,revocation_sequence,sequence)
    VALUES(NEW.entity,json_extract(NEW.payload,'$.revocation_sequence'),NEW.sequence);
END;
CREATE TRIGGER barrier_invalidate_live_consumers AFTER INSERT ON barrier_release_revocations
BEGIN
    INSERT INTO events(kind,entity,revision,payload_version,payload)
    SELECT 'attempt.barrier_invalidated',attempt_id,1,1,
           json_object('barrier_id',NEW.barrier_id,'revocation_sequence',NEW.sequence)
    FROM (
        WITH affected AS MATERIALIZED (
            SELECT attempt_id FROM barrier_live_consumers WHERE barrier_id=NEW.barrier_id ORDER BY attempt_id LIMIT 1000
        ) SELECT attempt_id FROM affected
    );
END;
-- Rebuild from retained authority, recording a present migration-time decision
-- for any live historical consumer whose release was already revoked.
INSERT INTO events(kind,entity,revision,payload_version,payload)
SELECT 'attempt.barrier_invalidated',r.attempt_id,1,1,
       json_object('barrier_id',r.barrier_id,'revocation_sequence',b.revoked_seq,'reason','schema43_retained_revocation')
FROM attempt_required_releases r JOIN attempts a ON a.id=r.attempt_id
JOIN barrier_current_status b ON b.barrier_id=r.barrier_id
WHERE a.termination_observed=0 AND b.revoked_seq IS NOT NULL ORDER BY r.attempt_id;

-- Exact transitive ancestry is derived when membership is frozen, rather than
-- rediscovered from retired attempt/result history during urgent withdrawal.
CREATE TABLE barrier_ancestors (
    barrier_id TEXT NOT NULL REFERENCES barrier_revisions(barrier_id),
    ancestor_id TEXT NOT NULL REFERENCES barrier_release_authorizations(barrier_id),
    PRIMARY KEY(barrier_id,ancestor_id),
    CHECK(barrier_id<>ancestor_id)
) STRICT;
CREATE TABLE barrier_ancestor_invalidations (
    barrier_id TEXT PRIMARY KEY REFERENCES barrier_revisions(barrier_id),
    ancestor_id TEXT NOT NULL,
    revocation_sequence INTEGER NOT NULL REFERENCES events(sequence),
    sequence INTEGER NOT NULL UNIQUE REFERENCES events(sequence),
    FOREIGN KEY(barrier_id,ancestor_id) REFERENCES barrier_ancestors(barrier_id,ancestor_id),
    CHECK(sequence>revocation_sequence)
) STRICT;
DROP VIEW barrier_current_status;
CREATE VIEW barrier_current_status AS
SELECT b.barrier_id,b.required_set_generation,b.memory_manifest_digest,b.release_token,
       b.released_seq,coalesce(b.revoked_seq,r.sequence,a.sequence) AS revoked_seq,b.created_seq
FROM barrier_revisions b
LEFT JOIN barrier_release_revocations r ON r.barrier_id=b.barrier_id
LEFT JOIN barrier_ancestor_invalidations a ON a.barrier_id=b.barrier_id;

CREATE TRIGGER barrier_ancestors_valid_insert BEFORE INSERT ON barrier_ancestors
WHEN NOT EXISTS(
    SELECT 1 FROM barrier_members m
    JOIN attempt_required_releases r ON r.attempt_id=m.attempt_id
    JOIN barrier_current_status p ON p.barrier_id=r.barrier_id AND p.revoked_seq IS NULL
    JOIN barrier_current_status b ON b.barrier_id=m.barrier_id AND b.revoked_seq IS NULL
    JOIN barrier_release_authorizations a ON a.barrier_id=NEW.ancestor_id AND a.sequence<b.created_seq
    WHERE m.barrier_id=NEW.barrier_id AND (r.barrier_id=NEW.ancestor_id OR EXISTS(
        SELECT 1 FROM barrier_ancestors x WHERE x.barrier_id=r.barrier_id AND x.ancestor_id=NEW.ancestor_id))
)
BEGIN SELECT RAISE(ABORT,'exact earlier barrier prerequisite required'); END;
CREATE TRIGGER barrier_ancestors_limit BEFORE INSERT ON barrier_ancestors
WHEN NOT EXISTS(SELECT 1 FROM barrier_ancestors WHERE barrier_id=NEW.barrier_id AND ancestor_id=NEW.ancestor_id)
 AND (SELECT count(*) FROM (SELECT 1 FROM barrier_ancestors WHERE barrier_id=NEW.barrier_id LIMIT 1000))>=1000
BEGIN SELECT RAISE(ABORT,'barrier exceeds 1000 prerequisite releases'); END;
CREATE TRIGGER barrier_ancestors_no_update BEFORE UPDATE ON barrier_ancestors
BEGIN SELECT RAISE(ABORT,'barrier ancestry is immutable'); END;
CREATE TRIGGER barrier_ancestors_no_delete BEFORE DELETE ON barrier_ancestors
BEGIN SELECT RAISE(ABORT,'barrier ancestry is immutable'); END;
CREATE TRIGGER barrier_ancestors_on_member AFTER INSERT ON barrier_members
BEGIN
    INSERT OR IGNORE INTO barrier_ancestors
    SELECT NEW.barrier_id,r.barrier_id FROM attempt_required_releases r WHERE r.attempt_id=NEW.attempt_id
    UNION
    SELECT NEW.barrier_id,a.ancestor_id FROM attempt_required_releases r
      JOIN barrier_ancestors a ON a.barrier_id=r.barrier_id WHERE r.attempt_id=NEW.attempt_id;
END;

CREATE TABLE barrier_open_descendants (
    ancestor_id TEXT NOT NULL,
    barrier_id TEXT NOT NULL,
    PRIMARY KEY(ancestor_id,barrier_id),
    FOREIGN KEY(barrier_id,ancestor_id) REFERENCES barrier_ancestors(barrier_id,ancestor_id)
) STRICT;
CREATE INDEX barrier_open_descendants_by_barrier ON barrier_open_descendants(barrier_id,ancestor_id);
CREATE TRIGGER barrier_open_descendants_valid_insert BEFORE INSERT ON barrier_open_descendants
WHEN NOT EXISTS(SELECT 1 FROM barrier_ancestors a
    JOIN barrier_current_status b ON b.barrier_id=a.barrier_id AND b.revoked_seq IS NULL
    JOIN barrier_current_status p ON p.barrier_id=a.ancestor_id AND p.revoked_seq IS NULL
    WHERE a.barrier_id=NEW.barrier_id AND a.ancestor_id=NEW.ancestor_id)
BEGIN SELECT RAISE(ABORT,'applicable barrier ancestry required'); END;
-- Refuse oversized fanout at freeze, never at urgent revocation of already
-- admitted work. Revoked descendants retain history but leave this projection.
CREATE TRIGGER barrier_open_descendants_limit BEFORE INSERT ON barrier_open_descendants
WHEN (SELECT count(*) FROM (SELECT 1 FROM barrier_open_descendants WHERE ancestor_id=NEW.ancestor_id LIMIT 1000))>=1000
BEGIN SELECT RAISE(ABORT,'barrier exceeds 1000 applicable descendants'); END;
CREATE TRIGGER barrier_open_descendants_no_update BEFORE UPDATE ON barrier_open_descendants
BEGIN SELECT RAISE(ABORT,'barrier descendant routing is immutable'); END;
CREATE TRIGGER barrier_open_descendants_no_live_delete BEFORE DELETE ON barrier_open_descendants
WHEN EXISTS(SELECT 1 FROM barrier_current_status WHERE barrier_id=OLD.barrier_id AND revoked_seq IS NULL)
BEGIN SELECT RAISE(ABORT,'applicable descendant cannot be removed'); END;
CREATE TRIGGER barrier_open_descendants_on_ancestor AFTER INSERT ON barrier_ancestors
BEGIN INSERT INTO barrier_open_descendants VALUES(NEW.ancestor_id,NEW.barrier_id); END;
CREATE TRIGGER barrier_open_descendants_on_pending_revocation AFTER UPDATE OF revoked_seq ON barrier_revisions
WHEN NEW.revoked_seq IS NOT NULL
BEGIN DELETE FROM barrier_open_descendants WHERE barrier_id=NEW.barrier_id; END;
CREATE TRIGGER barrier_open_descendants_on_release_revocation AFTER INSERT ON barrier_release_revocations
BEGIN DELETE FROM barrier_open_descendants WHERE barrier_id=NEW.barrier_id; END;

CREATE TRIGGER barrier_ancestor_invalidations_valid_insert BEFORE INSERT ON barrier_ancestor_invalidations
WHEN NOT EXISTS(SELECT 1 FROM barrier_ancestors a
    JOIN barrier_current_status p ON p.barrier_id=a.ancestor_id AND p.revoked_seq=NEW.revocation_sequence
    JOIN barrier_current_status b ON b.barrier_id=a.barrier_id AND b.revoked_seq IS NULL
    JOIN events e ON e.sequence=NEW.sequence AND e.entity=a.barrier_id AND e.kind='barrier.ancestor_invalidated'
    WHERE a.barrier_id=NEW.barrier_id AND a.ancestor_id=NEW.ancestor_id
      AND json_extract(e.payload,'$.ancestor_id')=NEW.ancestor_id
      AND json_extract(e.payload,'$.revocation_sequence')=NEW.revocation_sequence)
BEGIN SELECT RAISE(ABORT,'exact ancestor revocation required'); END;
CREATE TRIGGER barrier_ancestor_invalidations_no_update BEFORE UPDATE ON barrier_ancestor_invalidations
BEGIN SELECT RAISE(ABORT,'barrier ancestor invalidation is immutable'); END;
CREATE TRIGGER barrier_ancestor_invalidations_no_delete BEFORE DELETE ON barrier_ancestor_invalidations
BEGIN SELECT RAISE(ABORT,'barrier ancestor invalidation is immutable'); END;
CREATE TRIGGER barrier_ancestor_invalidation_on_event AFTER INSERT ON events
WHEN NEW.kind='barrier.ancestor_invalidated'
BEGIN
    INSERT INTO barrier_ancestor_invalidations VALUES(NEW.entity,
        json_extract(NEW.payload,'$.ancestor_id'),json_extract(NEW.payload,'$.revocation_sequence'),NEW.sequence);
END;
CREATE TRIGGER barrier_ancestor_invalidation_apply AFTER INSERT ON barrier_ancestor_invalidations
BEGIN
    DELETE FROM barrier_open_members WHERE barrier_id=NEW.barrier_id;
    DELETE FROM barrier_open_memory_records WHERE barrier_id=NEW.barrier_id;
    DELETE FROM barrier_open_memory_unknown WHERE barrier_id=NEW.barrier_id;
    DELETE FROM barrier_open_descendants WHERE barrier_id=NEW.barrier_id;
    INSERT INTO events(kind,entity,revision,payload_version,payload)
    SELECT 'attempt.barrier_invalidated',attempt_id,1,1,
        json_object('barrier_id',NEW.barrier_id,'revocation_sequence',NEW.sequence)
    FROM (WITH affected AS MATERIALIZED (
        SELECT attempt_id FROM barrier_live_consumers WHERE barrier_id=NEW.barrier_id ORDER BY attempt_id LIMIT 1000
    ) SELECT attempt_id FROM affected);
END;
-- Emit the complete transitive set flatly. No recursive trigger setting or
-- ancestor ordering is needed, and deleting routing rows cannot skip siblings.
CREATE TRIGGER barrier_invalidate_descendants AFTER INSERT ON barrier_release_revocations
BEGIN
    INSERT INTO events(kind,entity,revision,payload_version,payload)
    SELECT 'barrier.ancestor_invalidated',barrier_id,1,1,
        json_object('ancestor_id',NEW.barrier_id,'revocation_sequence',NEW.sequence)
    FROM (WITH affected AS MATERIALIZED (
        SELECT barrier_id FROM barrier_open_descendants WHERE ancestor_id=NEW.barrier_id ORDER BY barrier_id LIMIT 1000
    ) SELECT barrier_id FROM affected);
END;

-- Durable stop routing is distinct from observing termination. The controller
-- converts these obligations through the existing cancellation service, which
-- alone can prove a never-claimed reservation had no external worker.
CREATE TABLE barrier_pending_stops (
    attempt_id TEXT PRIMARY KEY REFERENCES attempt_barrier_invalidations(attempt_id)
) STRICT;
CREATE TRIGGER barrier_pending_stops_valid_insert BEFORE INSERT ON barrier_pending_stops
WHEN NOT EXISTS(SELECT 1 FROM attempt_barrier_invalidations i JOIN attempts a ON a.id=i.attempt_id
    WHERE i.attempt_id=NEW.attempt_id AND a.termination_observed=0)
 OR EXISTS(SELECT 1 FROM attempt_cancellations WHERE attempt_id=NEW.attempt_id)
BEGIN SELECT RAISE(ABORT,'live invalidated consumer without cancellation required'); END;
CREATE TRIGGER barrier_pending_stops_no_update BEFORE UPDATE ON barrier_pending_stops
BEGIN SELECT RAISE(ABORT,'barrier stop identity is immutable'); END;
CREATE TRIGGER barrier_pending_stops_no_live_delete BEFORE DELETE ON barrier_pending_stops
WHEN EXISTS(SELECT 1 FROM attempts WHERE id=OLD.attempt_id AND termination_observed=0)
 AND NOT EXISTS(SELECT 1 FROM attempt_cancellations WHERE attempt_id=OLD.attempt_id)
BEGIN SELECT RAISE(ABORT,'unfulfilled barrier stop cannot be removed'); END;
CREATE TRIGGER barrier_pending_stops_on_invalidation AFTER INSERT ON attempt_barrier_invalidations
BEGIN
    INSERT INTO barrier_pending_stops SELECT NEW.attempt_id
    WHERE EXISTS(SELECT 1 FROM attempts WHERE id=NEW.attempt_id AND termination_observed=0)
      AND NOT EXISTS(SELECT 1 FROM attempt_cancellations WHERE attempt_id=NEW.attempt_id);
END;
CREATE TRIGGER barrier_pending_stops_on_cancellation AFTER INSERT ON attempt_cancellations
BEGIN DELETE FROM barrier_pending_stops WHERE attempt_id=NEW.attempt_id; END;
CREATE TRIGGER barrier_pending_stops_on_termination AFTER UPDATE OF termination_observed ON attempts
WHEN NEW.termination_observed=1
BEGIN DELETE FROM barrier_pending_stops WHERE attempt_id=NEW.id; END;
INSERT INTO barrier_pending_stops
SELECT i.attempt_id FROM attempt_barrier_invalidations i JOIN attempts a ON a.id=i.attempt_id
WHERE a.termination_observed=0 AND NOT EXISTS(SELECT 1 FROM attempt_cancellations c WHERE c.attempt_id=i.attempt_id);
CREATE TABLE barrier_stop_cursor (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    attempt_id TEXT NOT NULL
) STRICT;

-- Exact target alias probes retain collision checks without decoding cold bindings.
CREATE INDEX runtime_bindings_local_pane ON runtime_bindings(
    json_extract(payload,'$.identity.socket'),json_extract(payload,'$.identity.pane_id'),id
) WHERE json_extract(payload,'$.identity.machine')='';
CREATE INDEX launch_targets_by_pane ON events(
    json_extract(payload,'$.route.socket'),json_extract(payload,'$.route.pane_id'),entity
) WHERE kind='runtime.launch_target';

-- Ownership generations stay cheap even when a binding has many past launches.
CREATE INDEX ownership_event_revisions ON events(entity,revision)
WHERE kind IN ('runtime.adopted','runtime.launched');

-- Pane checks include remote candidates and malformed routing identities.
CREATE INDEX runtime_bindings_by_pane ON runtime_bindings(json_extract(payload,'$.identity.pane_id'),id);
CREATE INDEX runtime_bindings_unknown_pane ON runtime_bindings(id)
WHERE json_type(payload,'$.identity.pane_id') IS NOT 'text';

-- Retained resource references follow the same lifecycle rule as the audited
-- inventory. Termination alone never removes an unacknowledged launch target;
-- workspace receipts remain references regardless of worker termination.
CREATE TABLE retained_launch_resources (
    sequence INTEGER PRIMARY KEY,
    operation_id TEXT NOT NULL,
    pane TEXT NOT NULL,
    unknown_pane INTEGER NOT NULL CHECK(unknown_pane IN (0,1))
) STRICT;
CREATE INDEX retained_launch_resources_operation ON retained_launch_resources(operation_id,sequence);
CREATE INDEX retained_launch_resources_pane ON retained_launch_resources(pane,sequence);
CREATE INDEX retained_launch_resources_unknown ON retained_launch_resources(sequence) WHERE unknown_pane=1;
CREATE VIEW retained_launch_resource_source AS
SELECT e.sequence,e.entity AS operation_id,
    CASE WHEN json_type(e.payload,'$.route.pane_id')='text' THEN json_extract(e.payload,'$.route.pane_id') ELSE '' END AS pane,
    CASE WHEN json_type(e.payload,'$.route.pane_id')='text' AND json_extract(e.payload,'$.route.pane_id')<>'' THEN 0 ELSE 1 END AS unknown_pane
FROM events e INDEXED BY events_by_entity_kind
LEFT JOIN attempt_inputs i ON i.operation_id=e.entity
LEFT JOIN attempts a ON a.id=i.attempt_id
WHERE e.kind='runtime.launch_workspace' OR (e.kind='runtime.launch_target'
    AND (a.id IS NULL OR a.termination_observed=0 OR NOT EXISTS(
        SELECT 1 FROM events started WHERE started.entity=e.entity AND started.kind='runtime.launch_started')));
INSERT INTO retained_launch_resources SELECT * FROM retained_launch_resource_source;
CREATE TRIGGER retained_launch_resources_events_insert AFTER INSERT ON events
WHEN NEW.kind IN ('runtime.launch_target','runtime.launch_workspace','runtime.launch_started')
BEGIN
    DELETE FROM retained_launch_resources WHERE operation_id IN (NEW.entity);
    INSERT INTO retained_launch_resources SELECT * FROM retained_launch_resource_source WHERE operation_id IN (NEW.entity);
END;
CREATE TRIGGER retained_launch_resources_events_update AFTER UPDATE OF entity,kind,payload,sequence ON events
WHEN OLD.kind IN ('runtime.launch_target','runtime.launch_workspace','runtime.launch_started') OR NEW.kind IN ('runtime.launch_target','runtime.launch_workspace','runtime.launch_started')
BEGIN
    DELETE FROM retained_launch_resources WHERE operation_id IN (OLD.entity,NEW.entity);
    INSERT INTO retained_launch_resources SELECT * FROM retained_launch_resource_source WHERE operation_id IN (OLD.entity,NEW.entity);
END;
CREATE TRIGGER retained_launch_resources_events_delete AFTER DELETE ON events
WHEN OLD.kind IN ('runtime.launch_target','runtime.launch_workspace','runtime.launch_started')
BEGIN
    DELETE FROM retained_launch_resources WHERE operation_id IN (OLD.entity);
    INSERT INTO retained_launch_resources SELECT * FROM retained_launch_resource_source WHERE operation_id IN (OLD.entity);
END;
CREATE TRIGGER retained_launch_resources_attempt_inputs_insert AFTER INSERT ON attempt_inputs
BEGIN
    DELETE FROM retained_launch_resources WHERE operation_id IN (NEW.operation_id);
    INSERT INTO retained_launch_resources SELECT * FROM retained_launch_resource_source WHERE operation_id IN (NEW.operation_id);
END;
CREATE TRIGGER retained_launch_resources_attempt_inputs_update AFTER UPDATE ON attempt_inputs
BEGIN
    DELETE FROM retained_launch_resources WHERE operation_id IN (OLD.operation_id,NEW.operation_id);
    INSERT INTO retained_launch_resources SELECT * FROM retained_launch_resource_source WHERE operation_id IN (OLD.operation_id,NEW.operation_id);
END;
CREATE TRIGGER retained_launch_resources_attempt_inputs_delete AFTER DELETE ON attempt_inputs
BEGIN
    DELETE FROM retained_launch_resources WHERE operation_id IN (OLD.operation_id);
    INSERT INTO retained_launch_resources SELECT * FROM retained_launch_resource_source WHERE operation_id IN (OLD.operation_id);
END;
CREATE TRIGGER retained_launch_resources_attempts_insert AFTER INSERT ON attempts
BEGIN
    DELETE FROM retained_launch_resources WHERE operation_id IN (SELECT operation_id FROM attempt_inputs WHERE attempt_id IN (NEW.id));
    INSERT INTO retained_launch_resources SELECT * FROM retained_launch_resource_source WHERE operation_id IN (SELECT operation_id FROM attempt_inputs WHERE attempt_id IN (NEW.id));
END;
CREATE TRIGGER retained_launch_resources_attempts_update AFTER UPDATE OF id,termination_observed ON attempts
BEGIN
    DELETE FROM retained_launch_resources WHERE operation_id IN (SELECT operation_id FROM attempt_inputs WHERE attempt_id IN (OLD.id,NEW.id));
    INSERT INTO retained_launch_resources SELECT * FROM retained_launch_resource_source WHERE operation_id IN (SELECT operation_id FROM attempt_inputs WHERE attempt_id IN (OLD.id,NEW.id));
END;
CREATE TRIGGER retained_launch_resources_attempts_delete AFTER DELETE ON attempts
BEGIN
    DELETE FROM retained_launch_resources WHERE operation_id IN (SELECT operation_id FROM attempt_inputs WHERE attempt_id IN (OLD.id));
    INSERT INTO retained_launch_resources SELECT * FROM retained_launch_resource_source WHERE operation_id IN (SELECT operation_id FROM attempt_inputs WHERE attempt_id IN (OLD.id));
END;

-- Missing identity references remain a hard refusal, without rescanning all
-- retired observations/ownership or imported source rows on each pane check.
CREATE TABLE identity_reference_gaps (
    kind TEXT NOT NULL CHECK(kind IN ('observation','ownership','imported')),
    reference_key TEXT NOT NULL,
    PRIMARY KEY(kind,reference_key)
) STRICT;
INSERT INTO identity_reference_gaps SELECT 'observation',r.binding_id FROM runtime_observations r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL;
INSERT INTO identity_reference_gaps SELECT 'ownership',r.binding_id FROM runtime_ownership r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL;
INSERT INTO identity_reference_gaps SELECT 'imported',s.path FROM legacy_sources s LEFT JOIN runtime_bindings b ON b.source_path=s.path WHERE (s.kind='thread' OR (s.path='.state/coordinator.json' AND s.kind='runtime')) AND b.id IS NULL;
CREATE TRIGGER identity_reference_gaps_runtime_observations_insert AFTER INSERT ON runtime_observations
BEGIN
    DELETE FROM identity_reference_gaps WHERE kind='observation' AND reference_key IN (NEW.binding_id);
    INSERT INTO identity_reference_gaps SELECT 'observation',r.binding_id FROM runtime_observations r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL AND r.binding_id IN (NEW.binding_id);
END;
CREATE TRIGGER identity_reference_gaps_runtime_observations_update AFTER UPDATE OF binding_id ON runtime_observations
BEGIN
    DELETE FROM identity_reference_gaps WHERE kind='observation' AND reference_key IN (OLD.binding_id,NEW.binding_id);
    INSERT INTO identity_reference_gaps SELECT 'observation',r.binding_id FROM runtime_observations r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL AND r.binding_id IN (OLD.binding_id,NEW.binding_id);
END;
CREATE TRIGGER identity_reference_gaps_runtime_observations_delete AFTER DELETE ON runtime_observations
BEGIN
    DELETE FROM identity_reference_gaps WHERE kind='observation' AND reference_key IN (OLD.binding_id);
    INSERT INTO identity_reference_gaps SELECT 'observation',r.binding_id FROM runtime_observations r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL AND r.binding_id IN (OLD.binding_id);
END;
CREATE TRIGGER identity_reference_gaps_runtime_ownership_insert AFTER INSERT ON runtime_ownership
BEGIN
    DELETE FROM identity_reference_gaps WHERE kind='ownership' AND reference_key IN (NEW.binding_id);
    INSERT INTO identity_reference_gaps SELECT 'ownership',r.binding_id FROM runtime_ownership r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL AND r.binding_id IN (NEW.binding_id);
END;
CREATE TRIGGER identity_reference_gaps_runtime_ownership_update AFTER UPDATE OF binding_id ON runtime_ownership
BEGIN
    DELETE FROM identity_reference_gaps WHERE kind='ownership' AND reference_key IN (OLD.binding_id,NEW.binding_id);
    INSERT INTO identity_reference_gaps SELECT 'ownership',r.binding_id FROM runtime_ownership r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL AND r.binding_id IN (OLD.binding_id,NEW.binding_id);
END;
CREATE TRIGGER identity_reference_gaps_runtime_ownership_delete AFTER DELETE ON runtime_ownership
BEGIN
    DELETE FROM identity_reference_gaps WHERE kind='ownership' AND reference_key IN (OLD.binding_id);
    INSERT INTO identity_reference_gaps SELECT 'ownership',r.binding_id FROM runtime_ownership r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL AND r.binding_id IN (OLD.binding_id);
END;
CREATE TRIGGER identity_reference_gaps_runtime_bindings_insert AFTER INSERT ON runtime_bindings
BEGIN
    DELETE FROM identity_reference_gaps WHERE kind='observation' AND reference_key IN (NEW.id);
    INSERT INTO identity_reference_gaps SELECT 'observation',r.binding_id FROM runtime_observations r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL AND r.binding_id IN (NEW.id);
    DELETE FROM identity_reference_gaps WHERE kind='ownership' AND reference_key IN (NEW.id);
    INSERT INTO identity_reference_gaps SELECT 'ownership',r.binding_id FROM runtime_ownership r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL AND r.binding_id IN (NEW.id);
    DELETE FROM identity_reference_gaps WHERE kind='imported' AND reference_key IN (NEW.source_path);
    INSERT INTO identity_reference_gaps SELECT 'imported',s.path FROM legacy_sources s LEFT JOIN runtime_bindings b ON b.source_path=s.path WHERE (s.kind='thread' OR (s.path='.state/coordinator.json' AND s.kind='runtime')) AND b.id IS NULL AND s.path IN (NEW.source_path);
END;
CREATE TRIGGER identity_reference_gaps_runtime_bindings_update AFTER UPDATE OF id,source_path ON runtime_bindings
BEGIN
    DELETE FROM identity_reference_gaps WHERE kind='observation' AND reference_key IN (OLD.id,NEW.id);
    INSERT INTO identity_reference_gaps SELECT 'observation',r.binding_id FROM runtime_observations r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL AND r.binding_id IN (OLD.id,NEW.id);
    DELETE FROM identity_reference_gaps WHERE kind='ownership' AND reference_key IN (OLD.id,NEW.id);
    INSERT INTO identity_reference_gaps SELECT 'ownership',r.binding_id FROM runtime_ownership r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL AND r.binding_id IN (OLD.id,NEW.id);
    DELETE FROM identity_reference_gaps WHERE kind='imported' AND reference_key IN (OLD.source_path,NEW.source_path);
    INSERT INTO identity_reference_gaps SELECT 'imported',s.path FROM legacy_sources s LEFT JOIN runtime_bindings b ON b.source_path=s.path WHERE (s.kind='thread' OR (s.path='.state/coordinator.json' AND s.kind='runtime')) AND b.id IS NULL AND s.path IN (OLD.source_path,NEW.source_path);
END;
CREATE TRIGGER identity_reference_gaps_runtime_bindings_delete AFTER DELETE ON runtime_bindings
BEGIN
    DELETE FROM identity_reference_gaps WHERE kind='observation' AND reference_key IN (OLD.id);
    INSERT INTO identity_reference_gaps SELECT 'observation',r.binding_id FROM runtime_observations r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL AND r.binding_id IN (OLD.id);
    DELETE FROM identity_reference_gaps WHERE kind='ownership' AND reference_key IN (OLD.id);
    INSERT INTO identity_reference_gaps SELECT 'ownership',r.binding_id FROM runtime_ownership r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL AND r.binding_id IN (OLD.id);
    DELETE FROM identity_reference_gaps WHERE kind='imported' AND reference_key IN (OLD.source_path);
    INSERT INTO identity_reference_gaps SELECT 'imported',s.path FROM legacy_sources s LEFT JOIN runtime_bindings b ON b.source_path=s.path WHERE (s.kind='thread' OR (s.path='.state/coordinator.json' AND s.kind='runtime')) AND b.id IS NULL AND s.path IN (OLD.source_path);
END;
CREATE TRIGGER identity_reference_gaps_legacy_sources_insert AFTER INSERT ON legacy_sources
BEGIN
    DELETE FROM identity_reference_gaps WHERE kind='imported' AND reference_key IN (NEW.path);
    INSERT INTO identity_reference_gaps SELECT 'imported',s.path FROM legacy_sources s LEFT JOIN runtime_bindings b ON b.source_path=s.path WHERE (s.kind='thread' OR (s.path='.state/coordinator.json' AND s.kind='runtime')) AND b.id IS NULL AND s.path IN (NEW.path);
END;
CREATE TRIGGER identity_reference_gaps_legacy_sources_update AFTER UPDATE OF path,kind ON legacy_sources
BEGIN
    DELETE FROM identity_reference_gaps WHERE kind='imported' AND reference_key IN (OLD.path,NEW.path);
    INSERT INTO identity_reference_gaps SELECT 'imported',s.path FROM legacy_sources s LEFT JOIN runtime_bindings b ON b.source_path=s.path WHERE (s.kind='thread' OR (s.path='.state/coordinator.json' AND s.kind='runtime')) AND b.id IS NULL AND s.path IN (OLD.path,NEW.path);
END;
CREATE TRIGGER identity_reference_gaps_legacy_sources_delete AFTER DELETE ON legacy_sources
BEGIN
    DELETE FROM identity_reference_gaps WHERE kind='imported' AND reference_key IN (OLD.path);
    INSERT INTO identity_reference_gaps SELECT 'imported',s.path FROM legacy_sources s LEFT JOIN runtime_bindings b ON b.source_path=s.path WHERE (s.kind='thread' OR (s.path='.state/coordinator.json' AND s.kind='runtime')) AND b.id IS NULL AND s.path IN (OLD.path);
END;

CREATE INDEX runtime_bindings_local_worktrees ON runtime_bindings(id)
WHERE json_extract(payload,'$.identity.machine')='' AND json_extract(payload,'$.identity.worktree_path')<>'';
CREATE INDEX runtime_bindings_unknown_worktrees ON runtime_bindings(id)
WHERE json_type(payload,'$.identity.machine') IS NOT 'text' OR json_type(payload,'$.identity.worktree_path') IS NOT 'text';

-- A terminal advisory subscription can have exactly one durable successor.
CREATE TABLE wait_rearms (
    predecessor TEXT PRIMARY KEY REFERENCES wait_conditions(wait_id),
    successor TEXT NOT NULL UNIQUE REFERENCES wait_conditions(wait_id),
    CHECK(predecessor != successor)
) STRICT;
CREATE TRIGGER wait_rearms_no_update BEFORE UPDATE ON wait_rearms
BEGIN SELECT RAISE(ABORT,'wait rearm is immutable'); END;
CREATE TRIGGER wait_rearms_no_delete BEFORE DELETE ON wait_rearms
BEGIN SELECT RAISE(ABORT,'wait rearm is immutable'); END;

-- Typed decision references may precede authenticated approval installation.
CREATE TABLE wait_triggers (
    wait_id TEXT PRIMARY KEY REFERENCES wait_conditions(wait_id),
    kind TEXT NOT NULL CHECK(kind IN ('approval_decision','attempt_capacity_released','owned_runtime_recovered')),
    reference_id TEXT NOT NULL CHECK(length(reference_id) BETWEEN 1 AND 512),
    reference_revision INTEGER NOT NULL CHECK(reference_revision>0),
    reference_generation INTEGER,
    CHECK((kind='owned_runtime_recovered' AND reference_generation IS NOT NULL AND reference_generation>0) OR (kind!='owned_runtime_recovered' AND reference_generation IS NULL)),
    CHECK(kind!='approval_decision' OR (length(reference_id)=73 AND substr(reference_id,1,9)='approval-'))
) STRICT;
CREATE TRIGGER wait_triggers_no_update BEFORE UPDATE ON wait_triggers
BEGIN SELECT RAISE(ABORT,'wait trigger is immutable'); END;
CREATE TRIGGER wait_triggers_no_delete BEFORE DELETE ON wait_triggers
BEGIN SELECT RAISE(ABORT,'wait trigger is immutable'); END;

-- Find the retained observation's publication without scanning prior samples.
CREATE INDEX runtime_observation_versions ON events(entity,revision,json_extract(payload,'$.observed_unix_ms'),sequence)
WHERE kind='runtime.observed';

-- Automatic request creation is opt-in and separate from execution authority.
CREATE TABLE auto_replan_control (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    revision INTEGER NOT NULL CHECK(revision>0),
    enabled INTEGER NOT NULL CHECK(enabled IN (0,1))
) STRICT;
INSERT INTO auto_replan_control VALUES(1,1,0);
CREATE TABLE replan_feedback_links (
    feedback_id TEXT PRIMARY KEY REFERENCES feedback_items(feedback_id),
    replan_id TEXT NOT NULL REFERENCES replan_requests(replan_id)
) STRICT;
INSERT INTO replan_feedback_links SELECT feedback_id,replan_id FROM replan_requests;
CREATE TRIGGER replan_feedback_links_no_update BEFORE UPDATE ON replan_feedback_links
BEGIN SELECT RAISE(ABORT,'replan feedback link is immutable'); END;
CREATE TRIGGER replan_feedback_links_no_delete BEFORE DELETE ON replan_feedback_links
BEGIN SELECT RAISE(ABORT,'replan feedback link is immutable'); END;
CREATE TABLE replan_pending_feedback (
    feedback_id TEXT PRIMARY KEY REFERENCES feedback_items(feedback_id)
) STRICT;
INSERT INTO replan_pending_feedback SELECT feedback_id FROM feedback_items f
WHERE state IN ('open','claimed') AND NOT EXISTS(SELECT 1 FROM replan_feedback_links l WHERE l.feedback_id=f.feedback_id);
CREATE TRIGGER replan_pending_insert AFTER INSERT ON feedback_items
WHEN NEW.state IN ('open','claimed')
BEGIN INSERT INTO replan_pending_feedback VALUES(NEW.feedback_id); END;
CREATE TRIGGER replan_pending_state AFTER UPDATE OF state ON feedback_items
BEGIN
    DELETE FROM replan_pending_feedback WHERE feedback_id=NEW.feedback_id;
    INSERT INTO replan_pending_feedback SELECT NEW.feedback_id
    WHERE NEW.state IN ('open','claimed') AND NOT EXISTS(SELECT 1 FROM replan_feedback_links WHERE feedback_id=NEW.feedback_id);
END;
CREATE TRIGGER replan_pending_link AFTER INSERT ON replan_feedback_links
BEGIN DELETE FROM replan_pending_feedback WHERE feedback_id=NEW.feedback_id; END;
CREATE TABLE replan_service_cursor (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    feedback_id TEXT NOT NULL
) STRICT;

-- Retained planner context is untrusted input, not an execution grant.
CREATE TABLE planner_sessions (
    session_id TEXT PRIMARY KEY CHECK(length(session_id) BETWEEN 1 AND 128),
    input BLOB NOT NULL CHECK(length(input) BETWEEN 1 AND 262144),
    input_digest TEXT NOT NULL CHECK(length(input_digest)=64)
) STRICT;
CREATE TABLE planner_proposal_inputs (
    proposal_id TEXT PRIMARY KEY REFERENCES plan_proposals(proposal_id),
    session_id TEXT NOT NULL REFERENCES planner_sessions(session_id),
    input_digest TEXT NOT NULL CHECK(length(input_digest)=64)
) STRICT;
CREATE TRIGGER planner_sessions_no_update BEFORE UPDATE ON planner_sessions
BEGIN SELECT RAISE(ABORT,'planner session is immutable'); END;
CREATE TRIGGER planner_sessions_no_delete BEFORE DELETE ON planner_sessions
BEGIN SELECT RAISE(ABORT,'planner session is immutable'); END;
CREATE TRIGGER planner_proposal_inputs_no_update BEFORE UPDATE ON planner_proposal_inputs
BEGIN SELECT RAISE(ABORT,'planner input binding is immutable'); END;
CREATE TRIGGER planner_proposal_inputs_no_delete BEFORE DELETE ON planner_proposal_inputs
BEGIN SELECT RAISE(ABORT,'planner input binding is immutable'); END;

-- Current accepted planning intent remains separate from executable queue state.
CREATE TABLE plan_task_intents (
    task_id TEXT PRIMARY KEY CHECK(length(task_id) BETWEEN 1 AND 128),
    proposal_id TEXT NOT NULL REFERENCES plan_proposals(proposal_id),
    plan_revision INTEGER NOT NULL CHECK(plan_revision>0),
    cancellation_reason TEXT CHECK(cancellation_reason IS NULL OR length(cancellation_reason) BETWEEN 1 AND 4000),
    dependencies TEXT NOT NULL CHECK(json_valid(dependencies) AND json_type(dependencies)='array' AND json_array_length(dependencies)<=256)
) STRICT;
INSERT INTO plan_task_intents(task_id,proposal_id,plan_revision,dependencies,cancellation_reason)
SELECT task_id,proposal_id,plan_revision,dependencies,cancellation_reason FROM (
    SELECT json_extract(c.value,'$.task_id') AS task_id,p.proposal_id,p.plan_revision,
        json_extract(c.value,'$.dependencies') AS dependencies,
        json_extract(c.value,'$.cancellation_reason') AS cancellation_reason,
        row_number() OVER(PARTITION BY json_extract(c.value,'$.task_id') ORDER BY p.plan_revision DESC) AS position
    FROM plan_proposals p,json_each(CAST(p.payload AS TEXT),'$.contracts') c
) WHERE position=1;
CREATE TRIGGER plan_task_intents_accept AFTER INSERT ON plan_proposals
BEGIN
    INSERT INTO plan_task_intents(task_id,proposal_id,plan_revision,dependencies,cancellation_reason)
    SELECT json_extract(c.value,'$.task_id'),NEW.proposal_id,NEW.plan_revision,json_extract(c.value,'$.dependencies'),json_extract(c.value,'$.cancellation_reason')
    FROM json_each(CAST(NEW.payload AS TEXT),'$.contracts') c WHERE 1
    ON CONFLICT(task_id) DO UPDATE SET proposal_id=excluded.proposal_id,
        plan_revision=excluded.plan_revision,dependencies=excluded.dependencies,cancellation_reason=excluded.cancellation_reason
    WHERE excluded.plan_revision>plan_task_intents.plan_revision;
END;

CREATE TABLE plan_proposal_requests (
    request_id TEXT PRIMARY KEY CHECK(length(request_id) BETWEEN 1 AND 128),
    proposal_id TEXT NOT NULL UNIQUE REFERENCES plan_proposals(proposal_id)
) STRICT;
CREATE TRIGGER plan_proposal_requests_no_update BEFORE UPDATE ON plan_proposal_requests
BEGIN SELECT RAISE(ABORT,'plan request binding is immutable'); END;
CREATE TRIGGER plan_proposal_requests_no_delete BEFORE DELETE ON plan_proposal_requests
BEGIN SELECT RAISE(ABORT,'plan request binding is immutable'); END;

-- Version 1 attested scope/outputs; version 2 also attests post-execution
-- HEAD/index/worktree identity. Never backfill historical acceptance.
CREATE TABLE verification_contract_checks (
    result_id TEXT PRIMARY KEY REFERENCES verified_results(result_id),
    version INTEGER NOT NULL CHECK(version IN (1,2))
) STRICT;

-- Fresh native integration confirmation attests the final merged output tree.
-- No backfill: historical receipts did not necessarily perform this check.
CREATE TABLE integration_contract_checks (
    integrated_id TEXT PRIMARY KEY REFERENCES integrated_commits(integrated_id),
    version INTEGER NOT NULL CHECK(version=1)
) STRICT;

-- Filter retired attempts/revisions before loading receipt candidates. The
-- single-column attempt index preserves rowid order within each task.
CREATE INDEX verification_runs_attachment ON verification_runs(task_id,attempt_id,contract_revision,state);
CREATE INDEX attempts_latest_for_task ON attempts(task_id);
CREATE INDEX integration_operations_by_verified_result ON integration_operations(verified_result_id,operation_id);

CREATE INDEX approval_grants_by_task_class ON approval_grants(json_extract(payload,'$.scope.task'),json_extract(payload,'$.scope.class'),id);

-- Scheduler capability hints select one current identity/adapter report before
-- validating its immutable payload. Historical reports remain available to audit.
CREATE INDEX native_profiles_by_store_kind_sequence ON native_profiles(
    json(json_extract(report,'$.source_store')),
    json_extract(report,'$.preparation.profile.kind'),
    sequence DESC
);

-- Old configurations and replaced/copied store identities do not consume the
-- active admission profile inventory bound. Preserve digest ordering for grants.
CREATE INDEX native_profiles_by_store_config_digest ON native_profiles(
    json(json_extract(report,'$.source_store')),
    json_extract(report,'$.preparation.profile.config.digest'),
    profile_digest
);

-- Membership needs one valid observation per level, not DISTINCT over history.
CREATE INDEX capability_evidence_by_window ON capability_evidence(
    adapter_kind,profile_digest,level,expires_unix_ms,observed_unix_ms
);
