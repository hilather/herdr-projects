# Scheduling foundation (partial T04.1)

Schema v10 adds canonical queue records, typed dependency edges and a revisioned
project capacity policy. Schema v11 adds atomic reservations behind an internal
preparation capability and audited cancellation. Worker launches remain disabled.
Schema v12 requires new reservations to retain validated [frozen profile evidence](profiles.md)
in version-2 inputs, while preserving historical version-1 records byte-for-byte.
Schema v13 adds [durable scoped approvals](authority.md) and consumes one grant with
each launch claim; neither a reservation nor a parsed approval is permission to run.
Use `migration PROJECT upgrade-store` for an older published store. Old exports
remain unchanged. Fresh/updated policy starts at zero workers until explicitly set;
legacy advisory limits are not silently promoted into execution authority.

`scheduler PROJECT inspect` reports policy, retained attempts, available slots,
priority order and blocking reasons. Every attempt without recorded termination
counts, including lost, awaiting-input and completed attempts. Lowering the limit
never cancels or releases those attempts. `available_slots` is arithmetic capacity,
not permission to launch; `launch_enabled` remains false in this increment.

```sh
herdr-projects scheduler demo policy --max-active-workers 3 \
  --max-attempts-per-task 3 --expected-revision 1 --expected-head H
herdr-projects task demo queue TASK --input-file queue.json \
  --expected-revision R --expected-head H
```

A queue request is a bounded JSON file:

```json
{"priority": 0, "dependencies": [
  {"predecessor": "api", "requirement": "integration_candidate"}
]}
```

Requirements are `verified_result`, `integration_candidate` or `landed_commit`.
Missing/duplicate/self edges and cycles refuse atomically. Existing live/uncertain
attempts block queue edits. Task state/revision and queue audit commit together;
unchanged requests are idempotent. Queueing preserves legacy TASKS.md and creates
no operation, worker, branch or worktree.

Priorities range from -20 to 20. Each minute waiting adds one to effective priority;
original enqueue sequence breaks ties. Changing queue priority/dependencies cannot
reset enqueue age, so a continuing stream of new high-priority work cannot starve
older work. Graph work is bounded to 10,000 queued tasks and 100,000 edges (256 per
task); unrelated unqueued tasks do not make a store unreadable.

Failed/cancelled predecessors block explicitly. Narrative succeeded state cannot
satisfy an evidence-bound edge. Verified-result producers arrive in W07; profile,
authority and production launch preparation remain W04 work. See the [frozen W04 interfaces](adr/0004-w04-scheduling-contract.md).

## Reservations and cancellation (schema v13; older history retained)

A reservation transaction counts every unterminated attempt, checks task, binding,
control and policy revisions, selects among sealed ready preparations using aged
priority, then commits the attempt, immutable inputs, task pointer and launch intent
together. Inputs pin this store, configuration, profile, approval and repository
identities. Dependency/memory/budget evidence producers are not available yet;
preparations requiring them refuse. There is no CLI constructor or production
producer for the sealed preparation capability. Generic intent enqueue cannot
create `runtime.launch` operations; the controller does not dispatch launches.

```sh
herdr-projects task demo cancel-attempt ATTEMPT \
  --expected-revision R --expected-head H --reason "operator requested stop"
```

Cancellation records desired state and an immutable audit. `released: true` requires
an exact reserved attempt/input/intent binding, no associated worker or other retained
attempt, unchanged task/binding, and a pending intent with zero lifetime claims.
Claim counters cannot move backwards. The transaction retires that intent and marks
the attempt/task cancelled before freeing capacity. Pending after a retry does not
qualify. Other cancellations retain the worker and capacity; this command does not
send a stop signal. There is no termination adapter yet.

| Before | Cancellation result | Capacity |
| --- | --- | --- |
| Reserved, proven never claimed | Attempt/task cancelled; intent permanently retired | Released |
| Claimed, retried, ambiguous or lost | Request audited; task revision fences old launch/result | Retained |
| Adopted attempt without sealed inputs | Request audited | Retained |

Schema upgrades preserve previous exports and claim history. Unsupported preexisting
`runtime.launch` intents without sealed inputs block upgrade before commit; they
cannot be promoted into launch authority. Reservation crash/rollback tests cover DB
boundaries only. External launch crash certification remains a separate gate.

## Elapsed-time polling (partial T04.2)

Remote polling uses a monotonic 60-second deadline scoped to project, socket and
machine. Failed commands start a 120-second retry deadline when they finish.
Fast ticks cannot retry early, and delayed ticks do not require additional ticks
to become due. An overdue resource receives one poll, without a catch-up burst.
The ticker's 15-second interval starts before each pass, so command duration does
not add another full interval. Wall-clock changes do not affect these deadlines.

Slow commands are still synchronous. Bounded queues, separate control/transfer
lanes, cancellation/drain and their load/fault metrics remain T04.2 work.

## Bounded command executor (T04.2 integration in progress)

The native executor has fixed control/transfer worker counts and bounded outstanding
queues. Defaults are two workers per lane, 64 control/32 transfer outstanding
commands, at most two commands per project per lane and one per machine per lane.
Machine/project limits are separate by lane so transfers cannot consume the
capacity reserved for control. Terminal keys serialize commands across both lanes.
Operation deduplication is scoped to the canonical project identity supplied by the
caller. Oldest eligible work from another project gets the next turn; FIFO age
breaks ties. These bounds cover this root's commands, not machine-wide agents.

Requests carry operation identity and expected revision. Replies retain those
values for a fenced commit; they do not themselves acknowledge durable operations.
Queue time consumes the deadline. Expired/cancelled queued requests never spawn;
running requests use the existing owned-process cancellation/cleanup path. Inputs
and captured output have admission caps. Metrics expose queue/running counts,
admission high-water marks, completed counts and maximum observed queue delay.

Stop rejects new work and cancels admitted work. A cleanup deadline miss returns
false while retaining thread ownership. Drop joins workers instead of detaching
commands that could outlive caller guards. A runner panic quarantines the executor,
cancels other work and reports uncertain cleanup even after threads exit. Neither
outcome authorizes worker replacement or capacity release.

The PR-read path is connected to the engine (below). Current cheap and slow passes both
hold the root execution lease; simply moving the slow pass to a thread would still
block status application. Integration must split observation from guarded effects,
retain exact record/operation revision checks, and preserve one terminal owner
before replacing these guards. Routine/SSH/artifact migration and end-to-end
drain/latency acceptance remain open. Standalone queue tests do not establish ticker
responsiveness.

## Asynchronous PR observations

The production ticker submits validated `gh pr view` reads to the control queue and
continues its guarded pass without waiting. A pending read does not change outage
state or advance the project's completed PR-check time. Replies are consumed on a
later pass, with the existing repository/branch reduction and guarded result/event/
finalization writes. Thread execution identity, recorded and actual report hashes,
and URL bind each request; a changed identity cancels/discards its predecessor.
No terminal command or finalization effect runs in the PR executor.

Each read has a 30-second queue-inclusive deadline and the existing ten-second
command timeout. Expiry before runner entry is local backpressure, not a GitHub
outage. Consumed observations have a 120-second cooldown so one pending project
query does not repeatedly enqueue its already-completed peers. The cache holds at
most 128 thread entries and cancels entries unused for 120 seconds when polling
next runs. Cache state is ephemeral; restart safely reissues only read-only queries.
Ticker stop/idle exit cancels and drains reads with a two-second deadline while
retaining ticker ownership. Failure to drain reports unresolved cleanup.

Full-ticker fixtures show that a delayed PR read leaves unrelated session checks
running, preserves outage state, applies a later response, discards a changed report
and drains cancellation. Remaining slow paths are still synchronous. This is not
yet acceptance of the complete T04.2 isolation requirement.

## Asynchronous remote observations

Remote agent/pane lists, saved-machine routing and report hashes now run as one
read-only executor batch. Production PR and remote services share the same root
pool; adding a service does not multiply worker limits. Every subprocess receives
the batch cancellation token and remaining deadline. The internal typed job marker
is interpreted by the native runner and never spawned as an executable.

The ticker continues local work while a batch is pending. Only a complete batch is
applied under the existing execution lease. Full thread records, session/machine
identity and a bounded config snapshot fence the response; input changes discard
and cancel earlier work. Missing config and an empty file are distinct. The cache
holds at most 128 batches with 256 threads each, cancels batches older than 60 seconds
when next polled, and uses a 30-second queue-inclusive command deadline. Pending or
locally expired batches do not establish a remote outage or pane absence.

Copy or pending terminal work rechecks current routing; pending launch/prompt work
also refreshes agents/panes under the guard. Copies, token publication and terminal
effects still run synchronously. These checks preserve authority while transfer
and effect integration remains open. Config reads reuse the nonblocking regular-file
reader, enforce a one-MiB observation snapshot cap, and refuse FIFOs before enqueue.
Fallback SSH config is read only when machine listing did not resolve the target.

Fixtures cover a held remote probe alongside a persisted local status update,
complete-batch application, changed thread/config cancellation, unavailable transport
without false pane closure, and FIFO/oversize refusal. T04.2 remains partial until
routines, transfers and remaining guarded effects have equivalent isolation.
