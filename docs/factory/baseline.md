# Factory baseline

Recorded against `main` at `3b3b351fe080a006dfc343506149c71c3b7f3ad1`
(`Harden canonical memory and enable verified worker dispatch`). This states the
tree. It does not change card counts in [task status](../task-status.md).

## Store and build

Schema is 26. `const SCHEMA: u32 = 26` in `src/store/mod.rs`.
`migrations/0026_factory_results.sql` sets both `store_meta.schema_version` and
`PRAGMA user_version` to 26. `SqliteStore::open` recognizes an existing schema
and does not migrate (`migrations are never implicit`).

`state-store` is off by default. `Cargo.toml` sets `default = []` and
`state-store = ["dep:rusqlite"]`. The plugin build in `herdr-plugin.toml` is
`["cargo", "build", "--release", "--locked"]` and does not pass
`--features state-store`.

## Prepared dispatch versus automatic admission

Prepared-launch dispatch is on and automatic admission is off. Those are
different gates. Because automatic admission is off, production launches remain disabled
and production worker launches remain disabled as scheduler preparation: the
scheduler will not prepare an arbitrary queued task. The controller will still
start a launch that is already prepared.

`src/canonical_controller.rs` quotes the prepared-launch gate as:

```rust
// Verified prepared launches participate in ordinary controller polling. Native
// capabilities, current inputs, signed approval and capacity remain enforced at
// ingress; enabling dispatch does not certify optional worker protocols.
const PREPARED_LAUNCH_DISPATCH_ENABLED: bool = true;
```

`process_next` passes `launch_dispatch_enabled()` into
`process_next_with_launches`. That call reads a dispatch hint. It does not call
`admit_prepared` and it does not approve a queued task. The dispatch audit
records the same split: the production gate selects prepared launches and does
not automatically reserve arbitrary tasks. See
[dispatch enablement](../dispatch-enablement.md).

`SqliteStore::queue_report` in `src/store/scheduler.rs` is the automatic-admission
report. For every queued task it appends this blocker:

```rust
blockers.push("launch_preparation_unavailable".into());
```

The returned report always sets `launch_enabled: false`, on the same source line
as the commit:

```rust
let report=QueueReport{head:head(&tx)?,policy:snapshot.policy,retained_attempts,available_slots,launch_enabled:false,entries:entries.into_iter().map(|(_,e)|e).collect()};tx.commit()?;Ok(report)
```

`launch_enabled: false` means the scheduler did not prepare the queue. It does
not mean `PREPARED_LAUNCH_DISPATCH_ENABLED` is false.

## Dependency refusal

Dependency refusal is on. `admit_prepared` in `src/store/reservations.rs`
returns `dependency evidence producers are not available` when a preparation's
dependency list is non-empty. It returns `task is not ready for reservation`
when the queue row has any edge (`!queue.dependencies.is_empty()`). Stored
requirements are only `verified_result`, `integration_candidate`, and
`landed_commit`. Narrative task success does not satisfy an edge.

## Out of scope

macOS and live SSH are out of scope for this baseline. `Command::Launch` exists
only under `all(feature = "state-store", target_os = "linux")`.
`launch_preparation` requires an unused local binding (`route.machine`,
`tab_id`, and `pane_id` empty, and no worktree path yet) and
`launch reserve` requires an installed owner-signed grant.

## Measurement appendix

`tests/factory_harness.rs` records one baseline on a schema-26 store with 1000
events. The store holds 996 tasks. Two `queue_task` calls append four events
(a task change and a queue event each) and leave one `verified_result` edge.
The harness uses a seeded logical clock, a fake runner, a disposable git
repository, and a manifest writer. It does not sleep, open a network, or start
an agent. It does not change production selection or the scheduler default cap.

These numbers are measurements. They are **not frozen**. They are not a
passing bar. The provisional targets — running-worker observation age 5 s p99,
other active bindings 15 s, and a targeted decision 250 ms p95 — are **not
frozen**. This appendix does not treat those targets as met. Today's 250 ms is
`next_tick_delay` in `src/ticker.rs` while canonical root-exclusive work is
pending. It is a tick, not a decision SLA. When that work is not pending the
cadence is `TICK` (15 s). That idle tick is existing behavior, not a freshness
bar, and it is **not frozen** here.

Cost is a row counter on the calls, not wall-clock time. `queue_report` input
rows are the task rows, attempt rows, queue rows, dependency edges, budget
policy rows, and the single project-control row visible to that call.
`read_snapshot` cost is the number of events it decodes, which is the whole
log. Seed 7 run twice produced the same decision log.

| Call | Counter | Observed |
| --- | --- | --- |
| `queue_report` | input rows | 1000 |
| `queue_report` | entries | 2 |
| `queue_report` | `launch_enabled` | false |
| `queue_report` | `max_active_workers` | 0 |
| `read_snapshot` | events decoded | 1000 |
| `read_snapshot` | tasks materialized | 996 |
| store | schema `user_version` | 26 |
| lost reply | trusted rows added | 0 |

Injecting a lost `git update-ref` reply — the fake runner times out and still
returns a forged receipt body — added no row. Schema 26 has no satisfaction
table and no verified-result table. Contract and result tables exist and stayed
empty. `memory_update_receipts` and the other existing receipt and approval
tables stayed empty. The forged body was not stored. `max_active_workers`
stayed at the scheduler default of 0.

No figure in this appendix is a gate. Do not describe this measurement as
meeting 5 s, 15 s, or 250 ms p95.
