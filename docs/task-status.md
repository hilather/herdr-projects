# Task status against the 41-card implementation plan

Updated 2026-09-19. Counts describe implementation progress, not release acceptance.
Local tests do not replace the plan's independent review, macOS or live-system gates.

**28 cards have not started. Thirteen cards now have local implementations and
regression evidence.** Phase A was explicitly accepted by the user with documented
testing gaps. W03 has begun with T03.1; full W03 acceptance remains open.

| Wave | Scope | Implemented locally | Partial | Not started |
| --- | --- | ---: | ---: | ---: |
| W00 | Baseline and architecture contracts | 2 | 0 | 0 |
| W01 | Process, schedule, polling and recovery reliability | 5 | 0 | 0 |
| W02 | Preservation, transport, lifecycle, diagnostics and context | 5 | 0 | 0 |
| W03 | Transactional store, migration, outbox and reconciliation | 1 | 0 | 3 |
| W04 | Scheduling, capacity, execution pools, profiles and authority | 0 | 0 | 5 |
| W05 | Versioned memory, snapshots, import and coordinator checkpoints | 0 | 0 | 4 |
| W06 | Memory proposals, promotion, updates, invalidation and barriers | 0 | 0 | 5 |
| W07 | Revision-bound results, integration and review gates | 0 | 0 | 4 |
| W08 | CI, live compatibility, failure testing and performance | 0 | 0 | 4 |
| W09 | Pilot, packaging and release acceptance | 0 | 0 | 3 |
| **Total** | | **13** | **0** | **28** |

Implemented locally: **T00.1–T00.2, T01.1–T01.5, T02.1–T02.5, T03.1**. See
[implementation progress](implementation-progress.md) and
[Phase B contract ADR](adr/0002-phase-b-contracts.md) for evidence and limits.

## Latest five-item batch

| Task | Local implementation | Remaining acceptance boundary |
| --- | --- | --- |
| T02.1 | Local/remote immutable snapshots; source recheck; Linux cooperative writer checkpoint and non-force cleanup | Remote and non-Linux cleanup refuse; no hostile same-user containment claim |
| T02.2 | Bounded binary file/native snapshot streaming; literal paths; durable unsupported-helper blocking | Live library rsync is a projection, not a hard bounded preservation receipt; Linux SSH exporter tested; broader version matrix remains |
| T02.3 | Durable removal intent, retained branch validation, crash recovery and reopen/restart | Linux live Herdr/Git and namespace cleanup tested; broader platform coverage remains |
| T02.4 | JSON corruption inspection, hash-checked restore with original backup, popup single-consumer isolation | Linux PTY popup actions tested; semantic repair is operator supplied |
| T00.2 | Compiled domain/store contract and reviewed SQLite integration with MSRV probe | Independent store/interface review passed; macOS packaging remains untested |

## W03 started

T03.1 adds an opt-in transactional SQLite store with typed records, revision/head
checks, atomic domain/event/intent writes and crash/contention/capacity fixtures.
See [schema v1 and ownership](adr/0003-project-store-v1.md). Legacy commands remain
file-backed; no projects have been migrated. Full W03/platform acceptance is open.

## Next work

**T03.2** explicit legacy migration and projections comes next, then **T03.3**
durable operation claims/outbox delivery, then **T03.4** reconciliation.
W04–W09 account for the other 25 not-started cards. Task count is not a time estimate.
Phase A was accepted with its [documented gaps](phase-a-acceptance.md); macOS remains
untested and unsupported cleanup must continue to refuse.
