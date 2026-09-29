-- A8 (contracts-collection.md "A8"): the second live run's shapes
-- (docs/telemetry/codex-live-0.154.0-run2.md), metadata only, under the
-- steward's §7 allowlist. Keyed by the rollout's own `session_meta.id` (or
-- its path digest) and written in the rollout's sidecar transaction. Never an
-- MCP call's `arguments` or `result.content`, a subagent's `agent_path`, a
-- collab call's `receiver_agents` or `agents_states`, nor any other content.
-- Re-runnable: every table is `IF NOT EXISTS`, no `ALTER ... ADD COLUMN`, so
-- a sidecar whose stream table was lost runs it again.

-- The fork point a `codex exec fork` names in its first `session_meta`:
-- `forked_from_ordinal_exclusive` and `history_base.{thread_id,
-- end_ordinal_exclusive, end_byte_offset}` (`NULL`: not reported or another
-- type). Written for every source with its first `session_meta`: a source
-- with a `rollout_sources` row but none here was read before A8, and the next
-- collect reads it again from byte 0.
CREATE TABLE IF NOT EXISTS rollout_forks (
    path_digest TEXT PRIMARY KEY,
    forked_from_ordinal_exclusive INTEGER,
    base_thread_id TEXT,
    base_end_ordinal_exclusive INTEGER,
    base_end_byte_offset INTEGER
) STRICT;

-- Whether the source's last tracked turn (`rollout_ingest_state`) ended with
-- `event_msg/turn_aborted` rather than `task_complete`. Written with
-- `rollout_ingest_state`; reset by a re-read from byte 0.
CREATE TABLE IF NOT EXISTS rollout_turn_ends (
    path_digest TEXT PRIMARY KEY,
    last_turn_aborted INTEGER NOT NULL CHECK (last_turn_aborted IN (0, 1))
) STRICT;

-- One row per `event_msg/turn_aborted` with a usable `turn_id`: its `reason`
-- (a tag; live: `interrupted`), `duration_ms` and the line time. The first stays.
CREATE TABLE IF NOT EXISTS codex_turn_aborts (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    reason TEXT,
    duration_ms INTEGER,
    aborted_unix_ms INTEGER,
    PRIMARY KEY (session_id, turn_id)
) STRICT;

-- One row per `item_completed` item of type `McpToolCall`, keyed by
-- `item.id`: `server` and `tool` (configuration names through the excerpt
-- rules), `status`, `readOnlyHint` and `result.isError` (0/1), and
-- `duration.{secs, nanos}` as reported (like an exec item's, the call's time
-- minus a startup window, not certified as its run time). The first stays.
CREATE TABLE IF NOT EXISTS codex_mcp_calls (
    session_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    thread_id TEXT,
    turn_id TEXT,
    server TEXT,
    tool TEXT,
    status TEXT,
    read_only_hint INTEGER CHECK (read_only_hint IS NULL OR read_only_hint IN (0, 1)),
    is_error INTEGER CHECK (is_error IS NULL OR is_error IN (0, 1)),
    duration_secs INTEGER,
    duration_nanos INTEGER,
    completed_unix_ms INTEGER,
    PRIMARY KEY (session_id, item_id)
) STRICT;

-- One row per `item_completed` item of type `SubAgentActivity` or
-- `CollabAgentToolCall`: its type, ids and status only (`agent_thread_id`
-- for the former; `status`, `sender_thread_id` and `receiver_thread_ids`, a
-- JSON array of ids, for the latter). The first stays.
CREATE TABLE IF NOT EXISTS codex_agent_items (
    session_id TEXT NOT NULL,
    item_type TEXT NOT NULL CHECK (item_type IN ('SubAgentActivity', 'CollabAgentToolCall')),
    item_id TEXT NOT NULL,
    thread_id TEXT,
    turn_id TEXT,
    status TEXT,
    agent_thread_id TEXT,
    sender_thread_id TEXT,
    receiver_thread_ids TEXT CHECK (receiver_thread_ids IS NULL OR json_valid(receiver_thread_ids)),
    completed_unix_ms INTEGER,
    PRIMARY KEY (session_id, item_type, item_id)
) STRICT;

-- `response_item/function_call.namespace` (a tag; live: `collaboration`) per
-- call id, beside `codex_tool_calls`. The first stays.
CREATE TABLE IF NOT EXISTS codex_tool_namespaces (
    session_id TEXT NOT NULL,
    call_id TEXT NOT NULL,
    namespace TEXT NOT NULL,
    PRIMARY KEY (session_id, call_id)
) STRICT;

-- Per fork session (a rollout with `history_base.thread_id`) and reported
-- total (`thread_total`, `token_count_total`): how its reported total, which
-- includes its origin's thread total at the fork point, was reconciled.
-- `reconciled`: the reported total minus the origin's last reported
-- `thread_token_usage` before `history_base.end_byte_offset` equals the sum
-- of the fork's accepted records; `discrepancy`: it does not (the
-- `codex_discrepancy` row holds the fork's own share as `reported_total`);
-- `origin_not_collected`: no certified rollout of the origin was read up to
-- the fork point, so no discrepancy is claimed; `fork_point_unknown`: no
-- usable `history_base.end_byte_offset`. `origin_total`: the origin's
-- `total_tokens` at the fork point (`NULL` unless reconciled or discrepancy).
CREATE TABLE IF NOT EXISTS codex_fork_reconciliation (
    session_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('thread_total', 'token_count_total')),
    state TEXT NOT NULL CHECK (state IN ('reconciled', 'discrepancy', 'origin_not_collected', 'fork_point_unknown')),
    origin_total INTEGER,
    observed_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (session_id, kind)
) STRICT;
