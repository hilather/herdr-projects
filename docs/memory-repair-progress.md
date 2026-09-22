# Memory review repair progress

Updated 2026-09-20. This tracks repairs against the
[independent review](reviews/2026-09-20-memory-audit.md), not a new certification.
T04.5 and T05.1–T06.2 remain partial; T06.3–T06.5 remain unstarted.

## Implemented in the first repair batch

- Object reads require a regular, no-follow file, enforce the 16 MiB object bound,
  consume the full payload and verify SHA-256. UTF-8 renderers reject invalid text.
  Revision insertion, proposal bodies/evidence and promotion verify bytes.
- Ingest and collection share an enforced filesystem lock. Staging uses exclusive
  creation, publication syncs directories, reingest restores availability, abandoned
  GC claims receive fresh fences, and failed unlink never certifies purge.
- Immutable accepted proposal JSON owns body/evidence retention, including old
  proposals and evidence retained after promotion. Acceptance rechecks availability
  and cancels pending collection inside the insertion transaction. Retention is
  intentionally conservative until explicit release/forget policy is implemented.
- Promotion checks the persisted review and its event boundary inside its write
  transaction, rejects worker changes to mandatory records, and rechecks base and
  dependency expiry. The fence currently requires rereview after **any** intervening
  event: safe, but less permissive than exact dependency/policy fencing.
- Promotion receipt replay returns the same sequence; equal body hashes no longer
  suppress applicability/dependency changes. New proposal record IDs use full
  hashes of their keys. Unsupported `narrow` decisions are rejected at ingress.
  Invalidation references now use record IDs and stable IDs, but affected-consumer
  expansion and the routing outbox are still missing.
- Snapshot selection charges object byte sizes as a conservative character upper
  bound. Cache identity includes the selected manifest, budget, estimator and
  subscriber. Expiry and mandatory-role changes therefore cannot reuse old entries.
  Scope eligibility is separate from ranking; dependency traversal follows current
  directed edges. Complete sensitivity semantics still need implementation.
- Coordinator rendering verifies mandatory bodies, includes current project text
  in deltas, rejects final output overflow, and prevents old acknowledgments from
  rewinding the session cursor/latest checkpoint. Generation binding and a single
  consistent runtime/memory read boundary are still needed.
- Profile-only rendering no longer searches other tasks' snapshots. Unbound callers
  use verified canonical project constraints and global facts; task-local and
  scoped optional facts are excluded. The explicit attempt renderer requires the
  snapshot recorded on that attempt and checks task/profile/digest identity.
  The legacy thread producer has no sealed attempt binding and uses the conservative
  fallback. Canonical memory is emitted as one pre-budgeted block to prevent the
  old file packer from dropping mandatory rules. Whole-brief accounting remains.
- SQLite context and fallback readers no longer consume editable generated Markdown.
  Edited projections are preserved. Projection refresh/export remains separate work.
- Import/cutover compares the complete supplied plan with the current inventory.
  Cutover verifies each imported head, body and provenance before changing authority.
  This does **not** implement durable recovery after policy/marker publication.
- New CLI mutations hold project operation exclusion. Review checks proposal identity
  before writing. Context validates ownership before profile resolution. The CLI
  tests now cover the intended named-profile setup and actionable missing-profile
  error. Executor metrics use an actual bounded read even if the file grows.

## Second repair batch

Schema 23 now retains immutable instructions, task text and structured scope input;
`snapshot-input` reconstructs historical knowledge without rereading project files.
Old manifests require a new snapshot rather than invented historical inputs.

Manual edits stage immutable expected-base candidates. Owner-signed approvals bind
exact candidate bytes/base and atomically advance heads with durable review receipts;
rejection leaves heads unchanged. Preview uses retained old/new text. Candidate
references participate in GC retention. Shadow reimport cannot overwrite approved
knowledge. Existing record metadata is preserved through body review.

The unsigned public proposal-review/promotion methods were removed. Review now
requires an expiring owner signature bound to project, proposal digest and exact
record keys; promotion rechecks current authority/configuration and the stored
authorization. This closes the unsigned supported path, but does not yet provide
delegated reviewer identities/grants. Internal state-machine tests have a private,
test-only seam; production callers cannot construct it.

Proposal acceptance now requires the attempt's actual consumed snapshot, both at
service validation and at insertion. Observed/dependency revisions must belong to
that snapshot. Unsupported repository claims and validation IDs are rejected rather
than accepted as verified evidence. Complete worker object staging is still needed.

Cutover leaves its journal pending through projection publication. Retrying the
same signed command recovers from the committed policy and ownership switch;
ordinary mutations are excluded while pending. Abort is limited to before signed
policy commit. A real post-owner-publication failure/recovery case is tested;
the comprehensive process-death matrix remains open.

Promotion and signed import approval now commit consumer invalidations and durable
routing intents with the new revisions. Routing conservatively covers active
subscriptions (including coordinators), with a 10,000-subscription refusal bound.
A routing failure rolls back revisions, receipts and all obligations. Intents stay
pending: neither selective transport nor consumer acknowledgments are claimed.

## Remaining acceptance work

| Review findings | Remaining work |
| --- | --- |
| 1, 17 | Delegated scoped reviewer grants and precise review dependencies instead of whole-head rereview |
| 2, 19 | Certified production profile/launch producer and final external prompt framing/accounting; sealed snapshot reservation, pre-effect checks and retained attempt-input rendering now exist |
| 3, 4, 12, 15 | Production profile/config authorization, sensitivity semantics and broader scope/dependency tests |
| 5, 8, 9 | Full process-death fault matrix, bounded indexed retention queries and explicit release/forget policy |
| 6, 7, 18 | Complete cutover kill/restart matrix, projection refresh/export and broader manual-review conflict fixtures |
| 10, 11 | Automatic native conversation identity, authenticated caller integration and old-session retirement; explicit route/epoch-bound sessions now exist |
| 13 | Worker object/evidence CLI staging and typed repository/validation evidence resolution |
| 14, 20 | Richer semantic review, queued subscription retirement and scheduled expiry notification; source mutation invalidation and read-time expiry checks now exist |
| Other observations | Shared read-budget coverage for approval revocations/uses and memory APIs |
| T06.3–T06.5 | Transport/repacking, worker result integration, forget/purge and full wave barriers; task-scoped signed reconciliation now exists |

## Validation

Second-batch results: **276 library tests passed** (including the 25 original
regressions, now internal), **10 signed service/CLI acceptance tests passed**, and
**2 contract tests passed**. The three migrated-context/runtime CLI cases passed
after updating their deliberately downgraded schema fixture for v23. Default-feature
`cargo check --locked --offline` and `git diff --check` passed. No live project was
upgraded. The affected binary ownership/upgrade regression also passed with its
disposable socket fixture. The complete long-running CLI/binary suites were not rerun.

First-batch results: **251 library tests passed**, **25 memory regression tests
passed** (including the original 17 probes), all **three previously failing CLI
acceptance cases passed** with the profile fixture/error-order corrections, and
the existing migrated runtime-binding test also passed. The focused brief-packing
and executor-metrics tests passed. Default-feature `cargo check --locked --offline`
and `git diff --check` passed. The complete long-running CLI/binary suites were
not rerun for this batch.

The original 25 regression tests now live in `src/memory/regression_tests.rs`,
inside the library so their private test-only review seam is unavailable to production.
Real signed service/CLI acceptance tests live in `tests/memory_control.rs`; the original
review probe source/log remains unchanged as historical evidence. Tests exercise
real SQLite, filesystem corruption, concurrent ingestion, failed collection,
proposal/evidence retention, stale review and expiry, receipt replay, snapshot
eligibility/budgets/cache, signed cutover validation, and coordinator context.

Run with:

```sh
cargo test --locked --all-features --lib --test memory_control -- --test-threads=1
cargo test --locked --all-features --test cli migrated_ -- --test-threads=1
cargo test --locked --all-features --test cli legacy_commands_refuse_store_ownership_even_without_feature
```

No live project migration, release build, macOS or real SSH certification is claimed.

## Third repair batch: selective routing and checkpoint consistency

Task delivery intents now select affected subscriptions using retained scope,
pinned keys, consumed records and transitive dependencies of the exact consumed
revisions. Scope changes cannot hide updates to already consumed records. Directory
and terminal wildcard scope matching is shared with snapshot selection and respects
path component boundaries. Global/mandatory changes remain broad, as do coordinator
subscriptions and legacy snapshots without retained inputs. Historical subscriptions
still need attempt-generation retirement; transport and applied acknowledgments are
unfinished. T06.3 is partial, not acceptance-complete.

Coordinator context renders runtime details from one snapshot. Coordinator memory
snapshot caching includes the event head; publication atomically checks the current
head, snapshot sequence, manifest and subscriber/session binding. Concurrent mutation
causes a retryable refusal without publishing a checkpoint or advancing its cursor.
Session generations and authenticated acknowledgment remain outstanding.

Validation: 277 library tests, 10 signed memory acceptance tests and 2 contract tests
passed. The additional checkpoint publication regression verifies rejection of a
foreign session snapshot and both stale-head and falsely advanced-head publication.
No live project was upgraded; the full long-running CLI/binary suites were not rerun.

## Fourth repair batch: exact worker update acknowledgments

Schema 24 adds immutable per-attempt `seen`/`applied` receipts. Workers pull an
exact single-change package with a digest binding attempt, starting snapshot,
record/revision, body hash, severity and event sequence. Reads verify retained
content and never acknowledge it. Explicit acknowledgment rechecks current live
attempt ownership and snapshot binding inside the receipt transaction. Applied
requires seen and refuses superseded, expired or invalid revisions. Replays reuse
the same receipt/event and never cover a newer change. A replaced attempt cannot
acknowledge even a previously recorded receipt. Active task routing now ignores
historical snapshots unrelated to the current attempt.

CLI: `memory PROJECT update --delivery ID --attempt ID`, `ack --input FILE`, and
`receipts --attempt ID`. Receipt insertion and its audit event commit atomically.
Receipts are worker declarations; they neither prove validation nor resolve any
invalidation or completion barrier. Production worker-channel authentication,
transport, multi-change packaging/repacking, deferred/rejected dispositions and
result/barrier integration are still unfinished. T06.3 remains partial.

Fourth-batch validation: **278 library tests**, **12 signed memory service/CLI
acceptance tests**, and **2 contract tests** passed. New acceptance coverage checks
side-effect-free pulls, CLI receipts, restart-safe replay, exact digests, old versus
current snapshots, replacement attempts, superseded revisions, corrupt bytes,
immutable receipts, and rollback of both audit event and receipt on an injected
insert failure. Schema-23 upgrade preserves events/tasks and invents no receipts.
No live project was upgraded; full long-running CLI/binary suites were not rerun.

## Fifth repair batch: transactional task completion guard

Added a read-only `memory PROJECT readiness --task TASK` report and checks at the
generic store task-success boundary. Checks run before and after all mutations in
one transaction, preventing attempt clearing/rebinding in the success batch from
hiding obligations. Required unapplied changes and unresolved invalidations block;
informational changes do not. A newer initial snapshot may cover an earlier change.
Consumed knowledge includes exact applied revisions and transitive dependencies;
expiry, revocation, unavailable object rows and changed pinned source revisions are
rechecked without relying on a new delivery event. Mandatory revisions are checked
even when changed through a path without a routing intent.

This does not complete T06.5: the result/gate contract is still a standalone fixture,
and wave participants, proposal closure, approved snapshot freezing, signed conflict
resolution and verified release are not implemented. Empty readiness is diagnostic,
not approval. Recording worker termination remains distinct from task success.

Fifth-batch validation: **284 library tests**, **12 memory service/CLI acceptance
tests**, and **2 contract tests** passed in the broad run. All **7 focused completion
guard tests** passed afterward, including an additional older-schema refusal case
(285 library tests exercised across the runs). Tests cover transaction rollback,
pre/post-batch bypass attempts, informational versus blocking updates, unrelated
tasks, exact receipt coverage, expired applied knowledge, stale derived sources,
unrouted mandatory rules, and the readiness CLI. No schema bump or live project
upgrade was needed for this batch. Full long-running CLI/binary suites were not run.

## Sixth repair batch: coordinator session isolation

Coordinator context starts with a fresh random session token unless `--session` is
explicitly supplied. Checkpoint acknowledgments require that token. The session key
binds the canonical project-store path, coordinator runtime binding/revision,
control epoch and configuration; legacy coordinator JSON and the shared `default`
identity no longer establish checkpoint continuity. Wrong-session or old-generation
acknowledgments fail before changing checkpoint/cursor state. The acknowledgment
transaction also fences the event head read while resolving the binding.

Continuations use deltas only after acknowledgment and only with matching profile,
configuration, budget and retained instructions. Changed inputs force full context.
Checkpoint IDs include fresh entropy to avoid same-millisecond collisions. Existing
unbound checkpoint history remains readable but does not authorize new sessions.

These explicit tokens are local continuation handles, not authentication against
same-user callers. Automatic native session/compaction identity and historical
subscription retirement remain unfinished. Operators must omit the token after
restart or compaction uncertainty. No schema bump or live upgrade was needed.

Sixth-batch validation: **287 library tests**, **12 memory acceptance tests**,
**2 contract tests**, and **3 migrated-project CLI tests** passed. New coverage
checks cross-session refusals without mutation, epoch rotation and old receipt
replay, full context after changed inputs, explicit CLI token requirements,
same-session delta continuation and fresh full context without a token. The full
long-running CLI/binary suites were not rerun.

## Priority batch: invalidation, reconciliation and sealed worker knowledge

Source replacement, revocation and policy changes now propagate through immutable
historical dependency edges. Current dependent heads become stale and their
consumers receive blocking obligations in the same transaction. Blocked validity
is never weakened, repeated propagation is quiet, and routing failure rolls back
the source change and dependent state together. Expired sources are also checked
at selection time. Republishing reviewed dependents against current sources
restores eligibility; it does not silently clear old consumer conflicts.

Owner-signed `memory-reconcile@herdr-projects` documents resolve an exact list of
task invalidations with expected-head, current-authority/configuration and expiry
checks. Invalid references roll back the complete list. Audit events retain the
signed document; stable replay never resolves later items. Resolved routing
invalidations count only as owner disposition of their exact delivery cause,
record and sequence. Remaining conflicts keep blocking completion. Resolution
cannot bless invalid consumed knowledge or missing mandatory rules. Consumed and
transitive source bytes are verified with a 64 MiB aggregate read budget. Global
invalidations still require a separate project-wide reconciliation design.

Sealed reservations can now carry immutable memory references. Validation binds
snapshot ID/manifest to task revision, retained profile and configuration; new
reservations with memory records cannot omit the snapshot. Approval scope includes
that reference. Reservation, claim and pre-effect checks reject stale memory and
expired dependencies. The attempt row retains the exact snapshot ID. The new
`attempt-input` renderer reads approved instructions/task/memory from retained
inputs, checks configuration and consumed bytes, enforces the captured budget,
and rechecks state after rendering. It grants no launch authority. Selection cache
identity now includes the event head, so reselection can produce a fresh snapshot
after a conservative freshness refusal.

Raw unsigned revision/head/validity/policy mutation APIs are no longer exported;
shadow import retains its restricted internal revision seam. Signed policy and
review services remain production mutation routes. `HardMemory` kind now counts
as mandatory without a separate hard flag, and mandatory expiry/invalidity causes
snapshot refusal rather than silent omission.

T06.4 moves from unstarted to partial. Automatic expiry notifications, global
reconciliation, forget/purge, certified production profile/dispatch, external
prompt framing, structured verified results and full wave freeze/release remain
unfinished. No live project was upgraded or launched.

Priority-batch validation: **292 library tests**, **12 memory acceptance tests**,
**2 contract tests**, and the external unsigned-write **compile-fail doctest** passed.
Coverage includes transitive invalidation rollback/recomputation, source expiry,
signed reconciliation namespace/identity checks, mixed-list rollback, corrupt
consumed bytes, replay that preserves newer conflicts, sealed reservation/profile
binding, claim/pre-effect refusal after memory changes, retained worker rendering,
and `HardMemory` mandatory treatment. No live project was upgraded or launched.
The complete long-running CLI/binary suites were not run.

The three migrated-project CLI checks, default-feature build check and `git diff --check` also passed for the priority batch.

## Canonical launch safety and complete prompt accounting

Generic operation outcomes can no longer confirm a canonical launch using an
arbitrary identity string. This restriction covers both completion and recovery
observation. Generic attempt commits cannot rewrite sealed launch records or
assert termination; batch refusal rolls back accompanying task edits. Launch
claim history independently forbids replay, beyond one-use approval enforcement.

Subprocess SIGKILL tests cover before/after reservation commit, after claim commit,
and after a simulated external effect. They verify rollback, durable approval use,
retained capacity and no replay after restart. Actual native launch/result receipt
and termination crash points still require their concrete adapters.

The canonical `attempt-brief` renderer includes protocol and execution identity in
the captured character budget, exposes the final digest, and refuses oversized
prompts. A worker snapshot constructor reserves that framing before optional
selection; estimator identity separates these snapshots from historical task
snapshots. Retained-input and corrupt-object tests cover the complete brief path.

This batch does not enable canonical dispatch. The exact outstanding implementation
and evidence are tracked in [canonical worker launches](canonical-worker-launch.md).

Validation: **297 library tests**, **12 memory acceptance tests**, and **2 contract
tests** passed. The default-feature build check, CLI help smoke checks for
`attempt-brief`, and `git diff --check` passed. Logs:
`/tmp/herdr-canonical-launch-checks.log` and
`/tmp/herdr-canonical-launch-default.log`. No live project was launched or upgraded.

## Typed launch acknowledgment and native supervision

The sealed start receipt service now atomically confirms submission, updates the
runtime binding, records exact session/terminal/agent ownership, and moves the
attempt to `launching` while retaining capacity. Observation recovery consumes
existing launch history rather than issuing another start; expired/revoked approval
or a paused project does not erase a physically observed worker. Generic JSON
and arbitrary outcome strings remain unable to establish this evidence.

Receipt rejection and injected transaction failures preserve the complete store.
SIGKILL tests on both sides of receipt commit establish atomic recovery and exact
receipt replay. The service deliberately refuses repository/worktree receipts
until their resource-creation contract exists.

Upstream Herdr v0.9.1 source inspection identified the native name length limit,
shell-source semantics of `pane run`, and the limits of session-based pane shutdown.
Deterministic names and literal POSIX quoting now match those constraints. A Linux
namespace supervisor command builder adds a wall deadline and detached-descendant
containment. Isolated live tests passed for native process observation/closure and
supervised `setsid` descendant cleanup; no user session was used.

The concrete profile/preparation, resource creation, brief-delivery and termination
producers are still required before dispatch can be enabled. See the updated
[canonical launch checklist](canonical-worker-launch.md).

Target selection is now durable before submission and immutable afterward. Start
receipts must match that target's exact session incarnation and terminal, including
during lost-response recovery. The Linux supervisor observer retains pidfds and a
namespace descriptor, checks the command and parent/child relationship, and tracks
those exact processes through exit/reaping. The isolated Herdr descendant-cleanup
test now exercises this observer. It remains process evidence, not authority to
release capacity or a replacement for preservation and restart recovery.

The socket transport now bounds request/reply bytes and shares one absolute
deadline across nonblocking connection, writes and reads. It refuses partial
replies without retrying a potentially submitted request. Tests exercise stalled
writes, trickling/oversized/invalid replies and a real temporary Unix socket.

Validation: **314 library tests**, **12 memory acceptance tests**, **2 contract
tests**, and **2 compile-fail doctests** passed. Five socket tests passed again
after adding bounded waits to their fixtures. The isolated native process test
and supervised detached-descendant test passed; the latter passed again with the
new observer. Socket and live-server checks required execution outside the sandbox
because it denies socket I/O. All used disposable fixtures; no user session or
live project was launched, altered or upgraded. Logs are in
`/tmp/herdr-launch-final-{library,memory,contracts,socket,doctests}.log` and
`/tmp/herdr-canonical-supervisor-live-observation.log`.


## Canonical brief and termination implementation (2026-09-20)

Concrete brief preparation/delivery and confirmed-worker termination/recovery are
now implemented. Briefs use retained memory bytes and the captured whole-prompt
budget, check exact native readiness before claiming, and never replay an uncertain
send. Persistent Linux pidfs identities let termination reconnect after controller
restart without mistaking PID reuse for ownership. Stop preserves resources and
ownership in place; a failed transaction retains capacity. Exit alone never marks
a task successful. Both controller execution paths rotate these obligations, and
root conflict inventory includes staged launch targets and legacy socket aliases.

Validation: 329 library tests, 12 memory acceptance tests, 2 contract tests, 9 focused
controller tests and 3 isolated live Herdr native-contract tests passed. The new
live test confirms `layout.apply` accepts literal argv for an exact supervised
executable. Native brief protocol tests use a fixture executable, not an authenticated
agent. Full profile preparation, resource/start production, partial-start recovery
and end-to-end launch certification remain open. Canonical startup stays disabled;
see [the current checklist](canonical-worker-launch.md).

Broader regression run: 504 of 508 binary tests passed. Three cleanup/preservation
tests refused to establish writer quiescence because this host denies inspection
of `/proc/32390/cwd`; the safety check was not weakened. One remote controller
timing scenario failed in the parallel suite and passed when rerun alone. Two
compile-fail doctests also passed. Details are in
`/tmp/herdr-lifecycle-binary.log`, `/tmp/herdr-lifecycle-remote-recheck.log` and
`/tmp/herdr-lifecycle-doctests.log`. The full binary suite is not claimed green.


## Recover missing start-to-brief handoffs (2026-09-21)

The controller now discovers confirmed supervised starts that have no initial
brief operation and schedules retained-memory preparation in both execution paths.
This closes the restart window between recording start and queueing the prompt.
Preparation sends nothing, validates the attempt revision and current head, retains
memory exclusion through rendering/publication, and rechecks cancellation/deadline
before committing. A repeated request returns the same obligation without writes.
Any existing brief operation prevents a replacement hint, including uncertain and
permanently failed deliveries. Trusted profile preparation and native start/resource
production are still open; this change does not enable canonical launches.

Validation: all 332 library tests passed, including recovery of missing preparation,
read-only replay, failed publication, expiry and no replacement after uncertain
submission. All 9 focused controller tests passed serially. The concurrent controller
run had one transient execution-lock conflict before the serial recheck. All-feature
compilation and whitespace checks passed. Logs:
`/tmp/herdr-brief-handoff-library.log`,
`/tmp/herdr-brief-handoff-controller{,-serial}.log`.


## Harden installation evidence before launch preparation (2026-09-21)

Profile version probes previously inherited the controller environment, reset time
budgets across subprocesses, and compared only canonical path/content digest across
observations. Probes now use a cleared explicit environment and fixed working
directory; a single 20-second deadline/cancellation covers hashing, both commands
and final validation. Both selected executables are pinned before either executes.
File incarnation/metadata checks reject identical-byte replacement and permission
changes, and original-path checks catch symlink retargeting even during the second
probe. No caller JSON or installation version becomes launch capability evidence.
Trusted profile preparation and launch resource production remain open.

Validation: all 14 agent/profile tests passed, including a real temporary version
executable that refuses inherited HOME/session/loader settings, identical-byte
replacement, selected-path retargeting, cancellation and expiry. Existing redaction,
version parsing and named-profile budget tests also passed. Log:
`/tmp/herdr-profile-all-tests.log`.


## Approval validation before capacity reservation (2026-09-21)

Reservation previously checked approval-reference syntax but deferred the actual
grant/configuration checks until claim time. Missing, expired, revoked or mismatched
approval could therefore reserve capacity and mark a task running even though no
launch could be authorized. Admission now validates the exact stored grant, action
scope, permission policy, validity interval, revocation/use state and current disk
configuration within the reservation transaction. It does not consume approval;
consumption remains atomic with claim, with the same checks repeated before effects.
New reservation admission requires the scoped-approval schema (13 or newer), while
historical inputs remain readable. Denials leave tasks, capacity and events unchanged.

Validation: all 333 library tests passed after the implementation change. The new
admission regression covers missing/wrong/expired/revoked grants, changed profile
inputs and disk configuration, and confirms reservation leaves grants unconsumed.
Additional cases cover a different permission policy and an already consumed grant.
Logs: `/tmp/herdr-admission-approval-tests.log` and
`/tmp/herdr-admission-policy-test.log`. Trusted profile preparation and native launch
resource production remain open; dispatch is still disabled.


## Partial supervisor exit and short stop deadlines (2026-09-21)

Cancellation previously required reconnecting to both still-live supervisor
processes. It could therefore remain stuck when the namespace init had exited but
the outer supervisor survived. The forced-stop threshold also equalled short
operation deadlines, causing the deadline check to run before any forced signal.

The authorized stop path now recovers each exact pidfs incarnation independently.
Already-exited/replaced processes are not signaled; inaccessible identity reads
remain errors. A live original peer can be stopped even after the other exits.
Graceful termination uses at most half the remaining budget, capped at two seconds,
leaving time for forced exit observation. Expired/cancelled requests before stop
send no signals. Quiescence and the existing atomic store receipt are still required
to release capacity; no artifact preservation or task-success claim was added.

The regression uses real disposable namespace workers, stops their supervisors,
and tests both a still-live init and an init killed before recovery. Each completes
within a one-second stop deadline, then verifies exit and read-only stop replay
after reaping. The focused supervisor tests passed; log:
`/tmp/herdr-partial-stop-tests.log`.

Full validation: all 334 library tests passed, including canonical brief delivery,
cancellation, rollback/restart recovery and memory regressions. Default-feature
compilation and whitespace checks passed. Logs:
`/tmp/herdr-partial-stop-library.log` and `/tmp/herdr-partial-stop-default.log`.


## Fail-closed cross-project worker ownership inventory (2026-09-21)

The canonical brief adapter previously indexed a legacy thread's missing `id`
directly (which could panic), and converted wrongly typed coordinator/thread route
fields to empty strings. It now decodes typed routing projections, checks thread-ID
format and filename agreement, and returns redacted errors for malformed records.
A valid pending thread with no pane remains supported. No brief claim or terminal
input occurs while neighboring ownership is unreadable.

Selected launch targets previously used the attempt ID inside event JSON to filter
out terminated attempts before checking provenance. A corrupted payload pointing at
a terminated peer could therefore hide a retained target. Inventory now joins the
event's operation to immutable attempt inputs, then to the actual attempt row, and
checks that the payload IDs match that relationship.

Regression coverage includes missing thread IDs, mismatched filenames, malformed
socket/pane/machine fields, non-object coordinator records, secret redaction,
unchanged delivery/store state on refusal, valid pending threads and corrupted
target JSON referencing a terminated peer. These changes do not enable canonical
startup; profile preparation and resource/start production remain open.

Validation: all 336 library tests and 16 focused binary ownership tests passed.
All-feature compilation and whitespace checks passed. Logs:
`/tmp/herdr-neighbor-identity-tests.log` and `/tmp/herdr-neighbor-ownership-tests.log`.


## Validate complete selected-target provenance (2026-09-21)

The bounded selected-target inventory checked the input payload hash but omitted
the full reader's content-derived ID and launch-operation checks. That allowed
internally inconsistent records to pass the lightweight ownership scan even though
full store decoding rejected them. It now shares retained-input validation and ID
derivation with the reservation reader, and verifies the actual launch operation's
task, kind, target, revision, payload/hash, payload version and idempotency key.
All newly read text fields are charged to the existing byte budget before decoding;
missing join rows fail closed. No public capability constructor was added.

A corruption regression verifies refusal of rehashed edited inputs, missing launch
operations, wrong operation kinds/targets/revisions and mismatched operation payloads.
Each case uses a disposable store and worker and sends no brief. Profile preparation
and native resource/start production remain open; canonical startup stays disabled.

Validation: all 337 library tests and 16 focused ownership tests passed; all-feature
compilation and whitespace checks passed. Logs:
`/tmp/herdr-target-provenance-library.log` and
`/tmp/herdr-target-provenance-ownership.log`.


## Revalidate brief authority after native command preparation (2026-09-21)

The brief adapter previously validated the claim and its configuration/memory
bindings, then hashed the Herdr executable again before launching the request.
That work could outlast a lease, grant or memory validity interval. The final
preflight now runs after executable verification and request serialization, with
fresh process/session fencing and deadline/cancellation checks before execution.
The real delivery path supplies the store's complete claim validation at that
boundary. Failure retains the original one-use claim and capacity; it never opens
another submission attempt.

The new native-adapter regression exercises expired claims, approval revocation
and a changed disk configuration at this final boundary. All cases send nothing,
retain the claim and capacity, and refuse replay after releasing the execution
lock. All 17 focused canonical worker tests passed; log:
`/tmp/herdr-brief-preflight-worker-tests.log`.

Full validation: all 338 library tests passed. All-feature library compilation and
whitespace checks passed. Full log: `/tmp/herdr-brief-preflight-library.log`.
Canonical startup remains disabled pending trusted profile preparation and native
resource/start production.


## Gated resource creation and staged cancellation (2026-09-21)

Added a concrete native resource service for an already approved local workspace.
It reconstructs the exact retained profile using the same schema as profile
inspection, checks full retained brief/configuration/executable identity, claims
once and creates a new tab with literal argv. The agent stays behind an input gate
until explicitly released; the supervisor wall deadline includes that waiting time.
A version-2 target records socket, pane, terminal and persistent supervisor identity
without claiming agent start or prompt delivery. Lost creation replies retain the
claim and cannot cause a second creation.

Recorded gated targets now participate in controller termination discovery.
Cancellation or observed exit can release capacity only after exact supervisor
quiescence, in one transaction with task/attempt/outbox changes. Injected commit
failure leaves the previous state and capacity intact, then recovers after restart.
Stopped staged panes remain in cross-project target inventory for future cleanup.
Changed configuration is rejected before approval consumption or resource creation.

Validation: 344 library tests passed in `/tmp/herdr-resource-library-final.log`.
The isolated real Herdr 0.9.1 literal-argv test also passed with the new gate: no
execution before release, unchanged literal arguments after release, and persistent
supervisor exit recovery. Fake transport resource tests use real Linux namespace
supervisors; they do not certify an authenticated agent conversation.

This closes the gated-tab creation implementation and recorded-target cancellation
gaps for local, existing workspaces. It does not finish production launch preparation:
trusted profile capability evidence/ingress, automatic one-use gate release and
start evidence, new workspace/worktree creation, and recovery of creation without
a recorded target remain open. Canonical dispatch is still disabled.


## Observe uncertain gated creation without replay (2026-09-21)

A durable pre-effect creation intent now retains the original socket incarnation,
prepared route and digest of the exact command vector. Native resource recovery
uses bounded workspace pane/process inventories to identify one matching Linux
supervisor and records its exact target without creating another tab, sending input,
consuming approval again or renewing a lease. It rechecks retained reservation and
binding provenance transactionally. Recovery can proceed after configuration changes,
claim expiry or approval revocation because it establishes resource identity only.
The recovered target then participates in the existing cancellation/exit service.

Regression fixtures cover successful recovery and read-only replay, expired claims
and revoked approval, changed configuration, replacement sockets, duplicate inventory,
changed commands, and a failed target commit followed by successful reobservation.
All 347 library tests passed: `/tmp/herdr-creation-recovery-library.log`.

The bounded controller hint inventory now discovers claimed creations without a
target, including expired claims and revoked approval. Both queued and synchronous
paths invoke observation only. New/unclaimed launch creation remains disabled.
The isolated live Herdr pane discovery and gated-command test also passed.

Still open: resources whose matching process has already exited, and older attempts
lacking the new pre-effect intent.
Trusted preparation, automatic gate release/start and workspace/worktree creation
remain separate unfinished launch requirements. Dispatch remains disabled.

Final controller validation: 347 library tests passed again after scheduling was
wired (`/tmp/herdr-recovery-controller-library.log`). All 9 focused controller tests
passed serially (`/tmp/herdr-recovery-controller-recheck.log`). An initial concurrent
run had one fixture lock-acquisition conflict; its log is retained at
`/tmp/herdr-recovery-controller-tests.log`. Whitespace checks passed.


## Preserve an observed supervisor through exit and fence terminal replacement (2026-09-21)

Resource recovery previously threw away an exact supervisor observation when the
process exited before target commit. It now keeps the pinned process/namespace
handles through commit and retains that observed identity even after exit. The
separate termination transaction can then prove quiescence, retire the launch and
release capacity; no start receipt or task success is inferred.

Both creation and recovery now compare stable pane, workspace, tab, terminal and
working-directory identity before and after process observation. A replacement
terminal during that interval is refused. Display/title/readiness changes do not
invalidate a stable terminal identity. Unrelated panes may have a different or
unknown working directory; the prepared directory is enforced on matching workers.

Regression coverage uses a native-transport barrier to kill the real supervised
fixture after observation but before target commit, then recovers termination.
It also replaces a terminal during creation/recovery and checks that no target is
published. A worker that exits before any identity observation still retains
capacity: process absence alone is not termination evidence.

Validation: all 350 library tests passed (`/tmp/herdr-exit-recovery-library.log`).
After extending the fixture with an unrelated pane of unknown working directory,
all 27 canonical worker tests passed again (`/tmp/herdr-exit-final-worker.log`).
Whitespace checks passed. Trusted preparation, automatic gate release and new
workspace/worktree creation remain open; canonical launch dispatch stays disabled.


## Enforce profile preparation budgets before creating resources (2026-09-21)

Consolidated profile-definition validation so inspection and native resource
preparation apply the same argument, environment, policy-name, intent and budget
checks, with source values withheld from errors. Previously the resource path only
checked digests and a subset of mappings; a matching definition could bypass the
inspection validator and its usage policy.

The gated adapter now requires its supported wall deadline, compares the entire
rendered retained brief (including framing) against the profile input estimate as
well as the snapshot's existing budget, and refuses `unknown_usage = "block"`
because it does not collect verified provider usage. `allow_with_warning` records
a typed `provider_usage_unavailable` warning in the durable creation intent.
Historical intents remain readable without the new optional warning.

Tests cover exact input boundaries, conversion overflow, unsupported deadlines,
missing wall limits and redacted mapping/validation failures. Native fixtures prove
that an oversized retained brief or blocking usage policy leaves approval unused,
claim count at zero, the database unchanged and no resource creation request sent.
Trusted profile capability evidence and production preparation ingress are still
unfinished; this change does not enable launch dispatch.

Validation: 353 library tests passed (`/tmp/herdr-profile-preparation-library.log`)
and all 14 focused agent/profile tests passed (`/tmp/herdr-profile-preparation-agents.log`).
Default-feature compilation and whitespace checks passed. Provider usage collection,
trusted preparation, automatic gate release and workspace/worktree creation remain open.


## Add one-use gate release boundary and waiting-process evidence (2026-09-21)

Added a sealed gate-release transaction separate from resource creation. It requires
the exact retained version-2 target and creation intent, fresh original claim and
current approval/reservation/configuration/knowledge/budget checks. It records the
single submission opportunity without asserting input delivery or worker start;
even identical replay is refused. Target mismatch, revoked approval, cancellation,
lease expiry and injected commit failure leave the database unchanged. A failed
commit may be retried because no external request is made by this store service.

Gated start receipts now require a matching release intent with ordered observation
times. The live waiting-gate proof holds process handles and verifies the actual
canonical gate shell, rather than assuming that a living supervisor means the gate
is still waiting. A real fixture releases the shell, confirms the supervisor remains
alive, and verifies that both the old proof and fresh waiting-gate observation fail.

These are the release boundary and process-evidence components. Automatic native
submission, exact execution-environment preparation and the concrete start receipt
producer remain open; this change does not enable dispatch or send gate input.

Validation: 356 library tests passed (`/tmp/herdr-gate-boundary-library.log`).
All 5 focused gate tests passed after adding cancellation following release recording
(`/tmp/herdr-gate-final-focused.log`); the original reservation releases only after
exact supervisor termination. Default-feature compilation and whitespace checks passed.


## Confirm an exact native start without resubmission (2026-09-21)

Added a concrete start-observation producer and controller scheduling for released
but unconfirmed workers. It requires the exact retained release/target, persistent
supervisor identity, direct child executable plus argument digest, and one matching
native agent record with the deterministic attempt name and terminal/route identity.
The process proof remains pinned and is rechecked after native observation and
cross-project inventory. Agent metadata alone cannot confirm a waiting gate.
Interpreter/launcher indirection and modified argv remain unsupported.

Observation may resolve an existing consumed claim while still claimed or after
expiry, including after approval revocation and configuration changes. It does not
renew authority or submit input. The atomic typed receipt publishes ownership and
moves the attempt to launching, retaining capacity. Readiness, brief delivery and
task success remain separate. Confirmed receipt replay checks the retained outbox
outcome and is read-only.

Regression fixtures exercise a real gate-to-executable transition, misleading native
metadata before exec, a foreign terminal after exec, claimed and expired recovery,
revoked approval, changed configuration, controller discovery, failed start commit
and retry through observation only. Native transport is a fixture; this does not
certify an authenticated vendor conversation. Automatic gate submission and trusted
execution-environment/profile preparation remain unfinished; dispatch is disabled.

Validation: all 357 library tests passed (`/tmp/herdr-start-confirm-library.log`),
all 9 focused controller tests passed serially (`/tmp/herdr-start-confirm-controller.log`),
and default-feature compilation and whitespace checks passed. The pending-launch
parser follows Herdr 0.9.1's omitted-false field contract and rejects true, null or
non-boolean values. Live authenticated vendor workflow certification remains open.


## Native gate sender with explicit clean agent environment (2026-09-21)

The full dispatch-enablement goal is active; `docs/dispatch-enablement.md` records
the completion audit without substituting component tests for a working pipeline.
Added the concrete one-use gate sender. A frozen execution-home reference binds
the credential/config home into profile and approval identity. The gated exec uses
`env -i` and a fixed non-secret baseline; inherited environment variables do not
reach the agent. Existing profiles remain byte-compatible when the optional home
is absent but cannot use the new sender until explicitly prepared.

The sender rechecks the exact waiting process, terminal, executable, retained brief,
budgets and original claim, records release atomically, then sends one native input
request. An OK response is not start confirmation. Lost reply coverage drives the
real fixture process through exec, refuses a second native input, and recovers the
start using observation only. Refusal fixtures cover missing/unsafe home, changed
configuration, revoked approval, replacement terminal and failed release commit.

The isolated Herdr 0.9.1 contract passed with the clean gate: exact argv survives,
the executable waits for the release line, the actual input result is typed `ok`,
and waiting-process proof fails after exec. Full dispatch remains disabled pending
trusted preparation, production admission and workspace/worktree handling.

Validation: all 360 library tests passed (`/tmp/herdr-clean-release-library.log`),
the clean-environment gate contract passed on the installed Herdr 0.9.1 in an
isolated session, and default compilation and whitespace checks passed. These
establish native gate input and environment contracts, not completed vendor-agent
workflow certification or dispatch admission.

### Deterministic native naming and confirmed launch recovery

Implemented `name_started_agent` with exact process and native identity checks,
current claim/approval validation, and a durable one-use naming boundary before
`agent.rename`. Existing names are never overwritten. Lost replies retain the
intent and recover through fresh observation without retrying the rename. Naming
and gate release share the transactional authority and retained-target checks.
Confirmed launch reconciliation now validates the stored start receipt directly,
without incorrectly requiring an unfinished resource-creation claim.

Expanded the native gate regression to cover actual fixture rename submission,
foreign-name refusal, naming event rollback before any request, lost rename reply,
and refusal to resend when an uncertain name disappears. All 360 state-store
library tests passed (`/tmp/herdr-name-library.log`); default and state-store
compilation and `git diff --check` passed. These fixtures are not live vendor-agent
certification. Dispatch remains disabled pending the completion audit.

### Complete launch advancement and fixture workflow recovery

Added `canonical_worker::advance_launch`, connecting explicit retained launch
selection to creation, release, naming and start confirmation under one deadline
and cancellation signal. It respects the exact original/new claim revision and
recovers durable boundaries without resending uncertain requests. The combined
path now refuses missing or unsafe execution homes before resource creation;
gate release independently rechecks the same home policy.

The new workflow regression covers normal launch and lost creation, release and
naming acknowledgments, followed by initial brief preparation/delivery,
cancellation, confirmed termination, capacity release and artifact preservation.
It verifies a single request/event per launch boundary and read-only replay of
confirmed starts. The full 361-test library suite passed before the final home
preflight change (`/tmp/herdr-advance-library.log`). After that change and its new
regression, all 36 canonical worker tests passed
(`/tmp/herdr-advance-worker.log`), state-store compilation passed, and
`git diff --check` passed. Automatic controller admission, trusted production
preparation, workspace/worktree support and live vendor-agent certification
remain outstanding in the completion audit.

### Project-bound production profile preparation

Added `profile prepare PROJECT PROFILE` and the concrete library preparation
service. It reads the project's pinned external owner configuration, selects the
named profile, resolves the interactive owner-approval reference and explicit
execution home, and freezes exact executable/version/config/definition/argument
identities. Version probes run under the real bounded supervisor with a clean
environment, without profile arguments or a caller-supplied Runner. Changed
configuration, policy, binaries or home metadata invalidate preparation.
Version parsing is shared with the existing installation probe.

This produces installation evidence, not invented capability evidence. All
capabilities remain Unknown; launch validation refuses the profile until a trusted
native workflow producer verifies them. No project state, approval or reservation
is changed. The CLI uses the canonical path rather than the legacy project loader.
Three production-preparation unit tests, eight existing probe tests, and all four
profile CLI tests passed. State-store compilation and default compilation passed;
`git diff --check` passed. Native capability certification and signed preparation /
reservation ingress remain the next production-preparation work.

### Live vendor startup and direct-launch readiness correction

The new disposable live contract passed with installed Codex 0.154.0 and Herdr
0.9.1/protocol 22: literal gated startup, exact executable/process observation,
native recognition and deterministic naming, and confirmed namespace termination.
It uses a fresh unauthenticated HOME and sends no task prompt. The sampled startup
screen does not provide visible prompt readiness.

Source inspection and this live contract revealed that `interactive_ready` only
tracks Herdr-managed launches. The canonical direct-command launch could never
satisfy the old brief readiness check. Replaced it with positive bundled detector
evidence via `agent.explain`, exact idle native identity, and identity reobservation.
Fallback idle, external manifests, blocked/busy/skipped/unknown detection refuse.
Readiness is checked again immediately before prompt submission; a post-claim
failure retains the claim and never retries input. Added direct-launch positive,
negative detector, and post-claim readiness-loss regressions.

All 367 state-store library tests passed (`/tmp/herdr-readiness-library.log`), the
live vendor startup contract passed (`/tmp/herdr-vendor-live.log`), state-store
compilation passed, and `git diff --check` passed. The review is recorded in
`docs/reviews/2026-09-21-native-readiness.md`. Authenticated prompt/protocol and full
workflow certification remain unproven; dispatch is not enabled.

### Owned workspace creation and separate layout boundary

Canonical resource creation now supports a prepared local route without an
existing workspace. It records the original creation intent, submits one explicit
`workspace.create`, observes and retains the bootstrap terminal identity, and
commits a separate one-use layout boundary before creating the gated worker tab.
No request defaults to Herdr's active workspace or replaces a prior tab.
`continue_workspace_layout` and `advance_launch` can continue a committed workspace
whose layout was never attempted, under the original live authority and current
identity/configuration checks. Lost layout replies recover through exact process
observation inside the recorded workspace.

Workspace receipts participate in bounded resource inventory and survive worker
termination; they do not certify bootstrap shell quiescence or permit deletion.
Lost workspace replies and workspace-receipt commit failures remain uncertain,
retain capacity, and never trigger layout or workspace recreation. Recovery of
that unknown workspace boundary and owned-workspace cleanup remain open, along
with worktree provisioning.

The complete fixture workflow now includes new-workspace creation through brief
delivery, cancellation and termination, and verifies retained bootstrap ownership.
Added regressions for lost layout/workspace replies, workspace receipt rollback,
layout commit rollback and safe continuation. All 371 library tests passed
(`/tmp/herdr-workspace-library.log`), state-store compilation passed, and
`git diff --check` passed. Dispatch remains disabled pending the completion audit.

### Recovering workspace ownership after a lost acknowledgment

New workspace creation now records a random per-creation marker before submission
and passes it to the native bootstrap process. The bounded observation service
matches that marker in a live, owner-matching process at the exact working
directory, holds a pidfd, and fences the native pane/terminal and server session.
Matching labels alone, duplicate inventories, changed layouts and absent process
proof cannot create an ownership receipt. Environment contents are not exposed.

The sealed observation transaction can retain the exact workspace after claim
expiry or approval revocation, without changing the delivery, attempt, approvals,
or authority to continue. Layout continuation remains separately gated by live
claim/configuration/policy checks. Recovery never repeats workspace creation.
Receipt-commit failure rolls back cleanly and permits a later observation attempt.
Bootstrap exit before observation and historical intents without a marker still
retain uncertainty; cleanup and worktree provisioning remain open.

All 372 library tests passed (`/tmp/herdr-workspace-recovery-library.log`). The
real Herdr 0.9.1 bootstrap marker contract passed
(`/tmp/herdr-workspace-marker-live.log`), including wrong-marker rejection and
proof invalidation after closure. State-store and default compilation and
`git diff --check` passed. The full dispatch completion audit remains open.

### Supervised first-pane compatibility patch (2026-09-21)

Prepared and built a local Herdr 0.9.1 compatibility patch against source commit
`065ef9d6a531c49fb8bee7e818ef837065b21ee9`. The reviewable patch and reproduction
instructions are retained in `patches/herdr/`. New JSON API method
`workspace.create_command` starts literal argv in the first pane using Herdr's
existing argv terminal support. This removes the need for an unsupervised
bootstrap shell for a future integrated launch. A separate method prevents stock
servers from silently discarding a command field. Installed Herdr was unchanged.

The final patched binary passed the disposable live contract
`live_workspace_command_starts_supervised_root_without_bootstrap_shell`:
positive default-shell marker control; empty, relative and missing executable
rejection with unchanged workspace inventory and no fallback shell; exact
production supervisor/gate observation; gate release; process exit after
workspace closure. Evidence: `/tmp/herdr-root-command-live.log`. Native schema
and bounded-command validation tests passed separately in
`/tmp/herdr-native-command-schema-tests.log` and
`/tmp/herdr-native-command-tests.log`. Native build and project live-test
compilation passed; native build reports unused legacy constructor warnings.
Patch reverse-application validation and repository whitespace checks passed.

This is a validated compatibility component, not completed production resource
creation. The canonical adapter still uses the legacy bootstrap/tab flow; direct
root ownership, lost-reply recovery and capability preparation must be integrated
before switching paths. Existing bootstrap ownership remains retained, and the
full dispatch audit remains open. No vendor authentication or task prompt was
used in this contract.

### Canonical supervised-root integration (2026-09-21)

New workspace launches now call the patched `workspace.create_command` directly.
Creation intent version 2 binds the pinned session, prepared route and exact
supervisor command digest before the one-use request. The first pane is the
supervised worker: no bootstrap shell or subsequent layout request is made.
Workspace ownership and the supervised launch target commit atomically with the
same version 2 target payload. The cross-project inventory retains this workspace
ownership after worker termination. Stock servers rejecting the new method do
not trigger fallback or creation replay.

Lost native replies and failure at either ownership-event insertion recover by
bounded workspace/pane/process observation. Exact command, PID namespace and
terminal incarnation evidence is required; labels alone are insufficient. Recovery
can retain resources after approval revocation and lease expiry without changing
the original delivery, attempt or approval. Missing processes remain uncertain;
duplicate workspaces, changed layouts and replaced terminals are rejected.
Historical bootstrap launches retain their version 1 continuation and marker
recovery paths; only test fixtures can initiate new legacy bootstrap creation.
Existing prepared workspaces still receive a gated tab.

A new real-server test found a previously fixture-hidden integration defect:
`pane.info` is not a Herdr JSON API method. The canonical adapter and fixture now
use the actual `pane.get` method. The ignored library test
`live_canonical_supervised_root_creation_release_and_termination` passed against
the patched runtime, exercising actual adapter creation, durable ownership, gate
release, exact executable observation and staged cancellation/termination. Its
profile/approval are fixture records and its worker is sleep; this is not vendor
workflow certification or production admission evidence.

Evidence: 374 library tests passed, with the separately exercised native test
ignored in the default run (`/tmp/herdr-direct-root-library.log`). The live adapter
contract passed (`/tmp/herdr-canonical-root-live.log`). State-store and default
compilation and `git diff --check` passed. The live test uses a stripped copy of
the patched debug runtime and a 60-second worker fixture budget; production claim
leases, request deadlines and identity validation were not relaxed. A test fixture
race reading an empty, newly opened gate-request file was corrected by atomically
publishing the complete fixture request.

Trusted production capability preparation/admission, worktree provisioning,
recovery after process exit before observation, cleanup, controller launch
admission and authenticated vendor workflow certification remain open. Dispatch
remains disabled; the active goal is not complete.

### Production native profile verification (2026-09-21)

Added `profile verify-native PROJECT PROFILE --herdr-executable ...
--agent-executable ... --execution-home ...` for Linux state-store builds. It
performs trusted installation preparation, starts a disposable independently
bounded Herdr server, creates a directly supervised first pane, observes the
exact gate, launches the exact configured agent, checks native kind and terminal
identity against its kernel process incarnation, and proves termination. Worker
wall time honors the shorter of the configured limit and the probe limit. The
server has its own timeout and bounded cleanup. No task prompt is sent and no
project reservation or state mutation is performed.

Only launch and stop capabilities receive Supported evidence. The report binds
the original frozen profile, supervisor identity and observation/termination times.
Readiness, prompt submission, checkpoint and workflow claims remain unresolved,
so launchable/protocol-capable/certified stay false. The typed native preparation
cannot be reconstructed from JSON. Installation preparation now binds adapter
revision 2, including `workspace.create_command` and the corrected `pane.get` API.

The production verifier passed against patched Herdr and the actual Codex 0.154.0
executable, using an empty temporary execution home and no credentials or prompt
(`/tmp/herdr-native-profile-live.log`). The test used optimized production code
so large executable hashing is measured under production compilation. Version-only
helpers are rejected without changing project state. Unmapped startup arguments
are refused before server launch because they can contain a task prompt or
subcommand; their contents remain withheld. All 376 library regressions
passed (two live tests ignored in the default run), as did three compile-fail
documentation tests and the state-store CLI compilation. Evidence is in
`/tmp/herdr-native-profile-library-final.log` and
`/tmp/herdr-native-profile-doc-tests.log`.

An interim debug run during release compilation hit an existing storage-read
deadline; the isolated recheck passed with the original budget
(`/tmp/herdr-native-profile-budget-recheck.log`). The final optimized suite passed
without concurrent builds. CLI help also exposes the new command.

This supplies the first native capability producer; it does not yet supply the
readiness/prompt/workflow evidence or retained certified profile needed for
production launch admission. The full dispatch completion audit remains active.

### Interaction verification and background child identity (2026-09-21)

Implemented `profile verify-interaction` using the worker's shared exact native
identity and positive bundled-detector readiness checks. It submits one fixed
text-only diagnostic prompt after a second readiness/process/configuration fence.
Only a typed acknowledgment for the exact native agent can supply prompt evidence;
there is no prompt retry after ambiguity. Evidence records the prompt digest,
native session/terminal and detector version. Workflow certification remains
separate from native prompt acceptance.

The empty-home live test exposed a real process-observation issue: Codex had two
children under namespace init, while the old code required exactly one child of
any kind. Selection now scans a bounded inventory for exactly one matching agent,
ignores exited and unrelated children, and rejects duplicate exact agents. Both
agent and gate proofs verify parent and PID-namespace membership as well as pidfd
lifetime, executable inode and argv. The real-process regression covers an orphan
helper, duplicate same-executable/same-argv agents, a foreign process, and namespace
termination (`/tmp/herdr-agent-children-tests.log`). The live unauthenticated test
now reaches readiness evaluation and refuses certification without submitting a
prompt (`/tmp/herdr-interaction-unauthenticated.log`).

The authenticated positive test was NOT executed. Automatic approval review
rejected copying and using the existing Codex authentication cache for an external
prompt because it requires explicit user authorization. No credentials were
copied by the rejected action. The implemented test uses a private temporary auth
copy and isolated read-only configuration, submits a single fixed diagnostic
prompt, then removes its temporary home. That specific action awaits approval;
other dispatch work remains available and the overall goal is not blocked or
complete.

Final validation: all 378 library tests passed serially (four live tests ignored),
and all three compile-fail documentation tests passed. The new CLI command's help
and the default-feature build check passed. Logs are
`/tmp/herdr-interaction-library-serial.log`,
`/tmp/herdr-interaction-doc-tests.log`,
`/tmp/herdr-interaction-cli-help.log`, and
`/tmp/herdr-interaction-default-check.log`. An earlier two-thread library run had
one migration lock-contention failure; that test passed in isolation and in the
serial suite. Its cause is not established, and no lock checks were relaxed.

### Retained native verification evidence (2026-09-21)

Added explicit `--retain` to both native verification commands, plus
`profile retained PROJECT DIGEST` for read-only retrieval. Schema 25 retains the
complete credential-free verifier report and its audit event in one transaction.
Only the sealed in-process result can enter the retention API; printed JSON has
no import path. Retention binds the original canonical database path and file
identity, checks the current owner policy/configuration, and is idempotent for
identical evidence. Retrieval verifies both report/profile hashes and original
store identity; a copied database cannot replay another project's report.

Reports remain historical evidence. Current executable/configuration revalidation,
workflow evidence, and preparation/reservation ingress still must connect them to
launch admission. Dispatch remains disabled. The authenticated diagnostic remains
unexecuted pending the previously requested explicit authorization.

Storage regressions cover reopening, duplicate retention, unchanged task/attempt
state, immutable rows, copied/foreign/replaced stores, changed owner policy,
explicit schema upgrade, transactional rollback after an injected insert failure,
and corrupt report hashes. The first broad run found older-schema test fixtures
that still contained the new table; those fixtures now remove it when reproducing
historical schemas. Production migration still refuses conflicting tables.

The sealed verifier result also holds an `O_PATH` descriptor to pin its source
inode until retention finishes. This prevents inode reuse without interfering
with SQLite's process-owned POSIX locks. A cross-process rollback-journal test
checks lock exclusion after the result is dropped; a separate negative-control
experiment confirmed that closing an ordinary I/O descriptor releases the lock,
while closing `O_PATH` preserves it.

Final validation passed: 383 library tests (four opt-in live tests ignored),
12 memory integration tests, 47 CLI integration tests, and three compile-fail
language/API tests. Both default and state-store builds checked successfully.
The final real `live_production_native_profile_verification` run launched and
stopped Codex 0.154.0 through patched Herdr, retained its sealed evidence, and
retrieved the identical report after reopening the store. It used an empty
execution home, no credentials and no prompt. Logs:
`/tmp/herdr-retained-profile-library-final.log`,
`/tmp/herdr-retained-profile-memory.log`,
`/tmp/herdr-retained-profile-cli-tests.log`,
`/tmp/herdr-retained-profile-doc-tests.log`, and
`/tmp/herdr-retained-profile-native-live.log`.

### Revalidated retained profiles (2026-09-21)

Added `profile revalidate PROJECT DIGEST` and a sealed `RevalidatedProfile` library
result. Revalidation reads only canonical retained evidence with SQL deadline,
cancellation and row-size controls. It checks evidence shape/chronology, rebuilds
the original unknown-capability baseline, and derives capability claims using the
same code as the native verifier. Rehashed claims of resume, workflow certification
or protocol capability cannot promote the result.

Current preparation re-observes owner policy/configuration, exact executable
hashes and versions, and execution-home ownership. Changed executable bytes are
refused before version probing. The live result pins both the database and home
inodes, holds the root maintenance barrier, and expires at the original deadline
(maximum twenty seconds). Its `launch_profile()` method rechecks mutable inputs,
cancellation and expiry. Launch/stop-only native evidence remains unlaunchable;
launchable fixture evidence remains uncertified and not protocol-capable. The
printed report cannot reconstruct this live proof.

Regressions cover forged but consistently rehashed capability/report claims,
changed binaries without executing the changed program, later policy/home/store
replacement, cancellation, original deadline expiry, and root-barrier ownership.
An initial deadline fixture gave the version supervisor less than its required
five-second cleanup allowance; the fixture now supplies eight seconds and then
checks expiration of that same deadline. Production budgets were not changed.

This provides current profile inputs for the forthcoming trusted reservation
producer. Task/repository/memory binding, owner approval, new-launch controller
admission and the remaining dispatch audit still apply. The authenticated prompt
test remains unexecuted pending the earlier explicit authorization request.

Final validation: all 388 library tests passed serially (four opt-in live tests
ignored), as did four compile-fail documentation tests, CLI help/build validation,
and the default-feature check. The real Codex 0.154.0 test passed native
launch/stop, retention, and current-installation revalidation against patched
Herdr using an empty execution home. It submitted no prompt and used no
credentials. Revalidation preserved the project event head and correctly kept
that launch/stop-only profile unlaunchable. Logs:
`/tmp/herdr-revalidated-profile-library.log`,
`/tmp/herdr-revalidated-profile-doc-tests.log`,
`/tmp/herdr-revalidated-profile-cli-help.log`, and
`/tmp/herdr-revalidated-profile-native-live.log`.

## Trusted launch drafts and reservations — 2026-09-21

Added `launch PROJECT draft` and `launch PROJECT reserve`. Selectors name an
existing task/binding, retained native profile, retained worker knowledge and
optional repository roots. The service revalidates the profile, pins local Git
commit/tree identities, reconstructs policy/control/task revisions, and uses shared
store admission checks. A draft is read-only and contains the complete brief plus
an unsigned exact approval document. Reservation requires the installed owner
signature and atomically records the attempt and launch intent; approval consumption
remains at the external-effect claim. Raw JSON cannot construct sealed authority.

Git observation disables replacement refs, ambient configuration and lazy fetching,
rejects partial clones, and runs within the original supervised deadline. Narrow
supervisor environment exceptions accept only exact restrictive values for clean
`/usr/bin/git` invocations. No credential helpers or network fetch are needed.

Added CLI worker-snapshot creation. Missing or invalid project instructions now
refuse instead of silently becoming empty. Launch previews use retained text and
the eventual content-derived attempt ID, and validate the complete protocol budget.
Memory rendering now bounds body reads before hashing/allocation, retains Unicode
character semantics, and enforces an aggregate byte cap. Consumed evidence is checked
before reservation.

Validation: 394 library tests passed, four opt-in live tests skipped; 12 memory
integration tests and five compile-fail documentation tests passed. A focused CLI
test verifies worker snapshots, retained Unicode instructions, missing/invalid
instruction refusal without state changes, and launch subcommand help. The default
feature build check and diff whitespace check passed. Logs:
`/tmp/herdr-launch-final-library.log`, `/tmp/herdr-launch-memory-integration.log`,
`/tmp/herdr-launch-doc-tests.log`, `/tmp/herdr-launch-cli.log`, and
`/tmp/herdr-launch-default-check.log`.

The controller still refuses new launch creation and only observes already-started
resource boundaries. Worktree creation/preservation, unobserved-process recovery,
new-launch controller admission and full live acceptance remain open in the dispatch
audit. No authenticated test or credential access was performed; its prior explicit
authorization request remains pending. The overall dispatch goal is not complete.

## Atomic resource-creation admission — 2026-09-21

Review found a crash window between the launch claim/approval-consumption commit
and the separate creation-intent commit. The native adapter now constructs its
intent first, then commits approval use, the delivery claim and the recovery intent
together. The native request follows only a successful commit. The unused public
split-write API was removed; existing historical events remain readable.

Real SIGKILL tests stop a child after claim mutation but before intent insertion,
after intent insertion but before commit, and after commit. Reopening SQLite proves
the first two cases retain the exact pre-claim snapshot and allow ordinary
never-claimed cancellation; the last preserves the claim and intent together,
consumes approval once and refuses replay. Native-adapter tests inject failures at
approval-use, claim-event and creation-event insertion and verify no external
creation request occurs before a successful retry. Existing lost-reply recovery
continues to retain capacity and never recreate resources.

This closes the new-launch claim/intent gap. It does not infer termination from
missing panes, repair historical claims without intent, or resolve process exit
before observation. Worktrees, broader resource lifecycle recovery, controller
admission and authenticated live workflow acceptance remain required.

Validation: all 397 library tests passed (four opt-in live tests skipped), including
16 focused creation tests. The disposable real-Herdr creation/release/termination
test then passed separately with an isolated empty home and a sleep worker; it
used no credentials and submitted no vendor-agent prompt. Logs:
`/tmp/herdr-atomic-creation-focused.log`, `/tmp/herdr-atomic-creation-library.log`,
and `/tmp/herdr-atomic-creation-native.log`.

## Signed worktree preparation stage — 2026-09-21

Added deterministic worktree plans to launch drafts and a Linux library service
that provisions those exact plans after reservation. It validates the retained
brief, profile/configuration/executable inputs and bounded local repository
objects before consuming approval. The claim, approval use and worktree creation
intent commit atomically before Git or output-directory writes. Branches and
paths derive from the actual attempt; existing paths/branches, configured filters,
partial clones, duplicate common repositories and unsupported tree entries refuse.

Creation uses a supervised, clean-environment Git process, disabled hooks/monitors/
maintenance/lazy fetch, requested fsync and a per-intent worktree lock token.
Source dirty files remain untouched. Observation verifies native Git associations
in both directions, branch/base/tree, lock token and pinned directory incarnations.
An independent bounded comparison of actual bytes and executable modes to Git
tree/blob objects prevents stat caches and assume-unchanged flags from hiding
modified files. Binary contents and symlinks are supported; unexpected files refuse.

Creation is one-use. Repeated calls only observe and can recover a lost receipt
commit after approval revocation. Missing, partial, changed or replaced resources
retain capacity and data. No worktree or branch removal was introduced.

All 401 library tests passed (four opt-in live tests skipped). New signed fixtures
cover dirty-source preservation, multiple repositories, binary files and symlinks,
hidden modifications, unavailable/unsafe inputs, intent rollback with unused
approval, and lost-receipt recovery without another Git add. Log:
`/tmp/herdr-worktree-library.log`.

This is a preparation stage with no CLI exposure yet. The native adapter still
refuses repository-backed starts. Connecting its retained worktree receipts to
native creation under the same claim, working-directory selection, cross-project
ownership inventory and start/stop/preservation remains required. Neither the
dispatch goal nor the worktree lifecycle gate is complete; see
[worktree details](canonical-worktrees.md) and the dispatch audit.

## Staged worktree ownership inventory — 2026-09-21

Added a bounded read-only worktree-reference inventory, including creation intents
whose Git request or ready-receipt commit was uncertain. It validates canonical
publication, content-addressed attempt inputs, task/binding/operation provenance,
the original consumed approval and claim generation, deterministic plans, and any
completed receipts. Missing joins, orphan receipts, duplicates, mismatched paths,
oversized records and exceeded cancellation/record budgets refuse the scan.
References survive missing files and terminal attempts; no cleanup authority or
implicit release is inferred.

Canonical/legacy adoption now includes these staged references. Its canonical
binding scan also uses the bounded identity reader instead of full neighboring
snapshots. New worktree provisioning checks root-wide canonical and legacy path
references before consuming approval. Equal, parent and child paths conflict;
the current binding's own retained references can still be observed for recovery.

Validation: ten focused signed launch/worktree tests, ten runtime-ownership tests,
nine publication/inventory tests and four legacy-adoption tests passed. A final
focused provenance regression additionally passed after adding missing-binding
checks. The initial oversized-input fixture was corrected to contain valid JSON
so it exercises the inventory's field bound rather than SQLite's JSON constraint.
Logs: `/tmp/herdr-worktree-inventory-focused.log`,
`/tmp/herdr-worktree-adoption-tests.log`,
`/tmp/herdr-worktree-publication-tests.log`,
`/tmp/herdr-worktree-legacy-adoption-tests.log`, and
`/tmp/herdr-worktree-inventory-provenance.log`.

Worktree receipts still need to feed native creation under the existing claim,
working-directory selection, native-start ownership and preservation. New-launch
controller admission and the other dispatch acceptance gates remain open.

## 2026-09-22 — Repository-backed native handoff

Connected approved worktree receipts to native resource creation, gate release,
start ownership and the complete retained worker brief. Continuation uses the
original claim and consumed approval. Root and nested working directories map
into the selected checkout; source-only directories fail before approval use.
Before release, checkout bytes must match the approved tree. Start instead pins
incarnations and Git backlinks/lock tokens, allowing legitimate worker output.

Verification: the shared library suite passed 404 tests (four live tests ignored)
before the final directory preflight additions; the expanded signed-ingress suite
passed 13 tests afterward. The disposable repository-backed patched-Herdr
creation/release/stop test passed in an optimized build (3.61 seconds), preserving
checkout output and source contents. The debug run refused with insufficient
cleanup budget; neither production deadlines nor the claim lease were extended.
No account credentials were used. Controller scheduling, worktree-only
cancellation recovery, full preservation acceptance and authenticated vendor
workflow evidence remain outstanding; dispatch stays disabled.

Final focused checks also passed: complete fixture launch/brief/stop recovery,
two full-brief budget tests (including Unicode checkout mappings), and all 12
memory-control integration tests.

## 2026-09-22 — Worktree-only cancellation and lease-expiry recovery

Added atomic retirement before any native launch intent exists. The termination
adapter requires the original reservation, consumed approval and valid worktree
inventory, then acquires the root barrier inherited by preparation descendants.
It can release capacity after cancellation or expiry without deleting or
certifying checkout data. The terminal delivery fences stale and fresh native
continuation attempts. Controller hints now offer this recovery as bounded
termination work. Native-intent uncertainty still requires native recovery.

Fixed retained ownership inventory after lease expiry: it accepts exactly the
single epoch advance supported by the original claim's expiry outcome event,
including its revision and ambiguous outcome, and rejects unexplained changes.

Validation: 408 library tests passed (five live tests ignored); the final focused
regression passed after tightening expiry provenance. Cases cover live preparation
no-op, inherited-lock exclusion, ready/partial/missing checkouts, expiry, corrupt
expiry evidence, native-intent refusal, transaction rollback, late continuation,
idempotence and preserved ownership references. Nine controller regressions
passed. New-launch dispatch remains disabled; controller launch scheduling,
complete preservation acceptance and the remaining native/live workflow evidence
are still required.

## 2026-09-22 — Connected-server capability admission

Inspection before controller wiring found that an exact client binary did not
prove the connected server implemented the local direct-root creation extension.
The local Herdr patch now advertises `workspace_create_command` through the
existing read-only `ping` response. The adapter checks the expected version and
explicit boolean capability before approval consumption. Repository preparation
performs the same check before creating worktrees; native creation rechecks it.
The earlier unadvertised patch build is intentionally refused for fresh roots.
Existing resource recovery remains based on retained identity evidence.

The negative regression covers missing, false, malformed, wrong-type and
version-mismatched advertisements for both repository and non-repository launches;
all preserve canonical state and create no checkout or workspace. Herdr's optional
capability/schema and literal-command validation tests passed. The updated patch
reverse-applies cleanly to the disposable source checkout. An optimized production
adapter test against `/tmp/herdr-capability-live` passed repository creation,
release and termination in 3.91 seconds without credentials or relaxed limits.
The installed runtime was not changed. Automatic launch scheduling remains next;
this capability advertisement is not vendor workflow certification.

Final validation: all 409 library tests passed (five live tests ignored), and
all-target compilation passed. The explicit native test and three focused Herdr
schema/validation tests above were run separately.

## 2026-09-22 — Prepared-launch controller wiring

Added bounded selection of due reserved launches and live original claims,
rotating with brief and termination work without duplicate launch hints. Paused,
cancelled, expired and reconciliation-required cases cannot receive advancement
hints. Foreground and executor controller routes now distinguish advancement from
observation. Executor input retains the original deadline and cancellation token;
concrete launch ingress continues to validate all authority and durable boundaries.

Found and fixed a queue integration issue: the generic 30-second error backoff
could exhaust the entire original launch lease after a lost reply. Launch jobs
now use a distinct queue identity and one-second failure/admission backoff, with
ordinary project fairness preserved and no lease renewal or effect replay.

Validation: the full library run passed 409 cases, including controller-selected
launch/brief/stop and lost-reply recovery. One new selector test incorrectly
modified reconciliation state without its publication marker; the reader correctly
refused it. After correcting the test to cover both inconsistent and consistent
publication, that test passed. Seven queue tests, the executor action/deadline test
and nine controller regressions passed. All-target compilation passed.

The production `PREPARED_LAUNCH_DISPATCH_ENABLED` gate remains false. Enabling it
still requires complete enabled-ticker acceptance and the remaining preservation,
native recovery and authenticated vendor-workflow evidence in the dispatch audit.

## 2026-09-22 — Retain creation observations after authority changes

Creation receipt storage was still coupled to a live effect claim even after the
adapter had observed the exact supervisor. It now uses the existing typed
observation ingress with current delivery/head fencing. The original creation
intent, consumed approval, binding and retained reservation must still match;
expiry, revocation and cancellation do not discard the identity needed for stop
recovery. Gate release still checks current authority and cannot renew the claim.

The focused regression passed six cases: direct-root and existing-workspace tab
creation, each interrupted by expiry, revocation or cancellation while the native
reply is pending. All retain the target, refuse gate input and release capacity
only after exact termination, without repeated creation or approval consumption.
The earlier gap (exit before any exact supervisor observation) remains open and
requires stronger native lifecycle evidence; absence is still not treated as proof.

Final validation: all 411 library tests passed (five live tests ignored). The final
focused regression also verifies rejected gate release leaves canonical state
unchanged. All-target compilation passed. Dispatch remains disabled.

## 2026-09-22 — Per-attempt output and cancelled-task preservation

Canonical workers lacked a recorded artifact source and an explicit report path.
Version 2 briefs now name a deterministic `.state/worker-output/<attempt-id>`
directory for report.md and library/. Start records that exact path in the runtime
binding. Preview, retained rendering and prompt hash/budget accounting agree.
Worker snapshot estimator v2 reserves the exact escaped output-path/instruction
framing before optional memory selection; older contracts require a new snapshot
and approved attempt rather than silently acquiring changed instructions.

Explicit finalization now accepts terminated cancelled tasks while preserving their
cancelled state and task revision. It still requires no active/retained attempt,
exact recorded source, current authority, verified bytes and an operation-bound
receipt. This adds report/library preservation, not whole-repository archival or
permission to delete worktrees.

Validation: controller-selected launch/brief/stop pipeline and cancelled-task
byte-preservation tests passed. The initial full library run passed 403 cases and
found nine outdated fixture assumptions (brief ending and noncanonical DB layout).
After updating those fixtures, all 45 reservation tests and 56 memory tests passed,
including the nine affected cases and SIGKILL receipt recovery. All 32 finalization
tests, 12 memory integration tests and all-target compilation passed. Dispatch
remains disabled pending the other requirements in the audit.

Final full-library rerun after the fixture corrections: 412 passed, zero failed,
five live tests ignored (103.94 seconds). The ignored tests are not counted as
live acceptance evidence; the dispatch gate remains disabled.

## 2026-09-22 — Prevent worktree inspection from blocking on special files

Reviewing the prerequisites for repository preservation found that worktree
metadata opened a pathname before checking its file type. A replaced `.git`,
`gitdir`, `commondir`, or lock file could therefore be a FIFO that indefinitely
held up recovery under root ownership. Checkout byte verification had the same
window between its pathname stat and open.

Both readers now share a no-follow, nonblocking open and validate the actual
descriptor as a single-link regular file before reading. Existing byte, size,
mode and incarnation checks remain in place. A child-process regression with a
bounded parent watchdog verifies that a FIFO without a writer is rejected;
other cases cover regular files, oversized metadata, symlinks, directories and
hard links. All 13 worktree-filtered tests and the complete fixture
creation/release/brief/stop pipeline passed. This closes an inspection hang, not
the still-open whole-repository preservation or native lifecycle requirements.

## 2026-09-22 — Keep recorded worktree branches truthful at start

Post-release worktree proofs checked directory incarnations, backlinks and the
creation lock but omitted HEAD's branch association. A switch before start could
therefore leave the canonical binding claiming the approved branch while its
checkout used another branch or a detached commit. The shared observation check
now requires the linked Git HEAD to name the retained plan's branch. It still
allows worker edits and new commits on that branch after gate release.

Regression coverage changes each association file, points HEAD at an existing
foreign branch and a detached commit, then makes a real commit on the approved
branch. Rejections and successful observations leave canonical records unchanged.
All 13 worktree tests and the fixture launch/recovery/brief/stop pipeline passed.
Repository archival and the other dispatch acceptance gaps remain open.

## 2026-09-22 — Retain capacity while historical bootstraps are unresolved

Historical root creation can leave an unsupervised bootstrap shell separate from
the supervised worker. Both termination transactions previously accepted worker
exit alone and could free capacity while that extra executable resource remained
unresolved. They now inspect retained creation/workspace provenance inside the
transaction and refuse the capacity-release receipt for historical bootstrap
launches. Cancellation may still stop the exact worker; canonical task, attempt,
delivery, approval and ownership records remain unchanged on the refusal.

The launch pipeline now exercises a fully started historical workspace as well
as current supervised roots and existing-workspace workers. Staged historical
launches are covered with normal and lost-layout acknowledgments. Tests check
the explicit diagnostic, retained capacity and unchanged state on repeated
reconciliation. All 49 canonical-worker tests passed (two live tests ignored).
This fixes premature release; dedicated bootstrap quiescence/recovery evidence
is still needed before these historical attempts can be retired.
All 45 reservation/receipt tests also passed, including process-death recovery.

## 2026-09-22 — Bind supervisor recovery to the recorded host

Reboot recovery previously accepted any changed kernel boot ID as termination,
without distinguishing a same-host reboot from records copied to another host.
New supervisor identities are version 2 and include a domain-separated hash of
the local machine ID. Reconnect, stop and exit observation require that identity
to match. A changed boot can establish exit only with retained matching host
evidence. Version 1 JSON remains compatible and usable in its recorded boot;
cross-boot recovery of those records now explicitly retains uncertainty.

Tests cover same-boot, same-host reboot, foreign/missing host identity, version
validation and legacy serialization. The live supervisor fixture verifies that
foreign-host evidence cannot reconnect, certify exit or signal its still-live
process. All 416 library tests passed (five live tests ignored), the focused
no-default-features compatibility test passed, and all-target compilation passed.
This supplies the host-identity prerequisite for reboot recovery; it does not
yet retire historical bootstraps or prove reboot acceptance with a live reboot.

## 2026-09-22 — Retire historical bootstraps with same-host reboot evidence

Staged and started termination now obtain typed same-host reboot evidence from
the native observer and retain it in the stop receipt. It names the host hash,
previous boot and observed current boot. The store validates that evidence
against the exact retained supervisor before allowing historical bootstrap
capacity release. Worker exit in the same boot still cannot discharge bootstrap
quiescence. Missing/foreign host evidence, old version-1 supervisor identities
after reboot, and bootstrap-only creations without supervisor evidence remain
unresolved. No workspace, worktree, report or ownership reference is deleted.

Tests simulate persisted prior-boot records after stopping the real fixture
worker; no machine reboot was performed. They cover staged and started retirement,
normal/lost layout acknowledgments, repeated reconciliation, rejection of missing
or mismatched evidence, and atomic rollback/retry after receipt-commit failure.
All 49 canonical-worker tests passed (two live tests ignored), as did the final
expanded staged-evidence test, 45 reservation tests, evidence validation tests,
and all-target compilation. Same-boot bootstrap cleanup and the other dispatch
requirements remain open.

## 2026-09-22 — Durable working-file snapshots for stopped checkouts

The plan audit confirms that ambiguous resources must remain blocked and that
cancellation must preserve partial results. Report/library finalization alone
does not cover checkout changes. Added an explicit library capture service for
all retained worktrees of a terminated attempt. It uses controlled database reads,
the root barrier, exact retained worktree proofs, shared bounded source traversal,
two byte-based scans and content-addressed files verified after durable writes.
The manifest is published last, includes executable bits and empty directories,
and identifies its limited `working_files` scope. No source, ownership record,
task state or Git metadata is modified. History/index preservation and automatic
finalization integration are not implemented by this step.

Three focused tests passed for special-file refusal, cancellation/deadlines,
same-size/same-mtime edits, interrupted publication and corrupt retained bytes.
The full fixture launch/brief/stop pipeline passed with active-attempt refusal,
binary/untracked output capture, idempotence and corruption refusal. The separate
multi-repository preparation/stop/capture test and all eight shared filesystem
tests passed. All-target compilation passed. Dispatch remains disabled.

## 2026-09-22 — Preserve recoverable Git history and staged state

Added repository-state capture alongside the existing working-file format. It
retains approved/current history and every indexed object in a bounded portable
pack, plus logical index entries, exact index bytes and referenced shared-index
bytes. Working files remain separately captured, so a later unstaged edit cannot
replace staged-only evidence. Git runs with the inherited execution lock, cleared
environment, no hooks/lazy fetch/replacement objects/automatic maintenance, fixed
packing limits and the original deadline. Index/ref state is rechecked around
packing, file verification and manifest publication. Capture remains separate
from task-result acceptance, restoration and cleanup authorization.

The restoration fixture initially omitted required execution ownership; it was
corrected rather than weakening the supervisor. Independent restores now pass
after source object databases are removed, covering normal/split/conflicted
indexes, SHA-256 repositories and split indexes, intent-to-add and index flags.
The same test checks repeatability, byte-budget refusal and changed-index
rejection. All four preservation tests, multi-repository capture and the final
launch/brief/stop/preservation pipeline passed. All-target compilation passed.
Automatic cancellation/finalization integration and the other dispatch gates
remain incomplete.

## 2026-09-22 — Capture repository state before native terminal disposition

Native staged/started termination now proves supervisor and workspace quiescence,
captures all approved repositories under the retained root barrier, then commits
the stop receipt and terminal disposition together. Receipts carry ordered plan
and manifest-digest references; the store rejects missing/mismatched coverage.
Capture failure leaves desired cancellation and capacity intact after any real
worker stop. A failed receipt commit retains the published snapshots for verified
retry. Neither recovery path relaunches a worker, refunds an approval, removes a
source, or treats preservation as task success.

The native stop phase remains capped at ten seconds. Overall termination/capture
uses the original admission deadline capped at 45 seconds, with cancellation and
deadline rechecked before commit; foreground controller admission now matches
the existing queued-job budget. Report/library capture and worktree-only partial
preparation preservation remain separate unfinished integrations.

All 49 existing canonical-worker tests passed, including destination-symlink
refusal and rollback/retry after repository capture. A new staged-worker test
passed for missing-reference rejection, retained partial bytes and idempotence.
All nine controller tests passed, followed by the final full launch/brief/stop
pipeline and all-target compilation. Dispatch remains disabled.

## 2026-09-22 — Preserve attempt outputs before native terminal disposition

Staged and started native termination now captures the dedicated attempt output
source after exact process/workspace quiescence, under the same root barrier and
original deadline as repository capture. Stop receipts require the exact source
and either a content-addressed manifest digest or an explicit absent-directory
observation. Existing empty directories receive manifests. Partial reports,
binary library files and ordinary `.git` output data are retained, with bounded
descriptor-based traversal, repeated byte scans and durable verified publication.
Missing or malformed receipt coverage is rejected before canonical state changes.

Source/destination symlinks and failed receipt commits leave cancellation and
capacity intact. Retry observes the stopped worker and verifies/reuses retained
blobs and manifests. No source is deleted. Explicit artifact finalization still
reads the original output source; consuming retained snapshots after source loss,
worktree-only partial preparation preservation and cleanup acceptance remain open.

All 51 canonical-worker tests passed (two opt-in live tests ignored), including
report/library bytes, empty versus absent sources, missing/malformed evidence,
unsafe paths and failed-commit retry. All four repository-preservation tests
passed, including independent Git restoration. State-store all-target compilation
and tracked diff whitespace validation passed. Dispatch remains disabled.

## 2026-09-22 — Finalize receipt-bound outputs after source loss

Added a bounded output snapshot reader that checks the manifest digest, exact
attempt/source identity, safe unique paths, parent structure, entry/depth/byte
limits, and every referenced blob. Descriptor-based reads reject links, special
files and substituted ancestors; original cancellation and deadlines apply.
Canonical finalization resolves evidence from the exact binding's termination
receipt and immutable launch inputs, requiring observed termination and released
capacity. An explicit absent-output observation or corrupted snapshot fails
closed rather than falling back to replacement source bytes.

Both foreground and queued finalization can now publish report/library artifacts
from verified stop snapshots without recreating the former worker directory.
Existing artifact manifests, authority rechecks, receipt publication and atomic
database disposition remain in use. Historical bindings without snapshot receipts
retain local-source capture. This adds no source deletion or automatic repository
restoration authority and does not certify task success.

All six preservation tests passed, including malformed manifests, source-free
reads and corrupt/unsafe storage. The full fixture launch/brief/stop pipeline
passed after deleting original outputs, verifying bound recovery and refusal of
changed termination/source evidence. All 33 finalization-related binary tests
passed. A new artifact conversion test matched the original capture's exact
manifest/digest and verified publication-authority rejection. The strengthened
foreground/queued test passed for absent source, cancelled-task preservation, and
corruption despite a replacement source. Its database fixture represents persisted
historical evidence; native stop provenance is independently exercised by the
launch pipeline. Dispatch remains disabled pending the completion audit's other
gates.

## 2026-09-22 — Preserve cancelled or expired preparation before retirement

Worktree-only retirement now validates original intent, claim/approval provenance,
absence of native effects and root-barrier quiescence, then captures repository
state and attempt outputs before the atomic stop transaction. Existing checkouts
need exact creation-token, branch and Git associations; ready receipts additionally
fence directory incarnations. Partial or modified content and a missing index can
be captured without certifying readiness. Plans lacking ready receipts can record
absence only when directory, branch and Git registration are absent both before
and after capture. Missing recorded resources or incomplete ownership metadata
remain blocked with capacity and all references retained.

Version-2 worktree stop events retain ordered repository snapshot/absence evidence
and output snapshot/absence evidence. Capture failures or failed commits leave
state unchanged, with durable captures available for verified retry. Active,
uncancelled preparation makes no snapshot. No source or branch is removed and
approval remains consumed.

The broader ingress run exposed a compatibility defect: launch supported repository
symlinks, but capture rejected them. Repository scans now preserve literal link
target bytes through bounded O_PATH/readlinkat reads without dereferencing them.
Manifests containing links use version 3 and explicit symlink entries. Hard links
and special nodes remain refused; attempt output sources still reject symlinks.
Tests restore a preserved link independently, while leaving the original intact.

All 15 launch-ingress tests passed, including ready/partial/missing/absent/expired
preparation, rollback/retry, unsafe destination refusal, malformed evidence, and
two repositories with an absent first checkout and index-less second checkout.
All six preservation and nine shared filesystem tests passed, plus artifact
conversion and the native launch/brief/stop pipeline after the capture refactor.
Dispatch remains disabled; cleanup acceptance and the other audit gates remain.
State-store and default-feature all-target compilation passed, along with tracked
diff whitespace validation.

## 2026-09-22 — Enabled controller effect-path acceptance across projects

Added a cross-binary acceptance test: the library fixture owns canonical reservations with internally prepared approvals,
repository resources and supervised sleep processes, while the binary-test driver
uses the real controller selector, effect queue, executor and native launch/brief/
termination services. The test enables selection through an internal parameter;
the production constant remains false and is checked to select no fresh launch.

Three repository-backed projects exercise two successful launches, one lost naming
acknowledgment, another project's admission before retry, and cancellation of the
third after queue offer but before admission. Cancellation performs no native
creation or approval consumption. A fresh queue handles the two running attempts'
cancellation from durable records; retained repository/output evidence is checked,
including report/library bytes. Creation, release, naming and exact retained
memory-brief delivery occur once. `scripts/test-canonical-dispatch` discovers both
executables from Cargo JSON output and runs the fixture reproducibly.

The acceptance script passed. All nine existing controller tests passed after
rerunning with permission for their disposable Unix socket; the first sandboxed
run passed eight and failed socket creation with EPERM. This is protocol-fixture
evidence across independent project roots. Whole-ticker background maintenance,
same-root contention, trusted preparation integration and live vendor certification
remain separate open requirements in the dispatch audit.

### 2026-09-22 — Whole-ticker shared-root launch acceptance

Added a second cross-executable dispatch fixture that drives the actual ticker
body with production background services and polling delays. Three repository
projects share a root; two must receive their retained brief, positive Herdr/Git
observations, and survive an executor restart followed by cancellation with
verified output preservation. The third is cancelled after its launch offer and
must never create resources or consume approval. The first loses its naming reply;
creation, gate release, naming and brief delivery remain one-use.

The fixture exposed maintenance probes repeatedly holding shared root ownership
while native launch stages require exclusive ownership. Admission now drains the
whole observation batch before an exclusive worker effect, then grants maintenance
a batch after that effect. The original 15-second polling interval also consumed
launch time across these stages. Active canonical work now uses 250 ms polling,
with the existing one-second launch retry delay; successful termination checks
cool down for 15 seconds so live-worker observations do not crowd out preparation.
Worker ownership and durable claim checks remain authoritative.

Both dispatch acceptance fixtures passed, including the shared-root ticker case
in approximately eight seconds. Queue regressions: 8 passed; ticker regressions:
23 passed; canonical controller/maintenance regressions: 57 passed (the two
cross-executable driver entrypoints are inert without their parent fixtures).
The cooldown regression initially asserted against a refreshed offer timestamp;
that test assertion was corrected and the complete queue suite rerun successfully.

This closes fixture coverage for whole-ticker maintenance and shared-root
contention. Internally prepared approvals and supervised sleep processes do not
certify trusted profile/signature ingress or authenticated vendor readiness and
workflow behavior. Those requirements, early-exit identity recovery and cleanup
acceptance remain in the dispatch audit; production dispatch stays disabled.

All-target checks passed with default features and with `state-store`; the final
state-store check is warning-free. `git diff --check` also passed.

### 2026-09-22 — Signed approval import in dispatch acceptance

Removed internal approval installation from both controller/ticker acceptance
fixtures. They now generate a disposable Ed25519 owner key, pin its public key in
the migration/configuration authority policy, sign the exact grant and import it
through `authority::import_signed`. Before accepting that grant, each project
tries a modified document against the original signature and verifies refusal,
an unchanged project head and no installed approval. The accepted grant then
flows through reservation, queued launch, brief delivery and cancellation;
cancelled queued launches still leave approval unconsumed.

Both cross-executable dispatch acceptance tests passed with this signature path.
The full canonical worker suite passed: 51 tests, with four explicit integration
tests ignored by the ordinary suite. Two of those are the dispatch tests run by
the script; the other two require an external live Herdr fixture. `git diff
--check` passed.

Profile capability evidence and reservation construction are still supplied by
internal test fixtures. This adds genuine owner-signature ingress coverage, but
it does not close the full trusted profile/draft/reservation-to-ticker acceptance
gap or establish live vendor certification. Production dispatch remains disabled.

### 2026-09-22 — Production preparation/reservation APIs connected to ticker

The dispatch fixtures now use production installation preparation, native report
retention/revalidation, launch drafting, signed approval import and reservation
instead of constructing their reservations internally. A small C fixture process
provides an exact executable identity and version response without starting a
vendor agent. The retained full worker snapshot and repository inputs flow through
the production draft into the one-use native launch and brief delivery path.

The only capability-evidence shortcut is isolated in the Linux test-only
`profile_preparation::fixture` module: it supplies synthetic native interaction
evidence to the normal retention and revalidation path. This module is absent
from production builds. It does not represent live vendor certification.

Before each successful reservation, the test modifies the retained Herdr
executable after signing approval. Production revalidation must reject that
reservation with an identical before/after snapshot. Restoring the exact bytes
allows the same signed grant to reserve normally. Existing signature tampering,
queued cancellation, lost naming reply, restart, one-use brief delivery and
repository/output preservation assertions remain in both dispatch fixtures.

Validation: both dispatch acceptance scenarios passed; worker regressions passed
51 tests (4 explicit integration tests ignored by that ordinary suite), and
profile/preparation regressions passed 31 tests (3 live integrations ignored).
All-target checks passed for default and `state-store` configurations, and
`git diff --check` passed. Dispatch remains disabled pending the live vendor
workflow and the remaining recovery/preservation acceptance in the audit.

### 2026-09-22 — Visible recovery blocker for unobserved creation

The recovery plan previously described an empty runtime binding as having no
external resources even when a native creation intent already existed. Generic
claim-expiry advice also omitted the missing process identity. The plan now joins
creation events to retained attempt inputs and distinguishes this case with
`inspect_launch_identity` advice for the attempt and runtime. A live claim still
gets `wait`; an expired/ambiguous operation requires exact creation evidence.
Event and retained-attempt sets are computed once per plan.

The existing real supervised-worker early-exit fixture now also checks these
reports with fresh, revision-fenced absent-resource observations. It proves that
no identity was recorded, creation occurred once, capacity stays held, and the
report leaves the snapshot unchanged before and after expiry. Its first run used
pre-reservation observation revisions and correctly failed the report's fence;
the test now constructs the current observation batch.

Validation: the early-exit fixture and both existing recovery-plan regressions
passed; all-target checks passed with default and state-store configurations.
This fixes misleading diagnostics. It does not invent termination evidence or
close automatic recovery when no process identity was ever recorded. Production
dispatch remains disabled.

### 2026-09-22 — Bound unresolved creation polling

Observation-only native resource recovery previously shared immediate successful
re-admission behavior with launch advancement. An unresolved creation with an
empty process inventory could repeatedly take exclusive root access at the fast
ticker interval despite gaining no evidence. Recovery now has a distinct volatile
queue identity and a 15-second successful-poll cooldown, like termination checks.
The original live launch claim retains its independent advancement identity and
short retry delay. Cooled-down recovery alone does not trigger fast polling.

Both dispatch acceptance scenarios passed. Queue regressions passed 9 tests, the
canonical job routing regression passed, and ticker regressions passed 23 tests.
The early-exit fixture now covers both historical tab creation and current
supervised-root creation; both preserve capacity, avoid recreation and report
missing identity without mutating state. T03.4 permits visibly blocked ambiguous
cases; the audit now distinguishes that supported outcome from unproved live
crash-boundary acceptance rather than promising unsafe automatic release.

Asked for explicit authorization for the prepared isolated authenticated Codex
diagnostic. The earlier automatic approval review rejected copying/using the
login cache without that authorization. No credential access or authenticated
prompt was performed; the question remains pending.

All-target checks passed with default and state-store features; `git diff --check`
passed. Production dispatch remains disabled while acceptance requirements remain.

### 2026-09-22 — Canonical references protect legacy cleanup

Legacy cleanup previously tried loading every neighboring project as legacy, so
any canonical neighbor prevented cleanup even when unrelated. Extracted the
existing bounded ownership inventory traversal and added destructive-path overlap
checks for canonical bindings, native launch targets, retained worktree plans and
legacy working/output directories. The CLI preparation and cleanup ingress both
check those references under the existing root lease before a removal reservation
or Git effects. Adoption conflict semantics remain unchanged.

Missing paths resolve through existing ancestors, so a not-yet-created child below
a symlink still protects its real parent. Dangling aliases fail closed. The Git
fixture covers equal, ancestor, missing-child, alias-child and dangling references,
corrupt canonical metadata, and successful non-force cleanup beside an unrelated
canonical project. Refusal leaves source report bytes and the removal reservation
unchanged. This supplies mixed-root cleanup evidence, not authority to delete
canonical resources or release their retained ownership.

Initial host-namespace cleanup tests refused writer quiescence because an existing
same-user process's /proc entries were inaccessible. Production checks were not
relaxed. In a disposable user/PID/mount namespace, all 5 cleanup tests passed;
all 10 canonical ownership regressions also passed. The final cleanup rerun checks
specific conflict/alias errors so unrelated refusal cannot hide an ownership bug.

All-target checks passed with default and state-store features, and `git diff
--check` passed. Authenticated acceptance remains pending authorization; production
dispatch stays disabled.

### 2026-09-22 — Protect removed paths during reopen

Extended the bounded cross-project ownership check to legacy worktree reopening.
The shared path check now resolves absent targets through existing ancestors,
so references acquired by canonical projects after cleanup prevent recreation.
This also removes the former blanket refusal beside unrelated canonical projects.
Existing branch/head, snapshot, registration and real-parent checks remain.

The new Git regression removes the legacy worktree, adds a canonical reference,
and tests equal paths, missing children, aliased paths, corrupt metadata and an
unrelated project. Refusal keeps the path absent, registration absent and removal
record unchanged; successful unrelated reopening retains the preservation snapshot.
All 6 cleanup/reopen tests passed in a disposable PID namespace and all 10 ownership
regressions passed. Default and state-store all-target checks passed.

### 2026-09-22 — Live native stop preserves recoverable bytes

Strengthened both live Herdr canonical creation/stop contracts. A real disposable
server creates/releases the exact supervised sleep process. The test records
report/library bytes before cancellation, verifies namespace termination and the
stop receipt, deletes the disposable output source, then verifies exact retained
text and binary bytes through the production snapshot reader. The repository
variant also validates receipt-bound manifest/plan identity, removes its source
report, checks the archived working-file bytes and verifies pack size/hash.

Both tests originally used 20-second operation deadlines and exhausted their
cleanup budget in the unoptimized build. Matching the production 45-second
operation deadline allowed the non-repository case to pass; the unoptimized
repository case still exhausted the original claim during gate release. No claim
extension or execution checks were relaxed. In the optimized release build,
both tests passed in 7.17 seconds with the original 30-second claim enforced.

Live binary: `/tmp/herdr-capability-live`, SHA256
`d92024479ef4eab25e4814e3d325705e8150cf20163eb4c663618a8c95100baa`.
Evidence log: `/tmp/herdr-native-preservation-release-live.log`.
The installed Herdr binary was untouched. No credentials or authenticated prompt
were used. Native source-loss preservation is now directly exercised; full
vendor workflow and remaining live crash boundaries remain open in the audit.

### 2026-09-22 — Real Herdr acknowledgment-loss acceptance

Added a live contract that forwards requests to the disposable real Herdr server
and drops one successful reply after either workspace creation or gate input.
The proxy logs method names only. Creation loss must recover the existing exact
supervisor and reject another creation attempt without changing the snapshot.
Gate-input loss must retain exactly one release event and refuse a second input
attempt. Both cases verify the exact process, cancellation/namespace termination,
and preserved report/library/repository bytes after disposable source loss.

The first test run used an incorrect expected gate method name in its request
count assertion; corrected it to the production `pane.send_input` method. The
acknowledgment-loss test then passed in the unoptimized build. All three live
canonical contracts passed together in the optimized release build (11.07 seconds),
using the unchanged capability-enabled Herdr binary documented in the preceding
entry. `git diff --check` passed. No credentials or vendor prompt were used.

This supplies real-server lost-reply evidence while preserving the distinction
from actual controller/host crashes and authenticated vendor workflow acceptance.
The production dispatch gate remains disabled.

### 2026-09-22 — SIGKILL at real native effect boundaries

Added a subprocess driver for the production creation/release ingress and a live
fault proxy that holds a successful real-server reply. After observing that the
server applied the effect, the parent SIGKILLs the separate caller and asserts its
signal exit status. It releases the proxy only after reaping the caller, then
waits for inherited root-lock cleanup before recovering from the database.

Creation recovery observes the existing exact native supervisor; a second creation
attempt is refused with no state change. Gate-input recovery retains one release
intent and refuses another submission. Both cases prove exact process execution,
namespace termination, receipt-bound output recovery after disposable source loss,
and repository snapshot identity/file/pack digests. Native requests and durable
creation/release intents each remain one-use. The ordinary subprocess-driver test
is inert unless invoked by its parent with the fixture request environment.

The crash test passed in the unoptimized build. All four live canonical contracts
passed together in the optimized release build (14.95 seconds), using the same
capability-enabled Herdr binary as before. `git diff --check` passed. No credentials
or authenticated vendor prompt were used. This covers effect-caller SIGKILL after
creation and gate input; host reboot, Herdr restart and authenticated workflow
acceptance remain distinct. Production dispatch stays disabled.

### 2026-09-22 — Live Herdr restart with a recycled pane identity

Added a live test that launches a real supervised worker, writes disposable
outputs, kills/reaps Herdr and starts a replacement server at the same socket
path. The replacement creates an unrelated supervised worker with the same pane
ID as the old target. The test verifies distinct session birth and supervisor
identities before recovering the old attempt.

Cancellation/termination settles the old attempt, leaves the replacement server
and worker alive, and retains its receipt-bound output/repository evidence. A
second reconciliation is read-only and idempotent. The existing source-loss
checks then validate archived bytes. Test teardown explicitly stops the new
supervisor after proving it survived old-attempt recovery.

All five live canonical contracts passed together in the optimized release build
(19.53 seconds); `git diff --check` passed. This is staged-worker lifecycle evidence
using isolated sleep processes and the documented capability-enabled Herdr binary.
It does not supply authenticated vendor readiness, native brief acknowledgment or
started-worker workflow evidence. The previously requested credential-use
permission remains unanswered, and production dispatch stays disabled.

### 2026-09-22 — Broad optimized launch regression audit

Built all state-store test targets and ran their ordinary tests serially in
isolated Linux user/PID/mount namespaces. This permits real local socket and
quiescence checks without weakening production refusal on unreadable host
processes. No credentials or authenticated vendor prompts were used.

The audit found two stale assertions: the worker-snapshot CLI expected the old
v1 estimator, and a legacy preservation scenario expected the old shared/adopted
error after root-wide canonical reference checks were introduced. Updated the
CLI expectation to v2 and made the cleanup diagnostic expectation feature-aware,
with the canonical case identifying the actual conflicting project/thread.
Strengthened that scenario to verify the original report bytes and retained
preservation snapshot after refusal. No production safety check was relaxed.

Validation (release build, state-store enabled):

- Library: 435 passed, 10 opt-in tests ignored.
- Memory-control integration: 12 passed; contracts: 2 passed.
- CLI: 46 passed in the original run, one stale assertion failed, and the
  ten-minute wrapper limit interrupted the final ticker test. The corrected
  snapshot test and interrupted ticker test both passed in a focused rerun:
  all 48 tests have passing results across the runs, not one clean suite run.
- Binary: 523 passed and one stale diagnostic assertion failed. After correction,
  all 11 preservation scenarios passed: all 524 binary tests have passing results
  across the original run and affected-scenario rerun.
- Nine opt-in live Phase A tests remained ignored. The previously recorded native
  Herdr contracts were not rerun for these assertion/documentation-only edits.
- `git diff --check` passed.

Corrected the launch documentation's obsolete estimator/rendering description
and the top-level status page's stale claims that worktree creation/controller
admission lacked implementations. The dispatch audit now distinguishes staged
native lifecycle evidence from still-open live unobserved-identity exit,
historical-bootstrap cleanup and authenticated started-worker workflow acceptance.
Production dispatch remains disabled; these results do not close those gates.

Local logs: `/tmp/herdr-launch-audit-*.log`, including
`cli-corrected`, `binary` and `preservation-rerun`. The initial wrapper stopped on
its CLI timeout; the separate follow-up runner supplied binary-suite coverage.

### 2026-09-22 — Live native exit before durable identity capture

Added an opt-in native contract for creation acknowledgment loss followed by
externally induced worker exit before the application records any exact identity.
The proxy retains the successful request/reply only in the disposable harness;
the harness observes and stops the matching supervisor without writing that
identity into the store. Production recovery is then exercised against the real
server and process inventory.

Repeated resource observation returns no target, repeated creation is refused,
and termination explicitly refuses the missing staged target. Both before and
after cancellation, checks retain capacity and consumed approval, preserve one
creation intent, prohibit gate input, preserve approved checkout bytes, and verify
full database snapshots remain unchanged by recovery. Ambiguity stays blocked;
no identity or cleanup authority is invented from an empty inventory.

All six optimized live canonical tests passed together (21.26 seconds), including
the existing acknowledgment-loss, caller SIGKILL, server-restart and source-loss
preservation cases. The native binary SHA-256 remains
`d92024479ef4eab25e4814e3d325705e8150cf20163eb4c663618a8c95100baa`.
Build/test logs: `/tmp/herdr-live-unobserved-build.log` and
`/tmp/herdr-live-unobserved.log`. `git diff --check` passed.

The test does not simulate a spontaneous vendor-agent crash, prove historical
bootstrap cleanup or supply authenticated started-worker workflow acceptance.
No credentials or authenticated prompts were used; dispatch remains disabled.

### 2026-09-22 — Real historical-bootstrap quiescence boundary

Added a live historical-layout contract using the existing test-only legacy
creation ingress. The first fixture attempted a modern repository reservation
with historical root creation; the store refused the missing preparation receipt
and then the incompatible combined flow. Corrected the test to the historical
non-repository shape already exercised by unit tests. Production validation was
not weakened, and no new historical creation entry point was exposed.

The final test creates a real bootstrap shell and supervised worker, releases the
gate, observes the exact executable and writes report/library outputs. Cancellation
stops the worker namespace. Two termination attempts then refuse unresolved
bootstrap quiescence, leaving the complete database snapshot unchanged and capacity
retained. A separate native process observation verifies the bootstrap is still
alive with its exact creation marker and working directory. Original report and
binary library bytes remain intact.

The individual live test passed (3.84 seconds), then all seven optimized native
contracts passed together (25.19 seconds). Logs are
`/tmp/herdr-live-bootstrap-build.log`, `/tmp/herdr-live-bootstrap.log` and
`/tmp/herdr-live-bootstrap-all.log`. `git diff --check` passed.

The dispatch audit now records live evidence for the deliberately blocked
historical disposition allowed by T03.4. This is not automatic historical cleanup,
live host-reboot certification or authenticated vendor workflow evidence.
Authenticated testing remains pending the prior explicit credential-use request;
production dispatch remains disabled.

### 2026-09-22 — Protected API and remaining vendor-gate audit

Ran the state-store documentation tests omitted by the earlier all-target audit:
all five compile-fail tests passed. They check that external callers cannot
construct protected memory/prepared-launch, native-preparation, revalidation and
started-receipt authority through the prohibited APIs. Command:
`CARGO_HOME=/tmp/herdr-projects-cargo cargo test --locked --offline --features state-store --doc`.
Log: `/tmp/herdr-launch-doc-tests.log`.

Rechecked the current profile verifier and retained-report derivation against
original cards T04.3 and T08.2. The native interaction verifier can establish only
launch/readiness/prompt/stop observations: it intentionally leaves protocol-capable
and certified false and has no workflow certificate. Revalidation enforces that
derivation instead of accepting caller-supplied certification flags. A passing
fixed-token interaction test therefore cannot justify changing the dispatch gate
or claiming task/memory protocol certification.

The still-missing vendor acceptance must exercise trusted preparation and approval,
controller launch admission, retained task brief, permission blocking, checkpoint
pull, result/report, restart and memory update for the selected supported profile
combination. Accounts and capabilities without evidence remain untested. Current
credential-use permission is still unanswered; no credential was read or copied,
and no authenticated prompt was submitted. Production dispatch remains disabled.

### 2026-09-22 — Authorized Codex interaction diagnostic passed

The user explicitly authorized the isolated fixed-token diagnostic. The first
attempt with an additional temporary-directory wrapper failed during native
creation and removed its temporary directory. A credential-free reproduction
identified `workspace.create_command` as the failure; the original shorter layout
passed the no-prompt native test. Authenticated attempts in that layout then
stopped before submission because the terminal displayed a workspace-trust prompt.

Improved API errors to name the failed method while withholding output, and
readiness timeouts to retain the last validation failure. Test-only diagnostics
report fixed screen categories without printing terminal text or credentials.
The authenticated fixture now allocates the disposable lab before configuring
trust, pins trust to its exact working directory instead of `/tmp`, and calls the
same internal verifier as the public API. Production callers still supply their
own execution-home configuration; no automatic trust bypass was added.

The authorized diagnostic passed in 16.99 seconds: authenticated readiness,
one fixed no-tools prompt submission acknowledged by native Herdr, supervised
termination, unchanged project snapshot and explicit temporary-home/login-copy
removal. It does not validate the model's reply, perform task work or certify
memory/workflow behavior. Earlier failed attempts did not reach prompt submission.
No additional authenticated prompt was submitted after success.

All 31 ordinary profile-preparation regressions passed (3 live tests ignored).
Logs: `/tmp/herdr-authorized-exact-trust.log` and
`/tmp/herdr-authorized-profile-regressions.log`. The full authenticated worker
workflow remains untested, and production dispatch remains disabled.

### 2026-09-22 — Integrated authenticated dispatch harness prepared

Added the ignored `live_authenticated_controller_workflow` parent test and
`live_dispatch_workflow_driver` binary test. The parent uses real native profile
verification and retained evidence, an ephemeral owner key, production draft,
Ed25519 approval import and reservation. It never substitutes synthetic capability
evidence on this path. It provisions exact disposable trust entries and a Codex
workspace-write sandbox limited to the temporary project, with tool networking
disabled. The parent removes the temporary login and stops recorded workers even
when the driver fails.

The driver uses the production ticker/background pool with the test-only dispatch
switch. It requires two agent-written output files matching retained instructions,
restarts ticker memory after brief confirmation, cancels, waits for verified
termination, checks one-use launch/brief events, deletes only the temporary output
source and verifies exact recovery through the termination receipt. Changing
PROJECT.md after reservation must not replace the retained task instructions.
The driver never creates the expected output files itself.

`scripts/test-live-canonical-dispatch` builds exact optimized artifacts, requires
explicit authentication/workflow opt-in and runs only this test. The complete
reviewable scope is in `docs/live-dispatch-acceptance.md`. The test has NOT been run
with authentication and is not launch-enablement evidence. It does not yet cover
live memory-update/checkpoint semantics, mixed agents or vendor repository edits.

Validation: both optimized test executables compiled; 52 ordinary canonical-worker
tests passed (10 opt-in tests ignored); both existing controller/ticker acceptance
fixtures passed together (12.68 seconds). The script's help and Python syntax were
checked, and `git diff --check` passed. Logs:
`/tmp/herdr-workflow-final-build.log`, `/tmp/herdr-workflow-worker-regressions.log`,
`/tmp/herdr-workflow-controller-regressions.log`.

No credential was accessed and no vendor prompt was submitted during this harness
implementation. The previous approval covered the fixed no-tools diagnostic; the
new file-writing workflow awaits expanded approval. Production dispatch remains
disabled until the actual remaining acceptance passes.

### 2026-09-22 — Authorized live controller acceptance and dispatch enablement

The user authorized the isolated dispatch workflow. It passed: actual native
Codex verification and retained evidence, signed draft/reservation, background
ticker launch, immutable retained instructions despite a changed PROJECT.md,
one brief, real agent-written report/library files, controller restart,
cancellation/observed termination, capacity release and exact receipt-bound recovery
after deleting the disposable output source. The parent verified removal of the
temporary home/login copy. Parent runtime: 85.50 seconds; controller child: 48.00
seconds. Log: `/tmp/herdr-authorized-dispatch-workflow.log`. No additional
authenticated prompt was submitted for the subsequent gate change.

Rechecked original T03.3/T03.4 and T04.1–T04.5 and the explicit independent-capability
contract in `06-scheduling-workers-and-integration.md`. The prior audit had
conflated full W08/memory protocol certification with enabling launchable profiles.
The existing launch contract requires verified launch, readiness, prompt and stop;
checkpoint acknowledgment, usage, resume and workflow certification stay separate.
The live controller workflow supplies the missing integrated launch evidence. It
does not close broader memory/release cards or promote unknown capabilities.

Enabled `PREPARED_LAUNCH_DISPATCH_ENABLED` and removed the thread-local test gate
override. Both multi-project controller fixtures now use ordinary production
selection, while the explicit excluded-launch selector still checks no-effect
behavior. Current profile/input revalidation, exact signed authority, project
capacity, one-use effects and conservative ambiguous recovery remain unchanged.
No existing user project was migrated, approved or launched.

Validation after enablement:

- Optimized state-store library and binary test artifacts compiled.
- Full binary suite: 525 passed in 87.27 seconds in an isolated Linux namespace.
- Both controller/ticker fixtures passed through production-default selection
  (12.05 seconds); no credentials or vendor prompts were used for these tests.
- Production executable built with
  `cargo build --release --locked --offline --features state-store --bin herdr-projects`.
- `target/release/herdr-projects launch --help` succeeded and exposed draft/reserve.
- `git diff --check` passed.

Production artifact: `target/release/herdr-projects`, SHA-256
`489595e404c7df6742a32db14188bcd0a11ba338a1a997661fe3110504daaffe`.
Logs: `/tmp/herdr-enabled-dispatch-build.log`,
`/tmp/herdr-production-enabled-binary.log`,
`/tmp/herdr-production-enabled-controller.log`,
`/tmp/herdr-enabled-release-build.log`.

This enables dispatch in the local state-store release build; it does not install
or replace the system Herdr binary. The native server must advertise the required
verified creation capability. Full live memory-update/checkpoint, mixed-agent and
vendor repository-editing certification remains uncompleted and is explicitly
recorded in the revised dispatch audit. Those capabilities are not inferred from
a passing launch, and dependency tasks without verified evidence stay blocked.
