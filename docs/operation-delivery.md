# Durable operation delivery foundation (partial T03.3)

Implemented 2026-09-19 as a dependency of T03.2's pending-obligation conversion.
The independent reviewer approved the claim/outcome foundation after verifying
entity fencing and crash fixtures. T03.3 remains partial: explicit notification and local finalization adapters exist,
but automatic controller dispatch and production ticker cutover remain. The canonical inbox has an explicit internal drain adapter.

## Persistence and protocol

Schema v3 adds `operation_delivery`, one row per immutable operation intent. A
SQLite trigger creates delivery state atomically with every inserted intent;
upgrading v2 backfills pending rows for its previously undispatched intents.
Explicit `migration PROJECT upgrade-store` upgrades a supported published store;
opening it never upgrades implicitly. The `sqlite-v2` format marker describes the
runtime ownership protocol, not the physical database schema number.

Each delivery has a revision, state, epoch, attempt count, owner, lease expiry,
next due time and last outcome. States are pending, claimed, ambiguous, confirmed
or permanent_failure. Every claim/outcome/expiry appends an audit event in the
same transaction. Project snapshots include delivery state and events from one
read transaction. `operations.json` projections share that snapshot's event head.

Claims compare expected delivery revision, due time, task revision and payload
hash. They allocate a new epoch, increment attempts and grant a 1–300,000 ms lease.
Completion rechecks owner, epoch, delivery revision, lease and task binding inside
its transaction. A changed task cannot receive a stale successful completion.
Expiry records the new fencing epoch and makes the outcome ambiguous, even if the
worker might have crashed before making an external call.

A retryable outcome requires explicit nonempty evidence of no effect. Backoff
starts at one second and is capped below five minutes; after 32 attempts the record
becomes a visible permanent failure. Timeouts, unreachable remotes and lost
acknowledgements are not evidence of no effect. An ambiguous record cannot be
claimed by an ordinary retry: an explicit observation with its expected revision
must first establish confirmed effect, no effect, continued ambiguity or permanent
failure. Old owners cannot commit over this observation or a newer claim.

The store enforces persistence and fencing, not the truth of supplied evidence or
execution policy. Future adapters must check project lifecycle, reconciliation,
authority and exclusive external-resource ownership before effects. Fencing
cannot retract already-sent terminal input. These APIs do not promise exactly-once
execution. No external effect, subprocess, network call or user prompt runs inside
a store transaction. Lease times are caller-supplied Unix milliseconds; clock
rollback can delay expiry, so lease duration is not a monotonic wall-time guarantee.

## Imported legacy obligations

Migration converts supported pending inbox events, finalizations and notification
retries into stable, uniquely keyed immutable intents in the same transaction as
source provenance, tasks, events and the import receipt. Legacy retry attempts,
next-attempt timestamps, blocked flags and diagnostics remain in the payload;
attempts and due times also populate delivery state. Invalid identities, types,
unknown referenced threads or malformed dates block migration.

Every imported intent starts ambiguous. A recorded retry does not prove the
previous attempt had no effect. Reconciliation must observe before deciding what
can be delivered. Project-level obligations use an explicitly blocked synthetic
task; finalizations bind their imported thread task. The raw ticker/inbox bytes
remain immutable provenance and are not rewritten as a second authority.

## Operator surface and evidence

`operations PROJECT inspect` exposes delivery state without dispatching anything.
`task PROJECT list/show/add/rename` and migrated `context PROJECT` use SQLite.
Task creation starts in draft; rename requires task revision and project event
head. Neither enables execution or changes task completion policy. Legacy task
Markdown remains the pre-cutover original, while `migration PROJECT export`
generates a new revisioned view. Recovery preserves accepted post-cutover edits.

Fixtures exercise concurrent claims, stale entity/owner fencing, bounded leases,
retry evidence/backoff/budget, schema-v2 upgrade, atomic import rollback, and
migration of all three obligation kinds. Native child processes die before an
effect, after a disposable file effect, and after result commit. Recovery never
blindly duplicates the effect. Earlier store fixtures cover death around intent
commit, busy and SQLite-full rollback. These are Linux process-failure fixtures,
not live Herdr delivery, power-loss or macOS certification.

Next: complete live ownership integration and runtime execution adapters, then wire reviewed
outbox dispatch/observation adapters and reconciliation. Both T03.2 and T03.3 stay
partial until those behaviors and their integrated acceptance checks exist.

The original foundation passed 265 tests in debug and release. See
[implementation progress](implementation-progress.md) for current validation.
Three live Phase A fixtures stay opt-in.

## Canonical inbox adapter

Schema v4 adds inbox records with revisions and seen/done state. Explicit upgrades
read the immutable source provenance in SQLite, never edited legacy inbox files.
`operations PROJECT drain-inbox --expected-head N` observes pending/ambiguous
`legacy.inbox` intents and commits insertion or deduplication together with their
confirmed receipts in one transaction. Claimed and terminal intents are skipped;
pending records respect their due time. Content conflicts roll back the batch.
Leading whitespace is significant; matching existing items retain seen/done state.
This internal operation performs no terminal, subprocess or network effects.

`inbox list`, `inbox done` and context use canonical records after migration.
Context marks only the displayed unseen IDs, guarded by the snapshot event head;
`--peek` is read-only. Raw legacy files remain preserved. Generated `inbox.json`
shares the snapshot with other projections; schema-qualified directories allow an
explicit schema upgrade to export at an unchanged event head without overwriting
older exports. Regression fixtures cover repeat drains, conflicts, claimed-intent
protection, upgrades from provenance, and the complete CLI inbox workflow.


## Common dispatch service

`operations::dispatch::dispatch_one` connects the durable claim protocol to an
adapter's prepared resource guard. Preparation must validate kind/version, project
lifecycle, reconciliation, authority and target identity without performing an
effect. The prepared value retains exclusive resource ownership until receipt
persistence finishes. Policy is rechecked after claim commit, followed by a store
check of owner, epoch, lease, payload integrity and task revision. This final
SQLite check does not replace external ownership or prevent subsequent external
races; production adapters remain responsible for that exclusion.

Only then does the service call the adapter, outside any SQLite transaction. An
ordinary adapter error is ambiguous, with its possibly sensitive details withheld.
An explicit retryable outcome requires adapter-provided proof of no effect. Policy
withdrawal before calling deliver can safely record no effect. If receipt commit
fails or the lease has expired, `Unrecorded` returns the claim and diagnostic;
callers must not replay. Process death leaves the durable claim for expiry and
observation. `operations PROJECT expire` marks expired claims ambiguous atomically
and is safe during the execution freeze; it never dispatches or clears ambiguity.

This is the common service and adapter contract, not production Herdr dispatch.
Legacy notification hashes alone cannot identify a sent prompt or prove absence.
Finalization still requires revision-bound artifacts and resource ownership.
Concrete notification/finalization adapters and reconciliation remain unfinished;
no migrated scheduler or terminal input is enabled by this service.

Service fixtures cover policy denial/revocation, stale task binding, lease expiry
before effects, delivery outside transactions, timeout ambiguity, and receipt loss.
Native child fixtures die before effect, after a disposable file effect and after
receipt commit. Reopening never blindly replays any of those operations.

## Imported completion receipts

`operations PROJECT receipt-plan` previews receipt evidence without changing store
state. `operations PROJECT observe-imported --expected-head N` atomically confirms
only ambiguous imported operations whose original task revision still matches.
Both use hash-checked database provenance, never subsequently edited legacy files.
The observer requires the deterministic imported operation ID, original revision
and idempotency key; a newly enqueued lookalike cannot reuse historical evidence.

A notification requires its exact imported retry payload and matching `nudged`
hash in the preserved ticker record. A finalization requires its exact pending
payload, matching thread identity/fingerprint, resolved status, completion operation
ID, PR and reason. The event records the source fingerprints. This confirms the
legacy delivery/bookkeeping receipt, not verified task success or present-day
artifact health. Task state is unchanged. Missing, mismatched, claimed or stale
records remain blocked; the observer never infers no effect or makes an external
call. Repeating observation does not duplicate confirmations.

Whole-batch hash verification and receipt writes share one transaction. Fixtures
cover stale heads, task revisions, active claims, mismatches, corrupted provenance,
fingerprint compatibility and newly enqueued lookalikes. This completes a narrow
observation adapter; production sending/copying and general live reconciliation
remain outside its scope.

## Explicit canonical session notifications

`operations PROJECT notify TASK --expected-head H` authorizes and queues one
count-only notification for the current unseen canonical inbox set. It requires
active, reconciled control, a task at that head, valid typed safety configuration,
and a coordinator binding with an explicit local socket. `runtime create` can
register a notification-only route with a socket and no pane. This does not adopt
or prompt a coordinator. Notification content is a fixed title/count; inbox text
is not sent and seen/done state is unchanged.

`operations PROJECT deliver-notification ID --expected-revision R` uses the common
claim/effect/receipt service. The adapter holds the root execution lease through
receipt persistence, validates the exact config bytes it parses, and fences task,
binding, inbox-set and control revisions. Herdr must meet the supported minimum;
its bounded call must fit within the remaining lease. Only `shown=true` confirms
success. Errors and unconfirmed responses become ambiguous; a missing durable
receipt requires observation, never blind replay. These commands do not send
terminal input or automatically retry imported `legacy.notify` obligations.

The inbox-set key remains unique across control epochs. Repeating a confirmed or
ambiguous request cannot send it again. Current limitation: retiring even an
unsent intent prevents authorizing that identical set again; no key bypass is
provided. A retryable no-effect result can retry only while its original config,
control and routing authorization still match. Explicit reauthorization of safely
superseded intents and automatic controller notifications remain future work.

## Canonical artifact finalization

`operations PROJECT finalize BINDING --reason TEXT --expected-head H` queues a
local artifact finalization for a task binding. It pins the canonical recorded
source, current report hash, task/binding/control revisions and config reference.
The operator may request preservation while paused; archived projects, active
execution and all retained attempts refuse. The source must already be recorded
in the runtime binding. Remote finalization remains unimplemented here.

`operations PROJECT deliver-finalization ID --expected-revision R` captures a
bounded immutable snapshot under `.state/canonical-artifacts`, using the existing
manifest/copy verification rules. It rechecks source identity and report hash,
keeps the root execution lease, and publishes a receipt with create-if-absent
semantics under `.state/finalization-receipts`. Legacy mutation guards remain
intact, and legacy files/projection paths are not dual-written.

The confirmed delivery receipt and task revision/state change commit together.
The resulting state is `awaiting_review`: preserved bytes are not verified task
success, writer quiescence, PR approval or permission to release resources.
No attempt reservation, source directory, pane, worktree or branch is removed.

After claim expiry, `operations PROJECT observe-finalization ID
--expected-revision R --expected-head H` verifies the operation-bound receipt and
all retained artifact bytes, including manifest execution identity. It can finish
an ambiguous operation without reading or recopying a now-missing source. Missing,
corrupt or stale evidence leaves the operation unresolved. A crash before receipt
publication remains ambiguous; it does not trigger blind copying. This explicit
adapter does not yet automatically replay imported `legacy.finalize` obligations,
perform remote transfers or bind completion to merged-PR evidence.
