# Codex 0.154.0 live certification: A4/B4 fields, quota, attention

Date: 29 September 2026 (UTC). Branch `telemetry/live-a4-certify` from
`main` `c2c4c86`. Linux 7.2.3-arch1-3; Codex CLI 0.154.0 (resolved binary
`~/.local/share/mise/installs/codex/0.154.0/bin/codex`); stock Herdr 0.9.1
(two isolated disposable servers, one per worker). Codex only. A private
login copy sat in the disposable execution home. It was never read and was
removed once the workers stopped. Owner decision 5 in
[phase2-lanes.md](phase2-lanes.md) authorized the run. It follows
[codex-live-0.154.0.md](codex-live-0.154.0.md).

**Setup.** A scratch driver ported the setup of the live F1 harness
(`live_f1_worker_result_integrates_and_dependent_launches_on_integrated_sha`),
as the earlier telemetry demo did. It ran the steps from signed v3 contract
to `launch reserve` and `ticker run`, with the release binary of this
branch. There was no result submission or integration. The ticker ran with
`HERDR_PROJECTS_TELEMETRY_COLLECT_SECS=10`. Every 4 s the driver took three
steps: it read the Herdr `agent list` status label (labels only), ran
`telemetry demo collect`, and ran `telemetry demo accounting
observe-attention`. The execution home's Codex config was `approval_policy
= 'on-request'`, `sandbox_mode = 'read-only'` and effort `low`. Each worker
got one trivial brief: run one shell command that writes a marker file, so
the read-only sandbox needs an approval.

- **A** (`attempt-ed9aa867…`, `approvals_reviewer = 'user'`): the prompt
  waited for the human. After about 36 s the driver sent `y` through
  `herdr agent send-keys`, and the file was written.
- **B** (`attempt-77a22d76…`, `approvals_reviewer = 'auto_review'`): Codex's
  automatic reviewer (the guardian) approved the request. That produced one
  guardian child session.

Codex sessions: **2 workers (A, B), plus the guardian session that B
started.** Two earlier driver attempts made no Codex model call. The first
exceeded profile preparation's 60 s budget while hashing the 262 MB binary
with a debug build. In the second, Codex refused to start because
`approval_policy = "untrusted" is no longer supported` in 0.154.0.

Afterwards the driver ran `collect`, `collectors sessions`, `usage`,
`accounting sync|sessions|quota|attention`, `collectors capabilities` and
`report --text`. Rollouts and outputs stayed in the scratch directory.

## Observed rollouts

| Rollout | `source` | `thread_source` | Model | Records | input | cached | output | reasoning | total |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `…f9bddb` (A) | `cli` | `user` | `gpt-6-astra` / `low` | 3 | 46615 | 42496 | 122 | 0 | 46737 |
| `…9d8cd3` (B) | `cli` | `user` | `gpt-6-astra` / `low` | 2 | 29667 | 26496 | 93 | 0 | 29760 |
| `…0ec86a` (B's guardian) | `{"subagent": {"other": "guardian"}}` | `guardian_review` | `codex-auto-review` / `low` | 1 | 7288 | 4864 | 174 | 106 | 7462 |

In all three, the sum of the records equals the final `thread_token_usage`
and `token_count.info.total_token_usage` (six fields), and every rollout lies
under `<execution_home>/.codex/sessions/2026/09/29/`. Each `cwd` is its
attempt worktree, and all three are bound (the guardian runs in B's
worktree). `usage`: A 46737 and B 37222 (= 29760 + 7462, records 3).
`report`: M08 83570 (= 46615 + 29667 + 7288), M09 389, M15 6/6. No
quarantine, no `thread_total` discrepancy.

## 1. A4 fields

| Field | Live value | Result |
| --- | --- | --- |
| `session_meta.model_provider` | `openai` in all 3 | **live** |
| `session_meta.forked_from_id` | key absent in all 3 (no fork happened) | stays **fixture** |
| `source.subagent` → `subagent_kind` | `other` for the guardian (`source.subagent.other` = `"guardian"`), `null` for the primaries | **live** |
| `subagent_parent_thread_id`, `subagent_depth` (from `thread_spawn`) | no spawned subagent occurred | stay **fixture** |
| `token_usage_record` line `timestamp` | 6 of 6 timed; see below | **live** |
| `rate_limits.secondary.*` | `null` in 6 of 6 snapshots (and in 15 more from the earlier demo home) | stays **fixture** |
| `rate_limits.rate_limit_reached_type` | key present, `null` in 6 of 6 (no limit reached) | stays **fixture** |

**Record times.** They are non-decreasing within each rollout and fall
inside their session: A at 04:11:25.844, 04:12:05.535 and 04:12:06.945,
with the session starting at 04:10:38.928; B at 04:13:42.130 and
04:13:51.963; the guardian at 04:13:48.233. They track the driver's wall
clock closely. The driver sent the approval key at 04:12:02.649 and the
tool output line is stamped 04:12:02.659. Herdr's first `blocked` label came
at 04:11:26.376, 0.5 s after A's first record. `collectors sessions`
reports the same span (`first_unix_ms` 1790655085844). Timestamps are UTC;
rollout file names use local time.

Also observed live with a value, so promoted from `fixture` as well:
`session_meta.{originator (codex-tui), source (cli)}`, `turn_id` of
`turn_context`, `task_started`, `token_usage_record` and `task_complete`,
`token_usage_record.{session_id, response_id}`, and
`task_complete.{duration_ms, time_to_first_token_ms}`.
`token_usage_record.session_id` gets the caveat
`guardian_reports_parent_session`, because the guardian's records carry the
parent's id (next section).

Promoted in `collectors::codex_fields` and the `CAPABILITIES` literal
(`tests/telemetry_conformance.rs`): `line.timestamp`, the fields listed
above, `session_meta.model_provider` and `session_meta.subagent_kind`.
Nothing else changed.

## 5. Guardian child session: parent id, and no double count

- The guardian **names its parent**, but outside `source.subagent`:
  `session_meta.parent_thread_id` (a top-level key) and
  `session_meta.session_id` both equal B's `session_meta.id`. Its own
  `session_meta.id` is distinct. Its `thread_source` is `guardian_review`,
  and it also has `multi_agent_version`. This refutes "a guardian carries no
  parent id" (contracts-collection.md, *Parent session id*). None of these
  keys is collected, so B2 still reports it as `guardian`, `unlinked_child`,
  `no_native_parent_evidence`.
- Its `token_usage_record` has `session_id` = the parent's id, `thread_id`
  = its own id, `root_turn_id` = the parent's turn, and `turn_id` = its own
  turn. The collector keys it by its rollout's `session_meta.id`, so it is a
  separate bound session (records 1), which is correct.
- **No duplication.** The guardian's `response_id` appears in no other
  session. The parent's `thread_token_usage` (29760) is the sum of its own
  two records and excludes the guardian's 7462. The guardian's record falls
  in time between the parent's two records. So the guardian's usage is
  additional and is counted once in B's attempt (37222). Counting it is
  correct, because it is real spend on the parent's approval.
- Primary sessions have `session_meta.session_id` = `id` and
  `token_usage_record.thread_id` = `session_id`.

## 2. Quota semantics

- **`resets_at` is fixed within a window. There was no drift or jitter.**
  Every snapshot reported `primary.resets_at` = 1791049774 (3 October
  2026 17:49:34 UTC): 6 in this run (04:12–04:13) and 15 from the earlier
  telemetry demo's home (00:24–00:33). That is 21 snapshots over 3 h 50 min
  and two execution homes, all with `window_minutes` 10080 (window start
  26 September 17:49:34), `limit_id` `codex` and `plan_type` `pro`.
  `accounting quota` built one window (`first_observation`, 6 trusted, 0
  flagged).
- **`used_percent` is coarse.** It held at 43.0 across all 21 snapshots,
  while the demo and this run used about 220k input tokens. The 28 September run saw 42–43. The
  15-minute stale threshold cannot be certified from this: at 1 %
  granularity a 15-minute-old value is rarely wrong. It is kept as is.
- **One execution home holds one login.** The two homes (the demo home and
  this run's) held private copies of the same login. They reported
  identical `limit_id`, `plan_type`, `used_percent` and `resets_at` at
  overlapping times, so the window belongs to the account, not the home.
  Account identity itself was not checked, because credentials are never
  read. Consequence for B4: `account = home_digest` splits one account used
  from two homes into two accounts with duplicate windows. Their
  `observed_increase` values must never be added together. Within one home,
  "one home = one account" held.
- **`token_count` can lag its record.** A's first `token_count`, the
  carrier of the rate-limit snapshot, is stamped 04:12:02.662. That is
  37 s after its `token_usage_record` and just after the approval. Quota
  `observed_unix_ms` uses the `token_count` line time, so a snapshot can
  look newer than the usage it follows. A's M40 is `no_observation`,
  because the fresh home had no snapshot before dispatch. B's is
  `remaining 57%`, age 44577 ms, `fresh`, with secondary `not_reported`.

## 3. Attention: a Codex approval prompt is `blocked`

- **Certified live.** Herdr's `agent list` label for A went `working` →
  `blocked` at 04:11:26.376, the first sample after the prompt. It stayed
  `blocked` for 10 consecutive 4 s samples, then went `working` 4 s after
  the approval key and `idle` 4 s later. B, whose approval was
  auto-reviewed, went `idle` → `working` → `idle` and **never showed
  `blocked`**. The 6.3 s guardian review appeared as `working`.
- **Product path.** Every `accounting observe-attention` pass returned
  `{"attempts": n, "states": n, "gaps": {}}`: no gap, and no
  `identity_mismatch` on the recorded pane. `accounting attention` for A
  shows one wait: `observed_transition` → `closed`, 37848 ms (04:11:26.405
  → 04:12:04.253), `interventions` 1. B shows 0 waits. There is one
  `not_observed` gap (04:12:25 → 04:12:51), from the time the driver
  stopped the ticker to launch B, which exceeds 2 × the 10 s interval. The
  report gives M32 `37848/228232`. M31 is `empty_denominator` (nothing
  decided). M33 stays `attention_reason_not_exposed`.
- **What this means for M33.** `blocked` is still untyped. An auto-reviewed
  approval never shows as `blocked`, so `blocked` counts only prompts routed
  to the human. The rollout holds no typed approval request or decision
  record: no `exec_approval_request` and no `guardian_assessment`. The gap
  between the tool call and its output does measure the wait (below). For
  A, `custom_tool_call` came at 04:11:25.749 and its output at 04:12:02.659,
  36.9 s, against Herdr's 37.8 s. For B the gap was 6.4 s, the guardian's
  `duration_ms` 6337.
- Follow-up (lane B, not done here): `accounting attention` still says
  `signal.certified: fixture`, and its basis text says Codex prompts are
  uncertified. Flip both to `live` for Codex approval prompts. Update
  `tests/telemetry_accounting.rs` accordingly.

## 4. Tool and exec record shape (structure only)

Key census over the three rollouts (key paths and value types, never
values; `[]` marks array elements). Only tag values (`type`, `status`,
`source`, `name`) were looked at, to name the variants. The census has no
object keyed by data.

**No typed `exec_command_end`, `exec_command_begin` or `mcp_tool_call_end`
event is written to 0.154.0 rollouts**, and neither are approval events.
Tool activity appears in three places:

```
response_item/custom_tool_call            (name: "exec")
  payload.{type str, id str, call_id str, name str, status str, input str}
  payload.internal_chat_message_metadata_passthrough.{turn_id str, create_time float}
  timestamp str, ordinal int
response_item/function_call               (name: "wait")
  payload.{type str, id str, call_id str, name str, arguments str}
  payload.internal_chat_message_metadata_passthrough.{turn_id str, create_time float}
response_item/custom_tool_call_output | function_call_output
  payload.{type str, id str, call_id str, output str | output[].{type str, text str}}
  payload.internal_chat_message_metadata_passthrough.{turn_id str, create_time float}
event_msg/item_completed                  (item.type: CommandExecution | UserMessage | AgentMessage)
  payload.{type str, thread_id str, turn_id str, started_at_ms int, completed_at_ms int}
  payload.item.{type str, id str}
  CommandExecution: payload.item.{status str, source str, exit_code int,
    duration.{secs int, nanos int}, process_id str, command[] str, cwd str,
    parsed_cmd[].{type str, cmd str}, stdout str, stderr str,
    aggregated_output str, formatted_output str}
  AgentMessage: payload.item.{phase str, content[].{type str, text str}}
  UserMessage:  payload.item.{client_id str, content[].{type str, text str, text_elements[]}}
```

Observed tags: `custom_tool_call.status` `completed`;
`CommandExecution.status` `completed`, `source` `unified_exec_startup`,
`exit_code` 0.

Findings that decide the allowlist:

- **The exec item and the tool call have no shared key.**
  `CommandExecution.item.id` (`exec-…`) is neither the tool call's
  `call_id` nor derived from it. They match only on `turn_id` and time. The
  call and its output share `call_id`.
- **`duration` and `started_at_ms`/`completed_at_ms` are not the command's
  run time.** Both were about 2 µs, and the item's start equals its
  completion. `source` is `unified_exec_startup`, and the item is written
  after approval. The call → output line times measure the call, including
  any approval wait.
- `process_id` is an OS process id: metadata, but of no use.

**Proposed allowlist** (for a steward §7 revision; not implemented):

| Kind | Allow (class) | Never |
| --- | --- | --- |
| `response_item/custom_tool_call`, `function_call` | `call_id` (Id), `name` (Tag, the tool name, e.g. `exec`, `wait`), `status` (Tag), `internal_chat_message_metadata_passthrough.turn_id` (Id), line `timestamp` | `input`, `arguments`, `id` (not needed) |
| `response_item/*_call_output` | `call_id` (Id), line `timestamp` | `output`, `output[].text` |
| `event_msg/item_completed` | `item.type` (Tag), `turn_id`, `thread_id` (Id), and for `CommandExecution` `item.id` (Id), `item.status`, `item.source` (Tag), `item.exit_code` (Number), `item.duration.{secs,nanos}` (Number, caveat `startup_not_run_time`) | `command`, `cwd`, `parsed_cmd`, `stdout`, `stderr`, `aggregated_output`, `formatted_output`, `process_id`, `content`, `client_id`, `phase` |

This keeps the call and exit metadata separable from command, arguments
and output, as the held A4 proposal needed. However, the proposal's named
kinds (`exec_command_end`, `mcp_tool_call_end`) do not exist in 0.154.0
rollouts, so its §7 text should name the kinds above instead. The
`exec_command_end.*` and `mcp_tool_call_end.*` capability rows stay
`not_collected`. No MCP tool was called, so MCP shapes remain unobserved.
Taking metadata from `response_item`, which is `content_forbidden` as a
whole today, needs that revision.

## Canary

The needles were the brief markers and wording, plus 40-byte slices of
every string in every rollout payload (132 needles). The targets were
`telemetry.db` and every CLI output and log. All hits were fragments of the
attempt worktree path and attempt ids (only `[A-Za-z0-9_./-]`), so there
were **0 content hits**.

## Follow-ups

- **A4+ (lane A, §5/§7 revision):** collect `session_meta.parent_thread_id`
  (top-level, Id) and `thread_source` (Tag). Also collect the
  `source.subagent.other` string (Tag, `guardian`) as the kind's detail.
  Then B2 can link the guardian through native evidence (`parent_thread_id`
  = the parent's `session_meta.id`).
- **B4:** an account spanning several homes shows up as several accounts
  (see quota). Either document this, or key accounts on an identity that
  does not come from credentials, if Codex ever reports one.
- **B6b:** flip the attention signal to `live` for Codex approval prompts
  (above).
- **B5:** the tool metadata allowlist above, after steward review.
- Profile preparation hashes the agent binary within a 60 s budget. A debug
  build of `herdr-projects` cannot finish that for the 262 MB Codex binary.
  Release builds can.
