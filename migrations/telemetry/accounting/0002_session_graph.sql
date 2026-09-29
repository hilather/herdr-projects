-- Stream `accounting` 2 (docs/telemetry/contracts-accounting.md §3): the
-- session graph and model segments, rebuilt whole with the usage ledger by
-- each sync. Counters only; no content (contracts §7).
-- One node per rollout source; `included` nodes are covered by an inclusive
-- parent (native evidence only) and are never added to it.
CREATE TABLE IF NOT EXISTS session_graph (
    path_digest TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('primary', 'guardian', 'subagent')),
    linkage TEXT NOT NULL CHECK (linkage IN ('root', 'included', 'unlinked_child', 'unresolved')),
    parent_path_digest TEXT,
    evidence TEXT CHECK (evidence IS NULL OR evidence = 'same_session_prefix'),
    attempt_id TEXT,
    inclusive_total INTEGER CHECK (inclusive_total IS NULL OR inclusive_total >= 0),
    CHECK ((linkage = 'included') = (parent_path_digest IS NOT NULL)),
    CHECK ((parent_path_digest IS NULL) = (evidence IS NULL))
) STRICT;
-- Counted entries of a session by position: a model switch opens a new
-- `model` segment (segment 1, 2, ...); a turn whose records carry several
-- models is `mixed`, a record without model evidence `unallocated` (segment 0).
CREATE TABLE IF NOT EXISTS model_segments (
    session_id TEXT NOT NULL,
    bucket TEXT NOT NULL CHECK (bucket IN ('model', 'mixed', 'unallocated')),
    segment INTEGER NOT NULL CHECK (segment >= 0),
    model TEXT,
    first_position INTEGER NOT NULL,
    last_position INTEGER NOT NULL CHECK (last_position >= first_position),
    entries INTEGER NOT NULL CHECK (entries > 0),
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    reasoning_tokens INTEGER NOT NULL,
    total_tokens INTEGER NOT NULL CHECK (total_tokens = input_tokens + output_tokens),
    PRIMARY KEY (session_id, bucket, segment),
    CHECK ((bucket = 'model') = (segment > 0)),
    CHECK ((bucket = 'model') = (model IS NOT NULL))
) STRICT;
-- A ledger synced before this version has no graph: it reads as not synced
-- until the next sync rebuilds all three.
DELETE FROM usage_ledger;
