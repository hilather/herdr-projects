-- Delegated code-review authority (docs/telemetry/contracts-review.md §10,
-- factory plan F2.5). A `code_review` grant is an owner-signed policy of its
-- own class: it names one reviewer principal and its public key, the project,
-- repositories, task contract revisions and review kinds it covers, the
-- permitted actions, a decision limit, a validity interval and the effects it
-- never grants. It is verified with the owner's key at import and again at
-- every acceptance; this schema stores the exact signed bytes and never trusts
-- a column over them. A revocation is a separate owner-signed record: it stops
-- later decisions and leaves every earlier one in place. Nothing here is a
-- launch approval or a reservation, and a grant cannot mint another grant.
CREATE TABLE review_authority_grants (
    grant_id TEXT PRIMARY KEY CHECK (length(grant_id) = 71 AND substr(grant_id, 1, 7) = 'sha256:'),
    raw_bytes BLOB NOT NULL CHECK (length(raw_bytes) BETWEEN 1 AND 65536),
    signature BLOB NOT NULL CHECK (length(signature) BETWEEN 1 AND 8192),
    scope TEXT NOT NULL CHECK (scope = 'code_review'),
    issuer TEXT NOT NULL CHECK (issuer = 'owner'),
    subject TEXT NOT NULL CHECK (length(subject) BETWEEN 10 AND 73 AND substr(subject, 1, 9) = 'reviewer:'),
    subject_public_key TEXT NOT NULL CHECK (length(subject_public_key) BETWEEN 1 AND 512),
    project_store TEXT NOT NULL CHECK (length(project_store) BETWEEN 1 AND 4096),
    actions TEXT NOT NULL CHECK (json_valid(actions) AND json_array_length(actions) BETWEEN 1 AND 8),
    repositories TEXT NOT NULL CHECK (json_valid(repositories) AND json_array_length(repositories) BETWEEN 1 AND 32),
    tasks TEXT NOT NULL CHECK (json_valid(tasks) AND json_array_length(tasks) BETWEEN 1 AND 128),
    kinds TEXT NOT NULL CHECK (json_valid(kinds) AND json_array_length(kinds) BETWEEN 1 AND 5),
    review_configurations TEXT NOT NULL CHECK (json_valid(review_configurations) AND json_array_length(review_configurations) <= 16),
    subject_configurations TEXT NOT NULL CHECK (json_valid(subject_configurations) AND json_array_length(subject_configurations) <= 16),
    max_decisions INTEGER NOT NULL CHECK (max_decisions BETWEEN 1 AND 1024),
    valid_from_unix_ms INTEGER NOT NULL CHECK (valid_from_unix_ms >= 0),
    expires_unix_ms INTEGER NOT NULL,
    authority_revision INTEGER NOT NULL CHECK (authority_revision > 0),
    authority_digest TEXT NOT NULL CHECK (length(authority_digest) = 64),
    installed_unix_ms INTEGER NOT NULL CHECK (installed_unix_ms >= 0),
    CHECK (expires_unix_ms > valid_from_unix_ms)
) STRICT;
CREATE TRIGGER review_authority_grants_no_update BEFORE UPDATE ON review_authority_grants
BEGIN SELECT RAISE(ABORT, 'review authority grant is immutable'); END;
CREATE TRIGGER review_authority_grants_no_delete BEFORE DELETE ON review_authority_grants
BEGIN SELECT RAISE(ABORT, 'review authority grant is immutable'); END;

CREATE TABLE review_authority_revocations (
    grant_id TEXT PRIMARY KEY REFERENCES review_authority_grants(grant_id),
    raw_bytes BLOB NOT NULL CHECK (length(raw_bytes) BETWEEN 1 AND 65536),
    signature BLOB NOT NULL CHECK (length(signature) BETWEEN 1 AND 8192),
    revocation_digest TEXT NOT NULL UNIQUE CHECK (length(revocation_digest) = 71 AND substr(revocation_digest, 1, 7) = 'sha256:'),
    reason TEXT NOT NULL CHECK (reason IN ('compromised', 'issued_in_error', 'reviewer_retired', 'scope_changed')),
    revoked_unix_ms INTEGER NOT NULL CHECK (revoked_unix_ms >= 0)
) STRICT;
CREATE TRIGGER review_authority_revocations_no_update BEFORE UPDATE ON review_authority_revocations
BEGIN SELECT RAISE(ABORT, 'review authority revocation is immutable'); END;
CREATE TRIGGER review_authority_revocations_no_delete BEFORE DELETE ON review_authority_revocations
BEGIN SELECT RAISE(ABORT, 'review authority revocation is immutable'); END;

-- Acceptance becomes active through that authority only. 0054 created the
-- table with an always-abort trigger, so it has no rows; it is replaced by one
-- that keeps the reviewer's exact signed request.
DROP TRIGGER review_acceptances_inactive;
DROP TABLE review_acceptances;
CREATE TABLE review_acceptances (
    session_id TEXT PRIMARY KEY REFERENCES review_completions(session_id),
    decision TEXT NOT NULL CHECK (decision IN ('accepted', 'rejected')),
    reason TEXT CHECK (reason IN ('insufficient_coverage', 'evidence_missing', 'protocol_violation', 'wrong_scope')),
    authority_principal TEXT NOT NULL CHECK (length(authority_principal) BETWEEN 1 AND 128),
    authority_ref TEXT NOT NULL REFERENCES review_authority_grants(grant_id),
    authority TEXT NOT NULL CHECK (authority = 'delegated_code_review.v1'),
    receipt_digest TEXT NOT NULL CHECK (length(receipt_digest) = 71 AND substr(receipt_digest, 1, 7) = 'sha256:'),
    request_digest TEXT NOT NULL UNIQUE CHECK (length(request_digest) = 71 AND substr(request_digest, 1, 7) = 'sha256:'),
    request_bytes BLOB NOT NULL CHECK (length(request_bytes) BETWEEN 1 AND 65536),
    request_signature BLOB NOT NULL CHECK (length(request_signature) BETWEEN 1 AND 8192),
    decided_unix_ms INTEGER NOT NULL,
    CHECK ((decision = 'accepted') = (reason IS NULL))
) STRICT;
CREATE INDEX review_acceptances_by_grant ON review_acceptances(authority_ref);
CREATE TRIGGER review_acceptances_no_update BEFORE UPDATE ON review_acceptances
BEGIN SELECT RAISE(ABORT, 'review acceptance is immutable'); END;
CREATE TRIGGER review_acceptances_no_delete BEFORE DELETE ON review_acceptances
BEGIN SELECT RAISE(ABORT, 'review acceptance is immutable'); END;
-- A decision needs a grant that covers it at its decision time: named
-- principal, unrevoked, within validity, the accept action, the session's
-- repository, task contract revision, kind and (when listed) reviewer
-- configuration, a completed review with the exact receipt, never the
-- reviewer's own session or the author attempt's work, and within the limit.
CREATE TRIGGER review_acceptances_authorized BEFORE INSERT ON review_acceptances
WHEN NOT EXISTS (
    SELECT 1 FROM review_authority_grants g
        JOIN review_sessions r ON r.session_id = NEW.session_id
        JOIN review_completions c ON c.session_id = r.session_id
        JOIN review_opportunities o ON o.opportunity_id = r.opportunity_id
        JOIN result_submissions s ON s.submission_id = o.submission_id
    WHERE g.grant_id = NEW.authority_ref AND g.subject = NEW.authority_principal
      AND NOT EXISTS (SELECT 1 FROM review_authority_revocations v WHERE v.grant_id = g.grant_id)
      AND NEW.decided_unix_ms >= g.valid_from_unix_ms AND NEW.decided_unix_ms < g.expires_unix_ms
      AND c.outcome = 'completed' AND c.receipt_digest = NEW.receipt_digest AND NEW.decided_unix_ms >= c.completed_unix_ms
      AND EXISTS (SELECT 1 FROM json_each(g.actions) a WHERE a.value = 'accept_review_completion')
      AND EXISTS (SELECT 1 FROM json_each(g.repositories) p WHERE p.value = s.repository)
      AND EXISTS (SELECT 1 FROM json_each(g.tasks) t WHERE json_extract(t.value, '$.task_id') = o.task_id
          AND json_extract(t.value, '$.contract_revision') = o.contract_revision)
      AND EXISTS (SELECT 1 FROM json_each(g.kinds) k WHERE k.value = o.kind)
      AND (json_array_length(g.review_configurations) = 0
          OR EXISTS (SELECT 1 FROM json_each(g.review_configurations) q WHERE q.value = r.configuration_id))
      AND r.same_attempt_as_author = 0
      AND substr(g.subject, 10) <> r.attempt_id AND substr(g.subject, 10) <> s.attempt_id
      AND NOT EXISTS (SELECT 1 FROM json_each(g.subject_configurations) m WHERE m.value = r.configuration_id
          OR m.value = (SELECT d.chosen_configuration_id FROM dispatch_decisions d WHERE d.attempt_id = s.attempt_id))
      AND (SELECT count(*) FROM review_acceptances x WHERE x.authority_ref = g.grant_id) < g.max_decisions)
BEGIN SELECT RAISE(ABORT, 'review acceptance needs a valid, unexpired, unrevoked code_review grant covering this session'); END;
UPDATE store_meta SET schema_version = 61;
PRAGMA user_version = 61;
