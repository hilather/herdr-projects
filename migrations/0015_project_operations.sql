-- Project operations bind project-control revision instead of a synthetic task.
-- Preserve dependent records while rebuilding with foreign keys enabled.
CREATE TEMP TABLE saved_operation_delivery AS SELECT * FROM operation_delivery;
CREATE TEMP TABLE saved_attempt_inputs AS SELECT * FROM attempt_inputs;
CREATE TEMP TABLE saved_approval_uses AS SELECT * FROM approval_uses;
DROP TABLE approval_uses;
DROP TABLE attempt_inputs;
DROP TABLE operation_delivery;
CREATE TABLE operations_next (
    id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT REFERENCES tasks(id),
    kind TEXT NOT NULL CHECK(length(kind)>0),
    target TEXT NOT NULL CHECK(length(target)>0),
    payload_version INTEGER NOT NULL CHECK(payload_version>0),
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK(length(payload_hash)=64),
    expected_revision INTEGER NOT NULL CHECK(expected_revision>0),
    due_unix_ms INTEGER NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE CHECK(length(idempotency_key)>0),
    CHECK(task_id IS NOT NULL OR kind='routine.run')
) STRICT;
INSERT INTO operations_next SELECT * FROM operations;
DROP TABLE operations;
ALTER TABLE operations_next RENAME TO operations;
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
CREATE TABLE attempt_inputs (
    attempt_id TEXT PRIMARY KEY NOT NULL REFERENCES attempts(id),
    operation_id TEXT NOT NULL UNIQUE REFERENCES operations(id),
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK(length(payload_hash)=64)
) STRICT;
CREATE TABLE approval_uses (
    approval_id TEXT PRIMARY KEY NOT NULL REFERENCES approval_grants(id),
    operation_id TEXT NOT NULL UNIQUE REFERENCES operations(id),
    claim_revision INTEGER NOT NULL CHECK(claim_revision>0),
    claim_epoch INTEGER NOT NULL CHECK(claim_epoch>0),
    consumed_unix_ms INTEGER NOT NULL CHECK(consumed_unix_ms>=0)
) STRICT;
INSERT INTO operation_delivery SELECT * FROM saved_operation_delivery;
INSERT INTO attempt_inputs SELECT * FROM saved_attempt_inputs;
INSERT INTO approval_uses SELECT * FROM saved_approval_uses;
DROP TABLE saved_operation_delivery;
DROP TABLE saved_attempt_inputs;
DROP TABLE saved_approval_uses;
CREATE TRIGGER attempt_inputs_no_update BEFORE UPDATE ON attempt_inputs BEGIN SELECT RAISE(ABORT,'attempt inputs are immutable'); END;
CREATE TRIGGER attempt_inputs_no_delete BEFORE DELETE ON attempt_inputs BEGIN SELECT RAISE(ABORT,'attempt inputs are immutable'); END;
CREATE TRIGGER approval_uses_no_update BEFORE UPDATE ON approval_uses BEGIN SELECT RAISE(ABORT,'approval use is immutable'); END;
CREATE TRIGGER approval_uses_no_delete BEFORE DELETE ON approval_uses BEGIN SELECT RAISE(ABORT,'approval use is immutable'); END;
CREATE TRIGGER operation_delivery_monotonic BEFORE UPDATE OF attempts,epoch ON operation_delivery
WHEN NEW.attempts<OLD.attempts OR NEW.epoch<OLD.epoch
BEGIN SELECT RAISE(ABORT,'claim history cannot move backwards'); END;
CREATE TRIGGER attempt_inputs_effective_profile BEFORE INSERT ON attempt_inputs
WHEN COALESCE(json_extract(NEW.payload,'$.inputs.version'),0)<>2
 OR COALESCE(json_type(NEW.payload,'$.inputs.effective_profile'),'missing')<>'object'
BEGIN SELECT RAISE(ABORT,'effective profile evidence is required'); END;
CREATE TRIGGER operation_delivery_insert AFTER INSERT ON operations BEGIN
    INSERT INTO operation_delivery VALUES(NEW.id,1,'pending',0,0,NULL,NULL,NEW.due_unix_ms,NULL);
END;
UPDATE store_meta SET schema_version=15;
PRAGMA user_version=15;
