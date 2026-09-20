# W03 handoff: transactional runtime and durable operations

Scope: opt-in `state-store` implementation, schema v9, local Linux evidence.
The independent review of `ff28844` found no production-code blocker in supported
paths and approved bounded acceptance after reviewing the handoff items below. This is
not release certification or a claim that every proposed runtime adapter exists.

## Accepted implementation scope

- T03.1: transactional task/attempt/operation/event store, revision checks,
  versioned upgrades, integrity constraints and crash/contention fixtures.
- T03.2: inspect/apply/recover/restore/export, verified retained source backups,
  explicit authority markers and task/runtime/inbox projections. The coordinator
  distinguishes SQLite runtime ownership from still-authoritative legacy memory.
- T03.3: atomic durable intents, claims, expiry, outcome fencing and bounded retries;
  canonical notification/local artifact adapters, verified receipts and ticker
  dispatch. At-least-once semantics and ambiguous effects are explicit.
- T03.4: bounded live observations, identity-preserving adoption/rebinding,
  retained worker reservations, audited relinquishment, repeatable recovery plans,
  session replacement checks, remote-outage classification and receipt recovery.

Unknown/newer formats refuse writes. Interrupted marker publication requires
forward recovery. No live user project was migrated to validate this handoff.
Memory Markdown remains its sole authority until T05.3. Defaults remain legacy.

## Handoff evidence

The combined controller tree `ff28844` passed 348 all-feature debug and release
tests, 227 default-feature tests and a release build. Independent review approved
all implementation increments, including the final controller lock/reachability
fixes. Linux process-namespace isolation allowed strict cleanup tests without
ignoring inaccessible host processes. Three live fixtures remain opt-in; macOS
remains untested because no host is available.

The final handoff tree passes 349 all-feature debug/release tests (73 library,
250 binary, 24 CLI, 2 contracts), 227 default-feature tests and a release build.
Logs: `/tmp/herdr-w03-handoff-{debug,release,legacy,build}.log`.

The handoff adds a recorded remote-query failure fixture through the collector and
controller: unavailable remains unknown, repeat polling never prompts/launches,
and lost attempts retain capacity. It also updates coordinator ownership guidance,
the explicit at-least-once contract and the task ledger. Remote failure is mocked;
this is not certification of a live remote-agent combination.

## Required carry-forward boundaries

- **W04 launch gate:** canonical worker launching is unavailable. T03.4's literal
  launch-boundary crash criterion is deferred, not passed. Before enabling launches,
  test intent/reservation commit, external launch and result-commit crash points;
  reuse exact matching resources, retain uncertainty and prevent duplicate workers.
- Missing termination evidence retains capacity. Adopted resource ownership never
  grants cleanup permission. No absence/idle observation releases a reservation.
- Imported notification/finalization obligations remain visibly ambiguous until
  exact receipt observation or explicit retirement; they are not blindly replayed.
- Automatic repair, terminal prompting, remote artifact finalization and resource
  cleanup are unsupported in the canonical path. Unsupported commands refuse;
  legacy files cannot be used as a fallback writer.
- W04 owns scheduling/profiles/authority. W07 owns verified results and PR/integration
  gates. W08 owns the broader live/platform/remote fault and performance matrix.

Stop the ticker for migration/restore/upgrade. Normal canonical edits serialize
with effects while ticker leadership remains held. See [reconciliation](reconciliation.md),
[delivery](operation-delivery.md) and [migration](migration-workflow.md) for recovery.
