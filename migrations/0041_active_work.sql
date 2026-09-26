-- Rebuildable projection of bindings that still need reconciliation.
-- Retired bindings stay in runtime_bindings and are not copied here.
-- Dropping these tables is safe: the next read rebuilds them from attempts
-- that still retain capacity, plus live bindings with no attempt history.
CREATE TABLE active_work_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    incarnation TEXT NOT NULL CHECK (length(incarnation) = 64),
    projection_revision TEXT NOT NULL CHECK (length(projection_revision) = 64),
    inventory_revision INTEGER NOT NULL CHECK (inventory_revision >= 0)
) STRICT;

CREATE TABLE active_work_index (
    ordinal INTEGER PRIMARY KEY CHECK (ordinal >= 0),
    binding_id TEXT NOT NULL UNIQUE REFERENCES runtime_bindings(id),
    task_id TEXT REFERENCES tasks(id),
    attempt_id TEXT REFERENCES attempts(id),
    retains_capacity INTEGER NOT NULL CHECK (retains_capacity IN (0, 1))
) STRICT;

-- Covering partial index so the retained-capacity check does not visit retired attempts.
CREATE INDEX attempts_retained_by_id ON attempts(id, task_id, revision) WHERE termination_observed = 0;

-- Zeros cannot match a SHA-256 of the retained set, so the first read rebuilds.
INSERT INTO active_work_meta(singleton, incarnation, projection_revision, inventory_revision)
VALUES (
    1,
    lower(hex(randomblob(32))),
    '0000000000000000000000000000000000000000000000000000000000000000',
    0
);

UPDATE store_meta SET schema_version = 41;
PRAGMA user_version = 41;
