-- Satisfaction is evidence, not admission. Historical requirement text is copied
-- verbatim so landed_commit is not rewritten into integrated_commit.
CREATE TABLE task_dependencies_schema30 (
    task_id TEXT NOT NULL REFERENCES tasks(id),
    predecessor_id TEXT NOT NULL REFERENCES tasks(id),
    requirement TEXT NOT NULL CHECK(requirement IN ('verified_result','integrated_commit','integration_candidate','landed_commit')),
    PRIMARY KEY(task_id,predecessor_id),
    CHECK(task_id<>predecessor_id)
) STRICT;
INSERT INTO task_dependencies_schema30(task_id, predecessor_id, requirement)
SELECT task_id, predecessor_id, requirement FROM task_dependencies;
DROP TABLE task_dependencies;
ALTER TABLE task_dependencies_schema30 RENAME TO task_dependencies;

-- Default off. No production function in this schema writes the column.
ALTER TABLE project_control ADD COLUMN factory_admission TEXT NOT NULL DEFAULT 'off' CHECK(factory_admission IN ('off','on'));

CREATE TABLE dependency_satisfactions (
    satisfaction_id TEXT PRIMARY KEY CHECK(length(satisfaction_id)=64),
    task_id TEXT NOT NULL CHECK(length(task_id) BETWEEN 1 AND 128),
    predecessor_task TEXT NOT NULL CHECK(length(predecessor_task) BETWEEN 1 AND 128),
    requirement TEXT NOT NULL CHECK(requirement IN ('verified_result','integrated_commit')),
    state TEXT NOT NULL CHECK(state IN ('valid','invalid')),
    evidence_kind TEXT NOT NULL CHECK(evidence_kind IN ('verified_result','integrated_commit')),
    evidence_id TEXT NOT NULL CHECK(length(evidence_id)=64),
    created_unix_ms INTEGER NOT NULL,
    CHECK(task_id<>predecessor_task),
    CHECK(
        (requirement='verified_result' AND evidence_kind='verified_result')
        OR
        (requirement='integrated_commit' AND evidence_kind='integrated_commit')
    )
) STRICT;
-- Hot path for "does this predecessor still have valid evidence?"
CREATE INDEX dependency_satisfactions_by_predecessor_state ON dependency_satisfactions(predecessor_task, state);
CREATE UNIQUE INDEX dependency_satisfactions_one_valid ON dependency_satisfactions(task_id, predecessor_task, requirement) WHERE state='valid';
CREATE TRIGGER dependency_satisfactions_no_delete BEFORE DELETE ON dependency_satisfactions
BEGIN SELECT RAISE(ABORT, 'satisfaction history is immutable'); END;
CREATE TRIGGER dependency_satisfactions_no_relabel
BEFORE UPDATE OF satisfaction_id, task_id, predecessor_task, requirement, evidence_kind, evidence_id, created_unix_ms ON dependency_satisfactions
BEGIN SELECT RAISE(ABORT, 'satisfaction identity is immutable'); END;
CREATE TRIGGER dependency_satisfactions_no_revalidate BEFORE UPDATE OF state ON dependency_satisfactions
WHEN OLD.state='invalid' OR NEW.state!='invalid'
BEGIN SELECT RAISE(ABORT, 'satisfaction cannot be revalidated'); END;

-- Empty until a later signed install. This migration does not insert a policy.
CREATE TABLE factory_admission_policies (
    policy_digest TEXT PRIMARY KEY CHECK(length(policy_digest)=64),
    project_store TEXT NOT NULL CHECK(length(project_store) BETWEEN 1 AND 4096),
    raw_bytes BLOB NOT NULL CHECK(length(raw_bytes) BETWEEN 1 AND 65536),
    evidence_digest TEXT NOT NULL CHECK(length(evidence_digest)=64),
    created_unix_ms INTEGER NOT NULL CHECK(created_unix_ms>=0)
) STRICT;
CREATE TRIGGER factory_admission_policies_no_update BEFORE UPDATE ON factory_admission_policies
BEGIN SELECT RAISE(ABORT, 'admission policy is immutable'); END;
CREATE TRIGGER factory_admission_policies_no_delete BEFORE DELETE ON factory_admission_policies
BEGIN SELECT RAISE(ABORT, 'admission policy is immutable'); END;

UPDATE store_meta SET schema_version=30;
PRAGMA user_version=30;
