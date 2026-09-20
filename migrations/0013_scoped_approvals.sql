CREATE TABLE approval_grants (
    id TEXT PRIMARY KEY NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK(length(payload_hash)=64)
) STRICT;
CREATE TABLE approval_revocations (
    approval_id TEXT PRIMARY KEY NOT NULL REFERENCES approval_grants(id),
    revoked_unix_ms INTEGER NOT NULL CHECK(revoked_unix_ms>=0),
    reason TEXT NOT NULL CHECK(length(reason)>0 AND length(reason)<=4000)
) STRICT;
CREATE TABLE approval_uses (
    approval_id TEXT PRIMARY KEY NOT NULL REFERENCES approval_grants(id),
    operation_id TEXT NOT NULL UNIQUE REFERENCES operations(id),
    claim_revision INTEGER NOT NULL CHECK(claim_revision>0),
    claim_epoch INTEGER NOT NULL CHECK(claim_epoch>0),
    consumed_unix_ms INTEGER NOT NULL CHECK(consumed_unix_ms>=0)
) STRICT;
CREATE TRIGGER approval_grants_no_update BEFORE UPDATE ON approval_grants BEGIN SELECT RAISE(ABORT,'approval grant is immutable'); END;
CREATE TRIGGER approval_grants_no_delete BEFORE DELETE ON approval_grants BEGIN SELECT RAISE(ABORT,'approval grant is immutable'); END;
CREATE TRIGGER approval_revocations_no_update BEFORE UPDATE ON approval_revocations BEGIN SELECT RAISE(ABORT,'revocation is immutable'); END;
CREATE TRIGGER approval_revocations_no_delete BEFORE DELETE ON approval_revocations BEGIN SELECT RAISE(ABORT,'revocation is immutable'); END;
CREATE TRIGGER approval_uses_no_update BEFORE UPDATE ON approval_uses BEGIN SELECT RAISE(ABORT,'approval use is immutable'); END;
CREATE TRIGGER approval_uses_no_delete BEFORE DELETE ON approval_uses BEGIN SELECT RAISE(ABORT,'approval use is immutable'); END;
UPDATE store_meta SET schema_version=13;
PRAGMA user_version=13;
