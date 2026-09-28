# Test effectiveness cleanup — in progress

Latest count: **20 pre-existing test functions removed or replaced**, plus
**nine duplicate binary executions removed** while retaining library coverage.
The latest batch removed two prototype-model tests, described at the end.

The first audit covers plan acceptance, retained intent and inspection. It does
not establish that every test in the repository is effective. Test counts are
not counts of independent bugs detected.

## Replaced coverage

These seven focused tests were removed after moving their observable guarantees
into `tests/cli.rs::planner_session_cli_retains_inputs_and_replays_bound_proposals`:

| Removed test | Replacement evidence |
| --- | --- |
| `proposal_path_does_not_call_verify_signature_or_write_contracts` | Actual CLI proposals succeed without signed grants and leave installed contracts, scopes, reservations and queue unchanged. Source-string absence did not prove helper behavior. |
| `plan_propose_leaves_scope_tables_empty` | Compare real authority-table counts before and after the complete CLI flow. |
| `replay_of_the_same_bytes_returns_one_plan_revision` | Retry through separate processes; verify unchanged head, retained bytes/digest, proposal/revision counts and audit-event count. |
| `stale_parent_conflicts_and_returns_the_current_revision` | CLI refuses a stale submission without advancing the head; stored parent/digest and inspection expose the accepted revision. Exact Rust error variant remains covered in competing-session tests. |
| `same_key_and_different_bytes_conflict` | CLI refuses changed bytes with an existing key and leaves history unchanged. |
| `successive_proposals_retain_unmaterialized_planned_tasks_after_restart` | Separate CLI processes accept a dependency on prior intended work without materializing execution tasks or queue entries. |
| `inspection_pages_exact_intent_and_rejects_revision_changes` | CLI checks empty inspection, current and source revisions, replacement text, digests, pagination, stale continuation and missing revision refusal. |

The workflow also covers version-3 creation, supersession, dependency addition
and cancellation requests, exact retry after each operation, wrong project/store/
actor/key/digest refusal, stale source references and reused request IDs. Local
E2E here means the real compiled CLI, filesystem, migration and SQLite store;
no live model/provider is involved.

## Demonstrated fault detection

Temporarily removing the production digest comparison for idempotent replay made
the CLI test fail at its changed-payload refusal assertion. The mutation runner
restored production source byte-for-byte in a `finally` block.
See `factory-corrections-evidence/factory-plan-e2e-mutation.log`. A mutation caught
by this workflow is evidence for that boundary, not a mutation score for the suite.

## Retained focused coverage

Keep corruption, concurrent acceptance, migration backfill, transaction rollback,
byte limits and measured history-cost tests. They exercise failures that a happy
CLI flow cannot establish. A new focused fault-injection test aborts request
publication after earlier transaction writes and checks rollback; it also proves
that cancellation of intended work preserves a running attempt and its capacity.
Its initial fixture incorrectly recreated a session at a newer event cursor; the
corrected fixture reuses the original retained session, without relaxing the guard.

The broader audit remains open. Source-string checks in controller, admission,
status, verification and migration tests are candidates for behavioral replacement;
inspect each test's actual runtime assertions before removing any part. E2E tests
also need auditing: being in an integration target does not make a test effective.
Use affected checks during development and full suites at substantive checkpoints.

Final affected validation: 54 focused planning cases passed in 13.00 seconds;
the consolidated CLI case passed in 1.74 seconds (13.42-second incremental build).
Logs: `factory-typed-plan-focused-final.log` and `factory-typed-plan-cli-final.log`
in `factory-corrections-evidence/`. No full suite was rerun for this batch.

## Status, watchdog and harness cleanup

The next batch replaces two report-level status tests with
`factory_status_cli_redacts_history_preserves_capacity_and_refuses_unknown_schema`.
It runs the compiled CLI against a migrated project with one retained attempt and
40 retired attempts. It verifies redaction, explicit unknown measurements, active
page counts, pause reporting, unchanged canonical state, and refusal of unknown
schemas without invented counters or an implicit migration. It then adds a
malformed unrelated historical task: full snapshot decoding demonstrably fails,
while the real status command succeeds and retains capacity.

This replaces status assertions that searched source for `drop(tx)`, particular
helper names, absence of `read_snapshot`, and absence of termination writes.
Transaction and deadline behavior still have dedicated controlled-store tests;
watchdog capacity behavior has direct state assertions. A deliberate mutation
that added a full snapshot read to production status was caught by the CLI
workflow at the malformed-history boundary. Source was restored byte-for-byte.
See `factory-corrections-evidence/factory-status-e2e-mutation.log`.

The watchdog's source-string busy-timeout assertion was removed. Its old lock
fixture installed a test-only busy handler that gave up immediately; this did
not test production timeout wiring. The replacement uses a real `SqliteStore`
mutation blocked by another SQLite write transaction, verifies a bounded retry
rather than immediate refusal, checks no event/task writes and no released
capacity, records the pause, and successfully retries after the lock is released.
This remains a focused integration test because it directly exercises the
contention boundary without depending on controller scheduling timing.

Removed the document-only `memory_gate_doc_is_a_simulator` test and the
`assert_scale_gate_text` / `fault_doc` helper assertions. They checked exact prose,
constant source text and absence of particular claims, rather than the behavior
of the simulator. Runtime memory, scale and fault scenarios remain intact; their
results still do not certify live adapters or latency gates. Review those claims
against actual evidence, not whether a document contains approved phrases.

The initial new CLI test did not compile because `Commit` was imported through a
private store re-export. Its import now uses the public domain type. The CLI test
then passed in 0.72 seconds excluding compilation; five watchdog cases passed in
0.89 seconds. Final affected validation is recorded in the correction tracker.

## Probe manifest cleanup

Removed the library-only `agents::probe` module: it constructed its own manifest,
asserted its own hard-coded fields, and optionally searched the host PATH for a
Claude executable. This was not a production capability-manifest implementation.
Also removed the duplicated Claude manifest harness and Codex happy-path harness
from `src/agents/probe.rs`. The meaningful mapping-refusal assertions remain
covered by `profile_config::tests::shared_validation_and_mapping_errors_do_not_expose_values`
and `profile_file_with_model_still_errors` (Claude and Codex respectively).

The existing CLI probe test now exercises both adapters using explicit executable
fixtures, real process invocation and production JSON output. It checks exact
observed versions, refusal of missing executables, redacted profile settings,
unknown capabilities, and no launch/protocol/certification grant. A process that
prints a plausible version and exits with failure must report `probe_failed`
without a version. Fixtures refuse any argv other than `--version`; installed
provider binaries are no longer discovered by these routine tests. The expanded
CLI case passed in 0.10 seconds. Existing focused tests for replacement races,
retargeted aliases, cancellation, shared deadlines, truncated output and explicit
process environment remain in place.

Final validation for these status/watchdog/harness/probe changes: 25 distinct
affected cases passed. The restored status E2E passed in 0.61 seconds, the probe
E2E in 0.10 seconds, eight retained probe boundary tests in 0.12 seconds, five
watchdog cases in 0.89 seconds, historical status in 0.92 seconds and nine factory
harness cases in 44.70 seconds. Compilation is excluded from those durations.
No full-suite or live certification claim follows from this selected run.

## Controller coverage exposed a production bug

Removed `targeted_hot_path_is_the_production_default`, which parsed synchronous
controller source for function names, constants and match arms. It never exercised
the background maintenance implementation used by the ticker. That implementation
still called `runtime::snapshot_controlled` solely to inspect `schema_version`.

The existing ticker CLI test now inserts an unrelated malformed retired task,
proves full snapshot decoding refuses it, and requires the running ticker to
commit a newer observation anyway. Before the fix, the ticker remained alive
without producing the observation and the E2E test timed out. After replacing the
background snapshot with the scoped schema reader, the same test passed in 2.57
seconds including its original stop/cancellation/restart sequence. The malformed
record remains present until the test explicitly removes its fixture; controller
progress must not erase diagnostic history or alter tasks/attempts.

The synchronous controller's ambiguous-notification test now runs with the same
kind of malformed cold task and checks the actual delivery row. It must expire
the claim without replaying the notification. Deadline fault injection remains:
the initial guard test now slows the schema-header query rather than an obsolete
full-history query, while post-probe SQL still cannot reset the original budget.

Evidence: `factory-background-history-before.log` and
`factory-background-history-after.log` in `factory-corrections-evidence/`.
The first attempted background unit-test filter selected zero tests because the
module is nested under `canonical_controller::observations`; it supplies no
coverage. The corrected `canonical_controller::` run covers both controller paths.

Controller batch final validation: 32 controller cases passed in 46.89 seconds,
four targeted-reader cases in 2.13 seconds, and the ticker CLI in 2.57 seconds:
37 distinct affected passes. No full-suite rerun was needed for this scoped change;
remaining performance and test-effectiveness audits are still open.

## Explicit upgrades and verifier isolation

Removed nine duplicated source-parsing checks for whether `open()` contains
`upgrade_v1` or a migration filename, from admission, plans, delegation,
capabilities, memory read sets, active-work inventory, update packages, barriers
and consumer bindings. Their historical-data migration tests remain intact.
The new CLI matrix uses actual migration prefixes 32 through 42, verifies ordinary
status reads leave the old schema intact, verifies new inspection is refused,
checks an execution guard prevents explicit upgrade, and then upgrades through
the real CLI. Task rows, retained attempt capacity, event head, control state and
exact proposal bytes/digest survive. Current intent backfills, repeated upgrades
are harmless, and foreign keys remain valid. A deliberately inserted upgrade in
ordinary `SqliteStore::open` failed this test at schema 32; source was restored
byte-for-byte. The initial unmodified matrix passed in 3.19 seconds.

Removed the verifier's source-string mount/pivot-root test. Its real verification
helper now compares the parent process's mount namespace, mount table and root
filesystem identity before and after accepted/rejected verifier runs. The existing
same-namespace setup test still uses a syscall filter to require exit before any
mount call. Nineteen selected verification tests passed in 11.37 seconds.

Removed consumer-routing helper-name assertions. The signed-promotion harness
now explicitly proves that a retired consumer's legacy subscription still exists
before the next promotion; fresh delivery must select the current binding, while
preserving pending obligations inherited from the retired binding.

Removed the default-admission/source-writer scan: the signed factory admission CLI
workflow already asserts a new project starts off, unsigned/SQL-shaped requests
are refused, and only valid owner-authorized policies change state. It observes
actual stored policies and denials rather than scanning source for an SQL string.

Eleven retained `upgrade_v1` migration cases passed in 2.89 seconds. Selected E2E
follow-ups and their final results are recorded in the correction tracker. This
is not a claim that all source-based tests or all ineffective tests have been
removed; PR feedback source scans and other test categories remain to audit.

Final upgrade/isolation/routing/admission validation: 33 distinct affected cases
passed (11 migration, 19 verification, three E2E). Restored upgrade E2E took 3.51
seconds; memory routing 0.36 seconds; signed admission 0.64 seconds, excluding
compilation. The broader cleanup and original correction goal remain open.

## PR observation and forged worker receipts

Removed `pr_modules_do_not_write_feedback_or_satisfaction`, which only scanned two
Rust files for table/helper names. Its replacement exercises the asynchronous PR
poller, executor and real subprocess against a migrated project. The local `gh`
fixture returns MERGED/APPROVED plus attacker-chosen comments and fabricated
feedback/satisfaction fields. Production reduction yields a PR summary; both
verified-result and integrated-commit prerequisites remain blocked. Store state,
retained attempt capacity and evidence tables remain unchanged after cached reads
and a fresh executor. This is a process/store integration test of the poller
boundary, not a canonical PR-refresh CLI or live GitHub certification.

Removed `worker_forged_json_is_only_a_display_copy`: it parsed a hard-coded JSON
object as `serde_json::Value` and checked those same fields. The real signed
contract/result CLI workflow now submits valid result documents with forged
`verified` and `verification_receipt` fields. Both are refused without changing
canonical state. A subsequent legitimate submission and retry succeed, while
verification, satisfaction and feedback tables remain empty and task/attempt
state remains unchanged. The compile-fail test preventing deserialization of
trusted `VerificationReceipt` remains and passed.

Removed the planning migration's SQL-wording checks for verification writes.
The actual accepted-verifier workflow now rebuilds the original schema-35 prefix,
checks that ordinary open preserves that version, explicitly upgrades, and compares
every stored value of its real verification run and verified receipt before and
afterward. Existing capacity and wait-replay checks continue after the upgrade.

Validation: two poller cases passed in 0.32 seconds, forged-receipt CLI in 0.36
seconds, five feedback cases in 0.49 seconds, receipt compile-fail in 0.03 seconds,
and accepted receipt upgrade in 0.25 seconds: ten distinct affected passes,
excluding compilation. The broader test audit and factory implementation remain
in progress.

## Remaining documentation and migration wording checks

Removed baseline, vertical-slice and planning-gate documentation phrase checks
from the factory harness. Markdown wording is not evidence that the workflows
work or that a live provider was tested. The actual deterministic scheduler,
Git/verifier/integration and planning workflows remain. The vertical slice already
reads the migrated admission state and requires `off` before enabling its fixture;
its duplicate SQL substring assertion has been removed. Owner-authorized
admission is separately exercised through the CLI.

Removed capability migration checks for the absence of `model`,
`reasoning_effort` and a dispatch constant. Retained behavioral capability tests
check evidence immutability, profile-digest changes and certification boundaries.
The real profile-probe CLI workflow checks redaction and uncertified output for
both adapter kinds, and now compares configuration bytes after successful,
missing-executable and failing-executable probes. This establishes configuration
preservation through the public command, rather than from SQL vocabulary.

No live provider was called. These removals do not establish that every remaining
test is useful; the audit records specific replacements rather than a suite-wide
effectiveness score.

Final validation: profile-probe CLI passed in 0.04 seconds; eight capability
cases passed in 0.76 seconds. Factory harness: test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 36.01s
Logs: `factory-probe-config-preservation.log`,
`factory-capability-behavior-cleanup.log`, and
`factory-harness-text-check-cleanup.log` in `factory-corrections-evidence/`.
No full-suite rerun was performed for this batch.

## Going-forward policy and removal count

The user requested E2E-only new/replacement coverage; this is now recorded in
`AGENTS.md`. Existing useful focused tests can still run. Relative to merged
`main` (`9e628fe`), 20 test function names disappeared from tracked Rust files.
Two are renamed controller deadline tests, leaving **18 pre-existing test
functions removed or replaced**. This count excludes individual assertion/helper
removals and intermediate tests introduced and removed during corrections.

No post-cleanup full-suite timing is available. The earlier full serial run took
about 24½ minutes. Selected workflow timings are not a comparable measurement of
full-suite improvement. Removing cheap text assertions while adding E2E workflows
may increase runtime; no speedup is claimed.

## Signed contract cycle enforcement

Extended the existing signed-contract CLI workflow with direct, transitive and
queue-mediated cycles. It observes unchanged canonical state and no new contract
revision on refusal, then removes the blocking edge through a newer signed
revision and successfully installs the now-acyclic revision. Temporarily removing
the production cycle-validation call made this E2E test fail because installation
incorrectly succeeded; the mutation script restored source byte-for-byte in a
`finally` block. See `factory-contract-cycle-mutation.log`.

No new unit tests were added. Twenty existing contract-related tests passed in
2.83 seconds, covering compatibility, admission, barriers and verification.

The restored CLI workflow passed in 1.30 seconds; 21 distinct affected tests
passed in this batch. See `factory-contract-cycle-cli-restored.log`.

## Duplicate target audit during full regression

Read-only `--list --format terse` queries against the already-built test binaries
listed 795 library and 541 binary cases. Nine names overlap, all from the same
`src/source_tree.rs` module compiled into both crate roots. The remaining 532
binary names are distinct; discarding that target would discard useful coverage.

A small unapplied draft at `/tmp/factory-shared-source-tree-next.patch` makes the
Linux state-store binary use the library's existing public source-tree module.
It retains the local module for builds where the library does not export it.
This would avoid executing those nine module tests twice without deleting their
coverage. Apply and validate only after the current suite terminates. This is a
pending draft, not a measured runtime improvement or completed change.

Read-only timing investigation: ticker CLI workflows explicitly wait across
production's 15-second polling cadence (`src/ticker.rs::TICK`). The ticker sleeps
until its next scheduled wake and only selects a 250 ms cadence for pending
canonical worker transitions. Several other executor completion paths wait for
the normal poll. This is a production wakeup behavior to investigate with E2E
coverage, not justification for weakening waits or introducing a test-only fast
clock. No change or measured aggregate saving is claimed yet.

The wakeup investigation also found a constraint: local observation `begin_pass`
clears ready samples and local-report reads expire consumed samples on the next
pass. Simply waking the entire ticker for every executor completion would cause
fresh observations to be repeatedly admitted. A safe implementation must separate
completion/effect servicing from observation admission cadence (or introduce
explicit bounded sample timing), rather than just shortening `TICK` or changing
sleep duration. No speculative wakeup patch was applied.

## Full post-cleanup checkpoint

The full serial suite passed: **1,417 passed, zero failed, 22 ignored** across
nine targets. Source/test hashes (359 files) matched. Wall time including build
was 24m 44s; summed test execution was 24m 17s versus 24m 20s previously. The
3.74-second difference (0.26%) is not evidence of a meaningful cleanup speedup.
There are 14 fewer passing test cases than that earlier checkpoint; this net
change differs from the 18 removed/replaced pre-existing functions because
replacement and other correction coverage was added.

See `factory-post-cleanup-full-regression-summary.json` for per-target counts and
timings. Its parser keeps each cargo target's final summary, avoiding duplicate
counting of the one-test child printed inside the library log. The pending
reserved-contract and shared-module changes are not included in this checkpoint.

## Reserved-result binding and shared-module completion

The signed delegated-reservation E2E reproduced an attempt reserved under contract
revision 1 submitting against newly signed revision 2. With the shared binding
check fixed, it rejects that result, accepts/replays the original revision, and
preserves immutable inputs and capacity. The before log fails at the intended
assertion; the final E2E passed in 0.92 seconds. No new unit test was added.

The shared-module draft is now applied. Linux state-store binary tests list 532
cases instead of 541, with nine source-tree duplicates absent; all nine still pass
in the library. Default builds retain the local module, and the default build
check passed. This removes nine duplicate executions, not nine unique test
functions or their coverage. The 18 removed/replaced pre-existing function count
remains a separate metric.

Affected validation passed with 63 distinct test names across the selected
CLI, factory harness and existing compatibility runs. See
`factory-reserved-contract-validation-summary.json`. These changes postdate the
full checkpoint; their effect on total suite time has not been measured.

## Operator verifier E2E ingress

The signed delegated-reservation workflow now invokes the production `result
verify` command with real retained Git objects and the isolated verifier process.
It requires rejection/nonzero exit for a substituted policy, accepts the exact
installed policy, replays the same run without a second receipt, preserves a
pre-existing scratch directory, removes owned scratch after completion, and
refuses oversized/symlinked policy inputs before recording a run. The work remains
synthetic adapter coverage, not live provider certification.

The final CLI passed in 1.03 seconds; 19 existing verifier cases passed in 12.27
seconds, and the default build passed. No new unit tests were added. Evidence uses
the `factory-verifier-cli-*` prefix. This addition is after the full checkpoint.

## Changed-file scope enforcement E2E

The signed contract/result CLI scenario now uses real SHA-256 Git commits instead
of opaque synthetic object bytes. It verifies an in-scope change while leaving
an outside file unchanged, then rejects an outside-to-inside rename. An injected
SQLite stop-write failure rolls back verification and feedback; retry creates one
rejection, cancellation and pending replan item while retaining attempt capacity.
Replay is stable and the public feedback command records a linked replan request.

Removing the production scope check made the test fail on the unauthorized
candidate; source was restored byte-for-byte. This tests an actual verifier
decision rather than declared scope strings. No unit tests were added.

Affected validation passed: 76 distinct cases. The existing ignored cross-project
controller case was explicitly run with matching compiled test binaries and
local synthetic workers (7.24 seconds); its nested helper is not counted again.
The default build passed. `factory-scope-validation-summary.json` records names
and timings; `factory-scope-verifier-mutation.log` records fault detection.
The full suite was not rerun for this batch.

## Historical verifier proof and receipt-fixture replacement

The signed result CLI E2E now verifies that a current receipt satisfies a queued
dependency, absence of scope/output proof blocks that dependency, replay does
not manufacture proof, and fresh verification restores usable evidence. A
mutation bypassing the shared proof guard fails at the historical-receipt
assertion. No new unit test was added.

The affected factory harness exposed direct SQL fabrication of accepted scoped
receipts in its old-attempt fault campaign. Those inserts are now replaced by
real retained-object verification while retaining its stale-attempt dependency
checks. This replaces fixture setup within an existing test; it does not change
the removed-test-function count or establish a full-suite timing improvement.

The corrected campaign passed in 0.90 seconds. Selected validation covers 112
passing cases, including the eight other unchanged factory cases from the prior
run. Both fixture-failure logs are retained; the initial replacement needed its
service scratch directories created explicitly. See
`factory-scope-provenance-validation-summary.json` for exact run provenance.

## Operator integration and SHA-256 recovery E2E

The existing signed result CLI workflow now configures and integrates an actual
SHA-256 repository. It verifies both merge parents and files from independent
worker/target changes. It exposed default-SHA-1 scratch initialization in both
initial and resumed integration. The corrected workflow also refuses absent
scope-check proof and existing scratch directories, survives injected check and
receipt write failures, reconciles an already-updated ref, and replays with one
receipt while preserving attempt capacity. The final E2E passed in 2.01 seconds.
No new unit tests were added, and no test function was removed in this batch.

All 43 affected cases passed across the extended CLI, existing integration and
verification regressions, and nine factory harness workflows. Timing is recorded
in `factory-integrator-validation-summary.json`; no full-suite rerun was made.

## Required outputs in the integrated tree

The existing signed CLI workflow reproduced successful integration after the
target deleted an unchanged required file. Its policy (`git diff --quiet`) passed,
showing why the verifier's earlier candidate-file check was insufficient. The
workflow now checks missing, symlink, directory and symlink-ancestor outputs in
real merged Git trees, unchanged target refs and capacity after rejection, and
replay without duplicate feedback. This extends an existing E2E without adding
unit tests or changing removed-function counts. The before log is
`factory-merged-outputs-before.log`; selected validation uses the same prefix.

All 43 selected cases passed. The final CLI rerun (2.54 seconds) additionally
checks a legitimate required file named `push`, preventing the Git command guard
from treating literal filenames as transport requests. Other selected regressions
precede that final path-prefix adjustment; exact provenance is in
`factory-merged-outputs-validation-summary.json`. No full-suite speedup is claimed.

## Historical integrated-receipt proof

The existing signed CLI E2E now queues consumers of an actual integrated commit,
removes its merged-output proof to model historical acceptance, and requires
both existing and later consumers to remain blocked. Terminal replay cannot
restore proof; fresh integration with a new key restores usable evidence while
retaining both receipts. Disabling the production proof guard makes this E2E
fail at the historical-integration assertion. Source was restored byte-for-byte.
This adds no test function and no unit tests. Evidence is recorded under
`factory-integrated-proof-*`; full-suite timing has not been repeated.

The extended CLI passed in 2.83 seconds. All 93 selected cases passed across CLI,
dependency, integration, barrier, historical-upgrade and factory workflows.
`factory-integrated-proof-validation-summary.json` records per-target counts
and timings. The historical-proof mutation failed at the intended assertion.

## Receipt-selection fallback and attachment deadline

The signed CLI workflow now creates a newer accepted receipt without scope proof
and queues a consumer that must use the older current proven receipt. Before the
fix, it failed at the dependency-satisfaction assertion. The same E2E injects a
recursive SQLite stall at satisfaction insertion, requires a production deadline
error before its independent test watchdog, checks complete queue/event/evidence
rollback, then retries successfully. No new test function or unit test was added.
Evidence is under `factory-receipt-selection-*`. These checks establish behavior
and a shared failure bound, not history-independent performance or suite speedup.

All 116 selected cases passed. The CLI completed in 5.10 seconds including its
intentional two-second database stall. The initial deadline assertion needed
case-insensitive matching of the existing `Deadline` diagnostic; the final run
verifies rollback and retry. Exact evidence is in
`factory-receipt-selection-validation-summary.json`. No full-suite timing change
is claimed.

## Queue mutation excludes cold metadata

The scheduler CLI E2E now retains invalid UTF-8 in an unrelated task title and a
terminated attempt's snapshot. These values pass SQLite structural checks, but
the previous broad readers fail while decoding them. The updated mutation and
policy paths succeed without touching those bytes, while a real dependency
cycle still fails with an unchanged head. The first fixture used an invalid
state enum and therefore exercised the administrative integrity check instead;
`factory-queue-inventory-before-decode.log` retains the refined reproduction.
The queue path now shares its scoped command deadline with receipt attachment.
No new test function or unit test was added. Selected logs use
`factory-queue-inventory-*`.

All 84 selected cases passed. The queue CLI completed in 0.30 seconds and the
receipt/deadline CLI in 4.94 seconds; remaining runs cover scheduler, dependency,
reservation, historical-upgrade and factory workflows. See
`factory-queue-inventory-validation-summary.json` for exact counts and limits.

## Scheduler inspection excludes cold metadata

The existing scheduler CLI workflow now inspects the queue after retaining
invalid UTF-8 in unrelated retired task/attempt metadata. The pre-fix report
failed decoding those values; the scoped report succeeds, reports two queued
tasks and three free slots, and preserves the event head. Its integration label
also reflects the Linux operator commands. This extends an E2E without adding
a test function or changing removal counts. All 41 selected cases passed; exact
counts/timings are in `factory-queue-report-validation-summary.json`. No full-suite
speedup or universal history-independent latency is claimed.

## Native profile selection excludes unrelated reports

The existing signed CLI E2E installs and queues a capability-requiring contract,
then retains corrupt-hash reports for another adapter and another store inode.
Inspection previously failed on those unrelated reports; it now succeeds with
an unsupported-capability blocker. Adding a corrupt matching report must still
fail closed. This extends existing E2E coverage with no new test function. All
19 selected cases passed, including capability and historical-upgrade checks and
nine factory workflows. Evidence is recorded in
`factory-profile-selection-validation-summary.json`. Removed-function counts and
full-suite timing claims are unchanged.

## Admission inventory excludes old configurations and store identities

The existing planning/admission workflow now retains 300 valid historical profile
reports in addition to its eligible profile. Previously the public preparation
call failed at the inventory limit; after indexed filtering it reserves nine
non-conflicting workers, leaves the overlap unreserved, and retains all 301
reports. This is deterministic public-service workflow coverage with synthetic
adapter evidence, not live worker certification. No new test function was added.
All 33 distinct affected cases passed, with a final planning rerun checking the
retained-report count. Exact results and intermediate compile-fix provenance are
in `factory-admission-profile-inventory-validation-summary.json`. No full-suite
speedup is claimed.

## Capability evidence history measured through admission

The existing planning workflow now requires discovered/launchable capabilities
and records actual admission SQLite VM work before and after 30,000 retained
expired/overlapping observations. It catches the prior full-history scan:
4,636 steps become 3,454,636. Indexed membership checks use 5,066 steps at both
sizes while still completing nine permitted reservations and refusing the
resource overlap. The final workflow takes 4.48 seconds including fixture setup.
The initial setup run was stopped and its inefficient insert-source query fixed;
the refined pre-fix reproduction takes 3.50 seconds. No new test function was
added. All 33 affected cases passed. Full-suite timing remains unmeasured after
these changes; work-count improvement is specific to this admission workload.
See `factory-capability-window-validation-summary.json`.

## Budget decisions retain exact operator reports

The existing signed reservation CLI E2E now inspects budget totals after a
cancelled reservation and a subsequent reservation: cancellation leaves lifetime
usage at one, and the next reservation raises it to two. Authorization now uses
capped threshold counts; the public report continues to show exact totals. No new
test function was added. All 85 affected cases passed, including canonical brief
accounting/deadline coverage. A supplementary legacy briefing CLI also passed;
it is not evidence for the canonical path. The empty store::worker_brief filter
is excluded from counts. See `factory-budget-decision-validation-summary.json`.
No runtime speedup is claimed for this batch.

## Latest full-suite checkpoint after scoped reads

The full serial state-store suite passed: **1,408 passed, zero failed, 22 ignored**
across nine targets, with all 366 recorded input hashes unchanged. Wall time
including compilation was **24m49s**, compared with **24m44s** at the prior full
checkpoint. Summed test execution was **24m27s** (1,467.49 seconds). This remains
no meaningful full-suite speedup. The nine fewer passing executions (1,417 to
1,408) match the removed binary duplicates; the unique library tests remain.
The 18 removed/replaced pre-existing function count is unchanged. See
`factory-current-full-summary.json` for target counts and timing. Ignored live
cases and unfinished production wiring remain outside this passing result.

## Post-execution verifier outcomes

The signed CLI workflow reproduced accepted receipts after a successful command
changed the index, HEAD or tracked files, and loss of a check's actual exit 128.
It now requires rejection without a receipt for each mutation, exact recorded
status, unchanged attempt capacity, cleanup and stable replay. A fifth case
creates an ignored Git archive and must still be accepted. No test function was
added. All 30 distinct selected cases passed (35 executions; five store cases were
repeated by overlapping filters). The extended CLI takes 6.03 seconds. These
checks cover fresh verification; the subsequent version-2 proof correction below
covers historical reuse. The full
1,408-pass checkpoint predates this patch. Evidence is recorded in
`factory-verifier-outcome-validation-summary.json`.

## Historical verifier receipt reuse

The same signed CLI workflow now downgrades a receipt to proof version 1 and
requires integration and late dependency attachment to refuse it. Replaying its
original verification key must retain version 1; fresh verification must produce
version 2 and restore dependency readiness. This includes a version-1 contract
without scopes or required outputs. The before-correction run failed because
integration accepted historical proof; the corrected workflow passes in 15.95s.
No new test function was added or removed for this extension. The full-suite
checkpoint predates both verifier corrections and is not validation of this exact
tree. Evidence: `factory-verifier-proof-v2-before.log` and
`factory-verifier-proof-v2-cli-final.log`.

## Remove the unused prototype contract test target

`tests/contracts.rs` compiled `contracts/phase_b.rs`, a separate illustrative
model imported nowhere in production. Its two tests could stay green while the
actual runtime violated the same rules. Removed both tests and retained the
historical design sketch with explicit non-runtime labeling.

| Removed test | Production workflow coverage |
| --- | --- |
| `ambiguity_never_authorizes_blind_retry_and_lost_attempt_keeps_capacity` | `ticker_native_briefs_confirm_or_recover_uncertainty_without_replay` executes the real ticker, loses a synthetic service reply, restarts it and asserts exactly one prompt. `factory_status_cli_redacts_history_preserves_capacity_and_refuses_unknown_schema` now seeds a lost attempt without termination evidence and checks the actual CLI's retained slot count and unchanged persisted state, including after reading malformed unrelated history. |
| `fixture_and_stale_results_cannot_pass_as_verified` | `task_contract_put_and_result_submit_keep_worker_bytes_untrusted` rejects worker-forged verification fields, confirms raw claims create no verified receipts or satisfaction, and rejects historical proof until fresh native verification. It executes production result capture, isolated verification and integration rather than the sketch's enum comparison. |

All three selected E2E workflows passed: 0.46s, 13.51s and 32.25s respectively.
See `factory-corrections-evidence/factory-obsolete-contract-summary.json`.
The baseline/current function-name comparison now finds 22 disappeared names;
excluding the same two renamed controller tests leaves **20 removed/replaced**.
The old two-test target took 0.00s at the last full checkpoint. No measurable
full-suite speedup is claimed, and no full suite was rerun for these test-only
changes. The nine duplicate binary executions are a separate count.

## Barrier unit tests replaced by CLI workflows

The audit marked 20 `src/store/barriers.rs` tests REPLACE. Nineteen were deleted
after `tests/barriers.rs` covered their guarantees through the compiled CLI:
signed contracts, `result submit`/`verify` in the sandbox, `memory barrier-*`,
signed memory review/policy/reconcile, `scheduler inspect` and a signed
delegated reservation. Attempts, one task edit and one binding retirement use
the public store API because no worker launches; the delegated profile report
is the same synthetic row `tests/delegated_reservation.rs` uses.

| New E2E test | Replaced unit tests |
| --- | --- |
| `release_refuses_evidence_that_moved_after_freeze_but_ignores_unrelated_memory` | `values_the_check_cannot_store_are_invalid`, `idempotent_freeze_returns_the_stored_row_not_a_later_max`, `barrier_read_set_rejects_task_revision_changes`, `barrier_read_set_rejects_memory_applied_after_freeze`, `barrier_read_set_rejects_contract_scope_phantoms`, `barrier_rechecks_latest_contract_before_freeze_and_release`, `barrier_read_set_ignores_unconsumed_optional_noise` |
| `hard_memory_blocks_release_until_the_member_has_applied_it` | `required_set_generation_move_blocks_release` (a hard-rule change now revokes the pending barrier), `mandatory_head_without_applied_blocks_release` |
| `revocation_blocks_dependents_and_retains_capacity_until_a_later_release` | `revocation_during_release_blocks_dependents_and_does_not_release_capacity`, `revoked_barrier_blocks_reattach_and_later_evidence_until_a_later_release` |
| `memory_changes_revoke_barriers_over_the_evidence_they_invalidate` | `barrier_memory_routing_distinguishes_noise_from_changed_evidence`, `retired_worker_barrier_is_revoked_when_its_transitive_source_changes`, `memory_invalidation_revokes_pending_and_released_barriers_atomically` |
| `revoked_upstream_barrier_blocks_downstream_results_but_keeps_capacity_and_cancellations` | `downstream_barrier_revocation_blocks_new_result_submission_but_preserves_history`, `revocation_records_the_live_consumer_without_releasing_capacity`, `barrier_stop_routing_preserves_an_existing_cancellation`, `terminated_consumers_leave_live_routing_but_keep_their_requirement` |

`create_ends_at_40_and_upgrade_from_39_reaches_40` was dropped without a
replacement: its STRICT/index checks were schema text, and row preservation
across the 39→40 step is covered by
`schema9_queue_upgrade_preserves_old_exports_and_starts_with_closed_capacity`,
which upgrades through every later step and compares tasks, attempts and head.

Kept: `stale_brief_after_revocation_is_recorded_and_cannot_be_accepted`.
`record_stale_brief`/`accept_stale_brief` have no production caller or CLI
entry point, so there is nothing end-to-end to drive. The unit-only details the
deleted tests also checked (internal routing-table counts, append-only triggers,
the legacy-manifest case) remain covered by the kept fault-injection and
upgrade tests in the same module.

Mutation check, each restored byte-for-byte afterwards:
- Disabling the release-time memory manifest comparison in
  `recheck_ready_with_budget` made
  `release_refuses_evidence_that_moved_after_freeze_but_ignores_unrelated_memory`
  fail: a release after the task edit was accepted.
- Disabling the `predecessor_revoked` check in satisfaction recording made
  `revocation_blocks_dependents_and_retains_capacity_until_a_later_release`
  fail: verification under a revoked barrier was accepted.
- Disabling the revoked-requirement check in `contract_binding::result_barrier`
  made the downstream test fail only on its message; the later "not currently
  released" check still refused the submission.

## Memory regression unit tests replaced by E2E workflows

The audit marked 14 `src/memory/regression_tests.rs` tests REPLACE. Eleven
were deleted after `tests/memory_regressions.rs` covered their guarantees over a
disposable migrated project through the compiled CLI: `memory propose`,
owner-signed `memory review`, `memory promote`, signed `memory import` hard-rule
policy, `memory inspect`/`snapshot-input` and `context --session/--ack`. Running
attempts, worker snapshots, object ingestion and GC use the public store API
because no worker launches and GC has no CLI verb.

| New E2E test | Replaced unit tests |
| --- | --- |
| `reviewed_proposals_keep_their_bytes_through_gc_recheck_hard_rules_and_replay` | `reingesting_collected_bytes_must_restore_availability`, `accepted_proposal_must_protect_its_body_from_gc`, `accepted_evidence_survives_collection_before_and_after_promotion`, `promotion_replay_must_return_same_sequence`, `promote_must_recheck_hard_rule_changes_since_review` |
| `snapshots_follow_hard_rules_and_scope_and_the_profile_fallback_stays_project_wide` | `newly_hard_rule_must_not_reuse_optional_snapshot`, `unrelated_domain_and_path_must_not_match_only_because_of_kind_weight`, `profile_only_fallback_never_borrows_task_scoped_snapshot` |
| `coordinator_sessions_keep_independent_monotonic_cursors_and_see_instruction_edits` | `coordinator_snapshots_need_session_specific_subscriptions` (now observed as independent full/delta cursors, not the internal subscriber string), `delta_must_include_changed_project_instructions` (the CLI reads the edited PROJECT.md itself), `acknowledging_old_checkpoint_cannot_rewind_the_session` |

Kept: `snapshot_cache_must_not_reintroduce_expired_facts`,
`promotion_rechecks_expiry_without_an_intervening_event` and
`hard_memory_kind_is_mandatory_and_expiry_cannot_silently_remove_it`. Every
production path writes `expiry_unix_ms: None` (promotion, import, checkpoint and
snapshot revisions; candidates only carry an existing value forward), and
`hard_memory` kind cannot be promoted by a reviewer, so an expiring record
exists only through injected state and an injected clock. Driving these
end-to-end would mean raw SQL fault injection, which is what the unit tests
already do.

Two findings from the conversion: promoted `task_local` records get the
`project` scope, so pinning one into any task snapshot is refused ("belongs to
another task"); and any signed policy change after review already fails
promotion through the review fence, before the dedicated mandatory-rule check.

Mutation check, each restored byte-for-byte afterwards:
- Dropping the `last_checkpoint_id`/cursor-range guard from
  `ack_coordinator_checkpoint` made the coordinator test fail: the stale ack
  rewound the cursor and the next delta repeated `new-task`.
- Dropping the `TaskLocal` filter from `load_brief_memory` made the snapshot
  test fail: the profile-only fallback rendered `private task finding`.

## Thread unit tests replaced by CLI workflows

The audit marked 16 `src/thread.rs` tests REPLACE. Fifteen were deleted after
`tests/threads.rs` covered their guarantees through the compiled CLI, with a
shell herdr fixture serving `agent list`/`pane list` and logging other calls,
and real git, du and rsync. Thread records are seeded as TOML files, the same
form the ticker leaves on disk. The file runs in both the default and the
`state-store` build.

| New E2E test | Replaced unit tests |
| --- | --- |
| `thread_list_groups_every_record_and_live_state` (28 cases, each a thread `thread list` must group and annotate, and listing leaves every record byte-identical) | `row1_resolved_wins_over_everything`, `row2_starting_is_working_for_five_minutes`, `row3_waiting_on_you`, `row4_working_including_a_launch_in_progress`, `row5_landing_needs_open_and_approved`, `row6_ready_for_review_until_ack_or_while_pr_open`, `row7_idle_and_precedence`, `pane_gone_with_a_report_keeps_its_place`, `identity_check_before_acting_on_a_pane`, `adopted_threads_match_without_the_name`, `live_state_duration_comes_from_the_record_only_when_states_agree` |
| `report_review_ack_and_resolve_copy_home` (list → `thread ack` → report edit → `thread resolve` with a symlinked library entry) | `updates_are_atomic_and_keep_other_fields`, `copies_report_and_library_and_skips_symlinks` |
| `start_restart_and_adopt_write_briefs_branches_and_launch_line` (failed `thread start`, `thread restart`, a second start, `thread adopt`, `thread show` id refusals) | `ids_branches_and_dirs`, `brief_order_and_memory_cap` |

The E2E timing thresholds use margins (10 s and 120 s around the 30 s and 60 s
debounces, 10 s and 400 s around the 300 s start window)
rather than the exact one-second edges, because wall-clock time passes between
seeding and listing.

Kept: `imported_receipt_fingerprint_matches_legacy_execution_identity`. The lib
recomputes the binary's execution fingerprint only for a legacy finalization
receipt whose thread is already resolved while `ticker.json` still holds the
pending entry, a state reached only by a crash between those two writes. No
CLI entry point injects that crash, and the test compares the two
implementations for every thread kind.

Mutation check, each restored byte-for-byte afterwards (`cmp` against a copy):
- Making an open pull request `Landing` without approval failed the list table
  on the changes-requested and acknowledged-with-open-PR cases.
- Requiring the agent name for adopted threads failed the list table on
  "adopted agent matched without its name".
- Inlining memory past the 32,000-character cap failed the brief assertions in
  the start/restart workflow.

## Migration unit tests replaced by E2E workflows

The audit marked 13 `src/migration/tests.rs` tests REPLACE. Twelve were
deleted after `tests/migration.rs` covered their guarantees through the compiled
CLI over a CLI-created legacy project: `migration inspect/plan/apply/recover/
status`, `reconcile --record`, `runtime state/admission/rebind/create`,
`task add/list` and `operations retire`. Legacy claim records are serialized
from the public claim types; lost attempts that retain capacity are committed
with the public store API because no operator verb records them.

| New E2E test | Replaced unit tests |
| --- | --- |
| `legacy_records_with_unreconciled_effects_block_migration_until_resolved` | `pending_live_projection_refuses_migration_until_exact_stage_recovery`, `final_copy_obligations_and_invalid_counters_block_migration`, `pending_brief_claims_refuse_migration_but_confirmed_history_is_validated`, `pending_or_unresolved_launch_claim_requires_reconciliation_before_import`, `coordinator_prime_import_requires_confirmed_delivery_and_valid_counters`, `coordinator_start_import_requires_confirmed_delivery_and_retains_uncertain_history`, `notification_claims_and_suppression_require_reconciliation_before_import` |
| `migrated_project_resumes_only_with_matching_evidence_and_pause_fences_the_epoch` | most of `controller_resume_requires_fresh_evidence_and_pause_fences_epoch` (see below) |
| `archived_projects_and_retained_attempts_refuse_resume_rebind_and_binding_creation` | `controller_preserves_archived_state_and_retained_attempts_block_admission`, `runtime_rebind_refuses_unselected_lost_attempt_that_retains_capacity`, `canonical_runtime_creation_refuses_unselected_retained_attempts` |
| `canonical_bindings_created_after_resume_keep_imported_provenance_and_repause` | `canonical_runtime_creation_preserves_provenance_and_fences_task_and_control` |
| `retiring_an_imported_ambiguous_finalization_keeps_the_effect_possible` | `retiring_an_ambiguous_intent_does_not_claim_absence_or_release_resources` |

The blocker table asserts exactly which record paths `migration inspect`
refuses, since per-record causes are reported only as "invalid or unsupported
record"; each refused fixture differs from an accepted sibling by one field.
Evidence recorded under one config no longer authorizes resume after the config
changes, which the unit test expressed as a digest argument.

Kept, trimmed to `controller_resume_rejects_stale_evidence`: `reconcile
--record` stamps observations with the wall clock, so the 30 s freshness window
is only reachable with an injected time.

Mutation check, each restored byte-for-byte afterwards:
- Dropping `claim.generation<=request` from the coordinator start rule made the
  blocker-table test fail (a generation-2 start was accepted).
- Dropping the observation `config_digest` match from resume blockers made the
  resume test fail (resume succeeded after the config changed).
- Dropping the retained-attempt check from `create_runtime` made the archived
  test fail (a binding was created for a task holding a lost attempt).
