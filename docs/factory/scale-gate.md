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

Observation-age and decision-latency samples were not collected in this docs
change. No existing harness prints those samples, so this file does not attach
a number and does not invent one.

`tests/factory_harness.rs` counts rows on a seeded logical clock. It does not
record wall-clock observation age or decision latency. The targeted reader
tests in `src/store/targeted.rs` compare decisions, including the harness
history fixture, and do not print a timing sample. Cargo's own test duration
is not a sample of either quantity.

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
histories of about 1,000 and about 100,000. It asserts no false satisfaction,
no slot released early, and complete coverage. The hot path constant must be
`HotPathRead::Targeted`. Shadow / `HotPathRead::Snapshot` fails the gate. This
is not a live 40-worker certificate. `max_active_workers` defaults are
unchanged. `factory_admission` stays `off` in production code. The harness does
not turn that column on. No live provider. No pull-request poll.
