-- Evidence is written by the verifier, never by a telemetry collector.
-- NULL preserves the absence of evidence on historical runs.
ALTER TABLE verification_runs ADD COLUMN metadata TEXT
    CHECK (metadata IS NULL OR (json_valid(metadata) AND json_type(metadata) = 'object' AND length(CAST(metadata AS BLOB)) <= 2097152));
CREATE INDEX verification_runs_by_tree_policy ON verification_runs(tree_oid,policy_id,policy_digest,created_unix_ms);
UPDATE store_meta SET schema_version = 68;
PRAGMA user_version = 68;
