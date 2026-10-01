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
| Stores | canonical `SCHEMA = 68`; sidecar streams `codex` 4, `ingest` 12, `accounting` 17, `quality` 4, `analytics` 4, `health` 1, `policies` 1, `otlp` 2 |
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
### 4.15 DG2: maintained M10 cache-read share (100k only)

Branch `telemetry/dg2-m10`, accounting stream **16**, registry
**analytics-registry.v5**. M10.v1 now reads accepted cache-read/input token
sums from maintained session aggregates. This extends the P2 read path
addressing L3; it does not close L3 or certify 1M. **Pending the steward's
serial 1M certification.** No 1M run was performed.

Built before and after with the §1 release command. Both measurements use
one unchanged dataset under `$PWD/bench-data/dg2`, seed 5100,
`SCALE_EVENTS=100000 SCALE_ACTIVE=64 SCALE_REPEATS=3`; queries also use
`SCALE_PER_ROUND=1`. Run §1's `scale_0_generate`, `scale_1_ingest`, then
`scale_2_queries` and `scale_9_analytics_refresh` with `SCALE_TAG=dg2-before`.
After the change, settle the same dataset through public `accounting sync`
and `analytics refresh`, then repeat the last two phases with
`SCALE_TAG=dg2-after`. One bench process at a time; no concurrent builds or
tests. The raw dataset and canonical state remain unchanged. Generated
observations: **106,517**, rollout lines **99,926**, logical bytes
**58,084,536** (rollouts **53,725,166**); 10,000 attempts, 64 active, eight
synthetic execution homes. The dataset is removed before committing.

| Surface, wall p50 / p95 (ms), n=3 | Before | After |
| --- | --- | --- |
| M08 lane query | 27.05 / 31.78 | 24.49 / 29.81 |
| M13 coverage query | 51.94 / 53.63 | 40.15 / 41.74 |
| Report (now includes M10) | 277.51 / 428.52 | 269.08 / 270.09 |
| Cost view | 107.96 / 194.39 | 105.31 / 105.62 |
| Analytics refresh | 933.44 / 952.43 | 893.16 / 911.94 |
| Refresh peak RSS (KiB) | 78,600 | 78,276 |

Load averages (1 / 5 / 15 minute), directly from phase results files:
queries before **3.99 / 4.11 / 3.47 → 4.08 / 4.11 / 3.48**, after
**4.29 / 4.23 / 3.82 → 3.87 / 4.13 / 3.80**; refresh before
**4.04 / 4.11 / 3.49 → 3.88 / 4.08 / 3.48**, after
**3.64 / 4.08 / 3.78 → 3.50 / 4.05 / 3.77**. These shared-host samples
establish bounded producer overhead, not a general speedup claim. M10 had
no producer before, so there is no comparable pre-change M10 latency.

The new M10 query returns **50,862,666 / 202,285,731**, matching the
hand-independent generator totals, with **19,800** accepted records and
**990** certified sessions. Refresh results show `violations: []` and
`canonical_unchanged: true` before and after. The unchanged
`scale_gates_hold_under_load` passes, including exact totals, one acceptance
per record, reproducible as-of results, byte-identical rebuild and untouched
canonical digest. New CLI E2E workflows prove mixed Codex/Claude/OpenCode
**470/1522**, writes **42**, exclusion reasons, frozen-configuration ratios,
known zero versus unknown, late restatement and pinned as-of results.
Gemini remains excluded because its message-update/SDK observations have
no reconciled additive ledger denominator; this is an explicit coverage
reason, never a fabricated zero. Non-Codex certification remains fixture-only.
See contracts-accounting §17 and phase2-lanes DG2.

The requested telemetry suites plus Claude/OpenCode/Gemini suites completed
with **202 passed, 12 ignored**, and four failures only at sandbox-denied
Unix-socket bind (`Operation not permitted`):
`telemetry::attempts_show_attention_summary`,
`telemetry_accounting::attention_intervals_union_and_censor`,
`telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`,
and `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`.
The steward must run these outside the sandbox. Existing metric and coverage
expectations are unchanged; only accounting stream and registry pins advance.
Focused final M10 workflows also pass. Clippy (`--locked --offline -j 3`,
state-store, changed test targets plus scale) has no warnings in changed lines;
pre-existing unrelated diagnostics remain.
### 4.15 P5 / L6 follow-up: lossless sidecar storage (100k only)

Branch `perf/sidecar-size`, 2026-10-01. **Pending the steward's serial 1M
certification.** Doc 10 sets no numeric storage target; the steward requires
at least halving sidecar bytes per collected rollout byte. This cold-ingest
comparison meets that target by **50.101%**. No 1M run or live service was used.

Build and phases use §1's commands with `SCALE_EVENTS=100000 SCALE_ACTIVE=64
SCALE_REPEATS=3`: release `--locked --offline -j 3`, then `scale_0_generate`
and `scale_1_ingest`, one process at a time. The latter produces **one cold
sample**, regardless of `SCALE_REPEATS`; these are not three-sample timing
percentiles. Both runs restore the same initial fixture at the same absolute
paths, including canonical bytes and source contents. Phase 0 replants the
same simulated quality/attention facts. The 64 active files' mtimes are
refreshed before each run to reproduce their initial freshness after the long
builds; native event timestamps and contents are unchanged. Every dataset,
copy and SQLite measurement lives on disk under `bench-data/`, removed before
commit. The host is the shared i7-8750H / Linux 7.2.5-3-omarchy workstation.

| `scale_1_ingest`, 100k/64 | Before | After |
| --- | --- | --- |
| Rollout bytes actually read | 53,714,270 | 53,714,270 |
| Sidecar database bytes | 209,563,648 | 104,570,880 |
| Sidecar WAL bytes at measurement | 0 | 0 |
| Sidecar bytes / rollout bytes | 3.901452× | 1.946799× |
| Collection wall time / peak RSS | 24,225.53 ms / 60,980 KiB | 23,498.86 ms / 60,788 KiB |
| Accounting sync wall time / peak RSS | 3,661.90 ms / 58,468 KiB | 5,098.86 ms / 61,372 KiB |
| Analytics refresh wall time / peak RSS | 2,736.18 ms / 210,052 KiB | 3,187.65 ms / 192,452 KiB |
| Health evaluation wall time / peak RSS | 2,345.97 ms / 88,484 KiB | 2,721.56 ms / 88,928 KiB |
| Load averages (1 / 5 / 15 min), start | 4.83 / 4.22 / 3.78 | 4.78 / 4.25 / 3.74 |
| Load averages (1 / 5 / 15 min), end | 4.54 / 4.24 / 3.81 | 4.54 / 4.25 / 3.77 |

Loads differ, so timings are observations, not a certified speedup. Both
results report zero violations, unchanged canonical digests and the same
19,800 accepted usage records: input 202,285,731; cached 50,862,666; output
20,258,435; reasoning 5,062,106. The size target uses the physical file, not
logical payload sums or an estimated compressed filesystem allocation.

Measured with `SELECT name,sum(pgsize) FROM dbstat GROUP BY name ORDER BY 2
DESC` (bytes; indexes listed separately):

| Consumer | Before | After |
| --- | --- | --- |
| Envelope table / compact rows | 73,175,040 | 23,732,224 |
| Envelope event-id index | 8,908,800 | 0 (derived identity) |
| Envelope epoch/sequence index | 7,991,296 | 0 (compact primary key) |
| Envelope string dictionary / unique index | — | 200,704 / 208,896 |
| Shared sanitized payload dictionary / unique index | — | 12,288 / 12,288 |
| Lineage table / compact rows | 18,649,088 | 2,449,408 |
| Lineage value dictionary / entity lookup index | — | 3,002,368 / 471,040 |
| Provider aggregate cache / compact rows | 9,486,336 | 20,480 |
| Dispositions table | 2,953,216 | 3,010,560 |
| Dispositions duplicate primary-key index | 2,805,760 | 0 |
| Native response lookup index | 2,859,008 | 2,048,000 |
| Dispatch headroom (unchanged) | 10,240,000 | 10,240,000 |
| Immutable analytics revisions (unchanged) | 9,916,416 | 9,916,416 |
| Accounting usage entries (unchanged) | 6,668,288 | 6,668,288 |
| Native usage table (unchanged) | 5,087,232 | 5,087,232 |
| Quota observations (unchanged) | 4,816,896 | 4,816,896 |

The envelope and its extracted native facts are **both still kept**:
contracts-collection §3/§5 requires sanitized reported evidence, including
uncertified fields that extraction cannot replace. Full payload JSON, headers,
timestamps, certification, original digests and `envelope_bytes` reconstruct
exactly through `source_observations`. Repeated headers and four small Codex
payload kinds share exact strings; canonical SHA-256 text uses binary bytes
only when losslessly reversible. Noncanonical digests and exceptional event
IDs stay inline. Native tables without a public rowid and dispositions use
`WITHOUT ROWID`; `codex_usage` retains its rowid watermarks. Lineage shares
exact `(entity_kind,entity_id,attrs)` values, keeping ordinals and immutable
revision bodies. Provider caches reference an immutable M40 byte range only
after byte equality; revision expiry evicts that disposable cache, allowing
the existing source evaluator fallback. Legacy cache bodies migrate unchanged,
even with different JSON whitespace. No metric, coverage or as-of rule changes.

Stream versions advance to `codex` 4 / `ingest` 12 / `accounting` 17 /
`analytics` 4. SQL `INSERT ... SELECT` copies rows in-place within the existing
atomic migration transaction, using SQLite's bounded pager/temp storage rather
than loading histories into Rust memory. Per-collection interning caches cap at
128 entries each. Native-table rebuilds preserve accounting 0012 incremental
triggers; analytics input-frontier triggers reinstall on the physical tables.

`EXPLAIN QUERY PLAN` preserves the indexed paths: epoch/sequence becomes
string-dictionary lookup plus compact primary key; lineage page becomes its
primary key plus integer value lookup; response exclusion still searches
`codex_usage_by_response` by session, response, binary digest, acceptance and
ordinal, then checks the original digest text. By-path, mixed-turn, quota and
as-of indexes remain. No useful secondary index was discarded; removed indexes
are duplicate rowid/primary-key structures or replaced identity access paths.

**Retention and page reclamation.** TM5.3 `retention.v1` already defaults to
90 days for eligible terminal native sessions and attention, 90 days for
health (also its existing newest-1,000 cap), and 365 days for superseded
analytics revisions. Applying destructive retention still requires the
operator's `maintenance plan` / `maintenance apply --confirm <digest>`.
P5 does **not** silently enable automatic destructive expiry. Holds, active
attempts, unresolved accounting, latest analytics revisions, valuation/import
history and tombstones can keep data indefinitely; defaults alone do not give
a universal finite database bound. For a stable eligible-session arrival rate
R, the native portion retains approximately 90 × R daily bytes plus active and
held histories; retained canonical-derived/latest and valuation history is
additional. L6's automatic-growth requirement therefore remains an operational
limitation, reported rather than changing the approved plan.

On an isolated copy with only `rollout_sources.observed_unix_ms` aged by 91 days,
the real default plan expires 926 terminal sessions and protects all 64 active
ones. Approved apply deletes their native/accounting rows and unreferenced
interned values without deleting rollout files. After reclaiming all free
pages in bounded 128-page passes, this fixture occupies **38,195,200 bytes**
with 64 sources remaining; the retained latest revisions/headroom/quality
still account for most of that floor. This is a finite-fixture steady-state
observation, not a bound for indefinitely accumulating canonical history.

New empty sidecars use `auto_vacuum=INCREMENTAL` (mode 2). Existing mode-NONE
stores retain their setting; enabling it requires an operator-scheduled
`PRAGMA auto_vacuum=INCREMENTAL; VACUUM;`, an exclusive file rewrite and
adequate disk space. Measured on a baseline copy, that rewrite alone shrinks
209,563,648 → 193,880,064 bytes (2.27 s), far short of halving. A compact
copy's optional `VACUUM` shrinks 104,570,880 → 101,269,504 bytes (1.54 s);
this extra compaction is excluded from the cold-ingest target. Confirmed
maintenance drains `incremental_vacuum(128)` outside deletion transactions,
reclaiming at most 128 free pages per pass. The final isolated retention copy
measures 104,656,896 bytes before apply → 104,132,608 after apply, exactly
128 × 4,096 bytes reclaimed; 16,078 free pages remain reusable. Another 126
bounded passes drain them and pointer-map pages to the 38,195,200-byte floor
(1.24 s), with canonical bytes unchanged. Unreclaimed pages remain reusable; dictionaries follow
surviving references instead of retaining deleted session metadata forever.

The unchanged load gate, exact telemetry goldens and migration/late-collect/
pinned-query/verified-rebuild E2E workflow validate the logical equivalence.
The E2E upgrade compares every envelope column, lineage row, full cache body
and ledger entry before/after public-store migration, then collects a late
session, checks the original pinned answer and verifies stored digests/rebuilds.
The retention E2E additionally checks incremental-vacuum mode and dictionary
cleanup. No unit/source-text tests, crates or process spawns were added.

Validation command: `cargo test --locked --offline -j 3 --features state-store
--no-fail-fast --test telemetry --test telemetry_accounting --test
telemetry_certification --test telemetry_collect --test telemetry_conformance
--test telemetry_health --test telemetry_query --test telemetry_views --test
telemetry_workspace --test telemetry_quality --test telemetry_review --test
telemetry_compare --test telemetry_export --test telemetry_scale --test
telemetry_operations` with `RUST_TEST_THREADS=1`; the touched Claude suite is
also run. The requested suites pass 180 tests, including the unchanged
`scale_gates_hold_under_load`, with four sandbox-only Unix-socket bind failures;
Claude adds eight passes. The socket-only failures are
`telemetry::attempts_show_attention_summary`,
`telemetry_accounting::attention_intervals_union_and_censor`,
`telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`,
and `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`
(each `Operation not permitted`; no sandbox workaround). Additional Gemini,
OpenCode, OTLP and routines suites add 20 passes; OTLP's
`http_auth_limits_malformed_and_replay` and `http_request_rate_is_bounded`
fail only because TCP loopback bind also returns `Operation not permitted`.
These are separate from the four requested-suite Unix-socket failures.
The final release load gate also passes unchanged (11.34 s).
Clippy with
`--locked --offline -j 3 --features state-store` and the touched test targets
reports no warning in changed lines; existing warnings remain elsewhere.

Files: `migrations/telemetry/0004_compact_native.sql`,
`migrations/telemetry/ingest/0012_compact_envelopes.sql`,
`migrations/telemetry/accounting/0017_compact_dispositions.sql`,
`migrations/telemetry/analytics/0004_compact_lineage.sql`;
`src/telemetry/{sidecar,metrics}.rs`, `ingest/mod.rs`, `collectors/mod.rs`,
`accounting/{mod,ledger}.rs`, `analytics/{mod,compact,store}.rs`,
`maintenance/{mod,backup}.rs` (all relative to `src/telemetry/`);
`tests/{telemetry,telemetry_accounting,telemetry_claude,telemetry_collect,
telemetry_conformance,telemetry_operations}.rs`; this certificate,
`contracts.md`, `contracts-collection.md`, `contracts-analytics.md` and
`operations-runbook.md` (relative to `docs/telemetry/`). The scale harness,
including its load-gate body, is unchanged.

### 4.16 P5b: preserve disposition invalidation after compaction (100k only)

Branch `perf/sidecar-size`, following the steward's rebase onto DG2/P2 main.
**Pending the steward's serial 1M certification.** Accounting remains stream
**17**; DG2 owns `0016_cache_read_share.sql`, and compaction is migration
`0017_compact_dispositions.sql`. No 1M run was performed.

Root cause: `DROP TABLE usage_dispositions` in 0017 also dropped 0012's
`accounting_dispositions_update` trigger. The operations workflow changes one
accepted disposition to unresolved after a completed sync. Without that
trigger, `accounting_stream.invalidated` stays null and no source session is
dirty. DG2's cache aggregates are complete, so `prepare_sync` selects an empty
incremental replay instead of rebuilding. The disposition row is **not lost**:
it remains unresolved, and retention correctly blocks the session, giving
eligible_count 0 instead of 1. P2's generic analytics generation triggers are
reinstalled, but they validate concurrent snapshots rather than force this
ledger replay. Neither DG2's cache evaluator nor a compacted-view DELETE
removes the row.

0017 now recreates the exact original AFTER UPDATE trigger after renaming the
compact table. An external correction again sets `projection_changed`; sync
replays the original native facts and atomically replaces ledger, graph, quota
and maintained aggregates, including DG2 cache totals. No derivation, metric,
coverage or as-of rule changes. The blocking operations test and scale gate
are unchanged; only the two remaining accounting-version pins advance 16 → 17.
No new tests, crates or source process spawns were added.

Both release builds used §1's exact locked/offline `-j 3` no-run command.
Phases 0/1 used `SCALE_EVENTS=100000 SCALE_ACTIVE=64 SCALE_REPEATS=3`, one
bench process at a time, without an overlapping build or test. The same
seed-5100 initial fixture was saved before baseline ingest and restored at
identical absolute paths before after ingest; source and canonical bytes
are identical. Active rollout mtimes were refreshed after the release build
to preserve initial freshness. All fixtures, copies and datasets remained
under `$PWD/bench-data/` on disk. Phase 1 produces one cold sample regardless
of `SCALE_REPEATS`; timings are not three-sample percentiles.

| Cold ingest, 100k/64 | Before P5b | After P5b |
| --- | --- | --- |
| Rollout bytes | 53,714,262 | 53,714,262 |
| Sidecar database / WAL bytes | 105,037,824 / 0 | 105,037,824 / 0 |
| Collection wall time | 16,368.81 ms | 16,777.20 ms |
| Accounting sync wall time / peak RSS | 2,993.93 ms / 59,460 KiB | 3,244.55 ms / 61,212 KiB |
| Analytics refresh wall time / peak RSS | 2,826.11 ms / 202,924 KiB | 2,796.37 ms / 202,588 KiB |
| Health evaluation wall time | 2,233.79 ms | 2,382.79 ms |
| Loadavg start (1 / 5 / 15 min) | 5.26 / 5.09 / 5.65 | 2.51 / 3.73 / 4.90 |
| Loadavg end (1 / 5 / 15 min) | 4.16 / 4.84 / 5.55 | 1.91 / 3.47 / 4.77 |

These `results-prepare-before.json` / `results-prepare.json` measurements
preserve L6's compact size exactly; they compare the trigger repair, not the
original storage halving. No timing speedup is claimed on the noisy shared
host. Both phases report `violations: []`, `canonical_unchanged: true`, and
identical 19,800 accepted records: input 202,285,731, cached 50,862,666, output
20,258,435 and reasoning 5,062,106. The earlier P5 size comparison remains
provisional until the steward's serial 1M certification.

Validation used `TMPDIR=$PWD/target/tmp`, `RUST_TEST_THREADS=1` and
`cargo test --locked --offline -j 3 --features state-store --no-fail-fast`.
The requested operations/accounting/Claude/scale command passed **51 tests**,
with **12 ignored** and only the accounting Unix-bind failure below. The
subsequent run of every **20** telemetry suite passed **212 tests**, with
**12 ignored** and **six socket-only failures**. The unchanged operations
regression and `scale_gates_hold_under_load` pass in both runs, retaining
exact totals, one acceptance, reproducible as-of answers, byte-identical
rebuild and the canonical digest. No other expected value was edited.

Unix socket binds return `Operation not permitted` in:

- `telemetry::attempts_show_attention_summary`
- `telemetry_accounting::attention_intervals_union_and_censor`
- `telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`
- `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`

TCP loopback binds return the same sandbox denial in
`telemetry_otlp::http_auth_limits_malformed_and_replay` and
`telemetry_otlp::http_request_rate_is_bounded`. No workaround was attempted.
Clippy checks the operations/accounting/Claude targets plus scale with the same
locked/offline `-j 3` flags; no warning falls on changed lines. Benchmark
datasets are removed before commit. Files: accounting migration 0017,
`tests/telemetry_accounting.rs`, `tests/telemetry_claude.rs`, and this certificate.

### 4.17 P5c: recover capture after compact table replacement (100k only)

Branch `perf/sidecar-size`, 2026-10-01. **Pending the steward's serial 1M
certification.** Stream versions and every existing metric/coverage expectation
are unchanged. This repairs the accounting frontier underlying P1's incremental
sync while preserving L6's compact size; it closes no freshness or 1M target.

`DROP TABLE` removes its triggers. The migration runner already preserves
surviving trigger SQL around native rebuilds, but cannot recover triggers lost
by an earlier upgrade once the stream versions are current. P5c checks the
22 required compact-table capture names in one read of `sqlite_master`, even
on the current-store fast path. Missing capture is restored atomically from
0012's exact trigger definitions, and `capture_repaired` requests one full
replay: mutations committed without capture are absent from the dirty queue.
The trigger-preservation selector also now follows accounting compaction at
**17**, rather than DG2's **16**. Intact current stores still take no migration
write transaction. No new stream, table, crate or source process spawn.

The complete restored trigger inventory is:

| Backing table | Restored triggers |
| --- | --- |
| `codex_turns` | `accounting_codex_turns_insert`, `accounting_codex_turns_update`, `accounting_codex_turns_delete` |
| `codex_rate_limits` | `accounting_codex_rate_limits_insert`, `accounting_codex_rate_limits_update`, `accounting_codex_rate_limits_delete`, `accounting_quota_limits_update` |
| `codex_rate_limit_windows` | `accounting_codex_rate_limit_windows_insert`, `accounting_codex_rate_limit_windows_update`, `accounting_codex_rate_limit_windows_delete`, `accounting_quota_secondary_insert`, `accounting_quota_secondary_update` |
| `rollout_metadata` | `accounting_rollout_metadata_insert`, `accounting_rollout_metadata_update`, `accounting_rollout_metadata_delete` |
| `rollout_threads` | `accounting_rollout_threads_insert`, `accounting_rollout_threads_update`, `accounting_rollout_threads_delete` |
| `rollout_forks` | `accounting_rollout_forks_insert`, `accounting_rollout_forks_update`, `accounting_rollout_forks_delete` |
| `usage_dispositions` | `accounting_dispositions_update` (also repairs stores predating P5b) |

Sequence increments, old/new dirty session selection, native-delete
invalidation, projection invalidation and quota rebuild conditions are copied
unchanged. In particular, metadata/thread/fork deletes retain main's dirty/sequence
behavior without adding native-delete invalidation. The secondary INSERT
`WHEN EXISTS` and untouched source quota `WHEN` conditions retain their exact
semantics. No arithmetic, normalization, coverage or as-of selector changes.

The audit covers every DROP/RENAME/view replacement in all four compaction
migrations. Native 0004 rebuilds the first two tables above and replaces only
an index on `codex_usage`. Ingest 0012 also rebuilds `codex_usage_times`,
`codex_tool_calls`, `codex_exec_items`, `codex_turn_aborts`, `codex_mcp_calls`,
`codex_agent_items`, `codex_tool_namespaces`, `rollout_subagents`,
`rollout_ingest_state`, `rollout_turn_ends`, `rollout_turn_terminations`,
`codex_tool_sources` and `source_bindings`; these have analytics capture,
not 0012 accounting capture. Accounting 0017 rebuilds dispositions. Analytics
0004 replaces `analytics_lineage` and `analytics_provider_aggregates` with
views over `analytics_lineage_rows`/`analytics_lineage_values` and
`analytics_provider_rows`; analytics-owned tables are intentionally excluded
from the input frontier. Ingest's `source_observations` becomes a view over
`source_observation_rows`, `source_observation_strings` and
`source_observation_payloads`. Its INSTEAD OF writes reach those physical
tables, which P2's schema-version installation discovers and captures.
No accounting-tracked table becomes a view in these migrations.

The new public CLI E2E reproduces a current compact sidecar with missing
capture, including a projection correction missed while capture was absent.
Sync repairs it and replays the original facts. The sidecar's own
`sqlite_master` guards all twelve original accounting-tracked tables and all
five quota triggers, and every eligible physical table's three analytics
input triggers. A zero-usage child session is established first (a brand-new
session intentionally uses DG2's missing-cache full replay). The workflow then
appends a completed turn, primary rate snapshot, secondary window, fork,
metadata and thread header to a resumed rollout, collects, and explicitly
requires incremental sync. Ledger, sessions, quota bytes and the stable
accounting status frontier equal a forced full replay; mode/rebuild reason
appropriately identify the different paths. Independent persisted updates
and deletes check each affected table's dirty session and exact sequence
increment, then run public sync. The first secondary insertion leaves replay
off; a replacement after its primary observation exists sets replay on.
The envelope backing-row input generation advances on actual collection.
No unit or source-text tests were added; the scale gate is unchanged.

Both release builds use §1's exact locked/offline `-j 3` no-run command.
Phases 0/1 prepare the baseline, followed by `scale_8_accounting_pass` with
`SCALE_EVENTS=100000 SCALE_ACTIVE=64 SCALE_REPEATS=3 SCALE_TAG=p5c-before`.
After the change, restore the archived initial fixture at the same absolute
paths (canonical/source bytes and generator state unchanged), refresh only
the 64 active files' mtimes, then repeat phases 1/8 with `p5c-after`.
Phase 8 appends the same deterministic counter/event mix, with its usual live
wall-clock timestamps. All datasets and copies remain under `$PWD/bench-data/`;
one bench process at a time, no overlapping build/test. No 1M run.

| Cold ingest, one sample per version, 100k/64 | Before | After |
| --- | --- | --- |
| Rollout bytes | 53,714,281 | 53,714,281 |
| Sidecar database / WAL bytes | 105,041,920 / 0 | 105,041,920 / 0 |
| Collection wall time | 22,538.43 ms | 16,284.29 ms |
| Accounting sync wall time / peak RSS | 3,898.61 ms / 60,896 KiB | 3,106.76 ms / 60,812 KiB |
| Analytics refresh wall time / peak RSS | 3,552.06 ms / 203,192 KiB | 2,783.45 ms / 202,664 KiB |
| Health evaluation wall time / peak RSS | 4,275.39 ms / 89,532 KiB | 2,242.21 ms / 89,404 KiB |
| Loadavg start (1 / 5 / 15 min) | 4.31 / 4.26 / 4.24 | 4.90 / 2.96 / 3.15 |
| Loadavg end (1 / 5 / 15 min) | 4.11 / 4.20 / 4.22 | 3.57 / 2.80 / 3.10 |

| Incremental accounting, three appended-data samples | Before | After |
| --- | --- | --- |
| Sync wall p50 / p95 | 317.42 / 340.42 ms | 250.84 / 252.38 ms |
| Peak RSS | 30,184 KiB | 29,988 KiB |
| Loadavg start (1 / 5 / 15 min) | 4.10 / 4.20 / 4.22 | 2.85 / 2.68 / 3.05 |
| Loadavg end (1 / 5 / 15 min) | 4.09 / 4.19 / 4.22 | 2.94 / 2.70 / 3.06 |

All four phase results report `violations: []` and
`canonical_unchanged: true`. Cold totals remain 19,800 accepted records:
input 202,285,731, cached 50,862,666, output 20,258,435, reasoning 5,062,106.
L6's compact bytes are preserved exactly on this intact fixture. The after
host is quieter over the accounting samples; **no timing speedup or RSS
improvement is claimed**, and damaged-store recovery pays one full replay.

Validation uses `TMPDIR=$PWD/target/tmp`, `RUST_TEST_THREADS=1` and the exact
locked/offline `-j 3` commands. The fifteen requested suites pass **184 tests**
with **12 ignored**; all twenty telemetry suites pass **213 tests**, with
**12 ignored** and only **six socket-only failures**. The four Unix-socket
bind denials (`Operation not permitted`) are:

- `telemetry::attempts_show_attention_summary`
- `telemetry_accounting::attention_intervals_union_and_censor`
- `telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`
- `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`

The additional TCP loopback bind denials are
`telemetry_otlp::http_auth_limits_malformed_and_replay` and
`telemetry_otlp::http_request_rate_is_bounded`. No workaround or expectation
change. The unchanged debug load gate passes all exact-total, single-acceptance,
as-of, byte-identical rebuild and canonical-digest checks. Two final unchanged
release gate runs pass in **11.61 s** and **11.65 s**. An initial additional
release run failed with SQLite `IOERR_WRITE` (778, error writing to disk);
it is recorded separately from socket failures, without assigning an
unconfirmed cause or changing the gate. Clippy on accounting and scale reports
zero warnings on changed lines (97 unrelated diagnostics).
Benchmark data is removed before commit. Files: `src/telemetry/sidecar.rs`,
`migrations/telemetry/accounting/repair_compact_capture.sql`,
`tests/telemetry_accounting.rs`, and this certificate.

### 4.18 DG3: observed operating hours and M03 (100k only)

Branch `telemetry/dg3-m03`, base `663e7aa`, 2026-10-01. This addresses the
operating-hours producer gap in L7. **Pending the steward's serial 1M
certification.** No 1M run, live agent, provider, owner data directory or running
Herdr service was used. Registry v4 adds `M03.operating-v1`; operating stream 1
retains interval/clock/gap source facts and includes them in backup inventories.

The original DG3 worker observed every canonical project on each available
ticker pass (nominal 15 s), including paused projects and when collection was
disabled. **This scheduling/opt-in contract is superseded by DG3b in §4.19.**
Consecutive Active endpoints in one run/control epoch merge. Pause/resume,
restart or a gap longer than **N=3** passes closes the previous prefix; open
tails are censored and never extrapolated. M03 clips and unions integer-ms
intervals and counts each currently authoritative task once at its original
acceptance time. Later receipts/corrections are not new work. The exact ratio
is `accepted * 3600000 / operating_ms`; zero or unknown hours remain null with
a reason. Reports use fresh M03 separately from maintained central bodies so
operating heartbeats neither stale M03 nor invalidate M13/M40 caches.
Configuration comparisons remain unsupported because project hours have no
observed attribution to configuration arms.

**Reproduction and scope.** Built before and after with
`cargo test --release --locked --offline -j 3 --features state-store --test telemetry_scale --no-run`.
One deterministic disk-backed dataset lived entirely under `$PWD/bench-data/`
(including `TMPDIR=$PWD/bench-data/tmp`), with `SCALE_EVENTS=100000`,
`SCALE_ACTIVE=64`, `SCALE_REPEATS=3`, `SCALE_PER_ROUND=1`, seed 5100 and 10,000
canonical tasks. Generate and ingest ran once: 106,517 observations, 58,084,538
logical fixture bytes (53,725,168 rollout bytes). Preserved pre-change binaries
and rebuilt post-change binaries ran `scale_2_queries` then
`scale_9_analytics_refresh` serially with `SCALE_TAG=dg3-before|dg3-after`.
Post-change migration/cache warming occurred outside timed samples; canonical
rows and rollouts stayed identical. These paired measurements deliberately
precede adding operating observations, so they measure compatibility and
no-observation overhead, not an M03 speedup. One bench process ran at a time;
cargo builds and other tests did not overlap measurement.

| Surface, ms (p50 / p95) | Before | After |
| --- | ---: | ---: |
| Report | 438.64 / 466.51 | 310.35 / 403.28 |
| M02 terminal cohort | 464.83 / 560.60 | 345.33 / 383.07 |
| M08 lane usage | 57.95 / 59.70 | 30.40 / 45.42 |
| M13 coverage | 77.33 / 127.77 | 51.26 / 62.63 |
| Pane, in process | 111.12 / 139.26 | 85.47 / 128.78 |
| Digest section, in process | 86.62 / 90.20 | 68.76 / 85.25 |
| Analytics refresh | 915.78 / 1030.92 | 970.30 / 986.76 |

All four results files quote these load averages (1 / 5 / 15 minutes):

| Phase | Start | End |
| --- | --- | --- |
| Queries before | 6.49 / 6.96 / 5.45 | 6.79 / 6.95 / 5.51 |
| Queries after | 5.06 / 5.89 / 6.71 | 4.86 / 5.75 / 6.63 |
| Refresh before | 7.45 / 7.11 / 5.61 | 7.45 / 7.11 / 5.61 |
| Refresh after | 4.74 / 5.69 / 6.60 | 4.76 / 5.68 / 6.59 |

Refresh peak RSS was 80,320 → 79,240 KiB; both phases had
zero usage-gate violations and unchanged canonical digests. The after query
run had lower short-term load, so faster surface samples are not evidence of
a causal speedup. Refresh p50 increased while p95 decreased. At 100k, after
M02/report p95s are below doc 10's provisional 500 ms aggregate target, and
pane/digest are below their 250/100 ms targets. This is three samples, not
doc 10's five repeated load runs or authoritative 1M evidence; existing L3/L4
remain open. The M03 change does not claim to fix those costs.

**Produced operating signal.** After the paired run,
`scale_10_operating_throughput` uses the public producer API to plant a single
observed interval spanning the fixture's accepted transitions, then runs real
CLI query/report/refresh three times and verifies the hand-computed M03.
This is a produced-signal scale sample, not a live ticker capacity certificate.
The real isolated ticker E2E separately checks three observed passes within
29–45 s, pause/resume splitting, and a second process opening a new interval;
assertions use cadence bounds rather than exact wall-clock durations.

The fixture has 5,942 unique accepted tasks and 2,574,895,800 observed ms
(one interval); the exact result is `21391200000/2574895800` tasks/hour.
All three bounded queries returned complete coverage and that exact value;
unbounded reports returned the same value with a censored tail. Query p50/p95
was 83.55/86.78 ms, report 342.86/343.34 ms, and M03 refresh
996.41/1427.07 ms. Load start/end (1 / 5 / 15 min) was
`5.12 / 5.71 / 6.59` / `5.19 / 5.70 / 6.58`. Full tracked-cell
`analytics rebuild --verify` was identical, usage violations were zero and the
canonical digest remained unchanged. This fixture duration is planted source
evidence, not a measurement of actual elapsed ticker running time.

**Correctness.** The unchanged `scale_gates_hold_under_load` passes: exact
accounting totals, one acceptance per record, reproducible as-of answers,
byte-identical rebuilds and untouched canonical digest. New E2E workflows use
CLI/public store/producer APIs for clipping, zero hours, outages, restart,
correction deduplication, sidecar-only restatement, fresh reports, old pinned
values/coverage/watermarks/digests, exports, retention and backup/restore.
The requested 15 suites returned 182 passed, 13 ignored and four sandbox-only
socket failures (`attempts_show_attention_summary`,
`attention_intervals_union_and_censor`,
`recommendations_and_notices_change_no_canonical_state_and_no_dispatch`,
`thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`): each failed
at Unix socket bind with `Operation not permitted`. After narrowing cache
dependencies, query/operations/scale passed again (30 passed, 13 ignored).
An initial load-gate writer encountered transient SQLite `database is locked`;
the unchanged gate passed on subsequent focused and full runs, with no timeout
or assertion relaxed. Clippy reported no warnings in changed lines (existing
warnings elsewhere remain). Bench datasets and preserved binaries were removed
before committing. L1–L6 and L8 remain unchanged; 100k samples on this shared
host do not establish authoritative 1M performance.

### 4.19 DG3b: ticker isolation and explicit telemetry opt-in (100k only)

Branch `telemetry/dg3-m03`, base `482f94b`, 2026-10-01. Registry remains v5.
This repairs the DG3 operating producer integration under L7; **pending the
steward's serial 1M certification and outside-sandbox socket regression run.**

**Design and failure mechanism.** DG3 scheduled telemetry ahead of controller
services and called `operating::observe` on every canonical project. This
created sidecars for source-free projects, activated additional lane work, and
held a canonical SQLite/OFD reader while opening/migrating/writing the sidecar
on an idle-priority thread. That reader can pin canonical files while sidecar
work waits. Accelerated 250-ms controller passes also scheduled operating
writes instead of respecting the nominal 15-second cadence. The original
integration fixture has no sidecar or recorded producer homes: DG3b removes
its telemetry worker and writes entirely. Its repository is separate from the
project, so the sidecar itself does not dirty the candidate repository. Tick
joining already checks `is_finished`; it does not join an unfinished worker.
The exact blocked frame in the steward's reported timeout is not established
here because this sandbox refuses the fixture's socket bind.

Telemetry now runs after controller services. Nominal cadence slots limit
operating observations independently of collection cadence and accelerated
controller ticks. `HERDR_PROJECTS_TELEMETRY_COLLECT_SECS=0` returns before any
telemetry scan, worker or write. Existing sidecars are observed, including
paused projects; absent sidecars require recorded native sources or explicit
OTLP receiver configuration before a worker is admitted. Empty work never
spawns a thread. A source-backed collect may opt a project in and then observe
its first prefix. The cheap eligibility reader has zero lock-wait budget;
contention defers collection instead of delaying the controller. The operating
reader is dropped before opening or waiting on the sidecar writer, and its
timestamp is captured with its state read.

**Identical answers.** No evaluator, schema, registry, acceptance placement,
coverage rule or as-of path changed. The operating transaction is factored
without changing interval arithmetic: consecutive Active endpoints in the same
session/control epoch merge; gaps longer than three passes split; pauses and
restarts split; open tails stay censored. Existing observations retain identical
M03 values. Projects without opted-in observations gain no invented duration.
Disabled collection is again disabled telemetry. All existing telemetry golden
values remain unchanged, and `scale_gates_hold_under_load` is unchanged.

**Reproduction and measurements.** Built both binaries with §1's release
command and ran `scale_2_queries` and `scale_9_analytics_refresh` serially with
`SCALE_EVENTS=100000 SCALE_ACTIVE=64 SCALE_REPEATS=3 SCALE_PER_ROUND=1`, tags
`dg3b-before|dg3b-after`. The same disk-backed `$PWD/bench-data/dg3b` dataset
(seed 5100, 10,000 attempts, eight homes) was generated and ingested once:
106,517 observations, 58,084,542 logical bytes, 53,725,172 rollout bytes.
`TMPDIR=$PWD/bench-data/tmp`; one bench process at a time, no builds or tests
overlapping samples. These surfaces do not exercise ticker scheduling, so
this is a compatibility comparison, not evidence of a scheduler speedup.

| Surface, ms (p50 / p95) | Before | After |
| --- | ---: | ---: |
| Report | 242.91 / 253.48 | 236.34 / 241.54 |
| M02 terminal cohort | 278.76 / 293.41 | 290.39 / 290.86 |
| M08 lane usage | 25.11 / 29.17 | 24.64 / 25.59 |
| M13 coverage | 38.93 / 48.62 | 39.90 / 40.09 |
| Pane, in process | 60.01 / 60.37 | 53.70 / 59.25 |
| Digest section, in process | 53.90 / 65.14 | 52.71 / 52.84 |
| Analytics refresh | 893.74 / 907.39 | 805.78 / 807.19 |

Results-file load averages (1 / 5 / 15 minutes):

| Phase | Start | End |
| --- | --- | --- |
| queries before | 1.64 / 3.00 / 3.74 | 3.65 / 3.31 / 3.82 |
| queries after | 2.75 / 1.73 / 1.65 | 2.25 / 1.68 / 1.63 |
| analytics-refresh before | 1.89 / 2.84 / 3.61 | 1.89 / 2.84 / 3.61 |
| analytics-refresh after | 2.07 / 1.66 / 1.62 | 2.07 / 1.66 / 1.62 |

Refresh peak RSS was 79,432 → 79,844 KiB. Both runs had zero usage-gate
violations and unchanged canonical digests. Five- and fifteen-minute loads
were lower afterward; these three samples do not establish a causal speedup.
After report/M02 p95s are below doc 10's provisional 500-ms aggregate target,
pane/digest below 250/100 ms, and memory below 256 MiB at 100k. This is neither
doc 10's five repeated load runs nor the authoritative 1M certification.


**Correctness and limitations.** Ran all 20 telemetry suites plus `cli`,
`canonical_worker` and `controller`, with `--locked --offline -j 3`,
`--no-fail-fast`, `RUST_TEST_THREADS=1` and `TMPDIR=$PWD/target/tmp`.
The combined run returned 277 passed, 14 ignored, 64 socket-restricted failures
and one failure in the new local recovery fixture. That fixture initially
allowed the faulted task to run first and block serial progress; it now confirms
the first publication before submitting the second and passes (106.83 s).
The final reader adjustment was verified again through query and scale suites
(19 passed, 13 ignored), including the unchanged load oracle. New E2Es exercise
real CLI tickers for disabled byte-identical sidecars, untouched Active/paused
projects, source-backed first collection, serial integration and lost-reply
recovery. Existing pause/resume/restart assertions retain their cadence bounds;
that workflow now explicitly opts into telemetry instead of setting the disable
switch. Clippy has no warnings on changed lines; existing unrelated warnings
remain. Bench data is removed before commit. L1–L6 and L8 stay open.

The following tests failed only because socket bind is denied with
`Operation not permitted` (some Python fixture servers report EPERM before the
parent times out waiting for their socket). TCP receiver/integration fixtures
are restricted too. None was bypassed or changed:

`canonical_worker`:

```text
a_hidden_path_covering_the_execution_home_refuses_the_launch_before_creation
a_launch_reaches_running_while_another_holder_takes_the_shared_root_intermittently
a_legacy_thread_holding_the_planned_worktree_blocks_its_creation
a_proven_worker_end_keeps_the_project_admitted_but_an_unexplained_pane_loss_pauses_it
a_sandboxed_reviewer_uses_its_worker_channel_through_the_spool
a_subdirectory_binding_runs_in_the_same_subdirectory_of_the_new_worktree
a_worker_branch_reaching_a_corrupt_quarantined_object_is_refused
an_isolated_codex_worker_commits_through_codex_workspace_write_sandbox
an_isolated_worker_cannot_read_owner_secrets_or_lift_the_hiding_but_still_commits_and_submits
an_isolated_worker_submits_only_through_its_own_spool
an_untracked_working_directory_is_refused_before_the_approval_is_used
canonical_attempt_sidebar_clears_after_termination_in_an_active_project
canonical_attempt_sidebar_does_not_publish_to_a_replaced_terminal
canonical_attempt_sidebar_refreshes_and_clears_on_pause_and_termination
canonical_attempt_sidebar_restart_offers_no_historical_cleanup_or_native_request
canonical_attempt_sidebar_uses_collected_usage_and_observed_waiting
review_assignment_launches_with_blind_brief_and_records_session
ticker_does_not_dispatch_a_launch_cancelled_before_creation
ticker_launches_and_briefs_once_then_stops_a_cancelled_worker_while_paused_and_revoked
ticker_launches_nothing_on_a_server_without_the_launch_contract_or_while_paused
ticker_recovers_a_lost_creation_reply_without_creating_again
ticker_retires_a_cancelled_gated_worker_without_starting_it
```

`cli`:

```text
canonical_ownership_cli_adopts_recorded_coordinator_without_prompting
controller_captures_uncommitted_worker_edits_for_submission_and_verification
hot_paths_skip_the_whole_store_check_and_the_ticker_checks_off_its_pass_then_pauses_admission_and_effects_on_corruption
integration_releases_project_ownership_during_the_candidate_check
launch_reserve_records_operator_reason
native_ticker_claims_legacy_routine_and_restart_delivers_without_rerun
operator_verify_releases_project_ownership_during_the_check
outcome_success_path
rejected_reservation_writes_no_decision
ticker_auto_chain_releases_verified_integrated_and_fan_in_dependents
ticker_auto_integrates_two_results_serially_and_recovers_stale_and_crash
ticker_auto_verification_releases_project_ownership_during_the_check
ticker_auto_verifies_once_and_recovers_after_kill
ticker_canonical_notification_confirms_or_retains_ambiguity_after_owner_death
ticker_canonical_observations_commit_cancel_and_restart_in_the_shared_pool
ticker_coordinator_prime_confirms_or_recovers_once_across_restart
ticker_coordinator_start_then_prime_recover_without_replaying_start
ticker_local_and_remote_launches_acknowledge_once_and_recover_lost_replies
ticker_native_briefs_confirm_or_recover_uncertainty_without_replay
ticker_native_copy_publishes_announces_and_does_not_recopy_after_restart
ticker_native_merged_finalization_resolves_and_replays_notice_after_restart
ticker_notifications_recover_across_restart_and_reconcile_through_cli
ticker_remote_briefs_confirm_or_recover_uncertainty_without_replay
ticker_tokens_use_supervised_local_remote_and_coordinator_refreshes_after_restart
```

`controller`:

```text
a_malformed_ambiguous_notification_blocks_new_notifications
a_notification_is_claimed_before_it_is_shown_and_never_shown_twice
a_notification_is_refused_while_the_project_safety_settings_are_invalid
a_notification_retry_is_not_delivered_before_it_is_due
a_store_error_that_is_not_a_full_disk_or_busy_database_does_not_pause_admission
interval_slots_are_anchored_at_the_start_counted_in_bulk_and_never_rescheduled
missed_slots_are_skipped_or_coalesced_and_a_revision_never_reuses_an_occurrence
the_ticker_runs_routines_only_while_active_and_never_revives_a_disabled_revision
ticker_delivers_a_notification_once_only_while_active_and_unleased
ticker_notifies_an_expired_wait_once_and_reserves_nothing
ticker_reserves_a_ready_dependent_once_only_with_factory_admission_on
ticker_runs_an_approved_routine_once_beside_one_edited_after_approval
```

`telemetry`:

```text
attempts_show_attention_summary
```

`telemetry_accounting`:

```text
attention_intervals_union_and_censor
```

`telemetry_health`:

```text
recommendations_and_notices_change_no_canonical_state_and_no_dispatch
```

`telemetry_otlp`:

```text
http_auth_limits_malformed_and_replay
http_request_rate_is_bounded
```

`telemetry_workspace`:

```text
thread_start_records_the_dispatch_reason_and_the_sidebar_suffix
```

### 4.22 Steward 1M re-certification of L1–L7 (main `628b59f`)

Run by the steward on 2026-10-01, 06:58–07:27, serially: one bench process at a
time, nothing else from this project running. Release build per §1, Linux
7.2.5-3-omarchy, same host. The owner's other sessions were active, so the
1-minute load was 1.25–2.59 throughout (each phase's start/end load is in its
results file). Dataset: `SCALE_EVENTS=1000000 SCALE_ACTIVE=64`, seed 5100,
1,006,427 events, 538,865,463 rollout bytes. Phases ran in the order
0, 1, 2, 3, 4, 8, 9 (health), 9 (analytics), 10, 6, 7, 5, with
`SCALE_REPEATS=3` and `SCALE_PER_ROUND=1` (queries), and `SCALE_CADENCE_MS=1000` (freshness).
Every correctness gate held in every phase: no violations, `canonical_unchanged`
true, and faults and late/slow passed. This supersedes the "pending the
steward's 1M certification" labels for the cards merged through #207.

| limitation | target | original 1M | re-certified 1M | verdict |
| --- | --- | --- | --- | --- |
| L1 admission, telemetry on vs off (configured) | p50 ≤ +5 %, p95 ≤ +10 % | +26.4 % / +12.2 % | 5.67 → 6.48 ms (+14.3 %) / 6.74 → 13.74 ms (+104 %) | **not met** (absolute p95 +7 ms) |
| L1 reconcile, on vs off | same | +4.0 % / +53.6 % | 3.00 → 2.84 ms (−5.3 %) / 14.86 → 13.98 ms (−5.9 %) | **met** |
| L2 freshness at 100/s steady | p95 ≤ 5 s | 34.5 s | 40.3 s (p50 17.4 s) | **not met**; burst p95 121 s (was 54.9 s), drain 26.4 s (was 31.8 s). See below |
| L3 lane/central metrics | p95 ≤ 500 ms | M08 2.5 s, M13 5.8 s, report 7.4 s, view health 8.4 s | M08 27.6 ms, M13 56 ms, report 308 ms, view cost 98 ms, view health 566 ms, view project 664 ms | **met** for M08/M13/report/view cost; view health/project just over |
| L3 compare / live health | (dashboard 500 ms) | compare M02 7.2 s, health 20.6 s | compare M02 **13.8 s (regressed)**, health 14.0 s | **not met**; compare regressed ~1.9× |
| L3 native cohort, as-of, export page | p95 ≤ 500 ms | 0.22–0.41 s | 25–344 ms | **met** |
| L4 accounting sync (warm) | 256 MiB | 13.4–22.5 s, 244 MB | 2.05–2.26 s, 46 MiB | **met** |
| L4 analytics refresh | 256 MiB | 9.5–11.7 s, 301 MB | p50 0.9 s / p95 2.1 s, 228 MiB | **met** |
| L4 health evaluate | 256 MiB | 20.2 s, 227 MB | 13.6 s p50, 150 MiB | **met** (memory) |
| L4 one ticker pass, own process | 256 MiB | 17.9 s, 294 MB | 2.9 s, 226 MiB | **met** |
| L4 cold first `accounting sync` | 256 MiB | — | 35.2 s, 262 MiB | marginal (one-time cold rebuild) |
| L5 workspace pane (in process / fresh) | p95 ≤ 250 ms | 16.1 s / 16.0 s | 56.9 ms / 56.5 ms | **met** |
| L5 digest section (in process / fresh) | ≤ 100 ms | 17.0 s / 18.5 s | 52.2 ms / 57.4 ms | **met** (`context --peek` views on 57 ms vs 2.9 ms off; digest 14 lines, 1,472 B) |
| L6 sidecar after cold ingest | halve bytes per rollout byte | 1.44 GB, 2.7× | 758.6 MB, **1.41×** | **met in effect** (−47 % vs the original 2.7×; 2.67 → 1.41) |
| L7 fairness, light-project freshness | p95 ≤ 5 s every round | (100k only: failed round 1) | 11.2 / 16.4 / 11.0 s; hot pass p95 10.8–16.8 s | **not met** |
| L7 M03 operating throughput | exact planted answer | — | exact `21391200000/2574895800`; query p95 94 ms, report 380 ms | **met** |
| Collector | 256 MiB, byte caps | 85–115 MB | 81–108 MiB per 256 MiB run, caps held | **met** |

**Freshness (L2) and fairness (L7) share a cause.** In the freshness passes the
`accounting` step takes p50 0.9 s but reaches 20–22 s in every phase (steady:
17 passes, max 20.6 s; burst p50 19.8 s), while a sync with nothing new takes
2.1 s. The light projects in the fairness phase take ~200 ms per pass but wait
behind the hot project's 11–17 s passes on the shared worker. Card P7
instruments the accounting sub-steps (full vs incremental replay, quota
rebuilds, dirty-session fan-out) and fixes the cause, then makes the worker
fair if it is still needed. Card P8 takes the `compare` regression and live
`health`. L8 is proved only in the TM5.4 live ramp (owner gate).

### 4.20 P7 / P7b: quota replay spikes and shared-worker fairness

Branch `perf/1m-freshness`, base `628b59f`, 2026-10-01. **Pending the
steward's serial 1M certification.** These are new P7b runs, not the interrupted
worker's measurements. Registry, stream versions, capture triggers, existing
metric expectations and the `scale_gates_hold_under_load` body are unchanged.

**Measured cause.** Accounting diagnostics now record attention sampling,
ledger preparation/validation and writes, graph storage, quota storage and its
mode/row count, quota dispatch, cache totals, tools/fleet/cost, analytics input
frontiers, termination summaries and final frontier/count/commit work. Each
freshness results JSON retains every pass's `last_mode`, `last_reason`, dirty
and selected session counts and `quota_rebuild`, beside these timers.
All 15 baseline 1M passes were `incremental`, with null reasons and
`quota_rebuild=false`; all selected 64 sessions. Six nevertheless replayed
entire quota accounts because a new snapshot sorted before the globally last
snapshot. Those replays read 99,064–106,211 native snapshots. The slowest
accounting tick was 68,142.01 ms, including 60,585.25 ms in quota storage.
Thus source-delete invalidation, cache-totals loss, secondary-window correction
triggers and graph fan-out were not the measured spike mechanism. No capture
trigger was weakened to remove it.

The following breakdown compares each version's slowest measured 1M accounting
pass (baseline steady, candidate drain), in milliseconds. Different ingress
backlogs and host loads make this diagnostic evidence, not a matched latency
microbenchmark:

| Accounting sub-step | Before | After |
| --- | ---: | ---: |
| Attention sample | 15.57 | 13.64 |
| Prepare and validate | 27.90 | 50.42 |
| Ledger writes | 234.19 | 1,231.19 |
| Graph store | 16.56 | 45.10 |
| Quota store, including affected floor | 60,585.25 | 1,583.30 |
| Quota dispatch | 181.82 | 63.22 |
| Cache totals | 57.81 | 83.83 |
| Tools | 1,408.84 | 81.91 |
| Fleet | 277.86 | 6.08 |
| Cost metrics | 1.07 | 0.03 |
| Analytics input frontier | 246.66 | 6.06 |
| Termination summary | 1,467.70 | 45.20 |
| Frontier, public counts and commit | 3,129.82 | 181.02 |
| Cost tick | 0.16 | 0.10 |

**Fix and identical answers.** A late *new* quota snapshot rewinds affected
accounts to complete window checkpoints and replays only their suffix in the
original `(observed_ts, session_id, ordinal)` order. The common floor repeatedly
rewinds windows crossing it, including decrease flags that do not advance the
last trusted timestamp. This fixes a correctness defect in the interrupted
worker's proposed suffix algorithm for staggered limits/windows. Native IDs are
pinned before deleting projections; existing observation-order and native
primary-key indexes select the suffix without replaying historical accounts.
The persisted prefix is the exact original state-machine checkpoint, including
start evidence, high-water values, trusted counts, flags and plan. The state
machine and fixed-point arithmetic are unchanged. Corrections, account
reassignments, source deletions, missing caches and inconsistent frontiers retain
the original replay/rebuild paths. Ledger, graph, quota, caches, watermark and
queue removal still commit together, with P1's source/generation validation.

Tools compare their actual canonical dependency (derived blocked spans) and
M40 dispatch storage compares the ordered decision/kind/home/time fingerprint,
instead of replaying all sessions/decisions for unrelated reconciliation writes.
Full canonical identities remain in read validation. The canonical stamp is
captured before its dependent decision read, so a concurrent new decision makes
the stamp stale and forces fallback. Legacy frontier bodies refresh safely on
first sync. No evaluator, coverage rule, disposition, digest or as-of selection
rule changes. After, all 119 measured 1M passes remain incremental with null
reasons and `quota_rebuild=false`: 77 append and 42 suffix quota turns.
The largest suffix replays 7,381 snapshots, not the entire 1M account history.
Active-session history and affected whole-window suffixes still grow; this is
not a constant-time guarantee for arbitrary late events or full rebuilds.

**Paired reproduction.** Reused and restored the deterministic seed-5100
prepared datasets, manifests/generator states, rollouts and SQLite stores at
identical absolute paths before every run. Both scales have 10,000 bindings,
64 active attempts and eight synthetic execution homes. The 1M seed starts with
198,000 usage records; the 100k seed with 19,800. Fairness restores that same 1M
hot seed plus the three original isolated 1k light projects. CLI collect/sync
settles source/file identities outside timing. Preserved baseline binaries
contain only diagnostics over `628b59f`; final binaries include these fixes.
An incomplete baseline overlapping a duplicate build was stopped and excluded;
all quoted completed runs were serial, without overlapping builds or tests.
Every dataset, seed and fixture home stayed under `$PWD/bench-data/` on disk.

Build each version with §1's exact release/no-run command (`--locked --offline
-j 3 --features state-store --test telemetry_scale`). Use §1's environment,
`SCALE_ACTIVE=64 SCALE_REPEATS=3 SCALE_CADENCE_MS=1000`, `SCALE_EVENTS=1000000`
only for phases `scale_4_freshness_burst` and `scale_6_fairness`; use
`SCALE_EVENTS=100000` for the freshness regression. Freshness retains 120 s
steady / 60 s burst / 30 s drain. Fairness retains three 15 s rounds, the
5 s reader cadence and the unchanged per-light-project 5 s criterion.
`SCALE_TAG=p7b-before|p7b-after` names freshness JSONs. Fairness writes
`results-fairness.json`; preserve it between variants. The optional
`SCALE_FAIRNESS_DERIVED=0` measures final accounting with single-worker scheduling.
No other 1M phase was run. Bounded workers: one collection/accounting worker and
one derived worker; FIFO capacity 64, one queued turn per project, no per-project
threads. With the controller, sleeping appender and surface reader, busy work
stays within six threads. The fairness workers now use the ticker's idle
CPU/I/O profile. Scheduling and enqueueing run off the controller; it never joins
unfinished work; orderly shutdown retains its existing shared 60 s bound.

| Freshness workload | Before p95 (s) | After p95 (s) | Passes before / after | Accounting p50 / max before → after (ms) |
| --- | ---: | ---: | ---: | --- |
| 1M/64 steady | 81.250 | 3.067 | 10 / 92 | 709.48 / 68,142.01 → 522.31 / 937.36 |
| 1M/64 burst | 262.323 | 8.467 | 3 / 18 | 40,517.33 / 61,686.04 → 1,751.12 / 3,059.98 |
| 1M/64 drain | 81.286 | 7.644 | 2 / 9 | 2,569.09 / 49,580.18 → 1,852.63 / 3,401.01 |
| 100k/64 steady | 10.985 | 1.742 | 72 / 118 | 557.51 / 6,636.96 → 387.27 / 768.44 |
| 100k/64 burst | 24.052 | 7.606 | 6 / 25 | 5,330.18 / 9,224.71 → 1,587.50 / 3,036.01 |
| 100k/64 drain | 12.031 | 3.886 | 7 / 15 | 2,714.83 / 6,351.08 → 1,659.86 / 1,713.51 |

All four freshness runs report zero violations and zero unseen usage after
settle. One baseline 1M burst pass exhausted the unchanged 8 MiB cap; subsequent
turns drained it. No after pass exhausted it or reported a pass error. The
local steady target is met at both scales; 1M burst/drain and 100k burst remain
above 5 s. Default ticker cadence remains 300 s. L2 is only partly addressed.

**Why fairness also needs scheduling.** With corrected accounting but
`SCALE_FAIRNESS_DERIVED=0`, round 1's hot pass takes 25,612.25 ms: accounting
676.23 ms, analytics 6,506.98 ms and health 17,778.38 ms. Round 3's due analytics
refresh takes 6,343.45 ms. Light turns themselves remain 0.18–0.20 s, yet the
fixed criterion fails. The ticker now serves collection/accounting independently
of one bounded FIFO worker for the other lanes, preserving their original lane
order. The same production worker is exercised by the fairness harness. No
parallel turns of the same derived worker or unbounded project threads appear.

The split-worker run passes every round's fixed append-to-ledger criterion,
even with a 27,980.94 ms hot derived turn (round 1) and 7,215.61 ms (round 3).
`pass_ms` now explicitly measures the core turn; `derived_passes` separately
records slower-lane durations/errors, so that work is not omitted or hidden.
Slow lanes can still delay other derived refreshes; their recorded `as_of`
remains explicit. This does not certify every derived surface within 5 s.

| Light project | Before p95, rounds 1 / 2 / 3 (s) | Accounting-only p95 (s) | Split-worker after p95 (s) |
| --- | --- | --- | --- |
| 1 | 53.326 / 11.291 / 14.263 | 24.897 / 1.809 / 8.241 | 1.846 / 1.766 / 1.708 |
| 2 | 53.537 / 11.519 / 14.501 | 25.080 / 1.987 / 8.338 | 1.830 / 1.810 / 1.849 |
| 3 | 53.746 / 10.863 / 14.728 | 25.262 / 2.065 / 6.822 | 1.744 / 1.730 / 1.910 |

Results-file load averages (1 / 5 / 15 minutes), start → phase ends → finish:

| Run | Start | Steady / burst / drain ends, or fairness round ends | Finish |
| --- | --- | --- | --- |
| 1M freshness before | 10.45 / 11.94 / 10.56 | 10.38 / 10.35 / 10.13; 7.87 / 9.04 / 9.65; 6.40 / 8.41 / 9.38 | 6.40 / 8.41 / 9.38 |
| 1M freshness after | 2.01 / 2.77 / 4.84 | 1.36 / 2.20 / 4.33; 1.64 / 2.12 / 4.15; 1.46 / 2.02 / 4.04 | 1.46 / 2.02 / 4.04 |
| 100k freshness before | 5.87 / 5.97 / 7.97 | 7.57 / 6.39 / 7.84; 7.28 / 6.63 / 7.82; 6.67 / 6.55 / 7.74 | 6.67 / 6.55 / 7.74 |
| 100k freshness after | 0.78 / 1.14 / 2.90 | 0.87 / 1.17 / 2.70; 1.53 / 1.31 / 2.64; 1.64 / 1.36 / 2.61 | 1.64 / 1.36 / 2.61 |
| 1M fairness before | 4.05 / 6.59 / 8.59 | 4.46 / 6.36 / 8.37; 3.59 / 5.87 / 8.10; 3.35 / 5.59 / 7.93 | 3.35 / 5.59 / 7.93 |
| 1M fairness accounting-only | 0.92 / 1.77 / 3.85 | 1.18 / 1.76 / 3.78; 1.87 / 1.90 / 3.78; 1.57 / 1.83 / 3.71 | 1.57 / 1.83 / 3.71 |
| 1M fairness after | 0.50 / 1.19 / 3.18 | 1.20 / 1.30 / 3.15; 1.13 / 1.28 / 3.11; 1.32 / 1.31 / 3.08 | 1.32 / 1.31 / 3.08 |

The after host is much quieter, especially for 1M freshness. Timing ratios are
provisional, not an authoritative causal speedup certification. Replay counts,
mode/reason diagnostics and the accounting-only fairness control establish the
specific work removed and the remaining scheduling delay. L1/L3–L6/L8 and the
unproduced signal/live-capacity limitations are not closed by this card.

**Validation.** The unchanged final release load gate passes in 11.72 s
(also 11.57 s before the conservative stamp-order correction). Accounting's
release suite passes 30 tests, including P1's incremental/full proofs and P5c's
capture schema guard; its only failure is the expected Unix bind denial.
Three new CLI/public-store E2Es prove: an append of a quota snapshot plus usage
only dirties/rewrites its changed session despite unrelated reconciliation;
staggered-window/flagged late quota replay preserves the prefix and equals full
replay byte for byte; a blocked hot derived refresh leaves another project's
public ingestion available and preserves current/pinned answers and rebuilds.
No unit/source-text tests, new crate or source process spawn was added.
The final debug run uses `TMPDIR=$PWD/target/tmp RUST_TEST_THREADS=1`,
`cargo test --locked --offline -j 3 --features state-store --no-fail-fast`,
all fifteen requested targets and the five remaining telemetry targets.
The requested targets pass **191 tests**, with **13 ignored** and only the
following four sandbox-denied Unix-socket binds (`Operation not permitted`):

- `telemetry::attempts_show_attention_summary`
- `telemetry_accounting::attention_intervals_union_and_censor`
- `telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`
- `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`

All twenty telemetry targets pass **220 tests**, with **13 ignored** and those
four failures plus two TCP loopback bind denials in the extra OTLP target:
`telemetry_otlp::http_auth_limits_malformed_and_replay` and
`telemetry_otlp::http_request_rate_is_bounded`. No other failure, changed
expectation or sandbox workaround. The unchanged debug gate and ticker
kill/resume workflow pass; old pinned values, coverage, watermarks and digests
remain reproducible, full ledger/analytics rebuilds match and canonical bytes
remain untouched by telemetry. Clippy (`cargo clippy --locked --offline -j 3
--features state-store --bin herdr-projects --test telemetry_accounting --test
telemetry_scale --message-format=json`) completes with zero warning locations
on changed lines, including secondary diagnostic spans. Existing unrelated
warnings remain.

Files: `src/telemetry/accounting/{ledger,mod,quota,tools}.rs`,
`src/telemetry/background.rs`, `src/ticker.rs`,
`tests/telemetry_accounting.rs`, `tests/telemetry_scale.rs`, this certificate.
All `bench-data/` datasets and preserved binaries are removed before commit.


#### P7c: writer admission and lock duration

Follow-up on the steward-rebased P7b commit `7dcb70d`, compared with its
main base `7e15a15`, 2026-10-01. **Pending the steward's serial 1M
certification.** No fetch/rebase, gate edit, expectation change, capture-trigger
change or new crate. Benchmark stores, preserved binaries and restored seed
archives remain under this worktree's disk-backed `bench-data/` and are removed
before commit.

**Measurement and diagnosis.** A temporary SQLite interposer measured successful
`BEGIN IMMEDIATE` completion through `COMMIT`/`ROLLBACK`, excluding admission
waits and killed unfinished transactions, in three unchanged release gate runs
per variant. The gate itself uses its original small racing fixture, not a
100k replacement. These are nearest-rank p50 / p95 / max, milliseconds:

| Variant | Accounting writer scopes (n) | Analytics writer scopes (n) |
| --- | --- | --- |
| Main base | 37.287 / 233.722 / 423.504 (96) | 7.303 / 10.835 / 27.630 (959) |
| P7b before | 55.962 / 282.285 / 358.642 (57) | 8.127 / 13.095 / 41.879 (1,019) |
| P7c before the quality race fix | 78.954 / 315.886 / 619.089 (91) | 1.944 / 3.820 / 56.065 (535) |

Main's committed accounting scopes alone are 38.522 / 233.722 / 423.504
(n=84); P7b's are 57.559 / 282.285 / 358.642 (n=49). Main/P7b each
passed all three local gates, so the steward's failure-rate difference was not
reproduced at these lighter loads. Main loadavg (1 / 5 / 15 minutes), first
start → final end: 3.25 / 5.78 / 7.67 → 2.97 / 5.44 / 7.49;
P7b: 2.97 / 5.44 / 7.49 → 2.74 / 5.08 / 7.28;
P7c: 10.16 / 6.52 / 5.61 → 7.30 / 6.26 / 5.56. Changed race batching,
checkpoint timing and host load make those accounting distributions unsuitable
for claiming an accounting speedup.

P7b did add quota suffix fixed-point/crossing selection, replay-identity
selection, source reads and state-machine evaluation inside the writer.
Analytics also opened/read the canonical database for every cell while holding
the sidecar writer. The fix moves those read-only steps into the prepared read
snapshot. Native JSON serialization, selected/dirty diagnostic counts and the
quota dispatch affected-floor calculation move there too. Quota deletion,
projection insertion, graph/cache/dispatch/frontier writes and commit remain
atomic. Diagnostics retain `quota_prepare_ms` separately from `quota_store_ms`
and add accounting `write_lock_ms`; the latter excludes preparation/admission.
The tools and dispatch frontier algorithms remain unchanged.

A serial paired 100k/64 control restores the exact same seed, including paths,
before phases 8/9 (`SCALE_REPEATS=3`). Writer scopes include each phase's warm
turn and its three timed turns:

| Scope | P7b p50 / p95 / max (ms) | P7c p50 / p95 / max (ms) |
| --- | --- | --- |
| Accounting writer (n=4 each) | 374.505 / 888.917 / 888.917 | 261.524 / 444.277 / 444.277 |
| Analytics writer (n=106 each) | 9.821 / 19.144 / 33.052 | 1.754 / 15.308 / 26.811 |
| Accounting wall, timed turns | 407.54 / 457.37 / 457.37 | 298.16 / 300.99 / 300.99 |
| Analytics wall, timed turns | 1,216.91 / 3,222.75 / 3,222.75 | 981.74 / 2,397.96 / 2,397.96 |

Accounting results-file loadavg: before 12.02 / 6.66 / 5.64 →
11.46 / 6.64 / 5.64; after 7.11 / 6.24 / 5.56 →
7.11 / 6.24 / 5.56. Analytics: before 11.46 / 6.64 / 5.64 →
10.16 / 6.52 / 5.61; after 7.11 / 6.24 / 5.56 →
6.62 / 6.15 / 5.53. Both report zero violations and unchanged canonical
bytes. These short samples and unequal shared-host load are provisional.

**Admission and correctness.** Foreground `accounting sync` and
`analytics refresh` share a cumulative **30 s writer-admission wait budget**
across their request's transactions, retrying SQLite BUSY/LOCKED with
10–50 ms process/request-seeded jitter. No writer transaction is held during sleeps.
The bound covers admission waits, not read preparation, sidecar migration/open,
execution after admission or unrelated collectors. Ticker admission tries once
and defers contention or an invalidated accounting plan to its next pass.
SQLite BUSY/LOCKED from the accounting tick's attention/cost writes or
sidecar opening also defers; non-contention errors still propagate.
No new worker/thread or controller wait is introduced.

Accounting validates the pinned source frontier and all durable input
projection generations after writer admission, re-preparing a foreground plan
if they changed. The quota state machine, ordered native identities, crossing
fixed point, prefix checkpoint and fixed-point arithmetic are identical;
planning reads the retained prefix before deletion using the same suffix floor
predicate. The dispatch affected floor is the same minimum of old and newly
planned selected observations. Analytics checks live input generations and
canonical file/WAL identities after admission; canonical head reading happens
before admission and a changed file identity defers publication. Existing
input stamps, revision bodies and pinned selection rules are unchanged.

A new CLI/public-store E2E holds a competing writer for six seconds, starts
sync and refresh, checks ticker admission defers promptly, then commits a native
correction while both foreground requests wait. Both survive the old five-second
timeout; current usage changes, the pinned body is identical, and full ledger
and analytics rebuilds match. Existing P1 incremental/full proofs and the P5c
capture schema guard retain their exact expectations.

The first after 1M fairness attempt exposed a separate quality collector
read-to-write upgrade race (`quality: database is locked` on a light project).
Its deferred transaction read `EXISTS` before writing; another lane could
commit and make that snapshot impossible to upgrade. Removing the lookup and
using `INSERT OR IGNORE`'s inserted-row count eliminates that upgrade, preserves
idempotent observed totals and reduces statements. The failed run is retained
as a real contention failure, not classified as a sandbox socket failure.
A second attempt exposed an incomplete new ticker deferral: its zero-wait
connection could return BUSY from attention/cost rather than ledger admission.
Accounting now defers SQLite BUSY/LOCKED across the whole ticker turn;
this is an admission deferral, and neither changes committed ledger values
nor suppresses other errors. Both interrupted runs precede the final rerun.

**1M rerun after ticker fixes and 100k regression.** Same restored seed-5100 hot/light
stores and binaries built with §1's exact release command; one bench process
at a time, no overlapping builds or test suites. `SCALE_ACTIVE=64`,
`SCALE_REPEATS=3`, `SCALE_CADENCE_MS=1000`; only freshness/fairness timed at 1M.
This rerun includes both ticker-contention corrections above.

| 1M freshness phase | P7b before p95 (s) | P7c after ticker fixes p95 (s) | Passes before / after |
| --- | ---: | ---: | ---: |
| Steady 100/s | 4.274 | 4.270 | 66 / 75 |
| Burst 1,000/s | 11.345 | 9.906 | 14 / 17 |
| Drain 100/s | 11.941 | 10.219 | 7 / 7 |

All completed before/final freshness phases report no violations, no pass
errors and zero unseen usage after settling. Steady meets 5 s locally;
burst/drain still do not. The P7b removal of 20–60 s quota replay spikes holds;
this is not certification that every freshness target is met.

Quota sub-step p50 / p95 / max, milliseconds, on these same 1M runs. Before
quota storage includes planning; after separates it outside the writer:

| Phase | P7b quota store | P7c quota prepare, outside writer | P7c quota store, under writer | P7c whole accounting writer |
| --- | --- | --- | --- | --- |
| Steady | 53.76 / 584.28 / 700.87 | 37.17 / 131.78 / 164.50 | 3.28 / 312.71 / 450.08 | 518.54 / 873.23 / 1,221.01 |
| Burst | 1,134.91 / 2,239.01 / 2,239.01 | 162.42 / 256.03 / 256.03 | 759.66 / 1,528.10 / 1,528.10 | 1,845.35 / 3,224.91 / 3,224.91 |
| Drain | 94.70 / 2,372.64 / 2,372.64 | 231.01 / 300.59 / 300.59 | 1,560.14 / 1,958.54 / 1,958.54 | 3,590.61 / 4,623.03 / 4,623.03 |

Different ingress backlog and batch sizes affect these distributions. In
particular, the drain median is higher despite less planning under the writer;
actual suffix deletion/insertion and whole selected-session rewrites still
hold the writer, and this card does not claim constant-time arbitrary replay.

| Light project | P7b p95 rounds 1 / 2 / 3 (s) | P7c after ticker fixes p95 rounds 1 / 2 / 3 (s) |
| --- | --- | --- |
| 1 | 1.578 / 3.621 / 2.897 | 2.756 / 2.771 / 1.815 |
| 2 | 1.728 / 3.769 / 3.134 | 2.898 / 2.339 / 1.852 |
| 3 | 1.876 / 3.722 / 3.298 | 3.041 / 2.187 / 1.909 |

The first rerun after ticker fixes passes every unchanged 5 s light-project criterion,
with zero unseen usage and no core/derived pass errors. Hot-core pass p95 is
3,963.62 / 2,289.76 / 1,615.64 ms; the bounded shared-worker design is retained.

Results-file loadavg (1 / 5 / 15 minutes):

| Run | Start | Steady / burst / drain ends, or fairness round ends | Finish |
| --- | --- | --- | --- |
| 1M freshness P7b | 6.61 / 6.12 / 6.54 | 3.30 / 4.99 / 6.07; 3.23 / 4.63 / 5.86; 3.22 / 4.44 / 5.75 | 3.22 / 4.44 / 5.75 |
| 1M freshness P7c after ticker fixes | 6.52 / 4.38 / 4.48 | 2.47 / 3.51 / 4.14; 2.93 / 3.43 / 4.06; 2.98 / 3.39 / 4.02 | 3.06 / 3.40 / 4.02 |
| 1M fairness P7b | 3.20 / 4.42 / 5.73 | 3.16 / 4.30 / 5.65; 3.17 / 4.23 / 5.60; 3.03 / 4.13 / 5.53 | 3.03 / 4.13 / 5.53 |
| 1M fairness P7c after ticker fixes | 3.14 / 3.41 / 4.02 | 6.08 / 4.07 / 4.22; 5.17 / 4.00 / 4.20; 4.03 / 3.82 / 4.13 | 4.03 / 3.82 / 4.13 |

Shared-host load differs; no timing ratio is an authoritative causal speedup.
The 100k/64 freshness regression has p95 steady/burst/drain
**3.337 / 9.537 / 18.067 s**, 108 / 14 / 8 passes, no pass errors,
zero unseen usage and no correctness violations. Loadavg start → steady/burst/
drain ends → finish: 4.03 / 3.82 / 4.13 →
4.58 / 4.02 / 4.16; 4.46 / 4.22 / 4.23; 8.61 / 5.22 / 4.56 →
8.61 / 5.22 / 4.56. The noisy drain is slower than P7b's earlier quieter
100k sample; this run certifies the answers and the local steady criterion,
not a drain latency improvement.

The first ten loaded debug gates passed 9/10: run 8 failed in a racing
`collect` at its existing five-second writer timeout; neither sync nor refresh
failed. Foreground collection (`create=true`, as the CLI invokes it) now uses
SQLite's **30 s busy timeout per admission**. Its transactional source-cursor
reads and parser are unchanged; ticker collection keeps its original policy.
This collector bound is per transaction, distinct from sync/refresh's
cumulative jittered admission budget. A second CLI E2E holds a writer six
seconds, collects a real pending fixture rollout, verifies one accepted usage
record, verifies the next collect observes zero new records, and matches a
full rebuilt ledger byte for byte. It does not retry whole collection calls
or lose the counts of already committed batches.

**Final production-binary rechecks after the collector wait adjustment.**
The repeated 1M freshness run remains correct, with no pass errors, no
violations and zero unseen usage. It is slower during the shared-host load
spike; these completed results are retained alongside the earlier sample:

| Phase | P7b before p95 (s) | Final binary p95 (s) | Passes before / after |
| --- | ---: | ---: | ---: |
| Steady | 4.274 | 14.767 | 66 / 50 |
| Burst | 11.345 | 17.594 | 14 / 10 |
| Drain | 11.941 | 8.754 | 7 / 8 |

Final-binary loadavg start → steady/burst/drain ends → finish:
5.50 / 3.35 / 4.68 → 7.64 / 4.77 / 5.02;
5.96 / 4.95 / 5.06; 3.80 / 4.51 / 4.91 → 3.80 / 4.51 / 4.91.
All 68 accounting passes remain incremental, with null invalidation reasons
and `quota_rebuild=false`. Steady's slowest whole pass is 11,061.61 ms:
collection 2,385.76 ms, accounting 1,657.68 ms, analytics 6,918.92 ms;
its accounting writer is 1,484.32 ms and replays two append snapshots.
Accounting's steady maximum is 4,827.70 ms; burst collection's maximum is
5,665.46 ms. The old 20–60 s quota-account replay mechanism does not recur,
but the 5 s steady target is not consistently met on this shared host.

Final-binary fairness has no core/derived writer errors and zero unseen usage,
but one light project misses the unchanged criterion in each of two completed
samples. These are genuine performance failures, not socket-only failures:

| Sample | Worst light-project p95 per round (s) | Criterion |
| --- | --- | --- |
| First final-binary recheck | 1.925 / 5.181 / 3.551 | fails round 2 |
| Further paired sample | 5.250 / 4.628 / 1.816 | fails round 1 |

First sample loadavg start → round ends → finish:
3.58 / 4.45 / 4.89 → 3.82 / 4.42 / 4.86;
4.61 / 4.57 / 4.91; 4.04 / 4.45 / 4.86 → 4.04 / 4.45 / 4.86.
Further sample: 3.80 / 2.79 / 3.75 → 5.15 / 3.24 / 3.86;
5.56 / 3.46 / 3.93; 4.41 / 3.34 / 3.87 → 4.41 / 3.34 / 3.87.
In the first miss the hot-core p95 is 4,200.25 ms; light turns themselves have
p95 319–507 ms and still wait behind that hot turn. In the further miss hot-core
p95 is 4,910.79 ms. No pass was deferred. This leaves a scheduling/whole-pass
limitation under variable load; the prior split-worker gain is reproducible
in the earlier passing run, but these final checks do not certify L7 as met.
No criterion was relaxed, and the steward's serial certification remains
necessary. No timing ratio is asserted to be a causal regression or speedup.

**Final loaded-gate proof.** After the foreground collector adjustment,
`scale_gates_hold_under_load` passes **10/10**, with no edits to its body.
Each invocation uses `cargo test --locked --offline -j 3 --features state-store
--test telemetry_scale -- --exact scale_gates_hold_under_load --test-threads=1`,
`TMPDIR=$PWD/target/tmp`. A concurrent serial loop runs `--no-fail-fast
--test telemetry_workspace --test telemetry_operations` (five iterations),
with one test thread and at most one extra fixture workflow. Operations passes
all 14 tests every iteration; workspace passes nine, with only its
sandbox-denied Unix bind test failing. No concurrent benchmark or build runs.

Gate loadavg (1 / 5 / 15 minutes), start → end:

| Gate | Result | Start | End |
| --- | --- | --- | --- |
| 1 | pass | 4.41 / 3.34 / 3.87 | 3.79 / 3.28 / 3.83 |
| 2 | pass | 3.79 / 3.28 / 3.83 | 4.72 / 3.57 / 3.91 |
| 3 | pass | 4.72 / 3.57 / 3.91 | 5.19 / 3.80 / 3.97 |
| 4 | pass | 5.19 / 3.80 / 3.97 | 4.93 / 3.88 / 4.00 |
| 5 | pass | 4.93 / 3.88 / 4.00 | 4.87 / 3.98 / 4.03 |
| 6 | pass | 4.87 / 3.98 / 4.03 | 4.90 / 4.08 / 4.06 |
| 7 | pass | 4.90 / 4.08 / 4.06 | 8.37 / 5.17 / 4.44 |
| 8 | pass | 8.37 / 5.17 / 4.44 | 8.36 / 5.53 / 4.58 |
| 9 | pass | 8.36 / 5.53 / 4.58 | 8.73 / 6.05 / 4.81 |
| 10 | pass | 8.73 / 6.05 / 4.81 | 8.95 / 6.40 / 4.97 |

**Final correctness and lint checks.** Run `cargo test --locked --offline
-j 3 --features state-store --no-fail-fast` with all 21 `tests/telemetry*.rs`
targets plus `--test cli --test canonical_worker`, `TMPDIR=$PWD/target/tmp`
and `RUST_TEST_THREADS=1`; leave paid/live tests ignored and unset
`HP_CODEX_SANDBOX_BIN` so worker fixtures use their deterministic stand-in.
The final run passes **291 tests**, with **18 ignored** and **53 failures solely
from sandbox-denied socket binds/startup**. All ordinary scale tests pass,
including the unchanged gate; all existing telemetry expectations remain
unchanged. Accounting passes 32 tests, including both new contention E2Es,
P1's incremental/full proofs and P5c's capture guard; its only failure is the
expected attention socket bind. No other correctness failure remains.
The preceding full run (before the collector-only policy) passed 290 tests,
with the same 53 socket denials and 18 ignored.

Final clippy command: `cargo clippy --locked --offline -j 3 --features
state-store --bin herdr-projects --test telemetry_accounting --test
telemetry_scale --message-format=json`. It completes successfully; checking
primary/secondary spans and child notes against changed lines finds **zero
warning locations in the diff**. There are 157 existing unrelated warning
diagnostics across these targets. `git diff --check` is clean.




Socket-only failure inventory (the same hard-sandbox bind prohibition also
denies the OTLP TCP loopback fixtures). Each listed failure has direct
`Operation not permitted` evidence, either at the bind or in the fixture
server's traceback before its startup deadline expires. No socket workaround,
real agent CLI, live service or owner data directory was used:

```text
canonical_worker::a_hidden_path_covering_the_execution_home_refuses_the_launch_before_creation
canonical_worker::a_launch_reaches_running_while_another_holder_takes_the_shared_root_intermittently
canonical_worker::a_legacy_thread_holding_the_planned_worktree_blocks_its_creation
canonical_worker::a_proven_worker_end_keeps_the_project_admitted_but_an_unexplained_pane_loss_pauses_it
canonical_worker::a_sandboxed_reviewer_uses_its_worker_channel_through_the_spool
canonical_worker::a_subdirectory_binding_runs_in_the_same_subdirectory_of_the_new_worktree
canonical_worker::a_worker_branch_reaching_a_corrupt_quarantined_object_is_refused
canonical_worker::an_isolated_codex_worker_commits_through_codex_workspace_write_sandbox
canonical_worker::an_isolated_worker_cannot_read_owner_secrets_or_lift_the_hiding_but_still_commits_and_submits
canonical_worker::an_isolated_worker_submits_only_through_its_own_spool
canonical_worker::an_untracked_working_directory_is_refused_before_the_approval_is_used
canonical_worker::canonical_attempt_sidebar_clears_after_termination_in_an_active_project
canonical_worker::canonical_attempt_sidebar_does_not_publish_to_a_replaced_terminal
canonical_worker::canonical_attempt_sidebar_refreshes_and_clears_on_pause_and_termination
canonical_worker::canonical_attempt_sidebar_restart_offers_no_historical_cleanup_or_native_request
canonical_worker::canonical_attempt_sidebar_uses_collected_usage_and_observed_waiting
canonical_worker::review_assignment_launches_with_blind_brief_and_records_session
canonical_worker::ticker_does_not_dispatch_a_launch_cancelled_before_creation
canonical_worker::ticker_launches_and_briefs_once_then_stops_a_cancelled_worker_while_paused_and_revoked
canonical_worker::ticker_launches_nothing_on_a_server_without_the_launch_contract_or_while_paused
canonical_worker::ticker_recovers_a_lost_creation_reply_without_creating_again
canonical_worker::ticker_retires_a_cancelled_gated_worker_without_starting_it
cli::canonical_ownership_cli_adopts_recorded_coordinator_without_prompting
cli::controller_captures_uncommitted_worker_edits_for_submission_and_verification
cli::hot_paths_skip_the_whole_store_check_and_the_ticker_checks_off_its_pass_then_pauses_admission_and_effects_on_corruption
cli::integration_releases_project_ownership_during_the_candidate_check
cli::launch_reserve_records_operator_reason
cli::native_ticker_claims_legacy_routine_and_restart_delivers_without_rerun
cli::operator_verify_releases_project_ownership_during_the_check
cli::outcome_success_path
cli::rejected_reservation_writes_no_decision
cli::ticker_auto_chain_releases_verified_integrated_and_fan_in_dependents
cli::ticker_auto_integrates_two_results_serially_and_recovers_stale_and_crash
cli::ticker_auto_verification_releases_project_ownership_during_the_check
cli::ticker_auto_verifies_once_and_recovers_after_kill
cli::ticker_canonical_notification_confirms_or_retains_ambiguity_after_owner_death
cli::ticker_canonical_observations_commit_cancel_and_restart_in_the_shared_pool
cli::ticker_coordinator_prime_confirms_or_recovers_once_across_restart
cli::ticker_coordinator_start_then_prime_recover_without_replaying_start
cli::ticker_local_and_remote_launches_acknowledge_once_and_recover_lost_replies
cli::ticker_native_briefs_confirm_or_recover_uncertainty_without_replay
cli::ticker_native_copy_publishes_announces_and_does_not_recopy_after_restart
cli::ticker_native_merged_finalization_resolves_and_replays_notice_after_restart
cli::ticker_notifications_recover_across_restart_and_reconcile_through_cli
cli::ticker_remote_briefs_confirm_or_recover_uncertainty_without_replay
cli::ticker_tokens_use_supervised_local_remote_and_coordinator_refreshes_after_restart
telemetry::attempts_show_attention_summary
telemetry_accounting::attention_intervals_union_and_censor
telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch
telemetry_otlp::http_auth_limits_malformed_and_replay
telemetry_otlp::http_protobuf_attempt_token_binding_auth_and_project_token_unchanged
telemetry_otlp::http_request_rate_is_bounded
telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix
```

Files: `src/telemetry/writer.rs`, `src/telemetry/mod.rs`,
`src/telemetry/accounting/{ledger,mod,quota}.rs`,
`src/telemetry/analytics/{inputs,store}.rs`, `src/telemetry/quality/flakes.rs`,
`src/telemetry/codex.rs`, `tests/telemetry_accounting.rs`, this certificate,
and the accounting/analytics contracts. No new source process spawn, crate,
unit/source-text assertion or existing test expectation change. Benchmark
stores, archives, temporary interposer and preserved binaries are removed
before commit; verification logs remain under ignored `target/`.

#### Steward 1M certification of P7c (2026-10-01, 16:31–16:43, serial, quiet host)

Release build of `perf/1m-freshness` (P7b + P7c), `SCALE_EVENTS=1000000
SCALE_ACTIVE=64`, seed 5100, `SCALE_CADENCE_MS=1000`; 1-minute load 1.66–2.39.
The worker's provisional 14.8 s steady p95 was host noise; it was measured at much higher load.

| 1M/64 freshness p95 | original | §4.22 main | **P7c** | target |
| --- | --- | --- | --- | --- |
| steady 100/s | 34.5 s | 40.3 s | **3.3 s** (p50 1.8 s; 84 passes, p95 1.79 s) | ≤ 5 s ✓ |
| burst 1,000/s | 54.9 s | 121 s | **9.7 s** | — |
| drain 100/s | 31.8 s | 26.4 s | **8.3 s** | — |

Fairness (`scale_6_fairness`, three 15 s rounds): `fair: true`. Light-project p95
3.7/3.8/3.8, 3.3/3.5/3.5 and 3.3/3.5/3.6 s; hot project 6.0–6.8 s. **L2 steady
freshness and L7 fairness are met at 1M.** Every correctness gate held. Pass
errors 0, budget exhaustion 0.

Contention: the worker's loaded gate loop passed 10/10 (load 3.8–9.0). The
steward's alternating loaded three-suite runs after rebase on main `155b280`:
P7c 0/6, main 0/6 (load 3.2–3.8).

### 4.21 P8: maintained M02 comparison and live health (100k follow-up)

Branch `perf/compare-1m`, base `628b59f`. **Pending the steward's serial
1M certification. This worker has not measured 1M.** The card permits only
`scale_2_queries` at 1M; this checkout had no prepared 1M dataset. Permission
to run the deterministic generation/ingest preparation, or a path to an
existing fixture, was requested and remains pending. Do not interpret the
100k numbers below as the requested same-dataset 1M comparison. Plan doc 10
is not present in this checkout; the unchanged targets quoted in §4/§7 apply.

**Cause and phase evidence.** Compare still called native `attempt_usage`
per attempt, scanned the maintained session graph once per attempt, and
computed the entire quality lane only to retain M42. A temporary SQLite
PROFILE interposer on the preserved main CLI produced these 100k observations
(one additional untimed diagnostic run; no instrumentation is committed):

| Compare phase | SQL elapsed (ms) | statements |
| --- | ---: | ---: |
| Canonical lifecycle inputs | 76 | 4 |
| Decisions/configurations/classifications | 85 | 5 |
| Per-arm native cost | 70 | 11,788 |
| Per-arm model allocation | 829 | 10,862 |
| Pairing | 0 at SQLite's timer resolution | 2 |
| Quality / verification flip rate | 0 at timer resolution | 6 |
| Other SQL | 65 | 19,910 |

Wall time was 1,643.62 ms. The remaining 518.62 ms includes statement
preparation, Rust candidate selection/arm attribution, bootstrap evaluation,
JSON processing and process overhead; those subphases were not individually
timed. This is a 100k profile, **not** a claimed breakdown of the steward's
13-second 1M sample. Health's corresponding diagnostic wall was 2,524.54 ms,
including 853 ms model allocation, 66 ms native cost, 201 ms lifecycle and
299 ms decision/classification SQL.

P1 (`93a242c`) added `session_graph_nodes_session`. The unchanged per-attempt
`WHERE attempt_id=? ORDER BY session_id` query now chooses a full scan through
that non-covering session index, rather than the original table scan plus
small DISTINCT sort. On the same 990-node 100k store, 9,937 lookups take
898.61 ms through the chosen index versus 587.79 ms with `NOT INDEXED`, returning
identical rows. Both remain full-history work per candidate; at 1M there are
roughly ten times as many graph nodes. This identifies a merged query-plan
regression, but the exact fraction of the 1M regression remains unmeasured.
DG1b already made M30 candidate loading lazy; M02 does not load candidates.
DG2's M10 branch is not entered for M02. DG3's M03 is not evaluated by compare.
DG6 adds an unnecessary flip-rate evaluation through the broad quality call;
it is negligible on this fixture, whose verification producer is unavailable.

**Design and identical answers.** The live fallback joins the maintained graph
and model segments once per arm, unions the same model sets per task, and
retains the same mixed-bucket flag. Duplicate graph nodes remain idempotent.
Pairing directly calls the original M41/M42 evaluator rather than evaluating
unrelated quality metrics. Native cost acceptance/deduplication, arm attribution,
class ordering, bootstrap seed/draws, suppression, pooling and ranking are
unchanged. Analytics refresh maintains the complete default terminal M02 body
in the existing disposable provider table. Computation uses its pinned read
snapshots; the existing short writer validates canonical file/head identity,
registry version and input generations before publishing. Live reads validate
and consume the body in one sidecar snapshot. Operating heartbeats are excluded
because compare does not read M03. Missing/stale bodies use the live evaluator;
windows, assignment follow-up, custom seeds and other metrics keep that path.
No schema, canonical write, metric revision/digest or as-of selector changes.

Health's recommendation rule consumes this identical validated comparison.
Collector age, moving windows, flake rules, quota expiry, waits, thresholds,
notices and health-rules.v3 still evaluate live. A current 100k health diagnostic
has zero native-cost/model-allocation statements and takes 919.79 ms; canonical
and moving-window work remains. The 500-ms health target is **not met**.

**Reproduction and paired measurements.** Both builds use §1's exact release
command, locked/offline `-j 3`. One disk-backed seed-5100 fixture under
`$PWD/bench-data/p8-100k`, `SCALE_EVENTS=100000 SCALE_ACTIVE=64`, was generated
and ingested once. Initial main query samples precede changes; an explicit
post-change analytics refresh warms the same inputs outside timing. Phase 2
uses `SCALE_QUERY_SET=p8 SCALE_REPEATS=3 SCALE_PER_ROUND=1`, one process at a
time and no build/test overlapping measurement. `SCALE_QUERY_BIN` selects the
preserved main CLI for before; `SCALE_COMPARE_BIN` runs that CLI after each
timed after comparison and asserts **identical stdout bytes** (three checks).
The unchanged load-gate body is untouched. No dataset is committed.

| 100k/64, n=3 | Before p50 / p95 ms | After p50 / p95 ms |
| --- | ---: | ---: |
| Compare M02 | 1,631.78 / 1,646.05 | 11.90 / 12.00 |
| Health live states | 2,747.01 / 2,759.03 | 895.17 / 921.58 |

Results-file load averages (1 / 5 / 15 minutes), start → end:
before `3.37 / 4.14 / 3.31 → 4.27 / 4.31 / 3.38`;
after `3.35 / 3.92 / 3.38 → 3.14 / 3.86 / 3.37`.
These are shared-host samples, not authoritative 1M certification. The default
comparison meets 500 ms at 100k; health and 1M L3 remain open.

CLI E2E coverage collects a real rollout, checks the cold and maintained
comparison bytes, appends a real usage record (1,120 → 1,680 native tokens),
then proves a poisoned stale body is never served after collection advances
input generations. After sync/refresh, warm bytes again equal cold evaluation.
Model-only and canonical classification corrections invalidate evidence;
pinned metric revisions remain reproducible. Existing exact expectations
are unchanged. The fifteen requested suites plus the corrected compare-suite
rerun establish **189 passing tests, 13 ignored**, with only four unresolved
socket-bind failures (`Operation not permitted`):

- `telemetry::attempts_show_attention_summary`
- `telemetry_accounting::attention_intervals_union_and_censor`
- `telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch`
- `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix`

The full run passed the unchanged `scale_gates_hold_under_load`. The new E2E
initially violated a model-segment constraint, then tried to update an immutable
classification. Its corrected fixture retains a valid model bucket and appends
a new classification revision; all six compare workflows pass. Neither failure
was a production failure or classified as socket-only. Clippy reports no warning
on changed lines (97 existing unrelated warnings). The final release `scale_gates_hold_under_load` also passes unchanged in
**12.51 s**. The table above uses the final release after build, including the
operating-heartbeat dependency exclusion. All three final comparisons match
main stdout byte for byte. Peak RSS is 87,704 → 16,644 KiB for compare and
90,516 → 78,692 KiB for health. Benchmark datasets were removed before commit.

Files: `src/telemetry/analytics/{compare,inputs,store}.rs`,
`tests/telemetry_{compare,scale}.rs`, this certificate. No new crate, source
process spawn, unit test or source-text assertion.

**Steward 1M certification (2026-10-01, 14:20–14:31, serial, load 1.7–7.3).**
Release build of this branch, `SCALE_EVENTS=1000000 SCALE_ACTIVE=64`, seed 5100.
Query phase with `SCALE_QUERY_SET=p8 SCALE_REPEATS=3 SCALE_PER_ROUND=1`;
every gate held, with no violations and the canonical store unchanged.

| 1M/64 | main `628b59f` (§4.22) | this branch |
| --- | --- | --- |
| `compare --metric M02` p50 / p95 | 13,213 / 13,771 ms | **12.6 / 12.8 ms** |
| `health` (live states) p50 / p95 | 13,881 / 14,029 ms | **870 / 936 ms** |
| `health evaluate` p50 / p95 | 13,640 / 15,990 ms | **911 / 968 ms** |
| `analytics refresh`, warm, p50 / p95 | 905 / 2,085 ms | 912 / 943 ms (maintaining the comparison adds no measurable cost) |
| first cold `analytics refresh` after ingest | 3,468 ms, 240 MiB | 4,686 ms, 233 MiB (builds the maintained comparison once) |

L3 verdict: `compare` meets 500 ms. Live `health` and `health evaluate` are ~15×
faster but still above 500 ms.

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

The steward's serial 1M re-certification of main `628b59f` (§4.22) gives the
current verdict for each limitation below. It supersedes their "pending 1M" labels.

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
  other lanes' work remain. **P7/P7b: paired 1M steady/burst/drain p95
  81.250/262.323/81.286 → 3.067/8.467/7.644 s; 100k regression
  10.985/24.052/12.031 → 1.742/7.606/3.886 s (§4.20), pending the
  steward's serial 1M certification.** Diagnostics identify whole-account
  quota replay inside incremental ledger turns; complete-window suffix replay
  removes it without weakening capture. The after host is quieter; burst/drain,
  growing active-window suffixes and full rebuild costs remain limitations.
  **P7c: 1M steady/burst/drain 4.274/11.345/11.941 →
  4.270/9.906/10.219 s in the first rerun; the final-binary recheck is
  14.767/17.594/8.754 s under rising load. Writer admission now retries for up to 30 s
  cumulatively in foreground requests, while quota read planning runs outside
  the writer (§4.20). Pending the steward's serial certification; the 100k
  regression is 3.337/9.537/18.067 s under rising load.**
  Owner: accounting lane.
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
  **DG2 extends maintained reads to M10. At 100k/64, M08/report p95
  31.78/428.52 → 29.81/270.09 ms; refresh 952.43 → 911.94 ms,
  pending the steward's 1M certification** (§4.15, all phase load averages
  included). M10 previously had no producer; no general speedup or L3
  closure is claimed.
  **P8 maintains default terminal M02 comparison bodies (§4.21). At 100k/64,
  compare p95 1,646.05 → 12.00 ms and live health 2,759.03 → 921.58 ms,
  at 1-minute loads 3.37 → 4.27 before / 3.35 → 3.14 after.
  Pending the steward's 1M certification; this worker's 1M fixture preparation
  remains pending clarification. Health still exceeds 500 ms; L3 stays open.**
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
- **L6: sidecar size — halving target met at 100k by P5; pending the
  steward's serial 1M certification (§4.15).** The original sidecar was
  2.7–3.1 times rollout bytes (1.44 GB at 1M). On the same current 100k
  fixture, lossless storage reduces 209,563,648 → 104,570,880 bytes:
  3.901452× → 1.946799×, a 50.101% reduction, at start/end 1-minute loads
  4.83 → 4.54 before / 4.78 → 4.54 after. **P5b preserves compact size
  at 105,037,824 → 105,037,824 bytes on its current 100k fixture (§4.16),
  at loads 5.26 → 4.16 before / 2.51 → 1.91 after; pending the steward's
  1M certification.** **P5c restores compact-table accounting capture and
  preserves 105,041,920 → 105,041,920 bytes at 100k/64 (§4.17), with
  cold-phase 1-minute loads 4.31 → 4.11 before / 4.90 → 3.57 after;
  incremental sync p50/p95 317.42/340.42 → 250.84/252.38 ms at loads
  4.10 → 4.09 before / 2.85 → 2.94 after. Pending the steward's 1M
  certification; no speedup claim.** TM5.3's defaults require an
  operator-confirmed apply; automatic destructive retention was not enabled.
  Incremental page reclamation is enabled on new stores. Indefinitely retained
  valuation/latest history, active attempts and holds still prevent a universal
  growth bound. The original 1M result remains the last certified one.
- **L7: workload gaps — addressed for produced signals at 100k by P6 and DG3;
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
  card makes no claim to fix L1–L6 or to certify live capacity. DG3 adds durable
  observed operating intervals and M03 in §4.18, with 100k paired surface costs
  and a separate planted operating-signal sample. Report p95 was 466.51 →
  403.28 ms; refresh p95 was 1030.92 → 986.76 ms (different shared-host
  loads; no causal speedup claim). DG3b (§4.19) restores explicit opt-in and
  disabled-telemetry semantics and isolates operating reads from sidecar waits.
  Its same-dataset 100k report p95 was 253.48 → 241.54 ms and refresh p95
  907.39 → 807.19 ms; the results-file load averages are quoted there. These
  compatibility samples do not measure controller speedup. Its 1M
  certification is pending. **P7/P7b: 1M worst light-project p95 per round
  53.746/11.519/14.728 → 1.846/1.810/1.910 s (§4.20), pending the
  steward's serial 1M certification.** The fixed 5 s append-to-ledger criterion
  passes every round with two bounded shared workers; accounting-only scheduling
  still fails behind 6.5 s analytics / 17.8 s health work. Derived-lane refresh
  delays and unproduced/live-signal gaps remain; no general freshness target is
  relaxed or closed. **P7c: first-rerun 1M worst light-project p95 per round
  1.876/3.769/3.298 → 3.041/2.771/1.909 s; all three rounds
  met the unchanged 5 s criterion in the first rerun. Final-binary samples
  miss at 5.181 and 5.250 s, without writer errors (§4.20); L7 remains open
  pending the steward's serial certification.**
- **L8: simulated capacity is not live capacity.** See the opening. Planted
  attempts and generated rollouts certify the telemetry path's behaviour at
  these volumes on this host, nothing about live workers or providers.

## 8. Verdict

**Re-certification (2026-10-01, §4.22):** at 1M, L4 (memory), L5 (pane and
digest), L6 (size, 2.7× → 1.41×) and most of L3 now meet their targets, and
L1 reconcile is within target. Still not met: L1 admission p95, L2 freshness,
L7 fairness, `compare` (which regressed) and live `health` (cards P7 and P8).
Correctness held in every phase.

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
