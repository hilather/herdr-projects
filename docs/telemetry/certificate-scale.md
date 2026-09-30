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
| Stores | canonical `SCHEMA = 67`; sidecar streams `codex` 3, `ingest` 8, `accounting` 11, `quality` 2, `analytics` 1, `health` 1, `policies` 1 |
| Build | `cargo test --release --locked --offline -j 3 --features state-store --test telemetry_scale --no-run` (rustc 1.98.0), system SQLite 3.53.4 |
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
`scale_5_faults`, `scale_6_fairness` and `scale_7_late_slow`. Each writes `results-<phase>[-<tag>].json` in the dataset
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
| F8 | `ticker.rs` `telemetry_pass` | the pass (collect and every lane tick) ran inline in the ticker's pass, so its whole duration delayed every later project's controller poll | its own thread, one project at a time; the ticker never waits for it (the integrity check's pattern) |
| F9 | `main.rs` | with the pass on a thread (F8), SQLite's memory statistics made every allocation of both threads take one process-wide mutex | statistics off in every build of the binary, as the crate's tests already do; nothing reads them |

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
  reordered. Remedies (not built): incremental ledger sync and analytics
  (L4), a pane that reads recorded revisions (L5), a lower CPU and I/O
  priority for the telemetry thread, or the sidecar on another device.
  Owners: accounting and analytics lanes, TM4.8, ticker steward.
- **L2: freshness.** By default the ticker collects once per 300 s per
  project, so a derived view is up to five minutes old by design. Even with
  a pass every second, p95 was 5.5–7.8 s at 100k and 34.5 s at 1M, because each
  pass rebuilds the ledger, session graph and quota windows from every
  collected record (`ledger::sync` is a full rebuild). Remedy: an
  incremental sync keyed on the collector's new records. Owner: accounting
  lane.
- **L3: lane and central metrics are not indexed aggregates.** Native cohort
  queries, as-of reads of stored revisions and paged exports meet 500 ms at
  1M. The lane metrics (M08/M09 derive the ledger again on every read, the
  tools and cost views, the central report with its per-decision M40) scan
  the history on each read: 2.3–8.4 s at 1M. Reading them from the
  analytics revisions (`--as-of-seq`, 159 ms) is the bounded path today.
  Owners: accounting lane, analytics (TM4.1).
- **L4: the ticker's telemetry pass exceeds the 256 MiB envelope at 10,000
  bindings.** The collector itself stays within it (58–115 MB) and its byte
  caps hold (8 MiB per pass, 256 MiB per CLI collect). But the lane ticks in
  the same pass build whole-history structures in memory: `accounting sync`
  244 MB, `analytics refresh` 301 MB, `health evaluate` 227 MB (2.17 GB
  before F7), one pass process 294 MB at 1M. Doc 10 says an overrun blocks
  promotion to release: this blocks promoting the lane ticks at this scale,
  not collection. Remedy: incremental sync and refresh (L2).
- **L5: the fleet pane and the digest section take seconds, not 250 ms / 100
  ms.** At 64 active attempts with 10,000 retained ones, one snapshot builds
  the full attempt projection three times (the active list, `compare`,
  `quality groups show`), the central report (M40 for every decision) and
  the accounting lane's report: 5.5 s p50 at 100k, 14.6 s at 1M, and the
  coordinator's `context` pays it too (15–17 s at 1M with views on, 3 ms
  with `[telemetry] views = false`). The digest stays bounded in size (13
  lines). Remedy: a snapshot over open attempts only, one projection shared
  by the three sections, and the recorded analytics revisions for the
  metrics. Owners: TM4.8, TM1.8, TM4.1.
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
moved the surfaces from minutes to seconds. The controller-overhead,
freshness, pane/digest and lane-tick memory targets are still missed; each
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
