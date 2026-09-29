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
- *Not collected:* `model_provider`, `rate_limits.{secondary, credits,
  limit_name, rate_limit_reached_type}`, `info.{last_token_usage,
  model_context_window}`, `turn_token_usage`, `thread_id`, `root_turn_id`,
  start and completion times, tool/exec metadata, and parent/guardian session
  linkage (A4). Content is never collected: instructions, messages,
  reasoning, `last_agent_message` and `response_item`.
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
  switches inside a session. A resumed session that restarts ordinals is
  quarantined (`rewritten_record_is_quarantined`), and B2 splits model
  segments. The rest is A4.
- A lost final event that is never written. There is no signal for it beyond
  the discrepancy between the thread total and the sum.
- Cross-surface overlap. Codex has one surface (`rollout_jsonl`).
