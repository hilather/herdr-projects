-- Stream `quality` 2: TM3.7 integration outcomes (docs/telemetry/contracts-quality.md §2).
-- Analytics only: never read to accept, verify, integrate or launch.
-- One row per integrated commit and horizon, written once the horizon has
-- passed. Canonical IDs and object IDs copied by value; counts only, no paths.
CREATE TABLE IF NOT EXISTS integration_outcomes (
    integrated_id TEXT NOT NULL,
    horizon_ms INTEGER NOT NULL CHECK (horizon_ms > 0),
    commit_oid TEXT NOT NULL,
    integrated_unix_ms INTEGER NOT NULL,
    horizon_oid TEXT,
    reverted TEXT CHECK (reverted IN ('trailer', 'tree_restore', 'none')),
    added_lines INTEGER CHECK (added_lines >= 0),
    surviving_lines INTEGER CHECK (surviving_lines >= 0 AND surviving_lines <= added_lines),
    churn_added_lines INTEGER CHECK (churn_added_lines >= 0),
    churn_deleted_lines INTEGER CHECK (churn_deleted_lines >= 0),
    unavailable_reason TEXT,
    rule TEXT NOT NULL,
    source_trust TEXT NOT NULL CHECK (source_trust = 'proxy_observed'),
    observed_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (integrated_id, horizon_ms),
    CHECK ((unavailable_reason IS NULL) = (reverted IS NOT NULL AND surviving_lines IS NOT NULL AND churn_deleted_lines IS NOT NULL AND horizon_oid IS NOT NULL))
) STRICT;
