# Scheduling foundation (partial T04.1)

Schema v10 adds canonical queue records, typed dependency edges and a revisioned
project capacity policy. It does not yet reserve attempts or enable worker launches.
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
authority, immutable reservation inputs and atomic capacity reservation remain W04
work. See the [frozen W04 interfaces](adr/0004-w04-scheduling-contract.md).
