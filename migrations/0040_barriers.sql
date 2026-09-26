-- Frozen wave membership. Editing a member creates a new barrier id.
-- The release token is bound to that id. Revocation does not free a live slot.
CREATE TABLE barrier_revisions (
    barrier_id TEXT PRIMARY KEY CHECK (length(barrier_id) = 64),
    required_set_generation INTEGER NOT NULL CHECK (required_set_generation >= 0),
    memory_manifest_digest TEXT NOT NULL CHECK (length(memory_manifest_digest) = 64),
    release_token TEXT NOT NULL UNIQUE CHECK (length(release_token) = 64),
    released_seq INTEGER REFERENCES events(sequence),
    revoked_seq INTEGER REFERENCES events(sequence),
    created_seq INTEGER NOT NULL REFERENCES events(sequence),
    CHECK (released_seq IS NULL OR revoked_seq IS NULL)
) STRICT;

CREATE TABLE barrier_members (
    barrier_id TEXT NOT NULL REFERENCES barrier_revisions(barrier_id),
    position INTEGER NOT NULL CHECK (position >= 0),
    task_id TEXT NOT NULL,
    contract_revision INTEGER NOT NULL CHECK (contract_revision > 0),
    attempt_id TEXT NOT NULL,
    result_id TEXT NOT NULL CHECK (length(result_id) = 64),
    verification_id TEXT NOT NULL CHECK (length(verification_id) = 64),
    integration_id TEXT CHECK (integration_id IS NULL OR length(integration_id) = 64),
    proposal_dispositions TEXT NOT NULL CHECK (json_valid(proposal_dispositions) AND length(proposal_dispositions) <= 65536),
    PRIMARY KEY (barrier_id, task_id),
    UNIQUE (barrier_id, position),
    FOREIGN KEY (task_id, contract_revision) REFERENCES task_contracts(task_id, contract_revision),
    FOREIGN KEY (attempt_id, task_id) REFERENCES attempts(id, task_id),
    FOREIGN KEY (result_id) REFERENCES verified_results(result_id),
    FOREIGN KEY (verification_id) REFERENCES verification_runs(run_id),
    FOREIGN KEY (integration_id) REFERENCES integrated_commits(integrated_id)
) STRICT;

-- A brief that arrives after revocation is recorded and cannot be accepted.
CREATE TABLE barrier_stale_briefs (
    brief_id TEXT PRIMARY KEY CHECK (length(brief_id) = 64),
    barrier_id TEXT NOT NULL REFERENCES barrier_revisions(barrier_id),
    attempt_id TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (length(payload) BETWEEN 1 AND 65536),
    recorded_seq INTEGER NOT NULL REFERENCES events(sequence),
    accepted INTEGER NOT NULL CHECK (accepted = 0)
) STRICT;

CREATE INDEX barrier_members_by_attempt ON barrier_members(attempt_id);
CREATE INDEX barrier_members_by_result ON barrier_members(result_id);
CREATE INDEX barrier_stale_briefs_by_attempt ON barrier_stale_briefs(attempt_id);

CREATE TRIGGER barrier_revisions_no_membership_update
BEFORE UPDATE ON barrier_revisions
WHEN OLD.barrier_id != NEW.barrier_id
  OR OLD.required_set_generation != NEW.required_set_generation
  OR OLD.memory_manifest_digest != NEW.memory_manifest_digest
  OR OLD.release_token != NEW.release_token
  OR OLD.created_seq != NEW.created_seq
  OR (OLD.released_seq IS NOT NULL AND OLD.released_seq != NEW.released_seq)
  OR (OLD.revoked_seq IS NOT NULL AND OLD.revoked_seq != NEW.revoked_seq)
  OR (NEW.released_seq IS NOT NULL AND NEW.revoked_seq IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'barrier revision membership is immutable'); END;

CREATE TRIGGER barrier_members_no_update
BEFORE UPDATE ON barrier_members
BEGIN SELECT RAISE(ABORT, 'barrier member is immutable'); END;
CREATE TRIGGER barrier_members_no_delete
BEFORE DELETE ON barrier_members
BEGIN SELECT RAISE(ABORT, 'barrier member is immutable'); END;
CREATE TRIGGER barrier_stale_briefs_no_update
BEFORE UPDATE ON barrier_stale_briefs
BEGIN SELECT RAISE(ABORT, 'stale brief cannot be accepted'); END;
CREATE TRIGGER barrier_stale_briefs_no_delete
BEFORE DELETE ON barrier_stale_briefs
BEGIN SELECT RAISE(ABORT, 'stale brief is immutable'); END;

UPDATE store_meta SET schema_version = 40;
PRAGMA user_version = 40;
