-- Telemetry task taxonomy (contracts §1). Contract properties only, never outcomes.
-- Written or reused inside the reservation transaction; rows are immutable and a
-- reclassification appends a revision with a reason.
CREATE TABLE task_classifications (
    classification_id TEXT PRIMARY KEY CHECK (length(classification_id) = 71 AND substr(classification_id, 1, 7) = 'sha256:'),
    task_id TEXT NOT NULL REFERENCES tasks(id),
    contract_revision INTEGER CHECK (contract_revision IS NULL OR contract_revision > 0),
    taxonomy TEXT NOT NULL CHECK (length(taxonomy) BETWEEN 1 AND 64),
    class TEXT NOT NULL CHECK (class IN ('schema_change', 'dependency_change', 'read_only', 'docs', 'tests', 'code', 'unscoped')),
    band TEXT NOT NULL CHECK (band IN ('small', 'medium', 'large', 'unknown')),
    features TEXT NOT NULL CHECK (json_valid(features) AND length(features) <= 1024),
    classifier TEXT NOT NULL CHECK (length(classifier) BETWEEN 1 AND 128),
    revision INTEGER NOT NULL CHECK (revision > 0),
    reason TEXT CHECK (reason IS NULL OR length(reason) BETWEEN 1 AND 160),
    created_unix_ms INTEGER NOT NULL,
    UNIQUE (task_id, contract_revision, taxonomy, revision),
    FOREIGN KEY (task_id, contract_revision) REFERENCES task_contracts(task_id, contract_revision),
    CHECK ((revision = 1) = (reason IS NULL)),
    CHECK ((contract_revision IS NULL) = (class = 'unscoped'))
) STRICT;
-- UNIQUE treats NULL revisions as distinct; uncontracted tasks get their own key.
CREATE UNIQUE INDEX task_classifications_unscoped ON task_classifications(task_id, taxonomy, revision)
WHERE contract_revision IS NULL;
CREATE TRIGGER task_classifications_no_update BEFORE UPDATE ON task_classifications
BEGIN SELECT RAISE(ABORT, 'task classification is immutable'); END;
CREATE TRIGGER task_classifications_no_delete BEFORE DELETE ON task_classifications
BEGIN SELECT RAISE(ABORT, 'task classification is immutable'); END;
UPDATE store_meta SET schema_version = 49;
PRAGMA user_version = 49;
