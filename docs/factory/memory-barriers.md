# Frozen barrier release

The state-store build provides an explicit owner-reviewed release workflow.
A release records readiness for one immutable set of member evidence. It does
not create reservations, launch workers, acknowledge memory on their behalf,
or free retained attempt capacity.

1. Write a JSON array of exact `BarrierMember` entries to a file. Each entry
   contains `task_id`, `contract_revision`, `attempt_id`, `result_id`,
   `verification_id`, `integration_id` (null for verify-only routes), and
   `proposal_dispositions` (entries with `proposal_id` and `disposition`).
2. Freeze it with `herdr-projects memory PROJECT barrier-freeze --input members.json
   --expected-head HEAD`. The response identifies the immutable barrier and its
   versioned memory digest. The service validates contract-bound evidence and
   dispositions while retaining the exact read set transactionally.
3. Inspect it with `herdr-projects memory PROJECT barrier --id BARRIER_ID`.
4. Draft the current owner authorization with `herdr-projects memory PROJECT
   barrier-release-draft --id BARRIER_ID --expires-unix-ms EXPIRY_MS`, saving
   stdout to `release.json`. Drafting requires active, reconciled project control
   and currently ready evidence. It publishes no release or event.
5. Review the saved document and sign those exact file bytes externally with
   `ssh-keygen -Y sign -f OWNER_KEY -n barrier-release@herdr-projects release.json`.
6. Submit `herdr-projects memory PROJECT barrier-release release.json release.json.sig
   --expected-head HEAD`, using the head in the signed document.

Freeze and inspection also use scoped database access under a two-second request
deadline. Freeze accounts for the membership document's bytes and JSON structure
before decoding, then shares the input budget with member evidence, proposal
dispositions, the memory read set and publication. The file remains limited to
16 MiB; excessive structure can refuse earlier. Membership is capped at 1,000
with at most 1,000 dispositions per member. Input limits never publish a partial
barrier. Inspection loads only the named barrier and preserves immutable history;
unrelated task history is not decoded as part of either operation.

The [authorization contract](../../contracts/factory/barrier-release-v1.md)
binds the exact barrier, memory digest, project/control identity, current owner
configuration, head, and expiry. The release service verifies consumed memory
object bytes with a 64-MiB ceiling, then rechecks the database evidence in the
final transaction. Draft and release each use one two-second deadline. Release
shares that deadline across signature verification, scoped database reads, object
verification, and atomic publication; denial logging uses the remaining time.
Selected rows, decoded JSON and object bytes share the existing 50-MiB weighted
input/structure budget, which can refuse before the separate object ceiling.
These limits are not a peak-memory or filesystem syscall latency guarantee.
An interrupted publication rolls back the release event, header and signed
receipt together. Existing successful commits remain successful. A changed head requires a new draft and
signature. A changed frozen read set requires a new freeze as well. The owner
key remains outside the service; no automated signing is provided.

The deterministic `release_token` shown by barrier inspection identifies the
frozen revision. It is not owner authorization. CLI release always uses the
signed service and cannot substitute this token for a signature. Existing
low-level store methods are trusted database APIs, not authenticated ingress.

Historical releases remain historical. A new signature cannot retrofit a
release that lacks retained authorization. Legacy pending barriers require a
version-2 freeze. Successful signed release retains the exact request bytes and
their digest atomically, and replay of the identical request returns that same
receipt under matching control identity. The authorization interval is checked
after acquiring the write lock and immediately before committing a new release,
so expiry during readiness or publication rolls back the entire transaction.
Expiry does not erase an already committed receipt or prevent exact historical
replay under matching control identity. The same pre-commit check covers the
earliest expiry of consumed memory, its transitive sources, mandatory records,
and the evidence of prerequisite barriers. Expiry during publication rolls back
the new release without rewriting earlier receipts. Unconsumed optional records
do not block release merely because they expire.

Release history and current applicability are separate. The store can append a
revocation after release without changing the original header, signed request,
or release event. Inspection then returns both `released_seq` and `revoked_seq`;
historical release replay retains that revocation status. A released sequence
alone is not evidence that the barrier is currently usable. Dependency checks
reject revoked memberships until a later, unrevoked release supersedes them.
Revocation preserves attempt capacity and prevents acceptance of a late brief.

Withdraw a barrier explicitly through local operator control:

```sh
herdr-projects memory PROJECT barrier-revoke --id BARRIER_ID \
  --expected-head HEAD --reason "Why this barrier must no longer be used"
```

This follows the existing local approval/delegation revocation boundary. It does
not use a release token or owner signature to grant new authority. A separately
signed/delegated revocation protocol remains unfinished. The reason must be
nonempty, contain no control characters and occupy at most 4,000 bytes. First
publication requires the current event head and records the reason and supplied
head in the immutable `barrier.revoked` event. Repeating the same barrier, original
head and exact reason returns that receipt after restart; changing the request
conflicts. An already automatic or otherwise differently recorded revocation
cannot be relabeled as this operator request—inspect its existing status instead.

The command works through the scoped store and its two-second database budget;
best-effort denial logging shares that original deadline. Event publication,
applicability changes and derived invalidation/stop obligations commit together.
A failed transaction retains the prior barrier status. The existing controller
services durable stop obligations; the command does not itself prove termination
or release retained capacity. Signed release history remains unchanged.

New unresolved blocking memory invalidations now revoke applicable barriers in
the same transaction as the invalidation. A task-scoped invalidation affects
that task's memberships; a global invalidation affects all open memberships.
Informational and already resolved records do not trigger revocation. The
revocation event identifies the invalidation and its triggering sequence.
Resolving an invalidation later does not resurrect a revoked barrier.

Routing uses an indexed projection containing only unrevoked memberships.
Removing a live membership directly is forbidden, and revocation removes it
automatically. Each new invalidation may affect at most 1,000 barriers; exceeding
that bound aborts publication instead of accepting a partial invalidation.
The complete bounded candidate set is materialized before revocation starts.
Migration rebuilds this projection from canonical membership and appends a new,
cause-linked decision for historical barriers with unresolved blocking
invalidations. It preserves the old release event and signed request and never
backdates the new decision. Migration is an administrative full-state pass,
separate from the runtime fan-out bound.
Direct record publication, record-policy application, dependency invalidation,
and memory-policy installation also route to frozen barriers independently of
worker delivery bindings. The routing index derives record IDs, including
transitive sources, from immutable version-2 read sets and retains them until
revocation. Worker retirement therefore does not remove barrier dependencies.
Consumed optional changes revoke the affected barriers; unrelated optional
changes and redelivery of unchanged evidence do not. Required/contract changes
and memory-policy changes revoke all applicable barriers, matching the
conservative global catalog/policy fields in the frozen read set. Each mutation
collects at most 1,000 affected barriers before publishing any revocation;
failure rolls back the source mutation as well.

Legacy barriers without a complete read set use a separate conservative routing
index: any published memory change revokes their applicability. Upgrade retains
their historical evidence and does not fabricate consumed revisions. Expiry
without a write and external object loss still require boundary revalidation;
event-driven routing alone is not proof of current readiness.

For a downstream task, use task-contract document version 2 and copy the
`release_reference` returned by signed release or barrier inspection into its
`required_barrier` field. The reference identifies the exact barrier, release
event sequence and signed authorization digest. Historical unsigned releases
do not expose this reference. Inspection retains the reference after revocation
for auditing; it is usable only while current readiness and authority checks pass.
Sign and install the downstream contract through the normal contract workflow,
then prepare and approve its launch. Its existing contract-digest binding covers
the barrier requirement without altering historical launch payloads.

Preparation, reservation, launch claim and the final pre-effect check revalidate
the referenced release and current frozen evidence. A refused claim or effect
does not mark an attempt terminated or free its capacity. Version-1 contracts
remain available for tasks without a wave prerequisite; selecting required wave
membership is still an explicit planning responsibility.

For version-2 contracts, new result submission checks the barrier before staging
objects and again in the submission transaction. Verification checks it before
loading a target and when committing a new verification run. These checks require
the result's exact contract to match the attempt's retained launch inputs and
recheck the pinned configuration file. An existing receipt can still be read or
replayed as history; replay does not create a new verification result.
Integration rechecks applicability when loading a verified result, creating its
operation and immediately before recording an attempted ref publication.
When the broker observes its candidate at the ref, confirmation rechecks barrier
applicability in the store transaction. If the prerequisite changed during the
external update, the operation becomes `reconciliation_required` and its delivery
becomes ambiguous. An immutable `integration.observed_stale_publication` event
retains the observed commit, candidate, result and ref; no integrated receipt or
dependency satisfaction is issued. The physical Git update remains in place
until reconciliation decides what to do. These checks do not make Git and SQLite
atomic or roll back an external effect.

Dependency publication and use now recheck the producing result's prerequisite
barrier. Release reuse also follows the prerequisites of its member results,
so a later wave cannot hide a revoked ancestor. New freezes, release drafts and
release execution perform the same ancestry checks. Historical release replay
remains historical and cannot override a failed current-applicability check.

An ancestry traversal uses a shared read budget and cancellation/deadline state.
Standalone traversal has a ten-second SQL deadline and the 50 MiB weighted input
budget; controlled callers retain their original budget and progress handler.
Traversal refuses more than 64 distinct release/authority identities or 1,000
member references. Each prerequisite release must precede its consuming release,
which rejects cycles and forward references. These conservative bounds refuse
oversized ancestry; they do not establish constant cost across arbitrarily long
wave histories. Incremental invalidation and active projections remain necessary
for the full scale requirement.

This workflow supplies explicit local ingress. Automated reviewer/barrier
orchestration,
cross-adapter stop certification,
post-effect reconciliation coverage and live F3 certification remain open.
The service tests use genuine migrated stores and real signatures with seeded
result evidence; they do not certify live workers or verifier isolation.

Each reserved downstream attempt now retains an immutable exact release
reference independently of later task contracts. Direct release revocation
records an immutable `attempt.barrier_invalidated` event and readiness blocker
for every live consumer in the same transaction. A failure rolls back the
revocation and all consumer invalidations. Reopening the store preserves these
decisions. They do not establish worker termination or release reserved capacity.

Routing uses an indexed projection of live consumers. Admission refuses a
1,001st live consumer of one release, so urgent revocation can always invalidate
the full admitted population within its 1,000-consumer routing bound. Terminated
consumers leave routing but retain their immutable prerequisite. A result from
such an attempt still has to satisfy its prerequisite; using a later version-1
contract cannot remove that reservation's barrier requirement. Version-1 results
from attempts without a reserved barrier retain their existing behavior.

Freezing also derives immutable transitive ancestor links from each member's
exact reserved prerequisite. An indexed projection contains only applicable
descendants. Revoking a release records a separate invalidation for every
applicable descendant, pending or released, and invalidates their live consumers
in the same transaction. The original signed releases remain historical evidence.
Inspection and readiness expose the decisions after restart. Propagation emits
the complete descendant set without relying on recursive SQLite triggers.

Freeze refuses more than 1,000 ancestors per barrier or a 1,001st applicable
descendant of any ancestor. Revoked descendants leave routing while retaining
their immutable links. These bounds are enforced before admitting work;
revocation never silently truncates the admitted descendant set. An overlapping
memory invalidation batch may record another cause for an already invalidated
descendant, but cannot replace its first applicability decision.

Invalidation also queues a durable stop obligation for each live consumer without
an existing cancellation. Controller polling services at most eight obligations
under a two-second controlled connection and shared read budget, before offering
worker effects. A persistent cursor rotates past failed requests. Publication
through the existing cancellation transaction retires the pending obligation;
failure leaves it available for retry. An existing operator cancellation is
preserved. Observed termination also removes a pending obligation.

Cancellation alone does not prove a worker stopped. The existing service can
release a never-claimed reservation only after its exact binding, ownership,
delivery history and absence of competing retained attempts establish no external
worker. Otherwise capacity remains held, and the canonical worker reconciler
uses the recorded supervisor identity, termination and workspace-quiescence
evidence. It preserves artifacts and retained resources. Unknown identity does
not authorize killing another process or releasing capacity.

An already consumed launch approval does not permanently authorize its initial
prompt. Brief enqueueing, claim and the last pre-submission check revalidate the
exact task contract, prerequisite release and frozen evidence. The canonical
brief adapter passes its original deadline and cancellation identity through a
controlled connection for both claim and pre-submission validation, sharing one
read budget. The selected launch-input, binding, ownership, task and attempt
lookups use their exact identities. Consumed approval validation also selects
only the exact grant, use, revocation flag and delivery, preserving historical
claim validation and current expiry checks under the same budget. Other reads
in this workflow still need the broader history-cost audit.

Initial brief preparation and delivery now use a scoped controlled connection
from selection through rendering and commit. Rendering selects the attempt's
sealed input and retained memory rows, without a full snapshot or a nested raw
store open. Its SQL rows and object bytes share the original read budget; object
reads check cancellation between chunks and verify the complete object digest.
The original deadline also governs the claim, final pre-effect check and receipt
commit. If that transaction cannot commit, the one-use claim remains uncertain;
the deadline does not authorize resending a prompt.

Termination reconciliation also selects only its attempt's launch evidence under
one controlled connection and the original deadline. Started, staged and
worktree-only stop transactions retain exact identity, quiescence and preservation
checks before releasing capacity. Worktree-only preservation runs between two
short database transactions under the retained root barrier: the final write
transaction rechecks head, attempt/task revisions, immutable inputs, creation
identity, ownership absence and historical delivery/approval evidence. A changed
selection or cancelled original control retains capacity and captured artifacts
for reconciliation. Git and filesystem capture hold no SQLite transaction.
Stop paths validate historical approval consumption;
current revocation or expiry cannot require renewed execution permission to stop
that worker. Indexed lifecycle-event and brief-obligation lookups exclude unrelated
history. Cross-project inventories, filesystem preservation and the broader
workflow cost audit still need their full scale measurements.

The ancestry checks above remain necessary for current authority, configuration
and member evidence. Eliminating their history-dependent cost, measuring combined
descendant/consumer fanout, and verifying the combined stop workflow across
supported adapters and live workers remain open.

If an initial prompt acknowledgment is recorded after its required barrier has
been invalidated, the delivery transaction also records `runtime.worker_brief_stale`
with the exact barrier and invalidation event sequences. Confirmation records the
delivery fact; the durable applicability blocker and stop obligation remain.
Failure to persist the stale observation rolls back the receipt and state change.
The one-use prompt claim prevents resubmission. After claim expiry and restart,
recovery requires independently retained, exact acknowledgment evidence; an
unknown external outcome is not evidence of delivery. Replaying a receipt that
was committed before revocation does not manufacture a stale-delivery event.
This covers durable required-barrier invalidation, not every possible generation
change or live adapter's acknowledgment retention.
