# Review correction checkpoint

The work is local and uncommitted on `fix/factory-review-main`, based on merged
main `9e628fe` (the reviewed `bd6433d` tree). Nothing has been pushed. This is a
handoff of the current work, not acceptance of the full factory-scale plan.

## Corrections implemented

- Signed contract dependencies constrain reservation, including contract revision
  binding, policy-specific evidence, and late dependency attachment.
- Foreground admission, queue, profile selection, and related controller reads
  use scoped queries and propagated budgets. This does not establish a universal
  history-independent bound for every operation.
- Wait subscriptions retain future wakes and filter relevant identities.
- Disposable verification and integration repositories support SHA-256 objects.
- Historical migration fixtures use migration prefixes rather than pretending
  current tables are an old schema.
- Fresh verification rejects changes to HEAD, index, and tracked contents made
  by check commands, and retains actual numeric check exit status.
- Receipt reuse requires native proof version 2. Old receipts and exact replay
  cannot claim the new post-execution checks; a fresh run is required.
- The verification backlog (`pending_verification_work`) keeps a submission
  pending until every acceptance policy of its contract revision has run, rather
  than clearing on the first run of any policy.

Additional work includes operator verification/integration commands, delegated
admission, signed planner operations, memory packages/barriers, and merged-output
checks. The detailed tracker links the individual evidence batches:
[corrections](2026-09-26-factory-corrections.md).

## Validation and test cleanup

The last full-suite checkpoint passed 1,408 tests, with zero failures and 22
ignored, in 24m49s. It predates the two latest verifier corrections. Their targeted
results must be considered separately; the full result does not certify the
current exact tree. Live adapter certification has not been performed.

The historical-receipt follow-up passed 158 distinct selected cases: signed CLI,
dependency satisfaction, integration, barriers, verifier, admission, memory
proposals, controller, factory harness, and historical CLI upgrade. The initial
barrier batch had one obsolete historical-reuse expectation; the corrected exact
test passed. Both outputs are retained in
`factory-corrections-evidence/factory-verifier-proof-v2-validation-summary.json`.
Default `cargo check --locked` and `git diff --check` also passed.

The cleanup removed or replaced 20 pre-existing test functions and eliminated
nine duplicate binary executions while retaining their library coverage. The
full-suite timing shows no meaningful improvement over the previous 24m44s run.
Root `AGENTS.md` requires E2E coverage for new or replacement tests. The test audit
records useful guarantees and measured results:
[test effectiveness](2026-09-27-test-effectiveness.md).

The latest two removals were tests of the unused `contracts/phase_b.rs` model.
Their replacement coverage uses actual CLI result ingress, lost-attempt status,
and ticker recovery after a lost response. The old two-test target took 0.00s
at the full checkpoint, so this removal has no measurable runtime benefit.

## Remaining work

The review's broader specification gaps remain open: automatic verifier and
integrator scheduling with durable claims/recovery; complete planner and reviewer
orchestration; remaining contract/vector and packaging acceptance; the specified
combined scale workload and frozen performance targets; live dependent-worker
and multi-adapter pilots; rollout ramps, backup/restore drill, and release record.
See the original review's F0–F5 table and the
[production wiring audit](2026-09-27-production-wiring-audit.md).

The scope expanded from corrections into missing implementation. Further work
should be tracked against explicit review requirements and delivered in bounded,
reviewable batches, rather than repeated open-ended audit cycles.
