# Task status against the 41-card implementation plan

Updated 2026-09-19. Counts describe implementation progress, not release acceptance.
Local tests do not replace the plan's independent review, macOS or live-system gates.

**16 cards have local implementations; T04.1–T04.2 are partial and 23 have not started.** W03 has a bounded
[reviewed handoff](w03-acceptance.md). Canonical launch crash certification remains
an explicit W04 prerequisite before enabling launches; it is not claimed as passed.
Phase A was accepted by the user with documented testing gaps.

| Wave | Scope | Implemented locally | Partial | Not started |
| --- | --- | ---: | ---: | ---: |
| W00 | Baseline and architecture contracts | 2 | 0 | 0 |
| W01 | Process, schedule, polling and recovery reliability | 5 | 0 | 0 |
| W02 | Preservation, transport, lifecycle, diagnostics and context | 5 | 0 | 0 |
| W03 | Transactional store, migration, outbox and reconciliation | 4 | 0 | 0 |
| W04 | Scheduling, capacity, execution pools, profiles and authority | 0 | 2 | 3 |
| W05 | Versioned memory, snapshots, import and coordinator checkpoints | 0 | 0 | 4 |
| W06 | Memory proposals, promotion, updates, invalidation and barriers | 0 | 0 | 5 |
| W07 | Revision-bound results, integration and review gates | 0 | 0 | 4 |
| W08 | CI, live compatibility, failure testing and performance | 0 | 0 | 4 |
| W09 | Pilot, packaging and release acceptance | 0 | 0 | 3 |
| **Total** | | **16** | **2** | **23** |

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

W04 T04.1 now has a [schema-v11 scheduling foundation](scheduling.md): DAG
validation, aged priority, retained-attempt capacity reporting and fenced CLI edits.
Atomic reservations and immutable inputs now have a sealed internal API, and cancellation
has an audited CLI. Production launch preparation/dispatch and termination remain
unavailable. T04.2 now uses monotonic remote poll/retry deadlines and pass-start
scheduling plus bounded command queues and asynchronous PR reads; routine/SSH/artifact
isolation remains. Next are bounded command execution, kind-bound worker profiles
(T04.3), budgets/routines/telemetry (T04.4) and operation-scoped authority (T04.5).
The [scheduler/executor/profile interfaces](adr/0004-w04-scheduling-contract.md) are frozen. Existing retained/adopted attempts
must count toward the cap; no uncertain worker may be replaced to free a slot.

The W03 handoff explicitly carries canonical launch crash tests into W04 before
launching is enabled. Missing termination evidence and unsupported imported/remote
adapters remain visibly blocked, rather than becoming implicit launch authority.
W05–W09 then cover memory snapshots and promotion, verified results/integration,
live validation, packaging and release. The 25-card count is not a time estimate.
macOS remains untested; unsupported cleanup continues to refuse.
