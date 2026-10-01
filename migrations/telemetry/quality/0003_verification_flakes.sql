-- Rebuildable metadata projection; canonical identities are references by value.
CREATE TABLE IF NOT EXISTS quality_verification_runs (
    run_id TEXT PRIMARY KEY,
    source_rowid INTEGER NOT NULL UNIQUE,
    tree_oid TEXT NOT NULL,
    object_format TEXT NOT NULL,
    policy_id TEXT NOT NULL,
    policy_digest TEXT NOT NULL,
    verdict TEXT NOT NULL CHECK(verdict IN ('accepted','rejected')),
    created_unix_ms INTEGER NOT NULL,
    load_1m REAL CHECK(load_1m IS NULL OR load_1m >= 0),
    tests_status TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS quality_verification_pairs ON quality_verification_runs(tree_oid,object_format,policy_id,policy_digest,created_unix_ms);
CREATE TABLE IF NOT EXISTS quality_test_results (
    run_id TEXT NOT NULL REFERENCES quality_verification_runs(run_id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK(length(CAST(name AS BLOB)) BETWEEN 1 AND 256),
    outcome TEXT NOT NULL CHECK(outcome IN ('pass','fail','ignored')),
    PRIMARY KEY(run_id,name)
) STRICT;
CREATE TABLE IF NOT EXISTS quality_verification_cursor (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    source_rowid INTEGER NOT NULL CHECK(source_rowid>=0),
    collected_unix_ms INTEGER
) STRICT;
INSERT OR IGNORE INTO quality_verification_cursor VALUES(1,0,NULL);
