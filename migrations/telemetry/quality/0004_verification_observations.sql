-- Rebuildable rerun evidence; these are observations, never acceptance receipts.
CREATE TABLE IF NOT EXISTS quality_verification_observations (
    run_id TEXT PRIMARY KEY,
    parent_run_id TEXT NOT NULL REFERENCES quality_verification_runs(run_id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK(sequence BETWEEN 0 AND 699),
    verdict TEXT NOT NULL CHECK(verdict IN ('accepted','rejected')),
    load_1m REAL CHECK(load_1m IS NULL OR load_1m >= 0),
    UNIQUE(parent_run_id,sequence)
) STRICT;
CREATE TABLE IF NOT EXISTS quality_observation_tests (
    run_id TEXT NOT NULL REFERENCES quality_verification_observations(run_id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK(length(CAST(name AS BLOB)) BETWEEN 1 AND 256),
    outcome TEXT NOT NULL CHECK(outcome IN ('pass','fail','ignored')),
    PRIMARY KEY(run_id,name)
) STRICT;
CREATE VIEW IF NOT EXISTS quality_completed_verifications AS
    SELECT run_id,tree_oid,object_format,policy_id,policy_digest,verdict,created_unix_ms,load_1m FROM quality_verification_runs
    UNION ALL
    SELECT o.run_id,v.tree_oid,v.object_format,v.policy_id,v.policy_digest,o.verdict,v.created_unix_ms,o.load_1m
    FROM quality_verification_observations o JOIN quality_verification_runs v ON v.run_id=o.parent_run_id;
CREATE VIEW IF NOT EXISTS quality_completed_tests AS
    SELECT * FROM quality_test_results UNION ALL SELECT * FROM quality_observation_tests;
-- Re-scan immutable canonical metadata to derive observations from older runs.
UPDATE quality_verification_cursor SET source_rowid=0;
