-- Upgrade also adopts the rendering tables created by the pre-v2 binary.
DROP TRIGGER IF EXISTS analytics_workspace_comparisons_no_update;
DROP TRIGGER IF EXISTS analytics_workspace_comparisons_no_delete;
DROP TRIGGER IF EXISTS analytics_workspace_metrics_no_update;
DROP TRIGGER IF EXISTS analytics_workspace_metrics_no_delete;
CREATE TABLE IF NOT EXISTS analytics_workspace_comparisons (
    revision INTEGER PRIMARY KEY CHECK(revision > 0),
    body TEXT NOT NULL CHECK(json_valid(body)),
    recorded_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS analytics_workspace_metrics (
    revision INTEGER PRIMARY KEY REFERENCES analytics_revisions(revision),
    body TEXT NOT NULL CHECK(json_valid(body))
) STRICT;
