-- Seeded-defect recall evaluation (TM3.6, docs/telemetry/contracts-review.md
-- §8). The evaluation authority (`operator:cli`, the project owner, recorded as
-- `evaluation_owner.v1`) registers a result submission as a seeded candidate
-- (with its seeds: class and a content-addressed reproducer reference, never
-- the seed itself) or as a clean control, before any review of it exists.
-- Detection is only the owner's link of a triaged claim of a review of that
-- candidate to a seed. Reveal comes only after every review of the candidate
-- has ended, and nothing reviews it afterwards. A seeded candidate never
-- integrates: no integration job, lease or operation can name it.
-- `seed_log` shares the one ordering of `finding_log`, `fix_log` and `protocol_log`, so
-- detections replay against triage to one watermark. Every row is append-only.
CREATE TABLE seed_log (
    seq INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('registered', 'detected', 'revealed', 'disposed', 'retracted')),
    principal TEXT NOT NULL CHECK (principal = 'operator:cli'),
    authority TEXT NOT NULL CHECK (authority = 'evaluation_owner.v1'),
    expected_seq INTEGER CHECK (expected_seq IS NULL OR expected_seq >= 0),
    recorded_unix_ms INTEGER NOT NULL
) STRICT;
-- One evaluation arm per submission, fixed before any review of it.
CREATE TABLE seeded_candidates (
    seq INTEGER PRIMARY KEY REFERENCES seed_log(seq),
    submission_id TEXT NOT NULL UNIQUE REFERENCES result_submissions(submission_id),
    candidate_oid TEXT NOT NULL CHECK (length(candidate_oid) IN (40, 64)),
    arm TEXT NOT NULL CHECK (arm IN ('seeded', 'clean_control')),
    reveal_policy TEXT NOT NULL CHECK (reveal_policy = 'reveal_after_close.v1')
) STRICT;
-- The seeds of a seeded candidate: class and reproducer reference only.
CREATE TABLE seeded_defects (
    seed_id INTEGER PRIMARY KEY,
    seq INTEGER NOT NULL REFERENCES seeded_candidates(seq),
    ordinal INTEGER NOT NULL CHECK (ordinal BETWEEN 1 AND 16),
    seed_class TEXT NOT NULL CHECK (seed_class IN ('logic', 'boundary', 'concurrency', 'security', 'test_weakening', 'requirement_omission')),
    reproducer_ref TEXT NOT NULL CHECK (length(reproducer_ref) = 71 AND substr(reproducer_ref, 1, 7) = 'sha256:'),
    UNIQUE (seq, ordinal)
) STRICT;
-- The owner's accepted link of one triaged claim to one seed.
CREATE TABLE seed_detections (
    seq INTEGER PRIMARY KEY REFERENCES seed_log(seq),
    seed_id INTEGER NOT NULL REFERENCES seeded_defects(seed_id),
    claim_id INTEGER NOT NULL REFERENCES finding_claims(claim_id),
    evidence_refs TEXT NOT NULL CHECK (json_valid(evidence_refs) AND json_type(evidence_refs) = 'array')
) STRICT;
CREATE TABLE seed_reveals (
    seq INTEGER PRIMARY KEY REFERENCES seed_log(seq),
    submission_id TEXT NOT NULL UNIQUE REFERENCES seeded_candidates(submission_id)
) STRICT;
CREATE TABLE seed_disposals (
    seq INTEGER PRIMARY KEY REFERENCES seed_log(seq),
    submission_id TEXT NOT NULL UNIQUE REFERENCES seed_reveals(submission_id),
    disposition TEXT NOT NULL CHECK (disposition IN ('discarded', 'repaired'))
) STRICT;
-- Reverses one detection recorded in error.
CREATE TABLE seed_retractions (
    seq INTEGER PRIMARY KEY REFERENCES seed_log(seq),
    reverses INTEGER NOT NULL UNIQUE REFERENCES seed_detections(seq),
    CHECK (reverses < seq)
) STRICT;
CREATE INDEX seeded_defects_by_candidate ON seeded_defects(seq, ordinal);
CREATE INDEX seed_detections_by_seed ON seed_detections(seed_id, seq);
-- One ordering with the finding, fix and protocol history.
CREATE TRIGGER seed_log_one_order AFTER INSERT ON seed_log
WHEN NEW.seq <= max(coalesce((SELECT max(seq) FROM finding_log), 0), coalesce((SELECT max(seq) FROM fix_log), 0), coalesce((SELECT max(seq) FROM protocol_log), 0))
  OR EXISTS (SELECT 1 FROM seed_log WHERE seq > NEW.seq)
BEGIN SELECT RAISE(ABORT, 'finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER finding_log_after_seeds AFTER INSERT ON finding_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM seed_log), 0)
BEGIN SELECT RAISE(ABORT, 'finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER fix_log_after_seeds AFTER INSERT ON fix_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM seed_log), 0)
BEGIN SELECT RAISE(ABORT, 'finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER protocol_log_after_seeds AFTER INSERT ON protocol_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM seed_log), 0)
BEGIN SELECT RAISE(ABORT, 'finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER seed_log_no_update BEFORE UPDATE ON seed_log BEGIN SELECT RAISE(ABORT, 'seed history is append-only'); END;
CREATE TRIGGER seed_log_no_delete BEFORE DELETE ON seed_log BEGIN SELECT RAISE(ABORT, 'seed history is append-only'); END;
CREATE TRIGGER seeded_candidates_no_update BEFORE UPDATE ON seeded_candidates BEGIN SELECT RAISE(ABORT, 'seed history is append-only'); END;
CREATE TRIGGER seeded_candidates_no_delete BEFORE DELETE ON seeded_candidates BEGIN SELECT RAISE(ABORT, 'seed history is append-only'); END;
CREATE TRIGGER seeded_defects_no_update BEFORE UPDATE ON seeded_defects BEGIN SELECT RAISE(ABORT, 'seed history is append-only'); END;
CREATE TRIGGER seeded_defects_no_delete BEFORE DELETE ON seeded_defects BEGIN SELECT RAISE(ABORT, 'seed history is append-only'); END;
CREATE TRIGGER seed_detections_no_update BEFORE UPDATE ON seed_detections BEGIN SELECT RAISE(ABORT, 'seed history is append-only'); END;
CREATE TRIGGER seed_detections_no_delete BEFORE DELETE ON seed_detections BEGIN SELECT RAISE(ABORT, 'seed history is append-only'); END;
CREATE TRIGGER seed_reveals_no_update BEFORE UPDATE ON seed_reveals BEGIN SELECT RAISE(ABORT, 'seed history is append-only'); END;
CREATE TRIGGER seed_reveals_no_delete BEFORE DELETE ON seed_reveals BEGIN SELECT RAISE(ABORT, 'seed history is append-only'); END;
CREATE TRIGGER seed_disposals_no_update BEFORE UPDATE ON seed_disposals BEGIN SELECT RAISE(ABORT, 'seed history is append-only'); END;
CREATE TRIGGER seed_disposals_no_delete BEFORE DELETE ON seed_disposals BEGIN SELECT RAISE(ABORT, 'seed history is append-only'); END;
CREATE TRIGGER seed_retractions_no_update BEFORE UPDATE ON seed_retractions BEGIN SELECT RAISE(ABORT, 'seed history is append-only'); END;
CREATE TRIGGER seed_retractions_no_delete BEFORE DELETE ON seed_retractions BEGIN SELECT RAISE(ABORT, 'seed history is append-only'); END;
-- Each detail row needs its own history row of the matching kind.
CREATE TRIGGER seeded_candidates_kind BEFORE INSERT ON seeded_candidates
WHEN NOT EXISTS (SELECT 1 FROM seed_log l WHERE l.seq = NEW.seq AND l.kind = 'registered')
BEGIN SELECT RAISE(ABORT, 'a seeded candidate needs its own registered history row'); END;
CREATE TRIGGER seed_detections_kind BEFORE INSERT ON seed_detections
WHEN NOT EXISTS (SELECT 1 FROM seed_log l WHERE l.seq = NEW.seq AND l.kind = 'detected')
BEGIN SELECT RAISE(ABORT, 'a seed detection needs its own detected history row'); END;
CREATE TRIGGER seed_reveals_kind BEFORE INSERT ON seed_reveals
WHEN NOT EXISTS (SELECT 1 FROM seed_log l WHERE l.seq = NEW.seq AND l.kind = 'revealed')
BEGIN SELECT RAISE(ABORT, 'a reveal needs its own revealed history row'); END;
CREATE TRIGGER seed_disposals_kind BEFORE INSERT ON seed_disposals
WHEN NOT EXISTS (SELECT 1 FROM seed_log l WHERE l.seq = NEW.seq AND l.kind = 'disposed')
BEGIN SELECT RAISE(ABORT, 'a disposal needs its own disposed history row'); END;
CREATE TRIGGER seed_retractions_kind BEFORE INSERT ON seed_retractions
WHEN NOT EXISTS (SELECT 1 FROM seed_log l WHERE l.seq = NEW.seq AND l.kind = 'retracted')
BEGIN SELECT RAISE(ABORT, 'a retraction needs its own retracted history row'); END;
-- The arm is fixed on the exact candidate, before any review or integration of it.
CREATE TRIGGER seeded_candidates_exact BEFORE INSERT ON seeded_candidates
WHEN NOT EXISTS (SELECT 1 FROM result_submissions s WHERE s.submission_id = NEW.submission_id AND s.candidate_oid = NEW.candidate_oid)
BEGIN SELECT RAISE(ABORT, 'a seeded candidate is the exact candidate of its submission'); END;
CREATE TRIGGER seeded_candidates_before_review BEFORE INSERT ON seeded_candidates
WHEN EXISTS (SELECT 1 FROM review_opportunities o WHERE o.submission_id = NEW.submission_id)
BEGIN SELECT RAISE(ABORT, 'an evaluation arm is registered before any review of the candidate'); END;
CREATE TRIGGER seeded_candidates_before_integration BEFORE INSERT ON seeded_candidates
WHEN EXISTS (SELECT 1 FROM operations o WHERE o.kind = 'integration.run' AND json_extract(o.payload, '$.submission_id') = NEW.submission_id)
  OR EXISTS (SELECT 1 FROM verified_results r JOIN integration_operations i ON i.verified_result_id = r.result_id WHERE r.submission_id = NEW.submission_id)
BEGIN SELECT RAISE(ABORT, 'a candidate with an integration job or operation cannot be registered for evaluation'); END;
CREATE TRIGGER seeded_defects_only_seeded BEFORE INSERT ON seeded_defects
WHEN NOT EXISTS (SELECT 1 FROM seeded_candidates c WHERE c.seq = NEW.seq AND c.arm = 'seeded')
BEGIN SELECT RAISE(ABORT, 'seeds belong to a seeded candidate, registered with it'); END;
CREATE TRIGGER seeded_defects_with_registration BEFORE INSERT ON seeded_defects
WHEN NEW.seq <> (SELECT max(seq) FROM seed_log)
BEGIN SELECT RAISE(ABORT, 'seeds are recorded with their candidate registration'); END;
-- A detection links a claim of a review of the seed's own candidate.
CREATE TRIGGER seed_detections_same_candidate BEFORE INSERT ON seed_detections
WHEN NOT EXISTS (SELECT 1 FROM seeded_defects d JOIN seeded_candidates c ON c.seq = d.seq
    JOIN finding_claims k ON k.claim_id = NEW.claim_id JOIN finding_submissions f ON f.submission_id = k.submission_id
    JOIN review_sessions r ON r.session_id = f.session_id JOIN review_opportunities o ON o.opportunity_id = r.opportunity_id
    WHERE d.seed_id = NEW.seed_id AND o.submission_id = c.submission_id)
BEGIN SELECT RAISE(ABORT, 'a seed is detected only by a claim of a review of its own candidate'); END;
-- Reveal only once every review of the candidate has ended; nothing reviews it afterwards.
CREATE TRIGGER seed_reveals_after_close BEFORE INSERT ON seed_reveals
WHEN NOT EXISTS (SELECT 1 FROM review_opportunities o WHERE o.submission_id = NEW.submission_id)
  OR EXISTS (SELECT 1 FROM review_opportunities o WHERE o.submission_id = NEW.submission_id
      AND NOT EXISTS (SELECT 1 FROM review_sessions r JOIN review_completions c ON c.session_id = r.session_id WHERE r.opportunity_id = o.opportunity_id))
  OR EXISTS (SELECT 1 FROM review_opportunities o JOIN review_sessions r ON r.opportunity_id = o.opportunity_id
      LEFT JOIN review_completions c ON c.session_id = r.session_id WHERE o.submission_id = NEW.submission_id AND c.session_id IS NULL)
BEGIN SELECT RAISE(ABORT, 'a seed is revealed only after every review of its candidate has ended'); END;
CREATE TRIGGER review_opportunities_not_after_reveal BEFORE INSERT ON review_opportunities
WHEN EXISTS (SELECT 1 FROM seed_reveals v WHERE v.submission_id = NEW.submission_id)
BEGIN SELECT RAISE(ABORT, 'a revealed evaluation candidate is not reviewed again'); END;
CREATE TRIGGER review_sessions_not_after_reveal BEFORE INSERT ON review_sessions
WHEN EXISTS (SELECT 1 FROM review_opportunities o JOIN seed_reveals v ON v.submission_id = o.submission_id WHERE o.opportunity_id = NEW.opportunity_id)
BEGIN SELECT RAISE(ABORT, 'a revealed evaluation candidate is not reviewed again'); END;
-- Integration guard: no integration job, lease or operation for a seeded candidate.
CREATE TRIGGER seeded_candidate_no_integration_operation BEFORE INSERT ON integration_operations
WHEN EXISTS (SELECT 1 FROM verified_results r JOIN seeded_candidates c ON c.submission_id = r.submission_id
    WHERE r.result_id = NEW.verified_result_id AND c.arm = 'seeded')
BEGIN SELECT RAISE(ABORT, 'a seeded candidate never integrates'); END;
CREATE TRIGGER seeded_candidate_no_integration_job BEFORE INSERT ON operations
WHEN NEW.kind = 'integration.run' AND EXISTS (SELECT 1 FROM seeded_candidates c WHERE c.submission_id = json_extract(NEW.payload, '$.submission_id') AND c.arm = 'seeded')
BEGIN SELECT RAISE(ABORT, 'a seeded candidate never integrates'); END;
CREATE TRIGGER seeded_candidate_no_integration_lease BEFORE INSERT ON operations
WHEN NEW.kind = 'integration.lease' AND EXISTS (SELECT 1 FROM verified_results r JOIN seeded_candidates c ON c.submission_id = r.submission_id
    WHERE r.result_id = json_extract(NEW.payload, '$.result_id') AND c.arm = 'seeded')
BEGIN SELECT RAISE(ABORT, 'a seeded candidate never integrates'); END;
UPDATE store_meta SET schema_version = 58;
PRAGMA user_version = 58;
