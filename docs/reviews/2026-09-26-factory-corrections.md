# Factory review corrections — work in progress

Baseline: merged `main` at `9e628fe`, whose tree matches the reviewed `bd6433d`.
Branch: `fix/factory-review-main`. The original review and reproducer evidence
remain in this directory. This document tracks repairs; it is not a factory
certification or a declaration that the plan is complete.

Latest test cleanup removed the two tests in `tests/contracts.rs`: both exercised
the unused Phase B illustrative model. Three production CLI workflows passed as
replacement evidence for untrusted results, stale evidence, lost capacity and
ambiguous response recovery. The status workflow now explicitly uses a lost
attempt without termination proof. Total removed/replaced pre-existing functions
is 20, plus nine duplicate executions removed earlier. See
`factory-obsolete-contract-summary.json`; no full-suite speedup is claimed.

The signed CLI E2E reproduced acceptance after a check changed the disposable
checkout's index, HEAD or tracked contents. All three runs minted receipts before
correction; a separate exit-128 check was mislabeled setup failure with no stored
status (`factory-verifier-outcome-before.log`). The native supervisor now checks
HEAD/tree and both index and worktree against the pin after execution and child
cleanup. Initial validation also checks the index explicitly. Ignored generated
artifacts remain allowed, verified by a successful Git archive case.

The supervisor distinguishes check failures from its own reserved exit codes;
the parent retains the reported actual check exit status instead of replacing it
with 1. A signalled check has no exit code, rather than a fabricated setup code.
The E2E checks rejected-run receipt absence, unchanged capacity, scratch cleanup,
replay identity/status and the successful ignored-artifact case. The final CLI
passes in 6.03 seconds; all 30 distinct selected regression cases passed (35 executions including
five repeated store cases), across verifier, signed CLI/reservation and factory
workflows. See `factory-verifier-outcome-validation-summary.json`. The preceding full checkpoint predates this fix.
Historical accepted receipts do not attest these new post-execution checks.
The follow-up correction requires a version-2 native check record on receipt
reuse, including unscoped contracts. The signed CLI reproduction accepted old
version-1 evidence before correction and passes after correction (15.95 seconds).
Exact replay retains old evidence; only a fresh verifier run writes version 2.
Migration does not backfill proof. Signal-number persistence and full hostile
code certification are not established by these changes.

The proof-version follow-up passed 158 distinct selected cases across signed CLI,
satisfaction, integration, barriers, verifier, admission, memory proposals,
controller, factory harness and historical CLI upgrade. One existing historical
barrier test needed its obsolete reuse expectation corrected; its exact rerun
passed after the other 59 barrier cases had passed. The initial fixture compile
error was also corrected. Failures and reruns remain recorded in
`factory-verifier-proof-v2-validation-summary.json`. No new test function was
added. The full-suite checkpoint below predates both verifier corrections.

The latest full serial state-store suite passed on the recorded current inputs:
**1,408 passed, zero failed, 22 ignored**, across nine targets. All 366 input hashes
matched after completion. Wall time including compilation was **24m49s**
(1,488.72 seconds); summed test execution was 1,467.49 seconds. Compared with the
previous 24m44s checkpoint this establishes no meaningful speedup. The nine fewer
passing executions correspond to the removed binary duplicates; their library
coverage remains. This checkpoint includes the scoped scheduler/profile/budget
changes and extended E2Es above. Logs, hashes, per-target counts and limitations
are under `factory-current-full-*`. It does not establish the missing automatic
producer chain, live gates, or completion of the full review scope.

Budget authorization now validates the pinned current policy before checking
its thresholds. It skips the attempt table when no attempt limit applies and
counts at most the configured threshold (plus one for an already-reserved
attempt), preserving pre-reservation count >= cap and post-reservation count >
cap semantics. Scheduler hints share the same threshold helper. Administrative
budget reports still return the exact lifetime count, including cancellations.
Worker-brief validation now propagates its original read budget into policy
validation instead of dropping input accounting at that boundary. The existing
signed reservation CLI workflow also checks exact budget totals after cancellation
and after the next reservation. All 85 affected cases passed across the signed reservation CLI, budget,
reservation, admission and canonical-worker brief checks. One supplementary
legacy briefing CLI case also passed. The empty store worker-brief test filter
is excluded from counts. Exact provenance is in
`factory-budget-decision-validation-summary.json`; no measured runtime
improvement is claimed.

Capability membership no longer computes DISTINCT levels across a profile's
entire observation history. Five indexed EXISTS queries seek unexpired rows and
stop at the first currently valid observation for each schema-defined level.
Both observed-at and expiry checks remain in force and use the shared input/SQL
budget. The planning/admission workflow now requires discovered and launchable
capabilities and measures the actual observed admission connection before and
after retaining 30,000 expired/overlapping observations. Before correction its
SQLite work grows from 4,636 to 3,454,636 VM steps; after correction both sizes
use 5,066 steps and the nine permitted reservations still succeed. This evidence
covers expired/overlapping observations, not arbitrary future-dated interval
histories. The fixture's initial insert loop was stopped because its own template
selection scanned growing history; direct-key fixture insertion completed the
reproduction in 3.50 seconds. All 33 affected cases passed: the extended planning workflow (4.48 seconds),
eight capability regressions, 23 admission regressions and the historical-upgrade
CLI. See `factory-capability-window-validation-summary.json` for timings and
measurement limits.

Admission profile inventory now selects the current store identity and the
configuration digest pinned by the admission header before decoding reports or
applying the 256-candidate limit. A migration-43 expression index preserves the
existing digest order, so eligible historical profiles and exact launch grants
remain usable. Hash, identity, configuration and launchability checks still run
on selected reports. The planning/admission workflow reproduces the original
limit failure with 300 reports from old configurations or a different inode;
after correction it admits nine non-conflicting workers and leaves one overlap
unreserved. The bound still applies to profiles sharing the current identity and
configuration; lifecycle management of that inventory remains a separate issue.
All 33 distinct affected cases passed (34 executions including the final
planning rerun): 23 admission regressions, nine factory workflows and the
historical upgrade CLI. The final planning run takes 1.61 seconds and also
asserts all 301 reports remain stored. Exact provenance, including an intermediate
fixture signature compile repair, is recorded in
`factory-admission-profile-inventory-validation-summary.json`.

Native profile selection for scheduler capability hints now uses an expression
index over the producer-serialized store identity and adapter kind, ordered by
retention sequence. It loads only the latest matching report, then checks its
payload hash, exact store identity, frozen-profile reference and kind. A newer
invalid matching report fails closed; unrelated adapter/store reports no longer
block inspection. The signed CLI E2E reproduces the prior unrelated-report hash
failure and checks both outcomes after correction. The index is part of the
unpublished local migration 43; immutable historical reports remain unchanged.
This closes the profile-history scan in scheduler selection, not the separate
administrative evidence publisher or admission profile inventory. All 19 selected
cases passed: signed CLI E2E (5.05 seconds), eight capability regressions, historical
upgrade and nine factory workflows. Exact provenance is in
`factory-profile-selection-validation-summary.json`.

Scheduler inspection now uses the scoped publication-checked opener and one
shared two-second SQL/input budget. It reads queued tasks and their predecessors,
retained-capacity aggregates, capped attempt counts, and task-specific unused
launch grants instead of decoding every historical task, attempt and approval.
An expression index supports task/class grant selection. The report retains
existing unused-grant hint semantics; it is not a launch authorization check.
The scheduler CLI reproducer failed on unrelated invalid UTF-8 metadata before
the fix and passes afterward without modifying event history. Linux integration
capability now reports `operator_local`, matching the available operator commands;
automatic dependency producers remain unavailable. Native profile selection still
scans profile history under the shared budget, and queue graph work remains
proportional to queued records. Universal history-independent latency is unproven.
All 41 selected cases passed across scheduler, dependency, capability, budget,
targeted-reader, CLI and factory workflows. The reproduced CLI passes in 0.32
seconds. Exact counts and limits are recorded in
`factory-queue-report-validation-summary.json`; no full-suite rerun was made.

Operator integration ingress now exposes target configuration, integration and
reconciliation under `result`. The prior integrator had only test callers.
Entry points hold the project mutation guard, require an already-upgraded store,
create only new private scratch directories, preserve existing caller files,
and serialize outcomes with a nonzero exit for non-integrated results.

The extended signed SHA-256 CLI workflow reproduced another object-format bug:
both integrator scratch paths used default SHA-1 initialization. It incorrectly
reported a conflict between compatible candidate/target histories. Both initial
merge and retained-candidate recovery now preserve the repository object format.
The CLI E2E verifies a combined tree with independent target changes, refuses
historical receipts lacking scope proof, retains attempt capacity, and exercises
real fault recovery before check-state persistence and after Git CAS but before
receipt persistence. Reconciliation confirms the same commit; replay leaves one
integration receipt. `factory-integrator-cli-before.log` preserves reproduction;
`factory-integrator-cli-recovery-final.log` passes in 2.01 seconds. Affected validation passed 43 cases across the CLI workflow, existing integration
and verifier regressions, and all nine factory harness workflows. See
`factory-integrator-validation-summary.json`. Automatic dispatch and live
F1.7 certification remain open.
Final merged-tree required-output enforcement is now implemented. The CLI E2E
reproduced a clean target-side deletion that the signed policy accepted and the
integrator published (`factory-merged-outputs-before.log`). Integration now loads
required outputs from the exact immutable signed contract and inspects the
retained candidate commit with one bounded literal-path `git ls-tree` call.
Only regular-file modes satisfy an output; symlinks, directories and absent
paths do not. The guard runs before publication and before confirming a pending
observed publication, including historical candidates marked checks-passed.
Failure before publication records `required_output_missing` feedback without
moving the ref or issuing a receipt; an already-published incompatible candidate
requires reconciliation. Terminal historical outcomes remain historical reads.
The extended E2E preserves attempt capacity and verifies stable rejection replay
without duplicate feedback. All 43 selected affected cases passed; the final
CLI E2E also includes a regular required file named `push`, handled as a literal
path rather than a transport command. Its final runtime was 2.54 seconds. See
`factory-merged-outputs-validation-summary.json` for exact run provenance and
limits. No full-suite rerun was made.

Historical integrated-receipt reuse now requires explicit merged-output proof
when the signed contract declares required outputs. Native confirmation writes
that proof atomically with the integration receipt in an additional unpublished
migration-43 table. Migration and terminal replay never backfill it. Dependency
creation/backfill, current dependency checks and barrier evidence checks all
require this proof; contracts without required outputs retain compatibility.
The E2E covers already-queued and newly-queued consumers, stable historical replay,
and fresh integration restoring usable evidence while retaining the old receipt.
Bypassing the shared proof guard makes it fail at the historical-integration
assertion (`factory-integrated-proof-mutation.log`); source was restored exactly.
All 93 selected cases passed: the extended CLI (2.83 seconds), dependency and
integration regressions, 60 barrier cases, historical-upgrade CLI and all nine
factory workflows. Exact counts and timings are in
`factory-integrated-proof-validation-summary.json`. No full-suite rerun or live
certification is claimed.

Receipt attachment now skips unusable newer evidence instead of allowing it to
hide an older current receipt. The signed CLI E2E reproduces the old failure by
adding a newer accepted run without proof, then queueing a consumer that must
use the earlier proven run (`factory-receipt-selection-before.log`).

Verified selection now filters the selected attempt/latest contract in SQL and
streams candidates rather than collecting all predecessor receipts. New indexes
support that filtering and latest-attempt lookup. Verified and integrated
selection share one two-second SQL/input deadline across all attachment edges;
there are at most 256 edges, and input accounting precedes candidate decoding.
The budget propagates through authority checks and insertion. Queue/contract
attachment updates only the selected consumer; native receipt publication owns
the separate fan-out. The E2E also injects expensive SQLite work during evidence
insertion and checks deadline failure, whole-queue rollback and successful retry.
All 116 selected cases passed across the extended CLI, dependency, scheduler,
reservation, verification, integration, historical-upgrade and factory checks.
The CLI including the intentional two-second stall passed in 5.10 seconds.
`factory-receipt-selection-validation-summary.json` records exact counts and
limits. No full-suite timing comparison was made.

Queue mutation now reads the selected task, an indexed retained-attempt existence
check, and the bounded graph's identities/edges. It no longer decodes all task
metadata or retired attempts. The complete queue graph still receives missing,
duplicate, self-edge and cycle validation with the existing bounds of 10,000 queued records,
100,000 edges, and 256 dependencies per task. Policy updates read only the policy row.
The runtime queue/policy commands now use the existing scoped publication-checked
opener and propagate its original two-second SQL/input budget through graph
validation and receipt attachment without replacing or renewing the handler.
Raw store queue callers retain an equivalent local transaction deadline.

The scheduler CLI E2E reproduced unrelated cold task metadata blocking a queue
operation (`factory-queue-inventory-before-decode.log`). Its refined fixture uses
invalid UTF-8 in fields that pass SQLite structural constraints, rather than
violating a CHECK constraint. After the fix, queue/policy commands succeed, the
cold task/terminated attempt bytes remain untouched, and a real cycle still
fails without changing the event head. The separate receipt E2E continues to
exercise deadline interruption/rollback. All 84 selected cases passed across both CLI workflows, scheduler, dependency,
reservation, historical-upgrade and factory checks. The queue CLI passed in 0.30
seconds; the receipt/deadline CLI passed in 4.94 seconds. Exact evidence is in
`factory-queue-inventory-validation-summary.json`. No full-suite rerun was made.

Remaining performance scope: queue graph work is bounded but proportional to
queued vertices/edges. The read-only queue report still reads broad inventories,
and integrated receipt selection can walk relevant history until an eligible
receipt or budget limit. Indexed selection/work measurements and a complete
foreground inventory audit are still required.

Changed-file scope enforcement is now implemented for contracts with explicit
path declarations. The verifier compares retained base/candidate trees, authorizes
literal write paths and directory prefixes, and checks both sides of renames.
Read declarations/globs do not widen scope; absent legacy declarations retain
legacy behavior. A missing/over-limit diff fails closed without a receipt.

Proven violations atomically publish a rejected run, local feedback and a stop
request through the existing cancellation logic. Capacity remains held unless
that logic proves launch never started. Pending feedback routes through the
existing replan service. The signed CLI E2E now uses actual SHA-256 Git objects,
accepts an in-scope output while preserving an unchanged outside file, rejects
an outside-to-inside rename, injects a cancellation-write failure to prove full
rollback, and checks replay, capacity retention and explicit replan routing.
Removing scope enforcement made the E2E fail; source was restored byte-for-byte.
Affected validation passed: 76 distinct cases across the two CLI workflows,
verifier and cancellation regressions, factory harness and explicitly enabled
cross-project controller E2E. The latter uses local synthetic worker binaries;
it passed in 7.24 seconds. Default build and `git diff --check` passed. Logs use
`factory-scope-*`, with selected cases in `factory-scope-validation-summary.json`.
The guard-removal proof is `factory-scope-verifier-mutation.log`. No full-suite
rerun was performed for this batch. This is post-execution evidence enforcement,
not a worker filesystem sandbox or complete planner decomposition.
Historical receipt reuse now requires separate contract-check evidence for
explicit scope/output contracts. Fresh native acceptance records it atomically
with the receipt in the unpublished migration-43 table; migration and exact
replay never backfill it. The shared dependency/integration/barrier authority
check refuses historical scoped receipts without this proof. Legacy contracts
without scope/output declarations retain compatibility. A new verification key
runs current checks and can restore usable evidence without rewriting history.
The signed CLI E2E proves dependency satisfaction, historical-proof absence,
unchanged replay semantics and fresh re-verification. Bypassing the shared guard
made it fail at the intended historical-receipt assertion; source was restored.
Affected validation covers 112 passing cases across selected CLI, verifier,
dependency, integration, barrier, upgrade and factory workflows. The factory
fault campaign initially failed because it fabricated accepted scoped receipts;
that fixture now uses actual retained Git objects and two real verifier runs.
Its stale-attempt evidence assertions pass. Eight other harness cases passed
unchanged; the corrected campaign passed separately in 0.90 seconds. Evidence
and run limitations are in `factory-scope-provenance-validation-summary.json`.
No full-suite timing or live certification is claimed.

Operator verifier ingress is now implemented as `result SLUG verify`. Code
inspection found that the service previously had callers only in tests. The new
Linux/state-store command uses the installed signed policy and retained Git
objects, returns structured outcomes, exits nonzero on rejection, supports exact
replay and creates only a fresh private scratch directory. Policy reads are now
bounded at 4,000 bytes and refuse symlinks/nonregular files before allocation.
The existing signed-reservation E2E now exercises actual CLI verification, owner
policy mismatch, replay, scratch preservation/cleanup and policy input refusal.
[Usage and remaining limits](../factory/verified-results.md). The subsequent scope check above extends this ingress; automatic verifier
dispatch remains open.
Final affected validation passed: the signed reservation/result/verifier CLI
workflow in 1.03 seconds and 19 existing verifier cases in 12.27 seconds (20
distinct affected cases). Default `cargo check --locked` and `git diff --check`
passed. Logs: `factory-verifier-cli-final.log`, `factory-verifier-cli-regression.log`
and `factory-verifier-cli-default-check.log`. The scratch cleanup adjustment was
rechecked through the CLI after the verifier regression run. No full-suite rerun
was performed for this ingress addition.

Full post-cleanup regression completed successfully: **1,417 passed, zero failed,
22 ignored**, across nine targets. All 359 recorded source/test hashes matched.
The embedded one-test subprocess result is not double-counted. Wall time including
compilation was 1,483.53 seconds (24m 44s); summed target execution was 1,456.65
seconds (24m 17s), versus 1,460.39 seconds previously: 3.74 seconds / 0.26% faster.
This single-run difference does not demonstrate a meaningful or isolated cleanup
speedup. Production changes and strengthened coverage were also included.
Evidence: `factory-post-cleanup-full-regression.log`, `-completion.json`,
`-source.json` and `-summary.json` under `factory-corrections-evidence/`.
This checkpoint precedes the pending reserved-contract and shared-module patches.

The subsequent reserved-contract correction is now implemented. A real owner /
subject signed reservation E2E reproduced ordinary results switching from their
frozen contract revision 1 to newly installed revision 2. The production fix
validates immutable attempt inputs before the non-barrier return, so ingestion,
verification and later evidence use enforce the frozen reference. The same E2E
now rejects the mismatched revision, accepts/replays the original revision after
supersession, and preserves frozen inputs, task state and retained capacity.
Manual attempts without a frozen reference and existing barrier requirements
remain supported. Before/after logs: `factory-reserved-contract-before.log` and
`factory-reserved-contract-final.log` (final E2E 0.92 seconds).

The Linux state-store binary now reuses the public library source-tree module,
removing nine duplicated test executions while retaining their library coverage.
Default/non-Linux builds retain the local module. These two changes are after the
full-suite checkpoint above. Affected validation passed: three CLI cases, nine
factory harness cases, existing contract/verifier/result-ingestion coverage and
nine retained source-tree tests. There were 63 distinct test names across
these runs (overlapping filtered runs deduplicated). Default `cargo check --locked`
passed. The binary test inventory is now 532 with the nine duplicate names absent;
the library retains and passes them. Logs use `factory-reserved-contract-*` and
`factory-shared-source-tree-*`, with `factory-reserved-contract-validation-summary.json`
recording the selected cases. No second full suite or new timing claim follows
from these two post-checkpoint changes.

Signed contract installation now rejects dependency cycles in the same transaction
as publication. It traverses only reachable latest contracts, falling back to
queue prerequisites for uncontracted tasks, with explicit task/edge/byte/time
bounds and iterative cycle detection. The actual owner-signed CLI workflow passed
in 1.20 seconds: direct, transitive and queue-mediated cycles are refused without
canonical state changes; replacing the blocking predecessor contract then allows
the now-acyclic revision. Exact retries keep their prior semantics. This closes
the direct-installation cycle gap; it does not complete F2 decomposition.
The guard-removal mutation was caught by the E2E test and source was restored
byte-for-byte. Final validation: 20 existing contract cases passed in 2.83 seconds
and the restored CLI workflow passed in 1.30 seconds (21 distinct passes).
Logs: `factory-contract-cycle-regression.log`, `factory-contract-cycle-cli-restored.log`
and `factory-contract-cycle-mutation.log`. `cargo check --locked --features
state-store` and `git diff --check` passed. No full-suite rerun was performed.

Required-output enforcement now has a task-contract v3 format. It requires 1–64
literal Git file declarations inside write scope. Result manifests must list the
outputs; verification independently checks retained candidate files, rejecting
missing files, directories and symlink substitutions without minting receipts or
releasing capacity. Existing v1/v2 signatures retain their format semantics.
This is an F2.2 component, not completion of planner-driven decomposition.
[Format and limits](../../contracts/factory/task-contract-outputs-v3.md).

The real signed-contract CLI refuses malformed declarations and incomplete result
manifests, then accepts and replays a valid submission (0.93 seconds). A real Git /
verifier workflow covers a valid file, missing file, directory, final symlink and
symlink ancestor (0.62 seconds). Logs: `factory-required-outputs-cli.log` and
`factory-required-outputs-verifier.log`. Existing verifier regression coverage also passed: 19 cases in 12.13 seconds
(`factory-required-outputs-regression.log`). Seven existing result-ingestion
cases passed in 0.91 seconds and four signed-vector compatibility cases passed
in under 0.01 seconds. Including the CLI, 31 distinct affected tests passed;
the separate one-case output workflow is already included in the 19 verifier
cases. No full-suite speedup is claimed.

Latest test cleanup removes documentation phrase checks and migration SQL-wording
assertions while retaining their behavioral workflow coverage. The public profile
probe now also proves configuration bytes survive successful and failed probes.
All 18 affected cases passed: one CLI, eight capability and nine factory harness
cases. See [test audit](2026-09-27-test-effectiveness.md) for replacement mappings
and timings. No full-suite rerun or live-provider certification is claimed.

Latest boundary-test cleanup replaces the PR source scan with a real subprocess/
executor/store integration test: merged and approved PR observations, including
fabricated evidence fields, leave canonical dependencies blocked and attempt
capacity held across repeated reads and an executor restart. A real CLI test
replaces a self-checking forged-JSON fixture: worker-supplied trusted-receipt fields
are refused, and ordinary result submission still mints no verification evidence.
The receipt type's compile-fail guard remains. A real accepted verifier run and
receipt now survive an explicit schema-35 upgrade with every stored field unchanged,
replacing migration SQL-wording assertions.

**Ten distinct affected tests passed**, excluding compilation: two poller cases
(0.32 seconds), one forged-receipt CLI (0.36 seconds), five feedback cases (0.49
seconds), one compile-fail receipt test (0.03 seconds), and one accepted-receipt
upgrade workflow (0.25 seconds). `git diff --check` is clean. These checks do not
certify live GitHub, implement canonical PR-driven evidence, or complete the wider
audit/implementation work. All corrections remain local and uncommitted.
[Poller boundary](factory-corrections-evidence/factory-pr-evidence-boundary-final.log),
[forged-receipt CLI](factory-corrections-evidence/factory-worker-forged-receipt-cli.log),
[feedback](factory-corrections-evidence/factory-feedback-behavior-cleanup.log),
[receipt type](factory-corrections-evidence/factory-verification-receipt-type.log),
[accepted receipt upgrade](factory-corrections-evidence/factory-verified-receipt-upgrade.log).

Latest test cleanup removes nine duplicated migration source-text checks and
replaces their implicit-upgrade coverage with a real CLI matrix over historical
schema prefixes 32–42. Reads preserve the old schema; a held execution guard
refuses upgrade; explicit and repeated upgrades preserve task/attempt/control
state, event history and exact proposal bytes, and backfill current plan intent.
A deliberately inserted upgrade during ordinary open was caught; production
source was restored and the E2E matrix passed again.

Verifier isolation now checks the actual parent mount namespace/table/root across
real runs instead of searching source for mount function names. Existing syscall
filter coverage remains. Signed memory promotion explicitly retains a legacy
subscription while proving new routing follows current bindings. The source scan
for admission writers was replaced by the existing signed-admission CLI assertions.
**33 distinct affected tests passed**: 11 migration cases (2.89 seconds), 19
verification cases (11.37 seconds), and three E2E workflows—historical upgrades
(3.51 seconds), memory retirement routing (0.36 seconds), and signed admission
(0.64 seconds). Compilation is excluded. `git diff --check` is clean. No full-suite
or live certification claim is made; remaining test and implementation audits
are open.
[Migration regressions](factory-corrections-evidence/factory-migration-behavior-cleanup.log),
[upgrade E2E](factory-corrections-evidence/factory-upgrade-cli-restored.log),
[detected implicit upgrade](factory-corrections-evidence/factory-upgrade-cli-mutation.log),
[verifier isolation](factory-corrections-evidence/factory-verifier-parent-state.log),
[memory routing](factory-corrections-evidence/factory-memory-routing-behavior.log),
[signed admission](factory-corrections-evidence/factory-admission-cli-behavior.log).

The controller test audit exposed another full-history read in production:
background canonical maintenance decoded a complete snapshot solely to inspect
its schema version. An unrelated malformed retired task therefore prevented fresh
observations. The real ticker CLI regression reproduced the stall before the fix
and passed afterward. Background maintenance now uses the scoped schema reader
under the existing job deadline, dropping the connection before planning/probes.
The synchronous controller regression also expires an ambiguous notification claim
with malformed cold history present, without replaying the effect.

The source-parsing `targeted_hot_path_is_the_production_default` test was removed;
it inspected only the synchronous implementation and missed the background bug.
The initial SQL deadline fixture now stalls the schema header rather than an
obsolete full-history read. **37 affected tests passed**: 32 controller cases
(46.89 seconds), four targeted-reader cases (2.13 seconds), and the ticker CLI
(2.57 seconds), excluding compilation. `git diff --check` is clean. This is a
scoped correctness/performance repair, not completion of the universal foreground
or test-effectiveness audits.
[Before-fix E2E](factory-corrections-evidence/factory-background-history-before.log),
[repaired E2E](factory-corrections-evidence/factory-background-history-after.log),
[controller regressions](factory-corrections-evidence/factory-background-history-controller.log),
[targeted readers](factory-corrections-evidence/factory-background-history-targeted.log).

Latest test-effectiveness cleanup replaces status source-string/report tests with
real CLI coverage and removes duplicated probe harnesses that generated their
own capability manifests. The CLI now checks both explicit adapter executables,
missing executables, failed version processes, redaction and absence of authority.
Status checks include malformed cold history, retained capacity, unknown-schema
refusal and unchanged canonical state. A deliberately reintroduced full snapshot
read failed the status E2E test; production source was restored and the test passed
again. The watchdog lock test now uses the production store/timeout instead of a
test-only busy handler. Document-wording assertions were removed from the factory
harness while its runtime memory, scale and fault scenarios remain intact.

**25 distinct affected tests passed**: nine factory harness (44.70 seconds), five
watchdog (0.89 seconds), one historical-schema status (0.92 seconds), eight probe
boundary tests (0.12 seconds), and two CLI workflows (status 0.61 seconds, probe
0.10 seconds). Times exclude compilation. No full-suite rerun or live adapter
certification is claimed. `git diff --check` is clean; all work remains local and
uncommitted. The broader effectiveness audit and outstanding factory corrections
remain open. Details and replacement rationale are in the
[test audit](2026-09-27-test-effectiveness.md).
[Status CLI](factory-corrections-evidence/factory-status-e2e-restored.log),
[probe CLI](factory-corrections-evidence/factory-probe-e2e-cleanup.log),
[probe boundaries](factory-corrections-evidence/factory-probe-focused-cleanup.log),
[watchdog](factory-corrections-evidence/factory-watchdog-behavior.log),
[historical schemas](factory-corrections-evidence/factory-status-history.log),
[factory harness](factory-corrections-evidence/factory-harness-behavior-cleanup.log).

Current changes after that full-suite checkpoint: version-3 unsigned plan
proposals now connect typed create/supersede/add-dependency/cancellation requests
to parsing, store/session identity, exact prior-intent references, atomic request
publication and inspection. Cancellation remains reviewed intent and preserves
running attempts/capacity. Migration 43 is still unpublished local work; no real
stores were upgraded. Complete executable contracts/decomposition and automatic
inference/dispatch remain open.

The user requested replacement of ineffective tests with E2E coverage. The first
[audit batch](2026-09-27-test-effectiveness.md) removes seven source-string or
overlapping planning tests and moves their observable guarantees into the real
CLI workflow, including all four typed changes and identity/replay refusals.
A deliberately introduced changed-payload replay bug was caught by that test;
production code was restored byte-for-byte. The final CLI flow passed in **1.74
seconds** excluding compilation. **54 focused planning tests passed** in 13.00
seconds, including request-publication rollback and retained running capacity.
The initial rollback fixture failed because it reused a session with a newer
cursor; the corrected fixture reuses the original input. This is **55 affected
passes**, not another full-suite run. The broader test-effectiveness audit remains
open.
[CLI log](factory-corrections-evidence/factory-typed-plan-cli-final.log),
[focused log](factory-corrections-evidence/factory-typed-plan-focused-final.log),
[detected mutation](factory-corrections-evidence/factory-plan-e2e-mutation.log).

Earlier full regression checkpoint, after the accepted-intent, inspection and
barrier ingress/deadline/expiry corrections:
`cargo test --locked --features state-store --no-fail-fast -- --test-threads=1`
completed successfully in an isolated user/PID namespace with **1,431 passed,
zero failed, 22 ignored**. Counts are 796 library, 542 binary, 58 CLI, two contract,
one delegated-reservation, ten factory harness, 13 memory-control and nine
documentation passes. Thirteen library and nine live Phase A tests were ignored.
The embedded one-test child process is not counted again. Library tests took
505.54 seconds, binary 205.35 seconds and CLI 703.10 seconds. No compilation
or source edits overlapped process tests.
[Full regression log](factory-corrections-evidence/factory-post-barrier-full-regression.log).

The [source checksum manifest](factory-corrections-evidence/factory-post-barrier-source.sha256)
records source, migrations, tests, contract artifacts and Cargo/build inputs from
the uncommitted worktree. `sha256sum --check --quiet` verified every entry after
the run, and `git diff --check` was clean. The prior affected run's default-feature
build also passed on this source. This is a local regression checkpoint, not live
adapter certification; no live provider or real-store upgrade was performed.

At that checkpoint the F2 audit identified typed changes as missing. Those are
now connected as described above, but production proposals still carry intended
text/dependencies rather than complete executable contracts. Full contract fields,
decomposition validation, bounded planner inference and signed dispatch remain
implementation work. The wider foreground, memory orchestration and F0–F5
acceptance gaps remain open.

Earlier full regression checkpoint, after the planner-session and bounded-graph
changes, **before the accepted-intent projection and inspection below**:
`cargo test --locked --features state-store --no-fail-fast -- --test-threads=1`
completed with **1,413 passed, one failed, 22 ignored** in an isolated user/PID
namespace. Counts are 779 library passes plus one failure, 542 binary, 57 CLI,
two contract, one delegated-reservation, ten factory harness, 13 memory-control
and nine documentation passes. The embedded one-test subprocess is not counted
again. The failure was a fixture deletion blocked by the new pending-feedback
foreign key, before reaching verification repair. No other target failed.
[Full checkpoint log](factory-corrections-evidence/factory-planner-full-regression.log).

That fixture now removes the derived pending row when modeling missing historical
feedback, keeps foreign-key enforcement enabled, and asserts exactly one pending
row after repair and repeated polling. A subsequent **73-test affected run passed**:
five verification, six feedback, 52 planning/wait and ten rebuilt factory harness
tests. The default build check passed with existing warnings; `git diff --check`
was clean. The full run's failure remains recorded; this follow-up is not a new
single-invocation full-suite pass.
[Repair validation](factory-corrections-evidence/factory-planner-full-repair.log).
No compilation overlapped process tests. No live provider was run.

An earlier entirely passing full local run, after the retained-launch-resource projection and
preceding corrections, **before the identity-gap, worktree-binding, wait-renewal, typed-trigger, replan-service and planner-session changes below**:
`cargo test --locked --features state-store --no-fail-fast -- --test-threads=1`
exited successfully in a user/PID namespace, followed by `cargo check --locked`.
**1,382 tests passed**: 752 library, 541 binary, 54 CLI, two contract, one
delegated-reservation integration, ten factory harness, 13 memory-control and
nine documentation tests. Thirteen library and nine live Phase A tests remained
ignored (**22 ignored**). The embedded one-test subprocess result is not counted
again. No compilation overlapped process tests. The default build check passed
with existing warnings, and `git diff --check` was clean.
[Full retained log](factory-corrections-evidence/factory-retained-pane-all-targets.log).

The earlier 1,358-test full run, before the prompt-rendering and subsequent
foreground refactors, remains archived as historical evidence
([earlier log](factory-corrections-evidence/all-targets-after-brief-approval.log)).
Neither run completes the unperformed certification gates or remaining production
workflow corrections. Cross-project worktree inventories, legacy completeness and
dangling-reference checks, and complete dependency-graph/fleet cost measurements
remain in the foreground audit. Local query measurements do not establish fleet
latency or live adapter certification.

## Correctness repairs implemented

A second publication-time regression reproduced a signed release committing after
consumed memory expired, even though the authorization itself remained valid.
[Before-fix failure](factory-corrections-evidence/factory-barrier-memory-expiry-before.log).
The fresh memory read-set collector now derives the earliest expiry of consumed
revisions/transitive sources and the global mandatory set used by readiness.
Readiness carries that deadline through prerequisite releases, so signed drafting
and publication check the complete supporting evidence's expiry at their final
boundary. It reuses selected rows and adds no SQL history scan, stored schema or
canonical payload field; existing fixed-vector digests remain unchanged.

Regressions cover directly consumed memory, a transitive source, and memory
consumed only by an ancestor wave. Unconsumed optional observations/contracts
remain allowed after expiry. Refused publication preserves the previous event
head, immutable ancestor release and attempt capacity. The ancestor fixture uses
a real retained child snapshot whose required framing consumes its budget, proving
the child does not itself consume the expiring optional record. An initial fixture
attempt correctly failed the existing missing-knowledge admission guard before
reaching the race; the corrected fixture passes without weakening that guard.
[Focused validation](factory-corrections-evidence/factory-barrier-memory-expiry-focused-repaired.log).

The final affected run passed **197 tests**: 174 selected library tests covering
barriers, reservations, dependency satisfaction, verification, memory readiness,
controlled budgets and authority; ten factory harness and 13 memory-control
integration tests. The default build passed with warnings and `git diff --check`
was clean. The focused tests overlap this run; no compilation overlapped process
tests. [Validation log](factory-corrections-evidence/factory-barrier-memory-expiry-final.log).
This repairs signed barrier publication and drafting, not every other time-sensitive
operation or the outstanding automated/live acceptance gates.

A release-expiry regression reproduced acceptance after the signed authorization
expired during receipt insertion. Reading the clock after acquiring the write
lock did not cover the rest of the transaction. The probe pauses the actual
receipt insertion until its real expiry passes, without changing the host clock
or forging retained evidence.
[Before-fix failure](factory-corrections-evidence/factory-barrier-publication-expiry-before.log).

New signed releases now check the authorization interval again immediately before
commit, after readiness, event/header publication and receipt insertion. Expiry
rolls back the complete transaction; attempt capacity remains unchanged. Drafts
also refuse when their interval has elapsed during preparation. Exact retries of
an already committed receipt remain historical reads, so later expiry does not
erase or renew that release. The regression verifies both sides of this boundary.

The final affected run passed **80 tests**: 58 barrier, 12 authority and ten factory
harness. The default build passed with warnings, and `git diff --check` was clean.
[Validation log](factory-corrections-evidence/factory-barrier-publication-expiry-final.log).
No compilation overlapped process tests. This repairs the signed authorization
publication boundary; it is not a full-suite or live acceptance result.

Barrier freeze and named inspection now use scoped controlled-store entry points
under a shared two-second request deadline. Freeze accounts for external JSON
bytes/structure before deserialization, then shares that budget with canonical
members, selected evidence, proposal dispositions, the memory read set and final
publication. Sorting member references avoids cloning an unchecked input tree;
duplicate dispositions are detected after sorting rather than repeated linear
searches. Existing membership/disposition limits and immutable identity/replay
semantics remain in force. Raw trusted store entry points remain available.

Two new regressions cover dense input refusal before decoding, withheld malformed
input details, an unrelated malformed task that makes the administrative snapshot
fail, selected freeze/inspection and exact retry, exhausted input budget, cancelled
inspection, and deadline interruption after header/read-set writes but during
member publication. The interrupted operation leaves no event, header, read set
or members. The migrated-project signature fixture now exercises the membership
file service, typed retry and named inspection. The CLI fixture verifies empty
and dense input refusals and missing-barrier inspection without changing execution
state or canonical head.

The final affected run passed **90 tests**: 57 barrier, 22 controlled-store,
one CLI and ten factory harness. The default build passed with warnings;
`git diff --check` was clean. The two focused tests overlap this run. No
compilation overlapped process tests.
[Validation log](factory-corrections-evidence/factory-barrier-freeze-budget-final.log).
This completes controlled ingress for the explicit freeze/inspection/release/
revocation operations; automated orchestration, the wider foreground audit and
live acceptance remain open. It is not a new full-suite checkpoint.

Barrier authorization drafting and signed release now use the scoped controlled
store and a single two-second deadline per request. Release passes that deadline
to signature verification, selected member/object reads, readiness/ancestry checks,
publication and best-effort denial logging. Object bytes and selected database
rows share the existing weighted input/structure budget; the separate 64-MiB
object ceiling remains an upper bound, not an allocation allowance. The raw trusted
store APIs retain their prior behavior. Controlled readiness does not install or
remove a nested SQL progress handler.

A new regression exhausts an existing input budget, refuses an expired draft,
and stalls the signed-receipt insertion after release event/header publication.
The original deadline interrupts SQL and rolls back all publication; a fresh
controlled retry succeeds, exact replay preserves the receipt, and cancellation
refuses further work. The owner signature test also checks deadline refusal.
The real migrated-project service continues to verify the correct owner namespace,
release/replay exact signed bytes, and preserve capacity on revocation.

The final affected run passed **122 tests**: 55 barrier, ten memory-readiness,
12 authority, 22 controlled-store, ten factory harness and 13 memory-control.
The default build check passed with warnings; `git diff --check` was clean.
The focused test overlaps this run. No compilation overlapped process tests.
[Validation log](factory-corrections-evidence/factory-barrier-release-budget-final.log).
This is local deadline/atomicity evidence, not a new full-suite checkpoint or
live acceptance. Filesystem calls are still subject to host syscall latency;
these checks do not claim a hard real-time bound.

The local `memory PROJECT barrier-revoke` command now records an operator reason
and expected head, invalidates the selected release and its dependents atomically,
and preserves signed history and attempt capacity until termination is proven.
Exact retries recover the original revocation; changed requests conflict. The
command and its best-effort denial record share one two-second deadline. Denial
logging now reads the current head directly instead of decoding a full snapshot.
The final affected run passed **90 tests**: 54 barrier, 12 authority, 22 controlled
store, one CLI and one memory-barrier harness test. The default build passed.
[Revocation validation](factory-corrections-evidence/factory-barrier-revoke-final.log).
This supplies explicit local revocation, not automated reviewer orchestration or
live memory acceptance.

`plan inspect PROJECT` now exposes the effective accepted intent through a bounded
production read path. Each page uses a database snapshot, returns the current plan
revision, and requires that revision on continuation. A changed plan rejects the
continuation instead of mixing revisions. Entries retain their source proposal ID,
source plan revision, payload digest, text and dependencies. Selected proposal
hashes and projection/source agreement are checked; unrelated superseded sources
are not decoded. This view does not report executable contracts or worker status.

Pages default to 32 entries, allow 1–64, and stop at one MiB of compact entry
encodings under the shared two-second deadline/input budget. Pretty CLI formatting
adds whitespace beyond this entry-byte accounting. A byte-limited page resumes
from the last returned task without skipping its first omitted task. Selected
source proposals are cached within a page. Tests cover empty plans, replacement
provenance, revision changes, missing continuation fences, source/projection
corruption, cold-source exclusion, byte pagination, bad limits and expired budgets.
The modeled 0 / 10,000 superseded-proposal probe measured **84 / 84 SQLite steps**
for inspection, excluding store opening and not claiming fleet latency.

The final affected run passed **93 tests**: 60 planning/wait, 22 controlled-reader,
one CLI and ten factory harness. The CLI fixture now accepts dependent planned work
before either task exists in execution state, inspects both revisions, refuses a
stale continuation and preserves task/attempt/control state. The default build and
`git diff --check` passed (existing build warnings remain). Initial focused tests
overlap the affected run; no compilation overlapped process tests.
[Validation log](factory-corrections-evidence/factory-plan-inspection-final.log).
This supplies the inspection side of the history projection; full contract
envelopes, planner inference and live acceptance remain open.

Two further planning regressions exposed forgotten accepted intent: successive
proposals could create a cycle across revisions, and a later proposal could not
reference a previously planned task until it appeared in execution state. Both
failed on the prior worktree
([before-fix log](factory-corrections-evidence/factory-plan-history-before.log)).
The current `plan_task_intents` projection now preserves the latest accepted
dependencies and source proposal for each planned task. Proposal validation
overlays retained planning intent and the new changes on the execution graph,
checks the resulting graph, and leaves execution tasks/queues untouched.
Publication maintains the projection transactionally; failed proposal or response
publication rolls it back. Unpublished migration 43 backfills latest intent from
immutable proposal history. Older schema behavior is preserved until explicit
upgrade; no real project stores were upgraded during this correction.

Regression coverage proves cross-revision cycle refusal, references to planned
tasks after restart, latest-edge replacement/backfill from a genuine schema-42
fixture, and rollback when projection publication fails. The scale probe models
10,000 superseded proposals through the projection's production insertion trigger:
whole acceptance used **741 / 741 SQLite steps** with zero / 10,000 such rows.
The unrelated-task probe remains constant at **749 / 749** for acceptance,
**74 / 74** for replay and **60 / 60** for stale-parent refusal. These are selected
local SQL measurements, not inference runs or fleet latency measurements.
[Boundary log](factory-corrections-evidence/factory-plan-history-boundary.log).
Queue/planned-task lookahead, per-task dependency bounds, aggregate edge bounds
and the shared deadline/input budget remain enforced.

The final affected run passed **204 tests**: 56 planning/wait, 83 migration,
22 controlled-reader, 32 controller, one CLI and ten factory harness. The default
build passed with existing warnings and `git diff --check` was clean. Focused
executions overlap this run; no compilation overlapped process tests.
[Final log](factory-corrections-evidence/factory-plan-history-final.log).
This repairs intended-plan continuity; it does not implement signed executable
contract installation, all proposal change types, planner inference or live
acceptance. Those review requirements remain open.

The full regression checkpoint above exposed and corrected the historical-feedback
repair fixture's new projection dependency. The production verification path did
not fail: the old fixture attempted to delete a referenced feedback row before
calling that path. Both canonical feedback and its derived pending row now
participate in the modeled absence, and the repair checks deduplication of both.
The contract inventory was also refreshed for sessions, typed waits, renewal and
automatic replan requests. Planning/fault documentation now identifies schema 43
fixtures and distinguishes admission's default-off setting from authenticated
policy activation. These documentation changes do not grant live acceptance.

Proposal validation no longer loads all retained task bodies twice through the
scheduler snapshot. The production CLI now uses the scoped store with a shared
two-second deadline/input budget, including proposal replay and planner-session
binding reads. Parent/session checks precede graph work. A dedicated reader loads
only bounded queue identities, dependency edges and indexed predecessor existence.
It checks the retained graph and proposed edge replacements without writing the
execution queue, preserving missing/duplicate/self/cycle refusals. SQL lookahead
enforces the existing 10,000 queued-task and 100,000 edge caps before unbounded
materialization; per-task dependencies remain capped at 256. Shared accounting may
refuse earlier. Task history outside the queue is not decoded for this operation.

The whole proposal-operation probe measured identical SQLite steps with zero /
10,000 modeled unqueued historical tasks: **661 / 661** for acceptance,
**74 / 74** for replay, and **60 / 60** for stale-parent refusal. These measurements
exclude opening the store and do not establish aggregate fleet latency.
Three new graph tests cover that probe, untouched queue edges, invalid dependencies,
new proposed predecessors, expired budgets and overfull-queue refusal.
[Boundary log](factory-corrections-evidence/factory-planner-graph-boundary.log).
The final affected run passed **120 tests**: 52 planning/wait, 22 controlled-reader,
32 controller, four CLI and ten factory harness. The default build check passed
with existing warnings; `git diff --check` was clean. Focused cases overlap that
run, and no compilation overlapped process tests.
[Final log](factory-corrections-evidence/factory-planner-graph-final.log).
Other graph/inventory callers, inference execution and live certification remain
open; the earlier full-suite result does not cover these changes.

[Planner sessions](../factory/planner-sessions.md) now retain immutable bounded
intent/event inputs, bound to the canonical store path/incarnation, input cursor
and parent plan revision. Production `plan session ... create/show` ingress uses
the scoped store with a two-second deadline. Creation validates the current head
and parent atomically, stores exact selected event records and intent bytes, and
returns their retained input digest. Exact creation retries recover the original
session; changed input conflicts. Recovery verifies hashes and store identity.
The input is limited to 64 KiB of intent, 64 selected events and 256 KiB total.
It supplies context, never execution authority or proof of success.

Version-2 proposals reference that session and digest with a rationale. Acceptance
validates the retained parent and identity, and stores the immutable session link
with the proposal/revision in one transaction. Changed digests, stale sessions,
attempted rebasing and missing links refuse acceptance. Version-1 manual proposals
remain supported. Proposed contract text is still not an installed signed task
contract. Full contract-catalog change envelopes, bounded inference execution,
automatic dispatch and live F2 acceptance remain open.

The affected run passed **164 tests**: 48 planning/wait, 83 migration,
22 controlled-reader, one CLI and ten factory harness. The new CLI test creates
and recovers a session across separate invocations, accepts a bound proposal,
replays its receipt and verifies unchanged tasks, attempts and control. The
default build passed with existing warnings; `git diff --check` was clean.
[Affected-run log](factory-corrections-evidence/factory-planner-session-final.log).
Four session tests then passed after adding a simultaneous two-writer race and
cross-store copied-input refusal; three overlap the broad run, for **165 distinct
affected tests**. Exactly one racing response advances the shared parent; the
other reports the new parent. Other cases cover accepted-reply loss/restart,
transaction rollback, immutable receipts, future/missing/duplicate/oversized
inputs, expired budgets and stored digest corruption.
[Final session tests](factory-corrections-evidence/factory-planner-session-concurrency.log).
These local fixtures do not run a planner provider or certify the live pilot.
No compilation overlapped process tests. The last full suite predates this work.

A further replan circuit-breaker probe reproduced a wall-clock rollback bypass:
the counter ignored requests timestamped earlier than the plan's reset record,
allowing **four automatic requests instead of two**. The fixture models a retained
reset timestamp ahead of the current clock without changing the host clock or
rewriting immutable rows. Counting now uses the plan revision and blocker alone;
reset timestamps remain audit metadata. The test covers manual and automatic
servicing, restart, escalation coalescing and a fresh budget on a new revision.
[Failing regression](factory-corrections-evidence/factory-replan-clock-before.log).
The final affected run passed **94 tests**: 45 planning/wait, six feedback,
32 controller, one CLI and ten factory harness. The default build check passed
with existing warnings and `git diff --check` was clean.
[Passing run](factory-corrections-evidence/factory-replan-clock-final.log).
This is local correction evidence; planner sessions, intent/evidence binding,
bounded inference jobs and live F2 acceptance remain incomplete.

The opt-in automatic replan-request service now connects eligible retained
feedback to durable requests during ordinary controller passes. It defaults off,
requires an active project, selects at most eight pending items under a two-second
budget, respects another consumer's live lease and persists rotation before
processing. Pausing or disabling it retains pending feedback. The head-checked
`plan auto-replan` CLI changes only request servicing; it does not enable admission
or run an inference job. Planner execution and signed dispatch remain open.

Immutable feedback-to-decision links retain every item coalesced into one
escalation. Linked items leave the indexed pending projection even when the
underlying escalated feedback stays open. They return the same decision after
restart or a new plan. Request, acknowledgment, notice and linkage are atomic;
failed notice publication leaves feedback available for retry. This closes the
repeated-selection gap for fourth and later feedback on the same blocker.

The focused idle-service probe used **53 / 53 SQLite steps** with zero / 10,000
modeled linked historical feedback rows. Three store tests and one controller
test passed in the [focused log](factory-corrections-evidence/factory-auto-replan-focused.log).
The final affected run passed **198 tests**: 44 planning/wait, six feedback,
83 migration, 22 controlled-reader, 32 controller, one CLI and ten factory harness.
The default build check passed with existing warnings and `git diff --check` was
clean. Focused cases overlap the affected run. These are local fixtures, not live
provider certification. The last full-suite run predates these changes.
[Affected-run log](factory-corrections-evidence/factory-auto-replan-final.log).

Adapter-recovery waits now support `OwnedRuntimeRecovered`, with exact binding
and ownership revisions supplied by the three `--recovery-*` CLI flags. It uses
production `runtime.observed` publications and the existing ownership predicates:
fresh v2 collector evidence must match current task revision, local resource
incarnations, agent and retained configuration identity. Registration and renewal
retain the typed reference atomically. Missing, replaced, stale, future-dated,
unmatched and remote evidence cannot establish recovery for the old claim. This
is an advisory reevaluation trigger; it neither clears reconciliation nor resumes,
adopts, dispatches or releases capacity. Broader recovery cases without retained
ownership and automatic orchestration remain incomplete.

The shared observation decoder now also supports an exact binding selection with
its existing payload hash/identity validation and input-budget accounting.
Unpublished migration 43 indexes observation events by binding, revision and
observation timestamp. The retained-publication lookup checks the current stored
payload and cites its exact event. This permits completion-before-subscription and
late older observations without treating unrelated history as current evidence.

The initial scale probe exposed **53 / 60,053 SQLite steps** at zero / 10,000 old
samples. Forcing join order alone gave **59 / 60,059**. Query-plan inspection
showed integer affinity prevented use of the timestamp expression-index key;
removing that coercion made the complete indexed lookup **50 / 50**. The historical
rows are modeled old events, not 10,000 live observations. An initial corruption
fixture also needed a schema-valid-length incorrect hash before reaching the
reader's hash check.
[Initial log](factory-corrections-evidence/factory-recovery-waits-focused.log),
[join-order log](factory-corrections-evidence/factory-recovery-waits-regression.log),
[passing indexed tests](factory-corrections-evidence/factory-recovery-waits-indexed.log).
All four new store tests and the existing production collector/adoption test passed.
The latter now registers a wait, commits fresh post-adoption evidence through the
collector, services exactly one notification and retains the adopted attempt.
These are local synthetic-adapter and filesystem fixtures, not live certification.
The final affected run passed **195 tests**: 41 planning/wait, two observation,
four controlled-runtime, 83 migration, 22 controlled-reader, ten ownership,
31 controller and two CLI tests. The new CLI fixture registers a typed recovery
wait against modeled retained ownership and verifies that replay changes neither
ownership, attempt capacity nor project control. The default build check passed
with existing warnings, and `git diff --check` was clean. The five focused
executions overlap the final run. No compilation overlapped process tests.
The last full-suite run predates these changes.
[Final retained log](factory-corrections-evidence/factory-recovery-waits-final.log).



Resource-availability waits now support `AttemptCapacityReleased`, selected by
`--capacity-attempt` and `--capacity-after-revision`. The trigger references an
existing attempt and a previously observed revision, persists atomically with the
wait and survives renewal. It uses the existing production worker-termination,
staged-launch-stop, worktree-preparation-stop and never-claimed-cancellation
receipts. Replay requires the newer canonical attempt revision to have observed
termination, and a receipt at that exact revision/sequence with the matching
attempt identity. Cancellation additionally requires `released: true`. Duplicate,
oversized or malformed selected receipts refuse processing; controlled reads
charge the selected payload before decoding. Approval and capacity CLI options
are mutually exclusive. Completion reports, stale generations, mere event names
and failed termination commits cannot wake this capacity subscription.

The seven focused tests passed, including actual local supervised worker and
staged-resource shutdown with commit-failure recovery, the real Git preparation
stop matrix, never-claimed versus uncertain cancellation, registration/renewal,
and CLI validation
([regression log](factory-corrections-evidence/factory-capacity-waits-regression.log)).
The initial compilation caught test imports using the binary crate name inside
library tests; those imports and the selected test target were corrected before
running the regressions
([initial log](factory-corrections-evidence/factory-capacity-waits-focused.log)).
An additional queued-old-event check now verifies that a later valid termination
receipt, rather than an older same-attempt event, is cited as the wake trigger.
Store-only malformed-event probes model metadata; the separate supervised and
Git tests supply production-path evidence. This subscription concerns retained
attempt capacity, not release of worktrees, panes or other retained references.
Admission still rechecks all resources and authority. Other resource and adapter
triggers, automatic planner workflows and certification remain incomplete.
The broad affected run passed **290 tests**: 37 planning, 56 reservation,
72 worker, 19 launch/worktree ingress, 83 migration, 22 controlled-reader and
one CLI, with ten opt-in worker tests ignored. The default build check passed
with existing warnings
([broad log](factory-corrections-evidence/factory-capacity-waits-final.log)).
Two subsequent tests also passed added renewal assertions against retained staged
and worktree receipts
([retained-receipt log](factory-corrections-evidence/factory-capacity-waits-retained.log)).
Those executions overlap the broad run.

A further out-of-order regression reproduced a lost renewal wake when an older
attempt-generation event arrived after the valid termination receipt
([failing probe](factory-corrections-evidence/factory-capacity-waits-ordering-before.log)).
The retained-receipt query now filters by the canonical attempt revision before
selecting the latest event. The broad run above predates this final query change;
the final query passed **42 focused tests**: all 37 planning tests, cancellation,
worker/staged/worktree termination and CLI checks, followed by the default build
check with existing warnings
([final ordering log](factory-corrections-evidence/factory-capacity-waits-ordering-final.log)).
These executions overlap the broad run. `git diff --check` was clean. No compilation
overlapped process tests. This is local synthetic-adapter/Git evidence, not live
provider certification.



User-decision waits now support an immutable `ApprovalDecision` trigger with an
exact content-addressed approval ID and signed task-scope revision. The CLI
accepts `--approval-id` with `--approval-task-revision` only for `user_decision`.
Unpublished migration 43 stores the reference atomically with registration and
preserves it on renewal. Existing registration identities remain unchanged when
no trigger is supplied. Production `approval.installed`/`approval.revoked` events
from the existing authority service drive replay; a matching event alone cannot
substitute for its retained, hash-validated approval record. Other tasks, other
approvals and stale task revisions do not match. Prior retained decisions are
checked at registration and renewal. These wakes are advisory even after grant
expiry/revocation and never consume an approval or create an attempt.

Project registration now uses the scoped two-second store opener and propagates
its read budget into retained evidence and trigger validation. A related notice
bug was corrected: `wait.notified` now records the actual `trigger_sequence`
separately from the existing `trigger_through` replay cursor, and the inbox text
cites the triggering event instead of possibly unrelated later history.

Three new store tests and the real signed-owner ingress/CLI tests passed initially
([focused log](factory-corrections-evidence/factory-wait-triggers-focused.log)).
The signed-owner test now also uses production bounded wait servicing to emit one
notice after valid signature import and zero on a second pass. Store tests cover
missing decision records, unrelated approvals, wrong task/revision, restart,
retained decisions, revoked decisions, inherited immutable triggers, transaction
rollback, registration idempotency and rejection after task revision advances.
Typed triggers for other conditions, broader authenticated user decisions,
resource/adapter producers, automatic renewal and planner dispatch remain open;
this is not acceptance of the live planning pilot.
The final affected run passed **183 tests**: 35 planning, 11 authority,
83 migration, 22 controlled-reader, 31 binary controller and one CLI. The default
build check passed with existing warnings, and `git diff --check` was clean.
The five focused executions overlap this final run. No compilation overlapped
process tests. The last full-suite run predates these changes.
[Final retained log](factory-corrections-evidence/factory-wait-triggers-final.log).



Schema 43 now supports explicit renewal of terminal advisory waits through
`plan wait PROJECT rearm WAIT_ID [--deadline RFC3339]`. A transaction creates one
immutable predecessor/successor link, a new registration cursor and any addressed
wake justified by currently retained evidence. The predecessor receipt remains
terminal. Exact retries return the same successor; changing the requested deadline
on a retry conflicts. An omitted deadline clears expiry on the successor. Pending
waits and waits from superseded plans refuse renewal. The CLI uses a scoped store
with a two-second deadline and propagates the read budget through selected fields
and dependency evidence validation. It does not start a model, grant authority,
release capacity or automatically renew subscriptions.

The initial two store regressions and actual CLI regression passed
([focused log](factory-corrections-evidence/factory-wait-rearm-focused.log)).
The expanded tests exercise restart idempotency, duplicate wakes addressed to the
predecessor, controller notification of the successor, capacity retention,
immutable links, transaction rollback on link failure, stale-plan refusal,
already-retained verification evidence and an expired read deadline. Typed trigger
references, additional production producers and automatic renewal/planner dispatch
remain incomplete. The full-suite result above predates this change.
The final affected run passed **169 tests**: 32 planning, 83 migration,
22 controlled-reader, one actual CLI and 31 binary controller tests. The default
build check passed with existing warnings, and `git diff --check` was clean.
The initial library controller filter selected zero tests; the subsequent binary
run supplies the controller coverage. The focused tests overlap the final run.
No compilation overlapped process tests.
[Planning/migration/reader/CLI/build log](factory-corrections-evidence/factory-wait-rearm-final.log),
[controller log](factory-corrections-evidence/factory-wait-rearm-controller.log).


Canonical worktree conflict checks now select nonempty local worktree bindings
and malformed machine/path identities through dedicated indexes. Empty retired
bindings and known remote paths do not consume the local inventory record budget.
Nonempty local paths remain references regardless of task state or filesystem
absence. The selected reader shares the existing provenance decoder and bounded
completeness checks; full administrative and exact-binding reads retain their
validation. A typed selector keeps their SQL predicates and parameters aligned.
Schema-42 readers use the fallback query without the new indexes.

The selected worktree-binding reader measured **4,252 / 4,252 SQLite steps** with
zero / 10,000 retired bindings, ownership/observation references and unrelated
legacy inbox records. Pane selection remained flat at **4,274 / 4,274** after the
shared-reader refactor. All 12 inventory tests passed, including unknown routing
fields, selected hash corruption, absent retained paths, cancellation and genuine
schema-42 fallback. The real Git preparation regression now retains 10,000 retired
bindings as well as audit events and completes creation and receipt recovery;
its selected input/render checkpoint measured **1,308 / 1,308** steps
([regression log](factory-corrections-evidence/factory-worktree-bindings-regression.log)).
These metrics exclude filesystem alias inspection, Git and receipt commits.

An additional actual preparation test passed for exact path, ancestor symlink and
descendant conflicts in a canonical neighbor: each refuses before approval
consumption or worktree creation, and clearing the reference permits one checkout
with the expected file bytes
([alias log](factory-corrections-evidence/factory-worktree-bindings-aliases.log)).
The initial alias fixture accidentally precreated the reserved worktree namespace;
it now aliases the existing `.state` ancestor instead
([fixture result](factory-corrections-evidence/factory-worktree-bindings-alias-fixture.log)).
Retained worktree creation/receipt inventories and aggregate filesystem/reference
costs still require audit; no retained nonempty path was discarded to improve the
measurement, and this is not a live fleet certification.
The final affected run passed **206 tests** (83 migration, 19 launch-ingress,
72 worker, 22 controlled-reader and ten ownership), with ten opt-in worker tests
ignored. The default build check passed with existing warnings, and
`git diff --check` was clean
([log](factory-corrections-evidence/factory-worktree-bindings-final.log)).
The 14 focused executions above overlap this final run. No compilation overlapped
process tests. The earlier 1,382-test full-suite result predates this refactor.


Pane checks now read a transactionally maintained set of missing identity
references on current schemas. Unpublished migration 43 backfills observation,
ownership and imported-source gaps; key-scoped triggers maintain them when
references, bindings or source paths/kinds are inserted, changed or removed.
Missing references still cause refusal. Full administrative inventories and
older-schema readers retain their original completeness checks. Selected payload,
row identity and imported-source validation remain unchanged.

The expanded regression exposed **4,200 / 164,200 SQLite steps** with zero /
10,000 retired binding/observation/ownership rows
([before log](factory-corrections-evidence/factory-identity-gaps-before.log)).
The final selected reader measured **4,260 / 4,260** with those references plus
10,000 unrelated legacy inbox records. These are modeled retained metadata,
not live ownership certification. All **11 focused inventory tests passed**,
including missing-binding refusals, repair, reference reassociation/deletion,
rollback isolation, source-kind changes and SQL backfill from a real schema-42
fault fixture
([log](factory-corrections-evidence/factory-identity-gaps-regression.log)).
The SQL backfill probe is separate from the administrative upgrade service's
integrity checks; it does not claim that upgrading a corrupt store is permitted.
An initial boundary fixture incorrectly expected a separate connection to see
uncommitted savepoint changes; it now checks writer state, committed reader
isolation, rollback and subsequent committed refusal explicitly
([initial fixture log](factory-corrections-evidence/factory-identity-gaps-fixture.log)).
This closes the completeness scans in the canonical worker's selected pane path;
other inventory callers and whole-graph/fleet costs remain in the audit.
The affected final run passed **189 tests** (82 migration, 72 worker, 22
controlled-reader, ten ownership and three observation), with ten opt-in worker
tests ignored, followed by the default build check with existing warnings
([log](factory-corrections-evidence/factory-identity-gaps-final.log)).
The 11 focused executions overlap this run. No compilation overlapped process
tests, and `git diff --check` was clean. The earlier full-suite result above
predates this change and is not presented as a full rerun of the current tree.


Repeated reuse of a pane now selects a transactionally maintained retained-resource
projection. Unpublished migration 43 backfills this projection from the existing
inventory lifecycle rule and updates it when relevant events, attempt inputs or
attempt termination evidence change. A workspace remains retained regardless of
worker termination; a target is removed only when its input-linked attempt has
observed termination and a start receipt exists. Missing attempt/input provenance
and loss of start evidence retain or restore the reference. The selected reader
still validates the original event, immutable input and operation provenance;
projection membership does not grant execution authority. Indexed operation-scoped
maintenance avoids rebuilding unrelated references. Older schemas retain the
read-only fallback. This supersedes the same-pane history limitation below.

The new regression measured **4,140 / 4,140 SQLite steps** for selection and
**656 / 656** for a termination update plus rollback at zero / 10,000 terminated
launches sharing one pane
([log](factory-corrections-evidence/factory-retained-pane-regression.log)).
Those historical rows model relational lifecycle evidence and intentionally do
not supply fresh valid authority for their synthetic operations. The real local
fixture worker was stopped before termination evidence was changed. Rollback
restores the reference; deleting start evidence restores an uncertain target;
malformed pane updates still cause refusal. Separate mutation probes compare the
maintained projection with the audited inventory rule after input/attempt deletion,
input reassociation and event entity/kind/sequence changes, including rollback.
Workspace receipts survive acknowledged termination. A genuine schema-42 upgrade
backfills the new table and preserves selected results. These three boundary tests
passed, including the other-pane history regression, now **4,140 / 4,140** steps
([log](factory-corrections-evidence/factory-retained-pane-boundaries.log)).

This is local lifecycle and bounded-query evidence, not fleet latency or live
certification. Worktree inventories, legacy completeness/dangling-reference checks,
full dependency-graph costs and broader production workflow corrections remain open.


Canonical worker pane-conflict checks now select staged launch/workspace targets
for the requested pane, including empty or malformed pane identities as
uncertainty. Candidate discovery drives the provenance joins; current-schema
queries explicitly use the pane and malformed-pane indexes in unpublished
migration 43. Older read-only schemas retain the unhinted fallback. Lifecycle
retention rules and selected input/operation/target validation are unchanged.
A per-operation/kind duplicate check prevents a second receipt from hiding
behind a different pane ID. Full administrative inventories remain available.

The regression initially measured **4,192 / 224,192 SQLite steps** at zero /
10,000 other-pane receipts, revealing that an IN predicate still allowed a
history scan. Driving from candidate sequences alone reduced this to
**4,167 / 94,167**; explicit index selection removed the remaining broad kind
scan. The final publication-checked reader measured **4,061 / 4,061** steps.
[Initial measurement](factory-corrections-evidence/factory-staged-pane-before.log),
[join-order measurement](factory-corrections-evidence/factory-staged-pane-join-order.log)
and [passing focused tests](factory-corrections-evidence/factory-staged-pane-regression.log)
are retained. A subsequent fixture error addressed the brief operation rather
than the launch receipt and was corrected before completing the boundary probes
([fixture result](factory-corrections-evidence/factory-staged-pane-fixture.log)).

All **three focused tests passed**. The new regression covers null, numeric,
array and empty pane identities, cross-pane duplicate receipts, cancellation,
and one actual local synthetic-adapter brief send with the unrelated receipts
retained. Existing forged-attempt and rehashed-input/broken-operation probes
also exercise the selected reader. The 10,000 rows model unrelated receipts
with known other pane IDs and deliberately missing input provenance; full
inventory still refuses them. This is not evidence for 10,000 completed live
launches, aggregate fleet latency or history independence of worktree/legacy
completeness inventories. Repeated reuse of the same pane across terminated
launches also remains to be measured and corrected: candidate selection narrows
by pane, while lifecycle filtering still follows candidate discovery.
The affected worker and migration run passed **150 tests** (69 worker, 81
migration), with ten opt-in worker tests ignored; the default build check also
passed with existing warnings
([log](factory-corrections-evidence/factory-staged-pane-final.log)). One subsequent
additional test passed against a genuine schema-42 prefix with both new indexes
absent, preserving matching target results and refusal of malformed identities
([historical fallback log](factory-corrections-evidence/factory-staged-pane-historical.log)).
Only this test was added after the broad run; production code stayed unchanged.
The three earlier focused tests overlap the broad run. No compilation overlapped
process tests, and `git diff --check` was clean.


Canonical worker pane-conflict checks now select bindings for the requested pane
through an indexed query, including every malformed pane field as uncertainty.
Selected rows retain the existing payload hash, row identity and imported-source
validation. A separate missing-imported-binding check preserves completeness
without comparing a selected subset to the full inventory count. Existing
orphan ownership/observation checks remain. New indexes in unpublished migration
43 cover pane IDs and malformed pane identities. Full administrative, worktree
and adoption inventories retain their existing behavior.

The selected publication-checked reader measured **4,102 / 4,102 SQLite steps**
with zero / 10,000 retired canonical bindings. The fixture also rejects a corrupt
matching hash, null/numeric/array pane identities, cancellation and a deleted
imported binding. The actual native brief-delivery regression uses a neighboring
canonical project containing 10,000 retired bindings: socket aliases, unresolved
remote identity and corrupt matching provenance each refuse before the brief
claim/send, while restoring the nonconflicting coordinator permits one send.
All **three focused pane tests passed**
([log](factory-corrections-evidence/factory-pane-selected-boundaries.log)).
The first focused run exposed the old all-bindings inventory-count assertion
being applied to a subset; the corrected reader keeps that assertion for full
reads and independently checks missing imported bindings for pane selection
([initial result](factory-corrections-evidence/factory-pane-selected-initial.log)).

This metric does not establish complete cross-project history independence:
launch-target/worktree inventories, legacy-source completeness and dangling
ownership/observation checks still require audit and aggregate measurements.
It is local synthetic-adapter evidence, not live provider or rollout certification.
The final affected run passed **171 tests** (68 worker, 81 migration and 22
controlled-reader), with ten opt-in worker tests ignored, followed by the default
build check with existing warnings. The three focused executions overlap this
run. `git diff --check` was clean; no compilation overlapped process tests.
[Final validation log](factory-corrections-evidence/factory-pane-selected-final.log).


Worktree verification before native launch, post-release incarnation pinning and
preparation-only termination now validate retained provenance for the exact
launch operation. The selected reader uses the same publication checks,
immutable input/operation identity, claim/approval validation, duplicate checks
and receipt validation as the full inventory, with indexed operation predicates
on both the orphan-receipt check and provenance query. Missing selected creation
provenance is an error. Root-wide allocation conflict checks still use the full
inventory; this change does not certify their history independence.

All **seven focused worktree tests passed**
([log](factory-corrections-evidence/factory-worktree-provenance-selected.log)).
The new regression verifies real local Git worktree incarnations and completes
preparation cancellation/preservation with 10,000 unrelated malformed creation
and ready events present. A two-record budget covers the one selected intent
and plan. The full inventory still refuses the unrelated corruption. Existing
corruption probes now also assert refusal by the selected reader for altered
paths/tokens/operation identities, duplicate creation, oversized payload and
orphan receipts. Cancellation and missing selected creation are refused.
This proves scoped validation and local recovery, not whole-fleet latency;
no SQL-step metric was added for this reader.
The final affected suites passed **94 tests** (18 launch-ingress, 67 worker and
nine migration inventory), with ten opt-in worker tests ignored. The default
build check passed with existing warnings; `git diff --check` was clean.
[Final validation log](factory-corrections-evidence/factory-worktree-provenance-final.log).
The seven focused executions above overlap this final run. No compilation
ran concurrently with process tests, and no live adapter was exercised.


Start/naming and launch reconciliation now select the exact immutable input,
delivery revision and at most five lifecycle event kinds instead of loading
administrative snapshots. Naming and start receipts share controlled budgets;
start validation selects the exact binding, attempt, ownership and historical
approval consumption. Receipt publication reconciles only the affected task.
The ownership-generation lookup has an index on its two relevant event kinds.

The start selector measured **183 / 183 SQLite steps** at zero / 10,000 unrelated
start events. Cancellation and stale revisions refuse selection. The local
synthetic-adapter regression now completes gate release, one-use naming, start
confirmation and read-only reconciliation with a corrupt unrelated grant still
present; the administrative snapshot continues to reject the corruption.
[Focused start results](factory-corrections-evidence/factory-start-selected-regression.log)
and [reconciliation results](factory-corrections-evidence/factory-reconcile-selected-regression.log)
are retained. The final affected run passed **199 tests** (67 worker, 55
reservation, 17 launch-ingress, 47 upgrade and 13 memory-control), with ten
opt-in worker tests ignored, followed by the default build check
([log](factory-corrections-evidence/factory-start-selected-final.log)).
This supersedes the start/naming status in the historical entries below; it
does not establish whole-lifecycle history independence. In particular,
worktree provenance helpers and cross-project inventories remain in the audit.


Launch advancement's initial and post-creation checkpoints now select the exact
delivery revision and immutable launch input using controlled connections under
the original absolute deadline/cancellation. Eight indexed existence probes
read only the one-use lifecycle boundaries needed for the next decision; they
do not decode event payloads or scan the retained log. The selected input reader
still validates the operation kind, task/input identity and payload hashes.
Each checkpoint closes its connection before invoking an independently guarded
effect phase, and effects retain their existing payload/authority validation.

The regression measured **197 / 197 SQLite steps** at zero / 10,000 events for
other launches. Those modeled events never set the selected launch's flags;
each of the eight selected kinds is recognized when subsequently added. Stale
revisions and cancellation are rejected. With a corrupt unrelated approval,
the actual advancement entry point reaches its intended execution-environment
refusal without creating a resource; administrative snapshots still reject the
corrupt grant. This is not a claim that cold corruption is ignored throughout
the remaining start/naming service, which still has snapshot reads.

All **three advancement tests passed**, including the existing multi-boundary
recovery sequence through brief delivery and termination, and refusal of an
unusable environment before creation
([log](factory-corrections-evidence/factory-advancement-selected-regression.log)).
The metric covers the selection checkpoint only, not native effects, subsequent
service phases, cross-project inventories or whole-fleet latency. Start/naming
and its reconciliation wrappers remain the next foreground readers to correct.
The default build check also passed
([log](factory-corrections-evidence/factory-advancement-selected-default.log)).


Worktree preparation now selects the launch's exact input, delivery, binding and
at most two creation-intent rows under a controlled read transaction. Selection,
prompt rendering, claim/approval consumption, pre-effect claim checks and final
checkout-receipt retention share the original deadline/cancellation and read
budget for the worktree phase. Git and filesystem work run after the read
transaction has ended. The final receipt path selects the current delivery/head,
revalidates exact creation identity and historical approval consumption, and
charges at most two retained receipt payloads before comparing them. It no longer
loads full snapshots or all approvals. Obsolete raw store/render wrappers were
removed after their final production callers moved to controlled methods.

The selected preparation-and-render regression measured **1,307 / 1,307 SQLite
steps** at zero / 10,000 generic historical events and rejects a cancelled control.
With a corrupt unrelated grant still present, the public preparer creates a real
local Git checkout, verifies its file bytes, then re-observes identical receipts
without publishing another ready event. Full administrative snapshot validation
continues to reject that grant. The metric excludes Git, inventory checks, claim
and receipt commits; these history rows model retained audit data rather than
10,000 worktrees
([regression log](factory-corrections-evidence/factory-worktree-selected-regression.log)).
The affected run passed **17 launch-ingress, 64 worker, 55 reservation and ten
delivery tests** (146 executions; ten opt-in worker tests ignored), then the
default build check
([log](factory-corrections-evidence/factory-worktree-selected-final.log)). No
compilation overlapped process tests. The preparation suite covers incomplete
checkout observation, lost receipts, immutable provenance, rollback, changed
configuration and approval refusal before external creation. These are local
synthetic-adapter and Git tests, not live provider certification.
Repository-backed creation still separates preflight, Git preparation and native
creation into guarded phases with the same absolute deadline and independently
bounded phase budgets. Cross-project inventories and start/launch-advancement
readers remain in the wider foreground audit.


Native resource creation now uses selected preflight inputs/delivery and a typed
resource selection instead of administrative snapshots. Selection, prompt
rendering, approval consumption, atomic creation intent, final claim checks,
legacy workspace/layout receipts and observed-target retention share the
controlled connection's budget. The native and worktree claim callbacks receive
the supplied budget rather than silently dropping it. Their creation validators
select the exact launch input/binding and approved execution route.

For native-only creation the preflight connection remains under one uninterrupted
root guard through creation. Repository-backed creation closes that connection
and releases the guard before the independently guarded worktree preparer, then
opens a new controlled connection under the root guard for the native phase.
It preserves the original deadline/cancellation; it does not carry a database
handle across an unlocked store-replacement interval. The subsequent worktree
preparation correction is recorded above; start/naming and launch advancement
still remain in the foreground audit.

The new native-creation regression inserts a corrupt unrelated approval before
the public entry point. Its first run exposed another full snapshot in that
wrapper, which is now replaced with selected preflight reads. After creation it
checks the exact retained target and refuses another creation attempt while
administrative snapshot validation continues to reject the cold corrupt grant.
The creation selection and rendering metric is **1,424 / 1,424 SQLite steps**
with zero / 10,000 unrelated creation events. It excludes preflight, the claim
transaction, native calls, inventories and target-retention commit. The fixture
models event history rather than 10,000 actual launches; cancellation prevents a
subsequent selection without creating any native resource
([initial regression and preflight failure](factory-corrections-evidence/factory-creation-selected-regression.log)).
The affected run passed **64 worker, 55 reservation, 16 launch-ingress and ten
delivery tests** (145 executions; ten opt-in worker tests ignored), plus the
default build check ([log](factory-corrections-evidence/factory-creation-selected-final.log)).
The guard handoff was refined after that worker binary began running; following
the completed run, **four resource-creation and two launch-advancement tests**
were rerun against the final guard code, including repository-backed advancement
([log](factory-corrections-evidence/factory-creation-selected-guard.log)). These are
**151 executions** with overlapping filters. Compilation and process tests did
not overlap. Native creation no longer opens a raw/full snapshot in the resource
adapter; start/advancement readers still need correction. The worktree preparer
was subsequently corrected as recorded above.


Gate release now selects the claimed launch and its exact lifecycle/worktree
receipts on a scoped controlled connection instead of loading a full snapshot.
Prompt rendering shares that connection's read budget, and filesystem proof
verification receives only the selected events. The final one-use release
transaction selects the exact input, attempt, task, binding, ownership and
creation/target evidence. Shared launch claim validation now propagates its
budget through operation payload validation, memory eligibility, authority,
budget policy, project control, scheduler and selected runtime records. Existing
lease, configuration, approval-revocation, supervisor and native identity fences
remain in place before gate input.

The gate preparation regression measured **2,183 / 2,183 SQLite steps** for
selection, claim validation and prompt rendering at zero / 10,000 unrelated
creation events, and rejects a cancelled control. This excludes native identity
checks, executable observation, cross-project inventories and the final release
commit. An additional case in the real local gate-submission fixture inserts a
corrupt unrelated approval after creation; gate submission succeeds once and
administrative snapshots still reject the corruption. The first test version
then tried to delete that immutable fixture row and failed at cleanup. It now
keeps the row, inspects the exact release/start receipts, and verifies no second
native submission. No production immutability guard was weakened
([initial measurement and fixture failure](factory-corrections-evidence/factory-gate-selected-regression.log)).
The final run passed **62 worker, 55 reservation, 16 launch-ingress and 13
memory-control tests**, followed by the default build check
([log](factory-corrections-evidence/factory-gate-selected-final.log)). Ten opt-in
worker tests remain ignored. The separately sequenced generic delivery suite
passed **ten tests** ([log](factory-corrections-evidence/factory-gate-selected-delivery.log)),
for **156 affected test executions**. The shared selection now includes gate and
worktree evidence; its recovery-only measurement is consequently **351 / 351**
steps in this source state (the earlier 311 / 311 log measured the smaller event
selection). No process tests overlapped compilation.


Resource recovery now opens one scoped controlled connection instead of loading
a full administrative snapshot. Its typed selection validates the exact delivery
revision/claim count, reserved attempt, immutable input, binding, historical
approval consumption and worktree route. It reads only that operation's creation,
workspace, layout and target events, rejects duplicate kinds, and charges payloads
before decoding. Both supervised target and historical workspace observation
commits use that connection's original deadline and shared read budget. Workspace
receipt validation also uses selected input/binding/attempt/approval readers;
normal creation's separate claim-validation path remains in the ongoing audit.

The actual recovery selection measured **311 / 311 SQLite steps** with zero /
10,000 unrelated creation events, and rejected cancellation on the same control.
The fixture deliberately models cold malformed audit payloads, then removes those
synthetic creation rows before the independent cross-project inventory validation.
With an unrelated corrupt approval still present, real local recovery after a
lost synthetic-adapter creation reply observes the target without creating a
second resource; administrative snapshots still reject the corrupt grant. This
measures the selected reader, not native observation, inventory, or the complete
recovery workflow
([regression log](factory-corrections-evidence/factory-resource-recovery-regression.log)).
The final affected run passed **61 worker, 55 reservation and 16 launch-ingress
tests** (132 executions; ten opt-in worker tests ignored), plus the default build
check. This includes legacy workspace recovery after authority revocation/expiry,
retained-target recovery after failed commits and the selected-history regression
([log](factory-corrections-evidence/factory-resource-recovery-final.log)).
Compilation and process-test execution were sequential. Creation still needs its own selected-reader/claim-path corrections; full fleet and live
certification requirements remain open.


Post-creation target retention now opens a controlled connection using the
original resource call's deadline/cancellation, selects its delivery and head,
and atomically validates/persists the observation. Target receipt validation
selects the operation's immutable input, exact attempt/binding and historical
approval consumption. It no longer decodes all retained approvals, inputs,
attempts or bindings. Runtime-binding and retained-target pane collision probes
use expression indexes in the unpublished schema-43 migration; conflicting
identities still reject the observation. Worktree execution-route validation
shares the budget, reads at most two matching event rows before rejecting a
nonunique receipt, and validates the exact historical approval use.

The selected retention regression uses the local synthetic resource adapter,
keeps administrative rejection of a corrupt unrelated grant, and models 10,000
unrelated retained target events. These are audit-history fixtures, not 10,000
actual worker launches. The initial regression measured **378 / 378 SQLite
steps** for idempotent re-observation of the exact existing target at zero /
10,000 rows. It does not measure a new target insertion or native observation. Cancellation rejects another observation and the
adapter creates only one resource. The expanded verification also checks a
conflicting retained pane and an expired control. Creation entry-point
snapshots, other launch lifecycle transactions and cross-project inventories
remain in the foreground audit; this is not a full resource-workflow cost bound.
The affected run passed **60 worker, 16 launch-ingress and 47 upgrade tests**,
with ten opt-in worker tests ignored, and the default build check
([log](factory-corrections-evidence/factory-target-selected-final.log)).
The initial measurement is retained separately
([log](factory-corrections-evidence/factory-target-selected-regression.log)).
The final boundary run passed **55 reservation tests** and the target regression
with a typed `Deadline` assertion, retaining the **378 / 378** measurement
([log](factory-corrections-evidence/factory-target-selected-boundaries.log)).
Together these are **179 test executions**, including the repeated regression.
No compilation overlapped running process tests; the final assertion-only test
change was compiled and exercised in the boundary run.


Worktree-only termination no longer runs Git/filesystem preservation inside an
SQLite transaction, as prohibited by plan 06. A short read transaction selects
and validates the stop context, then releases SQLite while the inherited root
barrier remains held for capture. A fresh write transaction repeats every
eligibility check and compares the complete selected attempt, task, input,
creation intent, delivery and cancellation flag, under the original head and
attempt revision. Budget cancellation is checked before capture and before
reentering SQLite. Concurrent changes or cancellation retain capacity and the
captured artifacts rather than publishing a stop receipt.

The new regression uses real local worktree/output preservation and a separate
SQLite connection with zero busy timeout. That writer successfully obtains and
commits an immediate transaction inside the capture callback. Its unchanged
case permits termination; appending an event, changing only the task revision
without advancing the event head, or cancelling the original read control after
capture prevents termination and retains artifacts/capacity. Direct SQL mutations
explicitly model a competing writer bypassing the root guard; they are not
claims that the production guard authorizes concurrent resource mutation.
The final affected run passed **16 launch-ingress and 59 worker tests** (75
executions; ten opt-in worker tests ignored), followed by the default build check
([log](factory-corrections-evidence/factory-preparation-transaction-final.log)).
The first extended test compilation used the wrong module path for `ReadControl`;
that test-only import was corrected before this successful run. Existing default
build warnings remain. This verifies the selected stop boundary, not all resource
adapters or whole-fleet performance.


The common launch-knowledge fence now selects an indexed maximum for each of
its seven memory event kinds, instead of aggregating all retained matching events.
The unpublished schema-43 migration adds `(kind, sequence)` for this lookup;
the scalar result is charged to the caller's read budget. The actual validator
regression measured **168 / 50,167 SQLite steps** at zero / 10,000 historical
events before the fix and **250 / 253** afterward. Each of the seven event kinds
still rejects a knowledge snapshot when appended after its selection. The
fixture models retained event history; it does not simulate signed promotions
or measure the full launch workflow. Evidence:
[before](factory-corrections-evidence/factory-memory-frontier-before.log),
[after](factory-corrections-evidence/factory-memory-frontier-after.log).
The affected run passed **55 reservation, 59 worker, 47 upgrade, ten factory
harness and 13 memory-control tests** (184 executions; ten worker tests remain
opt-in), plus the default build check
([log](factory-corrections-evidence/factory-memory-frontier-verified.log)).


Termination reconciliation now selects one attempt and its exact task, immutable
launch inputs, binding, ownership, delivery, approval use and lifecycle events.
Preservation receives those events explicitly instead of a full snapshot. The
same controlled connection and original deadline govern selection, quiescence
validation and the final stop transaction. Started-worker, staged-worker and
worktree-only stop commits use selected readers; initial brief retirement selects
only that attempt's obligations, and consumer reconciliation selects its task.
The unpublished schema-43 migration adds indexes for lifecycle events by entity
and initial briefs by attempt. No existing project stores were migrated.

Stop eligibility validates the original approval consumption and claim history
without demanding a currently unrevoked or unexpired execution grant. Exact
supervisor/ownership proof, repository/output preservation, historical-bootstrap
quiescence refusal, head/revision fences, rollback and retained-capacity rules
remain in force. The corruption regression now stops its cancelled local fixture
worker while full administrative snapshot validation continues to reject the
unrelated corrupt approval. All **58 worker tests passed**, with **10 opt-in
tests ignored** ([log](factory-corrections-evidence/factory-termination-selected-workers.log)).

The selected termination reader takes **383 SQLite steps** at both zero and
10,000 unrelated launch-history events, and refuses a cancelled control token.
This measurement covers the controlled selection connection, not filesystem
preservation, cross-project inventories, external process observation or the
whole stop transaction; fleet latency and complete history-cost gates remain open.
The boundary run passed the new measurement, **20 worktree**, **54 reservation**,
**47 upgrade**, **22 controlled-read**, **ten factory-harness** and **13 parallel
memory-control** tests. The default build also passed
([log](factory-corrections-evidence/factory-termination-selected-boundaries.log)).
Together with the worker suite these are **225 test executions**, with overlap
between filters; embedded child-test output is not counted again. The new
corruption fixture was corrected to use the revision returned by cancellation;
its initial full-snapshot failure identified unrelated approval decoding before
the revision check. These local process tests do not certify live providers or
the remaining whole-fleet scale gates.

Prompt preparation and delivery now open one scoped controlled connection under
the caller's original deadline and cancellation identity. They select the exact
attempt, operation, launch receipt, binding, ownership and memory inputs rather
than materializing a full store snapshot. Attempt-knowledge validation selects
its immutable launch input directly. Rendering retains that connection instead
of opening another raw store. Shared accounting covers selected snapshot/input/
record/revision rows, consumed-object references and object bytes; object reads
check cancellation between 64-KiB chunks and still verify the complete hash.
Claim, final pre-submission validation and acknowledgment retain the same SQL
deadline and budget. Current frozen-knowledge and authority checks remain in force.

The original delivery regression failed on an unrelated corrupt historical
approval ([before](factory-corrections-evidence/factory-brief-render-before.log)).
The expanded case now prepares and sends exactly once while full administrative
snapshot validation still detects that unrelated corruption. A separate valid
history test observes **1,746 SQLite steps** for prompt selection and rendering at
both zero and 10,000 unrelated events and grants. It also verifies
cancellation prevents rendering and claiming. This measurement excludes the
cross-project identity inventory, external adapter calls and acknowledgment
transaction, and does not establish bounded work for all retained memory graphs.

The affected suites passed **239 test executions**: 57 canonical-worker, 60
memory, 54 reservation, 22 controlled-read, 23 admission, ten factory-harness and
13 parallel memory-control tests. Ten opt-in worker tests remained ignored; the
default-feature build check also passed
([retained log](factory-corrections-evidence/factory-brief-render-verified.log)).
New checks reject oversized selected input before decoding/claiming and verify
that object reads share the existing budget and honor cancellation/expiry before
I/O. Two intermediate failures were corrected: restoring the already-claimed
diagnostic ([log](factory-corrections-evidence/factory-brief-render-diagnostic-failure.log))
and explicitly bypassing an ingress size CHECK in the corrupt-storage test fixture
([log](factory-corrections-evidence/factory-brief-render-fixture-failure.log)).
No live provider certification or new full all-target run is claimed for this refactor.

Initial prompt approval validation now reads only the exact grant, consumption,
revocation flag and launch delivery. It retains payload/hash, exact action,
consumption-time validity, claim history and current-expiry checks under the
original read budget. Full administrative snapshot validation is unchanged.
The new regression exercises the actual pre-effect decision with 10,000 valid
unrelated retained grants: before correction it exhausted the 50 MiB budget
([failure](factory-corrections-evidence/factory-brief-approval-before.log)); after
correction SQL progress counts were **366 steps at both 0 and 10,000 grants**.
The test also rejects corrupt selected claim history. All **54 reservation tests**
passed ([log](factory-corrections-evidence/factory-brief-approval-final.log)).
All **54 canonical-worker tests** also passed, with **10 opt-in tests ignored**
([worker log](factory-corrections-evidence/factory-brief-approval-worker.log)).
This removes the approval and launch-input inventory scans from this boundary;
it does not establish the complete workflow's history independence.

Initial prompt acknowledgments now retain an explicit stale-delivery event when
the reserved barrier is already durably invalidated. Receipt, observation and
attempt state commit atomically, while acceptance remains blocked and capacity
stays held until termination. The regression first failed with a missing stale
observation ([before](factory-corrections-evidence/factory-barrier-prompt-race-before.log)).
It now covers normal acknowledgment, injected commit failure, refusal to resend,
claim expiry, reopening, recovery from independently retained acknowledgment
evidence, and actual supervised local-process termination
([recovery test](factory-corrections-evidence/factory-barrier-prompt-race-recovery-final.log)).
The concurrent writer deliberately bypasses the fixture adapter's process guard;
the native adapter and predecessor authority are modeled. This is not live
provider certification or proof that a lost acknowledgment can be reconstructed.
A separate historical-replay assertion prevents fabricating a race for a receipt
committed before revocation. The worker suite passed **54 tests, 10 ignored**,
and upgrades passed **47 tests**
([boundary log](factory-corrections-evidence/factory-barrier-prompt-race-final.log));
the expiry/reopen extension subsequently passed its focused test. All **10 factory
harness and 13 parallel memory-control tests** passed
([integration log](factory-corrections-evidence/factory-barrier-prompt-race-integration.log)).
Other post-effect generation changes and the full foreground cost audit remain open.

Task-contract document version 2 now requires an exact signed barrier release.
The reference carries the barrier ID, release event sequence and authorization
digest; signed release and inspection return this reference for downstream
contract authors. Existing launch approvals already bind the complete contract
digest, so no historical launch-input format or action hash was changed.
Preparation, reservation, launch claim and the final pre-effect approval check
revalidate the exact release ledger/event, current owner/config/control/store
identity, unrevoked status and current frozen member/memory evidence. Refusal
does not free an existing reservation's capacity. Shared read accounting and
the caller's SQL progress handler remain in force through nested barrier reads.

The focused boundary suite passed **106 tests**: 40 barrier, 4 contract-vector,
53 reservation and 9 dependency-satisfaction cases
([log](factory-corrections-evidence/factory-barrier-contract-boundary-final.log)).
New cases cover revoked barriers at all four launch boundaries, mismatched
release references, unsigned historical releases, changed authority/config/
member/control evidence, cancellation and preservation of the SQL callback.
The version-2 contract has a fixed raw-byte/digest fixture; version-1 fixtures
retain their prior bytes. These store fixtures model authenticated evidence;
the separate barrier service test exercises genuine owner signatures.
All **10 factory harness and 13 memory-control integration tests** passed
against this source state
([log](factory-corrections-evidence/factory-barrier-contract-integration.log)).
Urgent routing to already running downstream attempts and automatic wave
orchestration still require further corrections. The next result-boundary
correction is recorded below.

A new regression reproduced downstream result submission succeeding after its
required barrier was revoked. Result ingestion now checks exact required-release
applicability before object staging and again in the publication transaction.
Version-2 results must match the exact contract in their attempt's immutable
launch inputs; an unreserved attempt cannot borrow that contract's release.
The configuration file is rechecked as well as the retained control/config
identity. Verification rechecks before target loading and acceptance. Integration
rechecks verified-result lookup, new operation creation and pre-publication.
Historical receipts stay retained and replayable without authorizing new work.
Tests also mutate a source member without advancing the store head, exercising
the frozen evidence check independently of verification's existing event fence.
These changes do not yet supply transitive dependency invalidation, active
worker stop routing or full reconciliation coverage for a revocation racing
an external effect.

The [before-fix failure](factory-corrections-evidence/factory-barrier-result-before.log)
shows a new post-revocation submission being accepted. The corrected boundary
and verifier/integration regression suites passed **94 tests**
([log](factory-corrections-evidence/factory-barrier-result-final-tests.log)).
The new end-to-end store-boundary case uses synthetic trusted verification
receipts to isolate acceptance checks; it does not claim live worker execution
or verifier isolation. Existing verifier and integration tests run separately.
The factory harness passed all **10 tests**. The accompanying parallel memory
suite passed 12/13: the worker-update replay test failed acquiring an execution
lock at `tests/memory_control.rs:1329`
([original run](factory-corrections-evidence/factory-barrier-result-integration.log)).
The unchanged memory suite then passed **13/13 serially**
([serial run](factory-corrections-evidence/factory-barrier-result-memory-serial.log)).
The serial pass alone did not explain the parallel lock contention.
The subsequent lock investigation below records the reproducer and test-client
correction. Lock-transfer semantics were not weakened to make the test pass.

The deterministic [lock-inheritance regression](factory-corrections-evidence/factory-lock-inheritance-regression.log)
pauses a real child between fork and exec while a different thread drops its
project guard. Acquisition returns the typed `TryLockError::WouldBlock` until
exec closes the unrequested inherited descriptor, then succeeds. This proves
a mechanism consistent with the parallel acknowledgment failure; the original
failure did not capture its holder, so it is not retrospective proof of that
specific child's identity. Three unchanged parallel memory runs also passed
([recheck](factory-corrections-evidence/factory-memory-parallel-lock-recheck.log)),
consistent with transient timing rather than a persistent stale lock.

Execution-lock acquisition errors now include the lock path. The worker package
acknowledgment integration-test client retries only the typed acquisition
`WouldBlock` error, for at most two seconds. It still immediately returns
authority, database and receipt-validation errors, and leaves production lock
behavior and intentionally inherited ownership unchanged. All **4 guard tests**
passed, followed by **three parallel runs of all 13 memory integration tests**
([final log](factory-corrections-evidence/factory-lock-final-tests.log)).

The next ancestry regression showed that revoking a barrier still left its
consumer's verified result satisfying a dependency, and a later released wave
containing that result still passed applicability checks. Dependency publication
and consumption now follow the exact result/contract/attempt lineage to the
required release. Reusing a release follows its members' prerequisite releases
iteratively, checking current authority and frozen evidence at every step.
New freezes, release drafts and release execution also check these prerequisites.
Historical receipts and release replay remain intact without granting current
applicability. The regression refreshes the candidate release's expected head
after revocation to test ancestry independently of the optimistic event fence.

The graph traversal preserves the caller's shared read accounting, cancellation
and SQL progress handler. Standalone traversal owns a ten-second deadline and
50 MiB weighted input budget. It refuses more than 64 distinct release/authority
identities or 1,000 member references, and requires strictly descending release
sequences along prerequisite edges. These are conservative correctness bounds,
not completion of the history-independent runtime-cost requirement. Durable
invalidation routing and active projections still need to replace repeated
ancestry work for long-running factories. Unit dependency fixtures now retain
real parseable contract bytes/digests and exact submission parents; no production
validator bypass was added for the old incomplete fixtures.

The [before-fix reproduction](factory-corrections-evidence/factory-barrier-ancestor-before.log)
reports both dependency and later-wave checks incorrectly allowing reuse.
The corrected source passed **124 tests**: 44 barrier, 9 satisfaction,
53 reservation, 5 verification-store and 13 integration cases
([log](factory-corrections-evidence/factory-barrier-ancestor-final-tests.log)).
The subsequent integration run exposed a missing-result reporting regression:
a forged satisfaction without a verified receipt must remain a blocked dependency,
not fail the queue report. That behavior is preserved, while existing receipts
require their actual contract/submission lineage. A harness fixture now links its
synthetic successful runs to the already retained submission. The shared-budget
read-set path also explicitly refuses new freezes on schema 42 before upgrade.
After those refinements, **44 barrier, 9 satisfaction and 10 factory harness
tests passed**
([follow-up](factory-corrections-evidence/factory-barrier-ancestor-final-followup.log)).
That run also records a retryable project-lock failure at a memory promotion
test call. The promotion test client now uses the same bounded, typed acquisition
retry as acknowledgment, with production locking unchanged. The final parallel
memory suite passed **13/13**
([log](factory-corrections-evidence/factory-barrier-ancestor-memory-final.log)).

Direct release revocation now records each live consumer's invalidation in the
same transaction. Immutable attempt/release bindings survive later task contract
changes and worker retirement; readiness exposes a durable
`required_barrier_revoked` blocker after reopening the store. Reservation and
revocation failure-injection tests establish all-or-none publication. Repeated
revocation adds no duplicate invalidations, and capacity remains reserved until
termination is observed. The
[before-fix reproduction](factory-corrections-evidence/factory-barrier-live-consumer-before.log)
showed the missing durable blocker;
[the corrected case](factory-corrections-evidence/factory-barrier-live-consumer-after.log)
passes.

An indexed live-consumer projection excludes retired history. Admission rejects
the 1,001st live consumer of one release rather than admitting a population that
cannot subsequently be invalidated atomically. Retiring a consumer permits a
replacement; revocation invalidates all 1,000 remaining consumers without freeing
capacity. The synthetic routing workload used **608 SQLite VM steps and zero
full scans** with both zero and 10,000 retired consumers
([work log](factory-corrections-evidence/factory-barrier-live-consumer-work.log)).
These are SQL-work measurements, not latency, live-worker, or storage benchmarks.
The broader source state passed **180 tests**: 48 barrier, 10 memory-readiness,
53 reservation, 22 controlled-store and 47 upgrade cases
([log](factory-corrections-evidence/factory-barrier-live-consumer-final-tests.log)),
plus **10 factory harness and 13 memory-control integration tests**
([integration log](factory-corrections-evidence/factory-barrier-live-consumer-integration.log)).
Durable descendant propagation, actual worker stop delivery and reconciliation
of external effects remain open; ancestry validation still fences descendants.

A follow-up regression reproduced another bypass: an attempt reserved under a
version-2 contract could submit a result against a later version-1 contract and
lose its barrier prerequisite before revocation. The immutable attempt binding
now rejects that substitution for both live and terminated attempts. This closes
the version-1 early-return path while preserving legacy attempts without a
reserved barrier.
The [before-fix failure](factory-corrections-evidence/factory-barrier-contract-downgrade-before.log)
retains the incorrectly accepted submission.
The [focused regression](factory-corrections-evidence/factory-barrier-contract-downgrade-after.log)
passes for both live and terminated attempts. The subsequent **71 tests** passed:
49 barrier, 7 result, 5 verification-store, 1 integration-store and 9 satisfaction
cases ([log](factory-corrections-evidence/factory-barrier-contract-downgrade-final.log)).

The next [race reproduction](factory-corrections-evidence/factory-barrier-publish-race-before.log)
showed integration confirmation recording `integrated` after its required release
was revoked between the pre-publication check and the broker's observation.
Confirmation now rechecks applicability in the receipt transaction. A stale
prerequisite records `reconciliation_required`, leaves delivery ambiguous and
retains an immutable observation event identifying the commit, candidate, result,
repository and ref. It creates no integrated receipt or dependency satisfaction
and does not claim to undo Git's physical update. Other store errors propagate
without being mislabeled as revocation. Worker capacity remains unchanged.

The [final store-boundary regression](factory-corrections-evidence/factory-barrier-publish-race-final.log)
passes through both live-claim completion and recovery observation. Injecting an
observation-event failure rolls back the reconciliation state, feedback and
delivery change together; retry succeeds and reopening retains the exact observed
OID. This test models the trusted broker's observation with synthetic candidate
evidence; it does not demonstrate a real Git race or cover external launch/prompt
delivery reconciliation. Those operational boundaries remain outstanding.
All **72 boundary test executions** passed: 49 barrier, 13 integration-filter
cases (including the store migration case), that store case separately, and
9 satisfaction cases
([log](factory-corrections-evidence/factory-barrier-publish-race-boundaries.log)).
The following integration run passed **10/10 factory harness** cases and **12/13
memory-control** cases
([original log](factory-corrections-evidence/factory-barrier-publish-race-integration.log)).
The individual-acknowledgment replay failed with typed acquisition `WouldBlock`
on its disposable project's `effect.lock` at `tests/memory_control.rs:1307`,
consistent with the retained fork/exec lock reproducer above. That test's
individual acknowledgments now use the same bounded, acquisition-only retry as
its package acknowledgments. Production lock ownership and all non-acquisition
errors remain unchanged.
The corrected parallel memory suite passed **13/13**
([final log](factory-corrections-evidence/factory-barrier-publish-race-memory-final.log)).

The next [descendant regression](factory-corrections-evidence/factory-barrier-descendant-before.log)
confirmed that ancestry checks blocked reuse but left a released descendant's
inspection status unrevoked. Freeze now derives immutable transitive links from
its members' exact reserved prerequisites. A separate indexed projection routes
only applicable descendants. Direct release revocation materializes that set and
records each descendant's invalidation and live-consumer notices in the same
transaction, without recursive-trigger dependence or changes to historical
signed release records. Failed descendant publication rolls back the root and
all consumer changes together.

The [chain regression](factory-corrections-evidence/factory-barrier-descendant-chain.log)
passes for direct revocation, overlapping global memory invalidation, and the
memory-policy routing path. It covers a released child, a pending grandchild,
another pending descendant, the child's live consumer, immutable evidence,
restart persistence, retained capacity and original signed-release replay.
These are synthetic trusted store receipts and reservations, not live workers.
An overlapping batch can retain an additional cause event after an earlier root
already invalidated that descendant; the first applicability decision remains
unchanged.

Admission at freeze limits each barrier to 1,000 ancestors and each ancestor to
1,000 applicable descendants. The fanout test rejects a 1,001st descendant,
rolls back its partial freeze, and admits it after an existing descendant is
revoked. Revoked history leaves the routing projection. Revoking one root with
one applicable descendant used **1,189 SQLite VM steps and zero full scans** with
both **1,000 and 10,000 revoked descendants**
([work log](factory-corrections-evidence/factory-barrier-descendant-work.log)).
The synthetic copied barrier IDs in this population test are explicitly not
canonical freeze evidence. These work counts do not prove latency, combined
descendant/consumer fanout performance or live delivery. The existing bounded
ancestry rechecks remain; replacing their history-dependent work and delivering
worker stops are still outstanding.
The broader regression run passed **182 test executions**: 50 barrier,
10 memory-readiness, 53 reservation, 9 satisfaction, 47 upgrade and 13 integration
filter cases ([log](factory-corrections-evidence/factory-barrier-descendant-boundaries.log)).
The filters overlap on some upgrade cases; this is not a unique-test count or
an all-target validation claim.
The [final fanout regression](factory-corrections-evidence/factory-barrier-descendant-work-final.log)
also invalidates all 1,000 admitted descendants and the root's live consumer in
one statement. Its final work samples were **1,188 VM steps and zero full scans**
at both history sizes. An intervening run passed the fanout assertions but
[failed an exact-count equality assertion](factory-corrections-evidence/factory-barrier-descendant-work-equality-failure.log)
on 1,189 versus 1,188 steps. The regression now enforces an explicit 2,000-step
ceiling and zero full scans for each independent store, rather than incidental
instruction-count equality. This bounds the tested statement well below a
row-by-row traversal of 10,000 historical descendants; it is not a substitute
for the plan's prospective end-to-end performance gates.
The final **10 factory harness and 13 parallel memory-control integration tests**
passed ([log](factory-corrections-evidence/factory-barrier-descendant-integration.log)).

Barrier invalidation now atomically queues a pending stop obligation for each
live consumer without a preexisting cancellation. Controller polling services up
to eight obligations before dispatching effects, using a controlled two-second
connection and the original shared read budget through launch-input, binding,
ownership and consumer-binding reads. A persistent cursor advances before each
attempt, so a failed request remains durable without starving every later one.
The existing cancellation transaction consumes the pending obligation; a failed
transaction restores it. An existing operator request is preserved, and observed
termination removes a pending obligation without fabricating cancellation.

Cancellation uses the existing proof of no external launch before releasing a
never-claimed reservation. A previously claimed launch retains capacity; the
canonical worker reconciler still requires the recorded supervisor identity,
termination, workspace quiescence and preservation evidence. Unknown workers
remain unresolved. This connects durable invalidation to existing desired-stop
handling rather than treating invalidation as termination evidence.

The [initial stop-service tests](factory-corrections-evidence/factory-barrier-stop-service.log)
passed restart, failed-cancellation rollback, capacity retention, exact no-launch
proof, bounded batches, cancellation and rotation past failure. The subsequent
[boundary run](factory-corrections-evidence/factory-barrier-stop-boundaries.log)
passed 53 barrier, 53 reservation and 47 upgrade test executions. The sandboxed
controller binary run passed 12/31 and failed
19, including denied temporary Unix socket creation and cascading controller
failures. The controller and synthetic process-stop checks were then rerun with
local socket permissions in a user/PID namespace; their separate evidence follows.
That rerun passed the three stop-routing cases with shared read accounting and
30/31 controller cases
([log](factory-corrections-evidence/factory-barrier-stop-controller-fixture-failure.log)).
The remaining controller fixture stored opaque `x` bytes as its predecessor's
contract, which the strengthened result-provenance checks correctly rejected.
It now stores a parseable version-1 document, its exact SHA-256 and the actual
policy-body digest. Production validators were not weakened.
The corrected [controller suite](factory-corrections-evidence/factory-barrier-stop-controller-final.log)
passed **31/31**. Four existing
[supervised-process stop checks](factory-corrections-evidence/factory-barrier-stop-process-final.log)
also passed: desired stop while paused/revoked, leaving an uncancelled worker
alive, stopping before brief delivery, and recovery after a staged-stop commit
failure. They exercise synthetic local processes, exact ownership and retained
resources. The new barrier-to-cancellation tests and these process checks cover
the components separately; combined barrier-to-process recovery across adapters
and live worker certification remain outstanding.
The final **10 factory harness and 13 parallel memory-control integration tests**
passed ([log](factory-corrections-evidence/factory-barrier-stop-integration.log)).

The combined barrier-to-process regression exposed an additional defect: an
already consumed launch approval still allowed a new initial prompt after its
required barrier was revoked
([before-fix failure](factory-corrections-evidence/factory-barrier-combined-stop-initial.log)).
Worker-brief authority checks now revalidate the exact task contract and required
release at enqueue, claim and final pre-submission validation. The native brief
adapter uses one controlled connection with its original deadline, cancellation
identity and shared read budget for the latter two checks; nested barrier reads
do not install a new timeout or replace its SQL progress handler. Worker context
now reads the selected launch input, binding, ownership, task and attempt by ID.
Other inventory readers still need the full history-cost audit.

The [corrected combined regression](factory-corrections-evidence/factory-barrier-combined-stop-after.log)
starts an actual supervised local process with a barrier-bound reservation and a
synthetic native adapter. It covers revocation both before and after initial
prompt delivery, refusal of a new stale prompt, reopening and servicing the
pending stop, controller termination selection, and a failed stop-record commit
after the process physically exits. Capacity stays held after that failed commit;
retry records termination, retains resources/output and releases capacity.
Replays add no effects. The fixture models authenticated predecessor results and
release bytes; it does not certify real native servers, signature ingress or
provider workers. Revocation racing an in-flight prompt still needs dedicated
post-effect reconciliation coverage.

The subsequent [boundary suite](factory-corrections-evidence/factory-barrier-combined-stop-boundaries.log)
passed **53 canonical-worker, 53 reservation and 22 controlled-store tests**.
Ten opt-in worker/adapter cases remained ignored. No real worker certification
was inferred from these local process tests.
The final **10 factory harness and 13 parallel memory-control integration tests**
passed ([log](factory-corrections-evidence/factory-barrier-combined-stop-integration.log)).

- Signed contract dependencies remain authoritative in their retained signed
  bytes. Queue/contract disagreement blocks candidate selection and reservation
  and is visible in queue reports. New launch inputs bind the exact contract ID,
  revision and digest; approval action hashes include that binding. Reservation
  and launch approval validation recheck it. Historical inputs without a contract
  retain their serialized bytes and identities.
- Dependency publication and consumption check both the required policy ID and
  SHA-256 of the exact signed policy body. Backfill filters evidence by the same
  policy, including integrated results. Contract installation also attaches
  already retained evidence, and consumption rechecks older satisfaction rows.
  Dependencies may bind an explicit `policy_digest` for an independent producer's
  policy. Legacy documents derive it from their named acceptance policy. This
  permits producers to use the same policy name with different bodies without
  confusing either producer's evidence with the consumer's own output checks.
- Admission checks the selected profile against the signed contract's kind and
  required capabilities, using evidence for that exact native profile. An
  unsupported candidate does not prevent preparing the next supported candidate.
- Schema 42 makes unresolved wait replay monotonic rather than terminal. Replay
  reads at most 1,000 events per call, advances its durable cursor, deduplicates
  applied events and accepts only wakes addressed to its wait ID. A fulfilled
  wait stays terminal. Old unresolved schema-41 waits resume after explicit
  upgrade, including across restart. Upgrade also discards historical wakes that did not
  address the registration.
- Wait registration/replay and feedback-backed replan requests now have CLI
  entry points. Controller passes rotate over at most eight current-plan waits
  with a durable cursor, shared input budget and two-second control. Replay
  recognizes verifier/integration receipts for the appropriate task/attempt and
  signed dependency policies, and writes one advisory inbox notice atomically
  with progress. Completion before registration is handled transactionally;
  late contract/queue events recheck earlier receipts. A regression caught and
  fixed a premature dependency wake when legacy evidence existed without a
  signed consumer contract. Wake notices do not release capacity or grant launch
  authority. [Wait workflow](../factory/waits.md) documents current producers and
  the still-missing typed-trigger/resource/adapter/user-decision and automated planner
  paths; this is not completion of F2.4.
- Wait registration accepts an optional immutable deadline on schema 43, exposed
  as an RFC 3339 `--deadline` in the CLI. Its identity includes the deadline while
  no-deadline identities remain unchanged. Expiry produces a durable addressed
  advisory wake when serviced, bypassing unrelated event backlog; it neither
  proves the condition nor releases capacity. Notification failure rolls the
  deadline event back. All 28 plan/wait tests and the real CLI registration/replay
  case passed (`/tmp/factory-wait-deadline-tests.log`,
  `/tmp/factory-wait-deadline-cli.log`), followed by all 44 upgrade tests
  (`/tmp/factory-wait-deadline-upgrades.log`). This is bounded controller service, not
  a real-time timer; automatic subscription renewal remains open.
- Automatic replan requests now deliver one durable `replan-request` inbox item
  in the same transaction as budget consumption and feedback acknowledgment.
  Replaying an older request can restore its missing notice. A schema-43 proposal
  using the request ID as its key records an immutable response link and completes
  the notice atomically. Stale requests cannot rebase, changed response bytes
  conflict, and an unrelated preexisting proposal cannot acquire response status.
  Request delivery and response acceptance remain advisory; signed contracts and
  launch authority are separate. Automated planner-process dispatch remains open.
  Validation: 65 plan/upgrade tests passed (`/tmp/factory-replan-final-tests.log`)
  and all 10 factory harness tests passed (`/tmp/factory-replan-harness-tests.log`).
- Verification initializes its disposable Git repository with the bound object
  format, including SHA-256.
- Migration fixtures now reconstruct the actual historical migration prefix and
  copy shared historical columns, instead of maintaining lists of newer tables
  to drop. Current-version assertions use `SCHEMA`; historical-version
  assertions remain explicit. Fixed shared-counter and test-source inspection
  failures without changing the production admission default.
- Schema 43 adds indexed admission pages of 64 candidates, with a durable scan
  cursor that survives unrelated head changes and restarts. Each wake scans one
  page, so unsigned candidates cannot permanently hide a later authorized task.
  Admission and reservation use selected tasks, bindings, approvals and launch
  inputs rather than historical snapshots. Cancellation now uses those same
  scoped validators and indexed checks for other retained attempts and ownership;
  it retains the existing never-claimed release conditions.
- Pending verification is maintained transactionally by submission/run triggers
  and backfilled on upgrade; successful and rejected runs both finish verifier
  work. Original admission cancellation/deadline controls now reach SQL and Git
  ancestry checks. Aggregate allocation accounting and whole-controller
  measurement still need work before declaring the hot path bounded.
- Admission now shares an input/JSON-structure budget across profile inventory,
  candidate tasks and bindings, contracts, dependency checks, approvals, held
  resource claims and reservation validation. These payloads are charged before
  application copies/decoding. Candidate contracts are reused during profile
  matching; capability reads return distinct levels rather than decoding every
  retained observation; held claims are loaded once per candidate page.
  Inactive/full projects skip profile inventory. Reservation and cancellation
  reconcile memory consumer routes only for the task whose state changed, using
  a new partial index. Ancillary/scalar budget coverage and whole-controller telemetry
  still need auditing; this is not a claim of total-heap or latency certification.
- Factory status no longer mislabels a separate inventory-page read as total
  decision work. Its count is named `active_inventory_page_rows`, and the
  unmeasured `rows_decoded` field is `null`. Status uses the pending-verification
  index on schema 43 and handles schema 26 without querying the then-absent
  verification-run table.
- Actual admission calls now return observations on success and error, and the
  controller includes them in its sanitized ticker logs. A connection-local
  SQLite callback counts returned-row notifications and VM steps from completed
  or reset statements, including interrupted queries. Reused statements reset
  their step counter each execution. Opening checks after hook installation are
  included; filesystem/Git work and the earlier enabled probe are excluded.
  Opening failures retain observations, and calls ending before attachment are
  explicitly marked unobserved. No SQL text or row values are logged. These are
  SQL work measurements, not decoded-object counts or heap measurements; status
  does not substitute this sample for a complete controller metric.
- Fixed a separate migration defect exposed by CLI validation: schema 34 omitted
  the already-supported `admission` denial class. Migration 34 now preserves it
  for older stores upgrading through that migration, and 43 repairs the published
  constraint for stores already at versions 34–42. Upgrade regressions cover both.
- Factory packaging now uses `scripts/build-factory`: a host-native Linux build
  under a separate target directory, followed by a new checksummed package with
  build metadata. It preserves the existing plugin executable and prior package
  directories. `build-info` reports features/platform/schema/SQLite without
  resolving config, projects or sessions; `--require-factory` refuses incompatible
  feature/platform/library combinations. Doctor's build guidance also uses a
  separate target directory. The packaging guide now covers explicit selection,
  opt-in migration, the current schema, and recovery/rollback boundaries.
- Delegation now accepts an explicit version-2 signed reservation scope with
  exact task-contract, profile and budget references, object-format-bound
  repository bases, and a finite lifetime-attempt limit consistent with its
  concurrency limit. Missing, duplicate, unsorted, widened and malformed bounds
  are refused; stored raw bytes retain every bound. Version 1 is not silently
  promoted, and version 2 still cannot act as a launch approval by itself.
  [Contract mapping and byte vector](../../contracts/factory/delegation.md)
  document the boundary. Authenticated attempt-specific authority derivation,
  atomic reservation/use accounting and revocation-at-claim wiring remain open;
  this does not yet fix the non-reserving delegation stub. Its denial path now
  reads only the event head instead of decoding an unrelated historical snapshot.
- The integrated scale fixture now crosses 32/64 active workers with 1,000/100,000
  events and 256/1,024 additional terminated attempts, all alongside 10,000 retired
  bindings. This schema requires a terminated attempt per retired binding; those
  10,000 supporting attempts are retained and counted separately. The same store
  exercises restart, false satisfaction, incomplete coverage, cancellation and
  capacity release before five measured unsigned admission decisions. Assertions
  compare actual SQL work across history sizes. This remains a partial scale
  workload: multiple waves, signed reservation, slow endpoints, integrated memory
  traffic and complete latency/freshness distributions are not yet covered.
- Controlled reconciliation and canonical observation reads now use a scoped
  opener that retains publication, file identity and deadline checks, without
  scanning whole-store integrity on each poll. Administrative opens still perform
  full integrity checks. The synchronous controller schema guard now uses this
  scoped opener with a two-second deadline. Controlled observation publication
  also uses it; synchronous polling carries its existing probe deadline through
  controlled collection, publication and claim expiry. Observation publication reads only tasks
  referenced by its binding inventory and uses indexed binding/observation
  lookups instead of nested searches. Focused library validation passed (10
  tests), along with 31 controller tests and the two existing observation/rebind
  fence regressions. Ownership rows represent outstanding claims, including
  terminated attempts whose resources have not been relinquished; those rows
  remain required observations. Their decode costs, active inventory IDs and
  selected task rows now share the controlled publication read budget. A claim
  for a different binding generation invalidates control instead of silently
  skipping that check. Seven focused tests passed, including atomic rollback on
  oversized ownership and stale-generation invalidation
  (`/tmp/factory-observation-ownership-tests.log`); all 31 controller tests also
  passed (`/tmp/factory-ownership-controller-tests.log`). Remaining read-budget
  gaps still prevent claiming a fully bounded complete controller turn.
- Schema 43 now maintains active-work candidate membership with transactional
  triggers on binding, attempt and ownership changes. Its explicit migration
  backfills the candidates once; ordinary projection rebuilds use an indexed
  candidate outer loop instead of visiting every retired binding. Raw lifecycle
  changes invalidate the projection in the same transaction. The new measured
  rebuild regression has identical SQLite row/VM-step counts with zero versus
  10,000 retired bindings after changing an active attempt. Attempt deletion,
  rollback, still-owned terminated resources and schema-42 backfill are covered.
  All 55 active-work/upgrade tests passed
  (`/tmp/factory-active-candidates-upgrades.log`), followed by all 10 integrated
  factory harness tests (`/tmp/factory-active-candidates-harness.log`). Broader
  validation also passed: 653 library tests (13 ignored) and 541 binary tests.
  [Retained log](factory-corrections-evidence/candidate-index-library-binary-tests.log).
  This does not substitute for
  the outstanding multi-wave, full-controller latency and recovery gates.
- Active projection queries, selected runtime bindings, retained source bytes,
  ownership payloads and encoded attempt lists now charge the controlled store's
  shared read budget before application copies or JSON decoding. Observation
  publication carries that same budget through projection synchronization and
  binding validation, including its historical-schema reader. Repeated reads
  cannot renew the allowance. Oversized binding tests verify both collection
  refusal and atomic rollback of observation publication. All 37 focused tests
  passed (`/tmp/factory-active-read-budget-tests.log`), followed by 31 controller
  tests (`/tmp/factory-active-read-budget-controller.log`) and all 10 integrated
  factory harness tests (`/tmp/factory-active-read-budget-harness.log`).
- Routine planning now reads the latest revision of each routine under the
  original SQL deadline and shared allocation budget, retaining at most 128
  current definitions. It preserves name rotation, disabled-revision withdrawal,
  and the original head fence across script validation. Historical payloads and
  the rest of the project snapshot are not decoded for selection. Both background
  and synchronous controller scheduling now use that bounded selector.
- Routine overlap checks now use a schema-43 work index. Delivery or completion
  changes reinsert an operation, including changes to old cleanup evidence.
  Scheduling validates each selected occurrence, outbox intent, delivery and
  receipt before removing a retired entry. It processes at most 64 entries per
  wake; incomplete scans persist progress without authorizing a command. A
  shared selection/scheduling budget charges payloads before decoding, and large
  valid output receipts cause an earlier yield rather than endlessly retrying
  an oversized page. Explicit upgrade validates historical routine evidence once
  before pruning the newly populated index. Uncertain execution still blocks
  overlap across revisions, generic outcomes and restarts.
- Routine claim checks and completion now validate the exact occurrence,
  definition, cursor and outbox operation. Completion checks for an existing
  receipt by operation ID and preserves atomic delivery/inbox/event updates.
  Unrelated historical routine payloads no longer prevent recording cleanup of
  an already-owned effect. Administrative snapshots still validate that history.
- Administrative budget reports again validate the full immutable policy
  history, while admission uses the current policy. The full-library regression
  exposed and verified this distinction after the scoped-reader change.

## Validation so far

- The five original review probes pass.
- Added cases cover changed contract after approval, profile-kind/capability
  routing, matching policy, same policy name with different body, late contract
  installation, and paged wait replay after upgrade/restart.
- Library suite after these changes: **616 passed, 0 failed, 13 ignored**
  (`/tmp/factory-fix-library-4.log`), before the schema-43 changes.
- The earlier all-target run (`/tmp/factory-fix-all-2.log`) finished with failures:
  seven binary tests, one CLI test and three factory harness tests. Five binary
  failures involved inaccessible unrelated `/proc` entries during writer
  quiescence checks; two involved copy-source stability and a lock race. These
  require investigation; no production quiescence check has been weakened.
- Current targeted results: admission **26 passed**
  (`/tmp/factory-admission-tests-5.log`), reservation/cancellation **53 passed**
  (`/tmp/factory-reservation-tests-2.log`), factory harness **10 passed**
  (`/tmp/factory-harness-fix-2.log`), and the formerly failing signed admission
  CLI denial test **1 passed** (`/tmp/factory-cli-admission-fix.log`).
  The harness fixtures now use full normative contracts and explicit producer
  policy digests, and wait assertions require a correctly addressed wake.
  The disk-full fixture forces allocation beyond available reusable pages;
  creating a small table alone no longer reliably fills a migrated database.
- Scoped opener publication/cancellation/size-limit tests: **2 passed**
  (`/tmp/factory-scoped-open-tests.log`); binary reconciliation tests:
  **3 passed** (`/tmp/factory-reconcile-tests.log`). Pending-verification
  backfill/rejection/transaction-rollback regression: **1 passed**
  (`/tmp/factory-pending-projection-tests.log`).
- Routine suite with scoped selection/claim/completion changes: **23 passed**
  (`/tmp/factory-routine-tests-3.log`). The full library run finished
  **625 passed, 2 failed, 13 ignored** (`/tmp/factory-library-current.log`).
  That run predates the exact routine claim/completion change. Its budget-report
  failure was fixed and its regression passes (`/tmp/factory-budget-recheck.log`).
  The retained-profile revalidation lock conflict passes in isolation
  (`/tmp/factory-profile-lock-recheck.log`); it is not evidence of a clean parallel
  suite. The real verifier accepted path also passes with pending-job assertions
  (`/tmp/factory-verifier-pending-accepted.log`).
- Updated routine suite after paged overlap, shared budgets and synchronous
  controller selection: **28 passed** (`/tmp/factory-routine-overlap-tests-4.log`).
  Cases include a blocked 65th entry, restart between pages, maximal valid output
  receipts, exhausted-budget rollback, upgrade pruning and corrupt-evidence
  upgrade rollback. Historical migration tests: **43 passed**
  (`/tmp/factory-upgrade-current.log`) after this schema change.
- Canonical controller integration tests: **10 passed**
  (`/tmp/factory-controller-current.log`), covering signed routine rotation,
  one dependent reservation per poll, ambiguous effects, claim expiry, and
  provenance/observation boundaries.
- After admission budget propagation: admission **27 passed**
  (`/tmp/factory-admission-budget-tests-4.log`), reservation **53 passed**
  (`/tmp/factory-reservation-budget-tests-2.log`), capability **8 passed**
  (`/tmp/factory-capability-budget-tests.log`), consumer routing **6 passed**
  (`/tmp/factory-consumer-scoped-tests-2.log`), and status **3 passed**
  (`/tmp/factory-status-metrics-tests.log`). Dense profile/binding payloads exceed
  the shared budget without changing the event head or creating attempts.
  The real policy-body/late-contract verifier regression also passes
  (`/tmp/factory-policy-budget-recheck.log`). The combined factory harness:
  **10 passed** (`/tmp/factory-harness-budget-tests.log`).
- Earlier binary failures rechecked: **6 cleanup tests passed** and the retained
  branch preservation scenario passed in a user/PID namespace with its own
  `/proc` (`/tmp/factory-cleanup-isolated-tests.log`,
  `/tmp/factory-preservation-isolated.log`). This avoids unrelated inaccessible
  host processes without bypassing production writer-quiescence validation.
  Copy recovery and PR polling each passed standalone
  (`/tmp/factory-copy-recheck.log`, `/tmp/factory-pr-recheck.log`).
  Broad-suite stability remains to be verified; standalone success does not
  explain or erase the earlier parallel-run failures.
- Admission telemetry validation: admission-filtered library tests **27 passed**
  (`/tmp/factory-admission-metrics-tests.log`), controlled-store tests **18 passed**
  (`/tmp/factory-controlled-metrics-tests.log`), controller integration tests
  **10 passed** (`/tmp/factory-controller-metrics-tests.log`). The final dedicated
  SQL-work run **4 passed** (`/tmp/factory-sql-work-tests-2.log`), including an
  added aggregate-query case: returning one row does not hide increasing VM
  work. Other cases cover reused statements, connection isolation, opening
  failure, cancellation, failed budget accounting and successful reservation
  logging. State-store `cargo check` also passed. This is focused validation,
  not a new all-target or scale certification result.
- Expanded factory harness: **10 passed**, including all eight combined scale
  cases. [Raw harness log](factory-corrections-evidence/integrated-harness.log),
  [40 admission samples](factory-corrections-evidence/scale-admission-inventory.jsonl)
  and [source/hardware manifest](factory-corrections-evidence/scale-admission-manifest.json)
  are retained. At fixed worker count the SQL row and VM-step counts were equal
  across the tested event/attempt histories. These samples follow one cancellation,
  leaving 31/63 capacity-retaining attempts; they measure unsigned selection,
  not worker launch. No prospective latency gate or baseline comparison is claimed.
- Packaging: a clean separate-target release build completed, the package's
  checksums passed, and the prior plugin binary's SHA-256 stayed unchanged.
  Both default/state-store `build-info` CLI probes passed, as did both doctor
  CLI checks. `scripts/test-factory-package` reproduced legacy-project readability
  with the prior binary and unchanged project bytes during inspection. It also
  verified refusal with interposed older SQLite version functions and a mocked
  non-Linux build preflight. [Package evidence](factory-corrections-evidence/packaging-check.json)
  distinguishes these refusal probes from actual old-library/macOS certification.
  No actual project was migrated; interrupted migration and canonical recovery
  still require their full acceptance evidence.
- Wait wiring: **21 wait/replan tests passed**
  (`/tmp/factory-wait-service-tests-2.log`), **11 controller tests passed**
  (`/tmp/factory-wait-controller-tests.log`), and the new CLI workflow test passed
  (`/tmp/factory-wait-cli-tests.log`). The real verifier policy regression passed
  with wrong policy/body, late-contract-after-replay and late-wait cases
  (`/tmp/factory-wait-policy-tests-5.log`). All **43 migration tests passed**
  (`/tmp/factory-wait-upgrade-tests.log`). Notification failure rolls replay back;
  durable rotation and duplicate notices are covered across restart.
  The accepted-verifier/late-registration probe also passes with an explicit
  post-wake retained-capacity assertion
  (`/tmp/factory-wait-late-registration-tests-2.log`). Combined factory harness
  after this wiring: **10 passed** (`/tmp/factory-wait-harness-tests.log`).
- Delegation scope validation: **15 tests passed**
  (`/tmp/factory-delegation-scope-tests-3.log`). Cases cover a fixed byte/digest
  vector, version separation, SHA-1/SHA-256 pins, absent/duplicate/widened bounds,
  real Ed25519 signature rejection after changing limits, exact storage retention,
  and denial-head recording despite unrelated corrupt historical bindings.
  State-store `cargo check` also passed before these test additions.
- Subsequent scan changes narrow prerequisite lookups to exact task IDs and
  retained-claim reads to attempts with unobserved termination. Delivery lookup
  uses an index in memory instead of a nested scan. The preliminary controller
  guard now reads only the schema header it consumes; scoped reconciliation and
  effect selection retain validation of their selected rows. These are partial work on
  finding 3, not evidence that the full controller hot path is bounded.

## Remaining work from the review

Barrier inspection found three further gaps before production release wiring:
an old contract could still authorize release after a newer revision existed;
release did not compare the current memory manifest with the frozen digest; and
the selected verifier policy was not checked against the retained signed bytes.
All three new probes failed on the previously corrected worktree
([before-fix log](factory-corrections-evidence/barrier-evidence-before-fix.log)).
Freeze/release now parse and bind the latest signed contract and its route,
validate the exact submission/run/result provenance and named policy digest,
and require an accepted integration operation when the contract calls for one.
Release recomputes the existing frozen memory manifest before publishing a
receipt. The proposal/head/member readers now use bounded SQL lookahead before
collecting their rows. Barrier test fixtures use valid retained contract bytes
and consistent digests instead of placeholder `x'61'` documents; their seeded
verification records remain store fixtures, not live verification evidence.

All **75 barrier, memory-readiness, dependency and upgrade cases passed**
([boundary log](factory-corrections-evidence/barrier-final-boundary-tests.log)),
including refusal of six mismatched provenance variants at both freeze and
release. All **10 factory harness tests passed**
([harness log](factory-corrections-evidence/barrier-evidence-harness-tests.log)).
The default-feature build also passed
([build log](factory-corrections-evidence/barrier-evidence-default-check.log)).
The existing deterministic release token is not a substitute for authorization.
The subsequent owner-signed service described below supplies explicit release
policy/current-authority checks. Downstream admission binding and automated
orchestration remain open.

The subsequent memory read-set audit reproduced three additional release
bypasses: memory applied after freezing, new contract scope entries, and changed
task revisions. The [version-2 read set](../../contracts/factory/barrier-memory-v2.md)
now binds member identity, consumed revisions and transitive sources, required
heads, scope generations, latest memory policy, and store/control identity.
It retains immutable bounded payload bytes atomically with the freeze. Legacy
pending barriers require a new freeze; migration preserves already released
receipts and never fabricates historical read sets. Unconsumed optional noise
does not invalidate a freeze.

All **23 barrier tests passed**
([log](factory-corrections-evidence/factory-barrier-read-set-final-store-tests.log)),
including a fixed byte/digest vector, an 8-MiB
payload refusal, publication rollback, immutable storage, source/policy changes,
and a genuine schema-42 upgrade. The preceding combined boundary run passed
**82 barrier, readiness, dependency and upgrade tests**
([log](factory-corrections-evidence/factory-barrier-read-set-boundary-tests.log)).
The final integration run passed **10 factory harness and 13 memory-control
tests** ([log](factory-corrections-evidence/factory-barrier-read-set-integration.log));
the default-feature build also passed
([log](factory-corrections-evidence/factory-barrier-read-set-default-check.log)).
The three original read-set regressions are retained in the
[before-fix log](factory-corrections-evidence/factory-barrier-read-set-before-fix.log).
These establish local
store behavior, not release authorization or a live F3 gate.

The [owner-signed barrier workflow](../factory/memory-barriers.md) now supplies
production freeze, inspection, authorization-draft and release ingress. Exact
raw signed bytes bind the frozen barrier/memory digest, store incarnation,
owner-policy reference, configuration digest, control revision/epoch, expected
head and expiry. Only `all_members_ready_v1` is accepted; it cannot waive member
evidence, proposal dispositions or memory readiness. Real Ed25519 verification
uses the separate `barrier-release@herdr-projects` namespace. Consumed object
bytes are checked before the final transaction, which revalidates current
control and frozen evidence and publishes the release and immutable signed
authorization together. The clock is read after obtaining the write lock.

The fixed authorization byte vector and real-signature tests reject changed
evidence, wrong keys/namespaces, mismatched authority and unsupported fields.
Transaction tests cover exact replay, changed-byte refusal, 15 stale/invalid
fence cases, immutable receipts and rollback when authorization publication
fails. A real migrated-project service test drafts, signs, releases and replays
without releasing attempt capacity. Its result/verification rows remain seeded
fixtures. These checks do not establish live F3 acceptance, automatic reviewer
orchestration, downstream reservation binding or urgent downstream invalidation.

The final authorization run passed **74 boundary and upgrade tests**
([log](factory-corrections-evidence/factory-barrier-authorized-final-boundary.log)),
including expiry during a held SQLite write lock and confirmation that upgrading
historical releases invents no authorizations. The **10 factory harness and
13 memory-control integration tests** passed
([log](factory-corrections-evidence/factory-barrier-authorized-integration-tests.log)),
as did **9 documentation tests**, including refusal to deserialize the trusted
release capability
([log](factory-corrections-evidence/factory-barrier-authorized-doc-tests.log)).
The default-feature build passed
([log](factory-corrections-evidence/factory-barrier-authorized-default-check.log)).
The compiled CLI help exposes the four new commands with the documented
arguments. Full live CLI/worker certification remains unperformed.

The next invalidation audit reproduced a store defect: a released barrier could
never be revoked, even when retaining its release as history was appropriate.
Revocation now appends an immutable `barrier_release_revocations` record linked
to a subsequent exact `barrier.revoked` event. The schema-40 release header and
signed authorization stay unchanged. The `barrier_current_status` view and
store inspection expose both sequences. Dependency checks use this current
status and reject revoked superseding releases as well. Rollback, replay,
receipt immutability, stale-brief refusal, preserved attempt capacity, and
historical schema-42 upgrade are covered. Explicit authorized revocation ingress
remains open. Next-wave reservation binding is supplied by the version-2
contract correction above; automatic routing of new memory invalidations is
implemented in the subsequent correction described below.
The [before-fix regression](factory-corrections-evidence/factory-released-barrier-before-fix.log)
failed on the explicit refusal to revoke a released barrier. The corrected
implementation passed **89 barrier, satisfaction, memory-readiness and upgrade
tests** ([log](factory-corrections-evidence/factory-released-barrier-final-tests.log)),
including rejection of unrelated or pre-release events as revocation evidence.
All **10 factory harness tests** also passed
([log](factory-corrections-evidence/factory-released-barrier-harness.log)).

Automatic memory-to-barrier invalidation is now connected at the database
boundary used by both promotion and delivery routing. A new unresolved blocking
invalidation emits cause-linked revocation events for the task's applicable
memberships, or all applicable memberships for a global invalidation. Each event
updates pending/released status and removes routing membership atomically.
Informational invalidations and duplicate insertion do not revoke or emit new
events. A failed revocation rolls back the invalidation and every affected
barrier transition. Current routing membership cannot be silently deleted or
rewritten. Resolving the original invalidation does not restore the barrier.

The routing projection excludes revoked history. At that source state, the measured statement used
**442 SQLite VM steps and zero full-scan steps** with both zero and **10,000
revoked historical barriers**. This measures that statement's SQL work, not
whole-controller latency or storage I/O. Runtime fan-out above 1,000 barriers
fails before publishing an invalidation; exactly 1,000 revocations complete
without skipped candidates as routing rows disappear. Upgrade rebuilds the
projection and records new migration-time decisions for retained unresolved
invalidations, preserving release history rather than inventing past decisions.
Automatic downstream reservation/effect fencing remains open. This first routing
correction consumed canonical invalidation rows; direct record/policy coverage
for retired consumers is supplied by the subsequent correction below.

The [before-fix test](factory-corrections-evidence/factory-barrier-auto-invalidation-before.log)
reproduced the missing automatic revocation. The corrected boundary/upgrade
suite passed **93 tests**
([log](factory-corrections-evidence/factory-barrier-auto-final-boundary.log)).
The [routing-budget test](factory-corrections-evidence/factory-barrier-auto-routing-budget.log)
retains the measured SQL-work pair and fan-out boundary. The refined
[resolution test](factory-corrections-evidence/factory-barrier-auto-resolution-test.log)
also confirms that clearing the memory invalidation does not revive the barrier
or allow its dependency satisfaction to be republished.
All **10 factory harness and 13 memory-control integration tests** passed
([log](factory-corrections-evidence/factory-barrier-auto-integration-tests.log)).

A further regression showed that revoking a transitive source left a released
barrier applicable when no active worker binding received an invalidation.
Frozen record-dependency routing now derives indexed record IDs from the exact
version-2 read set, including transitive sources and required heads. The index
outlives worker retirement and is removed only on barrier revocation. Direct
record publication, record-policy application, dependency invalidation and
memory-policy installation invoke the bounded barrier invalidator inside their
existing transactions. Transport redelivery alone does not invalidate evidence.
New contracts, hard rules and policy changes conservatively invalidate all open
barriers, matching the frozen global catalog/policy vector. Optional changes
invalidate only barriers that consume them, plus legacy barriers whose old
read sets cannot prove irrelevance.

Tests cover actual worker-binding retirement, transitive source revocation,
unchanged redelivery, unrelated optional publication, consumed-head changes,
contract phantoms, policy changes, legacy fallback, projection guards and cleanup,
and rollback of both source changes and revocations on failure or fan-out overflow.
The expanded boundary suite passed **109 tests**. With the new projection cleanup,
the earlier invalidation statement now uses **468 VM steps and zero full scans**
at both zero and 10,000 revoked historical barriers. These measurements remain
SQL-work checks, not full controller latency certification.
The complete direct-source-revocation transaction also used **1,316 SQL progress
steps** at both history sizes. A further fan-out audit moved the complete source
check before per-derived-record invalidation: otherwise two individually bounded
derived batches could remove their routing rows before the source-wide limit
was checked. The focused regression creates 501 barriers for one derived record
and 500 for another, all consuming the same source, and proves that all 1,001
remain unchanged when publication is refused. Required derived heads trigger
the conservative global check before any derived barrier is removed.

Retained evidence: [original retired-source failure](factory-corrections-evidence/factory-barrier-retired-source-before.log),
[boundary suite and SQL-work measurements](factory-corrections-evidence/factory-barrier-retired-final-boundary.log),
and [transitive fan-out regression](factory-corrections-evidence/factory-barrier-transitive-fanout.log).
The final source state also passed **10 factory harness and 13 memory-control
integration tests**
([log](factory-corrections-evidence/factory-barrier-retired-integration-tests.log)).
At that source state, downstream reservation binding and effect fencing remained
open. The version-2 contract correction recorded above supplies launch-boundary
binding; urgent downstream routing and result fencing remain open. None of these
local checks supplies live F3/F5 certification.

Package pulls now accept repeated `--change` selectors for exact unresolved
batches. The selected path uses indexed obligation lookups and validates the
entire selection before publication. This lets a current revision be applied
without requiring a false first-applied receipt for an obsolete revision in the
same backlog. Unselected obligations and mandatory reconciliation blockers
remain intact. Optional supersession now uses the distinct protocol described
below; mandatory and successor reconciliation remain separate requirements.

Repacking also exposed an accounting defect: application retargeted logical
seen receipts, which could invalidate an earlier package's supporting evidence
and prevent narrower packages from reusing identical receipts. Package replay
now reads its own immutable acceptance event, bounded by the kind/entity index.
Logical receipts preserve their original source package and sequence. Schema 43
rejects all updates to those rows and preserves historical retargets verbatim.
New explicit package acceptance can reuse identical receipts without creating
new logical consumption. The real signed-promotion/CLI regression applies the
current revision, replays both wider and selected package evidence, and proves
the obsolete revision has no applied receipt and both invalidations still block.
Historical schema-42 retargeted receipts also replay after upgrade with no new
events or invented worker evidence.

Validation: **68 store/upgrade cases and three memory integration cases passed**
([boundary log](factory-corrections-evidence/package-selection-boundary-tests.log)).
After the final event-envelope validation, **all 12 memory integration tests and
10 factory harness tests passed**
([integration log](factory-corrections-evidence/package-selection-integration-tests.log)).
The final six package-store cases also passed
([store log](factory-corrections-evidence/package-selection-final-store-tests.log)),
as did the default-feature build
([build log](factory-corrections-evidence/package-selection-default-check.log)).

Optional supersession now has a production `memory PROJECT supersede-update`
service and exact `supersession --binding ID --change ID` inspection command.
An immutable schema-43 ledger and `memory.update_superseded` event bind the old
delivery to a newer revision of the same record, the current worker generation,
its exact individual applied receipt and an explicit reason. The body is verified
before the write transaction rechecks identity, head, validity, expiry and source
dependencies. The old delivery must be informational and the record non-hard;
mandatory deliveries are refused. Package pulls exclude accepted supersessions
without deleting obligations or creating applied coverage for the old revision.
No invalidation, existing receipt or attempt capacity changes. Inspection retains
the evidence after worker retirement; duplicate acceptance is limited to the
still-current worker and the exact original request. Upgrades invent no entries.
This implements optional coalescing within one generation. Successor transfer
and mandatory reconciliation remain open; this does not establish F3 certification.
Validation: **68 store/upgrade cases and four memory integration cases passed**
([boundary log](factory-corrections-evidence/supersession-boundary-tests.log)),
followed by **all 13 memory integration and 10 factory harness tests**
([integration log](factory-corrections-evidence/supersession-integration-tests.log)).
The final expiry-clock and historical-fixture refinements passed the signed
promotion/CLI regression again
([final protocol log](factory-corrections-evidence/supersession-final-boundary.log)).
It covers missing/seen-only evidence, wrong worker/manifest/binding, hard and
mandatory changes, expiry/revocation/head movement, corrupt content, publication
rollback, exact replay, preserved blockers and immutable audit after retirement.
The default-feature build also passed
([build log](factory-corrections-evidence/supersession-default-check.log)).

Worker package acceptance now retains its supporting protocol evidence, rather
than relying only on a check of mutable current-worker identity at acceptance.
The schema-43 `worker_package_acknowledgments` index links a package/attempt/state
to an immutable `memory.worker_package_ack` event and package receipt sequence.
The event records the binding generation and every exact source change receipt's
manifest and sequence under `worker-change-declaration-v1`. CLI responses expose
that evidence sequence. Publication is atomic with package accounting; replay
checks the retained evidence and does not append another event. It remains
auditable after worker replacement. This records worker declarations, not new
validation or adapter certification.

The evidence-write fault test rolls back package events and receipts together;
replacement cannot rewrite or delete the retained evidence. Upgrading a genuine
schema-42 fixture preserves existing package receipts and leaves the evidence
index empty. **67 store/upgrade cases and three memory integration cases passed**
([boundary log](factory-corrections-evidence/worker-package-evidence-boundary-tests.log)).
The default-feature build passed
(`/tmp/factory-worker-package-evidence-default-check.log`).

Worker package acknowledgment now has a production service and
`memory PROJECT package-ack --attempt ID --input ack.json` command. It requires
an existing exact-change receipt for every package member under the current live
attempt and binding. The service checks body availability/hash with a shared
50-MiB allowance; the write transaction rechecks the generation, attempt,
snapshot, manifest and supporting receipts before updating package accounting.
These remain worker declarations, not validation evidence or authority to clear
invalidations, release capacity or acknowledge a successor/coordinator binding.
There is no raw unchecked package-ack CLI.

All **12 memory-control integration tests passed**, with the real CLI covering
package seen/applied, duplicate acceptance, missing supporting receipts, partial
membership, rollback, corrupt bodies and replaced-attempt refusal
([integration log](factory-corrections-evidence/worker-package-memory-tests.log)).
All **25 package, receipt and barrier store tests passed**
([store log](factory-corrections-evidence/worker-package-store-tests.log)).
The state-store and default-feature build checks passed (default log:
`/tmp/factory-worker-package-default-check.log`). Automated delivery, certified native-adapter
and coordinator protocols, successor-generation handling, reviewer orchestration
and barrier orchestration still require completion and live F3 evidence.

Delegated reservation now has an exact subject-signed production service and
`delegation PROJECT import|draft|reserve|revoke` commands. A v2 grant bounds the
signed contract, profile, budget and repository bases; the subject signs an exact
incarnation/head-bound request under `delegated-reservation@herdr-projects`.
The ordinary reservation transaction derives one action-specific approval and
commits its immutable request/quota ledger with the attempt, task and operation.
No raw delegation is treated as an owner launch approval. Claim/use rechecks
scope, expiry, revocation and incarnation. Confirmed termination frees only
concurrent capacity; lifetime use is not refunded. Repeated accepted requests
return the retained response without effects, and changed bytes conflict.
The legacy `reserve_attempt` method remains an inspection-only compatibility
probe; the new production entry is `reserve_delegated`.

Current admission, authority, ordinary reservation and upgrade cases: **120
passed** ([log](factory-corrections-evidence/delegated-reservation-boundary-tests.log)).
The separate delegation compatibility run passed **21 tests**, including real
Ed25519 subject/owner/namespace checks, ledger-write rollback, scope refusal,
restart replay, lifetime/concurrency accounting, and retained capacity after
cancellation plus lease expiry
([log](factory-corrections-evidence/delegated-reservation-compatibility-tests.log)).
The state-store binary build and CLI help checks passed; default-feature
`cargo check --locked` passed (`/tmp/factory-delegated-default-check.log`).
See the [delegated workflow](../../contracts/factory/delegation.md).
Automated planner signing/dispatch and live F2 acceptance remain open. The subsequent **six delegated reservation
tests passed**, including two independent SQLite clients racing different
requests under one concurrency slot: only one committed, and the other remained
blocked after refreshing its head
([contention log](factory-corrections-evidence/delegated-reservation-contention-tests.log)).
All **ten integrated factory harness tests passed** after the production changes
([harness log](factory-corrections-evidence/delegated-reservation-harness-tests.log)).

The [end-to-end CLI fixture](../../tests/delegated_reservation.rs) now exercises
the built binary with a migrated disposable store and real owner/subject Ed25519
keys: signed contract/budget/grant imports, unsigned drafting, subject-only
reservation, wrong-key/namespace/changed-byte rejection, a correctly signed
out-of-scope request, concurrency/lifetime limits, cancellation, revocation and
byte-identical response replay after a later config change. No worker is started;
the native profile is synthetic and explicitly uncertified.

This workflow exposed another resource-ownership defect: when automatic admission
was disabled, an explicit reservation skipped the overlap check. The new
regression actually reserved a second writer under a two-slot delegation before
the fix ([failing reproduction](factory-corrections-evidence/delegated-resource-before-fix.log)).
Reservation now enforces resource claims independently of automatic dispatch,
for owner and delegated authority. Draft validation checks overlap before any
cursor mutation, and candidate preparation skips blocked tasks with dispatch off.
All **76 admission/reservation tests passed** after the fix, including both
authority paths and draft checks
([regression log](factory-corrections-evidence/delegated-resource-reservation-tests.log)).
The final signed CLI scenario and all **ten factory harness tests passed**
([CLI and harness log](factory-corrections-evidence/delegated-cli-and-resource-harness-tests.log)).

The subsequent complete state-store run passed **1,291 tests**: 665 library
tests, 541 binary tests, 54 CLI tests, two contract tests, ten factory harness
tests, 12 memory-control tests and seven doc tests. Thirteen library tests and
nine live Phase A tests remained ignored. Exit status was zero; execution was
serial in a user/PID namespace with no overlapping compilation. This run covers
the controller, replan, deadline and package-manifest changes described below,
before the final admission cursor-budget correction.
[Retained full log](factory-corrections-evidence/all-targets-before-cursor-budget.log).
It does not establish the ignored live gates or the missing production workflows.

The admission scan cursor now charges its selected SQLite row to the original
shared read budget before copying the task ID, and validates that ID before using
it to resume scheduling. Previously this singleton read bypassed the accounting.
The new regression covers a 16-MiB-plus-one field, an invalid ID and an irrelevant
old-epoch cursor. All **18 admission and cursor tests passed** after this final
edit, including signed dependency enforcement and reservation checks.
[Focused log](factory-corrections-evidence/admission-cursor-budget-tests.log).

Memory package materialization now has a production pull service and CLI:
`memory PROJECT package --binding ID` or `--snapshot ID`. It returns immutable
membership references with the exact consumer generation and manifest hash,
using the same transactional materializer as existing package accounting.
The signed-promotion integration scenario now invokes the real CLI repeatedly
through both selectors and checks identical output, pending obligations and zero
new package receipts. All 12 memory-control tests passed
(`/tmp/factory-package-pull-memory-tests.log`), as did all four package-store
tests (`/tmp/factory-package-pull-store-tests.log`). See the
[package workflow](../factory/memory-packages.md) for its limits. Automated
delivery, certified native-adapter acknowledgment, reviewer and barrier workflows remain
open. The final versioned-manifest check passed all four package-store cases
and the real signed-promotion/CLI case
(`/tmp/factory-package-manifest-final-tests.log`). The default-feature build
after adding both CLI selectors passed
(`/tmp/factory-package-pull-default-check.log`).

The [contract implementation map](../../contracts/factory/catalog.md) now covers
all 24 logical catalog entities, including actual Rust/store representations,
authority boundaries, compatibility evidence and missing production pieces.
Its links and entity coverage were checked against the downloaded catalog.
Fixed [task-contract byte vectors](../../contracts/factory/task-contract.md) now
cover both Git object formats and legacy/explicit dependency policy references.
All three domain tests passed (`/tmp/factory-contract-vectors-tests.log`), including
raw digest retention and nested duplicate/unknown critical-field rejection.
Version-1 ordering/duplicate-capability compatibility is documented explicitly;
the parser was not silently tightened for already retained signed documents.
The universal envelope, complete canonical-vector set and independent boundary
review remain open; publishing this map alone does not establish F0.4 acceptance.

The full state-store run before the latest controller-scoping edits passed:
646 library tests (13 ignored), 541 binary tests, 54 CLI tests, 2 contract
tests, 10 factory harness tests, 12 memory-control tests and 7 doc tests.
The live Phase A suite's 9 tests remained ignored. The run used a user/PID
namespace and serial test execution, with no overlapping compilation.
[Retained full log](factory-corrections-evidence/all-targets-before-controller-scoping.log).
This supplies broad regression evidence for that source state, not live fleet
certification or validation of the subsequent controller edits.
The final controller edits passed the 10 library and 31 binary cases in
`/tmp/factory-controller-scoped-final-tests.log`, plus both observation/rebind
fence cases in `/tmp/factory-observation-fences-tests.log`. The new SQL-work
regression measures equal schema-open/guard costs with zero versus 10,000 events;
the new historical-task regression retains administrative corruption detection
while allowing an unrelated active observation batch to commit.

1. Replace actual foreground full snapshots and unbounded readers with scoped,
   indexed, budgeted reads; propagate original deadlines and measure the work
   performed by the decision path. Include admission and reservation, not only
   the preliminary controller schema check.
2. Complete production wait/replan, automated delegated dispatch,
   memory package/acknowledgment, barrier and
   reviewer workflows. Verify their authority and transaction boundaries.
3. Complete contract-catalog canonical vectors and independent boundary review;
   complete packaging/migration acceptance evidence while retaining conservative
   activation. Separate distribution and feature inspection are implemented.
4. Extend the combined history workload through multiple waves, signed reservation,
   slow endpoints, memory traffic and recovery; collect full latency/freshness
   distributions against a baseline under prospectively fixed gates.
5. Validate all corrected targets and restore/canonical recovery behavior.
6. Real worker and second-adapter certification, live ramps and release gates
   require the specified operational evidence and explicit live-run budget.
   Existing unperformed/unsupported labels must remain until proven otherwise.

Routine installation remains an explicit administrative history-validation path.
Admission still needs a complete shared read-budget audit, and full controller
measurements remain outstanding. The broad and subsequent focused validation
above cover the present corrections; further implementation will need its own
appropriate regression checks.

No paid worker sessions have been launched, no real project stores have been
migrated, and no changes have been published.
