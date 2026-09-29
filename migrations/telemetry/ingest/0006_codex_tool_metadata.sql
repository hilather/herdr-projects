-- A6 (contracts-collection.md "A6"): Codex tool and exec METADATA, never
-- content, beside the `codex` stream's tables, keyed by the rollout's own
-- `session_meta.id` and written in the same sidecar transaction. Identifiers,
-- enum-like excerpts, numbers and line times only: never a tool's input,
-- arguments or output, nor a command, its cwd, parsed form or output.
-- `IF NOT EXISTS`: a sidecar whose stream table was lost re-runs this migration.

-- Rollout sources whose tool metadata was read from byte 0. A source with a
-- `rollout_sources` row but none here was read before A6; the next collect
-- reads it again from byte 0.
CREATE TABLE IF NOT EXISTS codex_tool_sources (
    path_digest TEXT PRIMARY KEY
) STRICT;

-- One row per tool call id of a session, from `response_item`
-- `custom_tool_call` / `function_call` (`call_kind`, `name`, `status`,
-- `internal_chat_message_metadata_passthrough.turn_id`, line time) and its
-- `custom_tool_call_output` / `function_call_output` (`output_kind`, line
-- time). `call_kind` `NULL`: no call seen (an output only); `output_kind`
-- `NULL`: no output seen yet. The first call and the first output stay.
CREATE TABLE IF NOT EXISTS codex_tool_calls (
    session_id TEXT NOT NULL,
    call_id TEXT NOT NULL,
    call_kind TEXT CHECK (call_kind IN ('custom_tool_call', 'function_call')),
    name TEXT,
    status TEXT,
    turn_id TEXT,
    called_unix_ms INTEGER,
    output_kind TEXT CHECK (output_kind IN ('custom_tool_call_output', 'function_call_output')),
    output_unix_ms INTEGER,
    PRIMARY KEY (session_id, call_id)
) STRICT;

-- One row per `event_msg/item_completed` whose `item.type` is
-- `CommandExecution`, keyed by `item.id`: `thread_id`, `turn_id`,
-- `item.{status, source}` (excerpts), `item.exit_code`, and
-- `item.duration.{secs, nanos}` as reported. The duration is NOT the
-- command's run time (0.154.0: the unified exec startup, about 2 µs; caveat
-- `startup_not_run_time`). `completed_unix_ms`: the line time. The first stays.
CREATE TABLE IF NOT EXISTS codex_exec_items (
    session_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    thread_id TEXT,
    turn_id TEXT,
    status TEXT,
    source TEXT,
    exit_code INTEGER,
    startup_duration_secs INTEGER,
    startup_duration_nanos INTEGER,
    completed_unix_ms INTEGER,
    PRIMARY KEY (session_id, item_id)
) STRICT;
