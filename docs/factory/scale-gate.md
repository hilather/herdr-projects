# Scale gate

Recorded against `9fa68abaa9cdfa75b4adb851b0eefd1da9ec3564`
(`Act on targeted reads by default`). Targeted reads are the production hot
path: `HOT_PATH_READ` is `HotPathRead::Targeted` in `src/store/targeted.rs`,
and `poll` acts on `read_targeted_hot_path`. `read_snapshot` stays for admin
and diagnostics. This file does not change code, `SCHEMA`, or
`PREPARED_LAUNCH_DISPATCH_ENABLED`.

## Latency

`not frozen`. This is not a measured bar and not a lowered bar. It is not a
pass of the provisional targets: running-worker observation age 5 s p99, other
active bindings 15 s, and a targeted decision 250 ms p95.

### Samples

The corrected integrated fixture prints `scale_admission_sample` JSON records
with raw wall-clock admission duration and connection-local SQLite row/VM-step
counts. Each of the eight combinations uses 32/64 workers, 256/1,024 additional
terminated attempts, 10,000 retired bindings, and 1,000/100,000 historical events.
The current schema requires terminated attempt history to retire each binding,
so another 10,000 supporting terminated attempts are present and reported
separately. They do not replace the additional attempt-history dimension.

Samples exercise unsigned candidate selection after cancellation frees one slot
(31/63 attempts still retain capacity; `workers` labels the initial active set).
Every sample opens a new controlled connection. They do not measure signed
reservation, launch, observation freshness, memory traffic, or slow providers.
Five samples per combination are a smoke measurement, not sufficient evidence
for tail-latency certification. Cargo's whole-test duration is not a decision
sample. Run the disposable fixture with:

```sh
cargo test --locked --features state-store --test factory_harness scale_gate_for_32_and_64_workers -- --nocapture --test-threads=1
```

The correction run's [raw samples](../reviews/factory-corrections-evidence/scale-admission-inventory.jsonl),
[hardware/source manifest](../reviews/factory-corrections-evidence/scale-admission-manifest.json)
and [full harness log](../reviews/factory-corrections-evidence/integrated-harness.log)
are retained. Its ten harness tests passed. Admission SQL work stayed unchanged
across the tested history sizes at fixed worker count. The host was not frozen
as a certification reference, and this is not a baseline comparison.

### Tick delay is not this bar

250 ms in `next_tick_delay` is not this bar. `next_tick_delay` in
`src/ticker.rs` returns 250 ms while canonical root-exclusive work is pending.
It is a tick, not a decision SLA and not an observation-age bar. When that
work is not pending the cadence is `TICK` (15 s). That idle tick is existing
behavior, not a freshness bar.

## Simulator appendix

The latency section above is `not frozen`. This is not a measured bar and not a
lowered bar. It is not a pass of the provisional targets. Latency targets were
not claimed. This appendix does not add a numeric bar.

`tests/factory_harness.rs` runs 32 and 64 logical workers against event
histories of 1,000 and 100,000, with the additional attempt and retired-binding
history described above. It asserts no false satisfaction,
no slot released early, and complete coverage. The hot path constant must be
`HotPathRead::Targeted`. Shadow / `HotPathRead::Snapshot` fails the gate. This
is not a live 40-worker certificate. `max_active_workers` defaults are
unchanged. `factory_admission` stays `off` in production code. The disposable
fixture temporarily enables admission after creating a synthetic active control
row, then checks that missing contracts/grants prevent any new reservation.
It restores the flag to `off`. No live provider. No pull-request poll.
