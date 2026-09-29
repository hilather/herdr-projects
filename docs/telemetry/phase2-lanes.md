# Telemetry phase 2 (TM1 remainder, TM2, early TM3): lane plan

Base: main `ea1f215`, canonical `SCHEMA = 51`, sidecar `user_version 2`.

## Constraints found
- Migrations cannot gap: `src/store/test_schema.rs` `historical` applies
  `migrations/*.sql` sorted with `take(version)`; the sidecar runner indexes an
  `include_str!` array by `user_version`. Lanes must not pick numbers.
- `tasks.active_attempt` is singular, so TM3.8 concurrent sibling attempts
  collide with the scheduler and automatic integration.

## Owner decisions (defaults accepted)
1. TM3.8 records sequential arms; integration held until a selection exists;
   if that needs scheduler/integration edits, the lane stops and escalates.
2. TM2.4 budget bridge may be built in shadow mode only (no enforcement).
3. No collectors for Claude (TM1.4), Devin (TM1.5), OTLP (TM1.7), or
   grok/muse/opencode (not launchable profile kinds).
4. Fixture-only rate cards; no real prices shipped.
5. One small live Codex run later certifies A4/B4 fields.

## S0 — steward card (merge first, ~300 lines)
Single-steward files (only S0/steward edits or merges them): `src/cli.rs`,
`src/store/mod.rs` (SCHEMA, include_str!), `src/store/test_schema.rs`,
`src/telemetry/mod.rs`, `src/telemetry/sidecar.rs`, `src/telemetry/metrics.rs`,
`src/telemetry/panel.rs`, `src/ticker.rs`, `src/lib.rs`, `src/actions.rs`,
`herdr-plugin.toml`, `Cargo.toml`/`Cargo.lock`, `docs/telemetry/contracts.md`
(index only), `tests/support/`.

S0 adds:
- Sidecar streams: table `telemetry_streams(stream PK, version)`; existing
  `user_version` becomes stream `codex` (1–2 unchanged). Each lane owns one
  stream dir `migrations/telemetry/<stream>/`: `ingest` (A), `accounting` (B),
  `quality` (C), `review` (D). A reader refuses only a stream newer than it knows.
- CLI hooks: `TelemetryCommand::{Collectors, Accounting, Quality, Review}` each
  wrapping a clap `Subcommand` enum defined in the lane's own module.
- Metrics provider hook per lane module (`fn metrics(...) -> BTreeMap<String,
  Value>`) and a `tick(project)` hook, both registered centrally.
- Move `Fixture` from `tests/telemetry.rs` to `tests/support/telemetry.rs`.
- First E2E `sidecar_streams_upgrade_v2_store`: a v2 sidecar with Codex rows gives
  byte-identical `usage` output after upgrade and streams reads `codex=2`.

Canonical migration reservations in merge order: 0052 A1, 0053 C3, 0054–0058
lane D. Out-of-order merges take the next free number; the steward renumbers.

Rules for every card: E2E tests through the CLI or public store entry points with
hand-computed literals; spawns via `execution_guard::GatedSpawn`; NEVER
`git stash`; own worktree and own `CARGO_TARGET_DIR`; stop on any contracts §0
violation (unknown shown as 0, cross-database atomicity, content persisted,
changes to `LaunchInputs` or ID derivation).

## Lane A — collector foundation
Owns: `src/telemetry/{codex.rs, ingest/, sanitize.rs, binding.rs}`,
`src/store/collector_binding.rs` + one-call hook in `src/store/launch.rs`,
`migrations/0052_collector_bindings.sql`, `migrations/telemetry/ingest/`,
`tests/telemetry.rs`, `tests/telemetry_collect.rs`,
`tests/telemetry_conformance.rs`, `tests/fixtures/telemetry/codex-*`,
`docs/telemetry/contracts-collection.md`.
- A1 TM1.1 (0052): `collector_bindings` written in `apply_launch_started`;
  `telemetry <slug> collectors revoke <attempt>`; Codex binder requires an active
  binding, pre-0052 attempts fall back as `predates_binding`. E2E
  `rollout_binds_only_through_canonical_binding`.
- A2 TM1.2 (stream ingest 0001): `source_observations`, `ingest_quarantine`,
  `coverage_gaps`, `source_cursors`; `sanitize.rs`. E2E
  `envelopes_replay_identically_after_interrupted_collect`.
- A3 TM1.6: conformance suite + `collectors capabilities --json`
  (`{available, basis, certified: fixture|live}` per field). Unlocks lane D.
- A4 TM1.3 remainder: model segments, guardian child sessions, resume across
  files, tool/exec metadata (needs a reviewed §7 revision first). Fixture-only
  until a live run.

## Lane B — accounting
Owns: `src/telemetry/accounting/*`, `migrations/telemetry/accounting/`,
`tests/telemetry_accounting.rs`, `tests/fixtures/telemetry/accounting/`,
`docs/telemetry/contracts-accounting.md`. Read-only SQL over Codex/ingest tables;
never edits `codex.rs`.
- B1 TM2.1: `usage_entries`, `usage_dispositions`; M08/M09 move here. E2E
  `normalized_totals_match_doc05_golden`.
- B2 TM2.2: session graph, `model_segments`. E2E
  `model_switch_splits_segments_not_task`.
- B3 TM2.3: versioned rate cards, exact decimals, basis per entry. E2E
  `repricing_uses_rate_effective_at_usage_time`.
- B4 TM2.7: `quota_windows`, M38/M39/extended M40. E2E
  `window_reset_starts_new_window_not_negative`.
- B5 TM2.5 (after A4): tool decisions/executions, controller intervals, M16–M18.
- B6a TM2.8: concurrency buckets (M35), M36; M34/M37 unavailable.
- B6b TM1.8 remainder (S4): attention intervals via the gated Herdr client,
  M31–M33. E2E `attention_intervals_union_and_censor`.
Out of scope: TM2.4 (shadow later), TM2.6.

## Lane C — quality signals
Owns: `src/telemetry/quality/*`, `migrations/telemetry/quality/`,
`migrations/0053_candidate_groups.sql`, `src/store/candidate_groups.rs`,
`src/store/dispatch_log.rs`, `src/store/reservations.rs`,
`src/launch_preparation.rs`, `tests/telemetry_quality.rs`,
`docs/telemetry/contracts-quality.md`.
- C1 TM3.7: pinned-CI proxy (first submission's first verification) M45 and a
  test-weakening flag (`git diff --numstat` under `tests/`, counts only). E2E
  `first_candidate_ci_proxy_and_test_weakening_flag`.
- C2 TM3.7: revert detection, survival via bounded blame, recent censored;
  M46/flaky unavailable. E2E `revert_within_horizon_counts_recent_censored`.
- C3 TM3.8 (0053): `candidate_groups`, members, selections. E2E
  `group_arms_fixed_before_outcomes_and_losers_keep_cost`. Stop if it needs
  `tasks.active_attempt`/scheduler/integration changes.
- C4 TM3.8: selection + M41/M42.

## Lane D — review capture (after A3)
Owns: `src/store/review_capture.rs`, `src/telemetry/review/*`,
`migrations/telemetry/review/`, canonical 0054–0058, `tests/telemetry_review.rs`.
D1 TM3.1 (0054) → D2 TM3.2 (0055) → D3 TM3.3 (0056) / D4 TM3.4 (0057) /
D6 TM3.6 (0058, seeded-integration guard needs steward sign-off).

## Merge order
S0 → {A1, B1, C1} → {A2, B2, C2} → A3 (+start D1) → {C3, B3, B4} → A4 → B5 →
{B6a, B6b} → C4 → D2 → {D3, D4, D6}. Lanes rebase after every merge; the steward
merges one PR at a time.
