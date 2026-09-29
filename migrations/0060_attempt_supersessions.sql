-- Accepted supersession reasons (docs/telemetry/contracts-accounting.md §10;
-- plan doc 07 M37). The project owner records why an ended attempt was
-- superseded or abandoned: because a sibling attempt changed the same area,
-- because it duplicated a sibling's effort, or for another reason. One record
-- per attempt, append-only; evidence is a list of typed references, never
-- content. Analytics only: never read to launch, verify, integrate or complete.
CREATE TABLE attempt_supersessions (
    attempt_id TEXT PRIMARY KEY REFERENCES attempts(id),
    task_id TEXT NOT NULL REFERENCES tasks(id),
    outcome TEXT NOT NULL CHECK (outcome IN ('superseded', 'abandoned')),
    reason TEXT NOT NULL CHECK (reason IN ('sibling_changed_same_area', 'duplicate_effort', 'other')),
    sibling_attempt_id TEXT REFERENCES attempts(id),
    evidence TEXT NOT NULL CHECK (json_valid(evidence) AND json_type(evidence) = 'array'
        AND json_array_length(evidence) BETWEEN 1 AND 16 AND length(evidence) <= 8192),
    principal TEXT NOT NULL CHECK (principal = 'operator:cli'),
    authority TEXT NOT NULL CHECK (authority = 'operator_owner.v1'),
    canonical_json TEXT NOT NULL CHECK (json_valid(canonical_json) AND length(canonical_json) <= 16384),
    recorded_unix_ms INTEGER NOT NULL,
    CHECK (sibling_attempt_id IS NULL OR sibling_attempt_id <> attempt_id),
    CHECK (reason = 'other' OR sibling_attempt_id IS NOT NULL)
) STRICT;
CREATE TRIGGER attempt_supersessions_no_update BEFORE UPDATE ON attempt_supersessions
BEGIN SELECT RAISE(ABORT, 'supersession reasons are append-only'); END;
CREATE TRIGGER attempt_supersessions_no_delete BEFORE DELETE ON attempt_supersessions
BEGIN SELECT RAISE(ABORT, 'supersession reasons are append-only'); END;
-- Only an ended attempt is explained, and only as an attempt of its own task.
CREATE TRIGGER attempt_supersessions_ended BEFORE INSERT ON attempt_supersessions
WHEN NOT EXISTS (SELECT 1 FROM attempts a WHERE a.id = NEW.attempt_id AND a.task_id = NEW.task_id
    AND a.state IN ('completed', 'failed', 'cancelled', 'lost'))
BEGIN SELECT RAISE(ABORT, 'a supersession reason explains an ended attempt of its task'); END;
UPDATE store_meta SET schema_version = 60;
PRAGMA user_version = 60;
