# F1 automatic verification and integration dispatch

Status: cards 1–3 merged (#42, #48, #49), cards 4–5 done; the live F1.7 check passed on 28 September 2026 (`docs/reviews/2026-09-27-f1-live-gate.md`). Planned 27 September 2026. Scope is the dependent vertical slice (F1):
verified worker results are integrated automatically and release dependents.
Each card is one reviewable PR. Work stops at each card's stop condition; gaps
found along the way are recorded as new cards, not folded into the current one.

## Decisions

1. Automation is an explicit per-project opt-in, off by default, until the
   live F1.7 check passes.
2. When the integration target has moved since verification, the operation
   stops and blocks for an operator or replan. No automatic rebuild yet.
3. The crash window between `update-ref` and its record stays covered by the
   in-crate integration tests. No fault hook is added to the shipped binary.
4. Verifier and integration jobs run in their own executor lane with their own
   time budget (verifier timeouts reach 300 s). One verifier at a time to start.
5. `unshare` isolation is mandatory for automatic runs. Without it, jobs pause
   with a visible reason and never run unsandboxed.
6. Existing admission and feedback paths are accepted for F1.7; revisit only if
   the live check exposes a problem.

## Cards

The existing controller effect pattern is reused: `operations` and
`operation_delivery` hold claims, leases and ambiguous state;
`store/controller_hint.rs` selects work by kind; `canonical_controller.rs::offer_next`
hands it to `copy_jobs::Queue`; `canonical_finalization_jobs.rs` is the job
template. Stored verification/integration receipts already write
`dependency_satisfactions`, so only the automatic producers are missing.

### 1. Durable verification jobs (enqueue only) — done (#42)

- Migration `0044`: `result_automation_control` with a `verify` switch, off by
  default. Card 3 adds the integration switch in its own migration.
- `store/verification_jobs.rs`: for each (submission, acceptance policy) still
  pending, one `verification.run` operation. Identity is
  sha256(project_store, submission, contract_revision, policy_id, policy_digest).
  At most 8 new operations per turn; the insert rechecks contract revision,
  policy and fence in the same transaction.
- `canonical_controller.rs::finish_poll` calls the service; CLI
  `hp result <slug> auto --verify on|off --expected-head N` (slug first, like the
  other `result` subcommands). Enqueueing needs the project to be `active`.
- Not yet executable: the kind is left out of the dispatch hint.
- E2E first: `ticker_enqueues_one_verification_job_per_policy_across_restart`.
  Two-policy contract and a submission; automation off gives no operations;
  on gives exactly two pending operations with the expected ids; a restart
  keeps two and creates no `verification_runs`.
- Stop: that test passes and existing tests are unchanged.

### 2. Supervised verifier job with recovery — done (#48)

- `canonical_verification_jobs.rs` on the finalization-job template; queue,
  hint (pending and ambiguous) and `offer_next` wiring; a separate lane.
- `verification/mod.rs` gains an internal entry taking the stored policy bytes
  and a deterministic scratch directory `<project>/.verify-scratch/<op>` (beside
  `.state`: the verifier refuses a checkout inside the store directory).
- Card 1 inserts `verification.run` operations without an `operation_delivery`
  row; create it (pending) when the job becomes dispatchable.
- Recovery: unrecorded or lease-expired jobs look up the run by key and confirm
  it, or remove scratch and redeliver with the same key.
- A permanently failed job must not block its (submission, policy) forever:
  the unique index from `0044` allows one job per pair, so define how an
  operator retries it (a new operation identity or an explicit reset).
- E2E first: `ticker_auto_verifies_once_and_recovers_after_kill`. Kill the
  ticker mid-check, restart: exactly one run and one receipt, delivery
  confirmed, dependent satisfaction valid, scratch removed. A policy exiting 3
  is rejected with `exit_status=3` and releases nothing.
- Stop: that test and the verifier and CLI subsets pass.

### 3. Serial integration per target — done

- Eligible results: every policy accepted, `verify_then_integrate` route, a
  configured target, no integration operation yet.
- At most one non-terminal integration operation per (repository, ref),
  enforced in the enqueue transaction; ambiguous operations reconcile before
  any new effect; expected-old compare-and-swap on every ref update.
- E2E first: `ticker_auto_integrates_two_results_serially_and_recovers_stale_and_crash`.
  Two results land in order; an externally moved ref blocks instead of being
  overwritten; a crash during candidate checks yields one publication; a ref
  checked out by a user is refused.
- Out of scope: remote push, pull requests, feedback routing (F1.6).
- Stop: that test and the integration subset pass.

### 4. Dependent release through the ticker — done

- Mostly test code; glue only where the chain breaks.
- E2E first: `ticker_auto_chain_releases_verified_integrated_and_fan_in_dependents`.
  A verified-result dependent becomes ready after verification; an
  integrated-commit dependent only after integration, with the integrated SHA
  as base; a fan-in dependent's base contains both predecessors; a controller
  restart mid-chain produces no duplicates.
- Stop: that test passes, one full suite run is green, and a gate note records
  that this is fixture-only evidence.
- Result: no production glue was needed. Consumers carry queue edges only (no
  signed consumer contract), and factory admission stays off, so a released
  edge reports `admission_disabled:<edge>` instead of missing evidence. Gate
  note: `docs/reviews/2026-09-27-f1-fixture-gate.md`.

### 5. Launch released dependents — done

- Operator `launch draft` binds every queued edge to its current valid satisfaction (the signed grant covers them) and refuses a still-blocked dependent; reservation checks that evidence whether factory admission is on or off.
- Admission and delegated drafts bind the task's newest retained worker snapshot for the profile, so the brief is built; without one the candidate is skipped (`knowledge_missing`) instead of launching promptless.
- Every integrated-commit dependency, even a single one, must be an ancestor of the pinned base.
- E2E: `ticker_auto_chain_releases_verified_integrated_and_fan_in_dependents` now drafts, signs and reserves `c` on `a`'s integrated SHA, builds its brief and worktree, and refuses a blocked dependent and a base without the integrated commit.

## F1.7 live check (needs explicit authorization to spend)

One authenticated worker on the first adapter edits a disposable repository;
its result is verified and integrated automatically and a dependent task
launches on the integrated SHA, with one controller crash and one stale-head
injection. Record the integrated SHA, transcripts and which parts were live
versus fixture. Automation stays off by default until this passes.

Result: **passed** (live attempt 2 of 3), `scripts/test-live-f1`. A real Codex
worker, launched by the ticker on stock Herdr 0.9.1, committed A's change; the
ticker verified and integrated it (`1d205ca9…`) across a `kill -9`, a stale
target was blocked and recovered by an operator integration, and a second
Codex worker started for B on a worktree whose HEAD was A's integrated SHA.
Glue: the stock exec line is sent with an Enter key, profile preparation has a
60 s budget, and the Codex version probe runs with the execution home.
Decision 1 (automation off by default) is unchanged; lifting it is a separate
decision.

## Follow-up cards found along the way

Recorded here rather than folded into the card that found them.

- **Two workers on one Herdr server fail start naming** (F1.7) — done (this
  PR): the fence compares only the target agent's entry, and recovery finishes a
  recorded rename the target never received. The naming
  fence in `canonical_worker/start.rs` compares the whole `agent.list`, so any
  change in another agent's entry refuses the rename; recovery
  (`allow_name=false`) then loops on "native worker name mismatch". Compare
  only the target agent, and let recovery finish an unapplied rename.
- **No completion path for a finished worker** (F1.7) — done (this PR):
  `hp task <slug> complete <task> --expected-revision N` records a completion
  request for the task's started attempt, refused unless one of its
  submissions has an accepted run with a current contract check for every
  acceptance policy. The existing termination path stops the worker and
  releases capacity only on proven exit; the attempt becomes `completed` and
  the task `succeeded` (`blocked` if memory obligations remain), never
  `cancelled`. Repeats return the recorded request. Dependents still rely only
  on verified or integrated evidence. Operator-only: the controller does not
  complete automatically after verification yet. Only cancellation stopped a
  finished worker, which marked the task cancelled, so its dependents
  reported `predecessor_failed`.
- **Integration rechecks one policy** (F1.7): done (this PR). The candidate
  check runs every acceptance policy of the contract revision, in id order
  (each with its own budget; see below), and publishes only if all pass. Each verdict is
  recorded in `integration_policy_checks` (migration 0046) and returned as
  `policies`; a failing policy is named in the job diagnostic.
- **Codex cannot commit under `workspace-write`** (F1.7) — done (this PR):
  workers keep `workspace-write` and never commit; the controller commits for
  them with `result capture` (see `adapters.md`, "Worker sandbox and
  commits"). Its sandbox makes Git metadata read-only, including a linked
  worktree's gitdir. The live harness still runs `danger-full-access` and has
  the worker commit; switching it to capture needs its fixture mode to prepare
  worktrees through the product.

- **Notification enqueue reads the whole snapshot** (`runtime.rs`
  `enqueue_notification`) — done (this PR). Migration 0048 indexes unseen
  inbox items and claimed or ambiguous deliveries; `notification_rows` reads
  only those (with their operations), the task and the coordinator route,
  fenced to the head in one transaction. The notification delivery adapters
  (operator and ticker) validate over the same rows.
- **Remaining full-snapshot reads** in `finalization_delivery::enqueue`,
  `preserved_outputs`, the finalization adapter validation and
  `routines::schedule` — done (this PR). Enqueue reads the binding, its task
  and that task's live attempts (`finalization_binding_rows`); the adapter
  and the receipt commit validate over `finalization_rows`; preserved outputs
  read only termination events naming the binding (or naming none legibly,
  indexed in 0048); scheduling reads the routine's latest revision;
  `dispatch_one` reads one operation. Same approach as #43
  (`store/effect_rows.rs`).
- **Opening a store runs a whole-database `quick_check`**, so even targeted
  commands grow with database size — done (this PR). Hot paths (controller
  polls, effect jobs, targeted commands over `effect_rows`/`admission_read`)
  open through `open_active_scoped`/`open_active_unchecked` and skip the
  check, except on the first open after the schema version changes.
  Migration, upgrade, restore, `doctor`, `factory status` and snapshot reads
  keep it and refuse a corrupt store. The ticker runs the check at most once
  per project per `HERDR_PROJECTS_INTEGRITY_CHECK_SECS` (default 3600), with
  a 30 s budget, recorded in `.state/integrity-check.json` so restarts do not
  repeat it. A failure pauses admission (`integrity_check_failed`) and is
  never repaired automatically; `factory status` and `doctor` show the last
  check. No schema change. E2E:
  `hot_paths_skip_the_whole_store_check_and_the_ticker_checks_once_per_interval_then_pauses_on_corruption`.
- **Transferred locks can still outlive release.** Guards now unlock on drop
  (#46), but a lock handed to a supervisor through `inherit_transfer` is only
  closed, so a child forked concurrently on another thread can hold it until
  it execs. Three unit tests still hit this in parallel runs. A fix needs a
  gate between process spawning and transferred-lock release (about 68
  `Command::new` sites).
- **`approval_read_does_not_double_count_already_budgeted_inputs`** spent
  seconds of its 5 s budget even when run alone, and occasionally missed it on a
  loaded machine — done (this PR). The time goes to decoding and hashing its
  7 MiB payloads in an unoptimised build; the test asserts the byte budget, not
  the clock, so its read now runs under a far deadline. Deadline behaviour keeps
  its own tests.
- **Port the reviewed memory-review reminder work** (Herdr threads `t-0003`
  implementation and `t-0004` review, branch
  `hp/grok4-7-shiptest/t-0003-memory-workflow-reliability` at `b564f15`) —
  done (this PR). Replayed onto `main`: durable memory-review obligations from
  worker Remember sections, delivered by the canonical tick as one SQLite inbox
  row, committed before `notified` advances. No schema change was needed (the
  intake writes `inbox_items` and `inbox.delivered`). Adapted to current
  `main`: delivery holds project ownership and reads only the head and the
  row instead of a whole snapshot, and a divergent row is an error before the
  head check, so it never spends a conflict retry. E2E:
  `ticker_delivers_one_memory_review_row_and_a_crash_retry_neither_duplicates_nor_double_counts`
  drives full `ticker run` passes; legacy workflow and doctor checks are
  covered through the CLI. Herdr threads `t-0003` and `t-0004` can be closed.
- **A disposition inside the reminder crash window wedges that reminder.** If a
  crash lands after the reminder row commits but before `notified` advances,
  and the obligation is then deferred, the retry renders a different summary
  under the same stable id. Delivery reports divergent bytes on every tick and
  never advances (the port keeps the reviewed behaviour; the E2E pins it).
  Treating a committed row for that id as delivered regardless of summary
  would clear it.
- **Verification jobs bound to an older task revision never run** — done (this
  PR). A task revision change after enqueue left the job pending and
  unclaimable, and it kept counting toward backlog age. The producer now
  retires a pending (never a claimed or ambiguous) job whose fence no longer
  holds, at most 8 per turn, as a permanent failure with reason
  `task_revision_changed` and a `<lane>.job_retired` event, and enqueues a job
  fenced on the current revision (its id also binds the revision). Migration
  0047 adds the fence to the one-job unique indexes. Integration jobs get the
  same treatment. A retired job cannot be retried with `retry-*`. E2E:
  `ticker_replaces_result_jobs_bound_to_an_older_task_revision`.
- **The verifier holds project ownership for the whole check** — partly done
  (this PR). An automatic verification job now holds exclusive project
  ownership only to load and fence its inputs, claim, prepare scratch and
  record. The isolated check runs under a `CheckGuard`: the shared root (so
  migration and cleanup are still refused) plus a `scratch` resource fence on
  the job's scratch directory, which observation also takes before removing
  it. Afterwards the job regains ownership (retrying until its deadline),
  revalidates the claim (lease and task revision) and reloads the target; if
  the submission, contract, policy or attempt changed, it records no verdict
  and the job is retried or, after a task revision change, left for lease
  expiry, observation by key and replacement. Without regained ownership it
  records nothing. E2E:
  `ticker_auto_verification_releases_project_ownership_during_the_check`.
  The operator `result verify` command now does the same — done (this PR): it
  holds runtime ownership (project lock and record lock) to load and record,
  runs the check under a `CheckGuard` fenced on its work directory, regains
  ownership within its timeout, and records nothing if the task, submission,
  contract, policy or attempt changed (the target now carries the task
  revision); the error names the changed inputs. A run recorded under the same
  key meanwhile is replayed. E2E:
  `operator_verify_releases_project_ownership_during_the_check`.
  Still open: integration jobs keep exclusive ownership throughout, because
  their candidate policy checks run inside `integrate_job` between the merge
  and the compare-and-swap publication.
- **No E2E for the isolation-unavailable pause** (`verification.paused`); it
  cannot be simulated from outside without a hook in the shipped binary.
- **Integration job follow-ups (card 3):** retry command done (this PR):
  `hp result <slug> retry-integration <op> --expected-revision N` returns a
  permanently failed integration job to pending under the same key and records
  `integration.job_reset`; the next run rechecks the target, so a still-moved
  ref blocks again untouched. `result jobs` shows the reason and retry hint.
  Still open: jobs advance only on the 15 s ticker cadence.
- **Naming recovery with no recorded intent loops** — done (this PR). If a
  start claim expired before the `runtime.launch_name` intent was recorded,
  recovery had no authority to rename and retried without end. Recovery that
  finds the exact worker without its name, no naming intent and an ambiguous
  (expired) claim now closes the launch delivery as a permanent failure with
  reason `start_unnamed` and the `cancel-attempt` remedy, and the controller
  stops offering it. The worker keeps running and holding capacity until an
  operator cancels the attempt; nothing is renamed. E2E: the `expired` case of
  `workers_sharing_one_herdr_server_are_named_independently_and_recover_unapplied_names`.
- **Integration policy rechecks share one 30 s budget** — done (this PR). Since
  every acceptance policy was rechecked on the candidate within one shared
  budget, several slow policies could exhaust it and block the job. Each policy
  now gets its own 30 s check budget, and the integration claim is extended to
  cover all of them (30 s per policy plus 30 s to record) before the checks
  run. A contract with more than 6 policies, which one lease and the 240 s job
  budget cannot cover, is refused before any build with a diagnostic naming the
  limit. E2E: the `five` (two 18 s policies publish) and `six` (seven policies
  refused) cases of
  `ticker_auto_integrates_two_results_serially_and_recovers_stale_and_crash`.
- **`stock_herdr_unconfirmed_launch_closes_its_workspace_and_counts_nothing`
  failed once** (exec-line check in the stock shell stand-in) — done (this
  PR). The fake Herdr appended `exec-requests` with `open('a')`, so the
  stand-in could see the file before its line was written and read it empty
  (reproduced under CPU load). The fake now publishes the file by rename and
  acknowledges the typed line only after the stand-in has read it and checked
  the spec, so the launch cannot give up and remove the spec first. Under
  heavy artificial load the product's own 10 s launch budget can still run
  out before the workspace close; that is the budget, not the stand-in.
- **Periodic integrity check limits** (open-check PR): it runs inline in the
  ticker pass with a 30 s budget, so a very large store may never finish a
  check; move it to a background lane or make it incremental. A failure pauses
  only admission; queued effect jobs keep running on the store until an
  operator acts. A leftover `integrity-check.json` stays in `.state/migration/`
  after migration.
- **Dependent start fails naming on its own Herdr server** (telemetry live
  certification, 2 of 3 live attempts): worker B, alone on an isolated server,
  failed with "native agent changed before naming" and then `start_unnamed`.
  This is a different race from the shared-server one fixed in #55. The live
  F1 harness under `workspace-write` + capture has not passed end to end yet;
  A's side (sandboxed work, capture, verify, crash fault, integration, stale
  recovery) passed in all three attempts.
- **Replace-verdict tests** (352 in `docs/reviews/2026-09-27-test-audit.tsv`):
  convert to E2E alongside feature work, starting with `store/barriers.rs`.
