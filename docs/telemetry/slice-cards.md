# Telemetry thin-slice cards

Fixed spec for implementers. Contracts: [contracts.md](contracts.md); code
facts: [baseline.md](baseline.md). Base `main` `ebfe10e`, schema 45.

Order: S1 → S2 → S3 → S5 → S6 → S7, then **owner review**. S4 (attention) is
deferred. Canonical migrations: S1 `0049`, S2 `0050`, S3 `0051`; each bumps
`SCHEMA` and adds its `include_str!` step in `src/store/mod.rs` and stays
loadable by `src/store/test_schema.rs` `historical`. S5 uses only the sidecar
`migrations/telemetry/0001_codex_usage.sql` (`historical` reads
`migrations/*.sql` non-recursively, so it is not picked up). All code sits
behind `feature = "state-store"`.

**Tests (all cards).** End-to-end only (AGENTS.md): public entry points (CLI
binary, `admission::admit_once`, `launch_preparation::reserve`,
`authority::reserve_delegated`) in temporary projects; assert persisted rows
and CLI output against hand-computed literals, never values recomputed by
production code. Extend `tests/factory_harness.rs` (`vertical_slice`,
`plant_profile`, `install_fixture_contract`) where it covers the path; new:
`tests/telemetry.rs`, `tests/fixtures/telemetry/`. Run
`cargo test --features state-store --test <file> <name>`.

**Stop (all cards)** and report, do not work around, if a change would: alter
`LaunchInputs`, attempt/operation ID derivation or approval matching; write
outside the named transaction; persist a field not in contracts §5/§7; turn
an unavailable value into 0; or need cross-database atomicity.

## S1 — Task classification v1 (0049)

**Done.** `task_classifications` (0049) written or reused in `admit_prepared`
for all three paths; no-contract features are `unavailable: no_contract`, and
`docs`/`tests` need at least one write path.

Goal: every reservation writes or reuses the contracts §1 row inside
`admit_prepared`. Files: `migrations/0049_task_classifications.sql`
(immutable, unique `(task_id, contract_revision, taxonomy, revision)`),
`src/domain/telemetry.rs`, `src/store/dispatch_log.rs`,
`src/store/reservations.rs`. Tests (`tests/factory_harness.rs`):
- `classification_is_written_before_first_attempt` — 3 exact + 1 uncertain
  write path, 2 dependencies, `verify_then_integrate` → one row, `code`,
  `large`, features `{write_paths:4, uncertain_write_paths:1,
  write_named_resources:0, dependencies:2, repositories:1}`.
- `second_attempt_reuses_classification` — cancel, re-queue, reserve → one row.
- `schema_write_classifies_schema_change`; `task_without_contract_is_unscoped`
  (`band = unknown`).
Stop if classification needs an outcome field or a read outside the transaction.

## S2 — AgentConfiguration and DispatchDecision (0050)

**Done.** `agent_configurations` and `dispatch_decisions` (0050) written in
`admit_prepared` after the attempt row for all three paths; drafts, refusals
and delegated replays write none. Refinements in contracts §3.

Goal: exactly one decision per new attempt, same transaction, all three
paths. Files: `migrations/0050_dispatch_decisions.sql`
(`agent_configurations`, `dispatch_decisions`, immutable),
`src/store/dispatch_log.rs`, `src/store/reservations.rs` (`DispatchContext`
argument on `admit_prepared` and its four wrappers), `src/admission.rs`
(`decide_held` per-profile status), `src/launch_preparation.rs` (optional
`LaunchSelection.reason`/`note`), `src/store/delegated_reservation.rs`. Tests:
- `automatic_admission_logs_eligible_profiles` — two matching profiles, first
  lacks approval → `eligible` = `[no_approval/0, chosen/1000000]` ppm,
  `automatic_admission`, `["first_matching_approval"]`.
- `launch_reserve_records_operator_reason` (`tests/cli.rs`) — `reason =
  operator_preference`; note `/home/u/x?token=abc` plus a 40-char token →
  `~/x [redacted]`, ≤160; chooser `approval:<digest>`; no reason →
  `["unspecified"]`.
- `delegated_reserve_logs_grant_once` (`tests/delegated_reservation.rs`) —
  replay returns the same attempt, still one decision, `grant:<grant_id>`.
- `configuration_identity_is_stable_and_versioned` — same profile twice → one
  ID; agent version `0.154.1` → new ID; stored JSON equals the literal bytes
  in the test (`requested_model: null`, reason `mapping_unverified`).
- `rejected_reservation_writes_no_decision` — stale head and drafts leave
  `count(attempts) = count(dispatch_decisions)`.
Stop if passing the eligible set would widen what a caller may launch, or a
delegated replay would write a second row.

## S3 — Attempt outcome record (0051)

**Done.** `attempt_lifecycle` (0051) marks written in each transition's
transaction; `telemetry <slug> attempts [--json]` projects one outcome per
canonical attempt. The success path extends the F1 completion test
(`tests/cli.rs`); the upgrade test starts from schema 50, 0051's actual
predecessor. Refinements in contracts §4.

Goal: lifecycle marks at every transition, the read-only outcome projection,
and `herdr-projects telemetry <slug> attempts [--json]`. Files:
`migrations/0051_attempt_lifecycle.sql`; marks in `src/store/reservations.rs`,
`src/store/launch.rs`, `src/store/worker_brief.rs`,
`src/store/worker_termination.rs`, `src/store/worktrees.rs`;
`src/telemetry/outcome.rs`; `src/cli.rs`. Tests (`vertical_slice` extensions):
(A task whose attempt was cancelled cannot be re-queued through the public
CLI; tests needing a second attempt reset the task as S1's
`second_attempt_reuses_classification` does — now the `requeue` helper in
`tests/factory_harness.rs`. A decision's `rowid` order is insertion order.)
- `outcome_success_path` — four marks in order; `candidate_oid` = submitted
  commit; `accepted`/`integrated`/`accepted = true`; Claude fixture →
  `usage: unavailable adapter_absent`; `attention_not_collected`.
- `outcome_rejected_verification_reason_is_excerpted` — one line, ≤160, `~`.
- `cancelled_before_launch_is_censored_not_zero` — `active_ms` unavailable,
  `cancelled`, no `0` in the JSON.
- `pre_0051_attempt_reports_predates_lifecycle_log` — upgrade from 49.
Stop if a transition path exists that baseline §1 does not list (add it first).

## S5 — Codex usage adapter (sidecar)

**Done; live certification done** ([codex-live-0.154.0.md](codex-live-0.154.0.md)).
`telemetry <slug> collect|usage` and a ticker hook (once per 300 s, 8 MiB)
write `telemetry.db` (0001); `CERTIFIED = ["0.154.0"]`, and the gate test
`codex_usage_binds_and_sums_exactly` runs un-ignored. A session holding rows
collected before certification stays `cli_version_uncertified` (never `0`).
Outcome `usage` wiring (`telemetry attempts`) is still not done: follow-up.
Refinements in contracts §5.

Goal: `herdr-projects telemetry <slug> collect` scans Codex execution homes,
binds rollouts, writes `telemetry.db` idempotently; outcome `usage` reads it.
Files: `migrations/telemetry/0001_codex_usage.sql` (`rollout_sources`,
`codex_usage`, `codex_quarantine`, `codex_discrepancy`, `codex_rate_limits`,
`codex_turns`, `collect_offsets`), `src/telemetry/{mod,sidecar,codex,redact}.rs`,
CLI. Fixtures `tests/fixtures/telemetry/codex-0.154.0/*.jsonl`: synthetic,
hand-written, `cli_version 0.154.0`, canaries in `last_agent_message`,
`response_item`, `base_instructions`; `cwd` = a real reserved attempt's
worktree. Tests (`tests/telemetry.rs`):
- `codex_usage_binds_and_sums_exactly` — `1000/400c/120o/80r` +
  `500/100c/60o/20r` → input 1500, cached 500, output 180, reasoning 100,
  total 1680; matching final `thread_token_usage` → no `thread_total`
  discrepancy; `token_count` total 900 → one `token_count_total` row. Gate
  test: passes only after the live run certifies `0.154.0` (no test hook).
- `collect_twice_is_idempotent`; `rewritten_record_is_quarantined` (usage
  `unavailable: quarantined`); `uncertified_version_keeps_no_counters`
  (`0.999.0`); `partial_last_line_waits` (ingested once completed);
  `invariant_violation_is_not_accepted` (`total ≠ input + output`).
- `rollout_before_decision_or_elsewhere_is_unbound` — earlier timestamp, cwd
  outside the worktree, or file outside the execution home.
- `content_never_persists` — no canary bytes in `telemetry.db`, its WAL, or
  CLI output.
Live certification (Codex only, small): one canonical Codex attempt on a
scratch project within an owner-approved budget; record each baseline §2 live
item as observed or not in `docs/telemetry/codex-live-0.154.0.md`; certify
`0.154.0` only if home location, cwd equality and both subset rules hold.
Stop if rollouts are not under the execution home, `cwd` differs from the
worktree, a subset rule fails, or ordinals restart within one file.

## S6 — Metric report

**Done.** `telemetry <slug> report [--json|--text] [--since MS]` reads
`state.db` read-only and the sidecar if present; nothing is written. Like S5's
gate, `usage_metrics_follow_certified_sources` runs un-ignored since `0.154.0`
was certified. Refinements in contracts §6.

Goal: `herdr-projects telemetry <slug> report [--json|--text] [--since MS]`
for M02, M07, M08, M09, M13, M15, M40; M31–M33 listed unavailable. Files:
`src/telemetry/metrics.rs`, CLI; no migration. Tests (`tests/telemetry.rs`):
- `golden_acceptance_and_amplification` — contracts §6 scenario → M02 `2/3`,
  M07 `5/2`, open tasks 2.
- `usage_metrics_follow_certified_sources` — S5 fixture + one Claude attempt
  + one unbound Codex attempt → M08 1500, M09 180, M13 `1/2` with
  `adapter_absent: 1`, M15 `2/2`.
- `no_source_is_unavailable_not_zero` — empty project → M02 null
  `empty_denominator`, M08 `unavailable: no_certified_source`.
- `quota_headroom_at_dispatch` — `used_percent "37.5"` 60 s before the
  decision → M40 `"62.5"`, age 60000; nothing earlier → unavailable.
Stop if a metric needs a field not in contracts §5.

## S7 — Basic workspace panel

**Done.** `pane fleet` (popup, no handoff needed) renders the project of the
invoking workspace, else every project with a canonical store: collection
header, the S6 text report, then active attempts (`<kind> <agent_version>`,
attempt state, elapsed since the `reserved` mark, usage or `n/a (<reason>)`).
Stores are opened read-only without creating SQLite side files (`immutable`
when no `-wal` exists; the S6 report uses the same open). `doctor` prints
`telemetry: <sidecar presence; last collect age>` per store (last collect =
newest `collect_offsets.updated_unix_ms`, i.e. last ingest); the default build
says the panel needs `state-store` and doctor warns. No action opens the pane.

Goal: read-only `pane fleet` popup (`herdr-plugin.toml` `[[panes]] id =
"fleet"`, `placement = "popup"`, command `target/release/herdr-projects pane
fleet`, dispatched by `src/actions.rs` `run_pane`) showing the S6 text report
and active attempts (configuration label, state, elapsed, usage or `n/a`),
plus a `doctor` line for sidecar presence and last collect age. Files:
`herdr-plugin.toml`, `src/actions.rs`, `src/telemetry/panel.rs`,
`src/doctor.rs`. Tests (`tests/cli.rs`):
- `fleet_text_matches_report_json` — every number equals the S6 JSON;
  unavailable renders `n/a`, never `0`.
- `fleet_without_sidecar_says_unavailable` — usage `n/a`, header "collection
  not run", exit 0, nothing written.
Stop if the panel would write, launch, or read content outside contracts §7.

**Owner review gate** after S7: scratch-project demo, S5 live evidence
(done: `0.154.0` certified, zero canaries, see
[codex-live-0.154.0.md](codex-live-0.154.0.md)),
`attempts = dispatch_decisions`; contracts §8 stays deferred.

**Follow-ups found in S7:** the panel and S6 report open the canonical store
`immutable` when no `-wal` file exists (a plain read-only open of a WAL
database creates `-wal`/`-shm` files); a writer that starts mid-read could
make one display read inconsistent pages — display only, never the store.
The S6 report still opens an existing sidecar read-write. No action opens the
fleet pane yet. "Last collect" is the last ingest time.
