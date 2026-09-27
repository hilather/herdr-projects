# Retained planner inputs

Schema 43 adds immutable input snapshots for planner sessions. These store user
intent and selected canonical event bytes so a restarted proposal producer can
recover the exact input it used. Creating a session does not run a model or
change project execution state.

```sh
herdr-projects plan session PROJECT create SESSION_ID \
  --intent-file intent.txt --expected-head HEAD --expected-plan-revision REVISION \
  --evidence-event EVENT_SEQUENCE
herdr-projects plan session PROJECT show SESSION_ID
```

The intent file must contain nonempty UTF-8 text of at most 64 KiB, without NUL.
Repeat `--evidence-event` for at most 64 strictly increasing retained event
sequences, all at or before `HEAD`; omit it when there is no selected evidence.
Creation requires the current event head and plan revision in one transaction.
The service stores the exact event kind, entity, revision, payload version and
payload string. These are context for deliberation, not a claim that every event
establishes success or permission.

The stored input contains the session ID, canonical project-store path, store
incarnation, parent plan revision, input cursor, exact intent text and its SHA-256
digest, and selected event records. Its serialized bytes are capped at 256 KiB.
`input_digest` is the SHA-256 of those retained bytes. Database work shares a
two-second deadline and bounded input accounting.

Repeating creation with the same ID and exact original arguments returns the
retained session, even after the event head advances. Changed input under that ID
conflicts. `show` reads the retained snapshot, verifies its hash and store identity,
and does not rebuild it from newer evidence. Use a new session ID to reconsider a
new plan revision or input set.

A version-2 unsigned proposal binds to this input:

```json
{
  "version": 2,
  "planner": {
    "session_id": "SESSION_ID",
    "input_digest": "DIGEST_FROM_SESSION_OUTPUT",
    "rationale": "Why these proposed changes follow from the retained input"
  },
  "contracts": [
    {"task_id": "example", "text": "Proposed work", "dependencies": []}
  ]
}
```

Submit through `plan propose PROJECT --input-file proposal.json
--expected-plan-revision REVISION --idempotency-key KEY`. The proposal must match
the retained session digest, store identity and parent revision. The session link,
proposal, accepted plan revision and audit event commit together. Competing
responses based on an old parent conflict; changing the command's parent cannot
silently rebase a session. Replaying accepted bytes and key returns the original
receipt after restart, without creating tasks or another revision.

Proposal acceptance also uses the scoped store's two-second deadline and shared
input accounting. It checks parent/session bindings before graph work, and reads
only queued task identities, dependency edges and selected predecessor existence.
Unrelated retained task bodies are not loaded. Queue and edge inventories have
bounded lookahead at the existing 10,000-task / 100,000-edge limits, with at most
256 dependencies per task; the shared input budget may refuse earlier. The
execution graph and the resulting planned graph must be valid. Proposed edges
do not overwrite the execution queue. An exhausted budget rolls back acceptance.

Accepted revisions retain their effective task/dependency intent in a separate
projection. A new proposal replaces the intended edges of tasks it mentions and
keeps previously accepted intent for other tasks. It can reference a previously
planned task before that task enters the execution queue. Cycle validation checks
the combined intent, so splitting a cycle across multiple proposals does not
evade validation. Superseded proposal bytes remain immutable history; foreground
validation reads only the current projection. Migration 43 backfills the latest
accepted intent for each task from existing proposal history. Projection changes
roll back with a failed proposal or response-link publication.

Inspect current accepted intent without changing execution state:

```sh
herdr-projects plan inspect PROJECT --limit 32
herdr-projects plan inspect PROJECT --limit 32 \
  --expected-plan-revision REVISION --after NEXT_AFTER
```

The first response supplies `plan_revision`, `entries` and `next_after`. Continue
with that revision and cursor until `next_after` is null. A continuation requires
the expected revision; if the plan changed, restart inspection from the first
page. Each request reads one database snapshot. Entries are ordered by task ID
and include proposed text/dependencies, the source proposal ID and digest, and
`source_plan_revision` (which may precede the current plan when intent was kept).
The reader checks selected source hashes and compares projected dependencies with
the retained proposal. This describes accepted proposals, not worker status,
installed contracts or verification evidence.

Pages default to 32 entries and accept a limit from 1 to 64. They also stop at
one MiB of compact entry encodings, returning a cursor for the next unread task;
pretty-printed CLI formatting adds whitespace beyond that entry-byte accounting.
Reads share a two-second deadline/input budget, cache repeated source proposals
within the page, and do not scan superseded history. A budget or selected-record
integrity failure refuses the page rather than returning an unchecked entry.

Version-1 manual proposals remain supported without session binding. All formats
retain intended work; none installs authenticated executable contracts, grants
authority, reserves attempts or executes effects. Complete executable contracts,
decomposition validation, automatic planner inference, lifecycle budgets, signed
dispatch and the live planning pilot remain unfinished.

## Typed changes (version 3)

Version 3 requires the same retained `planner` binding, a `changes` array, and an
`envelope`. Each of at most 64 changes names exactly one task in `contracts`,
which supplies its complete resulting intent. The whole document remains capped
at 256 KiB. Changes are:

- `create_contract`: `task_id`; refuses existing intended or installed contracts.
- `supersede_unstarted`: `task_id`, `expected`; refuses running/completed tasks
  and retained attempts. Current implementation accepts absent, draft or queued
  tasks without an active attempt.
- `add_dependencies`: `task_id`, `expected`, `dependencies`; appends new edges
  without replacing existing edges or changing text. Combined graph checks apply.
- `request_cancellation`: `task_id`, `expected`, `reason`; preserves text and
  dependencies and sets the resulting contract's `cancellation_reason` to the
  same nonempty reason (at most 4,000 UTF-8 bytes, no control characters).

`expected` contains the current intent's `proposal_id`, `plan_revision` (the
inspection entry's `source_plan_revision`), and `payload_digest`. Stale references
conflict. Cancellation is a request for review: acceptance does not stop workers,
release capacity, or remove prerequisite edges. Inspection returns the reason.

The envelope contains `schema_version: 1`, `project_id`, `store_incarnation`,
`request_id`, `idempotency_key`, `actor_id`, `delegation: null`,
`expected_plan_revision`, and `payload_digest`. Project ID is lowercase SHA-256
of UTF-8 `project-store`, one NUL byte, and the canonical `project_store` path in
the session input. Incarnation comes from that input. Actor ID equals the retained
session ID; it is context identity, not authenticated dispatch authority. Non-null
delegation is currently refused. Request ID is immutable and cannot be reused by
another proposal. The key and revision must match submission arguments.

Payload digest is lowercase SHA-256 of compact JSON containing exactly
`contracts`, `planner`, and `changes`, with object keys recursively sorted and
array order retained. Omit `cancellation_reason` when absent. The retained raw
file also has its own digest, returned in the acceptance receipt: exact retry
requires those original bytes and key, including whitespace. The envelope is not
part of the payload digest; it is checked against current store/session identity
and retained request bindings. Request publication, intent projection and planner
binding commit or roll back with the proposal.

