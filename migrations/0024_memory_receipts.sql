-- Worker declarations are distinct from transport and never resolve invalidations.
CREATE TABLE memory_update_receipts (
    delivery_id TEXT NOT NULL REFERENCES memory_delivery_intents(id),
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    state TEXT NOT NULL CHECK (state IN ('seen','applied')),
    manifest_hash TEXT NOT NULL CHECK (length(manifest_hash)=64),
    sequence INTEGER NOT NULL REFERENCES events(sequence),
    PRIMARY KEY (delivery_id,attempt_id,state)
) STRICT;
CREATE INDEX memory_receipts_attempt ON memory_update_receipts(attempt_id,sequence);
CREATE TRIGGER memory_receipts_no_update BEFORE UPDATE ON memory_update_receipts BEGIN SELECT RAISE(ABORT,'memory receipt is immutable'); END;
CREATE TRIGGER memory_receipts_no_delete BEFORE DELETE ON memory_update_receipts BEGIN SELECT RAISE(ABORT,'memory receipt is immutable'); END;
UPDATE store_meta SET schema_version=24;
PRAGMA user_version=24;
