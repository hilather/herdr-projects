-- One row per consumer generation. The active row is the routing target.
-- A coordinator generation names no attempt. Retiring a row keeps it, and
-- does not delete snapshot bytes or delivery obligations.
CREATE TABLE consumer_bindings (
    binding_id TEXT PRIMARY KEY CHECK (length(binding_id) = 64),
    consumer_id TEXT NOT NULL CHECK (length(consumer_id) BETWEEN 1 AND 256),
    generation INTEGER NOT NULL CHECK (generation > 0),
    snapshot_id TEXT NOT NULL UNIQUE REFERENCES memory_snapshots(id),
    attempt_id TEXT,
    task_id TEXT REFERENCES tasks(id),
    active INTEGER NOT NULL CHECK (active IN (0, 1)),
    retired INTEGER NOT NULL CHECK (retired IN (0, 1)),
    successor_binding_id TEXT REFERENCES consumer_bindings(binding_id),
    created_unix_ms INTEGER NOT NULL,
    UNIQUE (consumer_id, generation),
    CHECK (attempt_id IS NULL OR task_id IS NOT NULL),
    CHECK (retired = 0 OR active = 0),
    CHECK (retired = 1 OR successor_binding_id IS NULL),
    CHECK (successor_binding_id IS NULL OR successor_binding_id <> binding_id),
    FOREIGN KEY (attempt_id, task_id) REFERENCES attempts(id, task_id)
) STRICT;
-- Hot index: consumer binding by (consumer_id, generation, active).
CREATE INDEX consumer_bindings_by_generation
    ON consumer_bindings(consumer_id, generation, active);
CREATE TABLE consumer_binding_obligations (
    binding_id TEXT NOT NULL REFERENCES consumer_bindings(binding_id),
    delivery_id TEXT NOT NULL REFERENCES memory_delivery_intents(id),
    PRIMARY KEY (binding_id, delivery_id)
) STRICT;
CREATE TABLE consumer_binding_undeliverable (
    binding_id TEXT NOT NULL REFERENCES consumer_bindings(binding_id),
    delivery_id TEXT NOT NULL REFERENCES memory_delivery_intents(id),
    reason TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 256),
    PRIMARY KEY (binding_id, delivery_id)
) STRICT;
CREATE TRIGGER consumer_bindings_no_delete
BEFORE DELETE ON consumer_bindings
BEGIN SELECT RAISE(ABORT, 'consumer binding is immutable'); END;
CREATE TRIGGER consumer_bindings_identity_fixed
BEFORE UPDATE OF binding_id, consumer_id, generation, snapshot_id, created_unix_ms ON consumer_bindings
BEGIN SELECT RAISE(ABORT, 'consumer binding identity is immutable'); END;
CREATE TRIGGER consumer_bindings_attempt_fixed
BEFORE UPDATE OF attempt_id ON consumer_bindings
WHEN OLD.attempt_id IS NOT NULL
BEGIN SELECT RAISE(ABORT, 'consumer binding attempt is immutable'); END;
CREATE TRIGGER consumer_bindings_task_fixed
BEFORE UPDATE OF task_id ON consumer_bindings
WHEN OLD.task_id IS NOT NULL
BEGIN SELECT RAISE(ABORT, 'consumer binding task is immutable'); END;
CREATE TRIGGER consumer_bindings_retire_once
BEFORE UPDATE OF retired, successor_binding_id ON consumer_bindings
WHEN OLD.retired = 1
BEGIN SELECT RAISE(ABORT, 'consumer binding retires once'); END;
CREATE TRIGGER consumer_bindings_retired_stays_inactive
BEFORE UPDATE OF active ON consumer_bindings
WHEN OLD.retired = 1 AND NEW.active <> 0
BEGIN SELECT RAISE(ABORT, 'retired consumer binding stays inactive'); END;
CREATE TRIGGER consumer_binding_obligations_no_update
BEFORE UPDATE ON consumer_binding_obligations
BEGIN SELECT RAISE(ABORT, 'consumer binding obligation is immutable'); END;
CREATE TRIGGER consumer_binding_obligations_no_delete
BEFORE DELETE ON consumer_binding_obligations
BEGIN SELECT RAISE(ABORT, 'consumer binding obligation is immutable'); END;
CREATE TRIGGER consumer_binding_undeliverable_no_update
BEFORE UPDATE ON consumer_binding_undeliverable
BEGIN SELECT RAISE(ABORT, 'undeliverable obligation is immutable'); END;
CREATE TRIGGER consumer_binding_undeliverable_no_delete
BEFORE DELETE ON consumer_binding_undeliverable
BEGIN SELECT RAISE(ABORT, 'undeliverable obligation is immutable'); END;
UPDATE store_meta SET schema_version = 37;
PRAGMA user_version = 37;
