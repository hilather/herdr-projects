-- v2 promotion compares this fence inside the write transaction.
-- required_set_generation moves only for a hard or constraint record.
-- A contract catalog change is per scope, so a phantom contract conflicts
-- without treating an optional observation as a new fence.
CREATE TABLE memory_required_generation (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    generation INTEGER NOT NULL CHECK (generation >= 0)
) STRICT;
INSERT INTO memory_required_generation(singleton, generation) VALUES (1, 0);

CREATE TABLE memory_scope_catalog (
    scope_id TEXT PRIMARY KEY CHECK (length(scope_id) BETWEEN 1 AND 128),
    generation INTEGER NOT NULL CHECK (generation >= 0)
) STRICT;
INSERT INTO memory_scope_catalog(scope_id, generation)
SELECT scope_id, 0 FROM memory_records GROUP BY scope_id;

DROP TRIGGER IF EXISTS memory_read_set_on_record;
DROP TRIGGER IF EXISTS memory_read_set_on_reclassify;
DROP TRIGGER IF EXISTS memory_read_set_on_revision;
DROP TRIGGER IF EXISTS memory_read_set_on_validity;
DROP TRIGGER IF EXISTS memory_read_set_on_head;

CREATE TRIGGER memory_read_set_on_record
AFTER INSERT ON memory_records
BEGIN
    UPDATE memory_required_generation SET generation = generation + 1
    WHERE singleton = 1
      AND (NEW.is_hard = 1 OR NEW.kind IN ('constraint', 'hard_memory'));
    INSERT INTO memory_scope_catalog(scope_id, generation)
    SELECT NEW.scope_id, 1
    WHERE NEW.is_hard = 1 OR NEW.kind IN ('constraint', 'hard_memory', 'contract')
    ON CONFLICT(scope_id) DO UPDATE SET generation = generation + 1;
END;

CREATE TRIGGER memory_read_set_on_reclassify
AFTER UPDATE OF is_hard, kind ON memory_records
WHEN OLD.is_hard != NEW.is_hard OR OLD.kind != NEW.kind
BEGIN
    UPDATE memory_required_generation SET generation = generation + 1
    WHERE singleton = 1
      AND (
          OLD.is_hard = 1 OR NEW.is_hard = 1
          OR OLD.kind IN ('constraint', 'hard_memory')
          OR NEW.kind IN ('constraint', 'hard_memory')
      );
    INSERT INTO memory_scope_catalog(scope_id, generation)
    SELECT NEW.scope_id, 1
    WHERE OLD.is_hard = 1 OR NEW.is_hard = 1
       OR OLD.kind IN ('constraint', 'hard_memory', 'contract')
       OR NEW.kind IN ('constraint', 'hard_memory', 'contract')
    ON CONFLICT(scope_id) DO UPDATE SET generation = generation + 1;
END;

CREATE TRIGGER memory_read_set_on_revision
AFTER INSERT ON memory_revisions
BEGIN
    UPDATE memory_required_generation SET generation = generation + 1
    WHERE singleton = 1
      AND EXISTS (
          SELECT 1 FROM memory_records r
          WHERE r.id = NEW.record_id
            AND (r.is_hard = 1 OR r.kind IN ('constraint', 'hard_memory'))
      );
    UPDATE memory_scope_catalog SET generation = generation + 1
    WHERE scope_id = (
        SELECT r.scope_id FROM memory_records r
        WHERE r.id = NEW.record_id
          AND (r.is_hard = 1 OR r.kind IN ('constraint', 'hard_memory', 'contract'))
    );
END;

CREATE TRIGGER memory_read_set_on_validity
AFTER UPDATE OF state, reason, expiry_unix_ms ON memory_validity
BEGIN
    UPDATE memory_required_generation SET generation = generation + 1
    WHERE singleton = 1
      AND EXISTS (
          SELECT 1 FROM memory_records r
          WHERE r.id = NEW.record_id
            AND (r.is_hard = 1 OR r.kind IN ('constraint', 'hard_memory'))
      );
    UPDATE memory_scope_catalog SET generation = generation + 1
    WHERE scope_id = (
        SELECT r.scope_id FROM memory_records r
        WHERE r.id = NEW.record_id
          AND (r.is_hard = 1 OR r.kind IN ('constraint', 'hard_memory', 'contract'))
    );
END;

CREATE TRIGGER memory_read_set_on_head
AFTER UPDATE OF status, revision ON memory_heads
BEGIN
    UPDATE memory_required_generation SET generation = generation + 1
    WHERE singleton = 1
      AND EXISTS (
          SELECT 1 FROM memory_records r
          WHERE r.id = NEW.record_id
            AND (r.is_hard = 1 OR r.kind IN ('constraint', 'hard_memory'))
      );
    UPDATE memory_scope_catalog SET generation = generation + 1
    WHERE scope_id = (
        SELECT r.scope_id FROM memory_records r
        WHERE r.id = NEW.record_id
          AND (r.is_hard = 1 OR r.kind IN ('constraint', 'hard_memory', 'contract'))
    );
END;

UPDATE store_meta SET schema_version = 38;
PRAGMA user_version = 38;
