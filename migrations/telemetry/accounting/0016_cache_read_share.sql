-- M10 totals share the ledger's atomic frontier and affected-session replay.
CREATE TABLE IF NOT EXISTS accounting_cache_totals (
 session_id TEXT PRIMARY KEY,
 input_tokens INTEGER NOT NULL,
 cache_read_tokens INTEGER NOT NULL,
 cache_write_tokens INTEGER NOT NULL,
 records INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS accounting_cache_frontier (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 inputs TEXT NOT NULL CHECK(json_valid(inputs))
) STRICT;
UPDATE accounting_stream SET invalidated='schema_upgrade' WHERE singleton=1;
