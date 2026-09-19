-- Store-owned schema. Runtime authority changes only through T03.2 migration.
CREATE TABLE store_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version INTEGER NOT NULL CHECK (schema_version = 1)
) STRICT;
INSERT INTO store_meta VALUES (1, 1);
CREATE TABLE tasks (
    id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    state TEXT NOT NULL CHECK (state IN ('draft','queued','ready','running','awaiting_review','blocked','succeeded','failed','cancelled')),
    title TEXT NOT NULL,
    active_attempt TEXT,
    FOREIGN KEY (active_attempt, id) REFERENCES attempts(id, task_id) DEFERRABLE INITIALLY DEFERRED
) STRICT;
CREATE TABLE attempts (
    id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(id) DEFERRABLE INITIALLY DEFERRED,
    revision INTEGER NOT NULL CHECK (revision > 0),
    state TEXT NOT NULL CHECK (state IN ('reserved','launching','running','awaiting_input','completed','failed','cancelled','lost')),
    snapshot TEXT,
    reservation TEXT NOT NULL CHECK (length(reservation) > 0),
    termination_observed INTEGER NOT NULL CHECK (termination_observed IN (0,1)),
    UNIQUE (id, task_id)
) STRICT;
CREATE UNIQUE INDEX live_reservation ON attempts(reservation) WHERE termination_observed = 0;
CREATE TABLE operations (
    id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    kind TEXT NOT NULL CHECK (length(kind) > 0),
    target TEXT NOT NULL CHECK (length(target) > 0),
    payload_version INTEGER NOT NULL CHECK (payload_version > 0),
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK (length(payload_hash) = 64),
    expected_revision INTEGER NOT NULL CHECK (expected_revision > 0),
    due_unix_ms INTEGER NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE CHECK (length(idempotency_key) > 0)
) STRICT;
CREATE TABLE events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    entity TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    payload_version INTEGER NOT NULL CHECK (payload_version > 0),
    payload TEXT NOT NULL CHECK (json_valid(payload))
) STRICT;
-- No memory tables/projections before T05.3. Future schemas add typed extensions.
PRAGMA application_id = 1213222994;
PRAGMA user_version = 1;
