-- Candidate groups (TM3.8, docs/telemetry/contracts-quality.md §3). Sequential
-- arms: each arm is an ordinary attempt of the group's task, reserved through
-- the existing launch path and bound to its arm in that reservation
-- transaction. The arm list is sealed when the group is written, before any arm
-- runs; a selection names one bound arm. No cost is stored here: an arm's cost
-- stays owned by its attempt. Every row is append-only.
CREATE TABLE candidate_groups (
    group_id TEXT PRIMARY KEY CHECK (length(group_id) = 71 AND substr(group_id, 1, 7) = 'sha256:'),
    task_id TEXT NOT NULL REFERENCES tasks(id),
    contract_revision INTEGER CHECK (contract_revision IS NULL OR contract_revision > 0),
    arm_count INTEGER NOT NULL CHECK (arm_count BETWEEN 2 AND 8),
    creator_principal TEXT NOT NULL CHECK (length(creator_principal) BETWEEN 1 AND 128),
    canonical_json TEXT NOT NULL CHECK (json_valid(canonical_json) AND length(canonical_json) <= 8192),
    created_unix_ms INTEGER NOT NULL,
    sealed_unix_ms INTEGER
) STRICT;
CREATE UNIQUE INDEX candidate_groups_per_revision ON candidate_groups(task_id, ifnull(contract_revision, 0));
CREATE TABLE candidate_group_arms (
    group_id TEXT NOT NULL REFERENCES candidate_groups(group_id),
    arm INTEGER NOT NULL CHECK (arm BETWEEN 1 AND 8),
    configuration_id TEXT NOT NULL REFERENCES agent_configurations(configuration_id),
    profile_digest TEXT NOT NULL CHECK (length(profile_digest) = 64),
    PRIMARY KEY (group_id, arm),
    UNIQUE (group_id, configuration_id)
) STRICT;
CREATE TABLE candidate_arm_attempts (
    group_id TEXT NOT NULL,
    arm INTEGER NOT NULL,
    attempt_id TEXT NOT NULL UNIQUE REFERENCES dispatch_decisions(attempt_id),
    bound_unix_ms INTEGER NOT NULL,
    source TEXT NOT NULL CHECK (length(source) BETWEEN 1 AND 64),
    PRIMARY KEY (group_id, arm),
    FOREIGN KEY (group_id, arm) REFERENCES candidate_group_arms(group_id, arm)
) STRICT;
CREATE TABLE candidate_selections (
    group_id TEXT PRIMARY KEY REFERENCES candidate_groups(group_id),
    outcome TEXT NOT NULL CHECK (outcome IN ('selected', 'no_selection')),
    arm INTEGER,
    attempt_id TEXT,
    submission_id TEXT REFERENCES result_submissions(submission_id),
    selector_kind TEXT NOT NULL CHECK (selector_kind IN ('operator', 'rule', 'judge')),
    selector_principal TEXT NOT NULL CHECK (length(selector_principal) BETWEEN 1 AND 128),
    reason TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 64),
    evidence TEXT NOT NULL CHECK (json_valid(evidence) AND json_array_length(evidence) <= 8),
    selected_unix_ms INTEGER NOT NULL,
    FOREIGN KEY (group_id, arm) REFERENCES candidate_arm_attempts(group_id, arm),
    CHECK ((outcome = 'selected') = (arm IS NOT NULL AND attempt_id IS NOT NULL AND submission_id IS NOT NULL)),
    CHECK (outcome = 'selected' OR (arm IS NULL AND attempt_id IS NULL AND submission_id IS NULL))
) STRICT;
-- Sealing: a group is written unsealed with all its arms in one transaction and
-- sealed before commit. Arms cannot be added to a sealed group, nor changed or
-- removed at all; the group itself changes only by being sealed once.
CREATE TRIGGER candidate_groups_seal_once BEFORE UPDATE ON candidate_groups
WHEN OLD.sealed_unix_ms IS NOT NULL OR NEW.sealed_unix_ms IS NULL
  OR NEW.group_id IS NOT OLD.group_id OR NEW.task_id IS NOT OLD.task_id OR NEW.contract_revision IS NOT OLD.contract_revision
  OR NEW.arm_count IS NOT OLD.arm_count OR NEW.creator_principal IS NOT OLD.creator_principal
  OR NEW.canonical_json IS NOT OLD.canonical_json OR NEW.created_unix_ms IS NOT OLD.created_unix_ms
  OR (SELECT count(*) FROM candidate_group_arms a WHERE a.group_id = NEW.group_id) != NEW.arm_count
BEGIN SELECT RAISE(ABORT, 'candidate group is sealed once, with every arm'); END;
CREATE TRIGGER candidate_groups_no_delete BEFORE DELETE ON candidate_groups
BEGIN SELECT RAISE(ABORT, 'candidate group is immutable'); END;
CREATE TRIGGER candidate_group_arms_sealed BEFORE INSERT ON candidate_group_arms
WHEN (SELECT sealed_unix_ms IS NOT NULL OR NEW.arm > arm_count FROM candidate_groups WHERE group_id = NEW.group_id)
BEGIN SELECT RAISE(ABORT, 'candidate group arms are fixed when sealed'); END;
CREATE TRIGGER candidate_group_arms_no_update BEFORE UPDATE ON candidate_group_arms
BEGIN SELECT RAISE(ABORT, 'candidate group arm is immutable'); END;
CREATE TRIGGER candidate_group_arms_no_delete BEFORE DELETE ON candidate_group_arms
BEGIN SELECT RAISE(ABORT, 'candidate group arm is immutable'); END;
-- An arm binds one attempt of the group's task, of the arm's configuration,
-- before the attempt has any result and before the group has a selection.
CREATE TRIGGER candidate_arm_attempts_bind BEFORE INSERT ON candidate_arm_attempts
WHEN NOT EXISTS (SELECT 1 FROM candidate_groups g JOIN candidate_group_arms a ON a.group_id = g.group_id
        JOIN dispatch_decisions d ON d.attempt_id = NEW.attempt_id
        WHERE g.group_id = NEW.group_id AND a.arm = NEW.arm AND g.sealed_unix_ms IS NOT NULL
          AND d.task_id = g.task_id AND d.contract_revision IS g.contract_revision AND d.chosen_configuration_id = a.configuration_id)
  OR EXISTS (SELECT 1 FROM candidate_selections s WHERE s.group_id = NEW.group_id)
  OR EXISTS (SELECT 1 FROM result_submissions r WHERE r.attempt_id = NEW.attempt_id)
BEGIN SELECT RAISE(ABORT, 'attempt cannot bind this candidate arm'); END;
CREATE TRIGGER candidate_arm_attempts_no_update BEFORE UPDATE ON candidate_arm_attempts
BEGIN SELECT RAISE(ABORT, 'candidate arm attempt is immutable'); END;
CREATE TRIGGER candidate_arm_attempts_no_delete BEFORE DELETE ON candidate_arm_attempts
BEGIN SELECT RAISE(ABORT, 'candidate arm attempt is immutable'); END;
-- A selection names a bound arm and a submission of that arm's attempt.
CREATE TRIGGER candidate_selections_member BEFORE INSERT ON candidate_selections
WHEN NOT EXISTS (SELECT 1 FROM candidate_groups g WHERE g.group_id = NEW.group_id AND g.sealed_unix_ms IS NOT NULL)
  OR (NEW.outcome = 'selected' AND NOT EXISTS (SELECT 1 FROM candidate_arm_attempts b JOIN result_submissions r ON r.attempt_id = b.attempt_id
        WHERE b.group_id = NEW.group_id AND b.arm = NEW.arm AND b.attempt_id = NEW.attempt_id AND r.submission_id = NEW.submission_id))
BEGIN SELECT RAISE(ABORT, 'selection must name a candidate of a group member'); END;
CREATE TRIGGER candidate_selections_no_update BEFORE UPDATE ON candidate_selections
BEGIN SELECT RAISE(ABORT, 'candidate selection is immutable'); END;
CREATE TRIGGER candidate_selections_no_delete BEFORE DELETE ON candidate_selections
BEGIN SELECT RAISE(ABORT, 'candidate selection is immutable'); END;
UPDATE store_meta SET schema_version = 53;
PRAGMA user_version = 53;
