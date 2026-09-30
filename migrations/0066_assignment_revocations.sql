-- Revocation of a `randomized_assignment` grant (TM4.7 follow-up,
-- docs/telemetry/contracts-evaluation.md §9; factory plan F2.5 requires
-- explicit revocation of bounded delegated authority). The pattern is D8's
-- `review_authority_revocations` (0061): a separate owner-signed record,
-- verified with the owner's key under its own namespace, append-only, one per
-- grant. It stops assignment under the grant from its commit on (admission
-- re-checks it inside the reservation transaction and the trigger below
-- repeats that on raw rows); every decision made earlier stays as recorded.
CREATE TABLE assignment_authority_revocations (
    grant_id TEXT PRIMARY KEY REFERENCES assignment_authority_grants(grant_id),
    raw_bytes BLOB NOT NULL CHECK (length(raw_bytes) BETWEEN 1 AND 65536),
    signature BLOB NOT NULL CHECK (length(signature) BETWEEN 1 AND 8192),
    revocation_digest TEXT NOT NULL UNIQUE CHECK (length(revocation_digest) = 71 AND substr(revocation_digest, 1, 7) = 'sha256:'),
    reason TEXT NOT NULL CHECK (reason IN ('compromised', 'experiment_ended', 'issued_in_error', 'scope_changed')),
    revoked_unix_ms INTEGER NOT NULL CHECK (revoked_unix_ms >= 0)
) STRICT;
CREATE TRIGGER assignment_authority_revocations_no_update BEFORE UPDATE ON assignment_authority_revocations
BEGIN SELECT RAISE(ABORT, 'assignment authority revocation is immutable'); END;
CREATE TRIGGER assignment_authority_revocations_no_delete BEFORE DELETE ON assignment_authority_revocations
BEGIN SELECT RAISE(ABORT, 'assignment authority revocation is immutable'); END;

-- A revoked grant assigns nothing more and cannot be switched on again.
CREATE TRIGGER dispatch_policy_assignments_unrevoked BEFORE INSERT ON dispatch_policy_assignments
WHEN EXISTS (SELECT 1 FROM assignment_authority_revocations v WHERE v.grant_id = NEW.grant_id)
BEGIN SELECT RAISE(ABORT, 'randomized_assignment grant is revoked'); END;
CREATE TRIGGER assignment_policy_settings_unrevoked BEFORE INSERT ON assignment_policy_settings
WHEN NEW.grant_id IS NOT NULL AND EXISTS (SELECT 1 FROM assignment_authority_revocations v WHERE v.grant_id = NEW.grant_id)
BEGIN SELECT RAISE(ABORT, 'randomized_assignment grant is revoked'); END;

UPDATE store_meta SET schema_version = 66;
PRAGMA user_version = 66;
