-- A5 (contracts-collection.md "A5"): the thread lineage a Codex rollout's
-- first `session_meta` reports outside `source`, beside `rollout_metadata`,
-- keyed and written like it in the same sidecar transaction. Metadata only:
-- two identifiers and an enum-like excerpt. `IF NOT EXISTS`: a sidecar whose
-- stream table was lost re-runs this migration.

-- `parent_thread_id`: the top-level `session_meta.parent_thread_id` (Id; the
-- live guardian names its parent's `session_meta.id` here). `session_id`:
-- `session_meta.session_id` (Id) only when it differs from the rollout's own
-- `session_meta.id` (`NULL`: absent or equal; the live guardian reports its
-- parent's). `thread_source`: `session_meta.thread_source` (excerpt; live
-- `user`, `guardian_review`). A source with a `rollout_sources` row but none
-- here was read before A5; the next collect reads it again from byte 0.
CREATE TABLE IF NOT EXISTS rollout_threads (
    path_digest TEXT PRIMARY KEY,
    parent_thread_id TEXT,
    session_id TEXT,
    thread_source TEXT
) STRICT;
