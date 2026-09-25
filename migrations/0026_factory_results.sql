-- Owner-signed task contracts and untrusted worker submissions.
-- No satisfaction, verifier receipt, or admission flag. Historical task rows are not rewritten.
CREATE TABLE task_contracts (
    task_id TEXT NOT NULL REFERENCES tasks(id),
    contract_revision INTEGER NOT NULL CHECK (contract_revision > 0),
    plan_revision INTEGER CHECK (plan_revision IS NULL OR plan_revision > 0),
    project_store TEXT NOT NULL CHECK (length(project_store) BETWEEN 1 AND 4096),
    expected_head INTEGER NOT NULL CHECK (expected_head >= 0),
    repository TEXT NOT NULL CHECK (length(repository) BETWEEN 1 AND 4096),
    base_oid TEXT NOT NULL,
    object_format TEXT NOT NULL CHECK (object_format IN ('sha1', 'sha256')),
    memory_snapshot_id TEXT CHECK (memory_snapshot_id IS NULL OR length(memory_snapshot_id) BETWEEN 1 AND 128),
    route TEXT NOT NULL CHECK (route IN ('verify_then_integrate', 'verify_only')),
    raw_bytes BLOB NOT NULL CHECK (length(raw_bytes) BETWEEN 1 AND 65536),
    raw_digest TEXT NOT NULL CHECK (length(raw_digest) = 64),
    installed_seq INTEGER NOT NULL REFERENCES events(sequence),
    PRIMARY KEY (task_id, contract_revision),
    CHECK ((object_format = 'sha1' AND length(base_oid) = 40) OR (object_format = 'sha256' AND length(base_oid) = 64))
) STRICT;
CREATE TABLE acceptance_policies (
    task_id TEXT NOT NULL,
    contract_revision INTEGER NOT NULL,
    policy_id TEXT NOT NULL CHECK (length(policy_id) BETWEEN 1 AND 128),
    body TEXT NOT NULL CHECK (length(body) BETWEEN 1 AND 4000),
    PRIMARY KEY (task_id, contract_revision, policy_id),
    FOREIGN KEY (task_id, contract_revision) REFERENCES task_contracts(task_id, contract_revision)
) STRICT;
CREATE TABLE result_submissions (
    submission_id TEXT PRIMARY KEY CHECK (length(submission_id) = 64),
    project_store TEXT NOT NULL CHECK (length(project_store) BETWEEN 1 AND 4096),
    idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
    payload_digest TEXT NOT NULL CHECK (length(payload_digest) = 64),
    payload TEXT NOT NULL CHECK (json_valid(payload) AND length(payload) <= 262144),
    task_id TEXT NOT NULL,
    contract_revision INTEGER NOT NULL CHECK (contract_revision > 0),
    contract_digest TEXT NOT NULL CHECK (length(contract_digest) = 64),
    attempt_id TEXT NOT NULL,
    repository TEXT NOT NULL CHECK (length(repository) BETWEEN 1 AND 4096),
    base_oid TEXT NOT NULL,
    candidate_oid TEXT NOT NULL,
    object_format TEXT NOT NULL CHECK (object_format IN ('sha1', 'sha256')),
    memory_snapshot_id TEXT CHECK (memory_snapshot_id IS NULL OR length(memory_snapshot_id) BETWEEN 1 AND 128),
    artifact_manifest TEXT NOT NULL CHECK (json_valid(artifact_manifest) AND length(artifact_manifest) <= 65536),
    claimed_checks TEXT NOT NULL CHECK (json_valid(claimed_checks) AND length(claimed_checks) <= 65536),
    created_unix_ms INTEGER NOT NULL,
    UNIQUE (project_store, idempotency_key),
    FOREIGN KEY (task_id, contract_revision) REFERENCES task_contracts(task_id, contract_revision),
    FOREIGN KEY (attempt_id, task_id) REFERENCES attempts(id, task_id),
    CHECK ((object_format = 'sha1' AND length(base_oid) = 40 AND length(candidate_oid) = 40) OR (object_format = 'sha256' AND length(base_oid) = 64 AND length(candidate_oid) = 64))
) STRICT;
CREATE TABLE result_objects (
    submission_id TEXT NOT NULL REFERENCES result_submissions(submission_id),
    oid TEXT NOT NULL,
    object_format TEXT NOT NULL CHECK (object_format IN ('sha1', 'sha256')),
    relative_path TEXT NOT NULL CHECK (length(relative_path) BETWEEN 1 AND 512),
    byte_sha256 TEXT NOT NULL CHECK (length(byte_sha256) = 64),
    size INTEGER NOT NULL CHECK (size >= 0 AND size <= 16777216),
    PRIMARY KEY (submission_id, oid),
    CHECK ((object_format = 'sha1' AND length(oid) = 40) OR (object_format = 'sha256' AND length(oid) = 64))
) STRICT;
CREATE INDEX result_submissions_by_task ON result_submissions(task_id, contract_revision);
CREATE TRIGGER task_contracts_no_update BEFORE UPDATE ON task_contracts
BEGIN SELECT RAISE(ABORT, 'task contract is immutable'); END;
CREATE TRIGGER task_contracts_no_delete BEFORE DELETE ON task_contracts
BEGIN SELECT RAISE(ABORT, 'task contract is immutable'); END;
CREATE TRIGGER acceptance_policies_no_update BEFORE UPDATE ON acceptance_policies
BEGIN SELECT RAISE(ABORT, 'acceptance policy is immutable'); END;
CREATE TRIGGER acceptance_policies_no_delete BEFORE DELETE ON acceptance_policies
BEGIN SELECT RAISE(ABORT, 'acceptance policy is immutable'); END;
CREATE TRIGGER result_submissions_no_update BEFORE UPDATE ON result_submissions
BEGIN SELECT RAISE(ABORT, 'result submission is immutable'); END;
CREATE TRIGGER result_submissions_no_delete BEFORE DELETE ON result_submissions
BEGIN SELECT RAISE(ABORT, 'result submission is immutable'); END;
CREATE TRIGGER result_objects_no_update BEFORE UPDATE ON result_objects
BEGIN SELECT RAISE(ABORT, 'result object is immutable'); END;
CREATE TRIGGER result_objects_no_delete BEFORE DELETE ON result_objects
BEGIN SELECT RAISE(ABORT, 'result object is immutable'); END;
-- The denial CHECK is part of this unshipped schema. Drop the abort triggers
-- first so foreign_keys=ON can remove the old table, then copy every row.
DROP TRIGGER IF EXISTS authority_denials_no_update;
DROP TRIGGER IF EXISTS authority_denials_no_delete;
CREATE TABLE authority_denials_v26 (
    id TEXT PRIMARY KEY NOT NULL,
    unix_ms INTEGER NOT NULL,
    class TEXT NOT NULL CHECK (class IN ('approval','budget','routine-store','memory','contract')),
    command TEXT NOT NULL,
    actor_channel TEXT NOT NULL CHECK (actor_channel IN ('cli-owner','unknown-rejected')),
    reason_code TEXT NOT NULL,
    policy_digest TEXT NOT NULL CHECK (length(policy_digest) = 64),
    expected_head INTEGER,
    actual_head INTEGER
) STRICT;
INSERT INTO authority_denials_v26(id, unix_ms, class, command, actor_channel, reason_code, policy_digest, expected_head, actual_head)
SELECT id, unix_ms, class, command, actor_channel, reason_code, policy_digest, expected_head, actual_head FROM authority_denials;
DROP TABLE authority_denials;
ALTER TABLE authority_denials_v26 RENAME TO authority_denials;
CREATE TRIGGER authority_denials_no_update BEFORE UPDATE ON authority_denials BEGIN SELECT RAISE(ABORT,'authority denial is immutable'); END;
CREATE TRIGGER authority_denials_no_delete BEFORE DELETE ON authority_denials BEGIN SELECT RAISE(ABORT,'authority denial is immutable'); END;
UPDATE store_meta SET schema_version = 26;
PRAGMA user_version = 26;
