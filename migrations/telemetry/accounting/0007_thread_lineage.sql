-- Stream `accounting` 7 (docs/telemetry/contracts-accounting.md §3, §8): consume
-- lane A's A5 thread lineage (ingest 0005 `rollout_threads`, certified `live`).
-- A guardian names its parent in `rollout_threads.parent_thread_id`, a new
-- link basis `session_nodes` cannot hold (its CHECK is fixed), so the graph
-- moves to `session_graph_nodes`. `session_nodes` (version 6) is superseded:
-- no longer written or read, left as it was (nothing is dropped).
-- Identifiers and enums only; no content (contracts §7). Derived: rebuilt by each sync.
CREATE TABLE IF NOT EXISTS session_graph_nodes (
    path_digest TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('primary', 'guardian', 'subagent', 'fork')),
    linkage TEXT NOT NULL CHECK (linkage IN ('root', 'included', 'linked_child', 'unlinked_child', 'unresolved')),
    parent_path_digest TEXT,
    evidence TEXT CHECK (evidence IS NULL OR evidence = 'same_session_prefix'),
    attempt_id TEXT,
    inclusive_total INTEGER CHECK (inclusive_total IS NULL OR inclusive_total >= 0),
    -- The collected parent session of a `linked_child` and the native field linking it:
    -- `parent_thread_id` (A4 `subagent_parent_thread_id`), `thread_parent_thread_id`
    -- (A5 `rollout_threads.parent_thread_id`) or `forked_from_id`.
    parent_session_id TEXT,
    link_basis TEXT CHECK (link_basis IS NULL OR link_basis IN ('parent_thread_id', 'thread_parent_thread_id', 'forked_from_id')),
    -- Why an `unlinked_child` has no parent, and the parent id it names when not collected.
    parent_reason TEXT CHECK (parent_reason IS NULL OR parent_reason IN ('no_native_parent_evidence', 'parent_not_collected')),
    claimed_parent_session_id TEXT,
    -- The session names a `forked_from_id` (its records may replay the parent's).
    forked INTEGER NOT NULL DEFAULT 0 CHECK (forked IN (0, 1)),
    CHECK ((linkage = 'included') = (parent_path_digest IS NOT NULL)),
    CHECK ((parent_path_digest IS NULL) = (evidence IS NULL)),
    CHECK ((linkage = 'linked_child') = (parent_session_id IS NOT NULL)),
    CHECK ((parent_session_id IS NULL) = (link_basis IS NULL)),
    CHECK ((linkage = 'unlinked_child') = (parent_reason IS NOT NULL))
) STRICT;

-- A ledger synced before this version has no graph nodes here: it reads as
-- not synced until the next sync rebuilds everything.
DELETE FROM usage_ledger;
