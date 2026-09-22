CREATE TABLE memory_snapshot_inputs (
    snapshot_id TEXT PRIMARY KEY NOT NULL REFERENCES memory_snapshots(id),
    instructions TEXT NOT NULL CHECK (length(CAST(instructions AS BLOB))<=1048576),
    instruction_hash TEXT NOT NULL CHECK (length(instruction_hash)=64),
    task_text TEXT NOT NULL,
    task_hash TEXT NOT NULL CHECK (length(task_hash)=64),
    request_json TEXT NOT NULL CHECK (json_valid(request_json)),
    request_hash TEXT NOT NULL CHECK (length(request_hash)=64)
) STRICT;
CREATE TABLE memory_import_candidates (
    id TEXT PRIMARY KEY NOT NULL,
    record_id TEXT NOT NULL,
    record_key TEXT NOT NULL,
    expected_revision INTEGER CHECK (expected_revision>0),
    body_hash TEXT NOT NULL REFERENCES objects(hash),
    provenance_hash TEXT NOT NULL REFERENCES objects(hash),
    created_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE memory_import_decisions (
    candidate_id TEXT PRIMARY KEY NOT NULL REFERENCES memory_import_candidates(id),
    decision TEXT NOT NULL CHECK (decision IN ('approve','reject')),
    authorization TEXT NOT NULL CHECK (json_valid(authorization)),
    authorization_hash TEXT NOT NULL CHECK (length(authorization_hash)=64),
    sequence INTEGER NOT NULL REFERENCES events(sequence),
    resulting_revision INTEGER
) STRICT;
CREATE TRIGGER snapshot_inputs_no_update BEFORE UPDATE ON memory_snapshot_inputs BEGIN SELECT RAISE(ABORT,'snapshot inputs are immutable'); END;
CREATE TRIGGER snapshot_inputs_no_delete BEFORE DELETE ON memory_snapshot_inputs BEGIN SELECT RAISE(ABORT,'snapshot inputs are immutable'); END;
CREATE TRIGGER import_candidates_no_update BEFORE UPDATE ON memory_import_candidates BEGIN SELECT RAISE(ABORT,'import candidate is immutable'); END;
CREATE TRIGGER import_candidates_no_delete BEFORE DELETE ON memory_import_candidates BEGIN SELECT RAISE(ABORT,'import candidate is immutable'); END;
CREATE TRIGGER import_decisions_no_update BEFORE UPDATE ON memory_import_decisions BEGIN SELECT RAISE(ABORT,'import decision is immutable'); END;
CREATE TRIGGER import_decisions_no_delete BEFORE DELETE ON memory_import_decisions BEGIN SELECT RAISE(ABORT,'import decision is immutable'); END;
UPDATE store_meta SET schema_version=23;
PRAGMA user_version=23;
-- Durable conservative routing obligations. Transport and applied acknowledgments
-- are separate; creating a row must never imply that a worker consumed a change.
CREATE TABLE memory_delivery_intents (
    id TEXT PRIMARY KEY NOT NULL,
    cause_id TEXT NOT NULL,
    subscriber TEXT NOT NULL,
    snapshot_id TEXT NOT NULL REFERENCES memory_snapshots(id),
    task_id TEXT REFERENCES tasks(id),
    record_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    severity TEXT NOT NULL CHECK (severity IN ('informational','reconcile_before_completion','stop_at_checkpoint')),
    triggering_seq INTEGER NOT NULL REFERENCES events(sequence),
    state TEXT NOT NULL CHECK (state='pending'),
    FOREIGN KEY (record_id,revision) REFERENCES memory_revisions(record_id,revision),
    UNIQUE (cause_id,subscriber,snapshot_id,record_id,revision)
) STRICT;
CREATE TRIGGER memory_delivery_no_update BEFORE UPDATE ON memory_delivery_intents BEGIN SELECT RAISE(ABORT,'delivery obligation is immutable'); END;
CREATE TRIGGER memory_delivery_no_delete BEFORE DELETE ON memory_delivery_intents BEGIN SELECT RAISE(ABORT,'delivery obligation is immutable'); END;
