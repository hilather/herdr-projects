# Memory store and review workflows

The implementation is under repair against the [independent review](reviews/2026-09-20-memory-audit.md).
See [repair progress](memory-repair-progress.md) for acceptance limits. T04.5 and
T05.1–T06.4 remain partial; production worker launches remain disabled.

Existing projects need an explicit `herdr-projects migration PROJECT upgrade-store`
to use schema 25. Opening a project never implicitly upgrades it. Schema 23 adds
retained snapshot inputs, staged import candidates, signed import decisions and
pending memory delivery obligations. Schema 24 adds immutable per-attempt receipts.
Schema 25 adds immutable native profile verification reports with atomic audit events.
Older snapshot manifests remain readable,
but their missing historical inputs cannot be reconstructed or invented.

## Objects and authority

Objects use SHA-256 identities and bounded, verified regular-file reads. Ingest
and collection serialize filesystem transitions; failed deletion cannot certify
purge. Immutable revisions, accepted proposals/evidence and import candidates retain
their referenced bytes. Release/forget policy remains unfinished.

Markdown remains authoritative until signed cutover changes `format.memory` to
`sqlite-v1`. Afterward it is an export/projection, never fallback authority.
`memory PROJECT inspect` reports records and ownership. Hard-rule, import-ack and
revocation policy still use owner-signed `memory@herdr-projects` documents.

## Manual edits

Initial `memory PROJECT import` creates unverified shadow revisions from the
current plan. It cannot replace an already approved record with changed bytes.
A manual edit uses a separate immutable candidate:

```sh
herdr-projects memory PROJECT preview --file /path/to/project/memory/api.md
herdr-projects memory PROJECT import --file /path/to/project/memory/api.md --expected-revision 1
herdr-projects memory PROJECT candidate --id CANDIDATE_ID
```

An existing record requires its exact expected revision, even for repeated bytes.
Staging leaves the approved head and mandatory rules intact. Candidate preview
returns the original and proposed retained text, independent of later file edits.
New records have no base revision.

Approve or reject with a `MemoryImportDecision` JSON document containing `version=1`,
canonical `project_store`, current owner `authority` reference, `expected_head`,
`candidate_id`, exact `body_hash`, `expected_revision` (or null), and `decision`.
Sign the exact file with the pinned owner key:

```sh
ssh-keygen -Y sign -f OWNER_KEY -n memory-import-review@herdr-projects decision.json
herdr-projects memory PROJECT review-import decision.json decision.json.sig --expected-head HEAD
```

Approval atomically advances the head, records its authorization/receipt and creates
routing obligations. Rejection preserves the head. Existing applicability,
dependencies, record kind and expiry are retained; body approval does not silently
change those fields. Review of a stale base fails. Replaying an identical signed
decision returns the original receipt.

## Snapshots and checkpoints

```sh
herdr-projects memory PROJECT snapshot --task TASK --profile PROFILE --input-file scope.json
herdr-projects memory PROJECT snapshot-input --id SNAPSHOT_ID
```

The snapshot retains exact instructions, task text, structured request, manifest,
profile/configuration identities and budget. Historical rendering reads only those
retained inputs and verified object bytes. Selection counts body sizes, packs
optional records whole and refuses mandatory overflow. Task-local knowledge cannot
be pinned by a different task. Scope eligibility precedes ranking.

An attempt renderer requires that attempt's recorded snapshot and matching
profile identity. Unbound legacy callers receive conservative canonical project
constraints/global facts; they never borrow another task's snapshot or read edited
Markdown projections. Complete production profile sealing and final worker-brief
serialization/accounting remain unfinished.

Coordinator `context PROJECT` requires a named planner profile (see [profiles](profiles.md)).
Checkpoint publication rejects mixed event heads and mismatched session/snapshot
manifests; retry context after a concurrent project mutation.
Context now starts a fresh session by default. The printed header includes a token.
Continue the same known conversation with `context PROJECT --session TOKEN`; acknowledge
with `context PROJECT --session TOKEN --ack CHECKPOINT`. Omit the token after a restart
or compaction uncertainty to force a fresh full context. Acknowledgment without a
session token is rejected. Tokens bind to the project store, canonical coordinator
route/revision, control epoch and configuration. A route or epoch change invalidates
the old session even when the caller reuses its token. Profile, budget or retained
instruction changes also force full context. These are local session handles, not
same-user process authentication; automatic native conversation identity remains
unfinished.

Full context is required until acknowledgment; deltas include current project text,
mandatory rules and blockers. Final output is budget checked; stale acknowledgments
cannot rewind the cursor. Session-generation fencing and a single consistent
runtime/memory read boundary remain unfinished.

## Worker proposals and review

`memory PROJECT propose --input proposal.json` stages untrusted findings. Producer
task, attempt and consumed snapshot must match; observed/dependency revisions must
be in that snapshot. Bodies and evidence are verified and retained. Repository
claims and typed validation IDs are refused until revision-bound validation can
resolve them; arbitrary strings are never treated as verified evidence. General
worker object staging through the CLI is still missing.

Proposal review requires an expiring, one-proposal owner authorization containing:

- `version`, canonical `project_store`, current `authority` and `expected_head`;
- `expires_unix_ms`, exact `proposal_digest` and complete `record_keys`;
- nested `review`: `schema_version`, `proposal_id`, `decision` (`approve` or `reject`), `reason`.

```sh
ssh-keygen -Y sign -f OWNER_KEY -n memory-review@herdr-projects review.json
herdr-projects memory PROJECT review --proposal ID --decision-file review.json --signature review.json.sig --expected-head HEAD
herdr-projects memory PROJECT promote --proposal ID --decision DECISION_ID
herdr-projects memory PROJECT deliveries
```

Unsigned review is not exported through CLI or public memory service APIs.
Promotion consumes the recorded approval and rechecks current authority,
configuration, expiry and reviewed state. Worker promotions cannot modify mandatory
rules. Any intervening event currently requires rereview; precise dependency fences
and delegated reviewer identities remain unfinished.

Memory heads, invalidations and durable pending routing obligations commit together.
Routing selects task subscriptions by consumed records, transitive historical
dependencies, pinned keys and retained scope. Mandatory and global changes remain
broad; foreign task-local records are excluded unless already consumed. Coordinator
subscriptions and legacy snapshots without retained scope remain conservative.
When a task has an active attempt, routing uses only that attempt’s snapshot;
queued tasks without an attempt retain conservative subscriptions. Rows stay
`pending` as immutable obligations. Receipts are tracked separately; an intent is
not evidence of delivery. Automatic transport and completion barriers remain unfinished.

Workers pull a single immutable change at a checkpoint:

```sh
herdr-projects memory PROJECT update --delivery DELIVERY --attempt ATTEMPT
herdr-projects memory PROJECT ack --input acknowledgment.json
herdr-projects memory PROJECT receipts --attempt ATTEMPT
```

The update contains verified body text, revision metadata and a `manifest_hash`
binding the delivery, attempt, starting snapshot, revision, body hash, severity
and triggering sequence. Reading does not acknowledge it. The acknowledgment is:

```json
{
  "schema_version": 1,
  "delivery_id": "DELIVERY",
  "attempt_id": "ATTEMPT",
  "manifest_hash": "HASH_FROM_UPDATE",
  "state": "seen"
}
```

After incorporating the change, explicitly acknowledge `applied` with the same
digest. Applied requires a prior seen receipt and a still-current valid revision.
A replaced, stopped or differently bound attempt is rejected. Duplicate receipts
are idempotent, including after database reopen; an older receipt never covers a
new change. Receipts are worker declarations, not independent proof of application
or validation. They do not resolve invalidations, advance a cursor, or release a
completion barrier. Repacking, deferred/rejected dispositions, authenticated worker
channel integration and multi-change batches remain unfinished.

## Cutover and recovery

```sh
herdr-projects memory PROJECT plan --output plan.json
herdr-projects memory PROJECT import
herdr-projects memory PROJECT cutover --plan plan.json cutover.json cutover.json.sig --expected-head HEAD --writers-stopped
```

The `memory@herdr-projects` cutover signature binds the inventory digest, project,
policy revision, event head and expected legacy owner. Imported bodies, provenance
and backups are verified before policy or ownership changes. The journal remains
`cutover_pending` until projection/control publication finishes. Ordinary mutations
are blocked while recovery is pending.

On interruption, repeat the **same signed cutover command**. The committed policy
is the durable authorization receipt; recovery does not need a newly signed head.
Completed projections are reused, divergent files are preserved/refused, and an
active cutover replays without reinstalling policy.

Before policy commit only, `memory PROJECT abort-cutover --plan plan.json
--writers-stopped` returns to the imported phase without deleting objects, backups
or edits. Once policy has committed or ownership changed, forward recovery is
required. The complete process-kill/publication fault matrix remains an acceptance
gate; local recovery tests do not certify every crash boundary.

## Completion readiness

```sh
herdr-projects memory PROJECT readiness --task TASK
```

This read-only diagnostic reports an event head and memory blockers. An empty list
is necessary but not sufficient for verified completion: it supplies no result,
validation or wave-release evidence.

Task-success mutations check memory readiness within the same transaction, both
before and after the entire mutation batch. Unresolved required invalidations,
unapplied required updates, invalid/expired/unavailable consumed revisions,
changed sources of consumed derived facts, missing snapshot bindings and missing
or invalid mandatory revisions block success. Informational updates do not block.
Clearing an active attempt in the success batch cannot discard its obligations.
A newer starting snapshot or exact applied receipt can cover a required change;
an applied revision remains subject to expiry and validity checks. Acknowledgments
do not resolve invalidations.

This is a task completion guard, not the full T06.5 wave barrier. Participant and
proposal closure, approved next-wave snapshot freezing, signed conflict resolution,
result/evidence validation and verified release remain unfinished. Worker process
termination can still be recorded; it does not imply task success.

## Invalidation and owner reconciliation

Source changes invalidate current derived facts transitively, including dependencies
through historical intermediate revisions. Invalidation and consumer obligations
commit with the source change. Expiry is checked during selection even without a
new event; scheduled expiry notifications remain unfinished. Recompute and review
derived revisions against their new sources before using them again.

```sh
herdr-projects memory PROJECT invalidations --task TASK
herdr-projects memory PROJECT reconcile resolution.json resolution.json.sig --expected-head HEAD
```

Sign the exact document with `memory-reconcile@herdr-projects`:

```json
{
  "version": 1,
  "id": "resolution-1",
  "project_store": "/absolute/project/.state/state.db",
  "authority": {"id": "owner-approval-policy", "revision": 1, "digest": "CURRENT_POLICY_DIGEST"},
  "expected_head": 123,
  "expires_unix_ms": 2000000000000,
  "reason": "Describe the reviewed reconciliation",
  "invalidations": [{"id": "INVALIDATION_ID", "task_id": "TASK", "triggering_seq": 120}]
}
```

This records an owner disposition, not verified test evidence. Each reference must
match an unresolved task invalidation. Replays reuse the receipt; altered contents
under the same ID are rejected. Other invalidations and newer deliveries remain
untouched. Invalid consumed knowledge, corrupt/missing bytes and missing mandatory
rules still prevent resolution. Global invalidations are not covered by this
per-task API. Raw unsigned memory mutation methods are no longer public APIs.

## Sealed worker knowledge inputs

`memory PROJECT snapshot --worker --task TASK --profile NAME --input-file SCOPE`
captures a worker snapshot with room for the complete protocol framing. Snapshot
creation requires readable, bounded UTF-8 `PROJECT.md`; missing instructions no
longer silently become empty text. Launch drafts preview the exact retained brief
and refuse ordinary task snapshots or changed snapshot/profile bindings.

Rendering bounds object reads before hashing and allocation, enforces the remaining
Unicode character budget, and caps aggregate rendered bytes at 64 MiB. A small
character budget cannot cause an oversized body to be read in full. Launch
preparation also verifies consumed evidence objects before reserving capacity.

```sh
herdr-projects memory PROJECT attempt-input --attempt ATTEMPT
```

This requires an actual sealed reservation with a memory reference; a manually
created attempt or snapshot alone does not qualify. The reference uses the
snapshot ID, revision `1`, and manifest hash, and is included in the exact launch
approval scope. Reservations retain it on the attempt. Claims and pre-effect
validation reject changed memory, profile/config mismatches and stale dependencies.
Freshness currently uses a conservative project-wide memory-event fence.

The renderer returns retained instructions, task text and memory within the
captured budget. It verifies consumed source bytes under a bounded aggregate read
budget and checks current execution/configuration again before returning. It does
not read current `PROJECT.md` as a replacement for approved input. Trusted
profile/launch preparation and complete prompt framing now exist; automatic
new-launch dispatch and live workflow acceptance remain outstanding. This command
does not start a worker.
