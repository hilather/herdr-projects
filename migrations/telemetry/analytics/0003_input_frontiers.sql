-- Input generations are advanced by source-table triggers, never by reads.
CREATE TABLE IF NOT EXISTS analytics_input_frontiers (
 input TEXT PRIMARY KEY,
 sequence INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE TABLE IF NOT EXISTS analytics_checked_inputs (
 cell TEXT PRIMARY KEY REFERENCES analytics_cells(cell),
 inputs TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS analytics_provider_aggregates (
 provider TEXT NOT NULL,
 window_key TEXT NOT NULL,
 inputs TEXT NOT NULL,
 body TEXT NOT NULL CHECK(json_valid(body)),
 PRIMARY KEY(provider,window_key)
) STRICT;
CREATE TABLE IF NOT EXISTS analytics_input_installation (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 schema_version INTEGER NOT NULL
) STRICT;
