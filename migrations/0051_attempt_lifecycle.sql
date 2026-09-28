-- Telemetry lifecycle marks (contracts §4). Analytics only: never read to grant
-- launch. Written in the transaction of each canonical attempt transition; a state
-- keeps the first time it was reached. Attempts reserved before 0051 have no marks.
CREATE TABLE attempt_lifecycle (
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    state TEXT NOT NULL CHECK (state IN ('reserved', 'launching', 'running', 'completed', 'failed', 'cancelled', 'lost')),
    attempt_revision INTEGER NOT NULL CHECK (attempt_revision > 0),
    unix_ms INTEGER NOT NULL,
    source TEXT NOT NULL CHECK (length(source) BETWEEN 1 AND 64),
    PRIMARY KEY (attempt_id, state)
) STRICT;
CREATE TRIGGER attempt_lifecycle_no_update BEFORE UPDATE ON attempt_lifecycle
BEGIN SELECT RAISE(ABORT, 'lifecycle mark is immutable'); END;
CREATE TRIGGER attempt_lifecycle_no_delete BEFORE DELETE ON attempt_lifecycle
BEGIN SELECT RAISE(ABORT, 'lifecycle mark is immutable'); END;
UPDATE store_meta SET schema_version = 51;
PRAGMA user_version = 51;
