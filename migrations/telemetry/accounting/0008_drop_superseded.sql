-- Accounting stream 8: drop the derived tables that later versions replaced
-- and that nothing reads or writes any more (owner decision, 2026-09-29):
-- `session_graph` (v2, replaced by `session_nodes` in v6),
-- `quota_observations` (v4, replaced by `quota_window_observations` in v6),
-- `session_nodes` (v6, replaced by `session_graph_nodes` in v7).
-- All three are rebuilt-on-sync projections, so no source data is lost.
-- `IF EXISTS` keeps this re-runnable when the stream table is lost and every
-- migration runs again.
DROP TABLE IF EXISTS session_graph;
DROP TABLE IF EXISTS quota_observations;
DROP TABLE IF EXISTS session_nodes;
