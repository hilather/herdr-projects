-- Local integration queue. Historical dependency rows are not rewritten:
-- landed_commit and integration_candidate stay non-executing, and this
-- migration does not write a satisfaction.
CREATE TABLE integration_targets (
    repository TEXT NOT NULL CHECK (length(repository) BETWEEN 1 AND 4096),
    ref_name TEXT NOT NULL CHECK (length(ref_name) BETWEEN 12 AND 256),
    created_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (repository),
    UNIQUE (repository, ref_name)
) STRICT;
CREATE TABLE integration_target_leases (
    repository TEXT NOT NULL,
    ref_name TEXT NOT NULL,
    operation_id TEXT UNIQUE REFERENCES operations(id),
    generation INTEGER NOT NULL CHECK (generation >= 0),
    PRIMARY KEY (repository, ref_name),
    FOREIGN KEY (repository, ref_name) REFERENCES integration_targets(repository, ref_name)
) STRICT;
CREATE TABLE integration_operations (
    operation_id TEXT PRIMARY KEY REFERENCES operations(id),
    project_store TEXT NOT NULL CHECK (length(project_store) BETWEEN 1 AND 4096),
    idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
    payload_digest TEXT NOT NULL CHECK (length(payload_digest) = 64),
    repository TEXT NOT NULL,
    ref_name TEXT NOT NULL,
    expected_old_oid TEXT NOT NULL,
    verified_result_id TEXT NOT NULL REFERENCES verified_results(result_id),
    candidate_id TEXT,
    state TEXT NOT NULL CHECK (state IN (
        'effect_pending',
        'candidate_prepared',
        'validating',
        'blocked',
        'integrated',
        'reconciliation_required',
        'discarded'
    )),
    generation INTEGER NOT NULL CHECK (generation > 0),
    object_format TEXT NOT NULL CHECK (object_format IN ('sha1', 'sha256')),
    checks_passed INTEGER NOT NULL CHECK (checks_passed IN (0, 1)),
    reason TEXT,
    created_unix_ms INTEGER NOT NULL,
    UNIQUE (project_store, idempotency_key, generation),
    FOREIGN KEY (repository, ref_name) REFERENCES integration_target_leases(repository, ref_name),
    CHECK ((object_format = 'sha1' AND length(expected_old_oid) = 40) OR (object_format = 'sha256' AND length(expected_old_oid) = 64)),
    CHECK (reason IS NULL OR (length(reason) BETWEEN 1 AND 128))
) STRICT;
CREATE TABLE integration_candidates (
    candidate_id TEXT PRIMARY KEY CHECK (length(candidate_id) = 64),
    operation_id TEXT NOT NULL UNIQUE REFERENCES integration_operations(operation_id),
    commit_oid TEXT NOT NULL,
    tree_oid TEXT NOT NULL,
    parent_base TEXT NOT NULL,
    parent_verified TEXT NOT NULL,
    strategy TEXT NOT NULL CHECK (strategy = 'ort'),
    object_format TEXT NOT NULL CHECK (object_format IN ('sha1', 'sha256')),
    state TEXT NOT NULL CHECK (state IN ('prepared', 'discarded', 'published')),
    created_unix_ms INTEGER NOT NULL,
    CHECK (
        (object_format = 'sha1' AND length(commit_oid) = 40 AND length(tree_oid) = 40 AND length(parent_base) = 40 AND length(parent_verified) = 40)
        OR
        (object_format = 'sha256' AND length(commit_oid) = 64 AND length(tree_oid) = 64 AND length(parent_base) = 64 AND length(parent_verified) = 64)
    )
) STRICT;
CREATE TABLE integrated_commits (
    integrated_id TEXT PRIMARY KEY CHECK (length(integrated_id) = 64),
    candidate_id TEXT NOT NULL UNIQUE REFERENCES integration_candidates(candidate_id),
    operation_id TEXT NOT NULL UNIQUE REFERENCES integration_operations(operation_id),
    repository TEXT NOT NULL CHECK (length(repository) BETWEEN 1 AND 4096),
    ref_name TEXT NOT NULL CHECK (length(ref_name) BETWEEN 12 AND 256),
    commit_oid TEXT NOT NULL,
    tree_oid TEXT NOT NULL,
    expected_old_oid TEXT NOT NULL,
    object_format TEXT NOT NULL CHECK (object_format IN ('sha1', 'sha256')),
    created_unix_ms INTEGER NOT NULL,
    CHECK (
        (object_format = 'sha1' AND length(commit_oid) = 40 AND length(tree_oid) = 40 AND length(expected_old_oid) = 40)
        OR
        (object_format = 'sha256' AND length(commit_oid) = 64 AND length(tree_oid) = 64 AND length(expected_old_oid) = 64)
    )
) STRICT;
CREATE INDEX integration_operations_by_ref ON integration_operations(repository, ref_name, state);
CREATE TRIGGER integration_targets_no_retarget BEFORE UPDATE OF ref_name ON integration_targets
BEGIN SELECT RAISE(ABORT, 'integration ref cannot be retargeted'); END;
CREATE TRIGGER integration_operations_no_relabel BEFORE UPDATE OF expected_old_oid, verified_result_id, repository, ref_name, object_format, idempotency_key, payload_digest, generation ON integration_operations
BEGIN SELECT RAISE(ABORT, 'integration operation identity is immutable'); END;
CREATE TRIGGER integration_candidates_no_relabel BEFORE UPDATE OF commit_oid, tree_oid, parent_base, parent_verified, strategy, object_format ON integration_candidates
BEGIN SELECT RAISE(ABORT, 'integration candidate identity is immutable'); END;
CREATE TRIGGER integrated_commits_no_update BEFORE UPDATE ON integrated_commits
BEGIN SELECT RAISE(ABORT, 'integrated commit is immutable'); END;
CREATE TRIGGER integrated_commits_no_delete BEFORE DELETE ON integrated_commits
BEGIN SELECT RAISE(ABORT, 'integrated commit is immutable'); END;
UPDATE store_meta SET schema_version = 28;
PRAGMA user_version = 28;
