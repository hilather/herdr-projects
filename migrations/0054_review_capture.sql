-- Review capture (TM3.1, docs/telemetry/contracts-review.md). A review
-- opportunity is a planned chance to examine one exact candidate submission;
-- its assignment, the sessions that execute it and each session's completion
-- are separate rows, so "no findings" differs from "no review happened".
-- Metadata and evidence references only: no source, diff, brief or review
-- text. A completion is the reviewer's receipt, recorded as a proposal;
-- privileged acceptance is a separate table that stays inactive (every insert
-- aborts) until a canonical reviewer-authority producer exists. Append-only.
CREATE TABLE review_opportunities (
    opportunity_id TEXT PRIMARY KEY CHECK (length(opportunity_id) = 71 AND substr(opportunity_id, 1, 7) = 'sha256:'),
    submission_id TEXT NOT NULL REFERENCES result_submissions(submission_id),
    task_id TEXT NOT NULL,
    contract_revision INTEGER NOT NULL CHECK (contract_revision > 0),
    candidate_oid TEXT NOT NULL CHECK (length(candidate_oid) IN (40, 64)),
    scope TEXT NOT NULL CHECK (scope IN ('candidate_diff', 'candidate_tree', 'contract_scope')),
    kind TEXT NOT NULL CHECK (kind IN ('code', 'skeptical', 'security', 'test', 'architecture')),
    role TEXT NOT NULL CHECK (role IN ('gate', 'evaluation', 'advisory')),
    protocol TEXT NOT NULL CHECK (length(protocol) BETWEEN 1 AND 64),
    prior_findings TEXT NOT NULL CHECK (json_valid(prior_findings) AND json_array_length(prior_findings) <= 64),
    budget_ms INTEGER CHECK (budget_ms IS NULL OR budget_ms > 0),
    creator_principal TEXT NOT NULL CHECK (length(creator_principal) BETWEEN 1 AND 128),
    canonical_json TEXT NOT NULL CHECK (json_valid(canonical_json) AND length(canonical_json) <= 8192),
    created_unix_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX review_opportunities_by_submission ON review_opportunities(submission_id);
CREATE TABLE review_assignments (
    opportunity_id TEXT PRIMARY KEY REFERENCES review_opportunities(opportunity_id),
    policy TEXT NOT NULL CHECK (policy IN ('operator', 'blind_cross_provider.v1')),
    reviewer_configuration_id TEXT NOT NULL REFERENCES agent_configurations(configuration_id),
    reviewer_profile_digest TEXT NOT NULL CHECK (length(reviewer_profile_digest) = 64),
    reviewer_family TEXT,
    author_attempt_id TEXT NOT NULL,
    author_configuration_id TEXT,
    author_family TEXT,
    same_family INTEGER CHECK (same_family IS NULL OR same_family IN (0, 1)),
    blind INTEGER NOT NULL CHECK (blind IN (0, 1)),
    reason TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 64),
    eligible TEXT NOT NULL CHECK (json_valid(eligible) AND json_array_length(eligible) BETWEEN 1 AND 16),
    assigner_principal TEXT NOT NULL CHECK (length(assigner_principal) BETWEEN 1 AND 128),
    assigned_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE review_sessions (
    session_id TEXT PRIMARY KEY CHECK (length(session_id) = 71 AND substr(session_id, 1, 7) = 'sha256:'),
    opportunity_id TEXT NOT NULL REFERENCES review_assignments(opportunity_id),
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    configuration_id TEXT,
    matches_assignment INTEGER CHECK (matches_assignment IS NULL OR matches_assignment IN (0, 1)),
    same_attempt_as_author INTEGER NOT NULL CHECK (same_attempt_as_author IN (0, 1)),
    recorder_principal TEXT NOT NULL CHECK (length(recorder_principal) BETWEEN 1 AND 128),
    started_unix_ms INTEGER NOT NULL,
    UNIQUE (opportunity_id, ordinal),
    UNIQUE (opportunity_id, attempt_id)
) STRICT;
CREATE TABLE review_completions (
    session_id TEXT PRIMARY KEY REFERENCES review_sessions(session_id),
    outcome TEXT NOT NULL CHECK (outcome IN ('completed', 'incomplete', 'failed', 'timed_out', 'interrupted')),
    reason TEXT CHECK (reason IS NULL OR length(reason) BETWEEN 1 AND 64),
    submission_id TEXT NOT NULL,
    candidate_oid TEXT NOT NULL,
    findings_submitted INTEGER NOT NULL CHECK (findings_submitted >= 0),
    finding_refs TEXT NOT NULL CHECK (json_valid(finding_refs) AND json_array_length(finding_refs) = findings_submitted),
    evidence_refs TEXT NOT NULL CHECK (json_valid(evidence_refs) AND json_array_length(evidence_refs) <= 64),
    coverage_basis TEXT NOT NULL CHECK (coverage_basis = 'declared'),
    trust TEXT NOT NULL CHECK (trust = 'proposal'),
    receipt_digest TEXT NOT NULL CHECK (length(receipt_digest) = 71 AND substr(receipt_digest, 1, 7) = 'sha256:'),
    recorder_principal TEXT NOT NULL CHECK (length(recorder_principal) BETWEEN 1 AND 128),
    completed_unix_ms INTEGER NOT NULL,
    CHECK ((outcome = 'completed') = (reason IS NULL))
) STRICT;
-- Inactive: a canonical reviewer-authority producer (scoped grant, principal
-- distinct from the proposing worker) must replace the trigger below.
CREATE TABLE review_acceptances (
    session_id TEXT PRIMARY KEY REFERENCES review_completions(session_id),
    decision TEXT NOT NULL CHECK (decision IN ('accepted', 'rejected')),
    authority_principal TEXT NOT NULL CHECK (length(authority_principal) BETWEEN 1 AND 128),
    authority_ref TEXT NOT NULL CHECK (length(authority_ref) BETWEEN 1 AND 128),
    decided_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TRIGGER review_acceptances_inactive BEFORE INSERT ON review_acceptances
BEGIN SELECT RAISE(ABORT, 'review acceptance is inactive: no reviewer authority producer'); END;
-- Bindings: an opportunity names its submission's exact task, contract
-- revision and candidate; a completion receipt names the same candidate.
CREATE TRIGGER review_opportunities_exact BEFORE INSERT ON review_opportunities
WHEN NOT EXISTS (SELECT 1 FROM result_submissions s WHERE s.submission_id = NEW.submission_id AND s.task_id = NEW.task_id
        AND s.contract_revision = NEW.contract_revision AND s.candidate_oid = NEW.candidate_oid)
BEGIN SELECT RAISE(ABORT, 'review opportunity must bind its exact candidate'); END;
CREATE TRIGGER review_completions_exact BEFORE INSERT ON review_completions
WHEN NOT EXISTS (SELECT 1 FROM review_sessions r JOIN review_opportunities o ON o.opportunity_id = r.opportunity_id
        WHERE r.session_id = NEW.session_id AND o.submission_id = NEW.submission_id AND o.candidate_oid = NEW.candidate_oid)
BEGIN SELECT RAISE(ABORT, 'review receipt names another candidate'); END;
CREATE TRIGGER review_opportunities_no_update BEFORE UPDATE ON review_opportunities
BEGIN SELECT RAISE(ABORT, 'review opportunity is immutable'); END;
CREATE TRIGGER review_opportunities_no_delete BEFORE DELETE ON review_opportunities
BEGIN SELECT RAISE(ABORT, 'review opportunity is immutable'); END;
CREATE TRIGGER review_assignments_no_update BEFORE UPDATE ON review_assignments
BEGIN SELECT RAISE(ABORT, 'review assignment is immutable'); END;
CREATE TRIGGER review_assignments_no_delete BEFORE DELETE ON review_assignments
BEGIN SELECT RAISE(ABORT, 'review assignment is immutable'); END;
CREATE TRIGGER review_sessions_no_update BEFORE UPDATE ON review_sessions
BEGIN SELECT RAISE(ABORT, 'review session is immutable'); END;
CREATE TRIGGER review_sessions_no_delete BEFORE DELETE ON review_sessions
BEGIN SELECT RAISE(ABORT, 'review session is immutable'); END;
CREATE TRIGGER review_completions_no_update BEFORE UPDATE ON review_completions
BEGIN SELECT RAISE(ABORT, 'review completion is immutable'); END;
CREATE TRIGGER review_completions_no_delete BEFORE DELETE ON review_completions
BEGIN SELECT RAISE(ABORT, 'review completion is immutable'); END;
UPDATE store_meta SET schema_version = 54;
PRAGMA user_version = 54;
