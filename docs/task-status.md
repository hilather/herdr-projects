# Task status against the 41-card implementation plan

Updated 2026-09-20. Counts describe implementation progress, not release acceptance.
Local tests do not replace the plan's independent review, macOS or live-system gates.

**16 cards have local implementations; T04.1–T04.5 are partial and 20 have not started.** W03 has a bounded
[reviewed handoff](w03-acceptance.md). Canonical launch crash certification remains
an explicit W04 prerequisite before enabling launches; it is not claimed as passed.
Phase A was accepted by the user with documented testing gaps.

| Wave | Scope | Implemented locally | Partial | Not started |
| --- | --- | ---: | ---: | ---: |
| W00 | Baseline and architecture contracts | 2 | 0 | 0 |
| W01 | Process, schedule, polling and recovery reliability | 5 | 0 | 0 |
| W02 | Preservation, transport, lifecycle, diagnostics and context | 5 | 0 | 0 |
| W03 | Transactional store, migration, outbox and reconciliation | 4 | 0 | 0 |
| W04 | Scheduling, capacity, execution pools, profiles and authority | 0 | 5 | 0 |
| W05 | Versioned memory, snapshots, import and coordinator checkpoints | 0 | 0 | 4 |
| W06 | Memory proposals, promotion, updates, invalidation and barriers | 0 | 0 | 5 |
| W07 | Revision-bound results, integration and review gates | 0 | 0 | 4 |
| W08 | CI, live compatibility, failure testing and performance | 0 | 0 | 4 |
| W09 | Pilot, packaging and release acceptance | 0 | 0 | 3 |
| **Total** | | **16** | **5** | **20** |

Implemented locally: **T00.1–T00.2, T01.1–T01.5, T02.1–T02.5, T03.1–T03.4**. See
[implementation progress](implementation-progress.md) and
[Phase B contract ADR](adr/0002-phase-b-contracts.md) for evidence and limits.

## Phase A completed batch

| Task | Local implementation | Remaining acceptance boundary |
| --- | --- | --- |
| T02.1 | Local/remote immutable snapshots; source recheck; Linux cooperative writer checkpoint and non-force cleanup | Remote and non-Linux cleanup refuse; no hostile same-user containment claim |
| T02.2 | Bounded binary file/native snapshot streaming; literal paths; durable unsupported-helper blocking | Live library rsync is a projection, not a hard bounded preservation receipt; Linux SSH exporter tested; broader version matrix remains |
| T02.3 | Durable removal intent, retained branch validation, crash recovery and reopen/restart | Linux live Herdr/Git and namespace cleanup tested; broader platform coverage remains |
| T02.4 | JSON corruption inspection, hash-checked restore with original backup, popup single-consumer isolation | Linux PTY popup actions tested; semantic repair is operator supplied |
| T00.2 | Compiled domain/store contract and reviewed SQLite integration with MSRV probe | Independent store/interface review passed; macOS packaging remains untested |

## W03 handoff

T03.1 adds an opt-in transactional SQLite store with typed records, revision/head
checks, atomic domain/event/intent writes and crash/contention/capacity fixtures.
See [schema v1 and ownership](adr/0003-project-store-v1.md). Legacy commands remain
file-backed; no projects have been migrated. See the bounded W03 handoff; platform/release acceptance remains open.

## Next work

W04 T04.1 now has a [schema-v16 scheduling foundation](scheduling.md): DAG
validation, aged priority, retained-attempt capacity reporting and fenced CLI edits.
Atomic reservations and immutable inputs now have a sealed internal API, and cancellation
has an audited CLI. Production launch preparation/dispatch and termination remain
unavailable. T04.2 now uses monotonic remote poll/retry deadlines and pass-start
scheduling plus a shared bounded executor for asynchronous PR and remote observations;
automatic routines now share that executor. Artifact work and remaining guarded
effects still need isolation. Live transfers now have bounded private staging,
recoverable guarded publication, cancellation/deadline checks through receipt commit,
and pre-transport spool reservation. The supervised copy worker now rechecks
execution/configuration/routing, requires successful native transport, and resumes
retained stages without fetching. Production Linux copy admission now shares
routine fairness after full project passes, with bounded overflow rotation and
review notices deferred until copies finish. Local source hashing now runs in bounded read-only helper jobs with fresh,
execution-bound observations and service-based fairness. Linux legacy commands
now share background admission, with durable pre-effect claims and restart-safe
output delivery. Immutable preservation receive now propagates cancellation and
original deadlines through extraction, verification and publication. Complete
received live stages can become immutable snapshots without fetching again; partial
stages cannot become preservation evidence. Report-optional final-copy intents now
retain stages and atomically commit copy/resolution decisions with replayable notices.
Idle eligibility is checked against the copied report, and merged eligibility
against its PR header. The supervised worker can execute and recover final copies;
idle resolution also requires fresh session-bound agent/pane observations.
Automatic Linux idle/merged-PR admission now shares the bounded background
queue, with retained recovery before session checks and durable notice replay.
Full staging inventories can reclaim unreferenced managed directories under
project ownership, preserving recovery references and unknown entries.
Durable brief claims fence replay after a lost response and recover before
session checks. Linux local briefs now enter the shared bounded queue and use a
concrete supervised sender with fresh target checks and typed confirmation.
A bounded canonical identity reader now permits nonconflicting migrated neighbors
in state-store builds; interrupted migrations and default builds still refuse.
Remote briefs remain open.
Remaining terminal effects still need isolation. T04.3 now fences legacy
launch arguments by explicit agent kind and provides redacted [named profile inspection](profiles.md);
explicit local version probes for Claude/Codex are available. Launch selection,
production profile resolution and verified capability evidence remain. Version-2
reservation inputs now retain and validate frozen profile evidence; historical
version-1 records remain readable. Explicit routine execution now has durable
occurrence, authority and cleanup contracts; automatic ticker dispatch now uses
the shared bounded executor with project ownership and an effect opportunity between runs. Next are bounded command execution, kind-bound worker profiles
(T04.3), budgets/routines/telemetry (T04.4) and operation-scoped authority (T04.5).
T04.5 now has an [approval scope contract](authority.md) binding exact launch inputs
without circular hashes, durable grants/revocations and atomic one-time launch-claim
consumption. Owner-signature import now uses the migration-pinned public-key config;
policy-change ingress, denial audit and other command-path coverage remain.
T04.4 now provides [signed durable admission budgets](budgets.md): lifetime attempt
limits, explicit unknown-provider-usage policy and reservation/claim/pre-effect
checks. Native usage, estimates, running limits and wider telemetry
remain. Budget changes are the first signed policy-change ingress; other policy
classes remain incomplete.
The outbox now supports project-scoped routine records fenced by control revision,
and the shared schedule module computes bounded due windows with explicit DST and
skipped-date handling. Schema 16 adds [signed routine revisions and durable occurrence
recording](routines.md), with atomic cursor/outbox writes and missed/overlap decisions.
Explicit Linux execution now records typed cleanup receipts and output inbox items
atomically, releasing overlap only after verified namespace cleanup. The ticker now
records due occurrences in rotating bounded turns, preserving routine-only liveness
and isolating invalid scripts. Automatic Linux dispatch now validates operation revisions,
absolute deadlines and cancellation at worker entry and commits through the trusted
execution service. Shared root/project ownership now permits unrelated canonical status
and task mutations while excluding same-project work and root-exclusive maintenance.
Legacy status has an observation-only fallback with durable transition notices.
Routine supervision inherits execution locks so abrupt ticker death cannot release
exclusion before namespace cleanup. Admission advances project fairness independently
of ticker cadence and gives exclusive effects a full pass between routine jobs.
The [scheduler/executor/profile interfaces](adr/0004-w04-scheduling-contract.md) are frozen. Existing retained/adopted attempts
must count toward the cap; no uncertain worker may be replaced to free a slot.

The W03 handoff explicitly carries canonical launch crash tests into W04 before
launching is enabled. Missing termination evidence and unsupported imported/remote
adapters remain visibly blocked, rather than becoming implicit launch authority.
W05–W09 then cover memory snapshots and promotion, verified results/integration,
live validation, packaging and release. The 25-card count is not a time estimate.
macOS remains untested; unsupported cleanup continues to refuse.
