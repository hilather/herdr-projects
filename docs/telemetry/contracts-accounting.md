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
