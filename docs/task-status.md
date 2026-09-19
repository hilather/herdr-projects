# Task status against the 41-card implementation plan

Updated 2026-09-19. Counts describe implementation progress, not release acceptance.
Local tests do not replace the plan's independent review, macOS or live-system gates.

**29 cards have not started. Twelve cards now have local implementations and
regression evidence.** Phase A acceptance is still open; the counts below do not
claim live SSH, macOS, interactive-popup or independent-review acceptance.

| Wave | Scope | Implemented locally | Partial | Not started |
| --- | --- | ---: | ---: | ---: |
| W00 | Baseline and architecture contracts | 2 | 0 | 0 |
| W01 | Process, schedule, polling and recovery reliability | 5 | 0 | 0 |
| W02 | Preservation, transport, lifecycle, diagnostics and context | 5 | 0 | 0 |
| W03 | Transactional store, migration, outbox and reconciliation | 0 | 0 | 4 |
| W04 | Scheduling, capacity, execution pools, profiles and authority | 0 | 0 | 5 |
| W05 | Versioned memory, snapshots, import and coordinator checkpoints | 0 | 0 | 4 |
| W06 | Memory proposals, promotion, updates, invalidation and barriers | 0 | 0 | 5 |
| W07 | Revision-bound results, integration and review gates | 0 | 0 | 4 |
| W08 | CI, live compatibility, failure testing and performance | 0 | 0 | 4 |
| W09 | Pilot, packaging and release acceptance | 0 | 0 | 3 |
| **Total** | | **12** | **0** | **29** |

Implemented locally: **T00.1–T00.2, T01.1–T01.5, T02.1–T02.5**. See
[implementation progress](implementation-progress.md) and
[Phase B contract ADR](adr/0002-phase-b-contracts.md) for evidence and limits.

## Latest five-item batch

| Task | Local implementation | Remaining acceptance boundary |
| --- | --- | --- |
| T02.1 | Local/remote immutable snapshots; source recheck; Linux cooperative writer checkpoint and non-force cleanup | Remote and non-Linux cleanup refuse; no hostile same-user containment claim |
| T02.2 | Bounded binary file/native snapshot streaming; literal paths; durable unsupported-helper blocking | Live library rsync is a projection, not a hard bounded preservation receipt; live SSH/version matrix remains |
| T02.3 | Durable removal intent, retained branch validation, crash recovery and reopen/restart | Real Git tested with mocked Herdr placement; live end-to-end acceptance remains |
| T02.4 | JSON corruption inspection, hash-checked restore with original backup, popup single-consumer isolation | Interactive Herdr popup acceptance remains; semantic repair is operator supplied |
| T00.2 | Compiled domain/store contract and reviewed SQLite integration with MSRV probe | Independent interface review and macOS packaging remain; no migration started |

## Next work

Finish the **Phase A acceptance gate**: review this batch and exercise disposable
live Herdr/SSH and macOS fixtures. Unsupported cleanup must continue to refuse.
Then begin **T03.1**, the transactional project store, followed by migration,
outbox and reconciliation (the four W03 cards). W04–W09 account for the other
25 not-started cards. These are substantial features, not 29 small fixes; task
count is not a time estimate.
