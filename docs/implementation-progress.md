# Implementation progress

Updated 2026-09-20. Base: `6e2bd7607d7bc64cf6155beceb299b44566d7f3b`.
The W01 reliability changes and initial regression fixtures were committed as
`af65de4` and pushed to the `hilather/herdr-projects` fork on
`hp-plan/w00/t00-1-baseline`. The local-preservation/diagnostics/context batch was
committed as `325caa7` and pushed to the same fork branch. The subsequent transport,
popup and lifecycle changes are recorded below and in the accompanying commit.
The user upgraded Herdr and requested that
implementation proceed; `herdr --version` now reports **0.9.1**.

## Implemented: W01 reliability changes

| Task | Behavior and evidence |
| --- | --- |
| T01.1 | Nonblocking subprocess I/O keeps deadlines active after parent exit. Owned groups receive direct TERM/KILL signals; capture is capped, raw bytes retained and cancellation distinct. Real subprocess tests cover inherited/escaped pipes, blocked stdin, large output on both streams, invalid UTF-8, ignored TERM, grandchildren, normal completion and cancellation. |
| T01.2 | UTF-8-safe suffix parsing, positive ASCII interval validation and checked conversion. Tests cover all unit boundaries, overflow in debug/release, a deterministic Unicode sample, per-file diagnostics and spring/fall DST transitions. |
| T01.3 | Poll/backoff/outage keys include project and session. Rotation removes a fixed slow-pass first project. Mocked full ticks cover two projects sharing a label, distinct sockets, launch/prompt/report copy, one failed session, local progress and recovery. |
| T01.4 | Merged final-copy intent and bounded retries persist before copying, independent of summary dedup and GitHub polling. Tests cover restart, stale identity, reopen, changed report PR, pause/resume, failed intent write, partial-copy warnings and crash after thread commit. Idle auto-resolution cannot bypass pending merge work. |
| T01.5 | Notification hashes advance only on confirmed delivery; failed notifications/nudges retain bounded retry across restart without stopping other work. Outage histories are persisted per resource; PR/outage inbox events retain delivery obligations and stable IDs. Tests cover failure, restart, healthy-resource isolation and replay after an item was handled. Doctor exposes pending retry state and malformed state files. |

The F02, F03, F04 and both F05 baseline reproductions pass in the W01 commit.
The initial W02 changes also enable and pass F01; all baseline regressions now run.

See [ADR 0001](adr/0001-phase-a-boundaries.md) for the runtime contract, compatibility
boundaries and dependency decision. ADR 0002 now supplies the Phase B interface
contract and dependency selection before any migration implementation. No complete-wave acceptance or
independent review is claimed by these local changes.

## Latest five-item batch: T02.1–T02.4 and T00.2

Base/head before this uncommitted batch: `30185f8` on
`hp-plan/w00/t00-1-baseline`. No live user sessions or production project roots
were modified. Native Rust tests and disposable Git/filesystem/process fixtures
supply the evidence below.

- **T02.1:** Versioned native remote artifact export/receive with 50 MiB payload,
  4 MiB manifest, 10,000 entries and depth limits. Source recheck, independent
  staged hashes, literal UTF-8 paths, binary payloads and immutable receipt reuse.
  Local Linux cleanup excludes supported managed operations, requires explicit
  known-writer shutdown confirmation, inspects same-user process cwd/descriptors/
  mappings, rechecks source/Git identity and uses non-force removal. Shared/adopted
  references, managed panes, ambiguity and failed verification preserve the source.
- **T02.2:** Runner file sinks stream without filling memory and terminate on the
  byte limit. All remote report paths use bounded staging and atomic publication.
  Remote final projections derive from verified snapshot bytes. Missing/incompatible
  helpers durably block merged finalization with installation/manual-retry guidance.
  Connection errors retain bounded retry. Live rsync projections retain preflight
  size checks but are not hard bounds against a growing source.
- **T02.3:** Removal intent records operation/repository/path/branch/head/snapshot
  before Git removal; a lost acknowledgement is recoverable. Pending removal
  excludes ticker launch and prompting. Logical reopen starts nothing; restart
  validates the retained branch and reattaches it without reset, force or branch
  deletion. Missing legacy creation evidence remains a human-inspection case.
- **T02.4:** `repair PROJECT inspect` emits structured JSON. Explicit restore checks
  the inspected hash and replacement identity/schema, excludes active writers,
  backs up original bytes and atomically replaces the record. Ticker lock prevents
  overwriting pending obligations from its in-memory state. No automatic repair.
  Earlier popup concurrency/replay/root/session/expiry tests remain enabled.
- **T00.2:** [ADR 0002](adr/0002-phase-b-contracts.md) and compiled illustrative
  domain/store interfaces define state, revision, event, approval, ambiguity and
  result/gate boundaries. Rust 1.89 rejects rusqlite 0.40.1; 0.37.0 with system
  SQLite 3.53.4 passes the standalone native transaction fixture. Its old bundled
  SQLite 3.50.2 was tested but rejected for engine maintenance. No application
  database dependency or migration was added.

Real Git tests exercise ignored artifact preservation, dirty/untracked refusal,
retained branch reopen, lost removal acknowledgement, and the entire resolve →
remove → reopen → restart path (Herdr responses mocked). Native CLI tests use a
scrubbed environment for multi-megabyte binary export and explicit repair. Process
fixtures cover sink overflow/exclusive creation, descriptor-based cleanup refusal,
and lifecycle lease exclusion. Stream fixtures reject path traversal, duplicates,
corrupt/truncated/oversize payloads and source changes during export.

## Validation and limits

Commands use Rust 1.89 with locked dependencies:

```sh
cargo test --locked --offline
cargo test --release --locked --offline
cargo build --release --locked --offline
CARGO_TARGET_DIR=/tmp/herdr-sqlite-smoke-target cargo run --locked --offline --manifest-path contracts/sqlite-smoke/Cargo.toml
```

**Results: 222 tests pass in both debug and release** (214 unit/scenario, six
CLI integration and two interface-contract tests), zero ignored. The locked release
build passes without warnings, `git diff --check` is clean, and the standalone
SQLite probe passes against system SQLite 3.53.4 on Rust 1.89. Tests use Linux, Git,
rsync 3.5.0, native subprocesses and disposable roots; Herdr lifecycle calls use
scripted fixtures. Real double-shell remote-path fixtures run locally.

Local command logs: `/tmp/herdr-five-debug.log`, `/tmp/herdr-five-release.log`,
`/tmp/herdr-five-build.log`, `/tmp/herdr-five-sqlite.log`. These temporary logs are
not release artifacts; the commands and fixture sources above are reproducible.

Remote and non-Linux destructive cleanup remain explicitly unsupported. The
checkpoint assumes cooperative same-user agents and stopped known writers; it
cannot contain hostile same-privilege processes or see managed state in other
roots. The subsequent [Phase A acceptance evidence](phase-a-acceptance.md) records
live Linux SSH, popup, lifecycle and independent-review results. macOS and the
remaining supported platform/agent matrix are still untested. Snapshot/repair-backup retention is manual.
Plain resolution retains the worktree unless cleanup is explicitly requested.
No force fallback, automatic merge/push or production data migration is introduced.

## Next work

[Task ledger](task-status.md): thirteen locally implemented cards, two partial, 26 not started.
The user accepted Phase A with its documented testing gaps and authorized W03. Proposed memory change: record native transport v1, cooperative checkpoint,
repair workflow and ADR 0002 decisions; no shared-memory promotion was performed.

## Phase A review follow-up

Independent review identified and verified fixes for a pause/lease race, invalid
lifecycle state defaulting to active, and missing legacy sources being finalized
as preserved. Three opt-in live Linux fixtures now cover Herdr/Git lifecycle,
loopback SSH streaming and PTY popup input. Positive cleanup also passes in an
isolated PID namespace. See [acceptance evidence](phase-a-acceptance.md).
The user subsequently accepted Phase A with these gaps; W03 may now proceed.


## W03: T03.1 transactional store

The opt-in `state-store` library adds schema v1 for tasks, attempts, immutable
operation intents and events. Commit batches compare entity revisions and event
heads, then write all records/events/intents atomically. Legacy CLI behavior and
memory authority remain unchanged. No migration or operation dispatcher is wired.
See [ADR 0003](adr/0003-project-store-v1.md) for schema ownership, failure evidence,
limits and reproduction commands. T03.2 migration is the next card.


## T03.2 offline migration foundation

Committed/pushed the reviewed Phase A and T03.1 changes as `52884bf` before this
work. The new [migration workflow](migration-workflow.md) adds explicit planning,
verified backups/import, journaled cutover/recovery, revisioned projections and
separate-root restore. Always-compiled ownership guards prevent legacy execution
or mutation after preparation. Memory remains file-authoritative. Independent
review approved this conservative subset; T03.2 remains partial pending runtime
adapters, live preflight and pending-operation conversion. No real user project
was migrated and the W03 gate is not claimed complete.


## Store-backed task commands and durable operations

The [delivery foundation](operation-delivery.md) adds schema v3, transactional
claims/outcomes, lease fencing, ambiguity and bounded retry. Migration converts
supported legacy pending inbox/finalization/notification obligations without losing
retry provenance or automatically replaying uncertain effects. Migrated task
commands and context read/write the store; recovery preserves post-cutover edits.
Snapshots and operation projections bind delivery state to the same event head.
Independent review approved the foundation after correcting finish-time task
fencing. T03.2/T03.3 remain partial; external execution adapters and live preflight
are not yet complete. No actual user project was migrated or dispatched.

The combined debug and release suites pass 265 tests (37 library, 216 binary,
10 CLI and two contracts), with three opt-in live fixtures ignored by default.
The default-feature suite passes 226 tests. The locked all-feature release build
and `git diff --check` pass. Final independent review found no further issues.
Validation logs: `/tmp/herdr-operations-debug.log`, `/tmp/herdr-operations-release.log`,
`/tmp/herdr-operations-build.log`, `/tmp/herdr-operations-legacy.log`.


## Canonical inbox and migration preflight

Schema v4 adds canonical inbox state, an atomic internal delivery/receipt adapter,
and migrated inbox/context commands. Explicit upgrades use preserved database
sources; schema-qualified projection directories preserve exports across upgrades.
Read-only preflight reports bounded external config fingerprints, destination
filesystem/space estimates, recorded Herdr identities and local worktree matches.
Apply enforces destination storage checks. Preflight observations retain migration
blockers and do not enable execution. External configuration binding, complete
execution adapters and reconciliation remain open; T03.2/T03.3 are still partial.
No user project or live session was changed. macOS remains untested.

Review identified and prompted fixes for an unbounded config reread, the storage
probe destination, significant inbox indentation, and projection identity across
schema upgrades. Regression coverage includes FIFO/oversized config refusal,
claimed-intent protection, conflict rollback, provenance-based upgrade and CLI
seen/done behavior.

Validation: 274 tests pass in debug and release (42 library, 218 binary, 12 CLI
and two contracts); 226 default-feature tests pass. Three live fixtures remain
opt-in. Logs: `/tmp/herdr-adapters-{debug,release,legacy,build}.log`.
The all-feature release build and `git diff --check` pass. Independent re-review
confirmed the four fixes with no remaining production blocker. Its parallel
library run encountered one transient maintenance-lock refusal; the isolated
rerun passed. The full debug and release runs above passed without that refusal.
The reviewer also passed all 42 library tests serially and the focused bounded
config CLI fixture, and approved this scoped inbox/preflight implementation.
The transient parallel contention's cause remains unconfirmed. Review approval
does not close T03.2/T03.3 or the W03 acceptance gate.


## External configuration bound through cutover

Version-2 CLI migration plans bind the resolved config path and content fingerprint
(or absence) into migration identity. Apply refuses old unbound plans; existing
version-1 prepared journals retain their original recovery contract. Config edits,
appearance or removal block unpublished cutover, including recovery. Active store
recovery remains independent of subsequent config edits. Config values are never
included in plans or copied into project backups. Safety parsing is validated
against the fingerprint used for planning. Initial root resolution now rejects
special/oversized config files promptly without requiring an explicit root.

Regression fixtures cover all unpublished journal phases, absent-to-present config,
symlinks, malformed content, stale CLI plans, active recovery and no-root FIFO/size
bounds. T03.2/T03.3 remain partial. External execution requires the remaining
ownership/reconciliation integration; no scheduler or live effect was enabled.

Independent review approved this scoped change and independently passed the new
migration and CLI regressions. Validation: 278 tests pass in debug and release
(44 library, 218 binary, 14 CLI, two contracts); 227 default-feature tests pass.
The all-feature release build and `git diff --check` pass. Three live fixtures stay
opt-in; macOS remains untested. Logs: `/tmp/herdr-config-{debug,release,legacy,build}.log`.


## Common dispatch service and operator claim expiry

The new generic dispatch service prepares an adapter-owned resource guard,
commits a claim, rechecks policy and store fences, calls the effect outside SQLite,
and records its outcome while retaining the guard. Bare adapter errors become
ambiguous. Receipt persistence failures return an explicit unrecorded claim and
never trigger retry. `operations PROJECT expire` exposes safe, idempotent claim
expiry during the execution freeze. It does not authorize replay.

Independent review approved the service contract and initial focused tests.
Fixtures now also cover lease expiry before effect, CLI expiry, and native process
death before effect, after effect and after receipt. Notification/finalization
production adapters still require ownership/reconciliation integration; T03.3 and
W03 acceptance remain open. No real user project or live resource was changed.

Final independent review approved the service, expiry wrapper/CLI and native crash
fixtures, and independently passed all six service tests plus the expiry CLI test.
The parallel debug run encountered the previously observed cleanup process-identity
inspection race; parallel release encountered transient migration maintenance-lock
contention. Production refusal guards remain intact. The complete serial debug
suite passes 285 tests (50 library, 218 binary, 15 CLI, two contracts), and the
default-feature suite passes 227. Three live fixtures remain opt-in.
The complete serial release suite also passes all 285 tests. The locked all-feature
release build and `git diff --check` pass. Validation logs are
`/tmp/herdr-dispatch-{debug,debug-serial,release,release-serial,legacy,build}.log`.
Use `-- --test-threads=1` to reproduce the complete successful feature suites.
macOS and production external adapter integration remain untested.

## Imported receipt observation

Committed and pushed the reviewed migration/inbox/dispatch foundation as `ec273e2`
to the fork before this batch. New receipt-plan and observe-imported commands
compare durable notification/finalization receipts against hash-checked database
provenance. Exact receipt matches can confirm ambiguous imported operations in one
head-checked transaction. Task state, legacy files and external resources remain
unchanged. Claimed, stale and unsupported records remain blocked.

Independent review caught a historical-receipt reuse risk for newly enqueued
lookalike operations. The observer now requires the shared deterministic import ID,
original task revision and idempotency key. Regression fixtures cover lookalikes
with both unchanged and advanced task revisions. The observer parses the ticker
once and indexes source paths to keep large batches from repeatedly parsing it.
T03.2/T03.3 remain partial; general live reconciliation has not been enabled.

Re-review approved the identity fix with no remaining blockers. All 291 tests pass
serially in debug and release (54 library, 219 binary, 16 CLI, two contracts);
227 default-feature tests and the locked all-feature release build pass.
`git diff --check` is clean. Three live fixtures remain opt-in; macOS remains
untested. Logs: `/tmp/herdr-receipts-{debug,release,legacy,build}.log`.


## Typed runtime identities

Pushed the reviewed receipt observer as `fc42188` before this batch. Schema v5 now
stores typed thread/coordinator identities with task/source references, binding
revisions and hashes. Recorded sockets retain coordinator provenance; absent
sockets remain absent. `migration PROJECT bindings` and runtime exports expose
these records as unverified, without granting ownership or enabling execution.

Explicit upgrades backfill from database provenance, preserve task edits and event
head, and retain prior exports. Reads detect corrupt payloads, corrupt source or
session bytes, missing sources and incomplete inventories. Unknown fields remain
in their original source bytes. Independent review approved the foundation and
subsequent read-integrity hardening. The earlier receipt-corruption test now
expects snapshot refusal until the corrupt source bytes are restored, then checks
that no receipt/task/event changes were committed. Upgrade failure also has a
schema-rollback/retry fixture. T03.2/T03.3 remain partial; no live resources changed.

Validation: 297 tests pass serially in debug and release (59 library, 219 binary,
17 CLI, two contracts); 227 default-feature tests and the all-feature release build
pass. An overlapping default-feature build replaced the debug CLI binary during an
earlier feature-suite run; rerunning the feature suite after that build completed
passed. Run feature variants sequentially or with separate target directories.
Successful logs: `/tmp/herdr-bindings-debug-isolated.log`,
`/tmp/herdr-bindings-final-release.log`, `/tmp/herdr-bindings-legacy.log`, and
`/tmp/herdr-bindings-build.log`. `git diff --check` passes. Three live fixtures stay
opt-in; macOS remains untested.


## Durable runtime observation

Committed schema-v5 runtime identities as `420d8a2`. Schema v6 and `reconcile
PROJECT [--record]` add a bounded Herdr/Git observation collector over those records,
complete-batch recording with event-head/binding/task revision fences, source/config
rechecks, observation timestamps and hash-checked persistence. No external effects,
capacity release, lifecycle changes or ownership grants occur.

Independent review approved the observation-only scope. Its suggested fixes now
reject contradictory Git branch/detached records and enforce complete binding
coverage in the store API. Tests cover absent/unreachable/mismatched/duplicate pane
identities, inconsistent agents, truncated Git output, stale state and old timestamps,
idempotent recording, preserved task state and the CLI preview/record path.
T03.4 is now partial: 13 implemented locally, three partial, 25 not started. The
remaining W03 adapters require this observation layer before ownership integration.

Validation: 302 tests pass serially in debug and release (60 library, 222 binary,
18 CLI, two contracts); 227 default-feature tests and the locked all-feature
release build pass. Three live fixtures remain opt-in. Logs:
`/tmp/herdr-reconcile-final-debug.log`, `/tmp/herdr-reconcile-release.log`,
`/tmp/herdr-reconcile-legacy.log`, `/tmp/herdr-reconcile-build.log`.
`git diff --check` passes. macOS and live ownership acquisition remain untested.

## Explicit runtime session rebinding

The `runtime inspect/rebind` commands add controlled routing replacement under
maintenance, event-head and binding-revision checks. Rebind clears stale
observations, bumps the linked task revision, invalidates the old execution
fingerprint and leaves ownership unverified. It does not modify legacy files or
release reservations. Independent review found an unselected lost-attempt case;
the guard now checks every task attempt with unobserved termination, not just the
active pointer. Re-review approved that fix. Fixtures cover stale claims, recovery,
legacy-byte preservation, duplicate pane references, selected/unselected retained
attempts and the CLI workflow.

Validation: 306 tests pass serially in debug and release (63 library, 222 binary,
19 CLI, two contracts); 227 default-feature tests and the all-feature release build
pass. Three live fixtures remain opt-in. Logs: `/tmp/herdr-rebind-final-debug.log`,
`/tmp/herdr-rebind-release.log`, `/tmp/herdr-rebind-legacy.log`,
`/tmp/herdr-rebind-build.log`. `git diff --check` passes.

## Canonical lifecycle and recoverable control publication

Schema v7 imports paused/archived lifecycle into canonical control and adds explicit
admission, state changes and epochs. Resume currently admits only projects without
existing resources needing adoption and with fresh matching observations, no retained
attempts and no unfinished intents. Pause/archive remain usable with malformed
external config. Rebind invalidates admission. Ordinary opens refuse an interrupted
control/format publication; migration recovery republishes from the committed DB.
Intent retirement stops pending/ambiguous retries without asserting effect absence
or releasing capacity. Legacy status and source bytes remain unchanged.

Separate review approved this controller scope. External adapters must still enforce
typed safety, scoped authority and ownership; W03 remains partial. Debug and release
all-feature suites pass 312 tests (68 library, 222 binary, 20 CLI, 2 contracts), with
three live fixtures opt-in. Default-feature tests and release build also pass.
Logs: `/tmp/herdr-control-final-debug.log`, `/tmp/herdr-control-release.log`,
`/tmp/herdr-control-legacy.log`, `/tmp/herdr-control-build.log`.

## Canonical runtime creation without legacy files

Schema v8 permits null import provenance for newly registered coordinator/task
bindings. Imported payloads retain their original serialization and source hashes;
the transactional upgrade preserves observations and foreign keys. `runtime create`
uses expected head/task revision, refuses retained attempts and duplicate references,
increments the linked task revision and invalidates control. No external resource
is launched or adopted. Existing imported bindings use rebind instead.

Independent review approved the change after the fresh-import schema check was
updated to the shared current version. Four added migration/CLI regressions cover
provenance, task/control fencing, retained attempts and schema-v7 observation
preservation. All-feature debug/release suites pass 316 tests (71 library, 222 binary,
21 CLI, 2 contracts); three live fixtures remain opt-in. Default tests and release
build pass. Logs: `/tmp/herdr-canonical-runtime-debug.log`,
`/tmp/herdr-canonical-runtime-release.log`, `/tmp/herdr-canonical-runtime-legacy.log`,
`/tmp/herdr-canonical-runtime-build.log`. W03 remains partial: actual notification/
finalization adapters, resource ownership and integrated restart acceptance remain.

## Concrete canonical notification delivery

Explicit `operations notify` creates a fixed count-only session notification from
canonical unseen inbox IDs, under an expected head. `deliver-notification` uses
the shared durable dispatch service with the execution lease held through receipt
persistence. It checks active control, task/binding/inbox/config fences, typed
safety parsed from hash-matching bytes, explicit local socket, supported Herdr and
a remaining timeout budget. Only `shown=true` confirms delivery; uncertainty never
automatically replays. Inbox content/seen/done state remains untouched.

Independent review caught and resolved config snapshot integrity and CLI fake
selection issues. Five adapter regressions and a subprocess CLI fixture cover
receipt/dedup behavior, ambiguity after restart, withdrawn policy, revision fences,
typed safety and lock coverage. No real notification was sent. This does not yet
port terminal nudges or automatic ticker dispatch; imported notifications remain
conservatively blocked without matching receipts. Permanent inbox-set dedup also
prevents reauthorizing a retired unsent set; that liveness limitation is explicit.

Validation: 322 all-feature debug/release tests pass after fixture isolation repair
(71 library, 227 binary, 22 CLI, 2 contracts). Default tests and locked release build
pass. Logs: `/tmp/herdr-notify-final-debug.log`, `/tmp/herdr-notify-final-release.log`,
`/tmp/herdr-notify-legacy.log`, `/tmp/herdr-notify-build.log`.

## Local canonical finalization and receipt recovery

Explicit finalization queues a recorded local source with report hash, revisions,
config and operator disposition. Bounded manifest-verified copies use a separate
canonical-artifact namespace while legacy mutation guards stay intact. Durable
create-if-absent receipts bind the immutable operation and exact manifest execution.
A confirmed outcome atomically increments the task revision and moves it to
AwaitingReview; it never certifies success or releases attempt/resource capacity.
The receipt observer can recover after process death with the original source gone.
Missing/corrupt evidence stays ambiguous. Remote transfers, merged-PR evidence and
automatic imported-obligation dispatch remain open W03 work.

Independent review caught source-ancestor replacement and verified its repair:
canonical source identity is checked before copying and in the captured manifest
before receipt publication. It also verified atomic receipt publication and indexed
operation reads. Seven native adapter tests (including child-process kill points,
SQL-trigger rollback, corruption, stale-state and retained-attempt refusal) plus a
CLI fixture pass. No real project/resource was changed.

Validation: 330 all-feature debug/release tests (71 library, 234 binary, 23 CLI,
2 contracts), default-feature tests and locked release build pass; three live
fixtures remain opt-in. Logs: `/tmp/herdr-finalize-debug.log`,
`/tmp/herdr-finalize-release.log`, `/tmp/herdr-finalize-legacy.log`,
`/tmp/herdr-finalize-build.log`.

## Local resource ownership and adoption

Schema v9 adds audited ownership claims bound to runtime/config revisions, local
socket/worktree incarnation and exact detected agent identity. Explicit adoption
runs bounded conflict inspection under the root execution lease, including retained
legacy references, canonical neighbors and corrupt/missing project inventory.
A live adopted task worker creates a retained running attempt; fresh post-adoption
evidence is required for admission. Changed resource evidence pauses active control
without releasing capacity. Owned routes cannot be rebound. Adopted resources gain
no destructive cleanup authority; termination and relinquishment remain next work.

Independent review approved after regressions repaired older-store inventory,
prunable/replaced worktrees, owned coordinator rebinding and missing project markers.
Six ownership fixtures (including native Git) and a subprocess CLI fixture cover the
new behavior. No real project or session was modified. Full all-feature debug and
release suites pass 337 tests (71 library, 240 binary, 24 CLI, 2 contracts); default
features and release build pass. Three live fixtures remain opt-in. Full suites used
disposable process namespaces because the host systemd user process denies /proc
inspection, correctly causing strict cleanup checks to refuse outside isolation.
Logs: `/tmp/herdr-ownership-{debug,release,legacy,build}.log`.

## Audited ownership relinquishment

Explicit relinquishment requires paused/archived control and the current head and
ownership revision. It atomically withdraws the active claim/observation, records
the complete prior claim and reason, advances a linked task revision and invalidates
control. Bindings and external resources remain intact. Live or lost retained
attempts and active task pointers refuse release. Adoption generations now derive
from immutable events so withdrawal/re-adoption cannot reuse an attempt identity.

Independent review approved; two new regressions cover atomic rollback, stale
inputs, retained live/lost workers and monotonic re-adoption. Native Git and CLI
fixtures also verify resource preservation and active-control refusal. All-feature
debug/release tests pass 339 tests; default tests pass. Process namespace isolation
continues to keep cleanup checks strict. Logs: `/tmp/herdr-relinquish-{debug,release,legacy}.log`.
No new termination proof is claimed; automatic repair and controller integration
remain open W03 work.

## Structured read-only recovery planning

`reconcile --plan` now classifies runtime resources, every retained attempt and
durable delivery intents against a complete fresh observation batch and fenced
head/revisions. Reports include proposed actions but always deny dispatch authority.
Ambiguous effects remain receipt-inspection work even after task changes; expired
claims require expiry rather than replay; supported pending adapters are only retry
candidates subject to their full validation. Planning never persists observations,
releases capacity or executes repairs.

Independent review approved. Two new library regressions, a read-only ownership
fixture and the extended CLI fixture cover uncertainty precedence, expired claims,
repeatability, stale inputs and unchanged canonical state. All-feature debug and
release suites pass 342 tests (73 library, 243 binary, 24 CLI, 2 contracts); three
live fixtures remain opt-in. Default-feature tests pass. Logs:
`/tmp/herdr-recovery-{debug,release,legacy}.log`.

## Canonical controller polling

The ticker now routes migrated schema-v9 projects to canonical polling. Each pass
records complete observations, expires old claims and processes at most one accepted
canonical notification/finalization or ambiguous finalization receipt. Project and
operation selection rotates. Existing adapters retain their execution guard through
claim, effect and receipt; no new intents, launches or terminal input are inferred.

Independent review caught two integration defects and approved their fixes:
canonical runtime edits now use execution/project locks without the ticker leadership
barrier (migration/restore/upgrade retain it), and blocked operation diagnostics no
longer discard observed reachability. Automatic subprocess/socket observation probes
share a 15-second deadline; exhausted collections are discarded before persistence.

Six new controller fixtures cover leader-held operator edits, restart deduplication,
expired claims without replay, paused/lease refusal, failed receipt commit followed
by source loss, retained live reachability and native probe deadlines. Full debug and
release suites pass 348 tests (73 library, 249 binary, 24 CLI, 2 contracts); default
features also pass. Three live fixtures remain opt-in. Logs:
`/tmp/herdr-controller-{debug,release,legacy}.log`.

## W03 bounded handoff accepted

Independent review assessed the exact T03.2–T03.4 cards and wave gate rather than
requiring future scheduler/result features prematurely. Its three final handoff
items are complete: repeated recorded remote-outage reconciliation retains lost
capacity, at-least-once semantics are explicit, and coordinator ownership guidance
and runbooks reflect the implemented ticker. The review approved this supported
opt-in scope after those changes. No further production-code blocker was found.

The combined handoff tree passes 349 all-feature debug/release tests, 227 default
feature tests and a release build. See `docs/w03-acceptance.md` for the scope and
`/tmp/herdr-w03-handoff-{debug,release,legacy,build}.log` for validation. The literal
canonical launch-boundary crash criterion remains unpassed and is a required W04
gate before launches are enabled. Missing termination evidence continues retaining
capacity; unsupported effects remain blocked. macOS and the broader live matrix
remain untested. Counts are now 16 locally implemented cards and 25 not started.

## W04 queue and capacity-policy foundation

The independently reviewed W04 scheduler/executor/profile contract is frozen in
ADR 0004. Schema v10 adds revisioned per-project capacity/attempt limits, canonical
queue age/priority and typed dependency edges. Policy defaults to zero workers until
explicit configuration. Queue edits atomically fence task/head, validate the DAG,
preserve enqueue age and append audit events. Age eventually outweighs bounded
priority; every unterminated attempt counts in capacity reporting. No predecessor
narrative success fabricates verified result/candidate/landed evidence.

Review caught and resolved a compatibility issue where unrelated task inventory
could make upgraded snapshots unreadable. Graph bounds now apply to queued work,
and dependency loading uses an index. The new 10,001-task commit/read/upgrade
regression passes. Six scheduler tests, an old-export upgrade fixture and a CLI
fixture cover rollback, graph rejection, policy races, aging, retained capacity,
verified-dependency blocking and unchanged legacy files. Full all-feature debug and
release suites pass 357 tests (80 library, 250 binary, 25 CLI, 2 contracts).
Default-feature tests and the release build also pass. Logs:
`/tmp/herdr-scheduler-{debug,release,legacy,build}.log`.

T04.1 remains partial: immutable prepared inputs, atomic reservation/launch intent
creation and cancellation still need implementation. Queueing does not reserve or
launch; profile/authority and the carried launch crash gate remain prerequisites.

## W04 atomic reservation and cancellation foundation

Schema v11 commits retained capacity, immutable attempt inputs, the task pointer and
launch intent atomically. Preparations are sealed internal capabilities scoped to
the exact store and task/control/policy/binding revisions; production profile and
authority producers remain unavailable. Generic enqueue refuses launch intents.
The new cancellation CLI audits requests. It releases capacity only for an exact
never-claimed reservation while atomically retiring its launch; retried, claimed,
lost and adopted workers retain capacity. SQL prevents claim counters from being
reset, and immutable input/request records survive restart and export.

Independent review approved the bounded foundation after orphan-input/read checks
and transactional refusal of unsupported older launch intents were added. Twelve
focused tests cover reservation contention, rollback, claim/cancel races, immutable
inputs, retained uncertainty, restart, upgrade and process death before/after commit.
A CLI regression confirms cancellation of an unproven worker records a request
without release. The full debug suite passes 368 tests, with both subsequently added
crash tests also passing in debug; the full release suite passes all 370 tests
(92 library, 250 binary, 26 CLI, 2 contracts). Default-feature validation passes 227.
Three opt-in live tests remain ignored; macOS remains unavailable. Logs:
`/tmp/herdr-reserve-{debug,focused,release,legacy,build}.log`.

T04.1 remains partial until production preparation/launch and termination paths
exist. No worker launch was enabled and no external launch crash gate is claimed.
Next is T04.2 bounded execution and elapsed-time scheduling, then kind-bound profiles
and authority producers. The overall 41-card counts remain 16 implemented locally,
one partial and 24 not started.

## W04 elapsed-time polling (partial T04.2)

Remote cadence/backoff now use monotonic deadlines per project/session/machine,
with 60-second normal polling and a 120-second retry delay measured from failed
command completion. Ticker sleep uses the remainder of its pass-start interval.
Fast passes cannot trigger early retries; slow passes do not stretch intervals by
requiring additional ticks, and overdue work does not generate catch-up bursts.

Independent review approved this increment. Deterministic clock fixtures cover
exact boundaries, long commands, rapid ticks, session isolation and outage recovery.
The full all-feature debug suite passes 371 tests (92 library, 251 binary, 26 CLI,
2 contracts). Slow commands remain synchronous: bounded pools, separate execution
lanes and cancellation/drain load tests are the next T04.2 work. Overall counts are
16 cards implemented locally, two partial and 23 not started.
Default-feature validation also passes all 228 tests; the three focused scheduler
checks pass in release mode. Logs: `/tmp/herdr-deadlines-{debug,release,legacy}.log`.

## W04 bounded executor engine (T04.2 continues)

Added a fixed-thread native command executor with separately bounded control and
transfer queues, per-lane project/machine limits, cross-lane terminal exclusion,
project-scoped operation identity, queue-inclusive deadlines and cancellation.
Replies retain expected revision for caller-side fenced commits. Admission bounds
include running commands and cap argv/environment/stdin/captured output. Metrics
record queue/active counts, admission high-water marks and maximum queue delay.

Independent review found and resolved project-scope deduplication and panic cleanup
issues. A panicking runner now quarantines the executor; uncertain cleanup never
permits successor work. Stop retains worker ownership on timeout; Drop joins rather
than detaching effects. Focused tests exercise slow-transfer/control isolation,
machine exclusion, queue bounds, expired/cancelled no-spawn behavior, project
rotation, terminal serialization, duplicate IDs across projects, dropped receivers,
input caps, panic quarantine and a 32-command bounded load fixture.

Ticker integration remains required: both current pass types hold a root execution
lease, so moving the slow pass to a thread alone would not make status responsive.
The next change must split observation and effects with record/operation fences and
preserve terminal ownership. T04.2 remains partial and overall card counts unchanged.
Validation: 378 all-feature debug tests (92 library, 258 binary, 26 CLI, 2 contracts),
235 default-feature tests, and all seven focused executor tests in release mode pass.
The final owned-process-group admission check also passes the focused debug suite.
Logs: `/tmp/herdr-executor-{debug,focused,release,legacy}.log`. Three optional live
checks remain ignored; no macOS or end-to-end ticker responsiveness claim is made.

## W04 asynchronous PR-read integration

The production ticker now submits read-only `gh pr view` commands to the bounded
executor. It continues guarded work while a query is pending and consumes responses
on a later pass. Pending work does not become an outage or advance the completed
PR-check timestamp. Results bind canonical project/thread identity, execution and
report fingerprints, and URL; changed input cancels/discards the prior query.
Existing branch/repository checks and finalization writes remain inside the guarded
pass. No terminal commands or finalization effects moved to background workers.

Consumed replies have a cooldown, and the bounded ephemeral cache expires inactive
entries. A queue-inclusive 30-second deadline preserves the ten-second gh timeout.
The executor now reports whether it entered the runner, so queued expiry is local
backpressure, never evidence of a GitHub outage. Stop/idle exit drains reads while
retaining ticker ownership and reports unresolved cleanup on failure.

Independent review approved the integration and queue-expiry distinction. Full debug
validation passes 380 tests; the subsequently added queue-expiry fixture is included
in the full release suite's 381 passing tests and the default suite's 238 passing
tests. Full-ticker fixtures exercise unrelated status progress while a PR query is
held, eventual application, report-change cancellation and shutdown. Logs:
`/tmp/herdr-async-pr-{debug,focused,release,legacy}.log`. Three optional live checks
remain ignored. T04.2 is still partial: remote observations, routines and artifact
work need asynchronous integration; canonical PR evidence remains W07 work.

## W04 asynchronous remote observation batches

Remote status collection now runs in the bounded executor: agent/pane inventory,
saved-machine target selection and SSH report hashes form one read-only batch with
a shared cancellation token and deadline. PR and remote reads share one root pool,
so thread/concurrency limits do not multiply per service. Results are applied under
the original execution lease only when full thread records, route and config still
match. Copies and pending terminal actions revalidate routing; pending launch/prompt
work refreshes inventory before effects. Copies, metadata publication, routines and
terminal effects remain synchronous and keep T04.2 partial.

Independent review caught a synchronous FIFO config-read hazard and an eager fallback
regression. Both now use the established bounded nonblocking regular-file reader;
resolved machine targets skip fallback config entirely. Observation fingerprints
preserve absent versus empty config. New tests cover FIFO/oversize no-admission,
lazy fallback, delayed remote work alongside persisted local status, complete batch
application, changed thread/config cancellation and unavailable transport without
false pane closure.

Full all-feature debug and release suites pass 385 tests (92 library, 265 binary,
26 CLI, 2 contracts). Default-feature validation also covers the subsequently added
transport-outage scenario. Logs: `/tmp/herdr-async-remote-{debug,release,legacy,focused}.log`.
Three optional live checks remain ignored and macOS remains unavailable. Canonical
remote execution is still blocked; this integration moves legacy observation work,
not external launch authority or termination certification.
The default suite passes all 243 tests, including the new outage scenario. Cross-project
observation sharing and remaining effect/transfer isolation still require work.

## W04 kind-bound launch arguments

Legacy worker and coordinator arguments now require an explicit agent-kind binding
when nonempty. New thread creation, restart and coordinator open validate before
resource creation; ticker retries validate before consuming a launch attempt or
the pass's launch slot. A mismatched worker cannot starve a compatible worker.
Empty argument defaults continue to support mixed kinds. Existing nonempty arrays
need the corresponding `thread_agent_args_kind` or `coordinator_agent_args_kind`
setting, naming the kind for which those flags were written. No flags are inferred,
translated or silently dropped.

Independent review found that safety config read failures previously fell back to
defaults. Loading now uses the bounded nonblocking regular-file reader and defaults
only when config is absent. Regression fixtures cover cross-kind refusal before
resources, retry counters/fairness, coordinator refusal, invalid UTF-8, directories
and FIFOs. Independent review approved the fixes.

T04.3 is partial: this is argument isolation, not capability certification. Named
profiles, installed-version evidence and immutable per-attempt profile resolution
remain. Routine execution also remains synchronous pending durable occurrence and
authority contracts. Canonical launch dispatch remains disabled.

Validation passes 391 all-feature tests in both debug and release (92 library,
271 binary, 26 CLI, 2 contracts), and 248 default-feature tests. Logs:
`/tmp/herdr-kind-{debug,release,legacy,focused}.log`. Three optional live checks
remain ignored; macOS remains unavailable.

## W04 named profile inspection

`profile inspect NAME` now validates user-owned named profiles and emits redacted
JSON without reading environment values, invoking agents or creating project state.
Definitions bind kind and literal argument arrays, environment-name references,
permission policy references, optional model/effort intent and requested budgets.
Unknown fields, invalid names, duplicate environment entries, value assignments and
invalid limits fail explicitly. Parse errors withhold source text. Exact config and
normalized profile digests distinguish changes without printing argument values.

Launch, readiness, prompt, stop, checkpoint acknowledgment, usage and resume remain
separately unknown without adapter evidence. Launchable/protocol/certified results
remain false; model, environment, permission and budget requests expose unresolved
requirements. No profile inspection grants permission or silently generates flags.
The compatibility matrix records Herdr 0.9.1's advertised kinds separately from
unprobed agent versions and uncertified workflows. Named profile launch selection,
version probes and immutable attempt resolution remain T04.3 work.

Independent review approved this read-only increment. Two focused default-feature
unit tests and the redaction/no-write CLI fixture pass; the full all-feature suite
passes 394 tests (92 library, 273 binary, 27 CLI, 2 contracts). Logs:
`/tmp/herdr-profiles-{focused,cli,debug}.log`. Three optional live checks remain
ignored and macOS remains unavailable. Card counts remain unchanged.

## W04 explicit installation-version probes

`profile probe` accepts explicit absolute local executable paths and gathers bounded
Herdr/Claude/Codex version observations. It never applies profile arguments or
environment references. Unknown kinds have no executable probe adapter. Each command
has an owned process group, a five-second timeout and 4 KiB capture limits; malformed,
failed, cancelled, truncated and timed-out output cannot supply a version. Raw output
and runner errors are withheld. Binary/config changes reject the observation;
reports bind normalized versions (including prerelease suffixes), exact output
hashes, executable hashes and profile/config identities.

This remains local installation evidence: interpreter/dependency identity, running
server compatibility, verified capabilities, immutable attempt binding and launch
authority are not implied. Independent review corrected inconsistent nested version
fields and approved the increment. All 399 all-feature tests pass (92 library,
277 binary, 28 CLI, 2 contracts), including failure/change/redaction regressions and
a real-runner CLI fixture. Focused default-feature tests also pass. Logs:
`/tmp/herdr-profile-probe-{focused,cli,debug}.log`.

A disposable-home live probe observed Herdr 0.9.1 and Codex 0.154.0; its
report is `/tmp/herdr-profile-probe-live.json`. An earlier PATH-based help query hit
a mise shim attempting tool installation and was interrupted; no tools installed.
The probe interface requires explicit executable paths to let operators select the
actual installation. Three optional live workflow checks remain ignored; no live
agent session or real user project was started. W04 and the full goal remain active.

## W04 frozen effective profile inputs (schema 12)

Version-2 launch inputs now retain the effective profile definition/config identity,
argument digest, environment names, exact executable/version identities, permission
and adapter references, and separate capability evidence. The profile's
content-addressed reference must match this record. Reservations require supported
launch/readiness/prompt/stop evidence and matching config/runtime kind; absent usage,
checkpoint or resume support remains explicit. No credentials or argv values are
added to attempt records, projections or events.

Schema 12 prevents old clients from writing new-format records and rejects new
old-format reservations. Existing version-1 inputs preserve serialization and
content IDs. Independent review required a literal historical v1 fixture; that
fixture now verifies unchanged payloads, IDs, events and head across upgrade and
reopen, plus safe never-claimed cancellation. The review approved the increment.
The sealed preparation still has no production adapter/policy producer, and launch
dispatch remains disabled. Version probes do not fabricate capability evidence.

Validation passes all 403 tests in debug and release (96 library, 277 binary,
28 CLI, 2 contracts). Logs: `/tmp/herdr-frozen-profile-{debug,release,lib,upgrade}.log`.
Three optional live checks remain ignored; macOS remains unavailable. No user store
was upgraded. Overall card counts remain 16 implemented, three partial, 22 not started.

## W04 operation-scoped approval contract

The new inert approval data contract binds exact version-2 launch inputs without
circular references: the action digest excludes only the grant reference, while
matching checks that final reference separately. Project identity comes from the
store caller. Config/profile/binary/resource/revision changes, another project and
expired/not-yet-valid grants fail matching. Object keys sort explicitly and a fixed
digest fixture protects the encoding contract. Unknown actor fields and unsupported
operation classes are refused; a parsed grant is never an authenticated credential.

Independent review approved this contract. All 99 library tests pass in debug and
release; focused approval tests additionally cover the golden digest in both modes.
Logs: `/tmp/herdr-approval-{lib,release,golden-debug,golden-release}.log`.
Trusted issuance, durable grant storage/revocation and atomic one-time consumption
with operation claims remain required. Canonical dispatch remains disabled. T04.5 is
now partial: 16 cards implemented, four partial, 21 not started (25 unfinished).

## W04 durable scoped approvals (schema 13)

Immutable grant, revocation and consumption records now support exact-action launch
claims. Grant installation accepts only a sealed internal capability; trusted
production issuance is still unavailable. Consumption and claim updates commit
atomically. Expired, revoked, missing, corrupt or already-used grants refuse; a
confirmed no-effect retry cannot silently reuse its consumed grant. Pre-effect
validation checks the same consumption and current control/config/policy/binding.
Snapshots and runtime projections expose validated grant/use/revocation history.

Independent review found that attempt-only mutations could invalidate capacity
without changing the task revision. Claim and pre-effect checks now require the exact
Running task's active Reserved attempt at its original revision, retained capacity,
expected reservation identity and no conflicting worker ownership. Regression tests
mutate attempts through supported Commit before and after claim. Other fixtures cover
rollback after failed claim writes, reopen, revocation, expiry and damaged grants.
Schema-12 pending/claimed launches upgrade without invented authority or released
capacity, preserving existing input/delivery/event records and remaining blocked.

Trusted control-route issuance, policy-change ingress, denial auditing and authority
coverage beyond launches remain T04.5 work. Canonical dispatch remains disabled; no
user store was upgraded. Overall counts remain 16 implemented, four partial and
21 not started. macOS remains unavailable.

Final independent review approved the fixes and nine focused approval regressions.
All 412 tests pass in debug and release (105 library, 277 binary, 28 CLI, 2 contracts).
A separately calculated fixed grant digest also passes its focused regression.
Logs: `/tmp/herdr-grants-{lib,debug,release,golden}.log`. Three optional live checks
remain ignored. Historical-schema test fixtures now remove approval tables when
emulating an older store; production upgrades do not remove or rewrite authority.

## W04 signed owner-control ingress

The approval CLI now verifies exact grant documents using Ed25519 SSH signatures
in the `approval@herdr-projects` namespace. The trusted public key comes from the
migration-pinned owner configuration, outside the project, owned by the current
user and not group/world writable. Caller HOME, actor strings and document fields
cannot select another trust key. The verifier uses the shared bounded production
runner, a five-second deadline, private temporary files and capped output. The
application never reads the signing private key.

Imports fence the canonical head and acknowledged config and install only sealed,
verified grants. Disk config edits now withdraw launch authority at claim and
pre-effect validation, even before the control epoch changes. CLI policy/inspect
are read-only; import and revocation use the guarded canonical mutation path.
Canonical launch dispatch remains disabled pending profile preparation and the
carried crash gate. Same-OS-user filesystem bypass remains outside the guarantee.

Independent review approved this increment. Real-signature fixtures cover exact
bytes, key and namespace, policy location/permissions, malformed input and config
withdrawal. A CLI fixture confirms the pinned policy ignores caller HOME and
unsigned import preserves state. All 417 tests pass in debug and release (125
library, 261 binary, 29 CLI, 2 contracts); three optional live tests remain ignored.
Logs: `/tmp/herdr-signed-{debug,release}.log`. No user project was migrated or
launched. Policy-change ingress, denial audit and other command rights remain.

## W04 durable admission budgets (schema 14)

Owner-signed budget documents now install sequential, immutable policy revisions
through a separate `budget@herdr-projects` signature namespace. Documents bind the
canonical project store and pinned owner authority. Imports check the current head
and acknowledged config; stale/replayed revisions, wrong namespace and cross-project
documents preserve history and head. CLI inspection exposes policy and blockers.

Lifetime attempt limits count imported, cancelled, completed and retained attempts.
Reservation checks are atomic with creation; cancellation does not refund an
admission. Claim/pre-effect checks allow an already-reserved attempt at the count
limit but reject changed policy. No denial releases uncertain worker capacity.
Provider tokens remain explicitly unknown. Positive thresholds either refuse or
mark admission incomplete according to signed policy; zero blocks in either mode.
These are admission controls, not provider billing caps or running-worker stops.

Schema-13 upgrade fixtures preserve pending inputs, deliveries and events without
inventing policy. Historical-schema fixtures omit the new table, and projections
include budget history only in schema 14. No user store has been upgraded.
All 422 tests pass in debug and release (130 library, 261 binary, 29 CLI, 2 contracts);
three optional live tests remain ignored. Logs: `/tmp/herdr-budget-{debug,release}.log`.
Final queue-report optimization computes project budget blockers once per report.
Five focused tests pass again in debug and release after that optimization
(`/tmp/herdr-budget-final{,-release}.log`). Independent review approved the budget
increment, including exact-limit, zero-token, signature and policy-change fixtures.

T04.4 is now partial: native usage, versioned estimates, wall-time actions, durable
routine occurrences and wider telemetry remain. Overall status is 16 implemented,
five partial and 20 not started. Canonical launch still awaits production preparation
and carried crash certification. macOS remains unavailable.

## W04 project outbox and bounded routine schedule windows (schema 15)

Project-scoped outbox records now bind project-control revision, while task-scoped
records retain their original revision checks and JSON representation. Only the
routine operation kind can omit a task. Generic enqueue and legacy import cannot
produce routine work; a sealed producer remains pending. Claims require active,
reconciled control and exact revision. A control change after claim prevents effects
and leaves uncertain delivery for reconciliation without creating a synthetic task.

The migration preserves dependent delivery, attempt-input and approval-use tables
while replacing the outbox parent with foreign keys enabled. A populated fixture
starts from the exact schema-14 migrations with historical v1 inputs, a v2 claimed
launch and consumed approval. Full snapshots, head, payloads and claim counters
survive upgrade/reopen; immutable and monotonic triggers still reject edits. Failures
injected after dependent-table removal, parent removal and metadata update roll back
to the original usable store. No user store was upgraded.

The schedule parser and daily resolution are shared between the binary and library.
A pure due-window helper uses anchored interval arithmetic and bounded calendar
endpoint searches with explicit timezone semantics. Independent review caught an
Apia skipped-date case returning a future endpoint; both the new helper and legacy
daily checker now search backward until the instant is due. Fixtures compare with
enumerated calendar slots across five zones, exercise repeated/gapped times and
centuries of downtime, and prove cursor replay/backward-clock behavior.

Durable routine revisions, occurrence/cursor transactions, overlap/missed-run records
and the bounded execution adapter remain next. Counts stay 16 implemented, five
partial and 20 not started; this increment does not complete T04.4 or enable launches.

Independent review approved the corrected schedule and populated migration fixture.
All 428 tests pass in final debug and release runs (136 library, 261 binary, 29 CLI,
2 contracts); three optional live checks remain ignored. Logs:
`/tmp/herdr-project-ops-final-{debug,release}.log`. The default-feature suite also
passed before the final skipped-date correction; final all-feature runs include the
legacy routine regressions. macOS remains unavailable.

## W04 signed durable routine occurrences (schema 16)

Owner-signed definitions now bind routine revision, project/config/authority,
timezone/schedule/start, missed/overlap policy and bounded script execution inputs.
Enabled routines require the explicit owner `routine_commands` setting. Imports use
a separate SSH signature namespace and reject replay, cross-project and stale config.
Independent review found an A→B→A config replacement race; permission parsing now
hashes the exact bytes against the signed identity before reading the enable flag.

An immediate transaction records the immutable occurrence, cursor, event and
project-scoped operation. Restart/duplicate scans cannot enqueue the same scheduled
instant twice. Skip/coalesce decisions are visible, with bounded catch-up arithmetic.
Historical instants remain authoritative across timezone-data updates. Replacing a
revision retires only Pending zero-claim intents; ever-claimed history conservatively
blocks overlap even after generic confirmation/retirement, pending typed cleanup
receipts. Claim/pre-effect checks revalidate current revision, owner config and script.

The `routine-store` CLI imports, inspects and records due occurrences without running
scripts. A real-key CLI fixture signs/imports/schedules successfully, rejects stale
heads, and proves the script marker was not created. Other fixtures cover config and
script withdrawal, disabled permissions, rollback, reopen, concurrent scans, missed
runs, overlap and schema-15 preservation without invented routine authority.

Independent review approved the fix and seven focused routine tests. All 436 tests
pass in final debug and release (143 library, 261 binary, 30 CLI, 2 contracts); three
optional live checks remain ignored. Logs: `/tmp/herdr-routines-final-{debug,release}.log`.
No user store was upgraded or script executed. Ticker scheduling/dispatch, typed
completion and termination receipts, overlap release and output inbox delivery remain
next. Counts stay 16 implemented, five partial and 20 not started; macOS is unavailable.

## W04 explicit routine execution and verified cleanup

The signed routine path now supports explicit Linux execution. It retains mutation
ownership across claim, pre-effect validation, bounded execution and receipt commit,
and feeds the exact verified script bytes over stdin. The fixed namespace supervisor
clears inherited environment before exec and maps command results to reserved exit
statuses. Only normally reaped supervisor exits certify descendant cleanup; bootstrap
errors, outer timeout, cancellation and I/O failures remain uncertain. Ordinary script
failure and deadline expiry both report failed execution without inventing an exact
exit classification. Detached process groups cannot survive verified namespace exit.

Typed, digest-bound completion events use the existing schema-16 event log. Output
inbox delivery, operation outcome and receipt commit atomically. Snapshots validate
receipt identity, approved revision, claim and output bounds. Overlap releases only
for a matching verified cleanup receipt; generic confirmation/retirement cannot do so.
Any prior claim prevents replay, including after generic no-effect retry advice.
Missing/expired/uncommitted receipts continue to block overlap. Output is observation,
never task authority. Existing projection heads without receipts retain their bytes.

Independent review approved the cleanup contract and implementation. Review also
identified quadratic receipt lookups; indexed occurrence/revision/cleanup maps now
avoid nested history scans. Full-suite namespace isolation exposed mapped host-root
UID handling; fixed helper paths compare with the mapped system owner as a sanity
check under the trusted-OS assumption. This does not independently attest host-root
ownership when UIDs share an overflow mapping.

Eight new tests cover real detached-child teardown, timeout, cancellation, bootstrap
failure, environment/output bounds, atomic receipt rollback, reopen, stale/misbound
completion and withheld cleanup. The real-key CLI fixture now schedules without
effects, refuses edited script bytes, explicitly executes once, persists output and
cleanup, and refuses replay after reopening. All 444 tests pass in debug and release
(151 library, 261 binary, 30 CLI, 2 contracts); three optional live checks remain
ignored. The default-feature suite also passes. Logs:
`/tmp/herdr-routine-execution-final-{debug,release}.log` and
`/tmp/herdr-routine-execution-default.log`.

No user store or script was used. Explicit execution holds root/project mutation
ownership synchronously; automatic ticker scheduling/dispatch and asynchronous
ownership remain next. T04.4 still includes usage/estimates/running budget/telemetry
work. Counts remain 16 implemented, five partial and 20 not started; macOS is unavailable.

## W04 automatic routine occurrence scheduling

Each canonical controller pass now schedules one latest enabled routine in rotating
name order. Scheduling retains the existing mutation guard, revalidates signed
authority/script inputs and uses the durable occurrence transaction. Paused,
unreconciled and pre-schema-16 projects do not schedule. This stage records intent;
automatic command dispatch remains pending asynchronous ownership integration.

Enabled routine work now separately contributes to ticker lifetime, including future
due instants and diagnostic turns. It does not assert observed session reachability.
Keeping the ticker alive on an invalid selected script prevents a long prefix of
invalid routines from exhausting the idle grace before a healthy sibling's turn.
Scheduling diagnostics stay visible while independent notification/finalization
processing continues in the same pass.

Independent review approved this increment. A real-key ticker fixture covers an edited
routine next to a healthy signed routine, zero script execution, a single durable
occurrence across controller restart, future-work liveness, pause, and notification
delivery despite the routine diagnostic. All 445 tests pass in debug and release
(151 library, 262 binary, 30 CLI, 2 contracts); three optional live checks remain
ignored. Logs: `/tmp/herdr-routine-ticker-{debug,release}.log`.

Next is asynchronous command dispatch and ownership, followed by the remaining W04
profile, budget, telemetry and authority integration. Counts remain 16 implemented,
five partial and 20 not started; macOS remains unavailable.

## W04 cancellation-aware routine queue bridge

The bounded executor can now carry routine jobs through a trusted adapter. Queued
identity binds operation and delivery revision rather than a stale global event head.
Worker entry revalidates current authority and exact script bytes before claiming;
cancellation, stale revisions and insufficient execution/cleanup allowance refuse
without a claim. The trusted service still uses concrete process containment and
persists its sealed receipt itself; injected observation runners and pool output
cannot manufacture cleanup authority. Transfer-lane jobs serialize per root under
the retained execution guard.

Independent review found that reconstructing the remaining deadline at adapter entry
could extend the original queue budget. Commands now carry an absolute monotonic
deadline from admission through worker entry, trusted ingress and concrete collection.
The runner refuses spawn after expiry and applies the earlier of relative timeout and
absolute deadline while collecting. A delayed-adapter regression proves the deadline
cannot be restarted at process entry.

Real-key queue fixtures cover script withdrawal while actually queued, unrelated
head changes, stale delivery revisions, insufficient/expired budgets, cancelled jobs,
control-lane progress during a running routine, cancellation/drain and receipt-commit
failure across executor restart. Claimed work is never replayed; cancellation retains
uncertain cleanup. A shared signed fixture also preserves the ticker regressions.

Independent review approved the implementation and deadline correction. All 452 tests
pass in debug and release (152 library, 268 binary, 30 CLI, 2 contracts); three optional
live checks remain ignored. The default-feature suite also passes. Logs:
`/tmp/herdr-routine-queue-{debug,release,default}.log`.

The ticker does not yet admit these jobs. Root-wide mutation ownership still obstructs
unrelated guarded status updates despite free control workers; project/resource
ownership refinement is the next prerequisite for automatic dispatch. No user scripts
or stores were used. Counts remain 16 implemented, five partial and 20 not started;
macOS remains unavailable.

## W04 project-scoped execution ownership

Canonical runtime mutations and routine execution now retain a shared root barrier
on the existing `.execution.lock` inode, exclusive `.state/effect.lock` ownership for
their project, and the record lock. The shared implementation rejects symlinks and
special lock files without blocking; failed acquisitions release earlier guards.
Effect locks are excluded from migration source inventory. Migration, cleanup,
adoption/conflict scanning, existing external adapters and legacy ticker passes retain
the root-exclusive barrier. No lock is upgraded in place.

The canonical controller now calls a guarded library service covering observation
commit, control-marker publication and claim expiry, rather than retaining an exclusive
root lease around those writes. The guard is not recursively reacquired by helpers.
This permits another canonical project's status refresh and task mutation while a
routine runs, while preserving same-project and root-wide maintenance exclusion.

Independent review approved the protocol and call paths. Lock fixtures cover separate
projects, exclusive barriers, symlink/FIFO refusal and release after record-lock failure.
The real running-routine fixture now creates a second project in the same root and
proves its guarded observations/task writes advance while mutation of the running
project, cleanup and schema upgrade refuse. All 455 tests pass in debug and release
(155 library, 268 binary, 30 CLI, 2 contracts); three optional live tests remain ignored.
The default-feature suite also passes. Logs:
`/tmp/herdr-project-ownership-{debug,release,default}.log`.

Automatic routine dispatch still awaits separation of legacy observational status
from prompt/token-metadata effects. Project ownership alone does not authorize
concurrent effects on aliased terminals. Counts remain 16 implemented, five partial
and 20 not started; macOS remains unavailable.

## W04 legacy observation under project ownership

When the exclusive root barrier is occupied by another project, the cheap legacy
pass can acquire shared root/project ownership and update local observations. It
sends no prompts, writes no terminal metadata, and makes no fresh report-copy claims.
Whole-session loss remains separately deduplicated by the exclusive slow pass.

Status changes and rendered notices commit atomically in the thread record with a
monotonic sequence and execution fingerprint. Inbox replay uses the stored identity
and content, including already-handled items; clearing the intent is conditional.
A replacement execution retains its own status while the historical notice drains.
Pending ticker events, retries and copy receipts remain untouched. Migration converts
these notices to legacy inbox obligations even without a ticker state file, and
refuses inconsistent identities/sequences.

Fixtures exercise blocked inbox delivery, crash after delivery, execution replacement,
unchanged copied-report markers, retained retry state, same-project/root exclusion,
and migration preservation. Automatic queue admission remains the next step.
Counts remain 16 implemented, five partial and 20 not started; macOS is unavailable.

Independent review approved this increment. All 458 tests pass in debug and release
(156 library, 270 binary, 30 CLI, two contracts), and the default-feature suite passes.
Three optional live checks remain ignored. Logs:
`/tmp/herdr-status-observation-{debug,release,default}.log`.

## W04 automatic routine queue admission

The production ticker now wraps the shared executor with trusted routine ingress and
admits due, pending, never-claimed routine operations on Linux. It drains volatile
tickets at tick entry and keeps the ticker alive while a ticket is outstanding.
Completion output changes no durable delivery state; the trusted service persists
its own receipt. Restart reconstructs eligibility from the store and never replays a
claimed occurrence. Shutdown cancels/drains the same pool used for observation jobs.

Independent review identified two starvation risks before enabling dispatch. Only one
automatic routine ticket may be outstanding per root, and admission occurs after all
legacy/canonical project effects have received a turn. A mid-pass completion stays
retained until the next pass. Project selection uses a separate last-admitted cursor,
advancing only on accepted tickets, rather than coupling fairness to ticker cadence.
Failed entries back off for 30 seconds and then rotate among pending operations.
Project admission inventory remains bounded at 128 entries.

Real signed fixtures exercise automatic execution and ticker restart without replay,
script withdrawal/backoff/rotation, pause refusal, retained completed tickets,
cross-project notification delivery before admission, and repeated identical project
scan orders with backlog. Existing queued authority/deadline, lost-receipt and
cancellation/descendant-cleanup fixtures remain in place. CLI inspection now advertises
automatic dispatch only on Linux. No user scripts or projects were used.

Counts remain 16 implemented, five partial and 20 not started. W04 still needs artifact
pool integration, production profile/capability resolution, usage/running-limit
telemetry and broader command-path authority; macOS remains unavailable.

Crash-boundary review also found that process-local guards alone released too early
if the ticker/explicit CLI was killed. Routine supervision now inherits the root and
project lock descriptions plus a root routine-serialization lock; parent descriptors
remain CLOEXEC and only the forked child's duplicates become inheritable. The caller
retains its copies through receipt commit. A surviving supervisor preserves exclusion
until namespace cleanup even after owner SIGKILL. The real owner-death fixture proves
immediate competing root effects, same-project routines and root routine ownership
refuse, delayed detached effects are killed, and the lost receipt never permits replay.
This fixes explicit execution as well as the new automatic path.

Independent review approved the fairness and crash corrections. All 465 tests pass in
debug and release (156 library, 277 binary, 30 CLI, two contracts); the default-feature
suite also passes. Three optional live checks remain ignored. Logs:
`/tmp/herdr-routine-admission-{debug,release,default}.log`.

## W04 surviving transfer supervision

Independent design review identified that inherited locks alone were insufficient for
copies: arbitrary SSH/rsync tools may close those descriptors, and a killed caller can
no longer enforce RealRunner's deadline. The new supervision service retains ownership
in fixed trusted namespace/timeout processes outside the target executable. The shared
ProjectGuard API supplies root, project and root-transfer lock descriptions; routine
execution reuses that API without changing its signed-script environment or receipts.

The immediate execution API preserves literal argv/stdin and binary output sinks,
uses the earlier absolute deadline while the caller lives, and gives surviving helpers
a bounded relative timeout. It reserves five seconds for cleanup and normalizes failed
commands to exit 200. Review also required a clean restricted bootstrap environment;
loader and shell-startup settings never reach supervision, unsupported explicit
settings fail without name/value diagnostics, and allowed values have byte limits.

Six focused fixtures cover literal metacharacters, binary output, environment policy,
normal failures/capture caps, deadlines/cancellation, and real owner SIGKILL while a
target closes all non-stdio descriptors and starts a detached child. The latter proves
root/project/transfer exclusion persists until bounded cleanup and delayed descendant
effects do not occur. Independent review approved the corrected prerequisite.

Copy publication is not wired to the service yet. Bounded source handling, durable
partial-copy obligations and shared copy/routine admission remain the next work.
Counts remain 16 implemented, five partial and 20 not started; macOS is unavailable.

All 471 tests pass in debug and release (162 library, 277 binary, 30 CLI, two
contracts), and the default-feature suite passes. Three optional live checks remain
ignored. Logs: `/tmp/herdr-transfer-supervision-{debug,release,default}.log`.

## W04 bounded artifact source reads

Local preservation and native stream export now traverse source directories through
opened descriptors, refusing symlink ancestors and entries. Enumeration uses a fresh
directory stream on every scan, checks errors separately from EOF, and bounds entry
collection, nesting and aggregate bytes while reading. File metadata is checked before
and after reads; exports hash the bytes actually streamed. Opened source-root identity
is checked after verification and immediately before publication or snapshot reuse.
The schema-1 manifest format and historical local root entry types remain compatible.

Local live-report hashing and copying use the same bounded reader. Oversized or unsafe
reports fail without replacing the last home report or advancing its copy receipt;
slow ticker passes expose source errors instead of treating them as absent reports.
The ten-second per-scan deadline is cooperative between filesystem calls, not a hard
interrupt for blocked native I/O. All source ancestors must have physical path spellings;
macOS alias handling is documented and macOS execution remains untested.

Eight new regressions cover repeated enumeration, ancestor replacement, missing versus
unsafe sources, growth during reading, limits/deadlines/cancellation, root replacement
before publication, excessive nesting, oversized-report retry, and historical schema
compatibility. Independent review approved after correcting historical root-shape
compatibility and moving the final root check after all publication verification.

Artifact pool integration and durable partial-copy delivery are still outstanding.
Counts remain 16 implemented, five partial and 20 not started.

All 479 tests pass in debug and release (162 library, 285 binary, 30 CLI, two
contracts); the default-feature suite also passes. Three optional live checks remain
ignored. Logs: `/tmp/herdr-source-tree-{debug,release,default}.log`.

## W04 durable live-copy warnings

Live local and remote ticker copies now commit a typed receipt with actual copied
report hash, execution fingerprint, monotonic sequence and bounded partial notes.
Partial receipts atomically prepare an immutable warning payload. Delivery uses
`write_once` and clears only the exact successfully delivered intent; a pending
warning prevents receipt replacement. Delivery does not wait for Ready/Landing or
an available Herdr session, and historical warnings survive execution replacement
without changing replacement state. Receipt notes remain available to later review
announcements; the transient ticker notes map has been removed.

Identical current receipts reuse their sequence; changed copies (including A/B/A
and changed notes for identical report bytes) get new sequences. Receipt updates
fence execution, prior hash and prior receipt and require an open thread without
removal intent. Failed libraries, including failure after report publication, never
advance the successful copy hash. Migration validates receipts and converts pending
warnings to legacy inbox operations even without a ticker-state file.

Regression fixtures cover restart before review readiness, failed delivery, a crash
after delivery but before acknowledgement, inbox/done replay, execution replacement,
stale copy completion, repeated hashes, changed notes, partial-library retry and
migration corruption. This does not yet make review announcements themselves
crash-idempotent; that and transfer-pool admission remain upcoming work.
Counts remain 16 implemented, five partial and 20 not started; macOS is untested.

Independent review approved this bounded increment after checking receipt fences,
warning replay, retained readiness notes and migration without ticker state.
All 484 tests pass in debug and release (163 library, 289 binary, 30 CLI, two
contracts), and the default-feature suite passes. Three optional live checks remain
ignored. Logs: `/tmp/herdr-copy-delivery-{debug,release,default}.log`.

## W04 durable review announcements

Ready/Landing announcements now prepare immutable notices under the short record
lock before calling inbox `write_once`. Thread records retain a monotonic notice
sequence, the pending payload and execution/copy-specific acknowledgement fields.
Identical typed receipts do not reannounce; A/B/A content or changed copy notes get
distinct receipt-bound notices. Legacy reports without matching typed receipts retain
their opaque hash compatibility and already-acknowledged state; malformed receipts
still refuse. Historical receipts are not treated as evidence for a replacement.

Pending announcements prevent another live copy at both admission and receipt
commit. Prepared notices drain independently of current readiness or session access.
Only the exact delivered pending payload is cleared. Acknowledgement updates require
the original execution, hash and matching receipt, so historical delivery cannot
acknowledge a replacement. Notice bodies identify the historical hash/execution and
explain that the home report path can change. Migration validates the notices and
converts them to inbox operations even without ticker state.

Four delivery regressions cover failed delivery, success-before-ack crash replay,
inbox/done replay, readiness changes, identical-report execution replacement, A/B/A,
changed copy notes, legacy acknowledgements and stale preparation. A migration
regression verifies conversion and identity corruption refusal. Independent review
approved the implementation and independently reran the focused tests.

Transfer-pool admission and remaining W04 work are still outstanding. Counts remain
16 implemented, five partial and 20 not started. macOS remains untested.

All 489 tests pass in debug and release (164 library, 293 binary, 30 CLI, two
contracts), and the default-feature suite passes. The existing finishing-thread
scenario now checks the provenance body while retaining its one-notice/one-nudge
assertions. Three optional live checks remain ignored. Logs:
`/tmp/herdr-review-delivery-{debug,release,default}.log`.

Design review for the next transfer step calls for a separate bounded live-copy
protocol and verified staging type, preserving schema-1 strict finalization. Report
and library retain independent caps; an oversized library must not block a valid
report. Bounded omissions must describe additive projection accurately, and partial
live staging must never authorize destructive cleanup. Unsupported remote helpers
must refuse without falling back to unbounded shell/rsync transfers.

## W04 bounded live-copy protocol and private staging

A separate live-copy protocol now exports bounded partial projections without
weakening strict schema-1 preservation. The native helper advertises live version 1
and accepts `--live`. Report/library byte budgets remain independent. Included files
are streamed with actual-byte hashing and a final anchored source recheck; links and
unsupported entries become bounded structured omissions. Library limits discard the
whole library inventory instead of sending an arbitrary traversal prefix, preserving
valid report progress. Typed source-limit errors distinguish omission policy from
I/O/deadline/source-change failures; directory scans now detect observed mutations.

The receiver verifies manifest paths, sizes, hashes, framing and no trailing data,
then returns a private stage under `.state/live-copies`. It cannot produce a cleanup
snapshot. Staging is removed on failure/drop. Six regressions cover binary/hostile
names/empty directories, links, each library limit, independent byte caps, source
mutation, corrupt/truncated/trailing/wrong-protocol data, and malformed manifests.
The existing CLI fixture now checks backward-compatible probe fields and live
partial export while strict preservation still refuses the same linked source.

Independent review approved the protocol/staging increment and reran focused tests.
The receiver does not certify sender success or execution authority: transfer-pool
integration must gate on supervised sender completion and current task/config/route
before publication. Projection and executor admission remain outstanding. Counts
remain 16 implemented, five partial and 20 not started; macOS remains untested.

Full validation exposed a section-deadline bug under concurrent build load: the
library timer began before report streaming. Each section now starts its budget on
first use, with checks after file flushes and between directory flushes. Independent
review approved the correction. All 495 tests pass in debug and release (164 library,
299 binary, 30 CLI, two contracts), and the default-feature suite passes. Three
optional live checks remain ignored. Logs:
`/tmp/herdr-live-stage-{debug,release,default}.log`.

## W04 recoverable live-copy publication

The guarded publication API now retains and fsyncs the exact verified stage and
manifest before committing a typed per-thread intent. That intent binds execution,
prior report hash/receipt, authority digest, sequence and staged report/manifest
hashes. Recovery revalidates those fences and resumes the same stage; it never
silently substitutes a newer source. Missing/corrupt retained bytes refuse.

Destination traversal is descriptor-relative. Included files use exclusive temporary
files, fsynced bytes, atomic rename and directory flush; unrelated/omitted files remain
untouched. Library publication precedes report publication. Home bytes are verified
before atomically committing the copy receipt and clearing intent. Stage retirement
follows durable receipt commit. Whole-projection atomicity is not claimed: pending
intent explicitly represents intermediate state. Staging admission is capped at 16
entries per project without preventing recovery of an existing intent.

Pending projection blocks unrelated copy completion, review preparation, execution
or lifecycle replacement, finalization/removal, project deletion and migration.
The publication API is still private groundwork for the supervised sender/executor;
no production copy dispatcher uses it yet. Counts remain 16 implemented, five partial
and 20 not started; macOS remains untested.

Independent review found and verified fixes for a missing ancestor-directory fsync
and a possible temporary-name/destination collision. Six projection fixtures cover
additive publication, source-loss recovery, report-written-before-receipt interruption,
missing/corrupt retained stages, wrong ownership, destination links and recovery at
the staging admission cap. A deterministic
atomic-write fixture verifies collision handling and hidden partial writes; a migration
fixture verifies refusal, respecting redacted inspector diagnostics.

All 503 tests pass in debug and release (165 library, 306 binary, 30 CLI, two
contracts), and the default-feature suite passes. Three optional live checks remain
ignored. Logs: `/tmp/herdr-live-projection-{debug,release,default}.log`.


## W04 live-copy cancellation and transport reservation

Live receive and guarded publication now accept one control object carrying the
original admission deadline and cancellation token. Each section's read budget is
capped by that deadline; verification, destination publication, intent creation and
receipt persistence check cancellation. The receipt check also runs under the short
record lock. A cancelled partial publication retains its exact intent and bytes for
recovery. These are cooperative filesystem checks, not interruption of blocked
filesystem syscalls.

A temporary download spool now reserves space in the same bounded 16-entry inventory
before transport, leaving one slot for extraction. Abandoned downloads therefore
count alongside retained stages. Dropping a spool removes only its own temporary
path; it cannot remove an intent-owned recovery stage. This API is groundwork for
the supervised worker; automatic copy dispatch remains disabled.

Independent review approved the increment and independently passed all 16 live-copy
fixtures. Four new fixtures cover pre-effect cancellation/expiry, cancellation during
publication and immediately before receipt commit, exact-stage recovery, and shared
spool/stage capacity with temporary ownership cleanup. Card counts remain 16 locally
implemented, five partial and 20 not started; macOS remains untested.

All 507 tests pass in debug and release (165 library, 310 binary, 30 CLI, two
contracts). The default-feature suite passes (29 library, 263 binary, 11 CLI, two
contracts). Three optional live checks remain ignored. Logs:
`/tmp/herdr-live-control-{debug,release,default}.log`.


## W04 trusted supervised copy worker

The bounded transfer lane now has a live-copy request/worker adapter. Requests freeze
canonical project path and filesystem identity, execution fingerprint, prior hash and
receipt, pending projection/sequence, configuration path and digest (including absent
versus empty), helper selection and machine target. Worker entry acquires project and
inherited transfer ownership and revalidates those inputs before effects.

The service uses concrete surviving process supervision for machine discovery, remote
capability probing and streaming. Remote helpers must advertise live protocol 1;
unsupported helpers never fall back to shell/rsync copy. Sender success is mandatory
before parsing or publication, and current configuration/routing is checked again
before publication. Original queue deadline and cancellation carry through transport,
receive, verification and receipt persistence. Temporary spools are reserved before
transport and removed separately from intent-owned recovery stages.

Pending projections resume exact retained bytes with current matching authority;
recovery does not probe or fetch the source. A changed configuration blocks recovery
while preserving its intent. Transfer ownership spans publication and durable receipt
commit. Completion output performs no durable certification. Automatic ticker
admission is still disabled pending shared routine/copy fairness integration.

Independent review approved the service and independently passed eight real-process
fixtures covering binary/literal local transfer, valid bytes from a failed sender,
stale execution/receipt/configuration/project, expired/cancelled requests, exact
source-loss recovery, remote capability/route changes, mid-stream cancellation and
ownership release, and refusal to trust an injected observation Runner. Remote tests
use disposable local SSH/helper fixtures; actual remote-host interoperability is not
claimed. Card counts remain 16 locally implemented, five partial and 20 not started;
macOS remains untested.

All 515 tests pass in debug and release (165 library, 318 binary, 30 CLI, two
contracts). Default-feature tests also pass (29 library, 271 binary, 11 CLI, two
contracts); three optional live checks remain ignored. Logs:
`/tmp/herdr-copy-worker-{debug,release,default}.log`.


## W04 automatic copy admission and shared background fairness

Production Linux tickers now wrap the shared executor with the trusted copy worker.
Local and remote observations offer copy requests; native transport and additive
publication execute after the full legacy/canonical project pass. A copy and a routine
cannot both hold a background ticket. Successful admission alternates the preferred
kind; completion is drained only at tick entry, so even a mid-pass completion cannot
skip the next full opportunity for guarded project effects. Stop drains the same
shared pool. Other platforms retain their existing path and remain untested.

Copy offers/recent metadata are bounded to 128 entries, failures back off for 30
seconds, and idle offers expire after 180 seconds. Normal selection rotates projects
and their threads, advancing only on accepted submission. The inventory retains the
nearest candidates to the admission cursor when saturated; refreshed failing offers
cannot permanently exclude new projects. After the bounded 128-project cursor history
fills, selection switches for that process to one cyclic project/thread cursor. This
preserves eventual service for every continuously offered pair in a finite backlog
with bounded history; overflow does not promise equal per-project turn frequency.
Pending tickets are never evicted, and volatile offer eviction never deletes a durable
recovery intent.

Pending projections are offered before checking session availability. Remote recovery
may omit an observed target: the worker resolves current routing under supervision,
then requires the complete authority digest to match the retained intent. It does not
probe or fetch from the source host. New review notices wait while a newer copy is due
or outstanding; remote notice preparation also requires a fresh observation. Already
persisted notices continue to replay independently.

Independent review found and verified fixes for stale-review ordering and two queue
saturation starvation cases. Five queue fixtures cover rotation, bounded eviction,
backoff, pending-ticket retention and overflow thread fairness. Ticker fixtures cover
copy/routine alternation, canonical effect opportunity, old unannounced reports and
source/session-independent recovery. A built-binary subprocess fixture exercises
actual current-executable transport, binary library/report publication, durable receipt,
announcement after restart and no recopy on another unchanged-report restart.
Independent review approved the final integration and reran the queue and native
subprocess fixtures.

Local report hashing and other remaining slow effects still need isolation, and
canonical launch/profile/budget/authority work remains open. Counts remain 16 locally
implemented, five partial and 20 not started; actual remote-host and macOS acceptance
remain untested.

All 525 tests pass in debug and release (165 library, 327 binary, 31 CLI, two
contracts). Default-feature tests pass (29 library, 278 binary, 12 CLI, two contracts).
Three optional live checks remain ignored. Logs:
`/tmp/herdr-copy-admission-{debug,release,default}.log`.


## W04 asynchronous local report observations

Production tickers now offer local report hashing to the shared transfer executor
after guarded background admission. The configuration-independent native helper
streams at most 50 MiB through descriptor-relative regular-file reads, with a
10-second execution timeout and bounded JSON output. Sixteen pending reads and
128 candidate offers bound inventory; reads hold no project effect lease.

Completed observations are collected only at pass entry and fenced by execution,
prior report hash, copy receipt and pending projection. Their original 30-second
queue deadline also bounds freshness. Missing or expired observations defer new
copy/review decisions while ordinary status updates continue. Existing persisted
notices and retained projection recovery do not require a fresh source read.
Observations never certify copied bytes or durable receipts.

Copy and read admission share a bounded cyclic cursor implementation. Read
selection advances its persistent cursor only for actual Runner entry, ordered
by recorded start time; expired queue tails retain their service opportunity.
Independent review identified and verified fixes for expired results and queue-tail
starvation, then approved the final changes and reran all five focused fixtures.
The built ticker fixture passes asynchronous hashing, native copy, announcement
and unchanged-report restart. Other slow effects and canonical launch/profile/
budget/authority work remain open; card counts stay 16 implemented, five partial
and 20 not started. macOS and actual remote-host acceptance remain untested.

All 531 tests pass in debug and release (165 library, 332 binary, 32 CLI, two
contracts). Default-feature tests pass (29 library, 283 binary, 13 CLI, two
contracts); three optional live checks remain ignored. Logs:
`/tmp/herdr-local-reports-{debug,release,default}.log`.


## W04 supervised legacy routine commands

Linux legacy command routines now use the shared bounded executor and the existing
128-entry copy/legacy offer inventory. Stable routine keys receive project/operation
rotation and share the one pending background ticket; canonical routine admission
continues alternating with that inventory after full project effect passes. Failed
or evicted offers leave the durable schedule cursor unchanged for later admission.

The trusted worker freezes project inode, configuration path, full routine fingerprint,
previous cursor and occurrence time. Under project and inherited transfer ownership
it rechecks current enablement, approval, definition, due time and cursor. An atomic,
fsynced ticker-state claim advances the cursor before concrete namespace supervision.
A lost claim is never replayed: recovery reports uncertainty. Saved results deliver
idempotently, including handled inbox items; equal output keeps existing deduplication.
Completion output from an injectable Runner performs no durable certification.

Independent review found unbounded approval/state reads; both now use nonblocking
regular-file reads with byte limits and metadata stability checks. Routine discovery
and definitions are bounded too. Writers refuse to exceed their reader limits.
Review approved the fixes and independently passed all six worker fixtures covering
stale authority/cursors, cancellation, concurrent unrelated status, FIFO/oversize
refusal, uncertain claims, and delivery across restart. A built-binary ticker fixture
also proves one execution followed by persisted result delivery after restart.

Supervision filters inherited environment variables and normalizes nonzero exits;
these compatibility changes are documented in operations. Same-user script edits
remain outside command-text approval, legacy commands still require a reachable
session, and non-Linux execution remains untested. Idle final-copy/auto-resolution,
remaining terminal effects and canonical launch/profile/budget/authority work remain.
Counts stay 16 locally implemented, five partial and 20 not started.

All 538 tests pass in debug and release (165 library, 338 binary, 33 CLI, two
contracts). Default-feature tests pass (29 library, 289 binary, 14 CLI, two
contracts); three optional live checks remain ignored. Logs:
`/tmp/herdr-legacy-routines-{debug,release,default}.log`.


## W04 controlled preservation ingress for final-copy jobs

The immutable artifact receiver now has a controlled ingress for queued final-copy
workers. The original monotonic deadline and cancellation token carry through
payload extraction, hashing, staged verification, existing-snapshot verification,
final authorization and publication. Archive files must be regular, single-link
files with bounded size; payload reads count actual bytes and verify each digest.
Legacy remote receive uses the same hardened extraction path. Existing public
capture/load interfaces retain their compatibility wrappers.

Caller-owned spool files remain untouched by the new ingress. Cancellation or
withdrawn authority before publication removes the temporary stage and preserves
existing evidence. A cancellation observed after the atomic publication can leave
immutable unreferenced evidence; it does not certify a thread result or resolution.
The future worker still must establish supervised sender success, bind execution
and manifest identity, and revalidate before committing finalization.

Independent review approved the receiver and independently passed all seven wire
fixtures, including three new cases for caller ownership, cancellation at extraction
and publication boundaries, authority withdrawal, special/oversized/corrupt archives
and retained-evidence integrity. Automatic idle/merged-PR finalization remains to be
moved into the queue; this prerequisite does not complete T04.2. Card counts remain
16 locally implemented, five partial and 20 not started. External remote-host and
macOS acceptance remain untested.

All 541 tests pass in debug and release (165 library, 341 binary, 33 CLI, two
contracts). Default-feature tests pass (29 library, 292 binary, 14 CLI, two
contracts); three optional live checks remain ignored. Logs:
`/tmp/herdr-preservation-control-{debug,release,default}.log`.


## W04 complete live-stage preservation for final-copy workers

A received native live stage can now retain its complete bytes as an immutable
artifact snapshot without fetching the mutable source again. The bridge keeps the
original live stage owned by its projection caller, copies through anchored reads,
compares the copied inventory with the received manifest, and uses controlled
publication with a final authority check. Omitted entries return no preservation
receipt. The existing combined 50 MiB and 10,000-entry preservation limits remain
stricter than independent live report/library limits.

The snapshot records the caller-bound thread, generation, source and machine; the
caller must first establish successful sender completion and execution/source
identity. This does not itself change a thread or authorize resolution. Complete
stages without a report may be preserved. Automatic finalization therefore needs
an explicit report-optional intent; the existing report-required live-copy intent
cannot represent all legacy final-copy outcomes.

All 11 focused live-artifact tests pass, including four new cases for source loss,
binary and empty-directory retention, partial versus report-absent copies, changed
staged bytes, wrong-project use, cancellation/authority withdrawal, and an actual
51 MiB live stage that exceeds preservation's combined limit. Independent review
approved the bridge and reran its authority/cancellation fault regression. Log:
`/tmp/herdr-live-preservation-focused.log`. The full suites passed at the preceding
controlled-receiver commit; they were not rerun for this not-yet-admitted worker API.
Automatic finalizer integration remains open; counts remain 16 locally implemented,
five partial and 20 not started. macOS and actual remote-host acceptance remain
untested.


## W04 durable report-optional final-copy projection

Final-copy projection now has its own durable intent, retained stage and historical
notice. It accepts missing reports, preserving the prior home report and receipt,
and distinguishes partial copies from complete immutable snapshots. Recovery
verifies the retained manifest, staged bytes, preservation identity and current
authority before additive library-first/report-last publication. Lifecycle changes
and migration refuse pending final projection; thread record reads and writes are
bounded at 16 MiB.

Copy completion and the resolution decision commit together. Idle finalization
rechecks settings and observed timestamps, and a changed incoming report resets
the idle clock instead of resolving. Merged finalization requires the published
report's first-line PR URL to retain the expected merged PR. Lost eligibility
finishes the copy and leaves the thread open. Notices persist before delivery,
reuse the same inbox identity after crashes, and do not mutate a replacement
execution.

Independent review approved all eight focused finalization fixtures after finding
and fixing the idle changed-report race. Coverage includes interruption/recovery,
missing and partial reports, stale eligibility, merged report identity, authority
and cancellation failures, corrupt preservation evidence, lifecycle exclusion and
historical notice replay. Automatic worker ingress and admission are still absent;
these APIs do not yet replace the ticker's synchronous finalizer. Counts remain
16 locally implemented, five partial and 20 not started. macOS and actual remote
host acceptance remain untested.

All 554 tests pass in debug and release (166 library, 353 binary, 33 CLI, two
contracts). Default-feature tests pass (29 library, 304 binary, 14 CLI, two
contracts); three optional live checks remain ignored. Logs:
`/tmp/herdr-final-intent-{debug,release}-corrected.log` and
`/tmp/herdr-final-intent-default.log`.


## W04 supervised final-copy worker ingress

The trusted copy worker now accepts final-copy requests with a frozen purpose,
operation, counter and optional retained intent. New work requires successful
native sender completion, then preserves complete staged bytes, records intent,
and performs recoverable projection/resolution. Recovery checks exact authority
and retained identity before publication and does not start a sender. Complete,
report-absent and partial outcomes retain the previously reviewed semantics.
The request and completion envelope cannot substitute for a durable receipt.

Independent review found that project ownership freezes ticker observations but
cannot prevent an agent independently resuming work. Idle requests now bind the
recorded session into their authority and use supervised Herdr agent/pane reads
before resolution. Foreign or duplicate pane/agent identities and unknown agent
states also refuse idle resolution; true absence retains the legacy policy only
when it matches the recorded state. A changed live state or failed observation prevents resolution;
retained copy recovery can finish while leaving the thread open. Configuration,
routing, session or cancellation failure still withdraws effect permission.
Supervision accepts an explicitly supplied HERDR_SOCKET_PATH, but never inherits
it or HERDR_SESSION from the ambient environment. Reads remain bounded by the
original worker deadline and command limits. This is a fresh observation check,
not a claim that an external agent is locked against subsequent activity.

The reviewer also identified maximum-counter recovery: an existing intent at
i64::MAX now remains recoverable, while creation beyond the limit refuses.
Fixtures cover these boundaries, live state changing during transport without
record changes, source/helper loss after intent, configuration withdrawal, failed
senders, cancellation, stale counters, partial/missing reports and merged PR
identity through supervised remote-helper fixtures. Actual remote-host and macOS
acceptance remain untested. Automatic ticker admission is still to be wired; card
counts remain 16 locally implemented, five partial and 20 not started.

Independent review approved the final worker increment and independently passed
the foreign/ambiguous observation regression. This approval covers worker ingress
and recovery, not automatic admission or unavailable live-platform acceptance.

All 561 tests pass in debug and release (167 library, 359 binary, 33 CLI, two
contracts). Default-feature tests pass (30 library, 310 binary, 14 CLI, two
contracts); three optional live checks remain ignored. Logs:
`/tmp/herdr-final-worker-{debug,release}-final.log` and
`/tmp/herdr-final-worker-default.log`.


## W04 automatic final-copy admission

Production Linux ticker passes now offer idle and merged-PR final copies to the
existing bounded background queue. Final copies share project/thread rotation,
capacity, cooldowns and canonical-routine alternation with live copies and legacy
routines. Queue completions remain bookkeeping only; the supervised worker owns
copy receipts, preservation evidence, resolution and its durable notice. Fresh
remote final requests resolve routing inside supervised ingress and freeze that
authority before publication.

Merged retry records remain durable until a later ticker pass observes completion
or invalidation. Retained final projections are discovered before session checks
and can recover without the original source/helper; notice delivery also precedes
session reachability checks. New review readiness includes outstanding final-copy
offers, and pending final projection prevents competing source-copy work.
Independent review caught automatic prompts/starts running before recovery; both
now skip pending live or final projections while status observation continues.

The built-binary CLI fixture passed: actual ticker admission and native transport
produce a merged resolution, immutable snapshot and copy receipt, then restart
drains the retry record and delivers one final notice without another sequence.
Focused tests also cover idle offer-only behavior, forged queue success, durable
merged retry reconciliation, source/session-independent retained recovery, and
retained merged intent excluding brief prompts and agent starts. Independent review
approved the integration and independently passed recovery and terminal-exclusion
fixtures.

Remaining terminal-effect isolation, abandoned staging inventory cleanup, canonical
launch/profile integration, telemetry/usage and broader authority coverage remain
open in W04. This does not close T04.2 or external platform acceptance. Counts
remain 16 locally implemented, five partial and 20 not started; macOS and actual
remote-host acceptance remain untested.

All 566 tests pass in debug and release (167 library, 363 binary, 34 CLI, two
contracts). Default-feature tests pass (30 library, 314 binary, 15 CLI, two
contracts); three optional live checks remain ignored. Logs:
`/tmp/herdr-final-admission-{debug,release,default}.log`.


## W04 guarded reclamation of abandoned copy staging

New copy-worker ingress can now reclaim recognized unreferenced directories when
the 16-entry live staging inventory cannot admit another spool and extraction.
The worker first owns the project and inherited transfer locks, then holds the
record lock while scanning all thread TOMLs. The scan bounds total bytes, per-file
size and inventory entries, checks record identity, rejects corrupt or unknown
fields, and retains every live/final projection reference before any deletion.
Unknown names and non-directory entries remain untouched; immutable preservation
snapshots are outside this cleanup namespace. Retained recovery bypasses cleanup.

Deletion uses anchored directory descriptors, refuses links, special/hardlinked
entries and device crossings, checks identity before unlink, and syncs parent
directories. Cancellation and deadlines can leave only a partially removed
unreferenced staging tree. Cleanup budgets account for the private stage root,
report and manifest in addition to the existing maximum library; source read
entry/depth limits remain unchanged. Each tree has a cooperative 10-second budget
bounded by the original worker deadline. This does not promise interruption of
a stalled filesystem or hostile same-user containment.

Independent review approved the implementation and independently passed all four
reclamation fixtures plus the corrected maximum-depth fixture. Tests retain both
pending intent types and unknown names, refuse corruption/future fields, preserve
external symlink targets, resume after cancellation following a durable unlink,
delete an actual maximum-entry/depth layout, and recover a worker from full orphan
inventory only after project ownership becomes available.

Remaining W04 work includes terminal-effect isolation, canonical launch/profile
integration, usage/telemetry and broader authority coverage. Counts remain
16 locally implemented, five partial and 20 not started. macOS and actual remote
host acceptance remain untested.

All 574 tests pass in debug and release (167 library, 371 binary, 34 CLI, two
contracts). Default-feature tests pass (30 library, 322 binary, 15 CLI, two
contracts); three optional live checks remain ignored. Logs:
`/tmp/herdr-reclamation-{debug,release,default}.log`.


## W04 durable brief-delivery claims

Thread records now retain a bounded prompt, execution fingerprint and monotonic
claim sequence before a future supervised prompt effect. Exact confirmation
atomically clears `prompt_pending`; stale or cancelled acknowledgements leave the
claim recoverable. Pending claims and retained copy projections are mutually
exclusive. No automatic claim producer or concrete terminal sender is enabled by
this increment.

Recovery owns the project guard, refuses thread-inventory diagnostics, and runs
before session checks. A lost response becomes uncertainty, fails only the same
open execution, and delivers an idempotent notice that recognizes handled inbox
items. A replacement execution remains open and may claim after historical notice
delivery. Same-execution replay remains forbidden. Migration rejects unresolved
claims and validates completed claim history.

Read-only inspection of the installed Herdr 0.9.1 bundled schema (protocol 22)
found no prompt idempotency or revision-precondition parameter. This recovery
policy therefore does not infer that a missing response means a prompt was not
submitted. Independent review approved the claim/recovery substrate and final
copy-exclusion and inventory safeguards; concrete sending and admission remain
outside that approval.

Counts remain 16 locally implemented, five partial and 20 not started. Remaining
W04 work includes supervised terminal effects, canonical launch/profile
integration, usage/telemetry and broader authority coverage. macOS and actual
remote-host acceptance remain untested.

All 580 tests pass in debug and release (168 library, 376 binary, 34 CLI, two
contracts). Default-feature tests pass (30 library, 327 binary, 15 CLI, two
contracts); three optional live checks remain ignored. Logs:
`/tmp/herdr-brief-claims-{debug,release,default}.log`.


## W04 supervised local brief delivery

Linux local thread briefs now enter the shared bounded queue during the cheap
pass and run outside its root-exclusive lease. The concrete worker owns a project
guard and surviving supervisor locks. It freezes project/socket identity,
configuration and execution sequence, checks fresh unambiguous matching idle
agent/pane observations, durably claims, and requires a typed matching
`agent_prompted` acknowledgement before confirmation. Generic queue completions
cannot write delivery authority. A 45-second original deadline includes queue
wait and preflight; each command retains the existing 10-second API timeout.

A conservative terminal-reference inventory checks legacy coordinators and all
threads, including resolved references and socket aliases. It refuses corrupt or
oversized inventories before claiming. Bounds are 1024 root entries/references,
256 entries per thread directory, 16 MiB per file, 50 MiB aggregate and a cooperative
10-second scan budget bounded by the worker deadline. Canonical/migrating neighbors
are conservatively refused in both feature modes until a bounded canonical
identity reader exists; loading an entire canonical snapshot is not an acceptable
way to satisfy these bounds. The shared root barrier excludes legacy rebinding
and canonical adoption, but does not freeze unverified canonical route edits.

Disposable fixtures cover exact confirmation, lost/malformed/foreign replies,
busy/ambiguous/stale targets, socket replacement, cancellation after claim,
neighbor/coordinator/alias/corrupt/canonical refusal, ticker admission/restart,
and an actual blocked sender while another project's observation progresses.
The built ticker also executes exactly once across confirmed and lost-response
restart scenarios. These tests invoke only disposable helper executables and
sockets, never user panes.

Remote briefs, coordinator prompts/notifications, starts and token effects still
need worker integration. Canonical identity inventory, launch/profile integration,
usage/telemetry and broader authority coverage remain open. Counts remain
16 locally implemented, five partial and 20 not started; macOS and actual remote
host acceptance remain untested.

Independent review approved the final bounded local sender after the canonical
refusal and inventory-before-observation safeguards. Full regression suites passed
588 tests in debug and release (168 library, 383 binary, 35 CLI, two contracts).
After the final two safeguards, all 15 brief-related checks passed again in both
modes, including the built ticker fixture. The full final default-feature suite
passed 382 tests (30 library, 334 binary, 16 CLI, two contracts). Three optional
live checks remain ignored. Logs: `/tmp/herdr-brief-worker-{debug,release,default}.log`
and `/tmp/herdr-brief-worker-{debug,release}-final.log`.


## W04 bounded canonical terminal identity inventory

State-store builds now permit local brief admission beside nonconflicting active
canonical projects. The dedicated reader validates the active migration journal,
format marker, import receipt and control publication, then reads only runtime
bindings and their provenance in one read-only SQLite transaction. It does not
open the full store or materialize unrelated task/event/delivery history. Dangling
ownership or observation references refuse rather than hiding retained resources.
Default builds and incomplete migrations still refuse canonical neighbors.

Identity and publication data share the caller's 50 MiB byte and 1024-reference
budget. Individual files/fields are capped at 16 MiB. SQLite `octet_length` measures
text bytes without first loading the text; a 32 MiB connection limit additionally
bounds string/encoded-row allocation. The existing SQLite 3.53.4 minimum supports
that operation. A progress handler checks cancellation and the original deadline
inside running SQL, with a maximum 10-second inventory budget and 10 ms busy wait.
Main database and existing sidecars must be regular single-link files. These are
cooperative filesystem/process guarantees, not interruption of stalled kernel I/O
or hostile same-user containment.

Focused fixtures cover compatible mixed-root sending, retained canonical
conflicts, byte/reference/field limits, malformed provenance/publication,
cancellation, a long SQL query interrupted before its first row, and deleted
canonical bindings with retained ownership/observation references. A large
unrelated event remains outside the identity materialization budget.

The full plan remains active: remote briefs and other terminal paths, canonical
launch/profile integration, usage/telemetry and broader authority coverage remain.
Counts remain 16 locally implemented, five partial and 20 not started. macOS and
actual remote-host acceptance remain untested.

Independent review approved the bounded reader and independently passed all five
inventory fixtures. Full suites pass: 594 debug and release tests (173 library,
384 binary, 35 CLI, two contracts), and 382 default-feature tests (30 library,
334 binary, 16 CLI, two contracts). Three optional live checks remain ignored.
Logs: `/tmp/herdr-identity-{debug,release,default}.log`.

## W04 frozen saved-machine JSON transport

The remote worker prerequisite now resolves the current saved-machine listing
with exact ID precedence, unique labels, enabled-profile checks and globally
unique IDs. It freezes the literal SSH target and named/default session separately
from label and UI selection. Legacy artifact routing also rejects ambiguous,
disabled and duplicate-ID matches instead of silently falling back.

The concrete supervised bridge sends bounded JSON on stdin, validates matching
response IDs and successful process completion, and shares the original deadline,
cancellation and inherited execution locks. It explicitly disables TTY allocation
and requires strict host-key verification. SSH URI targets remain a single argv
value; passwords, paths and invalid ports refuse. The bridge does not install or
start a remote server and never retries a request.

The contract was checked against Herdr upstream commit
`d59d0603d53bb88c5320ea508a4fb9858b61af68` and installed Herdr 0.9.1/protocol 22.
A disposable, network-isolated named-session server answered a native JSON bridge
ping successfully. This is local protocol evidence, not real SSH-host acceptance.
No user panes or saved profiles were modified.

This increment does not enable remote brief workers. Frozen route admission,
cross-project remote ownership and durable-claim integration remain next, followed
by other terminal effects and the remaining W04–W09 cards. Counts remain 16 locally
implemented, five partial and 20 not started. macOS and real remote-host acceptance
remain untested.

Independent review approved the final prerequisite after duplicate-ID, strict
host-key/non-TTY and rsync URI safeguards. Full debug and release suites passed
600 tests each (173 library, 390 binary, 35 CLI, two contracts) before the final
narrow rsync refusal; the default-feature suite below includes that refusal.
Logs: `/tmp/herdr-route-{debug,release,default}.log`.
The final default-feature suite passed 388 tests (30 library, 340 binary, 16 CLI,
two contracts), including all six routing/transport fixtures. Three optional live
checks remain ignored in every feature/build mode.

## W04 supervised saved-machine briefs

Linux remote observation batches now carry the complete saved route when its
contract is valid. Ready briefs enter the shared bounded queue with that frozen
route and execution/configuration/session identity. Incomplete contracts refuse
without falling back to synchronous prompting. The concrete worker holds the
root/project execution locks, checks bounded ownership inventory, revalidates the
saved selector, probes the installed JSON bridge and reads fresh exact agent/pane
identity before claiming. It rechecks routing before sending and after the typed
acknowledgement. Queue success alone remains insufficient to confirm delivery.

The existing durable claim/recovery path handles remote lost replies as uncertain
and never resends them automatically. SSH payload stays on stdin, with strict
host verification, no TTY, original deadline and cancellation. The remote binary
defaults to `herdr`; `HERDR_PROJECTS_REMOTE_HERDR_BIN` selects an installed path.
It must connect an existing server in the frozen saved session; no bootstrap is
performed. The shared root barrier and inherited locks retain exclusion while the
supervised SSH process lives.

Ownership conservatively refuses a matching pane ID in another record whenever
either side is remote, including local/loopback, distinct aliases, resolved threads
and canonical bindings. Different SSH names or sessions cannot prove different
servers. Unrelated servers with colliding pane IDs can therefore be refused.
Local-only references retain canonical socket comparison. Inventory bounds and
canonical-reader/default-feature limitations are unchanged.

The worker subprocess fixture covers exact/lost/foreign replies, unsupported
bridge, wrong request ID, busy/ambiguous targets, route changes before/after claim,
post-claim cancellation, retained conflicts and cancellation of a blocked SSH
process while a neighboring project can acquire its guard. Ticker fixtures prove
queued admission grants no delivery receipt and missing contracts refuse. A
separate regression retains fresh route/agent/pane preflight for delayed shell
observations that could still trigger synchronous starts. The built ticker uses
complete saved-route discovery, remote binary override, serialized worker ingress
and concrete SSH bridge for confirmed/lost restart scenarios, with exactly one
send and no synchronous prompt/start in either scenario.

Independent source review approved after the remaining-start preflight fix and
independently passed the remote worker variant fixture. The final focused remote
suite passed 36 binary tests plus the built ticker restart test. Full regression
results follow. No real remote host or macOS acceptance is claimed. Coordinator
prompts/notifications, starts, token effects, canonical launch/profile integration,
usage/telemetry and wider authority coverage remain. Counts stay 16 locally
implemented, five partial and 20 not started; the complete plan remains active.

Full debug and release suites passed 604 tests each (173 library, 393 binary,
36 CLI, two contracts), including the complete remote ticker restart fixture.
Logs: `/tmp/herdr-remote-brief-{debug,release,default}.log`; focused evidence is in
`/tmp/herdr-remote-brief-focused.log` and `/tmp/herdr-remote-brief-remote-tests.log`.
The final default-feature suite passed 392 tests (30 library, 343 binary, 17 CLI,
two contracts). Three optional live checks remain ignored in every build mode.

## W04 supervised legacy thread launches

Linux thread starts now share the bounded terminal queue with briefs, for local
and saved-machine sessions. Admission only offers work. Concrete worker ingress
owns root/project locks, freezes execution/configuration/routing, validates bounded
cross-project ownership, requires no agent of any name in the target pane and
checks exact pane plus native terminal identity. The same configuration bytes are
hashed against admission and parsed for kind-bound arguments, preventing an
A→B→A configuration swap from authorizing different arguments.

A durable launch claim records sequence, lifecycle generation, execution digest,
argument and route digests and terminal ID before any start request. Confirmation
requires a successful bounded process and matching request ID, `agent_started`
type and exact terminal/agent identity. Both local and remote starts use the raw
JSON bridge; the CLI start/wait wrapper would confuse a subsequent trust dialog
with failed submission. The native implementation was checked at upstream commit
`d59d0603d53bb88c5320ea508a4fb9858b61af68`, `src/app/api/agents.rs` and
`src/app/agents.rs`, against the installed protocol-22 schema. An acknowledgement
records submission, not readiness; later fresh observations govern the brief.

Pending starts exclude briefs, new copies and restart. Missing acknowledgement
recovers to uncertainty and one inbox notice, failing only the claimed generation.
No same-generation retry occurs, including after PR metadata changes. Explicit
restart advances generation and is still refused while an agent is observed.
Historical recovery cannot fail the replacement. Migration refuses pending or
unresolved launch claims and accepts confirmed/resolved acknowledged history.
Confirmed starts with no observed agent show Waiting on you and inspection/restart
guidance; this is observation status, not termination evidence. Exhausted legacy
launches retain their prior visible failure transition in both queue modes.

Fixtures cover claim/exclusion/recovery, historical generation handling, argument
redaction, configuration ABA, unsupported bridge, missing terminal ID, foreign/busy
agents, ambiguous panes, wrong terminal/request/type acknowledgements, lost replies,
post-claim cancellation/config changes and cancellation of blocked subprocesses.
The built ticker tests local/remote × confirmed/lost starts across restart, retaining
one start even when the acknowledged agent is launch-pending/blocked. Existing
remote shell observations can no longer bypass queue ingress with direct starts.
Independent review found and guided the configuration-byte binding and legacy/status
fixes, and independently passed four claim and six worker fixtures.

The complete plan remains active. Coordinator starts/prompts/notifications, token
reporting, canonical launch/profile integration, budgets/telemetry and wider
authority coverage remain, followed by W05–W09. Counts remain 16 locally implemented,
five partial and 20 not started. No real SSH-host or macOS acceptance is claimed.

A disposable network-isolated native Herdr 0.9.1 named session confirmed the raw
startup contract using only a fake `claude` executable. Its immediate reply had
`launch_pending=true`, status `unknown`, matching terminal/name/cwd and `argv`, but
omitted detected kind. The final validator permits missing/null kind only during
pending startup, requires any explicit kind to match, and validates the returned
argument tail. Briefs still require later exact kind/readiness observations.
Captured evidence: `/tmp/herdr-launch-contract-abnzidbc/reply.json`; the fixture
server and descendants were stopped. No user session or actual agent was used.
Worker and built-ticker fixtures now cover that real response shape plus foreign
kind/argument refusal. Configuration and command-vector parse errors withhold
contents. The confirmed-at-legacy-cap regression also prevents stale observations
from converting a durable current-generation launch into a failure.

Full suites passed 617 tests in debug and release (174 library, 404 binary, 37 CLI,
two contracts) before the final native-response and confirmed-at-cap corrections.
After those corrections, all 31 launch-related checks passed again in both debug
and release (seven library, 21 binary, three CLI), including complete local/remote
restart scenarios. The final default-feature suite below also includes the final
error-redaction regression. Logs: `/tmp/herdr-launch-{debug,release,default}.log`,
`/tmp/herdr-launch-{debug,release}-final.log`, and `/tmp/herdr-launch-focused.log`.
The final default-feature suite passed 405 tests (30 library, 355 binary, 18 CLI,
two contracts). Three optional live checks remain ignored in every build mode.

## W04 supervised coordinator priming

Linux coordinator priming now enters the shared Control queue from the ticker.
`open` and `open --reprime` leave the prompt pending for a concrete supervised
worker instead of sending synchronously. Admission freezes the project inode,
socket inode, coordinator execution/request, configuration and PROJECT.md digests,
and exact prompt. The worker rechecks that authority, active lifecycle, bounded
root ownership (including same-project threads and canonical neighbors), unique
ready agent/kind, and pane/terminal identity before recording a durable claim.
The JSON bridge must return a correlated typed acknowledgement naming the exact
terminal and agent before confirmation clears the pending flag.

Priming request numbers prevent mutable route/settings edits from authorizing
another send. Lost acknowledgements recover to uncertainty and one durable inbox
notice; explicit `open --reprime` advances the request and preserves old history
until a replacement claim is durable. Pending priming defers inbox nudges and
claimed priming cannot authorize automatic coordinator starts. Migration refuses
unresolved prime claims. Strict coordinator reads and bounded writes preserve
corrupt or oversized records rather than replacing them with defaults. Independent
review found the read/write size mismatch; the corrected writer rejects growth
past 16 MiB before replacing the old bytes, with a near-limit regression.

Ten new worker/ingress fixtures cover confirmed once-only delivery, explicit
reprime, lost/foreign/wrong-kind/wrong-terminal/wrong-request acknowledgements,
busy/duplicate/unsupported observations, settings/config/socket changes,
post-claim cancellation and request changes, reference conflicts, blocked worker
cancellation with neighbor progress, forged queue success, and corrupt/oversized
record preservation. A migration regression distinguishes null/confirmed history
from pending/uncertain claims. The built CLI ticker runs confirmed/lost cases
across restart, checking one send, durable recovery and no synchronous fallback.
The separate review agent approved this scoped increment after the storage fix.

The complete plan remains active: coordinator starts, inbox nudges/notifications,
token workers, canonical launch/profile preparation, budgets/telemetry and wider
authority tests remain before W05–W09. Counts stay 16 locally implemented, five
partial and 20 not started. No macOS or real SSH-host acceptance is claimed.

Full all-feature debug and release suites each passed 630 tests (175 library,
415 binary, 38 CLI and two contracts), with three optional live checks ignored.
Logs: `/tmp/herdr-coordinator-debug.log` and
`/tmp/herdr-coordinator-release.log`.
The default-feature suite passed 416 tests (30 library, 365 binary, 19 CLI,
two contracts), with the same three live checks ignored. Log:
`/tmp/herdr-coordinator-default.log`.

## W04 supervised coordinator starts

Linux `open` now records coordinator placement and the logical request, then queues
startup for the ticker. The start worker shares priming's frozen project/socket/
settings/configuration authority and bounded ownership inventory. It refuses any
agent already in the pane, requires unique matching pane and terminal identity,
and parses kind-bound arguments from the exact bounded configuration bytes whose
digest was admitted. Before sending, it persists a launch claim and increments the
legacy attempt counter; claims record argument/route hashes without raw arguments.

The concrete JSON API bridge must return the matching request, `agent_started`,
terminal, name, workspace/tab/pane/cwd and command argument tail. Detected kind may
be absent/null only during native launch-pending startup. Confirmation leaves
priming pending; later ready observations require the configured kind. A pending
or uncertain start blocks priming and further automatic starts. Recovery emits one
notice without inferring that the external effect did not happen. Migration rejects
pending/uncertain coordinator starts, including historical uncertainty.

Independent review found that plain `open` could increment the request after a
missing-agent observation of a previously claimed start. The corrected ingress
requires explicit `open --reprime` before allocating a replacement request when
current-request start/prime claims or any pending claim exist. A live coordinator
can still be focused without changing its request. Tests assert byte-equivalent
record preservation and continued worker denial for pending, confirmed and
uncertain starts; explicit reprime retains history and queues the replacement.

New fixtures cover native pending/null-kind acknowledgements, once-only starts,
busy/foreign/ambiguous/unsupported targets, missing terminal IDs, wrong response
identity/type/kind/arguments, lost replies, post-claim cancellation/config/request
changes, argument-byte binding and error redaction, conflicting owned panes,
blocked-child cancellation, and actual open ingress. The built CLI ticker exercises
start-to-prime confirmation and lost-start recovery across restarts without a
second start or synchronous fallback. The full plan remains active: inbox
nudges/notifications and token workers are next, followed by canonical launch/
profile integration, budget/telemetry/authority completion and W05–W09. Counts stay
16 locally implemented, five partial and 20 unstarted; macOS and real SSH-host
acceptance remain unavailable.

The separate review agent approved the corrected coordinator-start increment.
All-feature debug and release suites each passed 641 tests (176 library,
424 binary, 39 CLI and two contracts); three optional live checks remain ignored.
Logs: `/tmp/herdr-coordinator-start-{debug,release}.log`. The focused coordinator
run passed 30 checks (two library, 25 binary and three CLI), including the plain
open replay regression and start-to-prime restart cases.
The final default-feature suite passed 426 tests (30 library, 374 binary, 20 CLI,
two contracts), with the same three optional live checks ignored. Log:
`/tmp/herdr-coordinator-start-default.log`.
