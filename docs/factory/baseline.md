# Factory baseline

Recorded against `main` at `3b3b351fe080a006dfc343506149c71c3b7f3ad1`
(`Harden canonical memory and enable verified worker dispatch`). This states the
tree. It does not change card counts in [task status](../task-status.md).

## Store and build

Schema is 25. `const SCHEMA: u32 = 25` in `src/store/mod.rs`.
`migrations/0025_native_profiles.sql` sets both `store_meta.schema_version` and
`PRAGMA user_version` to 25. `SqliteStore::open` recognizes an existing schema
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
