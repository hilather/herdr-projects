CREATE TABLE attempt_inputs (
    attempt_id TEXT PRIMARY KEY NOT NULL REFERENCES attempts(id),
    operation_id TEXT NOT NULL UNIQUE REFERENCES operations(id),
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK(length(payload_hash)=64)
) STRICT;
CREATE TRIGGER attempt_inputs_no_update BEFORE UPDATE ON attempt_inputs BEGIN SELECT RAISE(ABORT,'attempt inputs are immutable'); END;
CREATE TRIGGER attempt_inputs_no_delete BEFORE DELETE ON attempt_inputs BEGIN SELECT RAISE(ABORT,'attempt inputs are immutable'); END;
CREATE TABLE attempt_cancellations (
    attempt_id TEXT PRIMARY KEY NOT NULL REFERENCES attempts(id),
    requested_unix_ms INTEGER NOT NULL CHECK(requested_unix_ms>=0),
    reason TEXT NOT NULL CHECK(length(reason)>0 AND length(reason)<=4000)
) STRICT;
CREATE TRIGGER attempt_cancellations_no_update BEFORE UPDATE ON attempt_cancellations BEGIN SELECT RAISE(ABORT,'cancellation request is immutable'); END;
CREATE TRIGGER attempt_cancellations_no_delete BEFORE DELETE ON attempt_cancellations BEGIN SELECT RAISE(ABORT,'cancellation request is immutable'); END;
CREATE TRIGGER operation_delivery_monotonic BEFORE UPDATE OF attempts,epoch ON operation_delivery
WHEN NEW.attempts<OLD.attempts OR NEW.epoch<OLD.epoch
BEGIN SELECT RAISE(ABORT,'claim history cannot move backwards'); END;
UPDATE store_meta SET schema_version=11;
PRAGMA user_version=11;
