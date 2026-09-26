-- Scope from a signed task contract. Overlap is data in this schema, not a lock.
-- Plan proposals do not write these tables, and installing a contract does not reserve.
CREATE TABLE contract_scope_paths (
    task_id TEXT NOT NULL,
    contract_revision INTEGER NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0 AND ordinal < 64),
    path TEXT NOT NULL CHECK (length(path) BETWEEN 1 AND 512),
    access TEXT NOT NULL CHECK (access IN ('read', 'write')),
    certainty TEXT NOT NULL CHECK (certainty IN ('exact', 'uncertain')),
    PRIMARY KEY (task_id, contract_revision, ordinal),
    UNIQUE (task_id, contract_revision, path),
    FOREIGN KEY (task_id, contract_revision) REFERENCES task_contracts(task_id, contract_revision)
) STRICT;
CREATE TABLE contract_named_resources (
    task_id TEXT NOT NULL,
    contract_revision INTEGER NOT NULL,
    name TEXT NOT NULL CHECK (name IN ('schema', 'lockfile', 'generated')),
    access TEXT NOT NULL CHECK (access IN ('read', 'write')),
    PRIMARY KEY (task_id, contract_revision, name),
    FOREIGN KEY (task_id, contract_revision) REFERENCES task_contracts(task_id, contract_revision)
) STRICT;
CREATE INDEX contract_scope_paths_by_path ON contract_scope_paths(path, access);
CREATE INDEX contract_named_resources_by_name ON contract_named_resources(name, access);
CREATE TRIGGER contract_scope_paths_no_update BEFORE UPDATE ON contract_scope_paths
BEGIN SELECT RAISE(ABORT, 'contract scope is immutable'); END;
CREATE TRIGGER contract_scope_paths_no_delete BEFORE DELETE ON contract_scope_paths
BEGIN SELECT RAISE(ABORT, 'contract scope is immutable'); END;
CREATE TRIGGER contract_named_resources_no_update BEFORE UPDATE ON contract_named_resources
BEGIN SELECT RAISE(ABORT, 'contract named resource is immutable'); END;
CREATE TRIGGER contract_named_resources_no_delete BEFORE DELETE ON contract_named_resources
BEGIN SELECT RAISE(ABORT, 'contract named resource is immutable'); END;
UPDATE store_meta SET schema_version = 32;
PRAGMA user_version = 32;
