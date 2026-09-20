# Implementation progress

Updated 2026-09-19. Base: `6e2bd7607d7bc64cf6155beceb299b44566d7f3b`.
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
