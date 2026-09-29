-- Collector bindings (TM1.1, docs/telemetry/contracts-collection.md). Analytics
-- only: never read to grant launch. Revision 1 of a launched attempt is written
-- in the `apply_launch_started` transaction; a revocation appends a revision and
-- never rewrites one. Attempts that exist before 0052 get one `predates_binding`
-- revision: collectors fall back to contracts §5 binding rules 1-4 for them.
CREATE TABLE collector_bindings (
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    revision INTEGER NOT NULL CHECK (revision > 0),
    state TEXT NOT NULL CHECK (state IN ('active', 'revoked', 'predates_binding')),
    collector TEXT CHECK (collector IS NULL OR length(collector) BETWEEN 1 AND 64),
    execution_home TEXT,
    unix_ms INTEGER NOT NULL,
    source TEXT NOT NULL CHECK (length(source) BETWEEN 1 AND 64),
    PRIMARY KEY (attempt_id, revision),
    CHECK ((state = 'predates_binding') = (collector IS NULL))
) STRICT;
CREATE TRIGGER collector_bindings_no_update BEFORE UPDATE ON collector_bindings
BEGIN SELECT RAISE(ABORT, 'collector binding revision is immutable'); END;
CREATE TRIGGER collector_bindings_no_delete BEFORE DELETE ON collector_bindings
BEGIN SELECT RAISE(ABORT, 'collector binding revision is immutable'); END;
INSERT INTO collector_bindings(attempt_id, revision, state, collector, execution_home, unix_ms, source)
SELECT id, 1, 'predates_binding', NULL, NULL, CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER), 'migration_0052' FROM attempts;
UPDATE store_meta SET schema_version = 52;
PRAGMA user_version = 52;
