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
claims neither paid-account nor production compatibility. No other adapter
exists (owner decision 3), so no other gate is open.

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
`provenance.adapter_version` names the version, so a ledger reader must accept
counters only when that version is in `certified_versions`. No reader of
`source_observations` exists yet.

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
- A lost final event that is never written. There is no signal for it beyond
  the discrepancy between the thread total and the sum.
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

Not covered: a lost final event (no signal); compaction; whether a child or
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
- **Lane A (optional):** collect the `source.subagent.other` string (Tag,
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

### Contracts.md §5/§7 revision (applied by the steward on merge)

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
