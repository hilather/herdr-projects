# Scale, overhead and fault certificate (TM5.1)

Card: plan doc 12 TM5.1. Matrix and gates: plan doc 10 §6. Verifier: the
performance verifier (TM5.1). Date: 2026-09-30. Harness:
`tests/telemetry_scale.rs`.

**What this certifies.** On one declared host, a simulated fleet (planted
canonical attempts and generated Codex 0.154.0 rollouts, collected and read
through the real CLI, real SQLite and the ticker's own telemetry pass code)
keeps every correctness gate at zero violations through load, bursts and
faults. It measures resource, freshness, query and controller overhead
against the provisional doc 10 targets. Where a target is missed, §7 records
a reviewed limitation.

**What it does not certify.** Simulated capacity is not live-agent capacity.
No agent was launched, no Herdr server was contacted and no provider was
called. The 32 and 64 "active attempts" are planted rows with rollouts that a
generator appends to. They certify nothing about 32 or 64 live workers, about
the factory's 5/10/20/40 ramp, or about any adapter other than Codex
0.154.0's rollout files (doc 10 §8: a 64-worker simulation is never
extrapolated into live support). The P6 follow-up adds planted quality/proxy observations, integration outcomes
and real waiting-state attention samples (§2, §4.6). Memory timing and sampled
traces remain explicitly not produced: this product has no telemetry producer
for either. This is not a certification of an OTLP transport.

## 1. Source, host and commands

| Item | Value |
| --- | --- |
| Source | branch `telemetry/tm51-scale-certification` from `main` `20a763f`, plus the fixes in §5 |
| Build | `cargo test --release --locked --offline -j 3 --features state-store --test telemetry_scale --no-run` (rustc 1.98.0), system SQLite 3.53.4 |
| Stores | canonical `SCHEMA = 68`; sidecar streams `codex` 3, `ingest` 11, `accounting` 15, `quality` 3, `analytics` 3, `health` 1, `policies` 1 |
| CPU / memory | Intel Core i7-8750H, 6 cores / 12 threads, 62 GiB RAM, zram swap |
| Disk | Intel SSDPEKNW010T8 NVMe, LUKS, btrfs (`compress=zstd:3`). Every dataset lived under `bench-data/` on this disk, never on the RAM-backed `/tmp` |
| OS | Linux 7.2.3-arch1-3 |
| Sharing | The owner's workstation: Chromium, terminals and other agent sessions ran throughout. The 1-minute load average at each phase's start and end is in every results file and quoted below. It ranged from about 1.5 to 13. |

Bounded parallelism: cargo `-j 3`; one test thread; at most four threads in
a bench process (controller, telemetry pass, appender, operator surfaces);
the appender sleeps between batches; no busy loops.

Commands (each phase is its own process, at most ten minutes):

```
env PATH=/usr/bin:/bin HERDR_BIN_PATH=/bin/false TMPDIR=$PWD/bench-data/tmp \
    SCALE_DATA=$PWD/bench-data/<dataset> SCALE_EVENTS=<100000|1000000> SCALE_ACTIVE=<32|64> \
    target/release/deps/telemetry_scale-* --exact <phase> --ignored --test-threads=1 --nocapture
```

Phases: `scale_0_generate`, `scale_1_ingest`, `scale_2_queries`
(`SCALE_REPEATS`, `SCALE_PER_ROUND`), `scale_3_controller` (`SCALE_REPEATS`,
`SCALE_BLOCK_S`, `SCALE_CADENCE_MS`, `SCALE_READER_MS`,
`SCALE_SQLITE_MEMSTATUS`), `scale_4_freshness_burst` (`SCALE_CADENCE_MS`),
`scale_5_faults`, `scale_6_fairness`, `scale_7_late_slow`,
`scale_8_accounting_pass` and `scale_9_health_evaluate`
(`SCALE_REPEATS`, `SCALE_TAG`, optional `SCALE_HEALTH_BIN` for a preserved
pre-change CLI). Each writes `results-<phase>[-<tag>].json` in the dataset
directory. The gate test (§3) runs in the ordinary suite:
`cargo test --features state-store --test telemetry_scale`.

## 2. Workload

One project, 10,000 retained identity bindings, 32 or 64 active attempts,
100,000 or 1,000,000 generated events. Deterministic: seed 5100, splitmix64.

- **Canonical rows** (planted in one transaction with every foreign key kept;
  the generator asserts `foreign_key_check` is empty): 10,000 tasks with
  contracts and classifications (6 classes), launch operations, sealed
  `attempt_inputs` on 8 Codex execution homes, dispatch decisions over 4
  configurations, active collector bindings for all 10,000 (every 20th also
  revoked a day after its rollout), lifecycle marks, and for the 60 % of
  terminal tasks that are accepted an acceptance policy, a verification run
  and a verified result (30 % failed, 10 % cancelled). The active attempts are
  `running` with a launch receipt whose Herdr socket does not exist, so the
  attention sampler records gaps and never calls Herdr. No attempt is launched.
- **Rollouts**: one per active attempt plus evenly spaced terminal attempts,
  each `1 + 10 × 10` lines (a session meta, then ten 10-line turns): turn
  context, user message (64–1024 B of text), tool call (32–512 B), exec
  completion, tool output (64–2048 B), usage record, reasoning (32–512 B),
  usage record, `token_count` with a 300-minute rate-limit window, task
  complete. Usage counters are random within bounds, with a consistent
  cumulative `thread_token_usage`. Active rollouts stop mid-turn and receive
  live appends in the same cycle. The free text is never stored (the
  collector skips it); it is there so bytes are realistic, not empty
  envelopes.
- **Event mix and bytes** (100,000-event dataset, 53.7 MB of rollouts; the
  1M dataset is 999,836 events and 537.5 MB, ten times each line):

  | line kind | events | bytes | mean bytes |
  | --- | --- | --- | --- |
  | `token_usage_record` | 19,800 | 11,567,889 | 584 |
  | `event_msg` `token_count` (quota) | 9,900 | 6,623,865 | 669 |
  | `event_msg` `task_complete` | 9,836 | 1,594,358 | 162 |
  | `event_msg` `item_completed` (exec timing) | 9,900 | 3,140,280 | 317 |
  | `turn_context` | 9,900 | 2,680,690 | 271 |
  | `response_item` tool call | 9,900 | 5,024,234 | 507 |
  | `response_item` tool output | 9,900 | 11,714,548 | 1,183 |
  | `response_item` user message | 9,900 | 6,879,809 | 695 |
  | `response_item` reasoning | 9,900 | 4,065,867 | 411 |
  | `session_meta` | 990 | 433,620 | 438 |

  Live appends follow the same cycle: 100 events/s steady, 1,000 events/s
  for the 60 s fault burst (doc 10's 10× normal ingress).
- **P6 quality and attention mix** (seed 5100, 100k/64): in addition to the
  original 99,926 rollout lines / 53,714,279 B, plant the following bounded
  observations into the product's real sidecar tables. They are simulated
  observations, not evidence from a live reviewer or repository. Each proxy
  references an existing first submission and pinned verification run; each
  integration has a complete canonical operation/candidate/commit chain with
  zero foreign-key violations. The ordinary load gate's body is unchanged;
  its shared oracle now checks these observations through public CLI reports.

  | producer | records | logical JSON bytes | mean bytes |
  | --- | --- | --- | --- |
  | quality `first_candidate_ci` | 5,942 | 4,160,565 | 700 |
  | quality `integration_outcome` (14-day horizon) | 329 | 155,477 | 473 |
  | attention `sample` | 320 | 43,328 | 135 |

  The CI proxies describe the generator's accepted candidates: 4,777 clear
  passes and 1,165 flagged net test removals (excluded from M45, never from
  canonical acceptance). Mature integration outcomes include 108 trailer
  reverts; 19,529 of 38,900 added lines survive, with bounded churn counts.
  Young integrations remain censored. Five samples per active attempt use
  source `herdr-agent-list-v1`, interval 60 s, and states `working`, `blocked`,
  `blocked`, `working`, `idle`: a closed 120 s wait and one intervention per
  attempt, 7,680,000 ms summed waiting. The real sampler still records genuine
  unreachable gaps after these planted historical intervals; no server is
  contacted. Exec timing already uses the collector's real
  `event_msg.item_completed` / `CommandExecution` lines plus matched tool
  call/output lines: startup timing and inferred call-to-output time, **not**
  a claimed execution runtime.

  Total mix: 106,517 observations / 58,073,649 logical B. Rollout bytes are
  actual JSONL bytes; planted-row bytes are the serialized full column
  payloads (no content or diff text), not fictitious collector input. The
  ingest throughput denominator counts only rollout bytes/events, and the
  results separately record SQLite file sizes.
- **Late arrival and slow collection**: `scale_7_late_slow` pins an already
  refreshed M08 window, then appends twenty real rollout lines per chunk,
  sleeping 100 ms between chunks and collecting/syncing each bounded chunk.
  Event times are historical active-window timestamps, separate from arrival
  times (active-window start plus five minutes, before the pinned refresh). Each repeat
  must append an M08 restatement superseding the pinned revision, preserve
  its full as-of answer, match the generator's exact new counters, and pass
  `analytics rebuild --verify`. A small public-CLI workflow also deletes and
  recreates the sidecar, replays the producer facts, checks identical ledger
  bytes and quality reports, and checks the canonical digest.
- **Separate fairness workload**: `scale_6_fairness` retains the 100k/64 hot
  project and adds three isolated 1k-event, 200-binding, four-active projects.
  One ticker-like controller loop visits all four projects, one telemetry
  worker processes projects sequentially, one throttled appender writes
  100 events/s hot and 10 events/s per light project, and one reader requests
  each project's panel and coordinator digest every configured 5 s. Telemetry
  cadence is configurable (1 s for this stress measurement); the controller
  never waits for telemetry or surfaces. Overdue work runs at the next
  opportunity, without overlapping readers or unbounded threads. Three
  15 s rounds record admission/reconcile latency, append-to-ledger freshness,
  pass duration, panel and digest time **per project**; all usage/quality gates
  run after draining. Fixed before measurement: each light project must have
  observed usage and p95 freshness ≤5 s in **every** round. Results are written
  before asserting this criterion; a miss is not hidden or retargeted.
- **Not produced**: memory timing and sampled traces. Memory operations have
  no telemetry timing producer; the telemetry lanes expose no trace sampler,
  span storage or trace-export producer. No timings or traces are invented to
  fill that gap. OTLP transport, non-Codex adapters, and live review/fix
  receipts remain outside this simulated certificate.

The generator keeps its own sum of every usage counter it writes. That sum,
never a production aggregate, is the expected value for the gates.

## 3. Correctness gates

Zero violations everywhere. Each bench phase that changes data ends with the
usage gates, and the CI test repeats every gate on a small project.

| Gate | How it is checked | Result |
| --- | --- | --- |
| No duplicate usage acceptance | no ledger entry has more than one `accepted` disposition; every disposition is `accepted` or `duplicate` | 0 violations (every phase, every dataset) |
| Exact totals | accepted delta entries' record count, input, cache read, output and reasoning tokens equal the generator's sums; `report` M08/M09 equal them; every rollout bound | 0 violations |
| As-of reproducibility | an M08 revision recorded before the load answers identically (value, body, digest, watermarks) after bursts, faults and restatements | pass (CI gate) |
| Byte-identical rebuild | after the load and the kills, the ledger (every entry, native counters, dispositions) equals one collected from scratch into a new sidecar; `analytics rebuild --verify` finds every tracked cell identical and intact | pass (CI gate; `faults` phase for analytics) |
| No canonical writes from telemetry | digest of `state.db` and its WAL before and after all telemetry work (collect, sync, refresh, evaluate, queries, exports, faults) | unchanged (CI gate; `ingest` phase at scale) |

CI gate test: `scale_gates_hold_under_load` (200 bindings, 32 active,
about 4,900 generated events and about 9,000 appended; about 9 s in a
release build, 18–23 s in a debug build).

## 4. Measurements against the doc 10 targets

Distributions are nearest-rank p50 / p95 / p99 (ms unless stated) over all
samples of a phase; `n` is the sample count. Noise is the coefficient of
variation of the per-block (or per-round) p50 across repeats. All numbers
are from release builds after the fixes of §5, with the controller's process
configured as the binary now is (§5 F8, F9).

### 4.1 Summary

| Measurement | Target | 100k events, 64 active | 1M events, 64 active | 100k, 32 active | Verdict |
| --- | --- | --- | --- | --- | --- |
| Controller impact, telemetry on vs off (configured cadence) | p50 ≤ +5 %, p95 ≤ +10 % | admission +20.7 % / +82.5 %; reconcile +21.5 % / +29.7 % | admission +26.4 % / +12.2 %; reconcile +4.0 % / +53.6 % | admission +14.1 % / +72.5 %; reconcile +5.5 % / +3.5 % | **not met** (L1) |
| Derived-view freshness at 100 events/s | p95 ≤ 5 s | p95 5.5 s with a pass every second | p95 34.5 s | p95 7.8 s | **not met** (L2) |
| Indexed dashboard aggregate over 1M events | p95 ≤ 500 ms | native cohort queries p95 0.48–0.89 s (noisy host); 0.19–0.44 s at 100k/32 | native cohort, as-of and export page p95 0.22–0.41 s; lane and central metrics 2.5–8.4 s | — | **met** for native cohort, as-of and paged exports; **not met** for lane/central metrics (L3) |
| Collector resources | 256 MiB RSS; byte caps enforced | `collect` 58 MB; lane ticks 234–280 MB | `collect` 115 MB; pass process 294 MB; `health evaluate` 227 MB | `collect` 57 MB; pass process 282 MB | collector **met**; the ticker's lane ticks **not met** (L4); caps **met** |
| Replay integrity | exact totals, zero duplicate acceptance | 0 violations | 0 violations | 0 violations | **met** |
| Workspace panel refresh, 64 active | p95 ≤ 250 ms | p50 5.5 s, p95 13.9 s | p50 14.6 s, p95 16.1 s | p50 4.6 s, p95 8.6 s | **not met** (L5) |
| Digest section generation | ≤ 100 ms | p50 5.8 s | p50 14.8 s; `context` 15.0–17.0 s with views on vs 2.9 ms off | p50 4.5 s; `context` 4.4 s on vs 3.0 ms off | **not met** (L5) |
| Coordinator digest size | ≤ 40 lines, bounded by top-N | 13 lines, 1,237 bytes | 13 lines, 1,233 bytes | 13 lines, 1,237 bytes | **met** |

### 4.2 Controller impact

The controller thread alternates an admission decision
(`admission::admit_decision_observed`, the read path the ticker's poll runs)
and a reconciliation commit (`record_observations` on the fixture's runtime
binding, a canonical write transaction) every 100 ms. Off: no telemetry work
(as with the ticker's `HERDR_PROJECTS_TELEMETRY_COLLECT_SECS=0`); on: the ticker's
telemetry pass on a thread of the same process, and the operator surfaces
(pane refresh, digest section, an export page) as their own processes. Both
have the same 100 events/s of live appends. 30 s blocks alternate off/on,
order swapped each round, five rounds (three for stress).

| configuration | admission off | admission on | change p50 / p95 | reconcile off | reconcile on | change p50 / p95 | block p50 noise off / on |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 100k/64 configured: pass every 15 s, surfaces every 5 s | 6.10 / 8.84 / 11.53 | 7.36 / 16.13 / 20.38 | +20.7 % / +82.5 % | 2.79 / 13.25 / 16.32 | 3.39 / 17.19 / 21.31 | +21.5 % / +29.7 % | 0.8 % / 25.8 % |
| 100k/64 stress: pass and surfaces every 1 s | 6.08 / 7.30 / 8.75 | 7.59 / 13.62 / 17.16 | +24.8 % / +86.6 % | 2.82 / 4.38 / 16.03 | 3.95 / 17.88 / 27.67 | +40.1 % / +308 % | 0.9 % / 0.2 % |
| 1M/64 configured | 6.36 / 20.24 / 29.35 | 8.04 / 22.70 / 82.71 | +26.4 % / +12.2 % | 3.51 / 19.26 / 64.22 | 3.65 / 29.58 / 111.07 | +4.0 % / +53.6 % | 5.2 % / 28.7 % |
| 1M/64 stress | 6.24 / 11.50 / 15.98 | 9.01 / 16.56 / 30.43 | +44.4 % / +44.0 % | 8.51 / 18.37 / 27.02 | 6.30 / 21.51 / 62.16 | −26.0 % / +17.1 % | 1.2 % / 7.5 % |
| 100k/32 configured | 6.54 / 13.89 / 20.57 | 7.46 / 23.96 / 33.64 | +14.1 % / +72.5 % | 2.91 / 15.27 / 19.07 | 3.07 / 15.81 / 19.89 | +5.5 % / +3.5 % | 18.9 % / 45.4 % |

Before F9 (SQLite memory statistics on, as the binary had them), the same
100k stress run gave admission +110.7 % p50 (7.26 → 15.30 ms). With the
surfaces also inside the controller's process (the harness's first design),
+158 % (stress) and +106 % (configured). Before F8 the pass was not
concurrent at all: its whole duration (100k: 1.9 s p50 steady, 5.7 s in the
burst; 1M: 11–47 s) was added to the ticker's pass for every project after
it.

Two observations bound the reading. The 1M reconciliation maximum was 7.3 s
with telemetry on and 2.4 s off: canonical commits (`synchronous=FULL`)
waited behind the pass's large sidecar transactions on the same btrfs
filesystem (telemetry never writes `state.db`, so this is I/O, not SQLite
locking; §6 shows 12.5 s at p99 beside a 47 s pass). Dirty sidecar pages
from an on-block can also be flushed during the next off-block, which makes
the off baseline worse and the measured difference smaller than the real
one. The absolute latencies stay far below the ticker's 15 s pass
(admission p95 ≤ 24 ms configured), but they are not within the target.

### 4.3 Freshness, burst and backpressure

The pass runs every second (`SCALE_CADENCE_MS=1000`, the fastest this
harness drives; the ticker's default is one pass per 300 s per project).
Freshness is the time from a usage record's append to the end of the first
pass after which the ledger holds it. After each phase, passes run until the
ledger holds every appended record (`settle`).

| dataset, phase | appended usage records | passes | pass p50 / p95 / max | budget exhausted | freshness p50 / p95 / p99 / max | settle |
| --- | --- | --- | --- | --- | --- | --- |
| 100k/64 steady 100/s, 120 s | 2,352 | 53 | 1.86 / 2.19 / 4.79 s | 0 | 3.2 / 5.5 / 6.8 / 7.2 s | 1 pass, 2.4 s |
| 100k/64 burst 1,000/s, 60 s | 11,989 | 18 | 2.37 / 5.71 / 5.71 s | 0 | 4.7 / 8.6 / 9.7 / 10.3 s | 1 pass, 3.4 s |
| 100k/64 drain 100/s, 30 s | 605 | 11 | 2.49 / 5.83 / 5.83 s | 0 | 4.5 / 7.3 / 8.4 / 8.9 s | 1 pass, 2.8 s |
| 1M/64 steady | 2,354 | 8 | 11.1 / 21.3 / 21.3 s | 0 | 22.1 / 34.5 / 39.5 / 40.5 s | 1 pass, 27.1 s |
| 1M/64 burst | 12,018 | 4 | 11.8 / 41.6 / 41.6 s | 2 | 30.3 / 54.9 / 57.5 / 87.5 s | 1 pass, 14.6 s |
| 1M/64 drain | 603 | 2 | 9.6 / 20.2 / 20.2 s | 0 | 25.1 / 31.8 / 33.4 / 33.8 s | 1 pass, 12.6 s |
| 100k/32 steady 100/s, 120 s | 2,375 | 50 | 1.87 / 4.05 / 7.63 s | 0 | 3.3 / 7.8 / 9.0 / 10.3 s | 1 pass, 1.4 s |
| 100k/32 burst 1,000/s, 60 s | 11,979 | 29 | 1.71 / 3.49 / 3.98 s | 0 | 2.9 / 5.8 / 6.7 / 7.1 s | 1 pass, 3.8 s |
| 100k/32 drain 100/s, 30 s | 610 | 9 | 2.94 / 6.59 / 6.59 s | 0 | 5.1 / 9.0 / 9.8 / 10.0 s | 1 pass, 3.0 s |

The per-pass byte cap (8 MiB) held: at 1M two burst passes stopped at it
(`budget_exhausted`) and the next pass continued; nothing was lost or
double counted (gates after every phase). Per step, at 100k steady the pass
spent p50 1.48 s in the accounting tick (attention sample, ledger sync),
0.31 s collecting and 0.06 s in the quality tick; the minute's analytics
refresh adds up to 2.9 s. At 1M: 10.1 s accounting, 1.0 s collect, and 9–10
s for the analytics refresh and 19 s for the five-minute health evaluation
when due.

### 4.4 Queries and exports

Each surface as its own CLI process (a fresh process includes 2.6 ms of
start-up), except the pane refresh and digest section measured inside a
long-running process as `watch` and `context` compute them. 100k: five
rounds of two; 1M: three rounds of two (light) and two of one (heavy), the
order rotated each round.

| surface | 100k/64 p50 / p95 / p99 (n = 10) | round noise | 1M/64 p50 / p95 (n) | round noise |
| --- | --- | --- | --- | --- |
| `query --metric M02` (terminal cohort) | 366 / 888 / 888 | 45 % | 318 / 370 (6) | 5 % |
| `query --metric M02 --by task_class` | 355 / 875 / 875 | 43 % | 325 / 407 (6) | 4 % |
| `query --metric M07 --cohort assignment_cohort` | 327 / 820 / 820 | 45 % | 286 / 326 (6) | 3 % |
| `query --metric M02 --as-of-seq` (stored revision) | 190 / 477 / 477 | 46 % | 159 / 222 (6) | 6 % |
| `export --metric M02 --drill numerator` (500-row page) | 344 / 623 / 623 | 30 % | 318 / 335 (6) | 2 % |
| `query --metric M08` (lane, usage over all events) | 710 / 1,826 / 1,826 | 39 % | 2,262 / 2,549 (6) | 8 % |
| `query --metric M13` (central report) | 1,424 / 3,574 / 3,574 | 34 % | 5,455 / 5,783 (6) | 10 % |
| `view project` | 1,119 / 2,661 / 2,661 | 34 % | 2,645 / 2,976 (6) | 5 % |
| `view cost` | 637 / 1,320 / 1,320 | 37 % | 2,179 / 3,377 (6) | 6 % |
| `view health` | 2,222 / 4,113 / 4,113 | 30 % | 7,878 / 8,433 (6) | 6 % |
| `report` | 2,082 / 4,056 / 4,056 | 32 % | 6,683 / 7,350 (2) | 5 % |
| `compare --metric M02` | 1,582 / 2,875 / 2,875 | 14 % | 6,913 / 7,247 (2) | 2 % |
| `health` (live states) | 5,365 / 8,126 / 8,126 | 16 % | 19,887 / 20,578 (2) | 2 % |
| `workspace show` (pane, fresh process) | 5,538 / 12,639 / 12,639 | 21 % | 15,445 / 16,048 (2) | 2 % |
| `workspace digest` (fresh process) | 5,062 / 11,453 / 11,453 | 20 % | 15,719 / 18,508 (2) | 8 % |
| pane refresh (in process) | 5,488 / 13,869 / 13,869 | 19 % | 14,610 / 16,065 (2) | 5 % |
| digest section (in process) | 5,848 / 16,739 / 16,739 | 40 % | 14,754 / 16,982 (2) | 7 % |
| `context --peek`, views on / off | — | — | 15,032–17,043 / 2.9 | — |

The 100k/64 round noise (14–46 %) comes from the host: other sessions ran
builds while it was measured (load 2.6–5.6). The 100k/32 dataset (same
history, 32 active) was measured at load 1.9 with 2–17 % noise: M02 terminal
cohort 326 / 443 ms, by class 298 / 340, M07 assignment cohort 291 / 300,
as-of 165 / 185, export page 318 / 396 (p50 / p95, n = 10), all within
500 ms; M08 578 / 932, M13 1,187 / 1,567, `report` 1,616 / 1,763, the pane
4,696 / 5,324 and the digest 4,712 / 6,229. The 1M rounds ran at load 2–4
and vary by 2–10 %. So the 100k/64 p95 of the native queries (0.5–0.9 s) is
noise, not a regression: quiet runs at both scales are within 500 ms.
Peak RSS: native queries 33–120 MB, lane metrics 48–228 MB, `report` 279 MB,
the pane 340 MB (1M).

### 4.5 Resources, throughput and size

| | 100k/64 | 1M/64 |
| --- | --- | --- |
| cold `collect` (whole dataset) | 12.6–15.0 s, 6,600–7,900 events/s, 3.4–4.1 MiB/s, 1 run (CLI cap 256 MiB) | 164–174 s, 5,750–6,100 events/s, 2.9–3.1 MiB/s, 3 runs |
| `collect` peak RSS / CPU | 58 MB / 10–12 s | 85–115 MB / 52–61 s per 256 MiB run |
| `accounting sync` | 0.8 s, 41 MB | 13.4–22.5 s, 244 MB |
| `analytics refresh` | 2.9 s, 280 MB | 9.5–11.7 s, 301 MB |
| `health evaluate` | 5.6 s, 234 MB | 20.2 s, 227 MB (2.17 GB before F7) |
| one ticker pass, own process (analytics due) | 3.5 s (collect 0.1, accounting 0.8, analytics 2.5), 282 MB (100k/32) | 17.9 s (collect 0.6, accounting 8.1, analytics 9.0), 294 MB |
| sidecar after cold ingest | 169 MB (3.1× the 53.7 MB of rollouts) | 1.44 GB (2.7× the 538 MB) |
| `state.db` | 39 MB, unchanged by telemetry | 39 MB, unchanged by telemetry |

### 4.6 P6 workload completeness follow-up (100k only)

**Pending the steward's serial 1M certification.** Branch
`perf/workload-completeness`; release build with `--locked --offline -j 3`,
`SCALE_EVENTS=100000 SCALE_ACTIVE=64 SCALE_REPEATS=3`, one bench process at a
time, every dataset under `$PWD/bench-data/`. No production metric or
coverage semantics changed. The old dataset was extended in place by a
second `scale_0_generate`: original rollout files and usage counters were
preserved, only declared quality/integration/attention facts were added.
`scale_2_queries` used three rounds of one sample (`SCALE_PER_ROUND=1`),
before any late/live appends. This compares workload cost, not an optimization.

| surface | before p50 / p95 (ms) | complete mix p50 / p95 (ms) |
| --- | --- | --- |
| M02 terminal cohort | 713.31 / 724.40 | 499.13 / 668.46 |
| M08 usage | 775.41 / 1,559.92 | 1,313.38 / 2,814.97 |
| M13 coverage | 2,604.30 / 2,875.52 | 2,447.13 / 2,551.79 |
| report | 2,468.93 / 4,502.92 | 3,831.98 / 6,414.57 |
| panel refresh in process | 6,514.45 / 10,612.85 | 10,665.83 / 17,840.93 |
| digest section in process | 8,944.75 / 10,626.29 | 7,404.74 / 9,976.89 |

Before `results-queries-before.json` recorded loadavg
`6.09 6.81 5.47` for both fields: the old harness sampled both at phase end;
P6 fixes the start sample. After `results-queries-after.json`: start
`8.21 7.74 8.34`, end `7.70 8.12 8.37` (1/5/15-minute averages).
Per-round p50 CV is 14–37% before and 22–45% after for these surfaces:
**inconclusive for a performance improvement/regression**, with the larger
workload and a noisy shared host. Baseline cold collection took 26.98 s
(3,703 rollout events/s, 1.90 MiB/s); ingest results recorded 1-minute load
5.92 → 6.01. A fresh derived store over the identical original rollout files
with the complete planted mix took 27.51 s (3,632 rollout events/s,
1.86 MiB/s), at load 8.77 → 8.74. These are single cold-cursor runs, with
no page-cache eviction. Sync was 1.20 → 1.95 s, analytics refresh
4.65 → 7.60 s, and health evaluation 5.92 → 12.57 s. Collector peak RSS
was 59,380 → 59,280 KiB. The complete-mix ingest reported zero violations
and an unchanged canonical digest.

`scale_7_late_slow`, three repeats of ten 20-line chunks (600 appended lines,
120 usage records): zero usage/quality/attention violations; each M08
restatement superseded its prior revision; all pinned answers and analytics
rebuilds identical; canonical digest unchanged. Loadavg from
`results-late-slow.json`: start `9.00 8.33 8.42`, end `5.40 7.45 8.11`.

| repeat | collect p50 / p95 (ms), n=10 | post-pin workflow (s) | new input tokens |
| --- | --- | --- | --- |
| 1 | 574.95 / 739.14 | 32.41 | 409,905 |
| 2 | 533.91 / 614.22 | 33.37 | 423,180 |
| 3 | 424.60 / 461.26 | 22.86 | 408,174 |

`scale_6_fairness`: all projects received controller service, all appended
usage became visible, and every usage/quality/attention oracle held after
drain. **The fixed 5 s light-project freshness criterion failed in round 1**:
a hot-project pass took 11.52 s when analytics was due, delaying the same
worker's light projects. Rounds 2 and 3 met it. This is a performance miss,
not a correctness exception; the test writes `fair: false` and then fails,
so a certification cannot silently pass. No target was relaxed and no L2
optimization was attempted. Loadavg from `results-fairness.json`: start
`4.88 7.27 8.04`, end `7.30 7.25 7.96`; round-end 1-minute loads
4.57 / 4.59 / 5.01. Reader cadence 5 s, telemetry cadence 1 s, three 15 s
rounds. Light freshness n=29 / 28 / 32 per project, hot n=306 per round;
controller n=38 per project per round; surface n=1 / 1 / 2 per project.
Panel/digest requests run at their configured cadence when the preceding
request finishes, so expensive hot surfaces can overrun that cadence.

| project | freshness p95, rounds 1 / 2 / 3 (ms) | admission p95 max (ms) | reconcile p95 max (ms) | panel p95 max (ms) | digest p95 max (ms) |
| --- | --- | --- | --- | --- | --- |
| hot, 100k/64 | 12,596 / 4,068 / 5,978 | 21.49 | 14.58 | 9,258.88 | 9,776.97 |
| light 1 | 9,333 / 2,421 / 2,706 | 21.82 | 13.72 | 372.44 | 516.33 |
| light 2 | 9,424 / 2,556 / 2,799 | 21.12 | 13.26 | 384.40 | 484.64 |
| light 3 | 9,517 / 2,596 / 2,899 | 21.69 | 13.90 | 874.64 | 427.23 |

Latency/surface columns take the worst **per-round** p95, not a pooled
percentile. The fairness scenario now exposes the shared worker's delay;
meeting the 5 s criterion belongs with incremental/bounded pass work (L2).
The digest in the complete-mix query phase stayed at 13 lines / 1,307 B.

Reproduction, after the build in §1 (same prepared dataset, serial phases):

```
env PATH=/usr/bin:/bin HERDR_BIN_PATH=/bin/false TMPDIR=$PWD/bench-data/tmp \
    SCALE_DATA=$PWD/bench-data/p6 SCALE_EVENTS=100000 SCALE_ACTIVE=64 SCALE_REPEATS=3 \
    target/release/deps/telemetry_scale-* --exact scale_7_late_slow --ignored --test-threads=1 --nocapture
env PATH=/usr/bin:/bin HERDR_BIN_PATH=/bin/false TMPDIR=$PWD/bench-data/tmp \
    SCALE_DATA=$PWD/bench-data/p6 SCALE_EVENTS=100000 SCALE_ACTIVE=64 SCALE_REPEATS=3 \
    SCALE_CADENCE_MS=1000 SCALE_READER_MS=5000 SCALE_FAIRNESS_S=15 \
    target/release/deps/telemetry_scale-* --exact scale_6_fairness --ignored --test-threads=1 --nocapture
```

## 4.6 P1 incremental accounting follow-up (pending steward's 1M certification)

Branch `perf/incremental-ledger`, accounting stream **12**. This follow-up
addresses accounting's contribution to L2 and L4; it does not certify the
other lanes or promote the complete ticker pass. No 1M run was performed.
The original measurements above remain the TM5.1 baseline.

`accounting_stream` holds a durable source-mutation sequence, committed
watermark, invalidation reason and last sync mode. SQLite triggers queue
sessions in the same transactions that change the native inputs. Sync
replays only affected sessions, including children whose missing parent has
arrived, in the original ordinal order. This dependency boundary preserves
repeated-response exclusion, quarantine, cumulative reconciliation and
model segmentation even for late records and corrections. Unaffected
sessions retain their projections. Ordered new quota snapshots resume the
exact persisted fixed-point window state; late snapshots, corrections and
account reassignment replay affected accounts in observation order.

Schema upgrades, source re-reads from byte zero, source deletions, retention
tombstones/enforcement, backup restores (including pre-frontier backups),
missing projections and inconsistent watermarks force a full rebuild.
`accounting status` reports the committed mode, sequence/watermark and
rebuild reason. Projection writes, watermark advancement and queue removal
share one immediate transaction: a kill rolls them all back while leaving
committed collector inputs queued. Public sync counts still describe the
whole projection; normalization and metric/read/as-of semantics are unchanged.

Both versions used the same on-disk 100k/64 dataset (10,000 bindings,
seed 5100). Its initial source bytes, canonical store and generator state
were restored before each comparison. After copying the project tree, a
full CLI collect settled changed file inodes **before** timing; its
`budget_exhausted` was false. Otherwise the 8 MiB ticker warm pass leaves a
cold re-read backlog: an exploratory after run exhausted five steady passes
and is excluded from the live-load comparison. The baseline freshness run
used the original ingested tree and had no such backlog. Builds and benches
were serialized; one bench process, at most four bench threads.

Build: `cargo test --release --locked --offline -j 3 --features state-store
--test telemetry_scale --no-run`. Use §1's environment with
`SCALE_EVENTS=100000 SCALE_ACTIVE=64 SCALE_REPEATS=3`. Freshness uses
`scale_4_freshness_burst`, `SCALE_CADENCE_MS=1000` (120 s steady, 60 s burst,
30 s drain). Additional isolated phase: `scale_8_accounting_pass`; it warms
sync, then appends 16 real events per active rollout (1,024 lines), collects
and measures the accounting CLI's wall time and `VmHWM` three times. Set
`SCALE_ACCOUNTING_BIN=$PWD/bench-data/baseline-cli` only for its before run
(the preserved pre-change CLI also performs collect); omit it for after.
`SCALE_TAG=before|after` names the results. Source-identity warm collects
are outside the timed samples. Every dataset remained under `bench-data/`.

| 100k/64 measurement | Before | After |
| --- | --- | --- |
| Accounting sync p50 / p95, n = 3 | 708.91 / 848.46 ms | 71.72 / 108.50 ms |
| Accounting sync peak RSS, three appended-data passes | 41,392 KiB (40.4 MiB) | 18,896 KiB (18.5 MiB) |
| Accounting tick p50 / p95 during steady freshness | 1,385.97 / 2,052.73 ms | 109.60 / 1,000.84 ms |
| Accounting comparison 1-minute load, start → end | 6.55 → 6.19 | 1.64 → 1.59 |
| Freshness p95, steady / burst / drain | 9.037 / 9.660 / 13.024 s | 4.964 / 7.302 / 6.335 s |
| Freshness 1-minute load, start → steady end → burst end → drain end → finish | 5.90 → 6.59 → 9.33 → 7.79 → 7.79 | 1.03 → 4.47 → 6.08 → 5.38 → 5.67 |

The sync RSS is the isolated accounting process, not the entire pass. A
separate exploratory forced source re-read/full rebuild reached 41,176 KiB
(40.2 MiB); it is retained as a fallback observation, not an incremental
sample. Both are well below 256 MiB at 100k. Active session histories are
still replayed, so this does not claim arbitrary constant per-event work or
bound a full rebuild at 1M. Host load differs: timings are provisional,
not an authoritative speedup certification. The steady 100/s target is narrowly met at 100k (4.964 s ≤ 5 s);
burst and drain p95 remain above 5 s. Analytics alone reached 5.62–6.07 s
in these passes, so the whole-pass freshness target is not closed by
accounting alone. No valid phase exhausted its byte cap or reported an
error. The first after snapshot experiment included a re-read backlog and
is not a comparable steady-load sample.

Both accounting comparisons and the valid freshness phases reported zero
violations; canonical digests stayed unchanged. The unchanged
`scale_gates_hold_under_load` passed in debug and release, retaining exact
totals, one acceptance, as-of reproducibility and byte-identical rebuild.
The requested 15 suites had **170 passed, four socket-only failures**
(`Operation not permitted` at Unix socket bind):
`telemetry::attempts_show_attention_summary`,
`telemetry_accounting::attention_intervals_union_and_censor`,
`telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`,
`telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`.
Affected suites were rerun after the final re-read/restore compatibility
changes; all 12 operations tests passed and accounting's only failure was
its socket bind. Clippy (`--locked --offline -j 3 --features state-store
--test telemetry_accounting --test telemetry_scale`) found no warning in
changed lines; existing warnings remain elsewhere.

New accounting E2E workflows cover staged late/out-of-order/repeated and
corrected records versus a forced rebuild; a process killed while holding
the sync write transaction (ledger and watermark roll back, resume equals
full rebuild); byte-zero re-read and retention reasons; current and
pre-frontier backup restores; inconsistent watermarks. The existing
secondary-window golden now collects successive snapshots across syncs,
compares with a full replay and adds a late-snapshot replay comparison,
without changing its golden values. Only two stream-version expectations
advance from 11 to 12. No unit or source-text tests were added.

Files: `migrations/telemetry/accounting/0012_incremental_sync.sql`;
`src/telemetry/accounting/{ledger,graph,quota,mod}.rs`;
`src/telemetry/{codex,sidecar}.rs`;
`src/telemetry/maintenance/{mod,backup}.rs`;
`tests/{telemetry_accounting,telemetry_scale}.rs`; this certificate.

### 4.7 P3 / L5 follow-up: workspace history revisions (100k only)

Branch `perf/workspace-snapshot`, 2026-09-30. **Pending the steward's serial
1M certification.** Same generated on-disk dataset before and after:
`SCALE_EVENTS=100000 SCALE_ACTIVE=64 SCALE_REPEATS=3 SCALE_PER_ROUND=2`,
10,000 retained attempts, 99,926 generated events / 53,714,281 rollout bytes.
Release builds used `cargo test --release --locked --offline -j 3 --features
state-store --test telemetry_scale --no-run`; phases 0, 1 and 2 before,
then `analytics refresh` and phase 2 after, with `SCALE_TAG=before|after`.
No 1M run. No concurrent bench or build during either query phase.

The existing `scale_2_queries` already measures `workspace show`,
`workspace digest`, in-process pane/digest and `context --peek`, so the
harness and its correctness oracle needed no change. Each distribution
below contains six samples (three rounds of two), in milliseconds:

| Surface | Before p50 / p95 / max | After p50 / p95 / max | 100k target |
| --- | --- | --- | --- |
| Pane refresh, in process | 6,643.09 / 9,425.82 / 9,425.82 | 63.69 / 96.56 / 96.56 | p95 ≤250 ms: met |
| Digest section, in process | 5,589.32 / 10,322.62 / 10,322.62 | 63.88 / 94.36 / 94.36 | ≤100 ms: met in all six samples |
| `workspace show --json`, fresh process | 7,123.33 / 10,402.54 / 10,402.54 | 69.60 / 96.63 / 96.63 | — |
| `workspace digest`, fresh process | 9,342.27 / 10,792.63 / 10,792.63 | 65.45 / 78.19 / 78.19 | — |
| `context --peek`, views on | 6,760.44 / 11,352.31 / 11,352.31 | 69.32 / 71.27 / 71.27 | whole command, including digest |
| `context --peek`, views off | 4.25 / 4.43 / 4.43 | 3.93 / 4.37 / 4.37 | startup/control |
| `context --peek`, views on again | 9,086.63 / 21,454.52 / 21,454.52 | 68.28 / 77.10 / 77.10 | whole command |

Results files `results-queries-before.json` / `results-queries-after.json`
record load averages (1 / 5 / 15 minutes) **14.70 / 9.30 / 6.52** before,
**3.86 / 7.64 / 9.02** after. Their `loadavg_start` and `loadavg_end` are
identical: this existing query harness samples both at the phase's end,
so these are end-of-phase host loads, not measured starting loads. Pane
round-p50 noise was 12.16% before / 23.91% after; digest noise 14.02% /
27.44%. The shared host and six samples limit certification strength;
these observations do not replace the steward's 1M run. Digest size changed
from 13 lines / 1,237 bytes to 14 lines / 1,406 bytes solely by adding the
history provenance line, within the unchanged 40-line / 4,096-byte caps.

The snapshot builds the rich attempt projection once for open attempts and
candidate arms, shares it with groups, and derives selected attention once
using the existing interval algorithm. Historical metric fields come from
immutable analytics revisions; their compact rendering bodies keep the
original fields and M40's exact latest-per-service/tie/encounter-order rule.
Configuration cells are recorded on analytics refresh using the same
comparison estimators, ordering, pooling, suppression and seed. Analytics-owned
rendering tables leave tracked metric cells, authoritative bodies, digests and
lineage unchanged. P3b adopts them through analytics
stream migration 2 and the retention/backup/rebuild lifecycle. Each history
section
adds `as_of` (its own revision and recording time); missing revisions remain
`unavailable (no_revision_as_of)`, with no live fallback. Digest reads bound
comparison cells/arms, pending selections and alerts to the printed top-N,
with exact omitted counts. Refresh also left the dataset's canonical bytes
unchanged. The E2E suite compares all displayed history values with their
existing public reads and checks current sections over retained cancellations
and terminal candidate arms; no new unit or source-text tests were added.

### 4.8 P3b: rendering projection lifecycle (100k only)

Branch `perf/workspace-snapshot`, 2026-09-30. **Pending the steward's serial
1M certification.** Analytics stream 2 adopts the two workspace tables through
`0002_workspace_projections.sql`, removes the legacy update/delete guards,
and creates them on new sidecars through the stream migration. Refresh no
longer performs schema DDL. Metric renderings are deleted in the same
transaction as their superseded revisions. Comparison renderings retain the
latest row plus the ANALYTICS window (365 days by default); holds and durable
tombstones apply. Both tables are included in backup/restore row inventories
(the online backup already copies the entire database). Restore migrates its
private copy before enforcing tombstones, including backups with the legacy
guards. Non-verifying rebuild recreates metric renderings from surviving
revision bodies and the current comparison through the unchanged estimator.
Read-only legacy/missing-history behavior remains unchanged.

The same fresh, generated disk dataset was measured before and after this
storage correction: 100,000 requested events, 99,926 generated events,
53,714,282 rollout bytes, 64 active attempts and 10,000 retained bindings.
Both release builds used the §4.6 locked/offline command with `-j 3`.
Before: phases 0, 1 and 2; after: `analytics refresh` then phase 2, with
`SCALE_REPEATS=3 SCALE_PER_ROUND=2` and `SCALE_TAG=p3b-before|p3b-after`.
One bench process at a time, no concurrent build during query measurement;
all data and temporary bench projects under `$PWD/bench-data/`, removed
before commit. No 1M run. Six samples per distribution, milliseconds:

| Surface | Before p50 / p95 / max | After p50 / p95 / max |
| --- | --- | --- |
| Pane refresh, in process | 55.84 / 75.03 / 75.03 | 47.76 / 53.62 / 53.62 |
| Digest section, in process | 54.88 / 70.23 / 70.23 | 48.03 / 53.44 / 53.44 |
| `workspace show`, fresh process | 63.69 / 71.62 / 71.62 | 53.07 / 55.65 / 55.65 |
| `workspace digest`, fresh process | 65.82 / 107.06 / 107.06 | 52.78 / 56.83 / 56.83 |

`results-queries-p3b-before.json` records load averages (1 / 5 / 15 minutes)
**5.39 / 6.99 / 7.91**; `results-queries-p3b-after.json` records
**1.91 / 5.29 / 7.03**. As in §4.6, the harness samples both load fields at
phase end. Pane round-p50 noise is 10.86% before / 5.28% after; digest noise
6.86% / 3.30%. The quieter after run prevents attributing the lower times to
this lifecycle correction. The 100k pane and digest targets remain met in
these samples; 1M remains pending. Digest size stays 14 lines / 1,406 bytes.

Correctness: rendering uses the identical projection algorithm, including
M40's latest-per-service, tie and encounter-order rule. No metric evaluator,
coverage rule, authoritative body, digest or lineage definition changes.
Retention deletes only superseded metric renderings and non-latest expired
comparisons; current recorded values and their `as_of` survive. CLI E2E
coverage extends the existing operations workflow with three changing
refreshes, legacy live-sidecar upgrade, workspace foreign-key/orphan checks,
retention apply, legacy-backup restore with identical recorded pane output
(excluding observation-clock fields), exact M40 value/as-of against the
public revision query, and recovery of both disposable projections through
`analytics rebuild`.


### 4.9 P3c: snapshot writer contention (100k only)

Branch `perf/workspace-snapshot`, 2026-09-30. **Pending the steward's serial
1M certification.** The branch initially forked from P6 `29daf9f`; it was
rebased onto current main `d1314c7` before the final comparison, preserving
P1's incremental accounting and retention invalidation. Before is the
rebased P3b code; after adds P3c. Main is `d1314c7`. Earlier experiments on
the old accounting base and the intermediate migration check are excluded.

The same generated disk dataset contains 100,000 requested / 99,926 rollout
events, 64 active attempts, 10,000 bindings and P6's quality/attention facts.
All datasets, private homes and fixture projects lived under `$PWD/bench-data/`.
Release builds used §1's locked/offline `-j 3` command. Phases 0/1 prepared
the dataset; phase 2 ran with `SCALE_REPEATS=3 SCALE_PER_ROUND=1` and
`SCALE_TAG=p3c-rebased-before|p3c-rebased-after`. Before and after the writer
comparison, the same SQLite seed was restored; schema/source identity and
accounting were settled before saving that seed. Main's private measurement
copy was marked analytics stream 1, leaving the extra rendering tables inert.
One bench process at a time, no concurrent build during final measurements,
no 1M run. The gate's five racing writers plus appender bound busy work to six.

Temporary source timers and a local SQLite interposer measured lock holds
from successful `BEGIN IMMEDIATE` through successful commit or rollback.
The interposer writes each record atomically, including idle collector
rollbacks; waits to acquire the lock are excluded. SQL/body contents, return
codes and the unchanged gate oracle are untouched. No instrumentation or
benchmark wrapper is committed. The tables below use the final SQLite trace;
processes killed during fault injection have no completed interval.

Three serial rounds of `collect`, `accounting sync`, `analytics refresh`,
`health evaluate` through the real CLI; milliseconds, p50 / maximum:

| IMMEDIATE scope | Main | Before P3c | After P3c |
| --- | --- | --- | --- |
| collect (2,973 scopes each, including idle rollbacks) | 0.077 / 9.706 | 0.081 / 8.330 | 0.073 / 6.751 |
| accounting sync (n=3) | 21.923 / 28.339 | 29.112 / 412.359 | 23.587 / 28.100 |
| analytics append (n=3) | 13.090 / 15.461 | 36.171 / 61.958 | 13.697 / 14.959 |
| health evaluate (n=3) | 8.188 / 12.460 | 28.912 / 48.830 | 7.805 / 8.594 |
| current-store migration transactions per 12 opens | 12 | 12 | 0 |

`results-traced-writers-main|before|after.json` loads (1 / 5 / 15 minutes):
main **8.30 / 11.34 / 9.86 → 8.47 / 10.92 / 9.81**;
before **17.10 / 11.94 / 9.36 → 17.57 / 13.60 / 10.17**;
after **5.09 / 8.38 / 9.88 → 9.40 / 9.30 / 10.11**.
The before run was substantially busier. Accounting and health algorithms
are unchanged by P3c; their variation is not an accounting/health speedup
claim. Analytics' after maximum is below main's maximum in this sample;
the small median difference from main is inconclusive on this host.

The unchanged `scale_gates_hold_under_load` also ran once per release build
with transaction tracing. Its ordinary small fixture is additional
correctness/contended-lock evidence, not another scale certification:

| Completed scope, p50 / maximum ms | Main | Before P3c | After P3c |
| --- | --- | --- | --- |
| analytics (n=12 / 8 / 11) | 8.391 / 24.309 | 16.442 / 33.541 | 4.324 / 22.815 |
| accounting sync (n=74 / 16 / 78) | 5.751 / 248.031 | 81.617 / 287.760 | 5.238 / 157.052 |
| collect (n=1,864 / 512 / 2,314) | 0.092 / 106.115 | 16.243 / 243.390 | 0.085 / 115.106 |

`results-traced-gate-main|before|after.json` loads:
main **8.47 / 10.92 / 9.81 → 8.65 / 10.84 / 9.80**;
before **8.65 / 10.84 / 9.80 → 10.66 / 11.16 / 9.93**;
after **9.40 / 9.30 / 10.11 → 9.16 / 9.25 / 10.08**.
All three gates passed every exact-total, single-acceptance, pinned as-of,
byte-identical rebuild and canonical-digest assertion. Interleaving changes
batch sizes and scope counts; the collector's slightly higher maximum than
main is not evidence of added collector work (its write path is unchanged).
One post-fault sidecar creation/upgrade remained in the after gate (32.097 ms);
current-store opens take no migration transaction.

P3 previously rebuilt rendering bodies (notably M40's whole decision scan)
inside the writer transaction even for unchanged revisions, and serialized
metric bodies/lineage there. P3c serializes bodies, watermarks, lineage attrs
and comparison/projection JSON before the write lock, retains the original
bucket/ordinal order, caches repeated SQL statements, and inserts a rendering
only for a new revision or a missing disposable row. Latest digests, comparison
bodies and supersession decisions are still rechecked under the same atomic
IMMEDIATE transaction. Migration validates versions read-only first (missing
zero-migration review stream means version zero), then rechecks under
IMMEDIATE only when an upgrade is needed. The five-second busy timeout remains.

No evaluator, metric definition, arithmetic, coverage rule, authoritative body,
digest, lineage or as-of selection rule changes. E2E coverage opens the public
sidecar API beside a held collector write lock and checks real persisted usage;
the existing operations workflow rejects redundant projection inserts, repairs
missing rows with refresh, and preserves exact recorded pane values/as-of.
No new unit or source-text tests. The scale gate and all existing golden values
are unchanged from rebased main/P3b.

The initial full correctness run also exposed the pre-existing first-creation
race (`create_new` lost to another collector and returned `File exists`).
Writable open now joins that collector's newly created regular file, rechecking
its type and retaining `SQLITE_OPEN_NOFOLLOW`. The existing four-collector
certification E2E passes with exact 100/20 usage and no failure. This cold-file
handling does not change the measured current-store transaction paths.

Workspace reads are unchanged. Three phase-2 samples, p50 / p95 milliseconds:

| surface | Before | After |
| --- | --- | --- |
| pane, in process | 77.78 / 159.34 | 109.71 / 142.98 |
| digest, in process | 75.95 / 106.31 | 106.98 / 151.42 |
| workspace show, fresh process | 141.76 / 212.36 | 77.70 / 166.71 |
| workspace digest, fresh process | 114.35 / 199.69 | 76.78 / 112.44 |

`results-queries-p3c-rebased-before.json` loads:
**15.52 / 13.34 / 10.14 → 9.21 / 11.88 / 9.98**;
after **9.16 / 9.25 / 10.08 → 8.34 / 8.83 / 9.84**.
Pane round-p50 CV is 37.31% → 20.20%; digest 16.64% → 33.19%.
These noisy, small read samples are inconclusive for a read-path speedup.
The pane meets 250 ms p95; the digest exceeds 100 ms in both runs. P3c fixes
the snapshot write-lock regression, not L5's remaining certification limit.

Final P3c verification used `--locked --offline -j 3 --features state-store`
and `RUST_TEST_THREADS=1`. All 15 requested suites ran: **175 passed,
10 ignored, four socket-only failures** (`Operation not permitted`):

- `telemetry::attempts_show_attention_summary`
- `telemetry_accounting::attention_intervals_union_and_censor`
- `telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`
- `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`

The exact three-suite command (`--no-fail-fast --test telemetry_scale --test
telemetry_workspace --test telemetry_operations`) ran ten times serially.
**All ten unchanged scale gates passed; no racing process or other correctness
failure occurred.** Each unfiltered command exited 101 solely for the workspace
socket test above; the sandbox cannot establish ten entirely green commands.
No test was skipped or assertion weakened to hide that failure. The repetition
results' load averages (1 / 5 / 15 minutes) ranged from **3.22 / 7.64 / 9.72**
at the first start to **5.87 / 6.89 / 7.49** at the final end (one-minute
start/end observations ranged 3.19–8.17).
Clippy completed with zero warnings in changed lines; existing unrelated
warnings remain. Temporary instrumentation, wrappers and all `bench-data/`
datasets were removed before the final commit.
## 4.10 P2 aggregate reads and incremental analytics (pending steward's 1M certification)

Branch `perf/incremental-analytics`, accounting stream **15**, analytics
stream **3**. This addresses L3 and analytics' portion of L4. Only the
100k/64 dataset was measured; no 1M run was performed.

Accounting sync maintains exact normalized usage totals, separate native
usage/model totals, source certification summaries and serialized session
tool tallies in P1's transaction. Tool tallies retain the complete observed
wait samples, so percentiles and inferred outcomes use the original
arithmetic. Historical M40 headroom is indexed by attempt and recomputed
when its canonical decision or an earlier quota observation changes.
Default cost bodies are indexed by valuation revision. Canonical lifecycle
watermarks and after-termination diagnostics are separately maintained and
validated, avoiding full lifecycle loads for a lane-only read. Fleet
caches hold lifecycle inputs; open ends are still supplied from the clock at read time.
Stale/missing frontiers use the original derivation. Retention and restore
invalidate these projections and preserve P1's recorded rebuild reasons.
Reads check validity and consume totals within one SQLite snapshot.
The complete report uses three bounded readers for accounting, the other
lanes, and central metrics/attention/diagnostics, then merges in registry
order so lane overrides and output order are unchanged. Cached JSON objects are
moved into the metric map directly, avoiding a second recursive allocation
pass over the M40 decision tree.

Analytics tracks durable per-table mutation generations (including updates
and deletes), the accounting frontier/dirty inputs, collector inputs and
the canonical head/file identity. A refresh evaluates only changed cells;
clock-dependent attention, fleet, quality and review cells always evaluate.
Unchanged cells advance `checked_unix_ms` without changing their revisions.
Evaluated cells are appended and released one at a time rather than keeping
all cells' lineage in memory. The original P2 path held one immediate transaction across evaluation;
P2b replaces that path with shared read snapshots and generation-validated
short write transactions (§4.11). Rebuild bypasses all
aggregate shortcuts and replays original sources; digest, lineage,
restatement and stored as-of semantics are unchanged.

Both CLIs used the same on-disk dataset under `bench-data/` (seed 5100,
100,000 configured events, 64 active, 10,000 bindings). After the separate
late-arrival fault phase, restore the original rollout bytes and expected
totals/mix, recreate the sidecar through CLI collects, and replay the same
producer facts and original 384 attention samples. The canonical store
stayed unchanged. Preserve the before CLI before rebuilding. Run
§1's release build and environment, `SCALE_REPEATS=3`, then
`scale_2_queries` with `SCALE_PER_ROUND=1 SCALE_TAG=before|after`.
`scale_9_analytics_refresh` repeats refresh without changing its inputs;
set `SCALE_ANALYTICS_BIN=$PWD/bench-data/baseline-bin` for before only.
Accounting sync installs/fills the new aggregates before the after phases.
The final refresh starts without P2 checked-input/provider cache metadata,
then repeats twice with unchanged inputs. One bench process ran at a time;
cargo used three jobs. These shared-host
measurements are provisional, pending the steward's serial 1M certification.

| 100k/64 read, n = 3; wall p50 / p95 | Before | After |
| --- | --- | --- |
| M08 lane usage | 959.32 / 1,073.07 ms | 26.58 / 27.91 ms |
| M13 central coverage | 1,669.41 / 2,542.99 ms | 40.99 / 46.54 ms |
| Complete report (including M40 decisions) | 2,356.08 / 2,443.80 ms | 278.46 / 308.27 ms |
| Cost view | 750.45 / 752.10 ms | 109.08 / 133.76 ms |
| Query 1-minute load, start → end | 5.52 → 4.25 | 5.33 → 3.37 |

All four measured lane/central read surfaces meet 500 ms at 100k. This is
pending the steward's 1M certification, not a certification of other
operator surfaces. Earlier after variants, before removing recursive
JSON-map deserialization, had report p95 532.83 and 903.83 ms (loads
3.26 → 4.96 and 3.33 → 4.61); those are retained here as exploratory
observations, not the final after samples.

| Analytics refresh, n = 3 | Before | After |
| --- | --- | --- |
| Wall p50 / p95 | 3,669.20 / 8,291.27 ms | 332.01 / 2,292.04 ms |
| Peak RSS | 283,000 KiB (276.4 MiB) | 184,284 KiB (180.0 MiB) |
| 1-minute load, start → end | 10.44 → 10.24 | 5.18 → 5.33 |

The after maximum includes the first refresh of legacy tracked cells;
subsequent unchanged-input refreshes took 332.01 and 296.29 ms and peaked
at 35,920 and 36,044 KiB (35.1/35.2 MiB). Even the cache-initialization refresh is below
256 MiB at 100k. This does not certify full invalidation/rebuild memory,
health, whole-pass memory or 1M. Both refresh phases reported zero usage
gate violations and unchanged canonical digests.

E2E coverage extends real collect/sync/query/refresh workflows: hand-computed
M08/M09 and tools values equal replayed answers, late usage restates the
cell while old as-of results remain reproducible, incremental snapshots
equal a full rebuild byte for byte, and retention/restore retain the exact
answers or unavailable coverage with a recorded full-rebuild reason.
The existing first-collector race E2E exposed a file-creation race: losing
`create_new` returned `AlreadyExists` before reaching SQLite migration
serialization. The loser now verifies the winning path is a regular file
and proceeds with the same no-follow SQLite open and migration lock.
The unchanged load gate passed in debug and release. The 100k late-arrival
phase passed all three repeats, with pinned as-of results and byte-identical
rebuilds. The requested 15 suites had **171 passed,
four socket-only failures**, the same four Unix-bind failures listed in
§4.6. Clippy found no warnings in changed lines; existing warnings remain.

### 4.10 P4 controller and health follow-up (100k only)

**Pending the steward's serial 1M certification. L1 remains open.** Branch
`perf/controller-overhead`, same host and release build command as §1,
`SCALE_EVENTS=100000 SCALE_ACTIVE=64`. No 1M run was performed. The seed-5100
initial dataset, including planted producer facts and fixture homes, was
archived immediately after generation and restored at the same absolute
paths before the final after-ingest run. Both controller runs start from
that dataset after `scale_1_ingest`, before live appends, with identical
workload knobs. All measurement files lived under `$PWD/bench-data/` on disk;
no build overlapped a benchmark and only one benchmark process ran at once.

`scale_3_controller`: `SCALE_REPEATS=5 SCALE_BLOCK_S=30
SCALE_CADENCE_MS=15000 SCALE_READER_MS=5000`, SQLite memory statistics off.
The harness's pass thread now applies the ticker worker's idle scheduling;
controller and operator-reader scheduling stay unchanged. Alternating block
order and ingress match §4.2; operator surfaces were not optimized by P4.

| operation | before off p50 / p95 (ms) | before on p50 / p95 (ms) | after off p50 / p95 (ms) | after on p50 / p95 (ms) |
| --- | --- | --- | --- | --- |
| admission | 8.03 / 17.40 | 9.72 / 21.00 | 8.94 / 17.02 | 11.28 / 24.78 |
| reconciliation | 3.30 / 28.46 | 4.23 / 32.60 | 3.72 / 17.28 | 4.20 / 17.57 |

Telemetry-on overhead (p50 / p95): admission **+21.05% / +20.69% →
+26.17% / +45.59%**; reconciliation **+28.18% / +14.55% →
+12.90% / +1.68%**. Sample counts off/on: 1,446/1,461 before,
1,496/1,483 after. The fixed doc 10 targets remain +5% / +10%; admission
misses both and reconciliation misses p50 in this run. Neither a target
pass nor a controller improvement is certified.

| per-block coefficient of variation | before off / on | after off / on |
| --- | --- | --- |
| admission p50 | 20.66% / 27.26% | 10.33% / 14.52% |
| admission p95 | 33.37% / 24.83% | 17.62% / 26.94% |
| reconciliation p50 | 98.92% / 80.85% | 58.82% / 18.17% |
| reconciliation p95 | 63.79% / 97.58% | 65.24% / 53.11% |

Loadavg (1/5/15-minute averages) from `results-controller-before.json`:
`6.14 6.71 6.45` → `2.44 4.10 5.38`; from
`results-controller-after.json`: `4.95 10.12 10.71` → `4.70 6.08 8.59`.
Block variability exceeds the before/after change in overhead (about
5/25 percentage points for admission, 15/13 for reconciliation):
**inconclusive for a controller improvement or regression** on this noisy,
shared host. Both phases reported zero correctness violations. Idle
scheduling cannot remove operator-surface CPU or all fsync contention.

`scale_9_health_evaluate`, three foreground public CLI evaluations each,
compares the preserved pre-change release CLI (`SCALE_HEALTH_BIN`) against
the final CLI on the identical prepared after-ingest store, before the
controller's live appends. Rules now count after-termination usage without
whole-history usage JSON, stream current quota windows without M40 dispatch
JSON, and derive waiting intervals one open attempt at a time. The same
classification, exact decimal validation, tie order and attention horizon
are retained; this measures health evaluation, not whole-pass memory.

| health evaluation | before | after |
| --- | --- | --- |
| peak RSS, three-run range (KiB) | 178,332–179,052 | 139,856–139,952 |
| maximum peak RSS (MiB) | 174.86 | 136.67 (21.84% lower) |
| wall p50 / p95 (ms) | 8,064.42 / 9,295.59 | 6,553.62 / 7,954.06 |
| wall CV | 14.08% | 19.07% |
| loadavg start → end (1/5/15 minutes) | `6.93 11.95 11.30` → `6.61 11.38 11.13` | `5.88 10.98 11.01` → `5.59 10.50 10.85` |

The RSS reduction is consistent in all three samples. The wall-time change
is **inconclusive**: noise and falling load exceed the p50 improvement.
Both health phases retained the canonical digest and reported zero gate
violations. No 1M health or whole-pass RSS claim follows from these samples.

Cold foreground collection (one sample each, CLI transaction scope retained)
was 22.09 → 31.68 s, 4,523 → 3,155 rollout events/s, 2.319 → 1.617 MiB/s.
Before prepare loadavg: `6.54 7.01 6.52` → `5.88 6.77 6.46`; after:
`11.01 13.79 11.76` → `9.69 12.93 11.59`. These cold-cursor runs did not
flush the page cache and do not establish a throughput improvement.
Both ingests reported exact totals, zero gate violations and an unchanged
canonical digest.

P4 validation: the required fifteen telemetry suites had **172 passed,
11 ignored, four sandbox-only failures** at Unix-socket binds (`Operation
not permitted`): `telemetry::attempts_show_attention_summary`,
`telemetry_accounting::attention_intervals_union_and_censor`,
`telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`,
and `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`.
The unchanged `scale_gates_hold_under_load` and new real-ticker kill/resume
workflow passed. The latter kills after a durable partial rollout prefix,
checks both cursors agree, resumes to exact totals/one acceptance per record,
reproduces a pinned as-of answer and byte-identical ledger rebuild, and
checks telemetry leaves the canonical digest unchanged. The existing quota
health workflow also checks expired/extra-precision windows and stable ties
through public health evaluation and persisted alerts. Existing expected
values and the gate body are unchanged. Clippy completed with no diagnostics
on changed lines (existing warnings remain elsewhere).

### 4.11 P2b: snapshot evaluation and short validated writes (100k only)

Branch `perf/incremental-analytics`, rebased onto main `033e93b` (P3/P3b/P3c).
**Pending the steward's serial 1M certification.** Analytics migrations are
now `0001_aggregate_revisions`, `0002_workspace_projections`, then
`0003_input_frontiers`: current analytics stream **3**, accounting **15**.
The original P2 measurements in §4.10 predate this workspace merge.

Providers that independently open canonical/sidecar readers now share pinned
DEFERRED snapshots for one refresh, including their nested read transactions.
Evaluation, metric/projection JSON, lineage attributes, comparison and provider
bodies are produced outside the writer lock. Each evaluated cell takes a short
IMMEDIATE transaction, reads live input generations and a fresh canonical
fingerprint, then atomically decides its revision and writes serialized data.
Changed inputs defer that cell without advancing its checked inputs; a new
request remains tracked even when deferred. Unchanged-input cells batch their
checked-time updates. Clock-dependent cells continue to evaluate. The final
short transaction independently validates provider bodies and the workspace
comparison before storing them. Refresh reports `evaluated`, `deferred`,
`comparison_deferred` and cumulative `write_lock_ms` (acquisition waits excluded).
Missing metric renderings are repaired without restating their authoritative
revision; workspace comparisons use main's unchanged estimator and projection.

Current-stream migration opens retain P3c's read-only shortcut only when the
input-trigger installation also matches `PRAGMA schema_version`. Otherwise,
installation runs under the migration transaction, covering tables created
since the last refresh. A bounded initial WAL-transition retry also handles
SQLite's immediate BUSY response when first collectors race to create a store.

Both release builds used §1's exact locked/offline `-j 3` command. Phases 0/1
prepared one on-disk dataset under `$PWD/bench-data/`, with 100,000 configured
events, 64 active attempts, 10,000 retained bindings and the complete P6 facts.
The preserved rebased P2 CLI supplies before; the final CLI supplies after.
No source events, canonical inputs or generator totals changed between runs.
Both refresh runs are warm-input measurements, not cache-initialization or
full-invalidation measurements. Run `scale_9_analytics_refresh` and
`scale_2_queries` serially with `SCALE_REPEATS=3`, `SCALE_PER_ROUND=1` and
`SCALE_TAG=p2b-before|p2b-after`, using §1's environment. Before refresh uses
`SCALE_ANALYTICS_BIN` to select the preserved CLI. A temporary SQLite step
interposer around each refresh records successful BEGIN IMMEDIATE through
successful COMMIT; lock acquisition waits are excluded. No instrumentation
wrapper or dataset is committed. No build or other bench ran during either
measurement. No 1M run was performed.

| 100k/64, three refreshes | Before | After |
| --- | --- | --- |
| Refresh wall p50 / p95 | 220.48 / 242.58 ms | 804.11 / 805.69 ms |
| Peak RSS | 36,240 KiB (35.4 MiB) | 77,732 KiB (75.9 MiB) |
| Uninterrupted IMMEDIATE scope p50 / maximum | 206.83 / 209.63 ms (3 scopes) | 6.73 / 14.78 ms (81 scopes) |
| Cumulative writer hold per refresh, p50 / maximum | 206.83 / 209.63 ms | 187.33 / 195.51 ms |
| Refresh load averages, 1 / 5 / 15 minutes, start → end | 2.77 / 5.66 / 4.89 → 2.77 / 5.66 / 4.89 | 2.50 / 4.58 / 4.80 → 2.50 / 4.58 / 4.80 |

This fixes the uninterrupted lock hold; cumulative hold is only modestly
lower. The after path also restores P3's workspace comparison that the old
P2 refresh omitted, and pays for 27 short scopes per refresh rather than
one long scope. Wall latency and RSS increase in this comparison; neither is
claimed as a speedup or a memory improvement. The 100k warm refresh stays
below the unchanged 256 MiB envelope. Full invalidation, whole-pass resources,
concurrent-load behavior and 1M remain for the steward's certification.
Both refresh phases report zero usage-gate violations and unchanged canonical
digests. Individual after scopes are milliseconds, comparable to P3c's
14.96 ms maximum (§4.9).

| 100k/64 reads, three samples; wall p50 / p95 | Before | After |
| --- | --- | --- |
| M08 | 23.00 / 23.38 ms | 22.83 / 26.80 ms |
| M13 | 37.55 / 37.79 ms | 37.81 / 38.21 ms |
| Report | 244.14 / 260.81 ms | 239.77 / 241.99 ms |
| Query load averages, 1 / 5 / 15 minutes, start → end | 1.52 / 4.86 / 4.66 → 1.72 / 4.63 / 4.59 | 1.37 / 4.04 / 4.61 → 1.30 / 3.81 / 4.52 |

All three read p95s remain below 500 ms at 100k; these small, shared-host
samples are not a read-path speedup claim. No evaluator, arithmetic, coverage
rule, digest serialization, lineage order or as-of selection rule changes.
The unchanged gate checks exact totals, single acceptance, pinned as-of
answers, byte-identical rebuilds and the canonical digest. CLI E2E coverage
creates a source table after installation, refreshes, mutates it without
changing its schema, then verifies dependent-cell evaluation with an identical
stored answer. A deterministic late-writer fixture changes inputs after the
read snapshot: refresh defers stale cells and a new requested cell, preserves
checked inputs and recorded answers, and the next refresh resumes the request.
Existing operations workflows still verify projection repair and retention.

Final validation used `RUST_TEST_THREADS=1 cargo test --locked --offline -j 3
--features state-store --no-fail-fast` with all fifteen requested telemetry
suites plus `--test telemetry_routines`: **178 passed, 11 ignored, four
socket-only failures**. The four failures are solely Unix bind denials
(`Operation not permitted`), without a workaround:

- `telemetry::attempts_show_attention_summary`
- `telemetry_accounting::attention_intervals_union_and_censor`
- `telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`
- `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`

The first full run also exposed `database is locked` in the first-collector
creation race. After the bounded WAL transition retry, that existing E2E
passed ten serial runs and the final full suite. The unchanged debug load
gate passed in the final full and focused runs; the unchanged release gate
also passed in 12.26 s. Clippy (`--locked --offline
-j 3 --features state-store --test telemetry_query --test telemetry_scale
--test telemetry_operations`) completed; no warning location falls on a line
changed from `origin/main`. Existing unrelated warnings remain. No unit or
source-text tests were added; only the stream-version expectation advances.

### 4.12 P2c: collector no-op scopes and validated ledger replay (100k only)

**Pending the steward's serial 1M certification.** This follows the P4/DG4a
rebase on `perf/incremental-analytics`. No busy timeout, scheduling policy,
metric, coverage rule or existing expected value changes. No 1M run.

The requested debug reproduction used `TMPDIR=$PWD/target/tmp`, one test
thread, ten `scale_gates_hold_under_load` commands beside a second cargo
process looping the complete `telemetry_operations` suite ten times. Both
cargo commands used `--locked --offline -j 3 --features state-store`; the
five gate racers plus the operations subprocess allow at most six CLI children;
the appender sleeps between one-second bursts.
Each loop finished within ten minutes. Before: **2/10 gates failed** (run 2:
accounting/analytics BUSY; run 8: the reported racing collect BUSY). After:
**10/10 gates and 10/10 operations suites passed**, without changing the gate.

Temporary SQLite step tracing measures successful BEGIN IMMEDIATE through
successful COMMIT/ROLLBACK, excluding acquisition waits and incomplete killed
transactions. Before samples cover the latter eight gates and their concurrent
operations (the first two CLI environments cleared the initial interposer);
after samples cover all ten. Counts therefore differ. The no-op scopes removed
by this fix also change the collector distribution's population. All values
are milliseconds, nearest-rank p50 / maximum:

| Writer under the reproduction load | Before (n) | After (n) |
| --- | --- | --- |
| Collect batches/bindings | 16.283 / 700.830 (4,649) | 27.687 / 765.933 (4,102) |
| Accounting sync, including aggregate maintenance and source-trigger effects | 99.813 / 1,536.294 (228) | 73.847 / 1,023.329 (348) |
| Analytics short writer scopes | 13.337 / 217.370 (4,137) | 9.306 / 1,549.387 (5,551) |
| Health evaluation | 8.857 / 26.275 (24) | 7.275 / 35.126 (30) |

Reproduction results files recorded loadavg (1/5/15 minutes): before
`6.42 6.11 7.74` → `6.06 7.72 8.15`; after
`5.32 4.76 6.20` → `3.68 4.91 5.95`. The traced failing collect run did not
show a completed individual IMMEDIATE hold over five seconds. Repeated writer
acquisitions can exhaust a waiter's timeout without one such hold; the trace
supports that explanation, not a universal upper bound. Collector/analytics/
health maxima are not improved in these noisy samples. DEFERRED termination
receipt writes are outside this IMMEDIATE-scope table.

Stage timers in the baseline gate measured ledger derivation, entry writes,
session graph, quota replay and usage summaries separately; on an isolated
follow-up gate their maxima were 20.90/154.25/15.17/125.89/7.39 ms. In the loaded
baseline, dispatch/tools/fleet/cost/canonical/termination aggregate stages had
maxima 112.11/68.86/48.25/0.23/49.44/49.40 ms. These observations did not justify
splitting the ledger/aggregate commit or weakening its atomicity.

Completed, unchanged rollout prefixes now check device, inode, end offset,
ingest cursor and completed/no-open-turn evidence in one read statement.
Explicit replays, replacements, missing ingest cursors and unfinished turns
still use the original writer path, preserving gap recovery and idle detection.
Binding batches compare the same rules outside the lock and re-read changing
batches under IMMEDIATE. Ledger replay/normalization uses a DEFERRED snapshot;
IMMEDIATE validates the accounting frontier (including invalidation, quota
rebuild and tombstones) and all analytics input generations before writing.
A changed source or racing sync discards the plan and recomputes it. Ledger,
graph, quota, summaries, watermark and dirty-queue removal still commit together.
An aggregate cannot become visible separately from its ledger rows. No trigger
coverage is removed, and no BUSY retry/timeout increase masks a writer failure.

The final release comparison restored the same generated 100k/64 seed, original
manifest, rollouts, SQLite stores and complete producer facts at the same
absolute paths before **both** runs. A collect/sync/refresh settled source/file
identities and cached canonical identities outside measurement. Both builds used
§1's exact release/no-run command. Run `scale_8_accounting_pass`,
`scale_9_analytics_refresh`, `scale_9_health_evaluate` serially with §1's
environment, `SCALE_EVENTS=100000 SCALE_ACTIVE=64 SCALE_REPEATS=3` and
`SCALE_TAG=p2c-before|p2c-after`. Their `SCALE_*_BIN` overrides select a temporary
tracing wrapper around the preserved before CLI or final CLI. No build or other
bench overlapped these final measurements; every dataset and seed was on disk
under `$PWD/bench-data/`. Earlier exploratory before samples are excluded.

| 100k/64 IMMEDIATE scope, p50 / max ms | Before | After |
| --- | --- | --- |
| Collect batches/bindings, three identical appended-data passes | 0.045 / 18.164 (2,976 scopes) | 5.568 / 37.742 (192 scopes) |
| Accounting sync, warm-up plus three changed-input passes (n=4) | 227.223 / 257.491 | 222.456 / 284.230 |
| Analytics refresh (95 scopes each) | 7.379 / 111.343 | 8.503 / 138.157 |
| Health evaluate (n=3) | 10.063 / 10.080 | 7.376 / 58.399 |

The collector removes **93.5% of these acquisitions**; its after distribution
contains actual/open-turn work rather than completed no-op scopes. This is an
acquisition reduction, not a per-scope latency claim. Sync's local contended
maximum drops (§4.12 above), but the serial 100k maximum and other writers'
maxima are not improved. No universal lock-duration bound is claimed.

| Phase wall p50 / p95 ms, n=3 | Before | After |
| --- | --- | --- |
| Accounting sync | 271.87 / 277.71 | 304.03 / 315.73 |
| Analytics refresh | 947.76 / 1,638.36 | 1,004.35 / 1,887.83 |
| Health evaluate | 2,468.60 / 2,537.04 | 3,385.33 / 4,505.57 |

Loadavg start → end (1/5/15 minutes), from the corresponding results JSON:

| Phase | Before | After |
| --- | --- | --- |
| Accounting pass | `1.59 3.55 4.87` → `1.70 3.54 4.86` | `7.04 5.65 5.31` → `6.87 5.64 5.31` |
| Analytics refresh | `1.70 3.54 4.86` → `1.65 3.50 4.84` | `6.87 5.64 5.31` → `6.64 5.61 5.30` |
| Health evaluate | `1.65 3.50 4.84` → `1.63 3.43 4.80` | `6.64 5.61 5.30` → `6.33 5.60 5.30` |

The much busier after host prevents a wall-time improvement/regression claim.
Accounting peak RSS is 29,756 → 30,524 KiB; no memory improvement is claimed.
All six measured phases report zero violations and an unchanged canonical
digest. These 100k observations do not certify 1M or close L1/L2/L4.


The existing completed-turn CLI workflow now repeats no-op collection and
checks persisted ledger bytes, usage and recovered coverage, then replaces the
producer file with identical bytes and checks forced replay and identical
accounting entries. Existing late/corrected-record, kill/resume and full-rebuild
workflows exercise the validated replay path. No unit/source-text tests.

The required fifteen uninstrumented telemetry suites ran with the preamble's
exact locked/offline command, `TMPDIR=$PWD/target/tmp` and one test thread:
**176 passed, 12 ignored, five failed**. Four failures are exclusively the
sandbox Unix-socket bind denial (`Operation not permitted`):

- `telemetry::attempts_show_attention_summary`
- `telemetry_accounting::attention_intervals_union_and_censor`
- `telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`
- `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`

The fifth, `telemetry_certification::accounting_fields_match_the_adapter_certificate`,
is an inherited DG4a expectation mismatch: its unchanged assertion expects
only `codex`, while the preserved before CLI already returns `codex`,
`otlp:claude-code`, `otlp:gemini-cli`, `otlp:codex`. It is not classified as a
socket failure or hidden by editing its expectation. The uninstrumented scale
suite passes all three workflows; the final release gate also passes unchanged
in 21.44 s. They retain the exact totals, single acceptance,
pinned as-of, byte-identical rebuild and canonical digest oracles. Clippy with
`--locked --offline -j 3 --features state-store` and the scale/accounting/collect/
operations targets reports no warning in changed lines; unrelated warnings
remain. No new crate or source process spawn. Temporary instrumentation and all
`bench-data/` datasets are removed before committing.


### 4.13 P2d: Claude-aware aggregates after DG4b (100k only)

Rebased P2/P2b/P2c onto `origin/main` at `ded2cd9`, preserving DG4b's
native Claude adapter and #196's fixture-only rule for every non-Codex
adapter. Main owns accounting `0013_claude_code.sql`; read aggregates are
`0015_read_aggregates.sql`, accounting stream **15**, ingest stream **10**.
P2e subsequently renumbered these read aggregates to 0015; the current
migration/version pins and Stores row agree.

Claude usage already enters the shared ledger and native usage tables, so
usage/native/source totals retain the full derivation's acceptance,
coverage, provenance, reasoning-unavailable and termination rules. The tool
summary now merges Claude execution/outcome counts, unknown outcomes and
sidechain counts, including when cached session rows replace raw rows.
Claude message/tool-result mutations advance the accounting frontier, queue
both old and new session identities when appropriate, and invalidate the
analytics tools dependency. Retention preserves main's `claude_messages` and
`claude_tool_results` purge and removes session/path aggregates with them;
remaining summaries validate their generation stamps.

The native Claude synthetic transcript and planted terminal attempt exercise
public CLI reads before sync, after incremental sync and after a forced full
ledger derivation. M08/M09, M15/M16/M17/M18, report/after-termination, tools,
and cost values/coverage/digests agree. Repricing preserves cost bytes and
M12/M14 bodies. A tool-only outcome correction invalidates summaries without
another usage record. `analytics rebuild --verify` reports identical and
rebuild preserves snapshot bytes. The mixed Codex/Claude workflow and Claude
retention workflow also pass. No fixture reads source files; no new crate or
source process spawn. Main's exact metric expectations remain intact except
for the required accounting version pins.

**Same disk dataset, 100k events / 64 active / 10,000 attempts / eight homes,
seed 5100, three repeats. Pending the steward's serial 1M certification.**
Built each release with §1's `--locked --offline -j 3 --features state-store
--test telemetry_scale --no-run`, then ran `scale_2_queries` and
`scale_9_analytics_refresh` serially with `SCALE_EVENTS=100000`,
`SCALE_ACTIVE=64`, `SCALE_REPEATS=3`, `SCALE_PER_ROUND=1`, tags `p2d-before`
and `p2d-after`. All data stayed under `$PWD/bench-data/`; no other benchmark
or build overlapped timing. The before binary contains the rebased P2c
implementation, prior to these Claude and contention follow-ups. Before the
after samples, the fixture's accounting version was reset to 13 to replay the
corrected, idempotent read-aggregate migration (now 0015 after P2e), then
public sync/refresh settled its
projections outside timing. Sources were neither regenerated nor collected.

| Surface | Before p50 / p95 ms | After p50 / p95 ms |
| --- | ---: | ---: |
| M08 lane usage | 33.44 / 56.74 | 24.65 / 38.20 |
| M13 coverage | 50.95 / 105.53 | 61.02 / 61.30 |
| Report | 247.15 / 254.19 | 365.54 / 422.02 |
| Cost view | 90.67 / 98.79 | 138.63 / 172.01 |
| Analytics refresh | 2,146.90 / 2,219.66 | 882.43 / 885.00 |

Refresh peak RSS is 78,024 → 78,224 KiB (76.2 → 76.4 MiB). Both refresh
results contain `violations: []` and `canonical_unchanged: true`. Load
averages (1/5/15 minutes, start → end), from the results files:

| Phase | Before load | After load |
| --- | --- | --- |
| Queries | 6.80/10.31/8.86 → 8.93/10.35/8.92 | 9.10/11.47/10.29 → 6.78/10.74/10.08 |
| Refresh | 8.93/10.35/8.92 → 9.34/10.41/8.95 | 6.78/10.74/10.08 → 6.87/10.69/10.07 |

These are noisy shared-host samples, not a general speedup claim: report and
cost p95 increased. The scale fleet remains Codex-only; Claude correctness is
fixture-only, not a live or 100k Claude certification. No 1M run was made.

Rebase contention reproduction initially exposed bounded-run SQLite BUSY
failures (accounting and analytics writer acquisition). Unchanged collector
prefixes now skip writer acquisition for completed turns and open turns
still below the existing idle deadline, rechecked after the cursor read;
idle, missing-cursor, replacement and explicit-replay paths retain the atomic
writer path. Termination receipts still reconcile around collection. A
snapshot-wide Claude-table emptiness probe also replaces one probe per Codex
session. Existing idle/replacement/replay checks and the extended public
collection workflow pass. Errors identify the affected writer; no retry or
busy-timeout limit was increased. The final reproduction runs ten unchanged
scale gates beside twelve full `telemetry_operations` cargo runs, all
**0 failures**, without recompilation in the loops. Load start/end was
5.61/8.90/9.07 → 6.02/6.74/8.04. This local result closes neither L1's
controller-latency target nor a universal contention bound.

All 18 `tests/telemetry*.rs` suites ran with `TMPDIR=$PWD/target/tmp`,
`RUST_TEST_THREADS=1`, `--locked --offline -j 3 --features state-store
--no-fail-fast`: **192 passed, 12 ignored**, six socket-only failures. Unix
bind EPERM affects `attempts_show_attention_summary`,
`attention_intervals_union_and_censor`,
`recommendations_and_notices_change_no_canonical_state_and_no_dispatch` and
`thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`; TCP
loopback EPERM affects `http_auth_limits_malformed_and_replay` and
`http_request_rate_is_bounded`. No other final failure. The unmodified scale
gate passes in the full suite, all ten contention runs and the final release
run, retaining exact totals, one acceptance, pinned as-of reproducibility,
byte-identical rebuild and canonical digest checks. Clippy reports no warning
in changed lines. Benchmark datasets are deleted before commit.

### 4.14 P2e: OpenCode/Gemini aggregate compatibility (100k only)

Rebased all four P2 commits through the former `052e14c` onto cached
`origin/main` `ce1b316` (DG6a–c, DG4c and DG4d). The requested fetch failed
before contacting GitHub because SSH rejected the permissions on
`/etc/ssh/ssh_config.d/20-omarchy-keepalive.conf`; no SSH configuration was
changed. This ref already contains both adapter commits named by the steward.
Main's accounting 0013 (Claude) and 0014 (OpenCode) are preserved; the P2
migration is `0015_read_aggregates.sql`, stream **15**. Canonical 0068,
quality 0003, registry v2, health-rules v3 and DG6's quality-collect flake
derivation remain intact. Every non-Codex adapter remains fixture-only.

OpenCode already shares the native usage tables and normalized ledger. The
maintained tool tally now merges its execution/success/failure scope counters
as well as the common totals. Native message/tool insert, update and delete
triggers advance P1's frontier and queue the affected sessions, including both
identities on updates. The analytics tools dependency includes `opencode_*`.
The ledger, summaries and committed frontier still become visible atomically;
stale reads fall back to the original derivation. No metric arithmetic,
coverage, as-of selection, pricing convention or certification rule changes.
Gemini native counters remain outside accounting-certified usage, exactly as
on main; its SDK records remain separate observations. They are never converted
into fabricated ledger deltas. Main's OpenCode/Gemini retention and backup lists
remain; session/path summaries are purged with their sources.

CLI E2E coverage reuses each adapter's native fixtures and planted-attempt
pattern. It compares M08/M09/M12/M14/M15/M16/M17/M18, tools, cost rows/digests
and after-termination diagnostics before sync and after maintained sync, then
forces a full ledger replay and checks identical ledger/cost bytes. Analytics
verify reports identical, and rebuilding preserves the snapshot byte for byte.
OpenCode additionally exercises a tool-only correction, mixed Codex/OpenCode
reads and exact partial pricing: an unknown cache-write convention remains
unpriced; a separate priceable invocation contributes **0.000365 USD**, coverage
**1/2**. Gemini preserves its exact unavailable accounting coverage and seven
SDK observations. Existing adapter retention workflows assert all four
session-summary tables are empty after source pruning. No unit/source-text
coverage, crate or source process spawn was added.

An exploratory contention run exposed an accounting writer-acquisition BUSY
in run 3 (1-minute load 14.46); it is not a socket failure. Collection now
skips the two empty termination-table DELETE transactions when no termination
applies and no receipt is retained. Existing receipt reconciliation, recovery,
idle and replacement paths keep their original writes. The completed-rollout
CLI workflow holds an IMMEDIATE writer lock during three no-op collects and
checks unchanged usage, ledger rows and recovered coverage. OpenCode tool-table
emptiness is checked once per pinned snapshot, avoiding a probe per Codex
session during aggregate maintenance. Neither busy timeout nor retry limits,
durability, gate assertions or existing golden values changed.

Final correctness validation ran all **20** `tests/telemetry*.rs` targets with
`TMPDIR=$PWD/target/tmp`, `RUST_TEST_THREADS=1`, `cargo test --locked --offline
-j 3 --features state-store --no-fail-fast`: **207 passed, 12 ignored, six
socket-only failures**. Every scale workflow passes, including the unchanged
`scale_gates_hold_under_load`. The final release gate also passes unchanged
in **14.30 s**. Unix socket bind EPERM affects:

- `telemetry::attempts_show_attention_summary`
- `telemetry_accounting::attention_intervals_union_and_censor`
- `telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`
- `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`

TCP loopback bind EPERM affects `telemetry_otlp::http_auth_limits_malformed_and_replay`
and `telemetry_otlp::http_request_rate_is_bounded`. No workaround or expected-value
change was made for these failures. Clippy checks the adapter, collect, query,
scale and operations targets against changed lines from `origin/main`.

The final reproduction prebuilt `telemetry_scale` and `telemetry_operations`
with `--locked --offline -j 3 --features state-store --no-run`, then ran ten
exact `scale_gates_hold_under_load` cargo commands beside ten complete
`telemetry_operations` cargo commands. Both loops used `TMPDIR=$PWD/target/tmp`
and one test thread; **10/10 gates and 10/10 operations suites passed, zero
failures**. A documentation edit triggered the package-wide build script and
recompiled unchanged source in run 2; all other final runs reused their
prebuilt binaries. Load averages (1/5/15 minutes)
were **1.27/3.65/5.65 → 6.29/5.09/5.64**. This is local contention evidence,
not a universal starvation bound or a controller-latency certification. Clippy
reported **zero warnings on changed lines**; unrelated warnings remain.

**Same on-disk 100k/64 dataset, three repeats; pending the steward's serial
1M certification. No 1M run.** Both releases used §1's exact
`cargo test --release --locked --offline -j 3 --features state-store
--test telemetry_scale --no-run`. The rebased P2d code with migration 0015
renumbered, before the adapter/correctness follow-up, supplies the baseline.
Phases 0 and 1 generate/ingest the seed-5100 dataset once. Run phase 2 and
`scale_9_analytics_refresh` with `SCALE_EVENTS=100000 SCALE_ACTIVE=64
SCALE_REPEATS=3 SCALE_PER_ROUND=1 SCALE_TAG=p2e-before|p2e-after`.
Before the after phases, reset only the sidecar accounting version to 14,
replay the corrected idempotent 0015 migration, then settle public accounting
sync and analytics refresh outside timing. No native source, manifest,
canonical row or producer fact was regenerated, moved or re-collected. All
fixture homes and data stayed under `$PWD/bench-data/`; one bench process at a
time, no overlapping build or other test. The final contention workload is
separate from these measurements.

| 100k/64 surface, n=3 | Before p50 / p95 ms | After p50 / p95 ms |
| --- | ---: | ---: |
| M08 lane usage | 39.04 / 52.03 | 36.61 / 66.41 |
| M13 coverage | 56.89 / 94.58 | 48.24 / 93.83 |
| Report | 672.72 / 729.37 | 396.78 / 413.84 |
| Cost view | 222.83 / 227.73 | 132.81 / 158.43 |
| Analytics refresh | 1,467.35 / 1,630.35 | 1,234.62 / 1,319.66 |

Refresh peak RSS is **78,408 → 78,364 KiB** (effectively unchanged). Both
refresh result files report `violations: []` and `canonical_unchanged: true`.
Load averages (1/5/15 minutes, start → end) from the results JSON:

| Phase | Before load | After load |
| --- | --- | --- |
| Queries | 4.58/5.08/6.28 → 5.78/5.32/6.31 | 6.15/6.41/6.06 → 5.53/6.24/6.01 |
| Refresh | 5.78/5.32/6.31 → 5.88/5.35/6.32 | 5.53/6.24/6.01 → 5.65/6.25/6.02 |

All four measured read p95s are below 500 ms after this follow-up, at 100k.
M08 p95 increased, and the host is shared and noisy: **no general speedup,
RSS reduction or L3 closure is claimed**. The scale fleet is Codex-only;
OpenCode/Gemini/Claude correctness remains fixture-only, not live or 100k
adapter certification. L1's controller-latency and L4's whole-pass resource
targets remain open. Benchmark data is deleted before commit.

Files for the P2e resolution/follow-up: accounting migration 0015;
`src/telemetry/accounting/{mod,tools}.rs`, `src/telemetry/analytics/inputs.rs`,
`src/telemetry/{codex,maintenance/mod}.rs`; shared telemetry test support and
`tests/telemetry_{accounting,claude,collect,gemini,opencode}.rs`; this
certificate and the collection/common/analytics contracts. Main's backup
inventory is preserved without edits.

### 4.15 DG1b: lazy M30 candidates and maintained report bodies (100k only)

Rebased DG1 onto already-fetched `origin/main` **8ec057a**; no fetch, no 1M
run. **Pending the steward's serial 1M certification.** Registry **v3** adds
M30 after DG6's registry v2; comparison **v2** follows main's comparison v1.
This addresses M30's added contribution to L3, without closing L3 at 1M.

Lifecycle extraction no longer reads submissions, verification runs or
policies for M02/M06/M07 or non-M30 comparisons. Native M30 enriches the
already-loaded tasks once, including mixed-metric requests in either order.
The first-submission query walks attempts once and searches
`result_submissions_by_attempt`; policies and verdicts are prepared once,
with policy-primary-key and `verification_runs_by_submission` searches.
Receipt checks use the unique run index. Existing acceptance reads retain
`verified_results_by_submission`; DG6's tree/policy index and flake producer
are preserved. Tree-equivalent runs on another submission cannot adjudicate
M30: its lookup must retain submission identity. Public `analytics plans`
reports no unexpected scans or automatic indexes for these three new reads;
the CLI E2E plan test checks the same actual query plans.

Report derives the identical M30 body from central's existing task evidence,
then maintains it in P2's validated central-provider aggregate. Warm reports
read that body, with no second rich lifecycle load. Stale/missing projections
fall back to the same evaluator. Non-M30 central fallback queries do not
compute M30; refresh includes its report body only when M30 is in the requested
or tracked set. Canonical file/head identities, input generations and registry
version validate caches. The ordinary lifecycle watermark keeps main's exact
serialization independent of lazy candidate enrichment; stored as-of revisions
are unchanged.

All releases used §1's build command (`--locked --offline -j 3`). Phases 0/1
prepared one seed-5100 on-disk dataset: 100,000 requested events, 64 active,
10,000 retained bindings, original producer facts. Every fixture home and
dataset stayed under `$PWD/bench-data/`; no source or canonical fact was
regenerated between comparisons. Warm `analytics refresh` runs are outside
measurement. Phase 2 used `SCALE_EVENTS=100000 SCALE_ACTIVE=64
SCALE_REPEATS=3`, four samples per round (n=12 per surface), one bench process
at a time, with no overlapping build or test. Full-phase results are
`results-queries-dg1-before|main|after.json`; milliseconds:

| Full query phase | Before DG1b p50 / p95 | origin/main p50 / p95 | After DG1b p50 / p95 |
| --- | ---: | ---: | ---: |
| Report | 436.44 / 476.14 | 254.11 / 447.56 | 335.02 / 411.16 |
| M02 terminal cohort query | 369.80 / 397.37 | 297.97 / 321.07 | 339.36 / 371.32 |

Load averages (1/5/15 minutes, start → end): before
**0.95/2.68/3.28 → 1.11/2.25/3.07**; main
**1.69/2.69/3.05 → 2.18/2.73/3.04**; after
**5.29/4.68/3.82 → 3.06/4.18/3.74**. Report p95 decreased; M02 is slower
than this main sample on the substantially busier after host. That first
comparison does not establish M02's no-regression requirement.

To resolve that concern, phase 2 adds optional `SCALE_QUERY_SET=dg1` (only
report and M02, no in-process workspace/context work) and `SCALE_QUERY_BIN`
(the preserved release CLI; identical isolated environment). Default phase
behavior and the scale gate are unchanged. A fixed serial order, selected
before observing its results, ran **main-pre → before → after → main-post**
on that same dataset. Each phase still uses three repeats / four samples per
round. Result tags are `dg1-pair-main-pre|before|after|main-post`; milliseconds:

| Focused phase | Report p50 / p95 | M02 p50 / p95 | Load 1/5/15, start → end |
| --- | ---: | ---: | --- |
| Main-pre | 299.45 / 306.73 | 344.86 / 365.58 | 4.15/3.68/3.41 → 4.30/3.72/3.43 |
| Before DG1b | 627.71 / 806.38 | 404.78 / 573.08 | 4.28/3.73/3.43 → 3.99/3.69/3.42 |
| After DG1b | 263.95 / 285.52 | 298.77 / 307.12 | 3.75/3.64/3.41 → 3.53/3.60/3.39 |
| Main-post | 232.66 / 242.37 | 277.57 / 282.08 | 3.33/3.56/3.38 → 3.14/3.51/3.37 |

Focused after p95 is below main-pre and the original main p95 for both
surfaces, and below the unchanged 500 ms target at 100k. The quieter main-post
is faster than after, and identical main code itself moves by 21%/23% in
report/M02 p95 across the bracket. All measurements are retained: this is
provisional evidence that removes the candidate-loading regression, not an
unqualified speedup or a closed 1M/noise-independent no-regression certificate.

Reproduce focused phases after §1's release build, using the same dataset and
preserved CLI for each tag:

```
env PATH=/usr/bin:/bin HERDR_BIN_PATH=/bin/false TMPDIR=$PWD/bench-data/tmp \
    SCALE_DATA=$PWD/bench-data/dg1 SCALE_EVENTS=100000 SCALE_ACTIVE=64 SCALE_REPEATS=3 \
    SCALE_QUERY_SET=dg1 SCALE_QUERY_BIN=$PWD/bench-data/<before|main|after>-cli \
    SCALE_TAG=dg1-pair-<tag> \
    target/release/deps/telemetry_scale-* --exact scale_2_queries --ignored --test-threads=1 --nocapture
```

Correctness: deterministic first-submission timestamp/ID ordering, immutable
policy-body digests, accepted-receipt precedence over rejection, pending and
unknown-policy exclusions, and the evaluator are unchanged. The full 100k M30
report body is identical before/after: 5,942 pending first candidates, zero
adjudicated candidates, null / `empty_denominator`, partial coverage. This
capacity fixture does not certify positive M30 adjudication; the CLI fixture
covers accepted/rejected/pending multi-policy candidates, retries, dimensions,
frozen comparison arms and old as-of revisions. Its extensions check mixed
M02/M30 ordering and cache invalidation after a real verdict change.

All fifteen requested correctness suites ran with the exact locked/offline
`-j 3` command and one test thread: **180 passed, 12 ignored, four socket-only
failures** (`Operation not permitted` at Unix socket bind):

- `telemetry::attempts_show_attention_summary`
- `telemetry_accounting::attention_intervals_union_and_censor`
- `telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`
- `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`

`scale_gates_hold_under_load` passed unchanged: exact totals, one acceptance,
pinned as-of reproducibility, byte-identical rebuild and canonical digest.
Every existing expected metric value is preserved; only registry/comparison
version pins advance. No unit/source-text coverage, crate or source process
spawn was added. The final release gate also passed unchanged in **11.36 s**.
Clippy (`--locked --offline -j 3 --features state-store --test telemetry_query
--test telemetry_scale --test telemetry_operations`) completed with **zero
warning locations on lines changed from origin/main**; 98 existing unrelated
diagnostics remain. Benchmark data is removed before committing.

Files: `src/telemetry/analytics/{lifecycle,query,compare,store,registry}.rs`,
`src/telemetry/metrics.rs`, `tests/telemetry_{query,scale,health,export}.rs`,
registry pins in the analytics/export/quality contracts and phase-2 lanes,
and this certificate. The rebased original DG1 retains its comparison contract
and CLI cohort coverage.

## 5. Inefficiencies found and fixed

The first measurement (same generator, same host) missed the query and
pane targets by two to three orders of magnitude, and every one of those
misses was a lookup repeated per source, per decision or per attempt that
scanned a whole table. Each fix below is an index or a one-pass rewrite with
the same answers: every telemetry suite passes unchanged except the
stream-version expectations (§9).

| # | Where | Inefficiency | Fix |
| --- | --- | --- | --- |
| F1 | sidecar stream `codex` 3 (`migrations/telemetry/0003_read_indexes.sql`) | per source: `codex_usage` scanned by `path_digest` (central and lane M08/M13/M15, the TM4.1 proposal); per attempt: `rollout_sources` scanned by `attempt_id` (attempt projection); per record over its session: the mixed-model turn test and the repeated-response exclusion (certificate-core R3) | four indexes: `codex_usage(path_digest, accepted, reason)`, `rollout_sources(attempt_id, binding)`, `codex_usage(session_id, turn_id, model)`, `codex_usage(session_id, response_id, payload_digest, accepted, ordinal)` |
| F2 | sidecar stream `accounting` 11 (`0011_quota_window_lookup.sql`), `accounting/quota.rs` | M40, per dispatch decision: a `count(*)` over the account's observations, a `DISTINCT` over them, a trusted-window search that scanned every observation of a kind with none trusted (Codex has no secondary window), and a shared-window search over every observation | two indexes, `EXISTS` for the count, the account's limit ids walked through the index, the reset tolerance as a `BETWEEN` range |
| F3 | canonical migration 0067 (`0067_telemetry_read_indexes.sql`) | per attempt: `result_submissions` scanned by `attempt_id`, and an automatic index built on `verified_results` by `submission_id` (the TM4.1 proposal) | `verified_results(submission_id)`, `result_submissions(attempt_id, created_unix_ms)`; `analytics plans` accepts either table as the one-pass driver of the acceptance-time join |
| F4 | `codex.rs` `bind` | every collect compared every source with every attempt and wrote two rows per source, each its own fsynced commit | one transaction, attempts by id, writes only rows whose result changed |
| F5 | `accounting` `ledger::sync`, `graph::store`, `quota::store` | every session filtered every entry; every insert re-compiled its SQL | entries grouped by session once; cached insert statements |
| F6 | `outcome.rs` `record`, `sidecar.rs` `attempt_usage`, `quota.rs` `headroom` | statements re-compiled once per attempt or decision (10,000 each) | cached statements |
| F7 | `health/rules.rs` `accounting_conflict`, `ledger::open_dispositions` | the rule built the whole ledger as JSON to count two dispositions: 2.17 GB peak RSS at 1M events | a grouped SQL count |
| F8 | `ticker.rs` `telemetry_pass` | the pass (collect and every lane tick) ran inline in the ticker's pass, so its whole duration delayed every later project's controller poll | its own thread, one project at a time; controller polling never waits for it (the integrity check's pattern); orderly shutdown waits for a running pass up to 60 s |
| F9 | `main.rs` | with the pass on a thread (F8), SQLite's memory statistics made every allocation of both threads take one process-wide mutex | statistics off in every build of the binary, as the crate's tests already do; nothing reads them |
| F10 (P3) | `workspace`, attempt/attention reads, analytics rendering projections | whole-history reports and repeated rich projections on every pane/digest read | one targeted shared projection, recorded history with `as_of`, bounded digest reads; 100k results in §4.6, pending steward 1M certification |
| F11 (P3c) | analytics append, sidecar migration | whole-history serialization/rendering and redundant projection/migration work while holding the writer lock | pre-serialized bodies/lineage, cached SQL, changed/missing rendering rows only, read-first version check; §4.9 has 100k lock timings, pending steward 1M certification |

| F10 (P4) | `telemetry/background.rs`, ticker and benchmark pass workers | background CPU/I/O competes with canonical controller commits | calling thread uses Linux SCHED_IDLE, nice 19 and I/O idle class; failures log once and never fail a pass; inherited by gated lane subprocesses |
| F11 (P4) | `codex.rs` collector/binding | ordinary ticker tails and binding updates hold large write transactions | complete-line prefixes at 2,000 lines / 8 MiB input, atomic cursors and parser state; bindings in 1,000-source batches; FULL durability retained; foreground CLI and byte-zero replay scopes retained; larger semantic lane transactions remain |
| F12 (P4) | health after-termination, quota and waiting rules | whole-history JSON built for small diagnostics, beyond F7's grouped count | reuse per-attempt after-termination query, SQL count/current-window stream, per-open-attempt attention derivation; exact decimal and tie semantics retained (§4.7) |

F8 preserves graceful shutdown of the whole pass: stop-file and idle exits poll
the running telemetry thread for up to 60 s (`TELEMETRY_SHUTDOWN_WAIT`), logging
“waiting for telemetry pass” once. If it is still running at the bound, the
ticker logs “telemetry pass still running at shutdown; exiting (the pass is
crash-safe)” and exits. Forced or abnormal exits do not wait.

Before and after on the 100,000-event dataset (10,000 bindings, 64 active;
before at load 4.8–11, after at load 2–7; single CLI runs before, p50 of
ten after):

| operation | before | after |
| --- | --- | --- |
| `collect` (cold, whole dataset) | 16.6 s | 12.6–15.0 s |
| `accounting sync` after 30,000 live records | 9.1 s | 2.0 s |
| `analytics refresh` | 48.9 s | 2.9 s |
| `health evaluate` | 81.6 s | 5.6 s |
| `query --metric M13` | 47.5 s | 1.4 s |
| `query --metric M08` | 3.2 s | 0.7 s |
| `report` | 55.5 s | 2.1 s |
| `health` (live states) | 139.8 s | 5.4 s |
| `workspace show` (pane) | 213.9 s | 5.5 s |
| `workspace digest` | 165.0 s | 5.1 s |
| telemetry pass, steady 100 events/s | p50 3.7 s, p95 7.5 s | p50 1.9 s, p95 2.2 s |
| `health evaluate` at 1M events (F7) | 25.5 s, 2.17 GB RSS | 21.1 s, 221 MiB |
| admission decision beside a continuous pass (F9) | p50 +111 % | p50 +25 % |

The remaining costs are the structural ones in §7.

## 6. Faults

`scale_5_faults` at scale, and every fault again in the CI gate test.

| fault | 100k/64 | 1M/64 |
| --- | --- | --- |
| **SQLite contention**, 60 s: `collect` + `accounting sync`, `analytics refresh` + `health evaluate`, and 500-row export paging, each looping in its own process, beside the controller | 0 failed runs (28 collect/sync loops, 8 refresh/evaluate, 106 export pages); admission p50 / p95 / p99 14.5 / 31.9 / 38.0 ms, reconcile 9.8 / 20.3 / 24.6 ms | 0 failed runs (8, 4, 130); admission 10.7 / 18.6 / 23.8 ms, reconcile 3.7 / 15.4 / 18.9 ms |
| **Full spool**: after a backlog of 400 lines per active rollout, a `collect` that may not write past 1 MiB of any file (the WAL truncated first: a full disk for the sidecar) | exit 0 in 0.7 s; one `sidecar_write_failed` gap `pending`, nothing of the range stored; the next `collect` recovered it in 3.4 s (gap `recovered`); totals exact | exit 0 in 1.3 s; same; recovered in 4.0 s; totals exact |
| **Slow exporter**: `export --external` to stdout (enabled in `telemetry-export.toml`) into a one-page pipe nobody reads | blocked in `write` throughout; 8 passes of 3.2–5.3 s ran beside it; admission 19.2 / 34.7 / 54.5 ms; WAL 0 bytes (checkpoints not held back) | blocked throughout; 1 pass of 47.6 s; admission 7.1 / 13.1 / 20.0 ms; reconcile p99 12.5 s during the pass (§4.2, I/O) |
| **Restart recovery**: `collect`, `accounting sync`, `analytics refresh`, `collect`, `health evaluate` each SIGKILLed after 50–650 ms (every one mid-run), then passes until caught up | recovered in 25.6 s (2 passes, a 25,600-line backlog); totals exact; `analytics rebuild --verify` identical and intact | recovered in 47.4 s (2 passes); totals exact; `rebuild --verify` identical and intact after a refresh (below) |

The first 1M `rebuild --verify` compared the recomputed cells with
revisions recorded before the recovery passes had collected the backlog
(the tick refreshes at most once a minute), so the usage and tool cells
differed as expected. After `analytics refresh`, only M35 differed, and it
matched on the next refresh: its activity windows move with the wall clock
while attempts run, a restatement contracts-analytics.md §4 already
documents. The harness now refreshes and retries once before verifying. The
exporter never touches a store while it writes: the query finishes, its
connections close, then the page is written.

## 7. Reviewed limitations

Each target that is not met, with its cause, what it would take, and its
owner. None is hidden by loosening the target.

- **L1: controller latency rises 14–26 % at p50 and 12–83 % at p95 while
  telemetry works at its configured cadence (target +5 % / +10 %).** Measured causes, in order: the
  SQLite allocation mutex shared with the ticker's telemetry thread (fixed,
  F9: +111 % → +25 %); CPU taken by the operator surfaces, which at this
  history size cost seconds per refresh (L5), so a pane refreshing every
  5 s is a continuous load on one core; the pass's own CPU (1.9 s at 100k,
  11–47 s at 1M per pass); and at 1M, fsync contention between the pass's
  large sidecar transactions and canonical commits on the same filesystem
  (seconds-long reconcile stalls). Absolute controller latencies stay below
  25 ms at p95 in the configured runs, against a 15 s ticker pass; no
  admission, reservation or commit failed and no controller operation was
  reordered. **P4 follow-up: idle CPU/I/O scheduling and bounded ticker
  collection/binding commits implemented (F10–F11). At 100k/64, admission
  overhead p50/p95 +21.05%/+20.69% → +26.17%/+45.59%; reconciliation
  +28.18%/+14.55% → +12.90%/+1.68%, inconclusive under block noise and
  pending the steward's 1M certification (§4.7). L1 remains open.** FULL
  durability stays in force: core R4's live attention and valuation history
  are not re-collectable. Ordinary ticker tails are bounded, but byte-zero
  replay and foreground collection retain their scopes; accounting and
  analytics watermark/revision transactions remain atomic. Remaining
  remedies: incremental lane work (L4), cheaper operator surfaces (L5, P3),
  or a separate sidecar device.
  **P2c follow-up: the P4/P2 concurrent-collect regression is resolved in
  the local ten-run workload (2/10 failures → 0/10). At 100k/64, three
  changed-input collects take 2,976 → 192 IMMEDIATE scopes; sync lock
  p50/max 227.22/257.49 → 222.46/284.23 ms (§4.12), at accounting-phase
  loads 1.59 → 1.70 before / 7.04 → 6.87 after. Pending the steward's 1M
  certification; no controller-latency target is closed.**
  Owners: accounting and analytics lanes, TM4.8, ticker steward.
- **L2: freshness.** By default the ticker collects once per 300 s per
  project, so a derived view is up to five minutes old by design. Even with
  a pass every second, p95 was 5.5–7.8 s at 100k and 34.5 s at 1M, because each
  pass rebuilds the ledger, session graph and quota windows from every
  collected record (the original `ledger::sync` was a full rebuild).
  **P1 follow-up: 100k/64 steady p95 9.037 → 4.964 s (burst 9.660 →
  7.302 s, drain 13.024 → 6.335 s); pending the steward's 1M certification**
  (§4.6). Sync now uses a durable collector-change frontier, affected-session
  replay and ordered quota-window extension. The default 300 s cadence and
  other lanes' work remain. Owner: accounting lane.
- **L3: lane and central metrics are not indexed aggregates.** Native cohort
  queries, as-of reads of stored revisions and paged exports meet 500 ms at
  1M. The lane metrics (M08/M09 derive the ledger again on every read, the
  tools and cost views, the central report with its per-decision M40) scan
  the history on each read: 2.3–8.4 s at 1M. Reading them from the
  analytics revisions (`--as-of-seq`, 159 ms) is the bounded path today.
  **P2 follow-up: 100k/64 M08 p95 1,073.07 → 27.91 ms, M13
  2,542.99 → 46.54 ms, report 2,443.80 → 308.27 ms and cost view
  752.10 → 133.76 ms; pending the steward's 1M certification** (§4.10).
  Maintained session/attempt/valuation summaries and validated provider
  bodies replace history replay on these reads. Full derivation remains the
  live fallback for missing/stale projections and the rebuild verifier.
  **P2b retains the 100k read target: M08/M13/report p95
  23.38/37.79/260.81 → 26.80/38.21/241.99 ms; pending the steward's
  1M certification** (§4.11; query load before 1.52 → 1.72, after
  1.37 → 1.30). No read-speedup claim on these shared-host samples.
  **P2d preserves Claude-equivalent maintained reads after DG4b. At
  100k/64, M08/M13/report/cost p95 56.74/105.53/254.19/98.79 →
  38.20/61.30/422.02/172.01 ms; refresh 2,219.66 → 885.00 ms,
  pending the steward's 1M certification** (§4.13, phase load averages
  included). Report/cost increased; no general speedup claim or L3 closure.
  **P2e preserves OpenCode/Gemini-equivalent maintained reads after DG4d.
  At 100k/64, M08/M13/report/cost p95 52.03/94.58/729.37/227.73 →
  66.41/93.83/413.84/158.43 ms; refresh 1,630.35 → 1,319.66 ms,
  pending the steward's 1M certification** (§4.14, with phase loads).
  M08 p95 increased; no general speedup or L3 closure is claimed.
  **DG1b removes M30's unconditional candidate history and second report
  lifecycle load (§4.15). At 100k/64, focused before → after report/M02 p95
  806.38/573.08 → 285.52/307.12 ms, at 1-minute loads 4.28 → 3.99 before
  and 3.75 → 3.53 after; pending the steward's serial 1M certification.**
  After is below main-pre (306.73/365.58 ms), but quieter main-post
  (242.37/282.08 ms) and the first busier full-phase M02 result prevent an
  unqualified no-regression/speedup claim. L3 remains open.
  Owners: accounting lane, analytics (TM4.1).
- **L4: the ticker's telemetry pass exceeds the 256 MiB envelope at 10,000
  bindings.** The collector itself stays within it (58–115 MB) and its byte
  caps hold (8 MiB per pass, 256 MiB per CLI collect). But the lane ticks in
  the same pass build whole-history structures in memory: `accounting sync`
  244 MB, `analytics refresh` 301 MB, `health evaluate` 227 MB (2.17 GB
  before F7), one pass process 294 MB at 1M. Doc 10 says an overrun blocks
  promotion to release: this blocks promoting the lane ticks at this scale,
  not collection. **Accounting P1 follow-up: 40.4 → 18.5 MiB peak RSS
  over three 100k/64 incremental-pass samples, pending the steward's 1M
  certification** (§4.6; load before 6.55–6.19, after 1.64–1.59). Full
  invalidation rebuilds remain; analytics and whole-pass RSS are not
  certified by that change. **Health P4 follow-up: 174.86 → 136.67 MiB
  maximum peak RSS over three 100k/64 samples (F12), pending the steward's
  1M certification (§4.7).** Timing is inconclusive at 14–19% CV; health load
  before `6.93 11.95 11.30` → `6.61 11.38 11.13`, after
  `5.88 10.98 11.01` → `5.59 10.50 10.85`. Neither change certifies
  whole-pass memory. Remaining remedy: incremental refresh (L2).

  invalidation rebuilds remain; analytics/health and whole-pass RSS are not
  certified by P1. **Analytics P2 follow-up: 276.4 → 180.0 MiB peak
  RSS across three 100k/64 refreshes including cache initialization; wall
  p50/p95 3,669.20/8,291.27 → 332.01/2,292.04 ms; pending the
  steward's 1M certification** (§4.10; load before 10.44–10.24, after
  5.18–5.33). Subsequent unchanged-input refreshes use 35.1–35.2 MiB.
  Health, whole-pass RSS and full invalidation/rebuild memory remain
  uncertified by P2. **P2b shortens uninterrupted analytics writer
  scopes at 100k from p50/max 206.83/209.63 to 6.73/14.78 ms;
  pending the steward's 1M certification** (§4.11; refresh load
  2.77 before / 2.50 after). Warm refresh p95/RSS increase from
  242.58 ms/35.4 MiB to 805.69 ms/75.9 MiB while restoring P3's
  omitted comparison work; this is a lock-hold fix, not a latency/RSS
  improvement. The 100k warm RSS remains below 256 MiB.
- **L5: the fleet pane and the digest section take seconds, not 250 ms / 100
  ms.** At 64 active attempts with 10,000 retained ones, one snapshot builds
  the full attempt projection three times (the active list, `compare`,
  `quality groups show`), the central report (M40 for every decision) and
  the accounting lane's report: 5.5 s p50 at 100k, 14.6 s at 1M, and the
  coordinator's `context` pays it too (15–17 s at 1M with views on, 3 ms
  with `[telemetry] views = false`). The digest stays bounded in size (13
  lines). Remedy: a snapshot over open attempts only, one projection shared
  by the three sections, and the recorded analytics revisions for the
  metrics. **P3 implements this remedy at 100k (§4.6): pane p50/p95
  6,643/9,426 → 63.69/96.56 ms; digest 5,589/10,323 → 63.88/94.36 ms,
  with recorded 1-minute loads 14.70 before / 3.86 after. Both 100k
  targets met in this run; pending the steward's 1M certification.**
  P3b's lifecycle correction retains the targets (§4.7): fresh 100k pane
  p95 75.03 → 53.62 ms, digest p95 70.23 → 53.44 ms, at recorded
  1-minute loads 5.39 → 1.91; pending the steward's serial 1M certification.
  P3c addresses the snapshot writer regression (§4.9): 100k analytics lock
  p50/max 36.17/61.96 → 13.70/14.96 ms, at 1-minute loads
  17.10 → 17.57 before / 5.09 → 9.40 after; current-store migration
  transactions 12 → 0. Pending the steward's 1M certification. The noisy
  P3c digest read still misses 100 ms; this does not close all of L5.
  P2b preserves these projections while reducing its regressed maximum
  uninterrupted refresh writer hold from 209.63 to 14.78 ms at 100k
  (§4.11; pending the steward's 1M certification).
  Original 1M measurements above remain the last certified ones.
  Owners: TM4.8, TM1.8, TM4.1.
- **L6: sidecar size.** The sidecar is 2.7–3.1 times the rollout bytes it
  reads (1.44 GB for 1M events) and grows without retention in this build.
  Retention and backup belong to TM5.3.
- **L7: workload gaps — addressed for produced signals at 100k by P6;
  pending the steward's 1M certification.** §2 declares the added 5,942 CI
  quality/proxy observations, 329 integration outcomes and 320 waiting-state
  samples with byte sizes. §4.6 records the same-dataset before/after surface
  costs, the slow/late collector and four-project fairness scenario. Memory
  timing and traces are honestly not produced because the product has no
  producer; live review/fix receipts, other adapters and another host remain
  outside scope. The new fairness gate fails its unchanged 5 s target in the first
  round (light p95 9.33–9.52 s), while later rounds are 2.42–2.90 s;
  shared-worker pass latency remains an L2 performance limitation. The shared workstation remains noisy;
  the 100k query comparison is inconclusive for performance changes, and this
  card makes no claim to fix L1–L6 or to certify live capacity.
- **L8: simulated capacity is not live capacity.** See the opening. Planted
  attempts and generated rollouts certify the telemetry path's behaviour at
  these volumes on this host, nothing about live workers or providers.

## 8. Verdict

Correctness holds at every scale and under every fault tried: exact totals,
one acceptance per record, reproducible as-of answers, byte-identical
rebuilds and no canonical write. Nine inefficiencies were fixed (§5), which
moved the surfaces from minutes to seconds. P3's subsequent 100k follow-up
(§4.6) meets the pane/digest targets; its 1M certification is pending.
At the original certified scales, the controller-overhead, freshness,
pane/digest and lane-tick memory targets were missed; each
is a reviewed limitation (L1–L5) with a named remedy and owner, and none of
them is a correctness or authority problem. The native, revision and export
read paths, the collector's own resources and byte caps, replay integrity
and the digest's size bound meet their targets.

Exchange with factory F4.6: the factory's simulation may cite this
certificate as evidence for the telemetry path (its overhead and its
correctness under load), and this certificate cites none of the factory's.
Neither release depends on the other.

## 9. Tests and changed expectations

- `tests/telemetry_scale.rs`: the CI gate test `scale_gates_hold_under_load`
  and the ignored bench phases.
- `tests/telemetry.rs` `sidecar_streams_upgrade_v2_store` now expects the
  `codex` stream at 3 (F1), and first returns the sidecar to a true v2 state
  (no streams table, no 0003 indexes, `user_version` 2).
- `tests/telemetry_accounting.rs`: `attention_intervals_union_and_censor`
  and the re-run check of accounting stream 10 now expect the stream at 11
  (F2).
- `tests/telemetry_query.rs` `hot_queries_use_indexes` is unchanged; `analytics
  plans` now accepts `verified_results` as the acceptance join's one-pass
  driver (F3).

Suite results (`--features state-store`, debug build, `-j 3`,
`RUST_TEST_THREADS=6`, `--no-fail-fast`): `cli` 81 passed (1 ignored),
`migration` 6, `telemetry` 20, `telemetry_accounting` 21,
`telemetry_certification` 13, `telemetry_collect` 4, `telemetry_conformance`
20, `telemetry_health` 13, `telemetry_query` 10, `telemetry_views` 7,
`telemetry_workspace` 6, `telemetry_quality` 6, `telemetry_review` 20,
`telemetry_compare` 5, `telemetry_export` 6, `telemetry_scale` 1 (the seven
bench phases ignored). The first run failed once, on the accounting stream
version expectation above; after that change every suite passed. No new
clippy warning in the changed files.

P6 validation: the required fifteen telemetry test targets ran with all
pre-existing expected values unchanged (`--locked --offline -j 3`, one test thread): 166 passed, five failed, nine
bench tests ignored. Four failures are sandbox-only Unix-socket bind denials
(`Operation not permitted`): `telemetry::attempts_show_attention_summary`,
`telemetry_accounting::attention_intervals_union_and_censor`,
`telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`,
and `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`.
The fifth, `telemetry_health::ticker_health_requires_operator_opt_in_obeys_interval_and_never_notifies`,
missed an asynchronous health evaluation in the full run and passed alone
in 2.14 s without changes. Both scale workflows passed (49.46 s combined
in debug, below the 60 s gate budget); after settling prior unavailable
producer observations was covered, both passed again in 37.24 s combined.
Clippy (`cargo clippy --locked --offline -j 3 --features state-store --test
telemetry_scale`) reported no warning in changed lines; existing library and
shared-support warnings remain. No expected value in another telemetry suite
was edited. The ignored fairness phase fails only its documented performance
criterion; its correctness oracles and the late/slow phase pass.

P3 follow-up verification: all 15 requested telemetry suites were run with
`--locked --offline -j 3 --features state-store --no-fail-fast` and three
test threads. After correcting the new fixture's retry limit and hand-counted
coverage alert, 169 tests pass, seven scale phases remain ignored, and only
these four existing tests fail because their Unix socket bind returns
`Operation not permitted` in the hard sandbox (no workaround):

- `telemetry::attempts_show_attention_summary`
- `telemetry_accounting::attention_intervals_union_and_censor`
- `telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`
- `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`

`scale_gates_hold_under_load` passes unchanged, including its exact totals,
one acceptance per record, pinned as-of answer, byte-identical rebuild and
canonical digest. Existing metric suites keep their expected values.
Workspace fixtures now refresh analytics to record the history used by the
same golden values; only `as_of` fields and the provenance line are added.
New CLI/store E2E coverage checks 31 retained cancelled attempts (the public
32-attempt-per-task limit), exact live waits/needs-you ordering, pinned history
until refresh, missing revisions, and terminal candidate arms in both pane
and digest. Clippy reports zero warnings in changed lines (existing unrelated
warnings remain). Datasets were removed before committing.

P3b verification: all 15 requested suites ran with `--locked --offline -j 3
--features state-store --no-fail-fast` and three test threads. The unchanged
`scale_gates_hold_under_load` passed twice. The final full run plus focused
reruns establishes 169 passing tests, seven ignored scale phases, and the
same four socket-only failures listed above. The first run's collector-create
race (`File exists`) passed in the final full run. The ticker-health timing
check failed in both full runs and passed in isolation; it is recorded as a
flaky result, not a socket failure. The new operations fixture's intermediate
assertions were corrected; its final complete suite passes all 12 tests.
Clippy has no warnings in changed lines; unrelated existing warnings remain.
