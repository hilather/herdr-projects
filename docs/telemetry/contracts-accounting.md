# Accounting contracts (lane B)

Owned by lane B ([phase2-lanes.md](phase2-lanes.md)); common rules are
[contracts.md](contracts.md) §0. Plan: doc 05 §2–§3 and §7.

## 1. Usage ledger (B1, TM2.1)

Sidecar stream `accounting` version 1
(`migrations/telemetry/accounting/0001_usage_ledger.sql`). The ledger is
derived: `telemetry <slug> accounting sync` (and the ticker pass after its
Codex collect) rebuilds it whole, in one sidecar transaction, from the Codex
tables (§5) read by SQL only. It never writes `state.db` or Codex tables.
`accounting entries` prints it read-only (`ledger_not_synced` before the first
sync; `collection_not_run` without a sidecar).

`usage_entries`, one per effective invocation or cumulative observation:

| Basis | Scope | Precedence | Identity (`entry_id`) | Position |
| --- | --- | --- | --- | --- |
| `delta` (a `codex_usage` row) | `request` | 1 (counted) | `codex:<session>:<ordinal>` | ordinal |
| `cumulative` (`rollout_sources.thread_usage`) | `thread` | 2 (reconciliation only) | `codex:<session>:thread:<path_digest>` | the source's `records` |

Each keeps `native` (the six Codex counters as stored, `null` when absent) and
normalized fields under `normalization_version = codex-v1`: all six native
counters present, non-negative, ≤ 2^53, `cached ⊆ input`, `reasoning ⊆
output`, `total = input + output`; then `input_tokens` (inclusive),
`cache_read_tokens`, `new_input_tokens = input − cached`,
`cache_write_tokens` (as reported; overlap with input not certified),
`output_tokens` (inclusive), `reasoning_tokens` (subset, never added),
`total_tokens`. Otherwise every normalized field is `NULL`
(`normalized: unavailable not_normalized`), never 0.

`usage_dispositions`, one per entry and rollout that observed it
(`path_digest`):

- Delta: the first-storing rollout is `accepted` (Codex row accepted),
  `unresolved` (Codex `reason`, or `normalization_refused`), or `conflict`
  (`payload_digest_mismatch`, a `codex_quarantine` row for the key; then every
  rollout is `conflict`). Every other rollout of the session whose `records`
  reach the ordinal is `duplicate`: one invocation, several provenance rows.
- Cumulative, per session in `(position, path_digest)` order against the
  high-water total: first or higher `accepted`; equal `duplicate`; lower
  `unresolved` / `regression_without_reset` (Codex reports no reset evidence);
  counters failing `codex-v1` `unresolved` / `invariant_violation`.

`usage_ledger(singleton, normalization_version, synced_unix_ms)` records the
last sync.

Golden (`tests/telemetry_accounting.rs`): input 1000 incl. cached 200, output
300 incl. reasoning 80 → total 1300, new input 800; the same record in two
rollouts is one entry with `accepted` + `duplicate`; a thread total falling
1300 → 1000 is `unresolved`.

## 2. Metrics

The lane provides M08 and M09 (contracts §6, `definition` still
`Mnn.slice-v1`, same shape and numbers as the central slice): Σ normalized
`input_tokens` / `output_tokens` (`reasoning_output_tokens` as a subset) of
counted entries (delta, an `accepted` disposition) of certified sessions, with
the §6 `coverage`. They are derived from the Codex tables at report time, so
they need no prior sync.

## 3. Session graph and model segments (B2, TM2.2)

Stream `accounting` version 2 (`0002_session_graph.sql`), rebuilt by the same
sync and transaction as the ledger (sync adds `sessions` and `model_segments`
counts); migrating to 2 clears `usage_ledger`, so a ledger synced before reads
`ledger_not_synced` until the next sync. `accounting sessions` prints it
read-only.

`session_graph`, one node per rollout source. Links come only from native
evidence:

- Resume: rollouts of one `session_meta.id` form one session. The rollout with
  the most `records` (then lowest `path_digest`) is the root; every other is
  `included` under it (`evidence = same_session_prefix`) when the root observed
  each delta entry it did (same key and payload, §1). Its `inclusive_total` is
  covered by the root and never added: a root of 200 over an epoch of 50
  stays 200. Any `conflict` in the session (quarantine), or an epoch the root
  does not cover, makes every such node `unresolved`: the session total and
  its parent are `unavailable: inclusion_unknown`.
- Role: `guardian` when a record carries model `codex-auto-review`,
  `subagent` when `source = subagent`, else `primary`. The Codex tables keep no
  parent id (`source` stores only its first key), so a guardian or subagent
  root is `unlinked_child` with parent `unavailable:
  no_native_parent_evidence`: reported apart and never added to any other
  session. Linking needs a native parent id captured by lane A (A4).
- `inclusive_total` = Σ normalized `total_tokens` of the delta entries the
  rollout observed (accepted or duplicate); `NULL` when it observed one that
  is not counted and normalized (session total `unavailable: incomplete`).

`model_segments` over a session's counted delta entries by position: the
model is the latest preceding `turn_context` (`codex_usage.model`); a model
switch opens the next `model` segment (1, 2, ...). Records of a turn whose
records or completion (`codex_turns`) carry more than one model go to the
`mixed` bucket; records without model evidence to `unallocated` (segment 0).
Buckets do not break a segment. Each row keeps entries, first/last position
and Σ input, output, reasoning (subset) and total, so segments + mixed +
unallocated = the session total. The requested model is not recorded; the
model is as reported by `turn_context`.

The rollup sums root totals of `primary` sessions (`sessions`) and of
unlinked children (`unlinked_children`) separately; with any incomplete or
unresolved session both are `unavailable: incomplete_sessions`, never a
partial sum.

Test `model_switch_splits_segments_not_task`: gpt-5.5 50 then gpt-5.5-mini
150 → two segments, total 200; the resumed epoch of 50 stays inside the root
of 200; an unlinked guardian of 50 (unallocated 10, `codex-auto-review` 30,
mixed 10) is reported apart; a rewritten record makes the session unresolved.
