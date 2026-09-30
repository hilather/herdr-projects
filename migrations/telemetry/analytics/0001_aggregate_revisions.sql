-- TM4.1 analytics stream (docs/telemetry/contracts-analytics.md §4): versioned
-- aggregate revisions derived from state.db (read-only) and the other sidecar
-- streams. Analytics only: never read to grant launch, change budgets or
-- accept results. Revisions and lineage are append-only; a late correction
-- appends a restatement that supersedes the previous revision of its cell.
-- A tracked cell: one (metric, definition, cohort, window, horizon, dimension).
-- `checked_unix_ms` is the only mutable column: the last refresh that found
-- the cell's content unchanged.
CREATE TABLE IF NOT EXISTS analytics_cells (
    cell TEXT PRIMARY KEY CHECK (json_valid(cell)),
    metric TEXT NOT NULL CHECK (length(metric) BETWEEN 1 AND 32),
    definition TEXT NOT NULL CHECK (length(definition) BETWEEN 1 AND 64),
    cohort TEXT NOT NULL CHECK (cohort IN ('activity_window', 'terminal_cohort', 'assignment_cohort')),
    window_from_unix_ms INTEGER,
    window_to_unix_ms INTEGER,
    horizon_ms INTEGER CHECK (horizon_ms IS NULL OR horizon_ms > 0),
    dimension TEXT,
    tracked_unix_ms INTEGER NOT NULL,
    checked_unix_ms INTEGER,
    CHECK (window_from_unix_ms IS NULL OR window_to_unix_ms IS NULL OR window_from_unix_ms < window_to_unix_ms)
) STRICT;
-- `revision` is the projection sequence (`--as-of-seq`); `recorded_unix_ms`
-- the knowledge time (`--as-of`). `body` and the lineage rows are the
-- deterministic content (`content_digest`); `watermarks` the source
-- positions it was computed from (provenance, not content).
CREATE TABLE IF NOT EXISTS analytics_revisions (
    revision INTEGER PRIMARY KEY CHECK (revision > 0),
    cell TEXT NOT NULL REFERENCES analytics_cells(cell),
    kind TEXT NOT NULL CHECK (kind IN ('initial', 'restatement')),
    supersedes INTEGER REFERENCES analytics_revisions(revision),
    body TEXT NOT NULL CHECK (json_valid(body)),
    content_digest TEXT NOT NULL CHECK (length(content_digest) = 71 AND substr(content_digest, 1, 7) = 'sha256:'),
    watermarks TEXT NOT NULL CHECK (json_valid(watermarks)),
    registry TEXT NOT NULL,
    recorded_unix_ms INTEGER NOT NULL,
    CHECK ((kind = 'initial') = (supersedes IS NULL))
) STRICT;
CREATE INDEX IF NOT EXISTS analytics_revisions_cell ON analytics_revisions(cell, revision);
CREATE INDEX IF NOT EXISTS analytics_revisions_cell_recorded ON analytics_revisions(cell, recorded_unix_ms, revision);
-- Drill-down lineage: the ledger/trace identities behind each bucket of a
-- revision (never metric labels), in a stable order for keyset pagination.
CREATE TABLE IF NOT EXISTS analytics_lineage (
    revision INTEGER NOT NULL REFERENCES analytics_revisions(revision),
    bucket TEXT NOT NULL CHECK (length(bucket) BETWEEN 1 AND 128),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    entity_kind TEXT NOT NULL CHECK (entity_kind IN ('task', 'attempt')),
    entity_id TEXT NOT NULL,
    attrs TEXT NOT NULL CHECK (json_valid(attrs)),
    PRIMARY KEY (revision, bucket, ordinal)
) STRICT, WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS analytics_revisions_no_update BEFORE UPDATE ON analytics_revisions
BEGIN SELECT RAISE(ABORT, 'analytics revision is immutable'); END;
CREATE TRIGGER IF NOT EXISTS analytics_revisions_no_delete BEFORE DELETE ON analytics_revisions
BEGIN SELECT RAISE(ABORT, 'analytics revision is immutable'); END;
CREATE TRIGGER IF NOT EXISTS analytics_lineage_no_update BEFORE UPDATE ON analytics_lineage
BEGIN SELECT RAISE(ABORT, 'analytics lineage is immutable'); END;
CREATE TRIGGER IF NOT EXISTS analytics_lineage_no_delete BEFORE DELETE ON analytics_lineage
BEGIN SELECT RAISE(ABORT, 'analytics lineage is immutable'); END;
