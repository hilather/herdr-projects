-- Finding triage and duplicate history (TM3.2, docs/telemetry/contracts-review.md
-- §5). Every change is one append-only row of `finding_log`, whose `seq` is the
-- replay watermark: submissions arriving from review completions, claim splits
-- and restores, triage decisions and merge/unmerge corrections. Current and
-- historical ("as of seq") states are derived from the log; nothing is updated
-- or deleted. A submission is the reviewer's proposal; only the triage
-- authority (`operator:cli`, the project owner) writes decisions and
-- corrections. Titles are contracts §7 excerpts (≤160); no finding text,
-- diff or source is stored.
CREATE TABLE finding_log (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL CHECK (kind IN ('submitted', 'decided', 'split', 'restored', 'merged', 'unmerged')),
    principal TEXT NOT NULL CHECK (length(principal) BETWEEN 1 AND 128),
    authority TEXT NOT NULL CHECK (authority IN ('proposal', 'operator_owner.v1')),
    expected_seq INTEGER CHECK (expected_seq IS NULL OR expected_seq >= 0),
    recorded_unix_ms INTEGER NOT NULL,
    CHECK ((kind = 'submitted') = (authority = 'proposal')),
    CHECK (authority = 'proposal' OR principal = 'operator:cli')
) STRICT;
-- One submission per finding reference of a review completion.
CREATE TABLE finding_submissions (
    submission_id INTEGER PRIMARY KEY,
    seq INTEGER NOT NULL UNIQUE REFERENCES finding_log(seq),
    session_id TEXT NOT NULL REFERENCES review_completions(session_id),
    finding_ref TEXT NOT NULL CHECK (length(finding_ref) BETWEEN 9 AND 72 AND substr(finding_ref, 1, 8) = 'finding:'),
    title TEXT CHECK (title IS NULL OR length(title) BETWEEN 1 AND 160),
    source TEXT NOT NULL CHECK (source = 'review_receipt'),
    trust TEXT NOT NULL CHECK (trust = 'proposal'),
    UNIQUE (session_id, finding_ref)
) STRICT;
-- Revisioned claim sets beneath an unchanged submission: `initial` (one
-- claim), `split` (two or more), `restore` (an earlier revision's claims again).
CREATE TABLE finding_claim_sets (
    seq INTEGER PRIMARY KEY REFERENCES finding_log(seq),
    submission_id INTEGER NOT NULL REFERENCES finding_submissions(submission_id),
    revision INTEGER NOT NULL CHECK (revision > 0),
    kind TEXT NOT NULL CHECK (kind IN ('initial', 'split', 'restore')),
    restores INTEGER CHECK (restores IS NULL OR (restores > 0 AND restores < revision)),
    CHECK ((kind = 'restore') = (restores IS NOT NULL)),
    CHECK ((kind = 'initial') = (revision = 1)),
    UNIQUE (submission_id, revision)
) STRICT;
CREATE TABLE finding_claims (
    claim_id INTEGER PRIMARY KEY,
    submission_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal BETWEEN 1 AND 32),
    title TEXT CHECK (title IS NULL OR length(title) BETWEEN 1 AND 160),
    UNIQUE (submission_id, revision, ordinal),
    FOREIGN KEY (submission_id, revision) REFERENCES finding_claim_sets(submission_id, revision)
) STRICT;
-- Canonical finding identity, minted by a `validated` decision.
CREATE TABLE canonical_findings (
    finding_id TEXT PRIMARY KEY CHECK (length(finding_id) BETWEEN 9 AND 72 AND substr(finding_id, 1, 8) = 'finding:'),
    seq INTEGER NOT NULL UNIQUE REFERENCES finding_log(seq),
    title TEXT CHECK (title IS NULL OR length(title) BETWEEN 1 AND 160)
) STRICT;
-- A claim's triage decision; a later decision on the same claim supersedes it.
CREATE TABLE finding_decisions (
    seq INTEGER PRIMARY KEY REFERENCES finding_log(seq),
    claim_id INTEGER NOT NULL REFERENCES finding_claims(claim_id),
    outcome TEXT NOT NULL CHECK (outcome IN ('pending', 'validated', 'rejected', 'duplicate')),
    finding_id TEXT REFERENCES canonical_findings(finding_id),
    reason TEXT CHECK (reason IS NULL OR length(reason) BETWEEN 1 AND 64),
    severity TEXT CHECK (severity IS NULL OR severity IN ('critical', 'high', 'medium', 'low', 'informational')),
    severity_policy TEXT CHECK (severity_policy IS NULL OR severity_policy = 'finding_severity.v1'),
    evidence_refs TEXT NOT NULL CHECK (json_valid(evidence_refs) AND json_array_length(evidence_refs) <= 64),
    supersedes INTEGER REFERENCES finding_decisions(seq),
    CHECK ((outcome IN ('validated', 'duplicate')) = (finding_id IS NOT NULL)),
    CHECK ((outcome = 'validated') = (severity IS NOT NULL AND severity_policy IS NOT NULL)),
    CHECK (outcome <> 'validated' OR json_array_length(evidence_refs) >= 1),
    CHECK ((outcome = 'rejected') = coalesce(reason IN ('insufficient_evidence', 'intended_behavior', 'out_of_scope'), 0)),
    CHECK (outcome <> 'pending' OR reason IS NULL OR reason IN ('reopened', 'decided_in_error')),
    CHECK (outcome IN ('rejected', 'pending') OR reason IS NULL)
) STRICT;
CREATE INDEX finding_decisions_by_claim ON finding_decisions(claim_id, seq);
-- Merge `source` into `target`; an unmerge reverses exactly one merge.
CREATE TABLE finding_relationships (
    seq INTEGER PRIMARY KEY REFERENCES finding_log(seq),
    kind TEXT NOT NULL CHECK (kind IN ('merge', 'unmerge')),
    source_finding TEXT NOT NULL REFERENCES canonical_findings(finding_id),
    target_finding TEXT NOT NULL REFERENCES canonical_findings(finding_id),
    reverses INTEGER UNIQUE REFERENCES finding_relationships(seq),
    CHECK (source_finding <> target_finding),
    CHECK ((kind = 'unmerge') = (reverses IS NOT NULL))
) STRICT;
CREATE TRIGGER finding_log_no_update BEFORE UPDATE ON finding_log BEGIN SELECT RAISE(ABORT, 'finding history is append-only'); END;
CREATE TRIGGER finding_log_no_delete BEFORE DELETE ON finding_log BEGIN SELECT RAISE(ABORT, 'finding history is append-only'); END;
CREATE TRIGGER finding_submissions_no_update BEFORE UPDATE ON finding_submissions BEGIN SELECT RAISE(ABORT, 'finding history is append-only'); END;
CREATE TRIGGER finding_submissions_no_delete BEFORE DELETE ON finding_submissions BEGIN SELECT RAISE(ABORT, 'finding history is append-only'); END;
CREATE TRIGGER finding_claim_sets_no_update BEFORE UPDATE ON finding_claim_sets BEGIN SELECT RAISE(ABORT, 'finding history is append-only'); END;
CREATE TRIGGER finding_claim_sets_no_delete BEFORE DELETE ON finding_claim_sets BEGIN SELECT RAISE(ABORT, 'finding history is append-only'); END;
CREATE TRIGGER finding_claims_no_update BEFORE UPDATE ON finding_claims BEGIN SELECT RAISE(ABORT, 'finding history is append-only'); END;
CREATE TRIGGER finding_claims_no_delete BEFORE DELETE ON finding_claims BEGIN SELECT RAISE(ABORT, 'finding history is append-only'); END;
CREATE TRIGGER canonical_findings_no_update BEFORE UPDATE ON canonical_findings BEGIN SELECT RAISE(ABORT, 'finding history is append-only'); END;
CREATE TRIGGER canonical_findings_no_delete BEFORE DELETE ON canonical_findings BEGIN SELECT RAISE(ABORT, 'finding history is append-only'); END;
CREATE TRIGGER finding_decisions_no_update BEFORE UPDATE ON finding_decisions BEGIN SELECT RAISE(ABORT, 'finding history is append-only'); END;
CREATE TRIGGER finding_decisions_no_delete BEFORE DELETE ON finding_decisions BEGIN SELECT RAISE(ABORT, 'finding history is append-only'); END;
CREATE TRIGGER finding_relationships_no_update BEFORE UPDATE ON finding_relationships BEGIN SELECT RAISE(ABORT, 'finding history is append-only'); END;
CREATE TRIGGER finding_relationships_no_delete BEFORE DELETE ON finding_relationships BEGIN SELECT RAISE(ABORT, 'finding history is append-only'); END;
-- Authority: a decision, mint or correction row must point at a log row of
-- the owner's triage authority; a submission at a proposal row.
CREATE TRIGGER finding_decisions_authority BEFORE INSERT ON finding_decisions
WHEN NOT EXISTS (SELECT 1 FROM finding_log l WHERE l.seq = NEW.seq AND l.kind = 'decided' AND l.authority = 'operator_owner.v1')
BEGIN SELECT RAISE(ABORT, 'finding triage needs the triage authority'); END;
CREATE TRIGGER canonical_findings_authority BEFORE INSERT ON canonical_findings
WHEN NOT EXISTS (SELECT 1 FROM finding_log l WHERE l.seq = NEW.seq AND l.kind = 'decided' AND l.authority = 'operator_owner.v1')
BEGIN SELECT RAISE(ABORT, 'finding triage needs the triage authority'); END;
CREATE TRIGGER finding_relationships_authority BEFORE INSERT ON finding_relationships
WHEN NOT EXISTS (SELECT 1 FROM finding_log l WHERE l.seq = NEW.seq AND l.kind = CASE NEW.kind WHEN 'merge' THEN 'merged' ELSE 'unmerged' END
        AND l.authority = 'operator_owner.v1')
BEGIN SELECT RAISE(ABORT, 'finding triage needs the triage authority'); END;
CREATE TRIGGER finding_claim_sets_authority BEFORE INSERT ON finding_claim_sets
WHEN NOT EXISTS (SELECT 1 FROM finding_log l WHERE l.seq = NEW.seq
        AND l.kind = CASE NEW.kind WHEN 'initial' THEN 'submitted' WHEN 'split' THEN 'split' ELSE 'restored' END)
BEGIN SELECT RAISE(ABORT, 'finding claim revision needs its history row'); END;
CREATE TRIGGER finding_submissions_proposal BEFORE INSERT ON finding_submissions
WHEN NOT EXISTS (SELECT 1 FROM finding_log l WHERE l.seq = NEW.seq AND l.kind = 'submitted')
    OR NOT EXISTS (SELECT 1 FROM review_completions c, json_each(c.finding_refs) j WHERE c.session_id = NEW.session_id AND j.value = NEW.finding_ref)
BEGIN SELECT RAISE(ABORT, 'a finding submission must be a finding reference of its review completion'); END;
-- Existing completions: one submission per finding reference, in completion order.
CREATE TEMP TABLE finding_backfill AS
    SELECT row_number() OVER (ORDER BY c.completed_unix_ms, c.rowid, j.value) AS n, c.session_id AS session_id, j.value AS finding_ref,
        c.recorder_principal AS principal, c.completed_unix_ms AS at
    FROM review_completions c, json_each(c.finding_refs) j;
INSERT INTO finding_log(seq, kind, principal, authority, expected_seq, recorded_unix_ms)
    SELECT n, 'submitted', principal, 'proposal', NULL, at FROM finding_backfill ORDER BY n;
INSERT INTO finding_submissions(submission_id, seq, session_id, finding_ref, title, source, trust)
    SELECT n, n, session_id, finding_ref, NULL, 'review_receipt', 'proposal' FROM finding_backfill ORDER BY n;
INSERT INTO finding_claim_sets(seq, submission_id, revision, kind, restores) SELECT n, n, 1, 'initial', NULL FROM finding_backfill ORDER BY n;
INSERT INTO finding_claims(claim_id, submission_id, revision, ordinal, title) SELECT n, n, 1, 1, NULL FROM finding_backfill ORDER BY n;
DROP TABLE finding_backfill;
UPDATE store_meta SET schema_version = 55;
PRAGMA user_version = 55;
