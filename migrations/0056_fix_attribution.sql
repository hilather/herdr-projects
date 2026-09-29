-- Fix attribution, regressions and role credit (TM3.3,
-- docs/telemetry/contracts-review.md §6). `fix_log` extends the finding
-- history's ordering: its `seq` values and `finding_log`'s form one strictly
-- increasing sequence (the replay watermark of both), so every triage
-- decision and every attribution decision has one place in one order.
-- Every row is append-only. Only the triage authority (`operator:cli`, the
-- project owner, `operator_owner.v1`) records attribution; workers and imports
-- cannot. Repair opportunities and their attempts are bound before any
-- outcome of the attempt exists; a fix is verified only by an accepted
-- verification of its exact candidate and integrated only by an integration
-- of that verified candidate. Metadata and references only: no text.
CREATE TABLE fix_log (
    seq INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('repair_opened', 'attempt_bound', 'proposed', 'verified', 'integrated', 'repair_closed',
        'reopened', 'introduced', 'credited', 'retracted')),
    principal TEXT NOT NULL CHECK (principal = 'operator:cli'),
    authority TEXT NOT NULL CHECK (authority = 'operator_owner.v1'),
    expected_seq INTEGER CHECK (expected_seq IS NULL OR expected_seq >= 0),
    recorded_unix_ms INTEGER NOT NULL
) STRICT;
-- A repair opportunity for one canonical finding, with its initial
-- assignment group frozen before any repair runs.
CREATE TABLE repair_opportunities (
    seq INTEGER PRIMARY KEY REFERENCES fix_log(seq),
    finding_id TEXT NOT NULL REFERENCES canonical_findings(finding_id),
    assignment TEXT NOT NULL CHECK (assignment IN ('configuration', 'unassigned')),
    configuration_id TEXT REFERENCES agent_configurations(configuration_id),
    profile_digest TEXT CHECK (profile_digest IS NULL OR length(profile_digest) BETWEEN 1 AND 128),
    policy TEXT NOT NULL CHECK (policy = 'repair_assignment.v1'),
    horizon_ms INTEGER NOT NULL CHECK (horizon_ms > 0),
    CHECK ((assignment = 'configuration') = (configuration_id IS NOT NULL AND profile_digest IS NOT NULL)),
    CHECK (assignment = 'configuration' OR (configuration_id IS NULL AND profile_digest IS NULL))
) STRICT;
-- An attempt bound to a repair opportunity before it has any result.
-- Ordinal 1 is the initial attempt; later ones are reassignments.
CREATE TABLE repair_attempts (
    seq INTEGER PRIMARY KEY REFERENCES fix_log(seq),
    repair_seq INTEGER NOT NULL REFERENCES repair_opportunities(seq),
    attempt_id TEXT NOT NULL UNIQUE REFERENCES attempts(id),
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    configuration_id TEXT REFERENCES agent_configurations(configuration_id),
    UNIQUE (repair_seq, ordinal)
) STRICT;
-- A fix proposal: one result submission of a bound attempt, at its exact candidate.
CREATE TABLE fix_proposals (
    seq INTEGER PRIMARY KEY REFERENCES fix_log(seq),
    repair_seq INTEGER NOT NULL REFERENCES repair_opportunities(seq),
    submission_id TEXT NOT NULL UNIQUE REFERENCES result_submissions(submission_id),
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    candidate_oid TEXT NOT NULL CHECK (length(candidate_oid) IN (40, 64))
) STRICT;
-- The owner's decision that an accepted native verification of the
-- proposal's exact candidate repairs the finding.
CREATE TABLE fix_verifications (
    seq INTEGER PRIMARY KEY REFERENCES fix_log(seq),
    proposal_seq INTEGER NOT NULL UNIQUE REFERENCES fix_proposals(seq),
    run_id TEXT NOT NULL REFERENCES verification_runs(run_id),
    result_id TEXT NOT NULL REFERENCES verified_results(result_id),
    commit_oid TEXT NOT NULL CHECK (length(commit_oid) IN (40, 64)),
    assurance TEXT NOT NULL CHECK (assurance IN ('regression_reproduced', 'approved_alternative')),
    evidence_refs TEXT NOT NULL CHECK (json_valid(evidence_refs) AND json_array_length(evidence_refs) BETWEEN 1 AND 64)
) STRICT;
-- The integration of a verified fix's exact candidate.
CREATE TABLE fix_integrations (
    seq INTEGER PRIMARY KEY REFERENCES fix_log(seq),
    proposal_seq INTEGER NOT NULL UNIQUE REFERENCES fix_proposals(seq),
    verification_seq INTEGER NOT NULL REFERENCES fix_verifications(seq),
    integrated_id TEXT NOT NULL UNIQUE REFERENCES integrated_commits(integrated_id),
    commit_oid TEXT NOT NULL CHECK (length(commit_oid) IN (40, 64)),
    integrated_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE repair_closures (
    seq INTEGER PRIMARY KEY REFERENCES fix_log(seq),
    repair_seq INTEGER NOT NULL UNIQUE REFERENCES repair_opportunities(seq),
    outcome TEXT NOT NULL CHECK (outcome IN ('fixed', 'no_fix', 'cancelled'))
) STRICT;
-- An accepted occurrence at a later exact revision: the defect remains or
-- returned (`regression`), or the integrated fix was reverted. It ends the
-- current resolution of `integration_seq` and keeps its history.
CREATE TABLE fix_reopenings (
    seq INTEGER PRIMARY KEY REFERENCES fix_log(seq),
    finding_id TEXT NOT NULL REFERENCES canonical_findings(finding_id),
    integration_seq INTEGER NOT NULL REFERENCES fix_integrations(seq),
    reason TEXT NOT NULL CHECK (reason IN ('regression', 'reverted')),
    observed_oid TEXT NOT NULL CHECK (length(observed_oid) IN (40, 64)),
    evidence_refs TEXT NOT NULL CHECK (json_valid(evidence_refs) AND json_array_length(evidence_refs) BETWEEN 1 AND 64)
) STRICT;
-- A causal introduction decision; without one a finding is `unattributed`.
CREATE TABLE introduction_decisions (
    seq INTEGER PRIMARY KEY REFERENCES fix_log(seq),
    finding_id TEXT NOT NULL REFERENCES canonical_findings(finding_id),
    status TEXT NOT NULL CHECK (status IN ('attributed', 'unattributable')),
    method TEXT CHECK (method IS NULL OR method IN ('controlled_reproducer', 'reliable_bisect', 'minimized_patch')),
    introducing_oid TEXT CHECK (introducing_oid IS NULL OR length(introducing_oid) IN (40, 64)),
    evidence_refs TEXT NOT NULL CHECK (json_valid(evidence_refs) AND json_array_length(evidence_refs) BETWEEN 1 AND 64),
    CHECK ((status = 'attributed') = (method IS NOT NULL AND introducing_oid IS NOT NULL))
) STRICT;
-- An owner's fractional allocation of one role's credit for one finding
-- (discovery) or one verified fix (implementation).
CREATE TABLE credit_allocations (
    seq INTEGER PRIMARY KEY REFERENCES fix_log(seq),
    finding_id TEXT NOT NULL REFERENCES canonical_findings(finding_id),
    role TEXT NOT NULL CHECK (role IN ('discovery', 'implementation')),
    proposal_seq INTEGER REFERENCES fix_verifications(proposal_seq),
    policy TEXT NOT NULL CHECK (policy = 'owner_allocation.v1'),
    evidence_refs TEXT NOT NULL CHECK (json_valid(evidence_refs) AND json_array_length(evidence_refs) BETWEEN 1 AND 64),
    CHECK ((role = 'implementation') = (proposal_seq IS NOT NULL))
) STRICT;
-- Shares of a credit allocation or an introduction decision; each is an
-- exact fraction with a denominator of at most 16, and the shares of one row
-- sum to at most 1 (the remainder stays unallocated).
CREATE TABLE credit_shares (
    seq INTEGER NOT NULL REFERENCES fix_log(seq),
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    share_num INTEGER NOT NULL CHECK (share_num > 0),
    share_den INTEGER NOT NULL CHECK (share_den BETWEEN 1 AND 16 AND share_num <= share_den),
    PRIMARY KEY (seq, attempt_id)
) STRICT;
-- A correction: the retracted credit, reopening or introduction no longer applies after this seq.
CREATE TABLE fix_retractions (
    seq INTEGER PRIMARY KEY REFERENCES fix_log(seq),
    reverses INTEGER NOT NULL UNIQUE REFERENCES fix_log(seq),
    CHECK (reverses < seq)
) STRICT;
CREATE INDEX repair_opportunities_by_finding ON repair_opportunities(finding_id, seq);
CREATE INDEX repair_attempts_by_repair ON repair_attempts(repair_seq, seq);
CREATE INDEX fix_proposals_by_repair ON fix_proposals(repair_seq, seq);
-- One ordering: a finding row and a fix row never share or reorder a seq.
CREATE TRIGGER fix_log_one_order AFTER INSERT ON fix_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM finding_log), 0) OR EXISTS (SELECT 1 FROM fix_log WHERE seq > NEW.seq)
BEGIN SELECT RAISE(ABORT, 'finding and fix history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER finding_log_one_order AFTER INSERT ON finding_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM fix_log), 0)
BEGIN SELECT RAISE(ABORT, 'finding and fix history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER fix_log_no_update BEFORE UPDATE ON fix_log BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER fix_log_no_delete BEFORE DELETE ON fix_log BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER repair_opportunities_no_update BEFORE UPDATE ON repair_opportunities BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER repair_opportunities_no_delete BEFORE DELETE ON repair_opportunities BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER repair_attempts_no_update BEFORE UPDATE ON repair_attempts BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER repair_attempts_no_delete BEFORE DELETE ON repair_attempts BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER fix_proposals_no_update BEFORE UPDATE ON fix_proposals BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER fix_proposals_no_delete BEFORE DELETE ON fix_proposals BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER fix_verifications_no_update BEFORE UPDATE ON fix_verifications BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER fix_verifications_no_delete BEFORE DELETE ON fix_verifications BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER fix_integrations_no_update BEFORE UPDATE ON fix_integrations BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER fix_integrations_no_delete BEFORE DELETE ON fix_integrations BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER repair_closures_no_update BEFORE UPDATE ON repair_closures BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER repair_closures_no_delete BEFORE DELETE ON repair_closures BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER fix_reopenings_no_update BEFORE UPDATE ON fix_reopenings BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER fix_reopenings_no_delete BEFORE DELETE ON fix_reopenings BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER introduction_decisions_no_update BEFORE UPDATE ON introduction_decisions BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER introduction_decisions_no_delete BEFORE DELETE ON introduction_decisions BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER credit_allocations_no_update BEFORE UPDATE ON credit_allocations BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER credit_allocations_no_delete BEFORE DELETE ON credit_allocations BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER credit_shares_no_update BEFORE UPDATE ON credit_shares BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER credit_shares_no_delete BEFORE DELETE ON credit_shares BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER fix_retractions_no_update BEFORE UPDATE ON fix_retractions BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
CREATE TRIGGER fix_retractions_no_delete BEFORE DELETE ON fix_retractions BEGIN SELECT RAISE(ABORT, 'fix history is append-only'); END;
-- Each detail row needs its own history row of the matching kind.
CREATE TRIGGER repair_opportunities_kind BEFORE INSERT ON repair_opportunities
WHEN NOT EXISTS (SELECT 1 FROM fix_log l WHERE l.seq = NEW.seq AND l.kind = 'repair_opened')
BEGIN SELECT RAISE(ABORT, 'fix attribution needs its history row'); END;
CREATE TRIGGER repair_attempts_kind BEFORE INSERT ON repair_attempts
WHEN NOT EXISTS (SELECT 1 FROM fix_log l WHERE l.seq = NEW.seq AND l.kind = 'attempt_bound')
BEGIN SELECT RAISE(ABORT, 'fix attribution needs its history row'); END;
CREATE TRIGGER fix_proposals_kind BEFORE INSERT ON fix_proposals
WHEN NOT EXISTS (SELECT 1 FROM fix_log l WHERE l.seq = NEW.seq AND l.kind = 'proposed')
BEGIN SELECT RAISE(ABORT, 'fix attribution needs its history row'); END;
CREATE TRIGGER fix_verifications_kind BEFORE INSERT ON fix_verifications
WHEN NOT EXISTS (SELECT 1 FROM fix_log l WHERE l.seq = NEW.seq AND l.kind = 'verified')
BEGIN SELECT RAISE(ABORT, 'fix attribution needs its history row'); END;
CREATE TRIGGER fix_integrations_kind BEFORE INSERT ON fix_integrations
WHEN NOT EXISTS (SELECT 1 FROM fix_log l WHERE l.seq = NEW.seq AND l.kind = 'integrated')
BEGIN SELECT RAISE(ABORT, 'fix attribution needs its history row'); END;
CREATE TRIGGER repair_closures_kind BEFORE INSERT ON repair_closures
WHEN NOT EXISTS (SELECT 1 FROM fix_log l WHERE l.seq = NEW.seq AND l.kind = 'repair_closed')
BEGIN SELECT RAISE(ABORT, 'fix attribution needs its history row'); END;
CREATE TRIGGER fix_reopenings_kind BEFORE INSERT ON fix_reopenings
WHEN NOT EXISTS (SELECT 1 FROM fix_log l WHERE l.seq = NEW.seq AND l.kind = 'reopened')
BEGIN SELECT RAISE(ABORT, 'fix attribution needs its history row'); END;
CREATE TRIGGER introduction_decisions_kind BEFORE INSERT ON introduction_decisions
WHEN NOT EXISTS (SELECT 1 FROM fix_log l WHERE l.seq = NEW.seq AND l.kind = 'introduced')
BEGIN SELECT RAISE(ABORT, 'fix attribution needs its history row'); END;
CREATE TRIGGER credit_allocations_kind BEFORE INSERT ON credit_allocations
WHEN NOT EXISTS (SELECT 1 FROM fix_log l WHERE l.seq = NEW.seq AND l.kind = 'credited')
BEGIN SELECT RAISE(ABORT, 'fix attribution needs its history row'); END;
CREATE TRIGGER credit_shares_kind BEFORE INSERT ON credit_shares
WHEN NOT EXISTS (SELECT 1 FROM fix_log l WHERE l.seq = NEW.seq AND l.kind IN ('credited', 'introduced'))
BEGIN SELECT RAISE(ABORT, 'fix attribution needs its history row'); END;
CREATE TRIGGER fix_retractions_kind BEFORE INSERT ON fix_retractions
WHEN NOT EXISTS (SELECT 1 FROM fix_log l WHERE l.seq = NEW.seq AND l.kind = 'retracted')
    OR NOT EXISTS (SELECT 1 FROM fix_log l WHERE l.seq = NEW.reverses AND l.kind IN ('credited', 'reopened', 'introduced'))
BEGIN SELECT RAISE(ABORT, 'only a credit allocation, reopening or introduction decision can be retracted'); END;
-- Binding before outcome: an attempt with a result submission cannot be bound.
CREATE TRIGGER repair_attempts_before_outcome BEFORE INSERT ON repair_attempts
WHEN EXISTS (SELECT 1 FROM result_submissions s WHERE s.attempt_id = NEW.attempt_id)
    OR EXISTS (SELECT 1 FROM repair_closures c WHERE c.repair_seq = NEW.repair_seq)
BEGIN SELECT RAISE(ABORT, 'a repair attempt is bound to an open repair opportunity before it has any result'); END;
-- A proposal is a submission of an attempt bound to that repair, at the submission's candidate.
CREATE TRIGGER fix_proposals_exact BEFORE INSERT ON fix_proposals
WHEN NOT EXISTS (SELECT 1 FROM result_submissions s JOIN repair_attempts a ON a.attempt_id = s.attempt_id
        WHERE s.submission_id = NEW.submission_id AND s.attempt_id = NEW.attempt_id AND s.candidate_oid = NEW.candidate_oid AND a.repair_seq = NEW.repair_seq)
BEGIN SELECT RAISE(ABORT, 'a fix proposal is a result submission of an attempt bound to its repair, at its exact candidate'); END;
-- Exact candidate: an accepted run and verified result of the proposal's own submission and commit.
CREATE TRIGGER fix_verifications_exact BEFORE INSERT ON fix_verifications
WHEN NOT EXISTS (SELECT 1 FROM fix_proposals p
        JOIN verification_runs r ON r.submission_id = p.submission_id AND r.commit_oid = p.candidate_oid AND r.state = 'accepted'
        JOIN verified_results v ON v.run_id = r.run_id AND v.submission_id = p.submission_id AND v.commit_oid = p.candidate_oid
        WHERE p.seq = NEW.proposal_seq AND r.run_id = NEW.run_id AND v.result_id = NEW.result_id AND NEW.commit_oid = p.candidate_oid)
BEGIN SELECT RAISE(ABORT, 'a fix is verified only by an accepted verification of its exact candidate'); END;
-- Exact candidate: the integration of a verified result of the same submission and commit.
CREATE TRIGGER fix_integrations_exact BEFORE INSERT ON fix_integrations
WHEN NOT EXISTS (SELECT 1 FROM fix_verifications fv JOIN fix_proposals p ON p.seq = fv.proposal_seq
        JOIN integrated_commits ic ON ic.integrated_id = NEW.integrated_id
        JOIN integration_operations o ON o.operation_id = ic.operation_id
        JOIN verified_results v ON v.result_id = o.verified_result_id
        JOIN integration_candidates c ON c.candidate_id = ic.candidate_id
        WHERE fv.seq = NEW.verification_seq AND fv.proposal_seq = NEW.proposal_seq AND v.submission_id = p.submission_id
            AND v.commit_oid = p.candidate_oid AND c.parent_verified = p.candidate_oid AND ic.commit_oid = NEW.commit_oid
            AND ic.created_unix_ms = NEW.integrated_unix_ms)
BEGIN SELECT RAISE(ABORT, 'a fix is integrated only by an integration of its exact verified candidate'); END;
-- Exact: every denominator (1..16) divides 720720 = lcm(1..16).
CREATE TRIGGER credit_shares_bound BEFORE INSERT ON credit_shares
WHEN (SELECT coalesce(sum(share_num * (720720 / share_den)), 0) FROM credit_shares WHERE seq = NEW.seq) + NEW.share_num * (720720 / NEW.share_den) > 720720
BEGIN SELECT RAISE(ABORT, 'the shares of one allocation sum to at most 1'); END;
UPDATE store_meta SET schema_version = 56;
PRAGMA user_version = 56;
