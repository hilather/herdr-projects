# Task status against the 41-card implementation plan

Updated 2026-09-19. Counts describe implementation progress, not release acceptance.
Local tests do not replace the plan's independent review, macOS or live-system gates.

**34 cards remain open: 5 partially implemented and 29 not started.** Seven cards
have their implementation and local regression evidence in place.

| Wave | Scope | Implemented locally | Partial | Not started |
| --- | --- | ---: | ---: | ---: |
| W00 | Baseline and architecture contracts | 1 | 1 | 0 |
| W01 | Process, schedule, polling and recovery reliability | 5 | 0 | 0 |
| W02 | Preservation, transport, lifecycle, diagnostics and context | 1 | 4 | 0 |
| W03 | Transactional store, migration, outbox and reconciliation | 0 | 0 | 4 |
| W04 | Scheduling, capacity, execution pools, profiles and authority | 0 | 0 | 5 |
| W05 | Versioned memory, snapshots, import and coordinator checkpoints | 0 | 0 | 4 |
| W06 | Memory proposals, promotion, updates, invalidation and barriers | 0 | 0 | 5 |
| W07 | Revision-bound results, integration and review gates | 0 | 0 | 4 |
| W08 | CI, live compatibility, failure testing and performance | 0 | 0 | 4 |
| W09 | Pilot, packaging and release acceptance | 0 | 0 | 3 |
| **Total** | | **7** | **5** | **29** |

Implemented locally: **T00.1, T01.1–T01.5, T02.5**. See
[implementation progress](implementation-progress.md) for evidence and limits.

## Next work, in order

1. **T02.1 — Finish verified preservation.** Add remote snapshots and independent
   verification, establish writer shutdown/exclusion, and finish cleanup eligibility.
   Local snapshots exist, but `--remove-worktree` currently refuses all removal.
2. **T02.2 — Finish the transport contract.** Complete bounded streaming and remote
   preservation integration, handle unsupported capabilities without perpetual
   finalization retries, and broaden compatibility evidence. Literal-path transfers
   and real-shell fixtures are already implemented.
3. **T02.3 — Finish removal/reopen recovery.** Persist removal identity, validate the
   retained Git branch/worktree, and distinguish intentional removal from incomplete
   creation. Inactive-project launch guards and conflicting-pane checks already exist.
4. **T02.4 — Finish corruption handling and acceptance.** Complete the structured
   inspection/repair handoff and remaining popup acceptance evidence. Thread/inbox
   diagnostics, preservation of broken files and isolated single-use popups exist.
   Counted conservatively as partial rather than declaring the whole card accepted.
5. **T00.2 — Finish Phase B contracts.** Define shared domain/store interfaces and
   select/review SQLite integration before implementing W03. The Phase A ADR exists.

Then complete the Phase A acceptance gate and proceed to **T03.1**, the transactional
project store. The remaining waves are substantial features, not 29 small fixes;
task count is not a time estimate. No database or memory-authority migration has begun.
