-- Review opportunities and assignments in the shared ledger
-- (docs/telemetry/contracts-review.md §9, card D11). Opening an opportunity
-- and assigning its reviewer each take the next `seq` of the one ordering of
-- `finding_log`, `fix_log`, `protocol_log`, `seed_log`, `review_log` and
-- `review_decision_log`, in the opening's or the assignment's transaction, so
-- `--as-of` views list an opportunity only from its opening and show its
-- assignment only from its assignment. Opportunities and assignments recorded
-- before this migration are sequenced after the head at upgrade, in time order
-- (an opening before its own assignment), and marked `backfilled`: their true
-- place among earlier rows is unknown, so replay keeps them visible at every
-- watermark (the pre-0063 view). Every row is append-only.
CREATE TABLE review_opportunity_log (
    seq INTEGER PRIMARY KEY,
    opportunity_id TEXT NOT NULL REFERENCES review_opportunities(opportunity_id),
    event TEXT NOT NULL CHECK (event IN ('opened', 'assigned')),
    principal TEXT NOT NULL CHECK (length(principal) BETWEEN 1 AND 128),
    authority TEXT NOT NULL CHECK (authority = 'review_capture.v1'),
    recorded_unix_ms INTEGER NOT NULL,
    backfilled INTEGER NOT NULL CHECK (backfilled IN (0, 1)),
    UNIQUE (opportunity_id, event)
) STRICT;
CREATE TRIGGER review_opportunity_log_no_update BEFORE UPDATE ON review_opportunity_log BEGIN SELECT RAISE(ABORT, 'review history is append-only'); END;
CREATE TRIGGER review_opportunity_log_no_delete BEFORE DELETE ON review_opportunity_log BEGIN SELECT RAISE(ABORT, 'review history is append-only'); END;
-- A row repeats its recorded opening or assignment; an assignment follows its opening.
CREATE TRIGGER review_opportunity_log_matches BEFORE INSERT ON review_opportunity_log
WHEN (NEW.event = 'opened' AND NOT EXISTS (SELECT 1 FROM review_opportunities o WHERE o.opportunity_id = NEW.opportunity_id
        AND o.creator_principal = NEW.principal AND o.created_unix_ms = NEW.recorded_unix_ms))
  OR (NEW.event = 'assigned' AND (NOT EXISTS (SELECT 1 FROM review_assignments a WHERE a.opportunity_id = NEW.opportunity_id
        AND a.assigner_principal = NEW.principal AND a.assigned_unix_ms = NEW.recorded_unix_ms)
      OR NOT EXISTS (SELECT 1 FROM review_opportunity_log l WHERE l.opportunity_id = NEW.opportunity_id AND l.event = 'opened' AND l.seq < NEW.seq)))
BEGIN SELECT RAISE(ABORT, 'an opportunity history row repeats its recorded opening or assignment, and an assignment follows its opening'); END;
-- Existing opportunities and assignments, after the head, in time order (an
-- assignment never before its own opening, whatever its recorded time).
CREATE TEMP TABLE opportunity_backfill AS
    SELECT row_number() OVER (ORDER BY at, phase, opportunity_id) AS n, opportunity_id, event, principal, recorded FROM (
        SELECT opportunity_id, 'opened' AS event, 0 AS phase, creator_principal AS principal, created_unix_ms AS at, created_unix_ms AS recorded FROM review_opportunities
        UNION ALL
        SELECT a.opportunity_id, 'assigned', 1, a.assigner_principal, max(a.assigned_unix_ms, o.created_unix_ms), a.assigned_unix_ms
        FROM review_assignments a JOIN review_opportunities o ON o.opportunity_id = a.opportunity_id);
CREATE TEMP TABLE opportunity_backfill_head AS
    SELECT max(coalesce((SELECT max(seq) FROM finding_log), 0), coalesce((SELECT max(seq) FROM fix_log), 0),
        coalesce((SELECT max(seq) FROM protocol_log), 0), coalesce((SELECT max(seq) FROM seed_log), 0),
        coalesce((SELECT max(seq) FROM review_log), 0), coalesce((SELECT max(seq) FROM review_decision_log), 0)) AS head;
INSERT INTO review_opportunity_log(seq, opportunity_id, event, principal, authority, recorded_unix_ms, backfilled)
    SELECT (SELECT head FROM opportunity_backfill_head) + n, opportunity_id, event, principal, 'review_capture.v1', recorded, 1 FROM opportunity_backfill ORDER BY n;
DROP TABLE opportunity_backfill;
DROP TABLE opportunity_backfill_head;
CREATE TRIGGER review_opportunity_log_not_backfilled BEFORE INSERT ON review_opportunity_log
WHEN NEW.backfilled <> 0
BEGIN SELECT RAISE(ABORT, 'only the 0063 upgrade backfills review opportunities'); END;
-- One ordering with every other ledger of the review history.
CREATE TRIGGER review_opportunity_log_one_order AFTER INSERT ON review_opportunity_log
WHEN NEW.seq <= max(coalesce((SELECT max(seq) FROM finding_log), 0), coalesce((SELECT max(seq) FROM fix_log), 0),
        coalesce((SELECT max(seq) FROM protocol_log), 0), coalesce((SELECT max(seq) FROM seed_log), 0),
        coalesce((SELECT max(seq) FROM review_log), 0), coalesce((SELECT max(seq) FROM review_decision_log), 0))
  OR EXISTS (SELECT 1 FROM review_opportunity_log WHERE seq > NEW.seq)
BEGIN SELECT RAISE(ABORT, 'review, opportunity, decision, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER finding_log_after_opportunities AFTER INSERT ON finding_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_opportunity_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, opportunity, decision, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER fix_log_after_opportunities AFTER INSERT ON fix_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_opportunity_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, opportunity, decision, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER protocol_log_after_opportunities AFTER INSERT ON protocol_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_opportunity_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, opportunity, decision, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER seed_log_after_opportunities AFTER INSERT ON seed_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_opportunity_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, opportunity, decision, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER review_log_after_opportunities AFTER INSERT ON review_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_opportunity_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, opportunity, decision, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER review_decision_log_after_opportunities AFTER INSERT ON review_decision_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_opportunity_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, opportunity, decision, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
-- A new session starts only on an opportunity whose assignment is in the ledger.
CREATE TRIGGER review_session_events_after_assignment BEFORE INSERT ON review_session_events
WHEN NEW.event = 'started' AND NOT EXISTS (SELECT 1 FROM review_sessions r JOIN review_opportunity_log l ON l.opportunity_id = r.opportunity_id
    WHERE r.session_id = NEW.session_id AND l.event = 'assigned' AND l.seq < NEW.seq)
BEGIN SELECT RAISE(ABORT, 'a review session starts after its opportunity''s assignment'); END;
UPDATE store_meta SET schema_version = 63;
PRAGMA user_version = 63;
