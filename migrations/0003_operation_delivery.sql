-- Durable at-least-once delivery metadata; immutable intent stays in operations.
CREATE TABLE operation_delivery (
    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operations(id),
    revision INTEGER NOT NULL CHECK (revision > 0),
    state TEXT NOT NULL CHECK (state IN ('pending','claimed','ambiguous','confirmed','permanent_failure')),
    epoch INTEGER NOT NULL CHECK (epoch >= 0),
    attempts INTEGER NOT NULL CHECK (attempts >= 0),
    owner TEXT,
    lease_until_ms INTEGER,
    next_due_ms INTEGER NOT NULL,
    last_outcome TEXT CHECK (last_outcome IS NULL OR json_valid(last_outcome)),
    CHECK ((state = 'claimed' AND owner IS NOT NULL AND lease_until_ms IS NOT NULL) OR
           (state != 'claimed' AND owner IS NULL AND lease_until_ms IS NULL))
) STRICT;
INSERT INTO operation_delivery SELECT id,1,'pending',0,0,NULL,NULL,due_unix_ms,NULL FROM operations;
CREATE TRIGGER operation_delivery_insert AFTER INSERT ON operations BEGIN
    INSERT INTO operation_delivery VALUES (NEW.id,1,'pending',0,0,NULL,NULL,NEW.due_unix_ms,NULL);
END;
ALTER TABLE migration_receipt ADD COLUMN operation_count INTEGER NOT NULL DEFAULT 0;
UPDATE store_meta SET schema_version=3 WHERE singleton=1;
PRAGMA user_version=3;
