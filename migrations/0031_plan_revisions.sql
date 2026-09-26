-- Unsigned planner history. These rows are not task contracts and do not reserve attempts.
CREATE TABLE plan_proposals (
    proposal_id TEXT PRIMARY KEY CHECK (length(proposal_id) = 64),
    project_store TEXT NOT NULL CHECK (length(project_store) BETWEEN 1 AND 4096),
    idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
    payload_digest TEXT NOT NULL CHECK (length(payload_digest) = 64),
    payload BLOB NOT NULL CHECK (length(payload) BETWEEN 1 AND 262144),
    parent_revision INTEGER NOT NULL CHECK (parent_revision >= 0),
    plan_revision INTEGER NOT NULL CHECK (plan_revision > 0),
    created_unix_ms INTEGER NOT NULL,
    UNIQUE (project_store, idempotency_key),
    UNIQUE (plan_revision),
    CHECK (plan_revision = parent_revision + 1)
) STRICT;
CREATE TABLE plan_revisions (
    plan_revision INTEGER PRIMARY KEY CHECK (plan_revision > 0),
    parent_revision INTEGER NOT NULL CHECK (parent_revision >= 0),
    proposal_id TEXT NOT NULL REFERENCES plan_proposals(proposal_id),
    payload_digest TEXT NOT NULL CHECK (length(payload_digest) = 64),
    contract_texts TEXT NOT NULL CHECK (json_valid(contract_texts) AND length(contract_texts) BETWEEN 2 AND 262144),
    created_unix_ms INTEGER NOT NULL,
    UNIQUE (proposal_id),
    CHECK (plan_revision = parent_revision + 1)
) STRICT;
CREATE TRIGGER plan_proposals_no_update BEFORE UPDATE ON plan_proposals
BEGIN SELECT RAISE(ABORT, 'plan proposal is immutable'); END;
CREATE TRIGGER plan_proposals_no_delete BEFORE DELETE ON plan_proposals
BEGIN SELECT RAISE(ABORT, 'plan proposal is immutable'); END;
CREATE TRIGGER plan_revisions_no_update BEFORE UPDATE ON plan_revisions
BEGIN SELECT RAISE(ABORT, 'plan revision is immutable'); END;
CREATE TRIGGER plan_revisions_no_delete BEFORE DELETE ON plan_revisions
BEGIN SELECT RAISE(ABORT, 'plan revision is immutable'); END;
UPDATE store_meta SET schema_version = 31;
PRAGMA user_version = 31;
