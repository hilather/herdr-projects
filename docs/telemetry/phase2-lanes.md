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

## Follow-up cards (recorded by the steward from lane reports)
- **A4+** (lane A, needs a contracts §5/§7 revision): collect per-record
  `token_usage_record` timestamps (narrows B3 usage intervals), the session's
  model provider, the parent session id (B2 child linking),
  `rate_limits.secondary`, and possibly `rate_limit_reached_type`/`credits`
  as metadata. Certify their meaning in the planned small live run.
- **Cache-write convention** (lane A/B): certify whether Codex cache writes
  overlap input. Until then, B3 leaves entries with cache writes unpriced.
- **Done (B10):** **M12/M14** (lane B): report the repriced estimate and cost coverage
  through the lane metrics hook.
- **Done (B10, accounting 0009):** **Delta revisions** (lane B): store valuation revisions as deltas before
  the ticker runs `reprice` automatically (the tick now reprices when cards or the ledger change).
- **Extended M40** (steward): replace the central M40 with the per-window
  form from `accounting quota`. Update the exact assertions in
  `tests/telemetry.rs` and `tests/cli.rs`, and make the pane render a
  `windows` array.
- **Quota semantics** (live run): confirm `resets_at` behavior (fixed or
  rolling, jitter), the 15-minute stale threshold, and one execution
  home = one account.
- **M38/M39** (lane A then B): add a `provider_availability` table once typed
  error/throttle events are certified.
- **Per-task window consumption** (lane B, with the TM2.4 shadow bridge): needs
  a certified invocation scope; `observed_increase` is account-wide only.
- **Done (B11, accounting 0010):** **Provider charges, invoice allocation, dated currency conversion, time-based
  `as_of`** (lane B, TM2.3 remainder), plus the TM2.4 budget bridge in shadow
  mode and M04/M11 (contracts-accounting.md §13–§14). Still open: enforcement
  (TM2.4 proper, owner-gated) and real provider charge imports (fixture-only).
- **Done (integration holds):** **Integration hold for candidate groups**
  (steward + integration; approved by the owner): `integration_jobs.rs`
  `ELIGIBLE` and `begin_integration` skip a member of an unselected group,
  and every selector queues a verified winner. `groups show` reports
  `integration_hold.enforced=true` and still lists candidates that
  integrated without a selection. An unselected arm also never releases a
  `verified_result` dependent (contracts-quality.md §3). **Done (H2):** a
  selected winner that is not the task's latest attempt now satisfies a
  `verified_result` edge (the selection is the current result of its
  revision; nothing later displaces it; seeded stays excluded), and `task
  complete` refuses a seeded or held submission. Still open: raw SQL is
  not guarded for arms, selections, satisfactions or completion requests
  (needs triggers, a schema change).
- **Done (C5):** **C5 M42 uncertainty** (lane C): a group-level percentile bootstrap with a
  recorded seed. **Min-sample registry** (TM4.1): replaces the constant 10.
  **Shared `arm_outcome` helper** (steward re-export). **Judge
  configuration id**. **Runner-up order for operator and judge selections**.
- **Done (A7):** **Envelope counters of uncertified versions** (lane A): decide whether
  `source_observations` should null them out before any reader appears.
- **Done (A7):** **Lost final events** (lane A): record coverage for a rollout that is idle
  without `task_complete`.
- **Done (steward):** **Herdr client in the lib** (steward): `src/herdr.rs` is binary-only.
  B6b re-runs `agent list` through the lib runner.
- **Done (#123 + Herdr client card):** **Outcome `attention`** (steward): show B6b's per-attempt summary in the
  §4 outcome record, and share the M31 T/A cohort SQL from `metrics.rs`.
- **Attention certification** (live run): Codex approval prompt shows as
  `blocked`; typed reasons would unlock M33. A finer attention sampler than
  the 300 s telemetry pass; remote routes; declared intervals.

### From the live A4 certification run (codex-live-0.154.0-a4.md)
- **Done (A5; B2 guardian linking still open):** **A5** (lane A): collect the guardian's `session_meta.parent_thread_id`,
  `session_meta.session_id` and `thread_source` (a guardian's usage records
  carry the parent's `session_id`). Then B2 can link guardians to their
  parent.
- **Done (B8):** **Attention basis live** (lane B): the Codex approval prompt was observed
  as Herdr `blocked`. Flip B6b's signal basis from `fixture` to `live`
  (auto-reviewed approvals never show `blocked`; say so).
- **Done (B8):** **Quota account identity** (lane B): two execution homes holding one login
  report identical windows. B4 keys accounts by home digest, so one account
  appears twice. Proposal: treat snapshots with the same limit, kind and
  `resets_at` from different homes as one account-window, or report
  `account_basis: execution_home` explicitly. `resets_at` was fixed within a
  window (no jitter observed).
- **Done (A6 + B5):** **B5 tool metadata** (steward §7 review first): 0.154.0 has no
  `exec_command_end`/`mcp_tool_call_end`. Tool metadata lives in
  `response_item` `custom_tool_call`/`function_call` (+ `*_output` by
  `call_id`) and `event_msg/item_completed` `CommandExecution` (id, status,
  exit_code). The exec item's `duration` is not the command's run time. The
  proposed allowlist is in the live doc; `response_item` is content-forbidden
  under §7 today. MCP shapes are still unobserved.
- **Done (B9):** **B9 guardian linking** (lane B): read `rollout_threads` (LEFT JOIN,
  tolerate a pre-A5 sidecar), use `parent_thread_id` as parent evidence with a
  new link basis, treat `thread_source = guardian_review` as the guardian role
  (live `subagent_kind` is `other`), keep guardian inclusion `separate`, never
  key nodes by either `session_id`, and update the accounting guardian fixture
  to the live shape.
- **Done (accounting 0008):** **Superseded sidecar tables** (owner decision): `session_graph`,
  `quota_observations` (B7) and `session_nodes` (B9) are no longer read or
  written. Lanes have been told not to DROP TABLE; a single cleanup migration
  waits for the owner's decision.
- **B5 follow-ups** (lane B): **done (B10):** infer the M16 accepted stage from a call
  overlapping a B6b `blocked` wait (human-routed) or a same-turn guardian
  (auto-review), labelled inferred; per-host M18 breakdown. Open: M18 stays
  unavailable until Codex records an execution's real start and end. A live
  run should certify exec statuses other than `completed`, and
  `function_call.status`.
- **B6a follow-ups** (lane B/steward): **done (S3):** M34 prices the
  `coordinator-scope-v1` sessions (Codex at the project directory, bound to
  no attempt) against the project's lifecycle cost, per active
  worker-thread-hour, allocated by `coordinator-allocation-v1` (even split
  across running tasks); M37 has its producer, the owner's accepted
  supersession reason (canonical 0060, `accounting supersede`, workers
  refused); M36 adds a `worker_observed` scope from attempt worktree reflogs
  (counts only) beside the integrator's (contracts-accounting.md §10). Still
  open: feed the F4.6/F5.4 scale-trial steps in as named concurrency levels;
  a declared coordinator execution home and a Claude coordinator adapter;
  supersession-reason corrections; a turn-reference allocation rule v2.
  **Done (B10):** per-configuration fan-out and a configurable window
  (`--window-minutes`, default 60).
- **D6 follow-ups** (lane D/steward): **done (integration holds):** a
  verified seeded result no longer satisfies a dependent task's
  `verified_result` requirement (`satisfaction.rs` `verified_counts`,
  contracts-review.md §8). **Done (D7, 0059, contracts-review.md §9):**
  seed-linked findings are outside M21/M22/M23 credit (`seeded_evaluation`);
  review session starts and completions are shared-ledger rows, so `--as-of`
  replays review status; the review CLI (seeds included) refuses a worker
  execution context (execution-home `HOME` or task-worktree cwd); D4 pass and
  exclusion retractions and `planned_units` enforcement. Still open: seeding
  replay-suite tasks (TM4.6) and a reviewer brief builder.
- **D8 follow-ups** (lane D/steward): **done (D8, 0061,
  contracts-review.md §10):** review acceptance is active through an
  owner-signed `code_review` grant only (reviewer-signed decisions,
  owner-signed revocation, covering-grant trigger); M24 is computed from
  review sessions' valuations; M21–M23 carry acceptance drill-downs.
  **Done (D9, 0062, contracts-review.md §11):** a review assignment launches
  as an ordinary canonical attempt of a review task through the existing
  launch path, with a blind brief (`review_brief.v1`, bound to the task's
  worker snapshot) and its session recorded by the reservation; the
  reviewing worker submits a proposal receipt (`review session|submit`);
  decisions are shared-ledger rows (`review show --as-of`); `review accept
  draft` writes the request bytes to sign offline. **Done (D10,
  contracts-review.md §12):** the trusted reviewer-signer process (`review
  signer init|run|status`): the operator holds the reviewer key under the
  owner config directory (hidden from isolated workers), decides completed
  reviews in an owner-signed grant's scope by a versioned mechanical policy,
  signs and submits through the D8 accept path, with an append-only audit;
  the coordinator stays trusted by owner decision, bounded by the grant.
  Still open: delegated triage (needs a `finding_log` rebuild); a
  planner/scheduler that opens, assigns and
  queues review tasks itself; review worktrees at the candidate commit;
  non-Codex reviewer usage for M24.

### From the test CPU review (docs/reviews/2026-09-29-test-cpu.md)
- **Lock-retry scheduling under load** (done, `fix/lock-retry-progress`):
  under load the next ticker pass began as soon as a launch was admitted.
  That pass's controller services held the root shared, so the launch's
  exclusive try-lock lost, both at start and between its stages, cycle after
  cycle. Now the ticker defers those services, and memory-review delivery,
  while a root-exclusive effect is admitted. Staged effects wait up to 2 s
  (bounded by deadline and cancellation) for shared holders. The
  retirement-step failure was the test stopping the ticker before the
  admitted observation committed; the test now waits for the commit. Details
  and pass counts are in the CPU review §3.
- **Per-connection schema load** (steward): each fresh connection spends about
  6 ms loading the schema (about 900 objects, 434 triggers). Measured at 129
  projects, a ticker pass opened 1,273 connections: per project 4 read-only
  (observation head, `wake_enabled`, dispatch hint, routine hint) and 5
  writable services (barrier stops, waits, replans, verification and
  integration jobs), plus 7 per background observation job (16 per pass).
  *Done (read-only half):* the ticker thread now reuses one read-only
  connection per project within and across passes
  (`store::identity_inventory::reuse`): 757 opens per pass, 0 read-only on
  the ticker thread, main-thread CPU 8.5 s to 5.5 s per pass (debug build,
  same load). Trigger consolidation was measured and not taken: removing all
  triggers saves only about half of the load, and merging same-table,
  same-event triggers would remove 78 of 434. *Still open:* the five
  writable service opens per project (about 0.8 s CPU per pass each at 129
  projects) could share one `open_active_scoped` connection per project and
  pass; that touches the service locking order, so it belongs to the
  scheduler work.
- **Wall-bound tests under ~3x oversubscription:** `local_reports` global cache
  bound, two `artifacts::live` byte-budget tests,
  `canonical_post_probe_sql_cannot_restart_its_budget`,
  `canonical_worker_and_routine_admission_take_separate_project_turns`,
  `coordinator_jobs::notification…seen_or_handled…` (effect.lock busy) and
  `routine_jobs::…completed_ticket_must_be_drained…` (line 188).

### From live run 2 (codex-live-0.154.0-run2.md)
Steward §7 decision on the proposed allowlist: approved, metadata only.
- `McpToolCall`: `id`, `server` and `tool` (Tag, excerpt rules applied; they
  are configuration names like tool names, no digest needed), `status`,
  `readOnlyHint`, `result.isError`, `duration` (with a startup caveat). Never
  `arguments` or `result.content`.
- `failed` is a certified exec status.
- `turn_aborted`: `reason` (Tag) and `duration_ms`.
- `SubAgentActivity` and `CollabAgentToolCall`: item type, ids and status
  only.
- The fork's `history_base.{thread_id, end_ordinal_exclusive,
  end_byte_offset}` and `forked_from_ordinal_exclusive`.
- `function_call.namespace` (Tag).

Cards:
- **A8** (lane A): implement the allowlist above. Treat `turn_aborted` as a
  final event (A7 now raises a false `final_event_missing` for an aborted
  turn). Reconcile a fork's thread and token_count totals against its
  origin's total at the fork point (today every fork records false
  discrepancies).
- **Done (B12, no migration; contracts-accounting.md §15):** **B12** (lane B):
  - M17 counts `failed` with a non-zero exit as a failure.
  - M16 reports calls that ended in `turn_aborted` as declined or aborted.
  - Fork inclusion becomes `separate`, since a live fork replays no records.
  - Quota window matching tolerates small `resets_at` jitter (+5 s seen).
  - `plan_type: null` is not a different account.
- Still unavailable: M18 (the exec item duration is run time minus a
  startup window, not run time); M38/M39 (transport errors never reach
  rollouts); the cache-write convention (always 0 live).

## Dogfooding: measuring workers with herdr-projects itself (DG cards)

On 2026-09-30 the steward kept a hand log of Codex dev workers (gpt-6.1-sol,
low reasoning) across 11 tasks: time per pass, send-backs, what review caught,
and tokens. Five tasks were clean on the first pass. Six needed a send-back,
for a load-only flaky test, a security hole, a resource amplification,
ticker-wide effects, or a workaround in place of the real fix. None reached
`main`. The owner's direction: this is what the product should measure once
it is done. Most of the log already maps to registry metrics:

| Hand-logged | Metric |
| --- | --- |
| Send-backs per card | M07 attempt amplification |
| Accepted first time | M02 acceptance rate; M45 first-candidate CI proxy |
| What review caught | M21, M22, M28 |
| Whether fixes held | M25, M27, M48 |
| Wall time per card | M06 lead time p95 |
| Tokens | M08, M09 |
| Steward effort | M31, M32 |
| Worker A vs worker B | `compare --by configuration`; head to head via M41/M42 candidate groups |

Cards (gaps that stop those questions being answered):
- **DG1 M30 first-candidate verification rate — done.** Native
  `M30.submission-v1` in analytics registry v2 measures accepted first
  candidates / adjudicated first candidates (dictionary doc 07), with pending
  cases shown separately. First submission time fixes the window; all required
  policies need independent verified receipts. Query, report, export, revisions
  and configuration comparison share the producer; CLI fixtures cover retries,
  partial verification, configuration splits and empty windows. See
  [contracts-analytics.md](contracts-analytics.md#dg1-first-candidate-independent-verification-m30).
- **DG2 M10 cache-read share** has no producer. Codex rollouts already report
  `cached_input_tokens` (collected in `codex_usage`), so this is a
  lane-accounting metric definition over existing data. The steward computed
  91–99 % by hand.
- **DG3 M03 accepted throughput** lacks operating hours
  (`operating_hours_not_recorded`). Record the ticker's active intervals per
  project (it already publishes pass metrics) as the denominator.
- **DG4 Harness coverage (owner decision, 2026-09-30).** "Codex only"
  governs our build and test work, not product scope. The telemetry must
  support all the common agent harnesses. Approach: a generic OTLP receiver
  first, then native session-file adapters for exact, certified data. Every
  adapter follows the Codex adapter's rules:
  - sanitizer allowlist (metadata plus short redacted excerpts; never prompts,
    transcripts or code);
  - capability table with `live`/`fixture`/`none` basis;
  - binding to canonical attempts;
  - conformance fixtures;
  - live certification per harness version before its fields count as
    certified. Live runs spend that tool's usage, so ask the owner first.

  Launch support is separate: the product knows only `codex` and `claude`
  worker kinds (`profile_config::observed_version`). Each new harness needs a
  worker kind (version probe, launch arguments, isolation profile) before it
  can run as a product worker. Its telemetry can be collected before that,
  from sessions run outside the product.
  - **DG4a OTLP receiver — done (fixture-certified).** Opt-in loopback
    OTLP/HTTP JSON logs/metrics, per-project bearer token, bounded requests,
    sanitized sidecar stream `otlp` (migration 0001), exact resource
    `herdr.attempt_id` binding and digest replay. Claude Code and Gemini CLI
    mappings are fixture-only; Codex OTLP is `none` pending native-name
    certification, retaining its rollout adapter. Protobuf/gRPC unsupported
    under the no-new-crates constraint. See contracts-collection.md DG4a.
    **DG4a follow-up: launch-env wiring** supplies `OTEL_*`, authorization
    and resource attempt/harness identity for product-launched workers;
    exporter JSON compatibility and token lifecycle remain to be reviewed.
  - **Done (DG4b; ingest 0010, accounting 0013): Claude Code native adapter.** Reads Claude Code session
    transcripts (`~/.claude/projects/<project>/<session>.jsonl`) for per-turn
    usage (input/output/cache tokens), model, tool calls and results
    (metadata only), subagents and compaction. Claude Code is already a
    product worker kind, so this is first among the native adapters.
    Metadata-only synthetic conformance covers 2.1.3 (`fixture`); live
    certification remains a separate owner-gated step (contracts-collection.md DG4b).
  - **Done (DG4c; OTLP 0002): Gemini CLI local files.** Fixture-certified
    0.62.0 SDK outfile conversion through DG4a, bounded incremental cursors,
    digest replay, exact canonical home/time binding, and native chat JSONL
    metadata updates (model, counters, tool names/status; never content).
    Native updates and OTel remain separate; native accounting promotion,
    live certification and Gemini worker launch support are separate cards.
    Synthetic privacy, replay, binding, upgrade, backup and retention E2E
    coverage: contracts-collection.md DG4c.
  - **Done (DG4d; ingest 0011, accounting 0014): OpenCode native adapter.**
    Metadata-only native SQLite v1/v2 messages and tools from recorded OpenCode
    attempt execution homes; 1.18.34 fixture-only, exact binding, deduplicated
    ledger usage, numeric reported costs and session retention/backup.
    OTel spans exist but DG4a log/metric mapping is none. Multi-provider, so
    the model/provider fields matter for M15.
  - **DG4e Grok.** The owner uses it (first wave). Identified 2026-10-01: xAI's
    official Grok Build CLI (`@xai-official/grok` 1.0.46 alpha, a native binary in
    an npm package). Next: establish what it records locally or over OTel.
    Then the OTLP mapping or a native adapter.
  - **DG4f second wave:** Cursor (`cursor-agent`), GitHub Copilot CLI, Amp,
    Aider. OTLP mapping where the harness supports it; otherwise a native
    adapter if its local records are stable enough to certify.
- **DG5 cost M04/M12 (external).** Needs real rate cards and provider charges
  imported by the owner (contracts-accounting.md). Still blocked.
- **DG6 flakes under concurrent load (owner-approved design, 2026-09-30).**
  A change can pass alone and fail when other work runs alongside it. In the
  2026-09-30 log, cx2 was a racy test and P3 a real lock-hold regression, so
  a flake is a signal, never noise to hide.
  - **Done (DG6a) verdict flip rate (telemetry).** When the same tree and
    verification policy get differing verdicts across runs, record a flip.
    Add a metric (flip rate per policy and project) and a health rule.
    Passive; it sees only flakes that happen to be re-run.
  - **Done (DG6b, canonical 0068) load context per verification run (telemetry).** Record the host
    load average, concurrent verifications and CPU/IO pressure with each run,
    so failures can be correlated with contention (e.g. "fails 12 % above
    load 8, 0 % below").
  - **Done (DG6c, quality 0003) per-test results (telemetry).** Capture the runner's structured
    test outcomes (test names and pass/fail: metadata, within the privacy
    default) so a flake is attributed to a specific test, not a whole
    verdict.
  - **Done (DG6d, policy v2 / metadata v2) stress verification (policy, opt-in).** A verification step that
    runs the declared suites under controlled concurrent load: the steward's
    six-run concurrent-load loop as a product feature. It applies to change
    classes that touch write paths, transactions, locks, migrations or the
    ticker. It caught P3's lock regression before merge.
  - **Done (DG6e, quality 0004) honest re-run on failure (policy).** A failed check may be re-run.
    A re-run that passes is recorded as a flake with its evidence and raises
    the flip metric. It is never recorded as a clean pass.
  - **Not planned:** a known-flaky quarantine that stops tests blocking. It
    would have hidden P3's real regression. It needs an explicit owner
    decision.
- **DG7 dogfood the dev loop (after TM5.4).** Run dev cards as herdr-projects
  tasks instead of the steward's scratchpad runner:
  - Each worker model and reasoning level is its own configuration, so
    `compare --by configuration` works.
  - The steward's review findings go through the review lane (M21/M22/M28).
  - Send-backs are new attempts on the same task (M07).
  - It depends on TM5.4 and on DG1.
