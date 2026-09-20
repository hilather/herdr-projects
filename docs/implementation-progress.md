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
