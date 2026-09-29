# Codex 0.154.0 live certification, run 2: forks, subagents, MCP, failures

Date: 29 September 2026 (UTC). Branch `telemetry/live-2-certify` from `main`
`a568634`. Linux 7.2.3-arch1-3; Codex CLI 0.154.0 (resolved binary
`~/.local/share/mise/installs/codex/0.154.0/bin/codex`); stock Herdr 0.9.1
(two isolated disposable servers, one per TUI worker). Codex only; the owner
approved Codex usage for testing. A private login copy sat in the disposable
execution home. It was never read and was removed when the run ended
(`find /tmp -name auth.json` finds nothing). It follows
[codex-live-0.154.0.md](codex-live-0.154.0.md) and
[codex-live-0.154.0-a4.md](codex-live-0.154.0-a4.md).

**Setup.** The same scratch driver as the A4 run (a port of the live F1
harness setup): a signed v3 contract, then `launch reserve` and `ticker
run` with the release binary of this branch. There was no result submission
or integration. `HERDR_PROJECTS_TELEMETRY_COLLECT_SECS=10`. Every 4 s the
driver read the Herdr `agent list` label (labels only), ran `telemetry demo
collect` and `accounting observe-attention`. The execution home's config set
effort `low`. Each worker got one trivial brief in a disposable repository.

- **A** (TUI through Herdr, `approval_policy = 'never'`, `sandbox_mode =
  'workspace-write'`, plus one local stdio MCP server): call the MCP tool
  once, run `sleep 3`, run a command that fails (`ls` of a missing path),
  write a marker file. The MCP server was a 40-line Python script in the
  scratch directory. It exposed one no-op tool (`readOnlyHint`), read JSON-RPC
  from stdin, had no network access, and was deleted afterwards. Its log (method
  names only) shows one `tools/call`.
- **B** (TUI through Herdr, `on-request`, `read-only`, reviewer `user`):
  request escalation for a write. On the prompt, which Herdr showed as
  `blocked`, the driver sent `n` after 8 s. The plan was to send `esc` for a
  second request, but no second request came (see §4).
- **C** (`codex exec fork <A's session id> "…"`, in A's worktree): Codex's
  own non-interactive fork.
- **D** (`codex exec "…"`, in A's worktree): the stable `multi_agent`
  feature (enabled by default in 0.154.0) spawned one subagent, waited for
  it, and finished.

Codex sessions: **5** (A, B, C, D and D's spawned child), within the budget
of 6. There were no failed or repeated driver attempts. C and D were slow
because of transient transport errors (§6).

## Observed rollouts

| Rollout | `source` | `originator` | `thread_source` | Records | input | cached | cache_write | output | reasoning | total |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `…591038` (A) | `cli` | `codex-tui` | `user` | 5 | 77822 | 73344 | 0 | 183 | 0 | 78005 |
| `…ac844f` (B) | `cli` | `codex-tui` | `user` | 1 | 15476 | 0 | 0 | 96 | 0 | 15572 |
| `…2ee612` (C, fork of A) | `exec` | `codex-tui` | `user` | 1 | 15780 | 6784 | 0 | 7 | 0 | 15787 |
| `…83e4d1` (D) | `exec` | `codex_exec` | `user` | 3 | 43878 | 26496 | 0 | 73 | 0 | 43951 |
| `…261157` (D's child) | `{"subagent": {"thread_spawn": {…}}}` | `codex_exec` | `subagent` | 1 | 15195 | 0 | 0 | 7 | 0 | 15202 |

Every rollout lies under `<execution_home>/.codex/sessions/2026/09/29/`, and
all five bind by `cwd`: A, C, D and the child bind to attempt A, B to
attempt B. `usage`: A 152945 (= 78005 + 15787 + 43951 + 15202, 10 records),
B 15572. `report`: M08 168151, M15 11/11, M32 12094/193880. No quarantine.
There are two discrepancies, both for the fork (§1).

## 1. `forked_from_id`: fork shape, no replay, but totals include the origin

`codex exec fork <id> [PROMPT]` (and the interactive `codex fork`) exists.
The fork is a **new rollout file with a new session id**:

- `session_meta.forked_from_id` = A's `session_meta.id`. It also has
  `forked_from_ordinal_exclusive` 37 and `history_base.{thread_id,
  end_ordinal_exclusive, end_byte_offset}` = A's id, 37 and A's file size
  in bytes (A's rollout had 37 lines, ordinals 0–36). There is also
  `history_mode` `paginated` and `multi_agent_version`. `source` is `exec`,
  but `originator` is `codex-tui`, which is A's.
- The fork's line `ordinal`s **continue the origin's**: 37–48, not 0.
- **No replay.** The fork has 12 lines, none copied from A, and exactly one
  `token_usage_record` (its own response, whose `response_id` is in no other
  session). Nothing is double counted: attempt A counted 15787 once beside
  A's 78005.
- **But its totals include the origin's.** The record's
  `thread_token_usage` and the `token_count.info.total_token_usage` are
  93792 = A's final 78005 + the fork's 15787 (every counter adds up the same
  way). Its `turn_token_usage` is its own. So the collector records
  `thread_total` and `token_count_total` **discrepancies** for the fork
  (summed 15787, reported 93792). They are false alarms.
- `collectors sessions` shows `forked_from_id` = A. `accounting sessions`
  links C as `role: fork`, `link_basis: forked_from_id` (`certified:
  fixture`), with inclusion `unavailable: fork_replay_not_certified`.

Promoted: `session_meta.forked_from_id` → **live**, caveat
`fork_thread_total_includes_origin`.

## 2. Spawned subagent (`thread_spawn`)

Triggered through a supported feature with no special setup: `multi_agent` is
`stable`/`true` in `codex features list`, and the model has `spawn_agent`
and `wait_agent` tools.

- Child `session_meta.source` = `{"subagent": {"thread_spawn":
  {parent_thread_id, depth, agent_path, agent_nickname, agent_role}}}`, with
  `parent_thread_id` = D's `session_meta.id` and `depth` 1 (`agent_role`
  was `null`). The top-level `parent_thread_id` and `session_id` are both D's
  id, as for the A4 guardian. `thread_source` is `subagent`. The child has
  `agent_nickname`/`agent_path` keys (not collected).
- The child's `token_usage_record.session_id` is D's id, its `thread_id` is
  its own, and its `root_turn_id` is D's turn: the same shape as the
  guardian. It is keyed by its own rollout (A5 rule), so it is correct.
- **No replay, and it is separate spend.** The child has 1 record. D's
  `thread_token_usage` (43951) is the sum of D's own 3 records and excludes
  the child's 15202. No `response_id` is shared.
- The parent writes `function_call`s `spawn_agent` and `wait_agent`, with a
  new key `namespace` (`collaboration`) and **no `status`**. It also writes
  `item_completed` items `SubAgentActivity` (`kind` `started`/`completed`,
  `agent_thread_id` = the child's id; the `started` item's `id` equals the
  spawn call's `call_id`) and `CollabAgentToolCall` (`tool` `wait`, `status`
  `completed`, `sender_thread_id`, `receiver_thread_ids[]`,
  `receiver_agents[]`, `agents_states{}`). There are new top-level kinds
  `inter_agent_communication_metadata` (`trigger_turn` bool) and
  `response_item/agent_message` (`author`, `recipient`, `content[]`).
- `collectors sessions`: child `subagent {kind thread_spawn, parent D,
  depth 1}`, `thread {parent D, session_id D, source subagent}`. `accounting
  sessions`: the child is a `linked_child` of D, `link_basis:
  parent_thread_id`, inclusion `separate` (correct), `certified: fixture`.

Promoted: `session_meta.subagent_parent_thread_id` and `subagent_depth` →
**live**. Caveats widened: `parent_thread_id` →
`observed_guardian_and_thread_spawn`; `session_meta.session_id` and
`token_usage_record.session_id` → `child_reports_parent_session`;
`thread_source` → `observed_user_guardian_review_subagent`.

## 3. MCP tool call (structure only)

**No typed MCP event and no MCP `response_item` exists.** In 0.154.0 the
model calls MCP tools through the `exec` custom tool (code mode), so the call
looks like any `exec` call. It is a `custom_tool_call` `name: "exec"`,
`status: completed`, with its `custom_tool_call_output` (an array). The MCP
invocation itself is one `event_msg/item_completed`:

```
event_msg/item_completed  (item.type: McpToolCall)
  payload.{type str, thread_id str, turn_id str, started_at_ms int, completed_at_ms int}
  payload.item.{type str, id str ("exec-…"), server str, tool str, status str,
    readOnlyHint bool, duration.{secs int, nanos int},
    arguments {…} (object keyed by argument name: data-keyed),
    result.{content[].{type str, text str}, isError bool}}
```

Observed tags: `status` `completed`, `readOnlyHint` `true`,
`result.isError` `false`; `duration` 556 µs (the local stub). As for exec,
the item `id` is not the tool call's `call_id`, so they match only by
turn and time. `started_at_ms` = `completed_at_ms`. **`arguments` is keyed by
data** (argument names), so it can never be allowlisted as structure.
Today the collector keeps only `item.type` (`McpToolCall`) for this item.
`issued` counts the call under `exec`, `executed` excludes it (not a
`CommandExecution`), and `accounting` still reports `mcp_calls:
not_collected`.

The capability placeholder `mcp_tool_call.*` is replaced by the observed
rows, all still uncollected: `item_completed.item.{server, tool,
readOnlyHint}` (`not_collected`) and `item.{arguments, result}`
(`content_forbidden`).

## 4. Exec statuses, declined calls, and run time

- **Failed command.** `CommandExecution.status` = **`failed`**, with
  `exit_code` 2 and `source` `unified_exec_startup`. The `custom_tool_call`
  itself still says `status: completed`. So `custom_tool_call.status` only
  says the call was made, and the exec item's status and exit code carry
  the result. Observed item statuses so far: `completed` (exit 0) and
  `failed` (exit ≠ 0). `accounting tools` today counts the failed item as
  `unknown: status_not_certified`, so M17 is `2/2` where it should be `2/3`.
- **Declined approval.** In the 0.154.0 TUI, `n` means "No, and tell Codex
  what to do differently" (the `esc` option). It **aborts the turn**: the
  call gets a string `custom_tool_call_output` and no exec item, and then
  `event_msg/turn_aborted {turn_id, reason: "interrupted", started_at,
  completed_at, duration_ms}` is written **instead of `task_complete`**. No
  second request followed, so "No, continue without running it" (a denial
  that does not abort) was not reached. The call still says `status:
  completed`. B6b saw one `blocked` wait (12094 ms), and M16 counts that call
  as `accepted` `human_routed`. That is the documented caveat (a denied
  approval also ends the wait), and here it was a denial.
- **`function_call.status`**: absent on every live `function_call`
  (`wait`; now `spawn_agent` and `wait_agent`). It stays `fixture`.
- **Run time (M18).** For `sleep 3`, the call → output lines were 3060 ms
  apart (no approval, policy `never`). The item's `duration` was 2.879 s, and
  its `started_at_ms` → `completed_at_ms` also 2879 ms. The failing `ls` and
  the marker `printf` show 3.6 µs and 3.1 µs, which is too short for any real
  process. So the item's duration is **not** run time: it undercounts by the
  startup window (about 0.12–0.18 s here), and a command that ends inside that
  window shows about 0. It is a lower bound, and call → output is an upper
  bound (it includes model-side handling and, under `on-request`, the
  approval wait). No record gives the true run time, so M18 stays
  `unavailable`. The caveat `startup_not_run_time` stands, now documented
  as "run time minus the startup window".

## 5. Cache-write convention

`cache_write_input_tokens` was **0 in all 11 records** (and in every record
of the earlier live runs). `cached_input_tokens ≤ input_tokens` always
held, and `total = input + output`. Within a session the cache warms: A's
records show cached 11904 of 15350 input, then 15232/15487, 15360/15567,
15360/15664, 15488/15754. The cached part is part of `input`, not added to it.
The fork's first record got 6784 cached of 15780 (a warm prefix from its
origin, 2 min later). A cold primary (B, D's first record, the child) had 0.
**The convention for a non-zero cache write stays uncertified**, because
Codex/OpenAI never reported one. The `overlap_with_input_not_certified`
caveat stays, and B3 keeps entries with cache writes unpriced (none exist
live).

## 6. Provider errors, throttling, rate limits

- **Transient transport errors occurred naturally** during C and D. Codex's
  stderr showed failed WebSocket connects (DNS lookup failures, one `504
  Gateway Timeout`), `Reconnecting... 2/5`–`5/5`, and a fallback to HTTPS.
  **None of this reached the rollout**: no `error`, `stream_error` or
  `warning` event, and nothing else typed. It appears only as a long silence:
  C's `time_to_first_token_ms` was 139608 (a 119.8 s gap between lines) and
  D's 114669 (85.9 s). M38/M39 therefore stay uncertifiable from rollouts.
- `rate_limit_reached_type` was `null` and `secondary` `null` in all 11
  `token_count` snapshots. Both stay `fixture`. New keys present with
  `null`: `individual_limit`, `spend_control_reached`. `credits` is an
  object with a balance string (not collected).
- `primary`: `used_percent` 43.0, `window_minutes` 10080 and `limit_id`
  `codex` everywhere. `resets_at` 1791049774 in 10 snapshots, the same as
  the A4 run's 21, but **1791049779 in the child's one snapshot (+5 s)**.
  That is the first jitter seen. `plan_type` was `null` in the 4 snapshots
  of C and D, and `pro` in A, B and the child.

## 7. Turn durations and idle gaps (A7's 600 s threshold)

| Turn | End | `duration_ms` | `time_to_first_token_ms` | Longest gap between lines |
| --- | --- | --- | --- | --- |
| A | `task_complete` | 27447 | 3986 | 5.9 s |
| B | `turn_aborted` | 20290 | — | 8.7 s (the approval prompt) |
| C (fork) | `task_complete` | 151301 | 139608 | 119.8 s (transport retries) |
| D | `task_complete` | 131826 | 114669 | 85.9 s (transport retries) |
| D's child | `task_complete` | 13640 | 13447 | 7.7 s |

The longest silence inside a healthy turn was 120 s, a fifth of
`FINAL_EVENT_IDLE_MS` (600 s). The A4 run's human approval wait was 37 s.
600 s holds for model and transport stalls. A human who leaves an approval
prompt open for more than 10 minutes will still make the turn `missing`
until its event arrives (then `recovered`), as designed. **But an aborted
turn never gets `task_complete`**: A7 ignores `turn_aborted`, so B's turn is
`open` now and becomes a **false `final_event_missing`** 600 s after the
file goes idle.

## Capabilities (`collectors::codex_fields`, `CAPABILITIES`)

| Field | Before | After |
| --- | --- | --- |
| `session_meta.forked_from_id` | fixture, `semantics_not_certified` | **live**, `fork_thread_total_includes_origin` |
| `session_meta.subagent_parent_thread_id` | fixture | **live** (`from_source_subagent`) |
| `session_meta.subagent_depth` | fixture | **live** (`from_source_subagent`) |
| `session_meta.parent_thread_id` | live, `observed_for_guardian_only` | live, `observed_guardian_and_thread_spawn` |
| `session_meta.session_id`, `token_usage_record.session_id` | live, `guardian_reports_parent_session` | live, `child_reports_parent_session` |
| `session_meta.thread_source` | live, `observed_user_and_guardian_review_only` | live, `observed_user_guardian_review_subagent` |
| `mcp_tool_call.*` (placeholder) | not_collected | replaced by `item_completed.item.{server, tool, readOnlyHint}` not_collected, `item.{arguments, result}` content_forbidden |

Unchanged: `function_call.status` (fixture; never valued live),
`rate_limits.secondary.*` and `rate_limit_reached_type` (fixture; `null`
live), `subagent_detail` (live, guardian only; the `thread_spawn` child has
no `other` tag), `cache_write_input_tokens` caveat. No collection changed.

## Conformance fixtures (hand-written, sentinels only)

`tests/fixtures/telemetry/codex-conformance/live2-tools.jsonl` and
`live2-fork.jsonl`, in the shapes above, with `LIVE2LEAK_*` in every content
field (MCP `arguments`/`result`, command fields, tool `input`/`output`,
spawn and wait arguments and outputs, `agent_path`, the declined output,
`thread_settings_applied` paths, `multi_agent_version`, `credits`). They
are not copied from any rollout. They are kept out of `CASES`, so the corpus
totals are unchanged. The test `live_run2_shapes_are_collected_without_content`
checks: no quarantine; the fork's own record counted once (attempt 2000 =
1680 + 320) and its `thread_total`/`token_count_total` discrepancies (320 vs
2000); `collectors tools` rows, including `exec-f1` `failed`/2 and
`spawn_agent`/`wait_agent` without status; envelopes (`McpToolCall`,
`SubAgentActivity` and `CollabAgentToolCall` typed only, `namespace`
dropped, no envelope for `turn_aborted`/`thread_settings_applied`); the
aborted turn `open`; M16 issued 6 and executed 2; M17 `1/1` with the failed
item `status_not_certified`; and no sentinel, MCP server name or MCP tool
name in the sidecar (with WAL/SHM) or any output. The assertions for the
fork discrepancy, the aborted turn and M17 pin **today's** behaviour. The
follow-ups below change them.

## Proposed allowlist (for a steward §7 review; not implemented)

| Kind | Allow (class) | Never |
| --- | --- | --- |
| `item_completed`, `item.type = McpToolCall` | `item.id` (Id), `item.server` (Tag, the configured MCP server name), `item.tool` (Tag, the tool name), `item.status` (Tag), `item.readOnlyHint` (Bool/Tag), `item.result.isError` (Bool/Tag), `item.duration.{secs,nanos}` (Number, caveat: the MCP call's wall time as Codex measured it, certified only for a local stub) | `item.arguments` (data-keyed object), `item.result.content` and every other `result` field |
| `item_completed`, `item.type = CommandExecution` | as A6; add `status` value `failed` (exit ≠ 0) to the certified tags | unchanged |
| `item_completed`, `item.type = SubAgentActivity` | `item.id` (Id; equals the spawn `call_id`), `item.kind` (Tag: `started`, `completed`), `item.agent_thread_id` (Id) | `item.agent_path` |
| `item_completed`, `item.type = CollabAgentToolCall` | `item.id` (Id; equals the call's `call_id`), `item.tool` (Tag), `item.status` (Tag), `item.sender_thread_id` (Id), `item.receiver_thread_ids[]` (Id) | `item.receiver_agents`, `item.agents_states` (keyed by thread id; values not observed) |
| `event_msg/turn_aborted` | `turn_id` (Id), `reason` (Tag: `interrupted`), `duration_ms` (Number), `started_at`, `completed_at` (Unix s) | — |
| `session_meta` (fork) | `forked_from_ordinal_exclusive` (Number), `history_base.{thread_id (Id), end_ordinal_exclusive (Number)}` | `history_base.end_byte_offset` (not needed) |
| `response_item/function_call` | `namespace` (Tag, e.g. `collaboration`) | `arguments` (unchanged) |

MCP server and tool names are configuration names chosen by the operator,
not model or user text. Like `model_provider`, they should use the excerpt
rules (rule 1–5) and never be free text. They reveal which integrations a
project uses, so the steward should decide whether a digest suffices.

## Canary

The needles were 40-byte slices of every string in all five rollouts' payloads
(1572), plus the brief markers. The targets were `telemetry.db` and every
`herdr-projects` output and log (Codex's own `exec` stdout excluded). Every
hit was a fragment of the attempt worktree path or an attempt or session id,
sometimes cut with JSON punctuation. There were **0 content hits**.

## Cleanup

Every process started (two Herdr servers, the ticker, TUI workers, `codex
exec`, the MCP stub) was stopped. Afterwards, `ps` shows only the owner's
pre-existing Herdr server, ticker (pid 607926) and Codex processes. No
`auth.json` exists under `/tmp`. The MCP stub script and its log were
deleted. Rollouts and outputs stayed in the scratch directory.

## Follow-ups

- **Lane A (fork totals):** reconcile a fork against its reported totals
  minus the origin's thread total at `forked_from_ordinal_exclusive` (the
  origin's last `thread_token_usage` at or before that ordinal), or record
  the discrepancy with a `forked_history_base` reason instead of
  `thread_total`. Collect `forked_from_ordinal_exclusive` and
  `history_base.thread_id` for that (proposal above).
- **Lane A (final events):** treat `event_msg/turn_aborted` with the tracked
  turn's id as the turn's final event (`final_event` state `aborted`, never a
  `final_event_missing` gap). Collect `turn_id`, `reason`, `duration_ms`.
- **Lane A (§7, after steward review):** the MCP, `SubAgentActivity`,
  `CollabAgentToolCall` and `namespace` metadata above. Then `accounting`
  can count MCP calls by server and tool and set `mcp_calls` to `live`.
- **Lane B (M17):** certify `CommandExecution.status = failed` with a
  non-zero exit code as a failed execution (M17 here: `2/3`, not `2/2`).
  Keep other statuses `status_not_certified`.
- **Lane B (M16 accepted):** a call whose turn ends in `turn_aborted`
  (`interrupted`) right after its output was declined, not accepted. Once A
  collects `turn_aborted`, report such calls as `declined_or_aborted`
  rather than `human_routed` accepted.
- **Lane B (session graph):** fork links are now live-certified and the fork
  replays no records, so fork inclusion can be `separate` instead of
  `unavailable: fork_replay_not_certified`. `thread_spawn` links
  (`parent_thread_id`) are live too (certified `fixture` → `live`). Update
  `tests/telemetry_accounting.rs` accordingly.
- **Lane B (quota):** `resets_at` jittered by 5 s once (1791049774 vs
  …779) within one window. Match windows with a small tolerance (for example
  ≤ 60 s), not by exact `resets_at`. `plan_type` can be `null` in a snapshot
  from an `exec` session; do not treat that as another account.
- **Lane B (M18):** still unavailable. If a lower bound is ever useful, the
  exec item's `duration` is "run time minus about 0.1–0.2 s startup" for
  commands that outlive the startup window. Never report it as run time.
- **M38/M39:** transport errors and retries are not in 0.154.0 rollouts
  (only a long `time_to_first_token_ms`). They stay unavailable.
