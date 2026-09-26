-- Isolated verification runs. A rejected setup stores no verified_results row.
-- Historical task, attempt, and dependency rows are not rewritten.
CREATE TABLE verification_runs (
    run_id TEXT PRIMARY KEY CHECK (length(run_id) = 64),
    project_store TEXT NOT NULL CHECK (length(project_store) BETWEEN 1 AND 4096),
    idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
    payload_digest TEXT NOT NULL CHECK (length(payload_digest) = 64),
    submission_id TEXT NOT NULL REFERENCES result_submissions(submission_id),
    task_id TEXT NOT NULL,
    contract_revision INTEGER NOT NULL CHECK (contract_revision > 0),
    contract_digest TEXT NOT NULL CHECK (length(contract_digest) = 64),
    attempt_id TEXT NOT NULL,
    policy_id TEXT NOT NULL CHECK (length(policy_id) BETWEEN 1 AND 128),
    policy_digest TEXT NOT NULL CHECK (length(policy_digest) = 64),
    commit_oid TEXT NOT NULL,
    tree_oid TEXT,
    object_format TEXT NOT NULL CHECK (object_format IN ('sha1', 'sha256')),
    memory_fence INTEGER NOT NULL CHECK (memory_fence >= 0),
    isolation TEXT NOT NULL CHECK (isolation = 'linux-unshare-user-pid-mount-v1'),
    argv TEXT NOT NULL CHECK (json_valid(argv) AND length(argv) BETWEEN 2 AND 65536),
    library_manifest TEXT NOT NULL CHECK (json_valid(library_manifest) AND length(library_manifest) <= 65536),
    state TEXT NOT NULL CHECK (state IN ('accepted', 'rejected')),
    reason TEXT,
    exit_status INTEGER,
    receipt_digest TEXT,
    store_device INTEGER NOT NULL,
    store_inode INTEGER NOT NULL,
    created_unix_ms INTEGER NOT NULL,
    UNIQUE (project_store, idempotency_key),
    FOREIGN KEY (task_id, contract_revision) REFERENCES task_contracts(task_id, contract_revision),
    FOREIGN KEY (attempt_id, task_id) REFERENCES attempts(id, task_id),
    FOREIGN KEY (task_id, contract_revision, policy_id) REFERENCES acceptance_policies(task_id, contract_revision, policy_id),
    CHECK ((object_format = 'sha1' AND length(commit_oid) = 40) OR (object_format = 'sha256' AND length(commit_oid) = 64)),
    CHECK (tree_oid IS NULL OR ((object_format = 'sha1' AND length(tree_oid) = 40) OR (object_format = 'sha256' AND length(tree_oid) = 64))),
    CHECK (
        (state = 'accepted' AND reason IS NULL AND receipt_digest IS NOT NULL AND length(receipt_digest) = 64 AND tree_oid IS NOT NULL AND exit_status = 0)
        OR
        (state = 'rejected' AND reason IS NOT NULL AND length(reason) BETWEEN 1 AND 128 AND receipt_digest IS NULL)
    )
) STRICT;
CREATE TABLE verified_results (
    result_id TEXT PRIMARY KEY CHECK (length(result_id) = 64),
    run_id TEXT NOT NULL UNIQUE REFERENCES verification_runs(run_id),
    submission_id TEXT NOT NULL REFERENCES result_submissions(submission_id),
    commit_oid TEXT NOT NULL,
    tree_oid TEXT NOT NULL,
    object_format TEXT NOT NULL CHECK (object_format IN ('sha1', 'sha256')),
    policy_digest TEXT NOT NULL CHECK (length(policy_digest) = 64),
    receipt_digest TEXT NOT NULL CHECK (length(receipt_digest) = 64),
    isolation TEXT NOT NULL CHECK (isolation = 'linux-unshare-user-pid-mount-v1'),
    memory_fence INTEGER NOT NULL CHECK (memory_fence >= 0),
    created_unix_ms INTEGER NOT NULL,
    CHECK ((object_format = 'sha1' AND length(commit_oid) = 40 AND length(tree_oid) = 40) OR (object_format = 'sha256' AND length(commit_oid) = 64 AND length(tree_oid) = 64))
) STRICT;
CREATE INDEX verified_results_by_commit_policy ON verified_results(commit_oid, policy_digest);
CREATE TRIGGER verification_runs_no_update BEFORE UPDATE ON verification_runs
BEGIN SELECT RAISE(ABORT, 'verification run is immutable'); END;
CREATE TRIGGER verification_runs_no_delete BEFORE DELETE ON verification_runs
BEGIN SELECT RAISE(ABORT, 'verification run is immutable'); END;
CREATE TRIGGER verified_results_no_update BEFORE UPDATE ON verified_results
BEGIN SELECT RAISE(ABORT, 'verified result is immutable'); END;
CREATE TRIGGER verified_results_no_delete BEFORE DELETE ON verified_results
BEGIN SELECT RAISE(ABORT, 'verified result is immutable'); END;
UPDATE store_meta SET schema_version = 27;
PRAGMA user_version = 27;
