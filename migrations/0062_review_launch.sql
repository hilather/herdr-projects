-- Review launch (docs/telemetry/contracts-review.md §11, card D9). A review
-- runs as an ordinary canonical task attempt through the existing launch
-- path. What makes a task a review is one binding: its worker knowledge
-- snapshot, whose retained instructions are the blind review brief built
-- from the `present` view of one assigned opportunity. Reserving an attempt
-- of that task records the review session in the same transaction (the
-- session start is a shared-ledger row, §9). Delegated acceptance decisions
-- (§10) take the next `seq` of the same one ordering, so `--as-of` places
-- them. Metadata and digests only: the brief bytes are the snapshot's
-- retained instructions, never copied here. Every table is append-only.
CREATE TABLE review_briefs (
    snapshot_id TEXT PRIMARY KEY REFERENCES memory_snapshots(id),
    opportunity_id TEXT NOT NULL REFERENCES review_assignments(opportunity_id),
    task_id TEXT NOT NULL REFERENCES tasks(id),
    brief_schema TEXT NOT NULL CHECK (brief_schema = 'review_brief.v1'),
    brief_digest TEXT NOT NULL CHECK (length(brief_digest) = 71 AND substr(brief_digest, 1, 7) = 'sha256:'),
    prior_disclosure TEXT NOT NULL CHECK (prior_disclosure IN ('withheld', 'disclosed')),
    principal TEXT NOT NULL CHECK (length(principal) BETWEEN 1 AND 128),
    recorded_unix_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX review_briefs_by_task ON review_briefs(task_id);
CREATE INDEX review_briefs_by_opportunity ON review_briefs(opportunity_id);
CREATE TRIGGER review_briefs_no_update BEFORE UPDATE ON review_briefs BEGIN SELECT RAISE(ABORT, 'review launch records are append-only'); END;
CREATE TRIGGER review_briefs_no_delete BEFORE DELETE ON review_briefs BEGIN SELECT RAISE(ABORT, 'review launch records are append-only'); END;
-- One review task per opportunity and one opportunity per review task; the
-- snapshot belongs to that task, which is never the reviewed task itself.
CREATE TRIGGER review_briefs_one_task BEFORE INSERT ON review_briefs
WHEN EXISTS (SELECT 1 FROM review_briefs b WHERE (b.task_id = NEW.task_id) <> (b.opportunity_id = NEW.opportunity_id))
  OR NOT EXISTS (SELECT 1 FROM memory_snapshots s WHERE s.id = NEW.snapshot_id AND s.task_id = NEW.task_id)
  OR EXISTS (SELECT 1 FROM review_opportunities o WHERE o.opportunity_id = NEW.opportunity_id AND o.task_id = NEW.task_id)
BEGIN SELECT RAISE(ABORT, 'a review brief binds one snapshot of one review task to one opportunity, never the reviewed task'); END;

-- A session the controller recorded when it reserved the reviewing attempt,
-- with the brief snapshot that attempt was launched with.
CREATE TABLE review_session_launches (
    session_id TEXT PRIMARY KEY REFERENCES review_sessions(session_id),
    attempt_id TEXT NOT NULL UNIQUE REFERENCES attempts(id),
    snapshot_id TEXT NOT NULL REFERENCES review_briefs(snapshot_id)
) STRICT;
CREATE TRIGGER review_session_launches_no_update BEFORE UPDATE ON review_session_launches BEGIN SELECT RAISE(ABORT, 'review launch records are append-only'); END;
CREATE TRIGGER review_session_launches_no_delete BEFORE DELETE ON review_session_launches BEGIN SELECT RAISE(ABORT, 'review launch records are append-only'); END;
CREATE TRIGGER review_session_launches_match BEFORE INSERT ON review_session_launches
WHEN NOT EXISTS (SELECT 1 FROM review_sessions r JOIN review_briefs b ON b.opportunity_id = r.opportunity_id JOIN attempts a ON a.id = r.attempt_id
    WHERE r.session_id = NEW.session_id AND r.attempt_id = NEW.attempt_id AND b.snapshot_id = NEW.snapshot_id
      AND a.task_id = b.task_id AND a.snapshot = b.snapshot_id)
BEGIN SELECT RAISE(ABORT, 'a launched review session is its attempt''s session, launched with its opportunity''s brief'); END;

-- Delegated decisions in the one ordering: each decision is one row at the
-- next `seq` after the finding, fix, protocol, seed and review history.
-- Decisions recorded before this migration are sequenced after the head at
-- upgrade, in decision order, and marked `backfilled`: their true place is
-- unknown, so replay shows them at every watermark (the pre-0062 view).
CREATE TABLE review_decision_log (
    seq INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL UNIQUE REFERENCES review_acceptances(session_id),
    decision TEXT NOT NULL CHECK (decision IN ('accepted', 'rejected')),
    principal TEXT NOT NULL CHECK (length(principal) BETWEEN 1 AND 128),
    authority TEXT NOT NULL CHECK (authority = 'delegated_code_review.v1'),
    recorded_unix_ms INTEGER NOT NULL,
    backfilled INTEGER NOT NULL CHECK (backfilled IN (0, 1))
) STRICT;
CREATE TRIGGER review_decision_log_no_update BEFORE UPDATE ON review_decision_log BEGIN SELECT RAISE(ABORT, 'review history is append-only'); END;
CREATE TRIGGER review_decision_log_no_delete BEFORE DELETE ON review_decision_log BEGIN SELECT RAISE(ABORT, 'review history is append-only'); END;
CREATE TRIGGER review_decision_log_matches BEFORE INSERT ON review_decision_log
WHEN NOT EXISTS (SELECT 1 FROM review_acceptances a WHERE a.session_id = NEW.session_id AND a.decision = NEW.decision
    AND a.authority_principal = NEW.principal AND a.authority = NEW.authority AND a.decided_unix_ms = NEW.recorded_unix_ms)
BEGIN SELECT RAISE(ABORT, 'a decision history row repeats its recorded decision'); END;
-- Existing decisions, after the head, in decision order.
CREATE TEMP TABLE decision_backfill AS
    SELECT row_number() OVER (ORDER BY decided_unix_ms, session_id) AS n, session_id, decision, authority_principal, authority, decided_unix_ms FROM review_acceptances;
CREATE TEMP TABLE decision_backfill_head AS
    SELECT max(coalesce((SELECT max(seq) FROM finding_log), 0), coalesce((SELECT max(seq) FROM fix_log), 0),
        coalesce((SELECT max(seq) FROM protocol_log), 0), coalesce((SELECT max(seq) FROM seed_log), 0),
        coalesce((SELECT max(seq) FROM review_log), 0)) AS head;
INSERT INTO review_decision_log(seq, session_id, decision, principal, authority, recorded_unix_ms, backfilled)
    SELECT (SELECT head FROM decision_backfill_head) + n, session_id, decision, authority_principal, authority, decided_unix_ms, 1 FROM decision_backfill ORDER BY n;
DROP TABLE decision_backfill;
DROP TABLE decision_backfill_head;
CREATE TRIGGER review_decision_log_not_backfilled BEFORE INSERT ON review_decision_log
WHEN NEW.backfilled <> 0
BEGIN SELECT RAISE(ABORT, 'only the 0062 upgrade backfills review decisions'); END;
CREATE TRIGGER review_decision_log_one_order AFTER INSERT ON review_decision_log
WHEN NEW.seq <= max(coalesce((SELECT max(seq) FROM finding_log), 0), coalesce((SELECT max(seq) FROM fix_log), 0),
        coalesce((SELECT max(seq) FROM protocol_log), 0), coalesce((SELECT max(seq) FROM seed_log), 0),
        coalesce((SELECT max(seq) FROM review_log), 0))
  OR EXISTS (SELECT 1 FROM review_decision_log WHERE seq > NEW.seq)
BEGIN SELECT RAISE(ABORT, 'review, decision, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER finding_log_after_decisions AFTER INSERT ON finding_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_decision_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, decision, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER fix_log_after_decisions AFTER INSERT ON fix_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_decision_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, decision, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER protocol_log_after_decisions AFTER INSERT ON protocol_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_decision_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, decision, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER seed_log_after_decisions AFTER INSERT ON seed_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_decision_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, decision, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER review_log_after_decisions AFTER INSERT ON review_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_decision_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, decision, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
UPDATE store_meta SET schema_version = 62;
PRAGMA user_version = 62;
