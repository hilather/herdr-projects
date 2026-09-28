-- Telemetry dispatch log (contracts §2, §3). Analytics only: never read to grant
-- launch. Written inside the reservation transaction, one decision per new attempt.
-- A configuration is content-addressed: an existing ID must carry identical bytes.
CREATE TABLE agent_configurations (
    configuration_id TEXT PRIMARY KEY CHECK (length(configuration_id) = 71 AND substr(configuration_id, 1, 7) = 'sha256:'),
    canonical_json TEXT NOT NULL CHECK (json_valid(canonical_json) AND length(canonical_json) <= 4096),
    first_decided_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TRIGGER agent_configurations_no_update BEFORE UPDATE ON agent_configurations
BEGIN SELECT RAISE(ABORT, 'agent configuration is immutable'); END;
CREATE TRIGGER agent_configurations_no_delete BEFORE DELETE ON agent_configurations
BEGIN SELECT RAISE(ABORT, 'agent configuration is immutable'); END;
CREATE TABLE dispatch_decisions (
    attempt_id TEXT PRIMARY KEY REFERENCES attempts(id),
    task_id TEXT NOT NULL REFERENCES tasks(id),
    task_revision INTEGER NOT NULL CHECK (task_revision > 0),
    contract_revision INTEGER CHECK (contract_revision IS NULL OR contract_revision > 0),
    classification_id TEXT REFERENCES task_classifications(classification_id),
    chosen_configuration_id TEXT NOT NULL REFERENCES agent_configurations(configuration_id),
    eligible TEXT NOT NULL CHECK (json_valid(eligible) AND json_array_length(eligible) BETWEEN 1 AND 256),
    chooser_kind TEXT NOT NULL CHECK (chooser_kind IN ('operator', 'automatic_admission', 'delegated')),
    chooser_principal TEXT NOT NULL CHECK (length(chooser_principal) BETWEEN 1 AND 128),
    reason_codes TEXT NOT NULL CHECK (json_valid(reason_codes) AND json_array_length(reason_codes) BETWEEN 1 AND 4),
    note TEXT CHECK (note IS NULL OR length(note) BETWEEN 1 AND 160),
    policy TEXT CHECK (policy IS NULL),
    seed TEXT CHECK (seed IS NULL),
    decided_unix_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX dispatch_decisions_task ON dispatch_decisions(task_id);
CREATE TRIGGER dispatch_decisions_no_update BEFORE UPDATE ON dispatch_decisions
BEGIN SELECT RAISE(ABORT, 'dispatch decision is immutable'); END;
CREATE TRIGGER dispatch_decisions_no_delete BEFORE DELETE ON dispatch_decisions
BEGIN SELECT RAISE(ABORT, 'dispatch decision is immutable'); END;
UPDATE store_meta SET schema_version = 50;
PRAGMA user_version = 50;
