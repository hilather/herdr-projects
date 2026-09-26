# Factory release record

Empty. This file does not approve a release and does not set flags.

Candidate SHA:

Reviewer:

## Schema

41. `pub const SCHEMA: u32 = 41` in `src/store/mod.rs`. This record does not change it.

## Admission and dispatch

`factory_admission` default is `off`. This record does not turn it on. The only production writer is the signed CLI. `src/cli.rs` does not compile that writer: `FactoryCommand` is `Status` only, so the factory argv is `herdr-projects factory status PROJECT`. There is no `factory PROJECT status` command. Status prints counters. It does not launch, admit, or set `factory_admission`. Release code does not `SET factory_admission` and does not call `install_admission_policy`.

`PREPARED_LAUNCH_DISPATCH_ENABLED` stays on (`true` in `src/canonical_controller.rs`). This document does not flip it.

## Platform

Linux local is the only factory target. A non-empty `route.machine` is refused before launch (`new launch requires an unused local binding`). `launch` is still compiled on Linux with `state-store`. See [platform](platform.md).

## Latency

`not frozen`. This is not a measured bar and not a pass of the provisional targets: running-worker observation age 5 s p99, other active bindings 15 s, and a targeted decision 250 ms p95. Those targets were not claimed. The 250 ms `next_tick_delay` in `src/ticker.rs` is a tick while canonical root-exclusive work is pending, not a decision SLA. See [scale gate](scale-gate.md).

## Capacity

This is not a live 40-worker certificate. `max_active_workers` stays 0 on a fresh store.

## Adapter matrix

Codex (`kind=codex`) remains the historical single workflow. That is not a new certificate. Claude (`kind=claude`) is an unsupported and uncertified harness. `certified` stays false. Model, effort, and environment mappings stay `refused` for both. See [adapters](adapters.md).

Maximum observed live overlap: zero beyond the historical single Codex workflow.

## Exclusions

macOS, live SSH, remote merge, pull-request poll evidence, model mappings, and the automatic admission default. Automatic admission stays off.

## Restore

Deleting an ownership marker is not a rollback. Restore is a new root. Do not add `recovery_epoch`. See [faults](faults.md).
