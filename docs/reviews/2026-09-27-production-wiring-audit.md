# Production wiring audit — incomplete

This audit follows the local corrections to merged `main` (`9e628fe`). It records
current source evidence, not release acceptance. The complete review and F0–F5
requirements remain in scope. No live workers were launched.

## Independent verifier and integration dispatch

The plan's results/verification document requires a verifier service receiving a
controller-issued operation and a serial integration queue per target ref.
Current operator entry points are `verification::verify_project`,
`integration::integrate_project`, and `integration::reconcile_project`.
`src/cli.rs` calls them from explicit `result` commands. They protect scratch
ownership and invoke native evidence producers; their existence does not prove
automatic scheduling.

`src/canonical_controller.rs::finish_poll` services routine scheduling, barrier
stops, prepared effects/admission, waits and replan requests. Inspection found no
verifier or integration scheduler there. The effect queue in
`src/copy_jobs/queue.rs` uses the bounded executor; producer jobs must join that
supervised scheduling path rather than block observation/admission polling.

Migration 43's `pending_verification_work` is a submission-level backlog
projection. A submission leaves it only once every acceptance policy of its
contract revision has a verification run (covered by the multi-policy case in
`task_contract_put_and_result_submit_keep_worker_bytes_untrusted`). Its current
consumers in `src/store/admission_read.rs` and `src/store/observability.rs` read
backlog ages. It is not a durable leased verifier operation queue. Before reusing
it for production dispatch, define policy-level job identity and claims:
`load_verify_target` takes a specific policy ID. This is a design requirement for
the missing dispatcher, not a dependency-release bypass.

Integration has durable operation states and reconciliation, but its existing
pending-operation index starts after an operator has created an operation. A
producer must also discover eligible verified results that have not yet created
an integration operation, respecting the signed route and configured target.

Next implementation requirements:

- Select bounded work under current store/control/contract/policy fences; derive
  immutable producer job identities and durable claims before external work.
- Pass only controller-selected policy bytes and isolated scratch ownership to
  supervised verifier workers. Preserve original deadlines and termination proof.
- Queue eligible verified results by target; retain serial ownership and expected
  ref CAS. Reconcile interrupted publication before generating another effect.
- Retry/recover jobs from durable state, without interpreting worker output or
  executor completion as a trusted receipt.
- Extend public workflow coverage through submission, automatic verification,
  integration, dependent readiness and restart recovery. Existing operator-driven
  E2Es establish producer behavior, not this missing automatic chain.

The live dependent-worker gate, independent review, broader F2/F3 orchestration,
F4 combined workload measurements and F5 rollout/restore evidence remain open.

## Verifier outcome fidelity: next reproduction

Source inspection identifies an outcome-fidelity gap: `verification::persist`
derives `exit_status` as `Some(1)` whenever the reason is `checks_failed`, instead
of retaining the supervisor's reported check status. `setup.rs` emits the actual
check code in `hp-verify checks=...`, but maps codes outside 0..70 to setup failure;
`ChildReport` does not retain the status. The store then writes the derived value
for rejected runs. Plan F1.2 requires observed process outcomes, so this needs an
E2E with a signed policy whose command exits with a distinct status, plus replay
checks against persisted run data. This is source evidence; no new behavioral
reproducer has run while the full suite is active.

Also inspect validation after the check process exits: setup checks the initial
HEAD/tree/worktree before running the command, then emits status and checks for
leftover children. A public workflow should establish whether a check that
changes tracked files or HEAD can still mint a receipt. Do not infer a confirmed
bypass from this inspection alone; reproduce with retained Git objects and the
real isolated verifier before changing the outcome rules.

The existing staged/untracked/tamper tests in `src/verification/tests.rs` inject
changes before the check command runs. They do not establish post-execution
immutability. The next public workflow should separately exercise changed
tracked contents, index changes and HEAD replacement during an otherwise
successful check. Generated output needs a distinct expectation: tests commonly
create ignored build artifacts, so a post-check validation must protect exact
source inputs without accidentally forbidding ordinary generated output.

Git-only preflight evidence is retained in
`factory-corrections-evidence/verifier-outcome-command-preflight.json` and
`verifier-index-command-preflight.json`. In disposable SHA-256 repositories,
`cat-file -e not-an-object` exits 128, while index replacement, HEAD replacement
and tracked-file deletion can each exit zero. The index-only probe additionally
shows `git diff --quiet HEAD` returning zero while `git diff --cached --quiet
HEAD` returns one and `git write-tree` differs from `HEAD^{tree}`. Therefore the
current clean-tree helper's worktree comparison alone cannot establish index
identity. These are command preflights, not yet verifier acceptance reproductions.

## Completed full regression checkpoint

The full serial state-store suite passed: 1,408 passed, zero failed, 22 ignored
across nine targets. All 366 hashes in `factory-current-full-inputs.json` matched
after the process exited. `factory-current-full-summary.json` records exact
per-target results; `factory-current-full-wall-time.json` records exit zero and
1,488.72 seconds including compilation. The initial `/usr/bin/time` invocation
failed before Cargo started; the actual run used Python timing. The source edit
pause is now over. Next work is the real verifier outcome/mutation E2E described
above, followed by missing durable producer dispatch. These results do not close
any unperformed live gate or the broader review objective.

## Verifier reproduction and fresh-run correction

The real signed CLI E2E subsequently reproduced all four outcome defects:
index, HEAD and tracked-content mutations were accepted; exit 128 became setup
failure with no stored status. Fresh verification now rejects the three mutations
after process cleanup and records actual numeric check status. A successful
ignored-artifact check remains accepted. All 30 distinct selected regressions
passed; see `factory-verifier-outcome-validation-summary.json`. The next evidence
boundary was historical receipt reuse: old proof version 1 cannot attest that
these new checks ran. The subsequent correction requires version 2 for all
receipt reuse, including contracts without scopes or outputs. The signed CLI
reproducer failed before correction and now passes; migration and replay do not
backfill that assurance. See the `factory-verifier-proof-v2-*` evidence logs.
