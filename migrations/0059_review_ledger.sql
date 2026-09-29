-- Review lifecycle in the shared ledger (docs/telemetry/contracts-review.md
-- §9). `review_log` takes the next `seq` of the one ordering of `finding_log`,
-- `fix_log`, `protocol_log` and `seed_log`: a review session's start and its
-- completion are each one row, so `--as-of` views replay review status (not
-- only triage) to a watermark. The completion row precedes the finding
-- submissions written in its transaction. The owner's corrections of the
-- protocol registry (a retracted pass binding or unit exclusion) are rows of
-- the same log. Sessions and completions recorded before this migration are
-- sequenced after the head at upgrade, in start/completion order, and marked
-- `backfilled`: their true place among earlier rows is unknown, so replay
-- keeps treating them as present at every watermark. Every row is append-only.
CREATE TABLE review_log (
    seq INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('started', 'completed', 'pass_retracted', 'exclusion_retracted')),
    principal TEXT NOT NULL CHECK (length(principal) BETWEEN 1 AND 128),
    authority TEXT NOT NULL CHECK (authority IN ('review_capture.v1', 'operator_owner.v1')),
    expected_seq INTEGER CHECK (expected_seq IS NULL OR expected_seq >= 0),
    recorded_unix_ms INTEGER NOT NULL,
    CHECK ((kind IN ('started', 'completed')) = (authority = 'review_capture.v1')),
    CHECK (authority = 'review_capture.v1' OR principal = 'operator:cli')
) STRICT;
-- A session's start and its completion, each at its own ledger row.
CREATE TABLE review_session_events (
    seq INTEGER PRIMARY KEY REFERENCES review_log(seq),
    session_id TEXT NOT NULL REFERENCES review_sessions(session_id),
    event TEXT NOT NULL CHECK (event IN ('started', 'completed')),
    backfilled INTEGER NOT NULL CHECK (backfilled IN (0, 1)),
    UNIQUE (session_id, event)
) STRICT;
-- Reverses one pass binding or one unit exclusion recorded in error.
CREATE TABLE protocol_retractions (
    seq INTEGER PRIMARY KEY REFERENCES review_log(seq),
    reverses INTEGER NOT NULL UNIQUE REFERENCES protocol_log(seq),
    CHECK (reverses < seq)
) STRICT;
CREATE INDEX review_session_events_by_session ON review_session_events(session_id, event);
-- One ordering with the finding, fix, protocol and seed history.
CREATE TRIGGER review_log_one_order AFTER INSERT ON review_log
WHEN NEW.seq <= max(coalesce((SELECT max(seq) FROM finding_log), 0), coalesce((SELECT max(seq) FROM fix_log), 0),
        coalesce((SELECT max(seq) FROM protocol_log), 0), coalesce((SELECT max(seq) FROM seed_log), 0))
  OR EXISTS (SELECT 1 FROM review_log WHERE seq > NEW.seq)
BEGIN SELECT RAISE(ABORT, 'review, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER review_log_no_update BEFORE UPDATE ON review_log BEGIN SELECT RAISE(ABORT, 'review history is append-only'); END;
CREATE TRIGGER review_log_no_delete BEFORE DELETE ON review_log BEGIN SELECT RAISE(ABORT, 'review history is append-only'); END;
CREATE TRIGGER review_session_events_no_update BEFORE UPDATE ON review_session_events BEGIN SELECT RAISE(ABORT, 'review history is append-only'); END;
CREATE TRIGGER review_session_events_no_delete BEFORE DELETE ON review_session_events BEGIN SELECT RAISE(ABORT, 'review history is append-only'); END;
CREATE TRIGGER protocol_retractions_no_update BEFORE UPDATE ON protocol_retractions BEGIN SELECT RAISE(ABORT, 'review history is append-only'); END;
CREATE TRIGGER protocol_retractions_no_delete BEFORE DELETE ON protocol_retractions BEGIN SELECT RAISE(ABORT, 'review history is append-only'); END;
-- Each detail row needs its own history row of the matching kind; a
-- completion follows its session's start and its recorded completion.
CREATE TRIGGER review_session_events_kind BEFORE INSERT ON review_session_events
WHEN NOT EXISTS (SELECT 1 FROM review_log l WHERE l.seq = NEW.seq AND l.kind = NEW.event)
  OR (NEW.event = 'completed' AND (NOT EXISTS (SELECT 1 FROM review_completions c WHERE c.session_id = NEW.session_id)
      OR NOT EXISTS (SELECT 1 FROM review_session_events s WHERE s.session_id = NEW.session_id AND s.event = 'started' AND s.seq < NEW.seq)))
BEGIN SELECT RAISE(ABORT, 'a review session event needs its history row, and a completion follows its start'); END;
CREATE TRIGGER protocol_retractions_kind BEFORE INSERT ON protocol_retractions
WHEN NOT EXISTS (SELECT 1 FROM review_log l JOIN protocol_log p ON p.seq = NEW.reverses WHERE l.seq = NEW.seq
    AND ((l.kind = 'pass_retracted' AND p.kind = 'pass_bound') OR (l.kind = 'exclusion_retracted' AND p.kind = 'unit_excluded')))
BEGIN SELECT RAISE(ABORT, 'a retraction reverses one pass binding or unit exclusion with its own history row'); END;
-- A `planned_units` experiment stops assigning at its planned number of units.
CREATE TRIGGER experiment_units_planned_stop BEFORE INSERT ON experiment_units
WHEN EXISTS (SELECT 1 FROM review_experiments e WHERE e.seq = NEW.experiment_seq
    AND json_extract(e.canonical_json, '$.stopping_rule') = 'planned_units' AND json_type(e.canonical_json, '$.planned_units') = 'integer'
    AND (SELECT count(*) FROM experiment_units u WHERE u.experiment_seq = e.seq) >= json_extract(e.canonical_json, '$.planned_units'))
BEGIN SELECT RAISE(ABORT, 'the experiment reached its planned units: its stopping rule ends assignment'); END;
-- Existing sessions and completions, after the head, in start/completion
-- order (a completion never before its own session's start).
CREATE TEMP TABLE review_backfill AS
    SELECT row_number() OVER (ORDER BY at, phase, session_id) AS n, session_id, event, principal, at FROM (
        SELECT session_id, 'started' AS event, 0 AS phase, recorder_principal AS principal, started_unix_ms AS at FROM review_sessions
        UNION ALL
        SELECT c.session_id, 'completed', 1, c.recorder_principal, max(c.completed_unix_ms, s.started_unix_ms)
        FROM review_completions c JOIN review_sessions s ON s.session_id = c.session_id);
CREATE TEMP TABLE review_backfill_head AS
    SELECT max(coalesce((SELECT max(seq) FROM finding_log), 0), coalesce((SELECT max(seq) FROM fix_log), 0),
        coalesce((SELECT max(seq) FROM protocol_log), 0), coalesce((SELECT max(seq) FROM seed_log), 0)) AS head;
INSERT INTO review_log(seq, kind, principal, authority, expected_seq, recorded_unix_ms)
    SELECT (SELECT head FROM review_backfill_head) + n, event, principal, 'review_capture.v1', NULL, at FROM review_backfill ORDER BY n;
INSERT INTO review_session_events(seq, session_id, event, backfilled)
    SELECT (SELECT head FROM review_backfill_head) + n, session_id, event, 1 FROM review_backfill ORDER BY n;
DROP TABLE review_backfill;
DROP TABLE review_backfill_head;
-- Only the backfill above is marked `backfilled`; the other ledgers follow this one too.
CREATE TRIGGER review_session_events_not_backfilled BEFORE INSERT ON review_session_events
WHEN NEW.backfilled <> 0
BEGIN SELECT RAISE(ABORT, 'only the 0059 upgrade backfills review session events'); END;
CREATE TRIGGER finding_log_after_reviews AFTER INSERT ON finding_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER fix_log_after_reviews AFTER INSERT ON fix_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER protocol_log_after_reviews AFTER INSERT ON protocol_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER seed_log_after_reviews AFTER INSERT ON seed_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM review_log), 0)
BEGIN SELECT RAISE(ABORT, 'review, finding, fix and seed history are one ordering: seq must follow the ledger head'); END;
UPDATE store_meta SET schema_version = 59;
PRAGMA user_version = 59;
