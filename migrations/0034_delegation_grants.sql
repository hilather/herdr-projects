-- Delegation is its own signed class. It is not a launch approval, and it
-- cannot authorize a child grant. Revocation stops new admits only: a provider
-- call that already started is not undone, which the stop-obligation row records.
CREATE TABLE delegation_grants (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 64),
    raw_bytes BLOB NOT NULL CHECK (length(raw_bytes) BETWEEN 1 AND 65536),
    raw_digest TEXT NOT NULL CHECK (length(raw_digest) = 64),
    project_store TEXT NOT NULL CHECK (length(project_store) BETWEEN 1 AND 4096),
    issuer TEXT NOT NULL CHECK (length(issuer) BETWEEN 1 AND 128),
    subject TEXT NOT NULL CHECK (length(subject) BETWEEN 1 AND 128),
    subject_public_key TEXT NOT NULL CHECK (length(subject_public_key) BETWEEN 1 AND 512),
    action_classes TEXT NOT NULL CHECK (json_valid(action_classes) AND length(action_classes) <= 1024),
    repositories TEXT NOT NULL CHECK (json_valid(repositories) AND length(repositories) <= 16384),
    profile_kinds TEXT NOT NULL CHECK (json_valid(profile_kinds) AND length(profile_kinds) <= 1024),
    max_concurrent_attempts INTEGER NOT NULL CHECK (max_concurrent_attempts BETWEEN 1 AND 64),
    expires_unix_ms INTEGER NOT NULL CHECK (expires_unix_ms > 0),
    revocation_epoch INTEGER NOT NULL CHECK (revocation_epoch > 0),
    child_delegation TEXT NOT NULL CHECK (child_delegation = 'forbidden'),
    policy_revision INTEGER NOT NULL CHECK (policy_revision > 0),
    authority_digest TEXT NOT NULL CHECK (length(authority_digest) = 64),
    installed_unix_ms INTEGER NOT NULL CHECK (installed_unix_ms >= 0),
    -- A grant cannot sign itself. The signing key is checked before insert.
    CHECK (issuer <> subject),
    CHECK (raw_digest = id)
) STRICT;
CREATE INDEX delegation_grants_by_project ON delegation_grants(project_store, expires_unix_ms);
CREATE TRIGGER delegation_grants_no_update BEFORE UPDATE ON delegation_grants
BEGIN SELECT RAISE(ABORT, 'delegation grant is immutable'); END;
CREATE TRIGGER delegation_grants_no_delete BEFORE DELETE ON delegation_grants
BEGIN SELECT RAISE(ABORT, 'delegation grant is immutable'); END;

CREATE TABLE delegation_revocations (
    grant_id TEXT PRIMARY KEY NOT NULL REFERENCES delegation_grants(id),
    revoked_unix_ms INTEGER NOT NULL CHECK (revoked_unix_ms >= 0),
    reason TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 4000),
    revocation_epoch INTEGER NOT NULL CHECK (revocation_epoch > 0)
) STRICT;
CREATE TRIGGER delegation_revocations_no_update BEFORE UPDATE ON delegation_revocations
BEGIN SELECT RAISE(ABORT, 'delegation revocation is immutable'); END;
CREATE TRIGGER delegation_revocations_no_delete BEFORE DELETE ON delegation_revocations
BEGIN SELECT RAISE(ABORT, 'delegation revocation is immutable'); END;

CREATE TABLE delegation_stop_obligations (
    obligation_id TEXT PRIMARY KEY NOT NULL CHECK (length(obligation_id) = 64),
    grant_id TEXT NOT NULL REFERENCES delegation_grants(id),
    created_unix_ms INTEGER NOT NULL CHECK (created_unix_ms >= 0),
    -- Stops new reserve_attempt admits. Does not undo a provider call already started.
    boundary TEXT NOT NULL CHECK (boundary = 'stops_new_admits_does_not_undo_started_provider_call')
) STRICT;
CREATE INDEX delegation_stop_obligations_by_grant ON delegation_stop_obligations(grant_id);
CREATE TRIGGER delegation_stop_obligations_no_update BEFORE UPDATE ON delegation_stop_obligations
BEGIN SELECT RAISE(ABORT, 'delegation stop obligation is immutable'); END;
CREATE TRIGGER delegation_stop_obligations_no_delete BEFORE DELETE ON delegation_stop_obligations
BEGIN SELECT RAISE(ABORT, 'delegation stop obligation is immutable'); END;

-- The denial CHECK is part of this unshipped schema. Copy every existing row,
-- then allow the delegation class. Do not drop denial history.
DROP TRIGGER IF EXISTS authority_denials_no_update;
DROP TRIGGER IF EXISTS authority_denials_no_delete;
CREATE TABLE authority_denials_v34 (
    id TEXT PRIMARY KEY NOT NULL,
    unix_ms INTEGER NOT NULL,
    class TEXT NOT NULL CHECK (class IN ('approval','budget','routine-store','memory','contract','delegation')),
    command TEXT NOT NULL,
    actor_channel TEXT NOT NULL CHECK (actor_channel IN ('cli-owner','unknown-rejected')),
    reason_code TEXT NOT NULL,
    policy_digest TEXT NOT NULL CHECK (length(policy_digest) = 64),
    expected_head INTEGER,
    actual_head INTEGER
) STRICT;
INSERT INTO authority_denials_v34(id, unix_ms, class, command, actor_channel, reason_code, policy_digest, expected_head, actual_head)
SELECT id, unix_ms, class, command, actor_channel, reason_code, policy_digest, expected_head, actual_head FROM authority_denials;
DROP TABLE authority_denials;
ALTER TABLE authority_denials_v34 RENAME TO authority_denials;
CREATE TRIGGER authority_denials_no_update BEFORE UPDATE ON authority_denials BEGIN SELECT RAISE(ABORT,'authority denial is immutable'); END;
CREATE TRIGGER authority_denials_no_delete BEFORE DELETE ON authority_denials BEGIN SELECT RAISE(ABORT,'authority denial is immutable'); END;

UPDATE store_meta SET schema_version = 34;
PRAGMA user_version = 34;
