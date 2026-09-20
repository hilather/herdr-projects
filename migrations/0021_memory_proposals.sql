CREATE TABLE memory_proposals (
    id TEXT PRIMARY KEY NOT NULL,
    payload_digest TEXT NOT NULL CHECK (length(payload_digest)=64),
    task_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    snapshot_id TEXT,
    review_state TEXT NOT NULL CHECK (review_state IN ('received','validated','rejected')),
    payload TEXT NOT NULL CHECK (length(payload)>0 AND length(payload)<=65536),
    created_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE proposal_validations (
    proposal_id TEXT NOT NULL REFERENCES memory_proposals(id),
    payload_digest TEXT NOT NULL CHECK (length(payload_digest)=64),
    validator TEXT NOT NULL,
    validator_version INTEGER NOT NULL CHECK (validator_version>0),
    result TEXT NOT NULL CHECK (result IN ('accepted','rejected')),
    reason TEXT NOT NULL,
    observed_heads TEXT NOT NULL,
    evaluated_seq INTEGER NOT NULL,
    PRIMARY KEY (proposal_id, payload_digest, validator, validator_version)
) STRICT;
CREATE UNIQUE INDEX one_proposal_identity ON memory_proposals(id);
CREATE TRIGGER memory_proposals_no_update BEFORE UPDATE ON memory_proposals BEGIN SELECT RAISE(ABORT,'memory proposal is immutable'); END;
CREATE TRIGGER memory_proposals_no_delete BEFORE DELETE ON memory_proposals BEGIN SELECT RAISE(ABORT,'memory proposal is immutable'); END;
CREATE TRIGGER proposal_validations_no_update BEFORE UPDATE ON proposal_validations BEGIN SELECT RAISE(ABORT,'proposal validation is immutable'); END;
CREATE TRIGGER proposal_validations_no_delete BEFORE DELETE ON proposal_validations BEGIN SELECT RAISE(ABORT,'proposal validation is immutable'); END;
UPDATE store_meta SET schema_version=21;
PRAGMA user_version=21;
