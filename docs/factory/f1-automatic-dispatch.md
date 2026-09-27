# F1 automatic verification and integration dispatch

Status: planned, 27 September 2026. Scope is the dependent vertical slice (F1):
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

### 1. Durable verification jobs (enqueue only)

- Migration `0044`: per-project automation switches, off by default.
- `store/verification_jobs.rs`: for each (submission, acceptance policy) still
  pending, one `verification.run` operation. Identity is
  sha256(project_store, submission, contract_revision, policy_id, policy_digest).
  At most 8 new operations per turn; the insert rechecks contract revision,
  policy and fence in the same transaction.
- `canonical_controller.rs::finish_poll` calls the service; CLI
  `hp result auto <slug> --verify on|off --expected-head N`.
- Not yet executable: the kind is left out of the dispatch hint.
- E2E first: `ticker_enqueues_one_verification_job_per_policy_across_restart`.
  Two-policy contract and a submission; automation off gives no operations;
  on gives exactly two pending operations with the expected ids; a restart
  keeps two and creates no `verification_runs`.
- Stop: that test passes and existing tests are unchanged.

### 2. Supervised verifier job with recovery

- `canonical_verification_jobs.rs` on the finalization-job template; queue,
  hint (pending and ambiguous) and `offer_next` wiring; a separate lane.
- `verification/mod.rs` gains an internal entry taking the stored policy bytes
  and a deterministic scratch directory `.state/scratch/verify/<op>`.
- Recovery: unrecorded or lease-expired jobs look up the run by key and confirm
  it, or remove scratch and redeliver with the same key.
- E2E first: `ticker_auto_verifies_once_and_recovers_after_kill`. Kill the
  ticker mid-check, restart: exactly one run and one receipt, delivery
  confirmed, dependent satisfaction valid, scratch removed. A policy exiting 3
  is rejected with `exit_status=3` and releases nothing.
- Stop: that test and the verifier and CLI subsets pass.

### 3. Serial integration per target

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

### 4. Dependent release through the ticker

- Mostly test code; glue only where the chain breaks.
- E2E first: `ticker_auto_chain_releases_verified_integrated_and_fan_in_dependents`.
  A verified-result dependent becomes ready after verification; an
  integrated-commit dependent only after integration, with the integrated SHA
  as base; a fan-in dependent's base contains both predecessors; a controller
  restart mid-chain produces no duplicates.
- Stop: that test passes, one full suite run is green, and a gate note records
  that this is fixture-only evidence.

## F1.7 live check (needs explicit authorization to spend)

One authenticated worker on the first adapter edits a disposable repository;
its result is verified and integrated automatically and a dependent task
launches on the integrated SHA, with one controller crash and one stale-head
injection. Record the integrated SHA, transcripts and which parts were live
versus fixture. Automation stays off by default until this passes.
