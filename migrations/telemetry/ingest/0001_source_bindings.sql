-- Why each rollout source is bound or not (TM1.1, contracts-collection.md),
-- recomputed with `rollout_sources.binding` on every collect. `IF NOT EXISTS`:
-- a sidecar whose stream table was lost re-runs this migration.
CREATE TABLE IF NOT EXISTS source_bindings (
    path_digest TEXT PRIMARY KEY,
    basis TEXT NOT NULL CHECK (basis IN ('collector_binding', 'predates_binding', 'binding_revoked', 'no_binding', 'no_match', 'ambiguous'))
) STRICT;
