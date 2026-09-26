-- Claims copied from a contract revision. Admission uses them as a readiness
-- blocker. Rows are not acquired or released, so this is not a lock.
CREATE TABLE resource_claims (
    task_id TEXT NOT NULL,
    contract_revision INTEGER NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0 AND ordinal < 72),
    kind TEXT NOT NULL CHECK (kind IN ('path', 'named')),
    resource TEXT NOT NULL CHECK (length(resource) BETWEEN 1 AND 512),
    access TEXT NOT NULL CHECK (access IN ('read', 'write')),
    certainty TEXT NOT NULL CHECK (certainty IN ('exact', 'uncertain')),
    PRIMARY KEY (task_id, contract_revision, ordinal),
    UNIQUE (task_id, contract_revision, kind, resource),
    FOREIGN KEY (task_id, contract_revision) REFERENCES task_contracts(task_id, contract_revision)
) STRICT;
CREATE INDEX resource_claims_by_task ON resource_claims(task_id, contract_revision);
CREATE TRIGGER resource_claims_no_update BEFORE UPDATE ON resource_claims
BEGIN SELECT RAISE(ABORT, 'resource claim is immutable'); END;
CREATE TRIGGER resource_claims_no_delete BEFORE DELETE ON resource_claims
BEGIN SELECT RAISE(ABORT, 'resource claim is immutable'); END;

INSERT INTO resource_claims(task_id, contract_revision, ordinal, kind, resource, access, certainty)
SELECT task_id, contract_revision, ordinal, 'path', path, access, certainty
FROM contract_scope_paths;
INSERT INTO resource_claims(task_id, contract_revision, ordinal, kind, resource, access, certainty)
SELECT task_id, contract_revision,
       64 + (SELECT count(*) FROM contract_named_resources earlier
             WHERE earlier.task_id = contract_named_resources.task_id
               AND earlier.contract_revision = contract_named_resources.contract_revision
               AND earlier.name < contract_named_resources.name),
       'named', name, access, 'exact'
FROM contract_named_resources;

UPDATE store_meta SET schema_version = 35;
PRAGMA user_version = 35;
