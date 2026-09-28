# Telemetry baseline (W0)

Status: inventory of `main` at `ebfe10e` (store schema 45) for the telemetry
thin slice. Source plan: `herdr-telemetry-plan-v1.1` (written against
`3b3b351`). Nothing here is implemented telemetry; it records what exists, what
the slice can reuse and where the plan's assumptions no longer hold. Contracts
are in [contracts.md](contracts.md); cards in [slice-cards.md](slice-cards.md).

## 1. What exists at main

### Factory status and admission logging

| Field / surface | Producer | Notes |
| --- | --- | --- |
| `factory status <slug>` JSON | `src/factory_status.rs` `report`; CLI `src/cli.rs` `FactoryCommand::Status` | Read-only; `schema`, `factory_admission`, `admission_paused`, `pause_reason`, `blockers`, `counters` |
| Queue ages (control, verification, integration) | `src/store/observability.rs` `SqliteStore::factory_counters` | `Option<i64>`: `None` when the table is absent or empty |
| `observation_age_ms` | same, `MAX(observed_unix_ms) FROM runtime_observations` | Newest observation only (one row per binding) |
| `ambiguous_effects`, `promotion_conflicts`, `coordinator_checkpoint_chars` | same | **Return 0 on schemas that lack the table**: unknown reported as zero (see gap G9) |
| `retained_slots`, `retained_at_cap`, `integration_reconciliation_stale` | same | Alert booleans with fixed thresholds |
| `lock_wait_ms`, `rows_decoded` | `src/factory_status.rs` `Counters` | Always `None` today (asserted by `status_keeps_missing_decision_measurements_unknown_across_historical_schemas`) |
| Per-decision admission line | `src/watchdog.rs` `admission_log_line_observed` | Ticker log line, allowlisted `reason`, `task_id`, `duration_ms`, `sql_work`; not durable, not canonical |
| Admission pause | `src/watchdog.rs` `pause_reason`, `note` | Side file `.state/admission-paused.json`, reasons `disk_full`/`database_busy`/`admission_paused` |

### Profiles and capability evidence

| Field | Where | State |
| --- | --- | --- |
| `FrozenProfile{kind, name, definition_digest, arguments_digest, environment_names, execution_home, permission_policy, agent, herdr, adapter, capabilities, workflow_certificate}` | `src/domain/profile.rs` | Retained in attempt inputs v2 (`LaunchInputs.effective_profile`, `src/domain/attempt_inputs.rs`) |
| `agent: ExecutableIdentity{path, digest, version}` | `src/domain/profile.rs`; probed in `src/profile_preparation.rs` (`observe`, kinds `herdr`/`claude`/`codex`) | Exact harness version and binary digest are available per attempt |
| `ProfileCapabilities.structured_usage` | `src/domain/profile.rs`; set `CapabilityEvidence::Unknown` in `src/profile_preparation.rs` (profile build), `src/profile_preparation/revalidation.rs`, `src/agents/profiles.rs` | **Always `Unknown`**; no producer sets it |
| Requested model / reasoning effort | `src/profile_config.rs` `validate_gated_preparation` | **Refused**: a worker profile with `model`, `reasoning_effort` or `environment` fails ("unsupported model or environment mapping") |
| Profile reference | `FrozenProfile::reference` (`src/domain/profile.rs`) | SHA-256 over the whole profile including `agent.path`, `execution_home`, `config`, capability evidence; too broad for an arm identity (G3) |
| `native_profiles` | `migrations/0025_native_profiles.sql`, `src/store/native_profiles.rs` | Immutable sealed native verification reports keyed by profile digest |
| `capability_evidence` | `migrations/0033_capability_evidence.sql`, `src/store/capabilities.rs` | Levels `discovered`…`workflow-certified`, `live` flag, expiry; comment: "Provider selection, effort, and process environment are not mapped here" |
| Adapter identity | `src/profile_preparation.rs` `adapter: canonical-local-herdr-0.9.1` rev 3 | Launch transport adapter, not a telemetry adapter |

### Budget

`src/domain/budget.rs`: `BudgetLimits{max_attempts, max_provider_tokens,
unknown_usage: Refuse|AllowIncomplete}`, `UsageAvailability::Unknown` is the only
variant. `src/store/budget.rs` (`budget_report`, `blockers_for_policy`):
`max_provider_tokens` set + `Refuse` → blocker `provider_usage_unavailable`;
`AllowIncomplete` → `incomplete=true`. No path accepts provider usage. The
slice keeps this unchanged (TM2.4 bridge deferred).

### Attempt lifecycle and F1 producers

| Record | Where | Timestamps |
| --- | --- | --- |
| `attempts(id, task_id, revision, state, snapshot, reservation, termination_observed)` | `migrations/0001_project_store.sql` | **None** |
| `events(sequence, kind, entity, revision, payload_version, payload)` | same | **None** |
| Reservation (`reserved`) | `src/store/reservations.rs` `admit_prepared` | `operations.due_unix_ms = now` only |
| `launching` | `src/store/launch.rs` `apply_launch_started` | none retained |
| `running` | `src/store/worker_brief.rs` `apply_worker_brief` | none retained |
| Terminal (`completed`/`failed`/`cancelled`/`lost`) | `src/store/worker_termination.rs` `record_worker_termination_with_budget`, `record_launch_stopped_with_budget`; `src/store/worktrees.rs` `stop_worktree_preparation_with_budget`; `src/store/reservations.rs` `cancel_attempt_in_transaction` | `now` passed in, not stored on the attempt |
| Completion request (task `complete`, PR #58) | `src/store/worker_termination.rs` `request_completion` | Revision bump and `attempt.completion_requested` event only; the terminal `completed` (task `succeeded`) is then written by `record_worker_termination_with_budget` (cause `Completion`) |
| Non-canonical attempts (no `attempt_inputs` row) | `src/store/ownership.rs` `adopt_runtime` inserts `adopt-*` attempts as `running`; `src/store/mod.rs` `commit` (`Mutation::Attempt`) writes any state but refuses sealed launch attempts | No dispatch decision or inputs; outside the outcome record (added for S3) |
| `result_submissions` (attempt, candidate_oid, created_unix_ms) | `migrations/0026_factory_results.sql` | yes |
| `verification_runs`, `verified_results` | `migrations/0027_verification_runs.sql` | yes |
| `integration_operations`, `integration_candidates`, `integrated_commits` | `migrations/0028_integration.sql`, `0045_result_integration.sql` | yes |
| `dependency_satisfactions` | `migrations/0030_dependency_satisfaction.sql` | yes |
| `task_contracts` (route `verify_then_integrate`/`verify_only`, repository), `contract_scope_paths`, `contract_named_resources`, `resource_claims`, `task_dependencies.requirement` | `0026`, `0032`, `0035`, `0030` | contract properties usable for taxonomy |

Canonical attempts are created **only** in `SqliteStore::admit_prepared`
(`src/store/reservations.rs`), reached by three production paths:

| Path | Caller | Chooser today |
| --- | --- | --- |
| Operator launch reserve | `src/launch_preparation.rs` `reserve` (CLI `launch <slug> reserve`) → `reserve_prepared` | Owner-signed approval; one profile from `LaunchSelection.profile` |
| Automatic admission | `src/admission.rs` `decide_held` → `reserve_prepared_controlled` | First profile in `binding_profiles` order with a matching launch approval |
| Delegated reserve | `src/store/delegated_reservation.rs` `reserve_delegated` → `reserve_delegated_prepared` | Derived grant (`grant_id`) |

### Launch environment (relevant to Codex binding)

`src/worker_supervision.rs` `isolated_gated_command` runs the agent under
`env -i HOME=<execution_home> PATH=/usr/bin:/bin …`; `CODEX_HOME` is not set,
so Codex writes under `<execution_home>/.codex/` (unverified live, see §2).
The execution home is per profile, not per attempt
(`src/canonical_worker/resources.rs` `validate_execution_home`), so concurrent
attempts on one profile share one sessions directory. Worker cwd is the
attempt worktree `<project>/.state/worktrees/<attempt-id>/repo-NN[/rel]`
(`src/domain/worktrees.rs` `worktree_plans`, `worktree_execution_route`); the
launch spec carrying cwd is single-use and deleted
(`src/canonical_worker/launch_spec.rs`).

### Other surfaces

- PR observations: `src/pr.rs` (`pr_line`, `reduce`, `describe_change`) keeps a
  fixed field set for legacy thread reports; bodies never copied.
- Waiting-on-you: legacy thread groups (`src/overview.rs`, `src/steps.rs`); no
  canonical attempt equivalent.
- `src/threads.rs`, `src/token_jobs.rs`: pane-metadata "tokens", not LLM usage.
- Plugin manifest `herdr-plugin.toml`: `[[panes]]` `overview`/`new` as popups
  running `target/release/herdr-projects pane <id>` (`src/actions.rs` `run_pane`).

### Missing (all slice-relevant)

No `AgentConfiguration`, `DispatchDecision`, `TaskClassification`, attempt
lifecycle timestamps, outcome record, usage ingestion, telemetry store,
`telemetry` CLI, or fleet pane. No `src/telemetry/` module.

## 2. Codex 0.154.0 field matrix

Observed read-only from local rollout JSONL
(`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`); not yet observed under an
execution home.

| Record (`type` / `payload.type`) | Fields used | Semantics / status |
| --- | --- | --- |
| `session_meta` | `id`, `session_id`, `timestamp`, `cwd`, `cli_version`, `originator`, `source`, `model_provider` | Binding keys; `cli_version` gates certification |
| `turn_context` | `turn_id`, `model`, `effort`, `cwd`, `approval_policy` | Reported model/effort per turn (may change) |
| `token_usage_record` (top-level) | `session_id`, `thread_id`, `turn_id`, `response_id`, `usage{input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens, reasoning_output_tokens, total_tokens}`, `turn_token_usage{…}`, `thread_token_usage{…}` | One per response. Σ`usage` = final `thread_token_usage`; `total = input + output`. **Accounting basis** |
| `event_msg/token_count` | `info.total_token_usage`, `info.last_token_usage`, `rate_limits{limit_id, primary{used_percent, window_minutes, resets_at}, secondary, credits, plan_type}` | Totals can be **lower** after compaction → not an accounting basis; rate limits only; difference recorded as discrepancy |
| `event_msg/task_started` | `turn_id`, `started_at` | Turn lifecycle |
| `event_msg/task_complete` | `turn_id`, `started_at`, `completed_at`, `duration_ms`, `time_to_first_token_ms` | `last_agent_message` is content → dropped |
| `response_item/*`, `world_state`, `item_completed` | — | Content; never read past type tag |
| Guardian / auto-review subagent | separate rollout, `turn_context.model = codex-auto-review` | Parent linkage unreliable |

Needs live verification before certification: rollouts land under
`<execution_home>/.codex/sessions`; `session_meta.cwd` equals the attempt
worktree cwd; `cached_input ⊆ input` and `reasoning_output ⊆ output`; whether
guardian rollouts share the worker cwd (and must be included or excluded);
resume/compaction effect on per-response ordinals; rate-limit semantics
(`used_percent` scope, `resets_at` unit); per-turn model changes; incremental
flush while running; a partial last line after the process is killed.

**Where doc 04 differs.** Doc 04 names the app-server `thread/tokenUsage/updated`
notifications and native OpenTelemetry as the Codex surfaces and has no rollout
file surface. For Herdr-pane workers neither is usable: the app-server session
is not the interactive terminal (doc 04 itself forbids assuming equivalence)
and OTel bodies can carry prompt content. The slice therefore uses the
"Codex terminal session / certified native event/log surface" row, concretely
the rollout JSONL. Doc 04's `model/rerouted` has no rollout counterpart
observed; model changes are visible only as `turn_context.model` changes.

## 3. Gap list: plan assumptions that no longer hold

| # | Plan assumption (doc) | At main | Consequence |
| --- | --- | --- | --- |
| G1 | Dispatch via `thread start --config/--reason/--race` (15 §6, TM1.8) | Canonical launches use `launch <slug> reserve` with a `deny_unknown_fields` `LaunchSelection` (`src/launch_preparation.rs`), automatic admission and delegated reserve | Reason code is a new optional selection field; `--race` out of scope |
| G2 | Eligible set = configurations the coordinator weighed (02) | Only automatic admission iterates several profiles (`src/admission.rs` `binding_profiles`); operator/delegated paths name one profile | Operator/delegated decisions have a singleton eligible set, probability 1 |
| G3 | `AgentConfiguration` includes requested model, effort, prompt-policy, toolset digests (03 §2) | Model/effort mappings refused (`src/profile_config.rs`); no prompt-policy digest; `FrozenProfile::reference` hashes host paths | Configuration derived from a subset of `FrozenProfile`; model/effort explicit null with reason |
| G4 | Attempt identity can carry dispatch data | Attempt/operation IDs derive from `LaunchInputs` (`record_ids`, `src/store/reservations.rs`) and approvals match inputs (`matches_launch`) | Decision must be a separate row, never part of `LaunchInputs` |
| G5 | Controller already has lifecycle times (TM1.8) | `attempts`/`events` have no timestamps | Slice adds lifecycle marks written at each transition |
| G6 | Task terminal cohort from task state (07 §1) | `tasks.state='succeeded'` is set only by generic commits (`src/store/mod.rs`), not by F1 integration | Acceptance derived from `verified_results`/`integrated_commits` per contract route |
| G7 | Waiting-on-you → `HumanAttentionInterval` (TM1.8) | Only legacy threads have waiting-on-you (`src/steps.rs`) | S4 deferred; M31–M33 unavailable |
| G8 | Codex surfaces = app-server/OTel (04) | Workers run in Herdr panes under `env -i HOME=` | Rollout JSONL adapter (§2) |
| G9 | Missing data never zero (00) | `factory_counters` returns 0 for absent tables | Telemetry must not reuse those counters as metrics; fix is out of slice |
| G10 | Plugin panes run `$HERDR_PLUGIN_ROOT/bin/herdr-projects telemetry watch` (15 §3) | Manifest runs `target/release/herdr-projects pane <id>` via `run_pane` handoff | Panel is a new `pane` id, not a new manifest convention |
| G11 | Separate telemetry process with spool, outbox, bindings (02, TM0.4, TM1.1) | None exist | Deferred; slice reads canonical rows directly and writes the sidecar in-process |
| G12 | Execution home per attempt (implied by binding) | Per profile, shared across attempts | Binding must use cwd + time, not directory alone |
| G13 | Plan F-card references (14) | F1 producers landed (schemas 26–45) | Verification/integration outcomes are available now for the outcome record |
| G14 | Features compile unconditionally | Store code sits behind `feature="state-store"` (`src/cli.rs`) | Telemetry CLI and store code use the same gate |
