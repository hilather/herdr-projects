# Collection contracts (lane A)

Owned by lane A ([phase2-lanes.md](phase2-lanes.md)). Extends
[contracts.md](contracts.md) §5; §0 rules apply unchanged.

## A1 (TM1.1): canonical collector bindings

**Canonical table** `collector_bindings` (migration 0052): append-only
revisions `(attempt_id, revision)` with `state` ∈ `active`, `revoked`,
`predates_binding`, `collector` (the launched profile kind), the profile's
`execution_home`, `unix_ms` (controller `now`) and `source`. Updates and
deletes abort. Analytics only: never read to grant launch.

- Revision `active` is written in the `apply_launch_started` transaction from
  the attempt's retained effective profile (no `LaunchInputs` change).
- 0052 gives every attempt that exists at migration one `predates_binding`
  revision (`source = migration_0052`).
- `herdr-projects telemetry <slug> collectors revoke <attempt>` appends a
  `revoked` revision (copying collector and home) and prints
  `{binding, written}`. Revoking a revoked binding writes nothing
  (`written: false`); an attempt without a binding or with `predates_binding`
  is refused.
- `telemetry <slug> collectors bindings` (read-only) prints every revision and,
  per rollout source, `{session_id, binding, attempt_id, basis}`.

**Codex binder.** Contracts §5 rules 2 (cwd under the attempt's worktree) and
3 (`session_meta.timestamp ≥ decided_unix_ms`) select candidate attempts; the
latest binding revision then decides:

| Latest revision | Rule 1 home | Bound when | `basis` |
|---|---|---|---|
| `active` | binding's `execution_home` | always | `collector_binding` |
| `revoked` at *r* | binding's `execution_home` | session timestamp < *r* | `collector_binding`, else `binding_revoked` (unbound) |
| `predates_binding` (or a store before 0052) | retained inputs | rules 1–4 | `predates_binding` |
| none (never launched) | — | never | `no_binding` (unbound) |

No candidate → `no_match`; several → `ambiguous` (rule 4). A revocation never
erases accepted usage: rollouts that started before it stay bound with all
their records; rollouts that start at or after it are not bound. The binding
is per rollout, so records appended later to an earlier bound rollout still
count. `basis` is kept in the sidecar stream `ingest` (migration
`ingest/0001_source_bindings.sql`, table `source_bindings`), recomputed with
`rollout_sources.binding` on every collect.

## A2 (TM1.2): ingest ledger

Sidecar stream `ingest` migration `ingest/0002_source_observations.sql`. Every
table below is written by the Codex tail in the same sidecar transaction as
that rollout's §5 rows and `collect_offsets`, so a killed collect leaves either
a whole rollout pass or none; the next collect replays it to identical rows.
§5 tables, totals and CLI output are unchanged.

**`source_observations`**: one envelope (doc 03 §1 subset) per allowlisted
record (§5 kinds plus `task_started`) after the file's first `session_meta`.

| Field | Codex value |
|---|---|
| `schema_version` | `1` |
| `event_id` (= `idempotency_key`) | `codex:<path_digest>:<byte offset of the line>` |
| `producer_id` / `producer_epoch` / `producer_sequence` | `codex:<session_id>` / `path_digest` (rollout file incarnation) / line byte offset |
| `event_kind` | `codex.<type>.v1` (`event_msg` uses its payload type) |
| `occurred_unix_ms` / `observed_unix_ms` | the line's `timestamp` (or `null`) / collector `now` |
| `identity` | `{"session_id"}` |
| `provenance` | `{"adapter":"codex","adapter_version":<cli_version>,"interface":"rollout_jsonl","source_trust":"collector_observed"}` |
| `measurement` | `{"coverage":"complete","measurement_basis":"reported","normalization_version":1}` |
| `payload`, `payload_digest` | sanitized payload as canonical JSON (§0) and `sha256:` over it |
| `envelope_bytes` | length of the canonical envelope (all fields above plus `idempotency_key`), ≤ 65536 |

`sanitize.rs` builds the payload: the §5 allowlist first (a new object holding
only allowlisted paths; absent or wrongly typed → `null`), then per field
class: identifiers verbatim when ≤128 chars without control characters;
`cwd` with §7 rule 2 only; other strings (model, effort, originator, source,
limit and plan ids, timestamps) through §7 excerpt rules 1-5; integers kept,
other numbers as decimal strings (`used_percent` `"37.5"`).

**`ingest_quarantine(source, sequence, reason, bytes, …)`**: a position that
yields no observation. `line_oversized`: a line over the 16 MiB parse limit
(still skipped whole, nothing retained); `envelope_oversized`: an envelope over
64 KiB; `digest_conflict`: same `event_id`, different `payload_digest` (first
and new digests kept; the first observation stays). Same id and digest: no-op.

**`source_cursors(source, producer_id, byte_offset, observations)`**: where
the envelopes of each source are written through, advanced with
`collect_offsets`.

**`coverage_gaps(source, start_offset, end_offset, reason, recovery)`**, with
`recovery` `pending` until a later pass reads the whole range (`recovered`):
- `sidecar_write_failed`: the rollout's transaction failed because the sidecar
  cannot take writes (SQLite full, I/O error or read-only). The pass rolls
  back, the unread range is recorded, the remaining rollouts wait for the next
  collect and the collect exits normally. If the gap itself cannot be written,
  the collect fails with the original error.
- `predates_ingest`: `[0, offset)` of a rollout read before this migration
  (no envelopes exist for it); recovered only if the file is re-read from 0.

## A3 (TM1.6): adapter conformance and capabilities

**Gate status.** The Codex adapter (`rollout_jsonl`, certified version
`0.154.0`) passes the conformance gate at the scope below. Closing the gate
authorizes downstream development (lane D review capture, lane B) only. It
claims neither paid-account nor production compatibility. The DG4a OTLP
adapters below are fixture-certified only and do not inherit this live gate.

**Suite.** `tests/telemetry_conformance.rs` runs the adapter on the CLI over
one shared corpus (`CASES`: `codex-0.154.0/{head,tail}.jsonl` and
`codex-conformance/edge.jsonl`, placed bound, uncertified-bound, cwd outside
the worktree, in another home, and before the decision). It covers the gate
properties the older crates do not already prove:

| Property | Test | Already proven elsewhere |
|---|---|---|
| Deterministic replay: identical identities, quarantines and rows after a second collect, into a fresh sidecar, and with the rollout appended in pieces cut mid-line | `corpus_replays_identically_in_any_chunking` | single-rollout idempotence, killed-collect replay, partial last line (`telemetry.rs`, `telemetry_collect.rs`) |
| Unknown kinds, `event_msg` types and fields (any level) ignored, not fatal; envelopes keep exactly the allowlist | `unknown_kinds_and_fields_are_ignored` | |
| Malformed or truncated complete lines quarantined with reasons | `malformed_lines_are_quarantined_with_reasons` | oversized lines (`telemetry_collect.rs`), rewritten records (`telemetry.rs`) |
| Binding required on every attributing output (`usage`, `attempts`, M08 coverage, `accounting sessions`) | `unbound_rollouts_are_never_attributed` | binder rules and revocation (`telemetry.rs`) |
| Version gating: an uncertified sibling makes the attempt `cli_version_uncertified`, not a partial sum; excluded from M08; metadata still stored; envelopes name the version | `uncertified_version_is_gated_everywhere` | uncertified counters, re-read after certification (`telemetry.rs`) |
| Privacy: sentinels in content, unknown kinds and fields, excluded rate-limit fields and malformed lines reach neither the sidecar (with WAL) nor any collect, usage, report, collectors or accounting output | `planted_sentinels_never_leak` | head/tail canaries (`telemetry.rs`) |
| Unknown → `unavailable`, never 0 | `unknown_is_unavailable_never_zero` | no-source metrics (`telemetry.rs`) |
| Capabilities match what is emitted | `capabilities_match_emitted_fields` | |

**Malformed lines** (sidecar stream `ingest` migration
`ingest/0003_malformed_quarantine.sql`, which widens the `reason` check).
A complete line that yields no observation because it does not parse is an
`ingest_quarantine` row with its byte offset and length, and nothing of its
content:
- `line_malformed`: not a JSON object with a string `type` (a line cut
  mid-file, an array, no `type`, a blank line).
- `record_malformed`: a read kind (§5) whose typed fields do not parse, for
  example a counter as a string or a fractional `window_minutes`. It stores no
  §5 row and no envelope and takes no usage ordinal. A malformed
  `turn_context` makes `model` and `effort` of later records `null`, not the
  previous turn's.

Unknown top-level kinds are skipped by `type` without reading further and are
not quarantined. A partial last line still waits for its newline. Before A3
such lines were skipped without a record.

**Capabilities.** `herdr-projects telemetry <slug> collectors capabilities
[--json]` reads nothing and creates no file. It prints `{adapters: [{adapter,
interface, certified_versions, uncertified_version, fields}]}` with one entry
per field, `{kind, field, available, basis, certified, caveat, reason}`:
- `available`: whether the adapter collects the field. It is `true` exactly
  for the §5 allowlist (`sanitize::codex_allowlist`) plus `line.timestamp` (the
  line time, kept as the envelope's `occurred_unix_ms`).
- `basis`: `reported` (identifier or number as reported), `reported_excerpt`
  (text after the §7 excerpt rules), `reported_home_redacted` (`cwd`, §7
  rule 2), or `unavailable`.
- `certified`: `live` if observed in the 0.154.0 live run
  ([codex-live-0.154.0.md](codex-live-0.154.0.md)), `fixture` if only the
  fixture corpus exercises it, `none` if not collected.
- `caveat`: what `live` does not certify (`semantics_not_certified`,
  `overlap_with_input_not_certified`, `reconciliation_only`), or `null`.
- `reason`: why an unavailable field is absent. `not_collected` means a
  reviewed §7 revision could add it. `content_forbidden` means §7 never allows
  it. Otherwise `null`.

The table is declared once, in `collectors::codex_fields`. The command fails
if the table and the allowlist disagree. The suite checks the table in two
ways. It compares the printed table with a reviewed literal. It also checks
that, over the whole corpus, the envelopes carry exactly the available fields,
each with a value somewhere, and no unavailable field.

**Certified scope (Codex 0.154.0), for downstream lanes:**
- *Live:* `session_meta.{id, timestamp, cwd, cli_version}`,
  `turn_context.{model, effort}`, and the six `token_usage_record.usage`
  counters. Summing accepted records is certified. Cache-write overlap with
  input is not.
- *Live, reconciliation only:* `thread_token_usage` and
  `token_count.info.total_token_usage`.
- *Live, storage only (semantics not certified):* `rate_limits.{limit_id,
  plan_type, primary.{used_percent, window_minutes, resets_at}}`.
- *Fixture only (installed but untested live):*
  `session_meta.{originator, source}`, every `turn_id`,
  `token_usage_record.{session_id, response_id}`,
  `task_complete.{duration_ms, time_to_first_token_ms}` and line timestamps.
  The live prerequisite is the planned small live run (owner decision 5).
- *Moved to live by the A4 live run* ([codex-live-0.154.0-a4.md](codex-live-0.154.0-a4.md)):
  line timestamps, `session_meta.{originator, source, model_provider,
  subagent_kind}`, every `turn_id`, `token_usage_record.{session_id,
  response_id}` (a guardian reports its parent's `session_id`: caveat
  `guardian_reports_parent_session`) and `task_complete.{duration_ms,
  time_to_first_token_ms}`. Still fixture (no value live):
  `forked_from_id`, `subagent_parent_thread_id`, `subagent_depth`,
  `rate_limits.secondary.*`, `rate_limit_reached_type`.
- *Moved to live by the second live run* ([codex-live-0.154.0-run2.md](codex-live-0.154.0-run2.md)):
  `forked_from_id` (a `codex exec fork`; caveat
  `fork_thread_total_includes_origin`: the fork replays no records, but its
  reported totals include its origin's, so `thread_total` flags a false
  discrepancy), `subagent_parent_thread_id` and `subagent_depth` (a
  `thread_spawn` subagent). Still fixture: `function_call.status`,
  `rate_limits.secondary.*`, `rate_limit_reached_type`. MCP calls are
  `McpToolCall` `item_completed` items; A8 (below) collects their metadata,
  the subagent and collab items, `turn_aborted`, `function_call.namespace`
  and the fork point, all live.
- *Not collected:* `rate_limits.{credits, limit_name}`,
  `info.{last_token_usage, model_context_window}`, `turn_token_usage`,
  `thread_id`, `root_turn_id`, start and completion times, and tool/exec
  metadata (A6 below collects tool/exec metadata). (A4 below adds `model_provider`, per-record times,
  `rate_limits.{secondary, rate_limit_reached_type}` and subagent/fork
  parent ids, fixture-certified.) Content is never collected: instructions,
  messages, reasoning, `last_agent_message` and `response_item`.
- Nothing about reviews, findings or verification comes from the Codex
  adapter. Lane D captures those canonically, not from rollouts.

**Envelopes of uncertified versions** keep the sanitized counters in
`source_observations.payload`, as reported evidence. §5 rows keep none. Their
`provenance.adapter_version` names the version, and since A7 their
`measurement.certified` is `false` (see A7): a ledger reader accepts counters
only from an envelope with `measurement.certified` `true`.

**Contracts §0 finding (open, steward and lane B).** A bound certified
session whose usage records all fail validation (`invariant_violation`) shows
attempt usage `0` with `records: 0` (`sidecar::attempt_usage`) and M08 `0`
with `certified_sessions: 1` (the lane B M08/M09 provider). Its usage is
unknown, so both should be `unavailable`. The expected result is in the
ignored test `rejected_records_are_unavailable_not_zero`: attempt usage
`unavailable: records_not_accepted` (the M13 reason) and the session excluded
from M08 coverage as `records_not_accepted`. A session with only some records
rejected shows the sum of the accepted ones, and only `records` < the
session's record count tells them apart.

**Gate items not covered by this suite:**
- Ordinal resets and resume across files, nested (guardian) usage, and model
  switches inside a session: covered by A4 (below).
- A lost final event that is never written. Before A7 there was no signal
  for it beyond the discrepancy between the thread total and the sum. A7
  records a rollout left idle without its last turn's `task_complete` as a
  `final_event_missing` coverage gap.
- Cross-surface overlap. Codex has one surface (`rollout_jsonl`).

## A4 (TM1.3 remainder): Codex metadata, child sessions, resume

### A4 proposed contracts.md §5/§7 revision

**Status: reviewed and applied to contracts.md §5/§7 by the steward (tool/exec metadata still held).** `contracts.md` is a
steward file, so the revision is written here; the implementation below
follows it and every new field is certified `fixture` until the planned small
live run (owner decision 5). No field below is content: each is an
identifier, an enum-like tag, a number or a time that Codex writes about the
session, never text a person or model wrote.

| Envelope field (kind) | Read from | Class (§7 rule) | Stored in | Why it is not content | Certified |
|---|---|---|---|---|---|
| line `timestamp` (`token_usage_record`) | the line | time → Unix ms | `codex_usage_times.record_unix_ms` | when Codex wrote the record | live |
| `model_provider` (`session_meta`) | `model_provider` | Text (excerpt rules 1–5) | `rollout_metadata.model_provider` | configured provider id (`openai`) | live |
| `forked_from_id` (`session_meta`) | `forked_from_id` | Id (≤128, no control chars) | `rollout_metadata.forked_from_id` | another session's UUID | fixture, semantics not certified |
| `subagent_kind` (`session_meta`) | `source.subagent` (string, or first key of an object) | Tag | `rollout_metadata.subagent_kind` | enum variant (`review`, `compact`, `thread_spawn`, `memory_consolidation`, `other`) | live (`other` for the guardian) |
| `subagent_parent_thread_id` (`session_meta`) | `source.subagent.thread_spawn.parent_thread_id` | Id | `rollout_metadata.subagent_parent_thread_id` | parent thread UUID | fixture |
| `subagent_depth` (`session_meta`) | `source.subagent.thread_spawn.depth` | Number | `rollout_metadata.subagent_depth` | nesting depth | fixture |
| `rate_limits.secondary.{used_percent, window_minutes, resets_at}` (`token_count`) | same | Number (decimal text for `used_percent`) | `codex_rate_limit_windows.secondary_*` | quota counters, like `primary` | fixture (live saw `null`) |
| `rate_limits.rate_limit_reached_type` (`token_count`) | same | Text | `codex_rate_limit_windows.rate_limit_reached_type` | enum-like limit tag | fixture, semantics not certified |

(Superseded by A6 below, which collects tool/exec metadata in the shape the
live run observed.) Proposed but **held** (not implemented; capability rows
`exec_command_end.*` and `mcp_tool_call_end.*` are `not_collected`): tool and
exec metadata for B5 (M16–M18): `call_id`, `turn_id` (Id), `exit_code`,
`duration.{secs,nanos}` (Number), `status` (Text), and for MCP
`invocation.{server, tool}` (Text). Never `command`, `cwd`, `parsed_cmd`,
`stdout`, `stderr`, `aggregated_output`, `formatted_output`, `arguments` or
`result`. Held because the record shapes in 0.154.0 rollouts are not
observed: the live run saw `item_completed` records (one record mixing the
command, its output and the metadata), not typed `exec_command_end`, and a
parser for a guessed shape would certify only its own fixture. The live
probe below records the shape first.

Still not collected: `agent_nickname`, `agent_role`, `agent_path`
(user-chosen names and a path), `forked_from_ordinal_exclusive` and
`subagent_history_start_ordinal` (semantics unknown), `credits` (holds a
balance string), `limit_name`, and all content (§7).

**Proposed contracts.md text** (steward diff):
- §5 *Allowlisted fields*: after `source` add "`model_provider`,
  `forked_from_id`, and from `source.subagent` its variant and, for
  `thread_spawn`, `parent_thread_id` and `depth`"; after
  `primary.{…}` add "`secondary.{used_percent, window_minutes, resets_at}`,
  `rate_limit_reached_type`"; and "the line `timestamp` of each
  `token_usage_record`".
- §5 new bullet: "A4 metadata (sidecar stream `ingest` 0004:
  `rollout_metadata`, `codex_usage_times`, `codex_rate_limit_windows`) is
  read leniently: a value of another type is stored as `NULL` and never
  makes its record malformed. It is outside `payload_digest`; the first
  stored value stays."
- §7 *Default*: "metadata only (IDs, digests, enums, counters, timestamps,
  durations, provider and parent-session identifiers)"; *Never collected*
  unchanged. The held tool/exec fields would add "tool call ids, tool and
  MCP server names, exit statuses and durations; never commands, arguments
  or output" once reviewed and shape-certified.

### Parent session id (Codex 0.154.0)

Evidence, without reading `~/.codex` or running Codex: the serde names in the
installed 0.154.0 binary (`strings`) show `SessionMeta` fields `id`,
`forked_from_id`, `forked_from_ordinal_exclusive`, `timestamp`, `cwd`,
`originator`, `cli_version`, `source`, `agent_nickname`, `agent_role`,
`model_provider`, `base_instructions`, …, `subagent_history_start_ordinal`,
and `SubAgentSource` variants `review`, `compact`, `thread_spawn` (a struct
of five: `parent_thread_id`, `depth`, `agent_path`, `agent_nickname`,
`agent_role`), `memory_consolidation`, `other`. So:
- A **spawned subagent** (`source: {"subagent": {"thread_spawn": {…}}}`)
  names its parent: collected as `subagent_parent_thread_id`.
- A **fork** names its origin in `forked_from_id`: collected.
- A **guardian / auto-review** session (`review` or `other`, model
  `codex-auto-review`) carries **no parent id** in `session_meta`: not
  available. Its variant is collected (`subagent_kind`), so it is
  recognizable, but it can only be attributed through the binding rule
  (same attempt worktree), never linked to a parent session. Whether its
  `token_usage_record.session_id` differs from its own `session_meta.id`
  (envelopes keep that field) is a live-run question.
  **Live (A4 run): the guardian does name its parent**, outside
  `source`: `session_meta.parent_thread_id` and `session_meta.session_id`
  both equal the parent's `session_meta.id`, `thread_source` is
  `guardian_review`, and its `token_usage_record.session_id` is the parent's
  (`thread_id` is its own). A5 (below) collects these keys; see
  [codex-live-0.154.0-a4.md](codex-live-0.154.0-a4.md).
- Whether `parent_thread_id` equals the parent's `session_meta.id` holds in
  the fixtures only (thread id = session id in every 0.154.0 fixture).

### Collection rules

- Tables in sidecar stream `ingest` 0004 (the `codex` stream, steward-owned,
  is unchanged), keyed like the Codex rows and written in the same
  transaction: `rollout_metadata(path_digest)` from the file's first
  `session_meta`; `codex_usage_times(session_id, ordinal, record_unix_ms)`
  for each stored or replayed usage record (not for a quarantined one);
  `codex_rate_limit_windows(session_id, ordinal, …)` beside each
  `codex_rate_limits` row. `CREATE TABLE IF NOT EXISTS`, so a sidecar whose
  stream table was lost re-runs it.
- Lenient: a new field of another type is `NULL`; the record keeps its §5
  rows and is not quarantined (a live-certified `primary` is never lost to
  an uncertified `secondary`).
- The line time is not in `payload_digest`, so existing digests (and
  `DIGEST_1`, `DIGEST_2`) are unchanged; the first stored time stays.
- **Upgrade.** A source with a `rollout_sources` row but no
  `rollout_metadata` row was read before A4. Each collect reads such sources
  again from byte 0 within its budget (like certification re-reads): stored
  keys dedupe, `collected.records` counts nothing twice, and the envelopes
  of the widened kinds are replaced.
- **Envelopes.** `session_meta` and `token_count` envelopes carry the new
  fields (absent ones as `null`) with `measurement.normalization_version` 2;
  other kinds stay at 1. An envelope with the same `event_id` and a lower
  normalization version is superseded in place, not a `digest_conflict`.
- `herdr-projects telemetry <slug> collectors sessions` (read-only, JSON):
  per rollout source `{session_id, path_digest, binding, attempt_id,
  records, model_provider, forked_from_id, subagent {kind,
  parent_thread_id, depth}, record_times {stored, timed, first_unix_ms,
  last_unix_ms}}`. `null` is a value the rollout did not report; a sidecar
  without the A4 tables shows each A4 field `unavailable:
  predates_collection`, and a source waiting for its re-read (for example
  its rollout is gone) `unavailable: pending_reread`, never `null`.
- Existing outputs (`usage`, `attempts`, `report`, `bindings`, accounting)
  are unchanged. Changed on purpose: the `token_count` envelope payload
  and digest, `session_meta`/`token_count` normalization version, the
  capabilities table, and the first collect after the upgrade re-reads
  bytes.

### Conformance (tests/telemetry_conformance.rs)

Corpus cases `child` (`codex-conformance/child.jsonl`: a `thread_spawn`
subagent and fork of the edge session, a model switch, exec and MCP tool
records, the secondary window and a reached type) and `guardian`
(`guardian.jsonl`: `codex-auto-review`; since A5 in the live shape, see
A5 below) join `CASES`, with
`A4LEAK_*` sentinels in agent names and paths, tool arguments, commands,
output, MCP results, credits and guardian transcripts.

| Property | Test |
|---|---|
| Model provider, fork and subagent parent ids, guardian without parent, per-record times, model switch within a session, secondary window and reached type; child and guardian usage counted once each in the bound attempt | `session_metadata_record_times_and_child_usage_are_collected` |
| Resume across files: a file replaying the session's history dedupes and only its new record counts (thread total reconciles); a file restarting the ordinals is quarantined and usage is `unavailable: quarantined` | `resume_across_files_dedupes_history_and_quarantines_an_ordinal_restart` |
| Upgrade of an A3 sidecar: `predates_collection` read-only, then a re-read that equals a fresh collect, no digest conflict, nothing counted twice; a gone rollout stays `pending_reread` | `rollouts_read_before_a4_gain_their_metadata_on_the_next_collect` |
| No `A4LEAK_*` sentinel in the sidecar (with WAL) or any output, including `collectors sessions` | `planted_sentinels_never_leak` |
| Capabilities match emitted fields, each new one valued somewhere in the corpus | `capabilities_match_emitted_fields` |

Not covered: a lost final event (no signal; A7 adds the idle-rollout gap); compaction; whether a child or
forked rollout replays its parent's `token_usage_record`s (a double count
if it does, see the live probe).

### Live probe recipe (the owner's planned small run)

Same harness and isolation as [codex-live-0.154.0.md](codex-live-0.154.0.md)
(disposable project and execution home, private login copy, workers under
`workspace-write`, `HP_LIVE_F1_KEEP=1`). The worker brief should make the
session run one shell command and, where the profile allows, spawn one
subagent and trigger one auto-review. Then, from the disposable root:

1. `herdr-projects --root <d>/root telemetry demo collect`, then
   `collectors sessions`, `usage --json`, `collectors capabilities`.
2. A key census of every rollout, printing structure only (never values):

   ```sh
   python3 - <rollouts> <<'PY' | sort | uniq -c
   import json, sys
   def keys(v, p=""):
       if isinstance(v, dict):
           for n, x in v.items(): keys(x, f"{p}.{n}")
       else:
           print(p, type(v).__name__)
   for f in sys.argv[1:]:
       for line in open(f):
           r = json.loads(line)
           inner = r["payload"].get("type") if isinstance(r.get("payload"), dict) else None
           keys(r, f"{r.get('type')}/{inner}")
   PY
   ```

   Keys are structure, but some records key objects by data (for example
   patch changes by file path): read the census before sharing it.

3. Check and record: record times non-decreasing and within the session;
   `model_provider`; `secondary` non-null and its window; the
   `rate_limit_reached_type` values; the `source` shape of each child and
   whether `parent_thread_id` equals the parent's `session_meta.id`; the
   guardian's `source` variant and its `token_usage_record.session_id`; the
   event types and key paths of tool and exec records (unblocks the held
   B5 fields); and duplicated usage across sessions:
   `SELECT response_id, count(DISTINCT session_id) FROM codex_usage GROUP BY 1
   HAVING count(DISTINCT session_id) > 1` on `telemetry.db`.
4. Run the canary as before, adding the child and guardian rollouts'
   strings. Then flip the certified rows in `collectors::codex_fields`
   from `Fixture` to `Live` (and literal `CAPABILITIES`) only for fields
   observed with a value.

### Follow-ups for other lanes

- **B2 (session graph):** link a child whose
  `rollout_metadata.subagent_parent_thread_id` (or `forked_from_id`) equals
  a known `rollout_sources.session_id` instead of `unlinked_child`, with
  evidence `thread_spawn_parent` / `forked_from`; use `subagent_kind` for
  the role (`review` = guardian) alongside the model. A guardian stays
  unlinked (`no_native_parent_evidence`). Before linking inclusively, rule
  out replayed parent history in the child (live probe step 3).
- **B3 (rate cards):** narrow the usage interval per entry to
  `codex_usage_times.record_unix_ms` (join on `session_id, ordinal`),
  falling back to session start → first observed when it is `NULL` or the
  row is absent; check `rollout_metadata.model_provider` against the card's
  provider. Both fixture-certified.
- **B4 (quota):** read `codex_rate_limit_windows` as a second window per
  snapshot (its own `window_minutes`/`resets_at`), and
  `rate_limit_reached_type` as candidate throttle evidence for M38 once its
  semantics are certified.
- **B5:** tool/exec metadata stays held until the live census and a steward
  review of the §7 wording above.
- **Steward:** apply the §5/§7 text above to `contracts.md` and list ingest
  0004 in its index.

## A5: Codex thread lineage and guardian usage identity

Source: the live A4 run ([codex-live-0.154.0-a4.md](codex-live-0.154.0-a4.md)
§5). The 0.154.0 guardian names its parent outside `source`, and its usage
records report the parent's session id.

### Fields

| Envelope field (`session_meta`) | Read from | Class (§7 rule) | Stored in | Certified (caveat) |
|---|---|---|---|---|
| `parent_thread_id` | top-level `parent_thread_id` | Id | `rollout_threads.parent_thread_id` | live (`observed_for_guardian_only`: only the guardian had the key; a spawned subagent was not observed) |
| `session_id` | `session_id` | Id | `rollout_threads.session_id`, only when it differs from `session_meta.id` (`NULL`: absent or equal) | live (`guardian_reports_parent_session`: primaries report their own `id`, the guardian its parent's) |
| `thread_source` | `thread_source` | Tag (excerpt) | `rollout_threads.thread_source` | live (`observed_user_and_guardian_review_only`: values seen were `user` and `guardian_review`) |

All three are identifiers or an enum-like tag that Codex writes about the
thread, never text a person or model wrote. `multi_agent_version` is not
collected (not needed; `A5LEAK_MULTI_AGENT` sentinel). The subagent tag
stays as A4 collects it: the live guardian's `source` is
`{"subagent": {"other": "guardian"}}`, so `subagent_kind` is `other`, and
its `other` string (`guardian`) is still not collected.

### Collection rules

- Sidecar stream `ingest` 0005 adds `rollout_threads(path_digest PK,
  parent_thread_id, session_id, thread_source)`, written from the file's
  first `session_meta` with `rollout_metadata`, in the same transaction and
  as leniently (a value of another type is `NULL`, never a malformed
  record). `CREATE TABLE IF NOT EXISTS`, so a sidecar whose stream table was
  lost re-runs it.
- **Upgrade.** A source with a `rollout_sources` row but no
  `rollout_threads` row was read before A5; each collect reads it again from
  byte 0 within its budget, exactly like the A4 backfill. Stored keys
  dedupe; `collected.records` counts nothing twice.
- **Envelopes.** `session_meta` envelopes carry the three fields (absent as
  `null`; `session_id` verbatim, even when equal to `id`) with
  `measurement.normalization_version` 3. An A4 envelope (version 2) of the
  same `event_id` is superseded in place, not a `digest_conflict`.
- `collectors sessions` adds `thread {parent_thread_id, session_id,
  source}` per rollout source. `thread.session_id` is `null` when the rollout
  reports its own id. A sidecar without `rollout_threads` shows `thread`
  `unavailable: predates_collection`; a source waiting for its re-read
  `unavailable: pending_reread`.
- Existing outputs (`usage`, `attempts`, `report`, `bindings`, accounting)
  are unchanged. Changed on purpose: the `session_meta` envelope payload,
  digest and normalization version, the capabilities table, and the first
  collect after the upgrade re-reads bytes.

### Guardian usage identity

A guardian's `token_usage_record.session_id` is its parent's id. The adapter
never keys usage by that field: `codex_usage`, `codex_usage_times`,
`codex_turns`, `codex_rate_limits`, the reconciliation and the envelope
identity all use the rollout's own `session_meta.id`, and the ordinal counts
records within that rollout. So a guardian's records take their own
`(session_id, ordinal)` keys, never the parent's (which would otherwise
collide at ordinal 1 and quarantine both). The envelope keeps the reported
value in `payload.session_id` as evidence. No code change was needed; A5
proves it (below). The live run agrees: the attempt counted the guardian's
7462 once beside the parent's 29760.

### Conformance (tests/telemetry_conformance.rs)

`codex-conformance/guardian.jsonl` now has the live shape: `source`
`{"subagent": {"other": "guardian"}}`, `thread_source` `guardian_review`,
`parent_thread_id` and `session_id` = the edge session's id, and its usage
record's `session_id` = the edge session's id (`thread_id` its own,
`root_turn_id` an edge turn). `codex-0.154.0/head.jsonl` reports
`thread_source` `user`, as the live primaries did.

| Property | Test |
|---|---|
| The guardian's usage record (reporting the edge session's `session_id`, its own rollout, its own `response_id`) is stored under the guardian's id at ordinal 1; the edge keeps ordinals 1–3; no quarantine, no discrepancy; attempt usage = 1460 + 30 = 1490 with 4 records, also when the guardian's rollout is read first; envelope identity is the guardian's, its payload keeps the reported id; the accounting graph shows the edge 1460 and the guardian 30 apart | `guardian_usage_reporting_its_parent_session_stays_with_its_rollout` |
| Lineage per rollout in `collectors sessions` (guardian: parent and session = edge, source `guardian_review`; primaries: `user`) | `session_metadata_record_times_and_child_usage_are_collected` |
| Upgrade of an A4 sidecar: `predates_collection` read-only, then a re-read that equals a fresh collect, no digest conflict, nothing counted twice | `rollouts_read_before_a5_gain_their_thread_lineage_on_the_next_collect` |
| Sentinels, capabilities matching emitted fields (each new one valued) | `planted_sentinels_never_leak`, `capabilities_match_emitted_fields` |

### Contracts.md §5 addition (applied by the steward)

- §5 *Allowlisted fields*, after the A4 `source.subagent` text: "and the
  thread lineage `parent_thread_id`, `session_id` and `thread_source` of
  `session_meta` (sidecar stream `ingest` 0005 `rollout_threads`); usage is
  keyed by the rollout's own `session_meta.id`, never by a record's
  `session_id`, which a guardian reports as its parent's."
- Index: list ingest 0005.

### Follow-ups

- **B2 (session graph), to link guardians:**
  1. Read `rollout_threads` (`LEFT JOIN` on `path_digest`, tolerate the
     table missing on a read-only pre-A5 sidecar) beside `rollout_metadata`.
  2. Use `rollout_threads.parent_thread_id` as native parent evidence when
     `subagent_parent_thread_id` is `NULL`, with its own `link_basis` (for
     example `thread_parent_thread_id`), and only when it differs from the
     session's own id. A guardian then becomes `linked_child` of the parent
     session when that session is collected, else `unlinked_child` with
     `parent_not_collected` and the claimed id, instead of
     `no_native_parent_evidence`.
  3. Role: `thread_source = 'guardian_review'` is guardian evidence beside
     the model (`codex-auto-review`); `subagent_kind` is `other` for the
     live guardian, not `review`, so the `Some("review")` arm alone never
     matches it (today the model fallback makes it `guardian`; a
     `Some(_) => "subagent"` arm must not win over it).
  4. Inclusion: the guardian's usage is separate spend (the live parent's
     thread total excluded it), so a linked guardian is `separate`, never
     part of the parent's inclusive total. Its link is `live`-certified
     evidence, unlike the fixture-certified `thread_spawn` link.
  5. Never use `token_usage_record.session_id` or `session_meta.session_id`
     as a node key: both name the parent for a guardian.
  6. Update `tests/fixtures/telemetry/accounting/guardian.jsonl` (lane B) to
     the live shape and `tests/telemetry_accounting.rs` expectations
     (`unlinked_child`/`no_native_parent_evidence` becomes a link).
- **Done (A7):** **Lane A (optional):** collect the `source.subagent.other` string (Tag,
  `guardian`) as the kind's detail, if B2 wants it beyond `thread_source`.
- **Steward:** apply the §5 text above and list ingest 0005 in the index.

## A6: Codex tool and exec metadata (never content)

Source: the key census of the live A4 run
([codex-live-0.154.0-a4.md](codex-live-0.154.0-a4.md) §4) and its
proposed allowlist, which the steward reviewed and approved as a §7
revision (metadata only). Codex 0.154.0 writes no `exec_command_end`,
`exec_command_begin` or `mcp_tool_call_end` event. Tool activity is in
`response_item` tool calls and outputs and in `event_msg/item_completed`.

### Fields

Every field is an identifier, an enum-like tag, a number or a line time
that Codex writes about a call. None is text a person, a model or a command
wrote. The census showed tag values `exec`/`wait` (`name`), `completed`
(`status`), `unified_exec_startup` (`source`) and `CommandExecution`,
`UserMessage`, `AgentMessage` (`item.type`). No allowlisted field carried
free text, so none was dropped.

| Envelope kind (`event_kind`) | Field | Class (§7 rule) | Stored in | Certified (caveat) |
|---|---|---|---|---|
| `custom_tool_call`, `function_call` (`response_item`) | `call_id` | Id | `codex_tool_calls.call_id` | live |
| same | `name` | Tag (excerpt) | `.name` | live (`exec`, `wait`) |
| same | `status` | Tag (excerpt) | `.status` | live for `custom_tool_call`; **fixture** for `function_call` (the live `wait` had none) |
| same | `internal_chat_message_metadata_passthrough.turn_id` | Id | `.turn_id` | live |
| same | line `timestamp` | time → Unix ms | `.called_unix_ms` | live |
| `custom_tool_call_output`, `function_call_output` | `call_id` | Id | `.call_id` (joins its call) | live |
| same | line `timestamp` | time → Unix ms | `.output_unix_ms` | live |
| `item_completed` (`event_msg`) | `thread_id`, `turn_id` | Id | `codex_exec_items.thread_id`, `.turn_id` | live |
| same | `item.type` | Tag | envelope only (rows for `CommandExecution` only) | live |
| same, `CommandExecution` only | `item.id` | Id | `codex_exec_items.item_id` | live (`command_execution_only`) |
| same | `item.status`, `item.source` | Tag | `.status`, `.source` | live (`command_execution_only`) |
| same | `item.exit_code` | Number | `.exit_code` | live (`command_execution_only`) |
| same | `item.duration.{secs, nanos}` | Number | `.startup_duration_{secs,nanos}` | live (`startup_not_run_time`) |
| same | line `timestamp` | time → Unix ms | `.completed_unix_ms` | live |

**Never read into a row or envelope:** `input`, `arguments`, `output` (string
or `output[].text`), the tool items' `id` and `create_time`; the item's
`command`, `cwd`, `parsed_cmd`, `stdout`, `stderr`, `aggregated_output`,
`formatted_output`, `process_id`, `content`, `client_id` and `phase`, and
`started_at_ms`/`completed_at_ms` (not the run time). `capabilities` lists
the content fields `content_forbidden`, the others `not_collected`. An
`AgentMessage` or `UserMessage` item keeps only its type, turn and thread.
Other `response_item` types (`message`, `reasoning`) stay unread,
`content_forbidden`. MCP shapes are unobserved (no MCP tool was called
live), so the row `mcp_tool_call.*` is `not_collected`. The stale guessed
rows `exec_command_end.*` and `mcp_tool_call_end.*` are removed, and so is
`response_item.*`. `response_item.message` and `response_item.reasoning`
replace it.

**What the metadata means (and does not).**
- A call and its output share `call_id`. The gap from call to output
  (`collectors tools` `call_to_output_ms`) measures the call, including any
  approval wait. Live A: 36.9 s against Herdr's 37.8 s `blocked`.
- An exec item and its tool call share **no key**. The exec `item.id`
  (`exec-…`) is neither the `call_id` nor derived from it. They match only on
  `(session_id, turn_id)` and line-time order.
- `item.duration` and the item's own start and completion times are the
  unified exec **startup** (about 2 µs live), not the command's run time.

### Collection rules

- **Typed allowlist parse.** The four `response_item` types and
  `item_completed` are deserialized into structs that name only the
  allowlisted fields. serde skips every other field (the content ones
  included) without retaining it. The line is never parsed as a whole
  `serde_json::Value`. The envelope payload is built from the struct and
  then sanitized like every other kind. The adapter also no longer parses
  an `event_msg` or `response_item` without an allowlist (for example
  `agent_message`) into a `Value` at all.
- **Lenient.** An allowlisted leaf of another JSON type (an object, an
  array, a string exit code, a numeric source) is `null`, and so is an
  allowlisted object of another type (`duration` as a string, `passthrough`
  as a string). The record keeps its other fields and is never quarantined.
  Only a record serde cannot read at all (a duplicated allowlisted key) is
  `record_malformed`. A `response_item` whose `type` tag does not parse is
  ignored unread, as before A6.
- **Storage.** Sidecar stream `ingest` 0006 (`CREATE TABLE IF NOT EXISTS`),
  written in the rollout's transaction, keyed by the rollout's own
  `session_meta.id`. Tool rows are stored for every `cli_version`
  (metadata, like turns).
  - `codex_tool_calls(session_id, call_id, call_kind, name, status, turn_id,
    called_unix_ms, output_kind, output_unix_ms)`. A call fills the call
    columns, and its output fills the output columns of the same row. The
    first call and the first output of an id stay. An output without a call
    keeps a row with `call_kind` `NULL`. A record without a usable
    `call_id` stores no row (its envelope has `call_id: null`).
  - `codex_exec_items(session_id, item_id, thread_id, turn_id, status,
    source, exit_code, startup_duration_secs, startup_duration_nanos,
    completed_unix_ms)`, one per `CommandExecution` item. The first stays.
  - `codex_tool_sources(path_digest)`: sources whose tool metadata was read
    from byte 0 (written with the first `session_meta`).
- **Upgrade.** A source with a `rollout_sources` row but no
  `codex_tool_sources` row was read before A6. Each collect reads it again
  from byte 0 within its budget, like the A4/A5 backfill. Stored keys
  dedupe, and `collected.records` counts nothing twice. The A6 envelopes are
  new `event_id`s (normalization version 1).
- **Envelopes.** `event_kind` `codex.<payload type>.v1`, as for
  `event_msg`: `codex.custom_tool_call.v1`, `codex.function_call.v1`,
  `codex.custom_tool_call_output.v1`, `codex.function_call_output.v1` and
  `codex.item_completed.v1`. An `item_completed` payload always holds every
  allowlisted path, `null` outside `CommandExecution`.
- `herdr-projects telemetry <slug> collectors tools [--json]` (read-only):
  per session `{session_id, attempt_ids (bound), tool_calls: [{call_id,
  call_kind, name, status, turn_id, called_unix_ms, output_kind,
  output_unix_ms, call_to_output_ms}], exec_items: [{item_id, thread_id,
  turn_id, status, source, exit_code, startup_duration {secs, nanos},
  completed_unix_ms}]}`. `[]` means an observed session with no tool activity.
  A sidecar without the A6 tables shows both lists as `unavailable:
  predates_collection`. A session with a rollout still waiting for its
  re-read shows `unavailable: pending_reread`. Without a sidecar, the
  result is `collection_not_run`. The text form prints one line per
  session, call and exec item.
- Existing outputs (`usage`, `attempts`, `report`, `bindings`, `sessions`,
  accounting) are unchanged. Changed on purpose: new envelopes for the A6
  kinds (a rollout with an older-shaped `function_call`, like the child
  fixture, gains one), the capabilities table, and the first collect after
  the upgrade re-reads bytes.

### Conformance (tests/telemetry_conformance.rs)

`codex-conformance/tools.jsonl` joins `CASES` (bound, certified, no usage
record). It has the live shape: an approved `exec` `custom_tool_call`, its
`CommandExecution` item and array output, a `wait` `function_call` with a
string output, `AgentMessage` and `UserMessage` items, a tool call and an
exec item with wrongly typed metadata, and an output without a call.
`A6LEAK_*` sentinels sit in every forbidden field: `input`, `arguments`,
both `output` forms, `command`, `cwd`, `parsed_cmd`, `stdout`, `stderr`,
`aggregated_output`, `formatted_output`, `content` (agent and user),
`phase`, `client_id`, `process_id`, the tool items' `id`, and in the
wrongly typed values (an object key under `name`, a `status` array or
object, a `passthrough`, `exit_code` or `duration` string) and
`last_agent_message`.

| Property | Test |
|---|---|
| `collectors tools --json` (hand-computed rows, `call_to_output_ms` 36910 and 1250), text lines, one exactly-allowlisted envelope per tool line, wrongly typed → `null` without quarantine, output-only row, message items typed only; the child's older-shaped `function_call` collected and its guessed `*_end` events unread | `tool_and_exec_metadata_is_collected_without_content` |
| Upgrade of an A5 sidecar: `predates_collection` read-only (no migration), then a re-read equal to a fresh collect, stream 6, nothing counted twice; a gone rollout's session `pending_reread`; a session without calls `[]` | `rollouts_read_before_a6_gain_their_tool_metadata_on_the_next_collect` |
| No `A6LEAK_*` sentinel in `telemetry.db`, `-wal`, `-shm` or any output (collect, usage, report, collectors incl. `tools` text and JSON, accounting) | `planted_sentinels_never_leak` |
| Capabilities match emitted fields, each new one valued somewhere in the corpus | `capabilities_match_emitted_fields` |
| Replay of the whole corpus (with `tools`) into a fresh sidecar is identical | `corpus_replays_identically_in_any_chunking` |

### Contracts.md §5/§7 revision (applied by the steward)

`contracts.md` is a steward file. The exact diff:

```diff
@@ §5 Codex usage (sidecar), **Source.**
-`<execution_home>/.codex/sessions/**/rollout-*.jsonl` for each Codex profile's
-`execution_home`. The collector reads only `session_meta`, `turn_context`,
-`token_usage_record`, `event_msg` of type `token_count`, `task_started`,
-`task_complete`. All other record types are skipped by type tag without
-retaining any field.
+`<execution_home>/.codex/sessions/**/rollout-*.jsonl` for each Codex profile's
+`execution_home`. The collector reads only `session_meta`, `turn_context`,
+`token_usage_record`, `event_msg` of type `token_count`, `task_started`,
+`task_complete`, `item_completed`, and `response_item` of type
+`custom_tool_call`, `function_call`, `custom_tool_call_output`,
+`function_call_output` (A6: tool metadata only, through a typed allowlist).
+All other record types are skipped by type tag without retaining any field.
@@ §5 **Allowlisted fields.**, after the `task_complete` fields
-`turn_id`, `duration_ms`, `time_to_first_token_ms`. Never:
-`last_agent_message`, instructions, messages, tool calls/outputs, reasoning.
+`turn_id`, `duration_ms`, `time_to_first_token_ms`. Tool calls
+(`custom_tool_call`, `function_call`): `call_id`, `name`, `status`,
+`internal_chat_message_metadata_passthrough.turn_id` and the line
+`timestamp`; their outputs (`*_call_output`): `call_id` and the line
+`timestamp`; `item_completed`: `thread_id`, `turn_id`, `item.type`, and for
+a `CommandExecution` item `item.{id, status, source, exit_code,
+duration.{secs, nanos}}` (the exec startup, not the command's run time) and
+the line `timestamp` (A6, sidecar stream `ingest` 0006 `codex_tool_calls`,
+`codex_exec_items`, `codex_tool_sources`; lenient like A4). Never:
+`last_agent_message`, instructions, messages, reasoning, tool `input`,
+`arguments` and `output`, and an item's `command`, `cwd`, `parsed_cmd`,
+`stdout`, `stderr`, `aggregated_output`, `formatted_output`, `process_id`,
+`content`, `client_id` or `phase`.
@@ §7 Privacy allowlist and excerpts
-Default: metadata only (IDs, digests, enums, counters, timestamps, durations,
-provider and parent-session identifiers).
+Default: metadata only (IDs, digests, enums, counters, timestamps, durations,
+provider and parent-session identifiers, tool call ids, tool names, call and
+exec statuses, exit codes and exec startup durations).
@@ §7 **Never collected**
-Never collected: prompts, briefs, transcripts, agent messages, tool
-arguments/output, diffs, file contents, reasoning text, environment values.
+Never collected: prompts, briefs, transcripts, agent messages, tool
+input/arguments/output, commands and their working directories, parsed
+commands and output (stdout, stderr, aggregated or formatted), diffs, file
+contents, reasoning text, environment values. Tool metadata is read from
+`response_item` and `item_completed` only through a typed allowlist that
+never deserializes these fields.
@@ Index / §0 Stores
+Sidecar stream `ingest` 0006: A6 tool/exec metadata (contracts-collection.md A6).
@@ §8 Landed since phase 1
-(contracts-collection.md A4), review opportunities, sessions and completions,
+(contracts-collection.md A4), Codex tool/exec metadata (contracts-collection.md
+A6), review opportunities, sessions and completions,
```

### Follow-ups for other lanes

- **B5 (M16–M18, tool decisions and executions)** needs, read-only:
  1. `codex_tool_calls` joined to `rollout_sources` on `session_id` for the
     attempt (`binding = 'bound'`). Count calls per `name`, with
     `status`. Read `call_to_output = output_unix_ms − called_unix_ms` as the
     call's wall interval, approval wait included. `output_kind IS NULL`
     means no output yet (open or lost), and `call_kind IS NULL` means an
     output whose call was not seen. Neither is 0.
  2. `codex_exec_items` for executions: `exit_code` (non-zero = failed
     command; `NULL` = unknown, never success), `status`, `source`. Never
     use `startup_duration_*` as run time (caveat `startup_not_run_time`).
     Attribute an item to a call only by `(session_id, turn_id)` and line
     time (the latest call at or before `completed_unix_ms`, with the
     output after it). There is no shared key, so report such a join as
     `inferred`.
  3. Coverage: a session whose rollout lacks `codex_tool_sources` is
     `pending_reread`, and a sidecar without the tables is
     `predates_collection`. Both are `unavailable`, never 0 calls. Tolerate
     the tables missing on a read-only pre-A6 sidecar.
  4. Decisions: 0.154.0 writes no typed approval request or decision
     (codex-live-0.154.0-a4.md §3). An approval can only be inferred: from a
     call whose interval overlaps a B6b `blocked` wait (human), or from a
     guardian session in the same turn (auto-review). Typed decision
     reasons stay unavailable.
  5. Certification: `function_call.status` is fixture only. MCP calls are
     not collected (unobserved shape). The `name` values seen live were
     `exec` and `wait` only.
- **Steward:** apply the §5/§7 diff above, and list ingest 0006 in the
  index.

## A7: collector follow-ups

Three follow-ups from [phase2-lanes.md](phase2-lanes.md) and A5, in sidecar
stream `ingest` 0007 (`migrations/telemetry/ingest/0007_codex_followups.sql`,
re-runnable: `IF NOT EXISTS` tables and a `coverage_gaps` rebuild from a
dropped scratch table, no `ALTER ... ADD COLUMN`).

### Envelope counters of uncertified versions

**Decision: keep the counters, mark the envelope.** Every envelope's
`measurement` gains `certified`: whether its `provenance.adapter_version` was
in `CERTIFIED` when the envelope was written. An envelope of an uncertified
version keeps its sanitized counters in `payload` as reported evidence, with
`measurement.certified` `false`. A ledger reader accepts counters only from an
envelope with `measurement.certified` `true`. §5 rows are unchanged (still no
counters for an uncertified version).

Why not null the counters:
- The payload stays what the source reported. Its `payload_digest` does not
  depend on certification, so certifying a version later never changes a
  digest and can never look like a `digest_conflict`. Nulled counters would
  need a second payload shape per kind and a supersession of every payload.
- The evidence survives. A version is certified by a later live run; with
  nulled counters the rollout would have to still exist to recover them,
  and a gone rollout would lose them. With the mark, only a flag flips.
- The mark is on the envelope, so a reader cannot mistake the counters for
  certified usage without ignoring a field it must read anyway, and no
  reader needs `CERTIFIED` or the §5 tables to decide.

**Supersession.** The pattern of the A4/A5 normalization versions, on the
measurement instead of the payload:
- An envelope whose `event_id` and `payload_digest` are stored but whose
  `measurement` differs (another `certified`, or an envelope written before
  A7 without the key) is superseded in place: `measurement`,
  `envelope_bytes` and `observed_unix_ms` are rewritten. Never a
  `digest_conflict`.
- `rollout_ingest_state.uncertified_envelopes` (per rollout source) is `1`
  once any envelope of the source was written uncertified, and stays `1`
  until the source is read again from byte 0. Each collect re-reads from
  byte 0 every source with the flag whose version is certified now, like the
  §5 re-evaluation of uncertified usage rows (it may coincide with it). So a
  rollout without usage records is re-read for its envelopes alone.
- A source read before A7 (no `rollout_ingest_state` row) is re-read from
  byte 0 by the A7 backfill, so every stored envelope gains the key.

### Lost final events

A rollout whose last turn never gets its `task_complete` (Codex killed, the
event lost) had no signal. A7 tracks each source's **last turn**, after its
first `session_meta`: a `turn_context` or `task_started` opens one unless it
repeats the tracked turn's id; a `task_complete` of the tracked id (any
`task_complete`, when the turn has no id) completes it. Stored in
`rollout_ingest_state(last_turn_offset, last_turn_id, last_turn_completed)`,
advanced in the rollout's transaction, and reset by a re-read from byte 0.

**Idle.** After each pass over a source (also a pass with nothing new to
read), the last turn is a **missing final event** when all hold:
- it is not complete;
- the pass read the file to its end (it did not stop at the byte budget); a
  trailing partial line counts as the end;
- the file's modification time is at least `FINAL_EVENT_IDLE_MS` = 600 000 ms
  (twice the default 300 s ticker telemetry pass) before the collector's
  `now`.

Only the file's size and modification time and the cursor decide; no content
is read for it. Then `coverage_gaps` holds a row `(source, start_offset =
the byte offset of the line that opened the turn, end_offset = the file's
end, reason = final_event_missing, recovery = pending)`, written again only
when the end moves. A later write to the file does not clear it; the gap
widens to the new end once the file is idle again.

**Recovery.** Only the event recovers the gap: a `task_complete` whose turn
id equals the `turn_id` of the envelope at the gap's start (or that completes
the tracked turn) marks it `recovered`. Reading the gap's byte range again
does not (the generic `finish` recovery skips this reason). A gap whose turn
was followed by another turn stays `pending`: its event never came.

**Outputs.** The gap is a `coverage_gaps` row like the A2 reasons. `collectors
sessions` shows per rollout `final_event {state, turn_id}`: `no_turn`,
`complete`, `open` (not complete, not idle yet), or `missing` (a pending
gap at the last turn). A sidecar without ingest 0007 shows `final_event`
`unavailable: predates_collection`, and a source waiting for its re-read
`unavailable: pending_reread`, never a state. Usage, attempts, report and
accounting outputs are unchanged: a missing final event does not change any
sum (a lost `token_usage_record` still shows only as a thread-total
discrepancy).

### `source.subagent.other` (subagent detail)

| Envelope field (`session_meta`) | Read from | Class (§7 rule) | Stored in | Certified (caveat) |
|---|---|---|---|---|
| `subagent_detail` | `source.subagent.other` (a string; any other type `null`) | Tag (excerpt rules 1–5) | `rollout_subagents.subagent_detail` | live (`observed_guardian_only`: the A4 live guardian's `{"subagent": {"other": "guardian"}}`) |

An enum-like tag Codex writes about the session, never text a person or model
wrote. It is `null` for every other `source` shape (`thread_spawn`, `review`,
a plain string).

- `rollout_subagents(path_digest PK, subagent_detail)` is written from the
  file's first `session_meta` with `rollout_metadata`, as leniently.
- `session_meta` envelopes carry `subagent_detail` with
  `measurement.normalization_version` 4. An A5 envelope (version 3) is
  superseded in place, not a `digest_conflict`.
- **Upgrade.** A source with a `rollout_sources` row but no
  `rollout_subagents` or `rollout_ingest_state` row was read before A7. Each
  collect reads it again from byte 0 within its budget, like the A4–A6
  backfill. Stored keys dedupe; `collected.records` counts nothing twice.
- `collectors sessions` adds `subagent.detail`: the tag, `null`, or
  `unavailable` (`predates_collection` without ingest 0007, `pending_reread`
  while the source waits for its re-read). `capabilities` lists
  `session_meta.subagent_detail` (`reported_excerpt`, `live`,
  `observed_guardian_only`).

### Conformance

| Property | Test |
|---|---|
| Uncertified envelopes keep their counters with `measurement.certified` false; certified ones true | `uncertified_version_is_gated_everywhere` |
| Certifying a version later: every envelope's measurement superseded in place, a rollout without usage re-read for its envelopes, no conflict, nothing counted twice, equal to a fresh collect | `envelopes_of_a_version_certified_later_are_superseded_without_conflict` |
| Idle without the last turn's `task_complete`: `open`, then a pending `final_event_missing` gap (also on a first read), widened by a later partial write once idle again, recovered by the event; usage unchanged | `idle_rollout_without_final_event_records_a_coverage_gap` (`telemetry_collect.rs`) |
| Guardian `subagent.detail` `guardian`, others `null`; `final_event` per rollout | `session_metadata_record_times_and_child_usage_are_collected` |
| Upgrade of an A6 sidecar: `predates_collection` read-only, then a re-read equal to a fresh collect, stream 7, no conflict; a gone rollout `pending_reread` | `rollouts_read_before_a7_gain_their_subagent_detail_and_turn_state_on_the_next_collect` |
| Sentinels, capabilities matching emitted fields, replay in any chunking | `planted_sentinels_never_leak`, `capabilities_match_emitted_fields`, `corpus_replays_identically_in_any_chunking` |

### Contracts.md §5 addition (applied by the steward)

- §5 *Allowlisted fields*, after the A4 `source.subagent` text: "and, for
  the `other` variant, its string tag as `subagent_detail` (A7, sidecar
  stream `ingest` 0007 `rollout_subagents`; live: `guardian`)".
- Index / §0 Stores: "Sidecar stream `ingest` 0007: A7 subagent detail,
  per-source ingest state, envelope `measurement.certified` and
  `final_event_missing` coverage gaps (contracts-collection.md A7)."

### Follow-ups

- **Ledger readers** (any lane, when one appears): gate counters on
  `measurement.certified`, never on `provenance.adapter_version` alone.
- **B2** may use `subagent_detail = 'guardian'` as guardian evidence beside
  `thread_source = 'guardian_review'` (both live).
- A live run could certify how long Codex leaves a rollout unwritten inside
  a healthy turn (a long tool call or approval wait); a turn idle past 600 s
  is `missing` until its event arrives, then `recovered`.

## A8: the second live run's shapes (MCP, subagents, aborted turns, forks)

Source: [codex-live-0.154.0-run2.md](codex-live-0.154.0-run2.md) and the
steward's §7 decision in [phase2-lanes.md](phase2-lanes.md) ("From live run
2"): approved, metadata only. Sidecar stream `ingest` 0008
(`migrations/telemetry/ingest/0008_codex_live_run2.sql`, re-runnable: `IF NOT
EXISTS` tables only, no `ALTER ... ADD COLUMN`).

### Fields

Every field is an identifier, an enum-like tag, a boolean, a number or a line
time that Codex writes about a call, a turn or a session. MCP server and tool
names are configuration names chosen by the operator (like tool names, no
digest; §7 excerpt rules applied).

| Envelope kind | Field | Class (§7 rule) | Stored in | Certified (caveat) |
|---|---|---|---|---|
| `item_completed`, `McpToolCall` | `item.id` | Id | `codex_mcp_calls.item_id` | live (`typed_items_only`) |
| same | `item.server`, `item.tool` | Tag (excerpt) | `.server`, `.tool` | live (`mcp_tool_call_only`) |
| same | `item.status` | Tag | `.status` | live (`observed_completed_failed`) |
| same | `item.readOnlyHint`, `item.result.isError` | Bool | `.read_only_hint`, `.is_error` (0/1) | live (`mcp_tool_call_only`) |
| same | `item.duration.{secs, nanos}` | Number | `.duration_{secs,nanos}` | live (`startup_not_run_time`: like an exec item's, not the run time) |
| `item_completed`, `SubAgentActivity` | `item.id`, `item.agent_thread_id` | Id | `codex_agent_items.item_id`, `.agent_thread_id` | live (`subagent_activity_only`) |
| `item_completed`, `CollabAgentToolCall` | `item.id`, `item.status`, `item.sender_thread_id`, `item.receiver_thread_ids[]` | Id, Tag, Id, IdList | `codex_agent_items.{item_id, status, sender_thread_id, receiver_thread_ids}` (JSON array) | live (`collab_agent_tool_call_only`) |
| `item_completed`, `CommandExecution` | `item.status` value `failed` (exit ≠ 0) | Tag | `codex_exec_items.status` | live: certified exec statuses `completed` (exit 0) and `failed` (exit ≠ 0) |
| `turn_aborted` (`event_msg`) | `turn_id`, `reason`, `duration_ms`, line time | Id, Tag, Number, time | `codex_turn_aborts(session_id, turn_id, reason, duration_ms, aborted_unix_ms)` | live (`interrupted`) |
| `function_call` (`response_item`) | `namespace` | Tag | `codex_tool_namespaces(session_id, call_id, namespace)` | live (`collaboration`) |
| `session_meta` | `forked_from_ordinal_exclusive`, `history_base.{thread_id, end_ordinal_exclusive, end_byte_offset}` | Number, Id, Number, Number | `rollout_forks(path_digest, forked_from_ordinal_exclusive, base_thread_id, base_end_ordinal_exclusive, base_end_byte_offset)` | live (`end_byte_offset`: `origin_file_length_at_fork`) |

**Never read into a row or envelope:** an MCP call's `arguments` (keyed by
data) and `result.content` (and any other `result` field); a subagent's
`kind` (not approved: type, ids and status only) and `agent_path`; a collab
call's `tool`, `receiver_agents` and `agents_states`; `turn_aborted.
{started_at, completed_at}`. The typed structs name only the allowlisted
fields, so serde skips the others without retaining them, as in A6.
`capabilities` lists `item.arguments`, `item.result.content`,
`item.agent_path` and `item.receiver_agents` as `content_forbidden`, and
`item.kind`, `item.agents_states` and `turn_aborted.{started_at,
completed_at}` as `not_collected`. The capability row `item.result` becomes
`item.result.content`.

### Collection rules

- **Typed and lenient**, as A6: a wrongly typed leaf is `null`, never a
  malformed record. `receiver_thread_ids` keeps an array of Ids (a non-Id
  element is `null`); any other value is `null`. Rows are stored for every
  `cli_version` (metadata); the first row of a key stays.
- **Envelopes.** `item_completed` payloads hold every allowlisted path,
  `null` outside the item type that reports it (a `CommandExecution` never
  carries `server`, an `McpToolCall` never `exit_code`). `turn_aborted` is a
  new kind (`codex.turn_aborted.v1`, normalization 1). Normalization versions
  rise so older envelopes are superseded in place, never a `digest_conflict`:
  `session_meta` 5, `function_call` 2, `item_completed` 2 (`custom_tool_call`
  keeps its allowlist and version 1).
- **Aborted turns are final events.** A `turn_aborted` whose `turn_id` is
  the tracked last turn's (or any, for a turn without an id) completes it
  like `task_complete`, with `rollout_turn_ends.last_turn_aborted = 1`. It
  recovers a pending `final_event_missing` gap for its turn, and an aborted
  turn left idle past `FINAL_EVENT_IDLE_MS` is never `final_event_missing`.
  `collectors sessions` shows `final_event.state = aborted`.
- **Fork reconciliation.** A fork (`history_base.thread_id` present) replays
  no records, but its `thread_token_usage` and `token_count` totals include
  its origin's thread total at the fork point (run2 §1). Its reconciliation
  subtracts, per counter, the origin's reported `thread_token_usage` of its
  last certified `token_usage_record` envelope whose byte offset is below
  `history_base.end_byte_offset` (zeros if there is none) from both reported
  totals, then compares with Σ accepted as in §5. The fork point is located
  in bytes because every stored record is keyed by its byte offset;
  `end_ordinal_exclusive` (the origin's line count) is stored and shown but
  line ordinals are not collected. States per session and total, in
  `codex_fork_reconciliation`: `reconciled`, `discrepancy` (then the
  `codex_discrepancy` row holds the fork's own share as `reported_total`),
  `origin_not_collected` (no certified rollout of the origin read up to the
  fork point: no `codex_discrepancy` row, never a false alarm) and
  `fork_point_unknown` (no usable `end_byte_offset`). After every rollout of
  a collect is read, forks still `origin_not_collected` (or not yet
  reconciled) are reconciled again, so read order never matters. A fork
  without `history_base` (the fixture-only `child`) reconciles as before.
- **Upgrade.** A source with a `rollout_sources` row but no `rollout_forks`
  or `rollout_turn_ends` row was read before A8. Each collect reads it again
  from byte 0 within its budget, like A4–A7. Stored keys dedupe;
  `collected.records` counts nothing twice.
- `collectors sessions` adds `fork`: `null` for a rollout that names no fork
  point, else `{forked_from_ordinal_exclusive, history_base {thread_id,
  end_ordinal_exclusive, end_byte_offset} | null, reconciliation
  {thread_total, token_count_total} | null}`. `final_event` needs ingest
  0008 too. Both are `unavailable` `predates_collection` without ingest 0008
  and `pending_reread` while the source waits for its re-read.
- `collectors tools` adds per session `mcp_calls` (`{item_id, thread_id,
  turn_id, server, tool, status, read_only_hint, is_error, duration {secs,
  nanos}, completed_unix_ms}`), `agent_items` (`{item_id, type, thread_id,
  turn_id, status, agent_thread_id, sender_thread_id, receiver_thread_ids,
  completed_unix_ms}`), `turn_aborts` (`{turn_id, reason, duration_ms,
  aborted_unix_ms}`) and each tool call's `namespace`, with the same
  `predates_collection` / `pending_reread` rules for ingest 0008. The text
  form adds `mcp`, `agent` and `abort` lines and ` namespace=` on a call
  that reports one.
- Unchanged: usage, attempts, report, bindings and accounting outputs (lane
  B still reads only the A6 tables). Changed on purpose: the envelopes above,
  the capabilities table, fork `codex_discrepancy` rows (a false discrepancy
  disappears), and the first collect after the upgrade re-reads bytes.

### Conformance (tests/telemetry_conformance.rs)

`live2-tools` and `live2-fork` join `CASES` (bound, certified; the fork's
`@ORIGIN@` is `complete` and `history_base.end_byte_offset` its planted
length). `live2-tools`' collab item now names its receiver (the live census
listed `receiver_thread_ids[]` with elements) and carries `LIVE2LEAK_*`
sentinels in `receiver_agents` and `agents_states`.

| Property | Test |
|---|---|
| MCP, subagent, collab, aborted-turn, namespace and fork rows and envelopes (exactly the allowlist per item type); `aborted` final event, also idle past the threshold; fork `origin_not_collected` with no discrepancy, `reconciled` against 1680 once the origin is collected, still after the origin grows past the fork point; lane B (B12) M16 executed 3 (the MCP call once) and M17 `2/3` (the `failed` item a failure); no `LIVE2LEAK_*` sentinel in the sidecar (with WAL/SHM) or any output | `live_run2_shapes_are_collected_without_content` |
| Upgrade of an A7 sidecar: `predates_collection` read-only, then a re-read equal to a fresh collect, stream 8, no conflict; a gone rollout `pending_reread` | `rollouts_read_before_a8_gain_their_live_run2_metadata_on_the_next_collect` |
| Sentinels (MCP arguments and results, agent paths, collab agents and states), capabilities matching emitted fields (each new one valued), replay in any chunking | `planted_sentinels_never_leak`, `capabilities_match_emitted_fields`, `corpus_replays_identically_in_any_chunking` |

### Contracts.md §5/§7 revision (applied by the steward)

`contracts.md` is a steward file. The exact diff:

```diff
@@ §5 Codex usage (sidecar), **Source.**
-`task_complete`, `item_completed`, and `response_item` of type
-`custom_tool_call`, `function_call`, `custom_tool_call_output`,
-`function_call_output` (A6: tool metadata only, through a typed allowlist).
+`task_complete`, `turn_aborted` (A8), `item_completed`, and `response_item`
+of type `custom_tool_call`, `function_call`, `custom_tool_call_output`,
+`function_call_output` (A6: tool metadata only, through a typed allowlist).
@@ §5 **Allowlisted fields.**, after "(A5, sidecar stream `ingest` 0005 `rollout_threads`)."
+A fork's `forked_from_ordinal_exclusive` and `history_base.{thread_id,
+end_ordinal_exclusive, end_byte_offset}` (A8, sidecar stream `ingest` 0008
+`rollout_forks`).
@@ §5 **Allowlisted fields.**, after "`task_complete`: `turn_id`, `duration_ms`, `time_to_first_token_ms`."
+`turn_aborted`: `turn_id`, `reason`, `duration_ms` and the line `timestamp`
+(A8: the aborted turn's final event).
@@ §5 **Allowlisted fields.**, tool calls
-(`custom_tool_call`, `function_call`): `call_id`, `name`, `status`,
-`internal_chat_message_metadata_passthrough.turn_id` and the line
-`timestamp`;
+(`custom_tool_call`, `function_call`): `call_id`, `name`, `status`,
+`internal_chat_message_metadata_passthrough.turn_id`, the line `timestamp`,
+and for a `function_call` its `namespace` (A8);
@@ §5 **Allowlisted fields.**, `item_completed`
-a `CommandExecution` item `item.{id, status, source, exit_code,
-duration.{secs, nanos}}` (the exec startup, not the command's run time) and
-the line `timestamp` (A6, sidecar stream `ingest` 0006 `codex_tool_calls`,
-`codex_exec_items`, `codex_tool_sources`; lenient like A4). Never:
+a `CommandExecution` item `item.{id, status, source, exit_code,
+duration.{secs, nanos}}` (the exec startup, not the command's run time;
+certified statuses `completed` and `failed`) and the line `timestamp` (A6,
+sidecar stream `ingest` 0006 `codex_tool_calls`, `codex_exec_items`,
+`codex_tool_sources`; lenient like A4); for an `McpToolCall` item
+`item.{id, server, tool, status, readOnlyHint, result.isError,
+duration.{secs, nanos}}`; for a `SubAgentActivity` item `item.{id,
+agent_thread_id}`; for a `CollabAgentToolCall` item `item.{id, status,
+sender_thread_id, receiver_thread_ids[]}` (A8, sidecar stream `ingest` 0008
+`codex_mcp_calls`, `codex_agent_items`, `codex_turn_aborts`,
+`codex_tool_namespaces`, `rollout_turn_ends`). Never:
@@ §5 **Allowlisted fields.**, the "Never" list
-`stdout`, `stderr`, `aggregated_output`, `formatted_output`, `process_id`,
-`content`, `client_id` or `phase`.
+`stdout`, `stderr`, `aggregated_output`, `formatted_output`, `process_id`,
+`content`, `client_id` or `phase`, an MCP call's `arguments` or `result`
+content, a subagent's `agent_path`, or a collab call's `receiver_agents` or
+`agents_states`.
@@ §5 **Usage record**, Reconciliation bullet
 - Reconciliation: Σ accepted `usage` per session vs the last
   `thread_token_usage`; any difference → `codex_discrepancy(kind =
   'thread_total')`. Last `token_count.total_token_usage` differing from Σ →
   `codex_discrepancy(kind = 'token_count_total')` (expected after
   compaction; informational, never used for sums).
+  A fork (`history_base.thread_id`) reports both totals including its
+  origin's thread total at the fork point: that total (the origin's last
+  certified `thread_token_usage` before `history_base.end_byte_offset`) is
+  subtracted first. An origin not collected up to the fork point records
+  `codex_fork_reconciliation.state = 'origin_not_collected'` and no
+  discrepancy (A8, sidecar stream `ingest` 0008).
@@ §7 Privacy allowlist and excerpts
-provider and parent-session identifiers, tool call ids, tool names, call and
-exec statuses, exit codes and exec startup durations).
+provider and parent-session identifiers, tool call ids, tool names and
+namespaces, call and exec statuses, exit codes and exec startup durations,
+MCP server and tool names with their read-only and error flags and
+durations, subagent and collab item ids and statuses, turn abort reasons,
+and fork points).
@@ §7 **Never collected**
-Never collected: prompts, briefs, transcripts, agent messages, tool
-input/arguments/output, commands and their working directories, parsed
+Never collected: prompts, briefs, transcripts, agent messages, tool
+input/arguments/output (MCP `arguments` and `result` content included),
+subagent paths and collab agent records, commands and their working directories, parsed
@@ Index / §0 Stores
-  `ingest` 0006 tool/exec metadata, 0007 subagent detail, per-source ingest
-  state, envelope `measurement.certified` and `final_event_missing` gaps,
-  contracts-collection.md A6–A7), mode 0600, created on first collect. No
+  `ingest` 0006 tool/exec metadata, 0007 subagent detail, per-source ingest
+  state, envelope `measurement.certified` and `final_event_missing` gaps,
+  0008 MCP calls, subagent and collab items, aborted turns, function call
+  namespaces, fork points and fork reconciliation,
+  contracts-collection.md A6–A8), mode 0600, created on first collect. No
@@ §8 Landed since phase 1
-(contracts-collection.md A4), Codex tool/exec metadata (contracts-collection.md
-A6), review opportunities, sessions and completions,
+(contracts-collection.md A4), Codex tool/exec metadata (contracts-collection.md
+A6), Codex MCP, subagent, aborted-turn and fork metadata and fork
+reconciliation (contracts-collection.md A8), review opportunities, sessions
+and completions,
```

§0 check: no content field is read into a typed struct, a row or an
envelope (the sentinels prove it end to end); every new value is metadata
(§7 default above); unknown stays `unavailable` (`predates_collection`,
`pending_reread`, `origin_not_collected`), never 0 or `null`.

### Follow-ups for lane B (B12)

All read-only, joined on the rollout's own `session_id` (bound sources only),
tolerating the A8 tables missing on a read-only pre-A8 sidecar
(`predates_collection`) and a session with a source lacking `rollout_forks`
(`pending_reread`); both `unavailable`, never 0.

1. **M17 (`failed`).** `codex_exec_items.status = 'failed'` with a non-zero
   `exit_code` is a certified failed execution (run2: `2/3`, not `2/2`).
   `completed` with exit 0 is success. Any other status, or `failed` with
   exit 0 or `NULL`, stays `status_not_certified`. `custom_tool_call.status`
   only says the call was made.
2. **M16 (aborted).** `codex_turn_aborts(session_id, turn_id, reason,
   duration_ms, aborted_unix_ms)`: a call whose `codex_tool_calls.turn_id` is
   an aborted turn and whose output is the turn's last before
   `aborted_unix_ms` (run2: `reason = 'interrupted'` right after a declined
   approval) is `declined_or_aborted`, not `human_routed` accepted. Other
   calls of an aborted turn keep their decision. `collectors sessions`
   `final_event.state = 'aborted'` marks the rollout's last turn.
3. **MCP calls (M16–M18).** `codex_mcp_calls(session_id, item_id, thread_id,
   turn_id, server, tool, status, read_only_hint, is_error, duration_secs,
   duration_nanos, completed_unix_ms)`: count by `server` and `tool` and set
   `accounting` `mcp_calls` to `live`. An MCP call is also an `exec`
   `custom_tool_call` (code mode): it counts once, as the MCP call; the item
   `id` (`exec-…`) is not the call's `call_id`, so match the carrying
   `exec` call only by `(session_id, turn_id)` and line time (`inferred`).
   `is_error = 1` is a failed MCP call; `status` values seen: `completed`.
   Never use the duration as run time (M18 stays `unavailable`).
4. **Fork inclusion.** A fork replays no records and its links are live, so
   fork inclusion becomes `separate`, not `unavailable:
   fork_replay_not_certified`. Never add a fork's reported totals: its
   `codex_fork_reconciliation` states say whether its own share reconciled.
5. **Collab and subagents.** `codex_tool_namespaces.namespace =
   'collaboration'` marks `spawn_agent` / `wait_agent` calls (not tool
   executions); `codex_agent_items` gives the spawned child's thread id
   (`SubAgentActivity.agent_thread_id`, whose `started` item id equals the
   spawn `call_id`) and a wait's receivers.

## A9: the third live run's follow-ups (terminated turns, usage after termination)

Source: [certificate-live.md](certificate-live.md) §5 and §3.1 (F4, F3).
Ingest 0009 (`migrations/telemetry/ingest/0009_turn_terminations.sql`,
re-runnable: `IF NOT EXISTS`).

### Turns the product ended (F4)

Codex 0.154.0 writes neither `task_complete` nor `turn_aborted` for a turn
whose worker the product stops. The collector reads the canonical
`runtime.worker_terminated` receipt of each attempt (read-only, its first
receipt: `attempt`, `cause`, `observed_unix_ms`). On every collect, before
reading rollouts and again after the binding, a bound rollout whose last turn
is still open gets one `rollout_turn_terminations(path_digest, turn_offset,
attempt_id, cause, terminated_unix_ms)` row when:

- the bound attempt's receipt cause is `cancellation` or `completion` (the
  product ended it; `process_exit`, the agent ending itself, does not count);
- the turn was opened at or before the receipt (the line time of the
  envelope that opened it, or an unknown time).

Such a turn is never `final_event_missing`: no pending gap is written for
it, and one written earlier is recovered. `collectors sessions` shows
`final_event {state: ended_by_termination, turn_id, termination {cause,
observed_unix_ms}}`. A later turn (a resume after termination) is judged on
its own and can be `missing`. Health reads no coverage gap, so it never
alerted on this; the conformance test asserts no open alert.

### Usage after termination (F3)

`collectors sessions` adds `after_termination` per rollout: for one bound to
an attempt with a termination receipt (any cause), `{terminated_unix_ms,
records, first_unix_ms}` counts its usage records whose line time is after
the receipt; `null` otherwise; `unavailable: predates_collection` without
record times (ingest < 4). The records stay in the attempt's usage and M08:
this flags them, it does not move them.

`usage` JSON and text add `after_termination` per attempt, aggregated over
bound rollouts: `{records, first_unix_ms, terminated_unix_ms}` when records
exist, `null` otherwise, or `{status: unavailable, reason: predates_collection}`
when ingest < 4 lacks record times. `report` adds an `after_termination` array
only for affected attempts, each with the flag and `accounting: still counted
in M08`; text prints one equivalent line per attempt. This diagnostic is
unwindowed, including with `report --since`; metric windows and values stay
unchanged. Health rule `usage_after_termination` (health-rules.v2) warns for
any bound post-termination record, with bounded consumption/Codex labels;
missing timing is unknown, never zero. E2E:
`post_termination_usage_is_visible_without_changing_accounting` proves two
post-termination records, resolution with none, and unchanged M08 = 2000.

### Conformance

| Behaviour | Test |
| --- | --- |
| A turn open when the product cancelled the attempt: `ended_by_termination` with the receipt's cause and time, even idle past the threshold; no pending gap, no health alert; a turn opened after the receipt and left idle is `missing`, and its usage record is counted `after_termination` | `a_turn_the_product_ended_is_ended_by_termination_not_a_missing_final_event` (`telemetry_collect.rs`) |

## DG4a: generic OTLP receiver (fixture certification)

`telemetry <slug> otlp serve [--port 4318] [--seconds 3600]
[--max-requests 10000]` opts into a foreground receiver. It listens only on
**127.0.0.1**, including when port `0` selects an ephemeral port. Startup
prints its address and token **path**, never the secret. Nothing starts by
default. The per-project random 256-bit bearer token is stored under the
config directory as `otlp-<sha256(canonical-project-path)>.token`, created
0600, opened without following symlinks, and refused unless owned by the
current user with exactly 0600 permissions. Stop the receiver and remove its
token to rotate it. Tokens are external secrets, excluded from backups.

Only POST `/v1/logs` and `/v1/metrics`, with `Content-Type: application/json`
and `Authorization: Bearer <token>`, are supported. **Protobuf, gRPC,
traces, compression and chunked bodies are unsupported.** Exporters that
only offer protobuf need an external JSON conversion step; HTTP support alone
does not establish compatibility. Authentication failure is 401; a body over
4 MiB is 413; invalid JSON, structure or mapped field types is 400; unsupported
encoding is 415. Headers are capped at 16 KiB, each request has a two-second
read deadline, each request accepts at most 4096 records and each attribute
container at most 128 keys. A global fixed window allows 30 requests/second
(429 thereafter). One connection is handled at a time, closed after its reply;
there is no unbounded thread pool. Lifetime is 1–86400 seconds and accepted
connections are capped at 1–100000. This bounds receiver work independently
of the controller. Storage I/O/SQLite failures return 503 without exposing SQL or payload
details; callers must retain their own retry evidence.

Optional ticker configuration in `<config_dir>/config.toml`:

```toml
[telemetry.otlp.projects.my-project]
enabled = true
port = 4318
```

The disposable telemetry pass reads this configuration and starts a separate
receiver thread per project, with a 300-second / 10000-connection lease.
Later telemetry passes restart an expired lease; cadence changes can create
gaps. A second active thread for the same project is never started. Each
project must choose its own port. Disabling the config takes effect after the
active lease expires; ticker exit ends its threads. The normal telemetry
collection interval (and its disable switch) also governs lease startup.
The receiver spawns no processes; the controller never waits for requests.

### Mapping and sanitizer

The single reviewed mapping declaration in `telemetry::otlp` also generates
`collectors capabilities`. Adapter IDs are `otlp:claude-code`,
`otlp:gemini-cli`, `otlp:codex` and `otlp:unknown`. Every available field is
**fixture**, never live. No version or live compatibility is certified. Names
come from [Claude Code monitoring](https://code.claude.com/docs/en/monitoring-usage)
and [Gemini CLI telemetry](https://geminicli.com/docs/cli/telemetry/);
fixtures under `tests/fixtures/telemetry/otlp` are synthetic OTLP JSON, not
captured paid sessions. Codex OTLP names were not established from the
offline evidence reviewed for this card: its OTLP capability is **none**;
the existing certified rollout adapter remains its collection path.

| Harness / native name | Kind | Allowlisted attributes |
|---|---|---|
| Claude `claude_code.token.usage` | usage | `type` (input/output/cacheRead/cacheCreation), `model` |
| Claude `claude_code.cost.usage` | usage | `model` |
| Claude `claude_code.api_request` | usage | `model`, `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_creation_tokens`, `duration_ms`, `cost_usd` |
| Claude `claude_code.tool_result` | tool | `tool_name`, `success`, `duration_ms` |
| Gemini `gemini_cli.token.usage` | usage | `type` (input/output/cache/thought/tool), `model` |
| Gemini `gemini_cli.api_response` | usage | `model`, `input_token_count`, `output_token_count`, `cached_content_token_count`, `thoughts_token_count`, `tool_token_count`, `total_token_count`, `duration_ms` |
| Gemini `gemini_cli.tool_call` | tool | `function_name`, `success`, `duration_ms` |

Gemini's documented name uses **`api_response`**, not `api.response`;
the latter is unmapped. Known log names may arrive in `eventName`, a string
body containing exactly the reviewed name, or `event.name`. Arbitrary bodies
are never stored. Known sum metrics retain their nonnegative value, reviewed
unit (empty, USD, token/tokens/{token}), start/end nanosecond timestamps and
explicit delta/cumulative temporality. Cumulative samples remain snapshots,
never deltas computed by guessing resets. Both logs and metrics are retained
as independent native evidence: do not sum their overlapping token or cost
reports. Cache/thought/tool inclusion is not certified. No OTLP evidence
feeds budgets or existing Codex accounting totals.

Text identifiers go through the existing excerpt sanitizer after a
128-byte/control-character limit; numbers are nonnegative, booleans typed
(or Claude's documented true/false strings). Unknown services, events,
metric token categories and attributes produce unmapped diagnostics with
**attribute keys only**, no unknown values, arbitrary service names or event
names. Known records also retain the keys of discarded attributes, making
unmapped coverage countable. Body text, prompts, response text, tool
arguments/input/output/parameters, file contents and resource attribute
values outside the binding allowlist are never persisted. Malformed payloads
write nothing, even if earlier records in that payload were valid.

### Binding, identity and storage

Only resource `herdr.attempt_id` exactly equal to a canonical attempt ID
binds a record (`exact`), with resource `service.name` selecting the mapping.
Missing ID is `unbound`; an unknown ID is `unknown_attempt`, with its value
dropped. There is no cwd/home/time inference. This per-project transport
token authenticates delivery, not an attempt-scoped launch capability, and
never grants workflow authority. Provenance is `collector_observed` and
measurement basis is reported (excerpted for text).

Stream `otlp`, migration `otlp/0001_records.sql`, stores sanitized usage,
tool and unmapped rows in `otlp_records`. `otlp records` reads them without
creating or migrating a store. Identity is SHA-256 over canonical sanitized
record JSON, including adapter, exact binding, native timestamp, metric
start timestamp/temporality and discarded attribute **keys**. Receipt time
and discarded values are excluded. An identical resend is a no-op, including
when only forbidden values differ. Without native timestamps, identical
sanitized observations collapse; this limitation is explicit rather than an
invented source identity. A changed sanitized observation is separate
evidence, not a guessed correction. A whole request commits in one sidecar
transaction under the existing maintenance lock. Raw payloads are never
spooled, logged or stored.

Retention class `sidecar.otlp` is source-of-truth **retain**, because exporters
may not replay. It does not follow Codex session deletion. Full sidecar
backups include this stream, its version and row counts; tokens remain
external. Future automatic expiry/operator deletion needs a reviewed OTLP
identity/tombstone contract rather than reusing Codex session tombstones.

`tests/telemetry_otlp.rs` exercises public collector/store and CLI entry points
with isolated projects: exact hand-computed rows per harness, metric native
semantics, privacy canaries, atomic malformed rejection, unbound/unknown
attempts, unknown harnesses, digest replay, capabilities and backup coverage.
TCP ephemeral-port tests exercise authentication, body limits, malformed
rejection, both routes, replay and rate limiting. A sandbox denying loopback
binds must report these failures for the steward to run outside it.

**DG4a follow-up: launch environment wiring.** Configure product-launched
workers' `OTEL_*` endpoint, JSON exporter protocol, authorization header and
resource `herdr.attempt_id`/`service.name`, with explicit token lifecycle and
per-harness exporter compatibility. This is a separate card; DG4a does not
modify launch environments or claim protobuf-only exporters work directly.
