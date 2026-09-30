# Core telemetry certificate (TM2.6: accounting and execution gates)

Card: plan doc 12 TM2.6. Fixtures: plan doc 10 §3 (golden accounting), §4
(replay, time, corrections, budget authority) and §5a (coordinator overhead,
quota and throttling, candidate group). Verifier: an independent accounting
verifier who did not write the accounting or collector code. Date:
2026-09-30. This certificate is for F4.5 adoption of the core contracts. It
does not wait for full TM5.

## 1. What was certified, on what

| Item | Value |
| --- | --- |
| Source | branch `telemetry/tm26-accounting-gate` from `main` `3a8bb3e`, plus the fixes in §4 |
| Canonical store | `SCHEMA = 62` (state.db) |
| Sidecar streams | `codex` 2, `ingest` 8, `accounting` 10, `quality` 2 |
| Adapter | Codex CLI `0.154.0` (the only certified version), `rollout_jsonl` |
| Host | Linux 7.2.3-arch1-3, rustc 1.98.0, debug build |
| Metric registry | contracts.md §6 and contracts-accounting.md §2 definitions (`Mnn.slice-v1`, `M04.cost-v1`, `M11.charges-v1`, `M12/M14.cost-v1`, `M16–M18.tools-v1`, `M31–M33.attention-v1`, `M34/M37.fleet-v1`, `M35.fanout-v1`, `M36.integration-v1`, `M40.quota-windows-v1`) |
| Rate cards, charges, FX | invented synthetic fixtures only (owner decision 4). No real price, charge or invoice was used. |

Command (all green, `--no-fail-fast`):

```
CARGO_TARGET_DIR=… RUST_TEST_THREADS=6 cargo test -j 3 --features state-store \
  --test telemetry_certification --test telemetry --test telemetry_accounting \
  --test telemetry_conformance --test telemetry_collect
```

Results: `telemetry_certification` 13/13 (about 18 s), `telemetry` 20/20,
`telemetry_accounting` 21/21, `telemetry_conformance` 20/20,
`telemetry_collect` 4/4.

### Evidence classes

- **fixture**: synthetic Codex rollouts written line by line by the test,
  real SQLite stores, the real `herdr-projects telemetry` CLI as separate
  processes (including concurrent and SIGKILLed ones). Expected values are
  hand-computed from the plan's numbers in `tests/telemetry_certification.rs`
  and never read back from a production aggregate.
- **real transport**: none beyond the above. No Herdr server, provider API
  or network was used. The attention signal's transport (Herdr `agent list`)
  is exercised only through a stand-in in the lane tests.
- **live**: cited only from the live runs, not re-run here:
  [codex-live-0.154.0.md](codex-live-0.154.0.md) (S5: counters, subsets,
  independent sum M08 = 106038, M13 1/1, M15 7/7),
  [codex-live-0.154.0-a4.md](codex-live-0.154.0-a4.md) (model provider, line
  times, guardian lineage, quota `resets_at` fixed, Herdr `blocked` for a
  Codex approval) and
  [codex-live-0.154.0-run2.md](codex-live-0.154.0-run2.md) (fork and spawn
  shapes, exec failures, MCP calls, quota jitter, no transport errors in
  rollouts, cache writes always 0).

## 2. Doc 10 fixtures run (independent suite)

All tests are in `tests/telemetry_certification.rs`.

| Doc 10 fixture | Required answer | Test | Result |
| --- | --- | --- | --- |
| §3 Cumulative sequence 100/20 → 160/35 | total 160/35, second increment 60/15 | `cumulative_sequence_and_overlapping_delta` | pass |
| §3 Overlapping delta | no extra usage; contradictions quarantined | same test: a contradicting cumulative is a `thread_total` discrepancy (195 vs 210), never counted; a rewritten key is quarantined and its session is excluded, not guessed | pass |
| §3 Replay (reread, replaced file, resumed rollout) | totals unchanged, duplicate kept diagnostically | `replayed_observations_are_accepted_once` | pass |
| §3 Replay (event repeated inside the source) | totals unchanged | `repeated_response_in_one_rollout_is_not_counted_twice` | **failed, fixed** (§4 D1) |
| §4 Two collectors racing | one accepted transition per record | `replayed_observations_are_accepted_once` (6 racing collects, 4 racing syncs), `first_collectors_racing_to_create_the_sidecar` | **failed, fixed** (§4 D2) |
| §3 Proven reset 10/2 | combined 170/37, separate provenance | `proven_reset_and_unproven_decrease` | pass |
| §3 Unproven decrease | gap, no negative usage | same (`regression_without_reset`; the new delta still counts: 175/38) | pass |
| §3 Inclusive child 200 ⊇ 50 | project 200; exclusive 150 only with evidence | `inclusive_and_unknown_child_relations` (live fork shape: origin 50, fork reports 200, own 150, `history_base` reconciles; M08 + M09 = 160 + 40) | pass |
| §3 Unknown child relation | both kept, no fabricated sum | same (fork without `history_base`: `fork_replay_not_certified`, no children sum) | pass |
| §3 Delayed child (child after parent exit) | linked late, parent unchanged | same (`parent_not_collected`, then `linked_child`; parent stays 100) | pass |
| §3 Cache/reasoning subsets | 100/30, subsets not added | `cache_reasoning_subsets_identity_scope_and_hidden_models` (new input 60, total 130) | pass |
| §3 Event-ID reuse across scopes | counted per scope | same (same `response_id`, other session: 200/60) | pass |
| §3 Hidden model / model record after usage | unknown, never the requested model | same (`unallocated`; M15 3/4; late `turn_context` rewrites nothing) | pass |
| §3 Cost 1,000 @ $2/M + 500 @ $4/M | exactly 0.004 | `cost_golden_corrections_and_as_of_views` | pass |
| §3 Missing model rate, cache convention, cache-read rate, FX | partial / unavailable | same (`no_rate_card`, `cache_write_convention_unknown`, `cache_read_rate_missing`, `no_fx_rate`; M14 1/4) | pass |
| §4 Repricing appends; earlier evidence reproducible | new revision; old one byte-identical | same (4 racing reprices append one revision; `--revision 1` and `--as-of` byte-identical) | pass |
| §4 Charge correction 0.0100 → 0.0080 | adjustment −0.002, history kept | same (M11 0.01 → 0.008; `--as-of` shows 0.01) | pass |
| §4 Event at 09:00 observed at 15:00 | as-of 12:00 excludes it; restatement includes it with a new cutoff | same (late record: `--as-of` revision 2 excludes it, revision 3 includes it) | pass |
| §3 `terminal_cohort` $4 + $3 + $1, 1 accepted, open $5 | M04 = $8/accepted task | `terminal_cohort_coordinator_budget_race_and_rebuild` (M04 `8/1`, open excluded 2, M02 1/3, M07 3/1) | pass |
| §5a Coordinator $4, workers $16 | M34 = 20%; unknown coordinator → partial | same (`1/5`, then `partial`) | pass |
| §4 Budget race (two admissions vs shadow budget) | in-flight exposure counted; unknown is never 0 | same (limit $30: accepted 16 + reserved 12 → request $2 allowed, $3 blocked, unsized admission `provider_usage_unavailable`; state.db byte-identical) | pass (shadow only, R5) |
| §4 Analytics rebuild | same view, no double spend, no refund | same, and `killed_collectors_partial_lines_and_outages_replay_identically` (ledger, graph and usage byte-identical; estimates, budget consumption and metrics equal); M34 allocation also in `coordinator_overhead_and_overlap_waste_from_accepted_reasons` | **intermittent, fixed** (§4 D3); pass (R4) |
| §4 Collector killed mid-collect | byte-identical after resume | `killed_collectors_partial_lines_and_outages_replay_identically` (SIGKILL at growing delays; only whole rollouts ever commit; a kill landed mid-pass) | pass |
| §4 Partial line, outage during active work, lost final event | wait; collect on return; explicit gap | same (partial line waits; `final_event` `missing` → `complete`, gap `recovered`; sums unchanged) | pass |
| §5a Quota 40 → 55 | 15 units, monetary unknown; message text is diagnostic only | `quota_window_consumption_is_native_units_only` (percent, increase 15, remaining 45, no amount, the `error` line is no observation, M38 unavailable) | pass |
| §5a Candidate group A/B/C | lifecycle cost includes all three; C is a failure | `candidate_group_cost_includes_every_arm` (arms total 1900/800 over 3 records; winner 1000/500; C `failure_no_candidate`) | pass (tokens only, R8) |
| Adapter certificate check | fields and units behind accounting are certified | `accounting_fields_match_the_adapter_certificate` | pass |

Doc 10 §5a fixtures owned by other lanes are not re-derived here. They are
cited in §5 with their lane tests: fan-out (M35), human attention (M31/M32)
and the tool metrics (M16–M18).

## 3. Source units and fields (adapter certificate)

Checked against `collectors capabilities --json` and the live docs:

- Counted tokens are the per-response `token_usage_record.usage` deltas:
  `input_tokens` (includes cached), `cached_input_tokens`, `output_tokens`
  (includes reasoning), `reasoning_output_tokens`, `total_tokens`. All are
  `reported` and certified `live` (S5: subset rules and the running sum held
  on all live records).
- `thread_token_usage` and `token_count.total_token_usage` are
  `reconciliation_only`. They are never summed.
- `cache_write_input_tokens` is live but carries
  `overlap_with_input_not_certified` (always 0 live). Entries with cache
  writes stay unpriced.
- Model: `turn_context.model` is live. `model_provider` is live and checked
  against the card's provider. Line `timestamp` is live and gives the usage
  interval.
- Quota: `rate_limits.primary.*` is live in unit percent, with
  `semantics_not_certified`. `secondary.*` and `rate_limit_reached_type` are
  fixture only.
- Only the `codex` adapter is collected. Version `0.154.0` is the only
  certified one. Another version is stored without counters and excluded
  (`cli_version_uncertified`), never summed.

## 4. Disagreements found and how they were resolved

**D1: a response recorded twice inside one rollout was counted twice
(product wrong, fixed).** Doc 10 §3 "Replay" requires the repeat to leave
totals unchanged. The ledger keyed Codex deltas only by `(session,
ordinal)`, so the same response (`response_id` and payload) written again at
a later ordinal gave 220/50 instead of 160/35, and would have been priced
again. This is a native identity (a live-certified `response_id` within the
session scope), not deduplication by amount.

Fix, in lane B:
- `src/telemetry/accounting/ledger.rs`: such a record is `duplicate` /
  `response_repeated`, not counted.
- `graph.rs`: it adds nothing to its rollout's inclusive total.
- `cost.rs`: it is never valued.

The same `response_id` with another payload, or in another session, still
counts. Regression: `repeated_response_in_one_rollout_is_not_counted_twice`.
Contract: contracts-accounting.md §1. The Codex row stays accepted in the
steward-owned sums; see R3.

**D2: racing collectors failed with "database is locked" (product wrong,
fixed).** Doc 10 §4 "two collectors racing". A deferred sidecar transaction
that reads, and then writes after another writer committed, gets
`SQLITE_BUSY` without waiting. Duplicates never resulted: the losing pass
rolled back. But a ticker pass racing a CLI `collect`, `sync` or `reprice`
failed outright.

Fix: immediate transactions in the collector's per-rollout pass and fork
reconciliation (`src/telemetry/codex.rs`), in `ledger::sync` and in reprice
(`cost.rs`). Racers now wait (5 s busy timeout).

Regression: 6 racing collects and 4 racing syncs in
`replayed_observations_are_accepted_once`, 4 racing reprices appending one
revision in `cost_golden_corrections_and_as_of_views`, and
`first_collectors_racing_to_create_the_sidecar`.

**D3: M34's coordinator allocation changed between reads and across a
rebuild (product wrong, fixed).** Doc 10 §4 "Analytics rebuild" requires the
same view. `terminal_cohort_coordinator_budget_race_and_rebuild` failed
intermittently at `assert_eq!(rebuilt.5, original.5)`. The same $4 moved
between `allocation_unknown` and `unallocated`.

Cause: rule `coordinator-allocation-v1` cut every span without a terminal
mark at the read's horizon (now). The pre-log open attempt `c-open` has the
unknown span `[log start, now)`. The coordinator record is at decision
+ 1 s, and a fast run read the original view before that instant.
Reproduced by moving the record to decision + 1.8 s: 3 of 4 runs failed.
The record has a line time, so its interval is `record_time` and identical
across the rebuild. A second, latent cause was the fallback interval
(`session_start..first_observed`): a rebuild moves its end.

Fix: rule `coordinator-allocation-v2` (`src/telemetry/accounting/fleet.rs`)
treats a span without a terminal mark as open-ended. It reads only record
times; with none the entry is `allocation_unknown` (`usage_time_unknown`).
Contract: contracts-accounting.md §10. The §4 fallback's rebuild behavior is
documented there.

Regression: the certification test asserts the v2 allocation
(`activity_unknown` 1, $4) and passed 10 of 10 bounded reruns.
`coordinator_overhead_and_overlap_waste_from_accepted_reasons` adds a
coordinator record without a line time (`usage_time_unknown`) and rebuilds
the sidecar: allocation and M34 identical.

**No other disagreement.** Every other plan value in §2 matched on the first
run. Where the plan's fixture assumes a producer that does not exist, the
item is a restriction (§6), not a pass.

## 5. Metric families

| Metric | Status | Evidence |
| --- | --- | --- |
| M08/M09 input/output consumption | **certified-live** (fields, and the S5 independent sum 106038) + **certified-fixture** (all §3 goldens) | this suite (§2); `normalized_totals_match_doc05_golden`, `child_sessions_link_to_parent_without_double_count`; live S5 |
| M04 cost per accepted task | **certified-fixture**; restricted R1 (estimate from fixture-only cards), R5 | `terminal_cohort_coordinator_budget_race_and_rebuild`; `shadow_budget_bridge_matches_doc05_goldens` |
| M11 reported spend subtotal | **certified-fixture** (append, correction, as-of); **restricted** R2: no real provider charge import | `cost_golden_corrections_and_as_of_views`; `provider_charges_reconcile_allocate_and_convert` |
| M12 repriced estimated spend | **certified-fixture**; **restricted** R1 | `cost_golden_corrections_and_as_of_views`; `repricing_uses_rate_effective_at_usage_time`; `ticker_reprices_only_when_inputs_change` |
| M13 usage coverage | **certified-live** (S5 1/1) + fixture | `usage_metrics_follow_certified_sources`, `attempts_show_bound_usage_or_its_reason` (tests/telemetry.rs) |
| M14 cost coverage | **certified-fixture**; restricted R1 | `cost_golden_corrections_and_as_of_views` (1/4) |
| M16 tool call volume | **certified-live** fields (calls, exec items, MCP, aborts, namespaces) + fixture derivation; `accepted` is inferred | `tool_volume_success_and_latency_are_honest`, `accepted_stage_is_inferred_from_waits_and_guardians`, `live_run2_failures_aborts_and_mcp_calls_count_once`; run2 §3–§4 |
| M17 tool execution success | **certified-live** (`completed`/`failed` statuses, MCP `isError`) + fixture | same; run2 session A 2/3 |
| M18 tool latency p95 | **restricted**: `execution_duration_not_exposed` (the exec item duration is startup, not run time) | `tool_volume_success_and_latency_are_honest`; run2 §4 |
| M31/M32 interventions, waiting share | **certified-live** signal for Codex (a4 §3), fixture for other kinds; human-routed waits only | `attention_intervals_union_and_censor` (doc 10 §5a: union 5 min, sum 6, censored at the horizon); not re-derived here |
| M33 permission prompts per attempt | **restricted**: `attention_reason_not_exposed` | same |
| M34 coordinator overhead | **certified-fixture**; **restricted** R6 | `terminal_cohort_coordinator_budget_race_and_rebuild`; `coordinator_overhead_and_overlap_waste_from_accepted_reasons` |
| M35 fan-out efficiency | **certified-fixture** (doc 10 §5a: 3/4, marginal 1/4); no live fan-out | `fan_out_buckets_and_integration_conflicts`; not re-derived here |
| M36 integration conflict rate | **certified-fixture** | `fan_out_buckets_and_integration_conflicts` |
| M37 overlap waste share | **certified-fixture** (owner supersession reasons, canonical 0060) | `coordinator_overhead_and_overlap_waste_from_accepted_reasons` |
| M38 throttled time share | **restricted**: `throttling_not_certified` (run2 §6: transport errors never reach rollouts; a message line is never a field) | `quota_window_consumption_is_native_units_only`; `window_reset_starts_new_window_not_negative` |
| M39 provider error rate | **restricted**: `provider_errors_not_certified` | same |
| M40 quota headroom at dispatch | **certified-live** for primary-window fields and the fixed `resets_at` with jitter tolerance (a4 §2, run2 §6) + fixture; secondary windows fixture only; restricted R7 | `quota_window_consumption_is_native_units_only`; `window_reset_starts_new_window_not_negative`, `shared_window_across_homes_is_flagged_not_summed`, `resets_jitter_and_null_plan_stay_one_window`, `secondary_window_is_tracked` |

## 6. Deployment restrictions

These are restrictions, not passes. Each names what is missing.

- **R1: estimates are not spend.** Rate cards are invented fixtures (owner
  decision 4). M04, M12 and M14 and every `accounting cost` figure are
  `published_rate_estimate` values from whatever cards are imported. They
  are never a billing ceiling or an invoice-accurate claim. Prerequisite:
  owner-approved real rate cards.
- **R2: no provider billing source.** Charges, invoices and FX tables are
  synthetic imports (`synthetic: true` is required). There is no invoice
  reconciliation claim. Prerequisite: an authorized provider export.
- **R3: resolved by the steward in this PR.** `telemetry usage`, `attempts`
  (and so `arms_total`) and the central M15 now skip a repeated response with the
  clause below; `repeated_response_in_one_rollout_is_not_counted_twice` asserts
  attempt usage 160/35 with 2 records. Original finding (D1): the Codex
  row stays `accepted`. So `telemetry usage`, `attempts` (and the candidate
  group's `arms_total`, which reads them) and the central M15 count still
  include such a record twice. The lane M08/M09, the ledger, estimates,
  budget shadow and M04 do not. The steward diff: in
  `src/telemetry/sidecar.rs` `attempt_usage` and in `src/telemetry/metrics.rs`
  (the `codex_usage … accepted=1` sum), add
  `AND NOT EXISTS(SELECT 1 FROM codex_usage p WHERE p.session_id=codex_usage.session_id AND p.response_id=codex_usage.response_id AND p.payload_digest=codex_usage.payload_digest AND p.accepted=1 AND p.ordinal<codex_usage.ordinal)`.
  Live Codex has never written such a repeat (S5: the running sum held on
  every record). Until the steward applies the diff, the collector's
  `thread_total` discrepancy flags the case.
- **R4: the sidecar is not fully derivable.** Deleting `telemetry.db` and
  collecting again rebuilds identical ledger, graph, usage and quota views,
  but only from rollouts still on disk. It loses:
  - the valuation revision history (as-of views of earlier revisions);
  - the imported rate cards, charges and FX tables, which must be
    re-imported from their files;
  - the attention samples, which are sampled live and cannot be re-derived.

  Revision numbers restart. Deploy with `telemetry.db` backed up and import
  files retained. Canonical state is never touched: `state.db` stays
  byte-identical.
- **R5: the budget bridge is shadow only.** `budget-shadow` reads `state.db`
  read-only and enforces nothing (owner decision 2). Admission does not
  consume telemetry usage. No canonical usage acceptance exists, so "zero
  duplicate canonical acceptance" is certified in this sense: telemetry
  makes no canonical write, and ledger acceptance is unique per record.
  Enforcement (TM2.4 proper) needs the canonical bridge command and an
  approved unknown-usage policy.
- **R6: the coordinator is observed only as Codex from a scanned home.**
  The default `coordinator_agent` is `claude`, which has no collector, and
  gives `coordinator_usage_not_observed`. Allocation rule v2 is an even
  split at the record time. A record without a line time is not allocated
  (`usage_time_unknown`).
- **R7: a quota account is an execution home.** Two homes holding one login
  are two accounts (flagged as shared-window candidates, never summed).
  Window consumption is account-wide and is never attributed to a task.
  `used_percent` is coarse (1 % live).
- **R8: candidate-group cost is in tokens only.** No priced group figure
  exists. `arms_total` reads the steward usage path (R3). M41/M42 belong to
  the TM3.5 quality certificate.
- **R9: Codex only.** No collector exists for Claude, Devin, OTLP or other
  kinds (owner decision 3). Their usage is `adapter_absent` everywhere,
  never 0.
- **R10: shapes not seen live stay safe failures.**
  - A resumed session whose new file restarts ordinals is quarantined
    (unknown); resume and compaction were not observed live.
  - A fork without `history_base` keeps `fork_replay_not_certified`.
  - M08/M09 count each session's own records. For the live 0.154.0 fork and
    spawn shapes this is correct (run2 §1–§2: no replay). A future shape
    that replays an origin's records into a child session would be counted
    in both sessions.
  - The same `response_id` with different counters in one session is
    counted as two invocations and is not quarantined.
- **R11: coverage gaps are explicit only in the collector view.** A missing
  final event, a sidecar write failure or an oversized line is a
  `coverage_gaps` row (`collectors sessions`). M08/M09 coverage lists only
  the excluded sources, not those gaps.
- **R12: no scale or real-transport evidence.** Everything here is a debug
  build on one host with small fixtures. Scale gates belong to TM5.1 and
  live field certification of later fields to TM5.2.

## 7. Verdict

The accounting families M04, M08/M09, M11–M14, M34–M37 and M40, and the
execution families M16/M17 and M31/M32, meet doc 10's fixture answers
exactly, with reproducible as-of views and one acceptance per logical
record. Three product defects found here were fixed (D1, D2, D3). M18, M33, M38
and M39 stay unavailable by design. The core contracts are fit for F4.5
adoption under restrictions R1–R12. None of these restrictions is a
fictitious pass.
