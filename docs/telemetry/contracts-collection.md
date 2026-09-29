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
