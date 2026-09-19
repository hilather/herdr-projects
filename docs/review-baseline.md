# Architecture implementation baseline — T00.1

Recorded 2026-09-19. This is the historical first implementation slice of the supplied
`herdr-projects-design-plan`, specifically W00/T00.1. It adds reproducible defect
fixtures and records the actual checkout baseline. **It does not repair these
defects or mark W00/Phase A accepted.**

Subsequent fixes and the upgrade to Herdr 0.9.1 are recorded in
[implementation progress](implementation-progress.md). Test counts and failures
below describe the original measurement, before those repairs.

## Source and task claim

| Item | Value |
| --- | --- |
| Actual base commit | `6e2bd7607d7bc64cf6155beceb299b44566d7f3b` |
| Package | `herdr-projects 0.1.0` |
| Implementation branch | `hp-plan/w00/t00-1-baseline` |
| Candidate | Uncommitted changes on the base above |
| Owner | Primary implementation agent; single task, no concurrent writers |
| Owned paths | `src/scenarios.rs`, `src/scenarios/review_regressions.rs`, `tests/fixtures/review/`, this report |
| Lockfile SHA-256 | `bcb500298c676a2a4d2a25182a7a42c9753927a05eea1be5740ab40a9d86f689` |
| Design's reviewed commit | `a4cdb0a69713d982d96f9062548cf885f013c442` |

The reviewed commit is not present in local Git history, so no ancestry or exact
diff equivalence is claimed. Source inspection found the same affected parser,
runner, copy, polling and PR-finalization paths. All five reported defects were
then reproduced on the actual base. This checkout has **142 unit/scenario tests
plus four CLI tests (146 total)**, rather than the review's 146+4. Future work must
use this baseline and its measured counts, not assume the design's commit exists.

## Environment

| Tool | Observed version |
| --- | --- |
| OS/target | Linux `7.2.3-arch1-3`, `x86_64-unknown-linux-gnu` |
| Rust | `rustc 1.89.0 (29483883e 2025-08-04)`, LLVM 20.1.7 |
| Cargo | `cargo 1.89.0 (c24e10642 2025-06-23)` |
| Git | `2.55.0` |
| rsync | `3.5.0-g471e17dc`, protocol 32 |
| Herdr | `0.8.2` |
| GitHub CLI | `2.100.0` |

Rust was initially unavailable. An isolated Rust 1.89 toolchain and locked Cargo
dependencies were downloaded under `/tmp/herdr-projects-{cargo,rustup}`; no shell
startup files or default toolchains were changed. These temporary directories are
not project prerequisites and may be removed by system cleanup.

The installed Herdr is below `src/herdr.rs`'s minimum 0.9.1. Only its `--version`
and `agent start --help` were inspected. No default Herdr session, agent, real
project root, SSH machine or GitHub repository was used. Agent launch support,
model/effort flags and authenticated compatibility remain unverified.

## Baseline evidence

With Rust 1.89 selected and the locked dependencies cached:

```sh
cargo test --locked --offline
cargo build --release --locked --offline
```

Both passed before the regression module was enabled. The test results were
`142 passed; 0 failed` and `4 passed; 0 failed`. The release build completed
successfully. Cargo reported 44.38 seconds for the initial debug/test build and
43.25 seconds for the initial release build; these overlapped and are observations,
not a controlled performance benchmark. Unit/scenario execution took 2.82 seconds
and CLI execution 0.01 seconds in that run.

The captured [CLI help](../tests/fixtures/review/cli-help.txt) comes from the built
baseline binary. [Fixture provenance](../tests/fixtures/review/README.md) distinguishes
captured CLI output from synthetic Herdr/GitHub responses.

## Reproduced failures

The six tests in [review_regressions.rs](../src/scenarios/review_regressions.rs)
assert the **required repaired behavior**. Each is explicitly ignored until its
assigned repair lands, with its finding/task ID in the ignore reason. They do not
invert assertions to make defective behavior look successful. Normal tests staying
green is not evidence that these defects are fixed.

```sh
# Expected to exit nonzero while these defects remain unfixed:
cargo test --locked --offline review_regressions -- --ignored --nocapture

# Reproduce one defect, for example:
cargo test --locked --offline review_regressions::f03_ -- --ignored --nocapture
```

Observed result: **six failed, zero passed**, each for the intended defect after
its fixture setup succeeded.

| Finding / repair | Fixture boundary | Observed failure |
| --- | --- | --- |
| F01 / T02.1 | Real `du`, `rsync`, binary files and preserved timestamps in a temporary project | Copy reported `Complete`; destination retained version A while source held equal-length version B with the same modification time |
| F02 / T01.3 | Full multi-project ticker passes, FakeRunner, separate session sockets and the same machine label | First project was observed; `beta` had zero remote polls through tick 4 |
| F03 / T01.4 | FakeRunner PR/copy calls, real persisted ticker state reloaded with fresh process memory | Injected first-copy failure was confirmed; unchanged merged PR was polled again after recovery, but thread remained `Open` |
| F04 / T01.1 | Real shell and background descendant in an owned process group | 150 ms timeout returned after 2.009 seconds with code 0 and `timed_out = false` |
| F05 / T01.2 | Real Rust parser, Unicode suffix | Panic: byte index is inside `日` |
| F05 / T01.2 | Real Rust parser, `i64::MAX` interval | Debug panic: multiplication overflow |

The parser reproductions were also run with optimized release arithmetic:

```sh
cargo test --release --locked --offline review_regressions::f05_ -- --ignored --nocapture
```

Both failed for the intended reason: Unicode still panicked; the oversized interval
was accepted after wrapping instead of returning an error. This distinguishes the
release overflow defect from the debug arithmetic panic.

The descendant fixture is finite even on the broken runner: its child sleeps for
two seconds and exits. It does not create an indefinite daemon. The artifact test
asserts both successful initial transfer and unchanged source metadata before
checking stale destination bytes. It performs no worktree removal. Missing rsync
is a setup failure, not a reproduced artifact bug. Mocked poll and PR tests never
invoke live Herdr, SSH or GitHub.

On each repair, remove the matching `#[ignore]`, retain the behavioral assertions,
and add that task's broader acceptance coverage. F02 currently reproduces polling
starvation; launch/copy fairness, outage recovery and ordering permutations remain
T01.3 acceptance work. F03 uses an in-process restart model with disk reload, not
process-kill injection. F04 is the original pipe-lifetime counterexample, not the
full cancellation/output-cap/escaped-descendant suite.

## Remaining gates and handoff

- T00.1 core baseline and six defect counterexamples are captured. The candidate's
  aggregate `cargo test --locked --offline` passed: 142 unit/scenario tests plus
  four CLI tests, with the six known-failing counterexamples explicitly ignored.
  `git diff --check` passed and the lockfile hash is unchanged.
- Live Herdr JSON captures and a disposable-session contract check require a
  supported Herdr installation. The synthetic responses are not live evidence.
- macOS, mixed-agent workflows, authenticated SSH/GitHub and p50/p95/RSS/context
  performance measurements have not been run.
- T00.2 remains next: review and freeze component/state/authority contracts,
  minimal result/gate interfaces and a compatible SQLite dependency decision.
  No database dependency or migration has been introduced by T00.1.
- W01 follows the W00 gate; W02 follows W01. No wave acceptance, independent
  review, release, deployment, commit or publication is claimed by this report.

Proposed knowledge for the next task: use the actual base/count above; F01–F05
still reproduce here; keep legacy state authoritative through Phase A and preserve
the separate W03 runtime and T05.3 memory cutovers. This report is implementation
evidence, not a promotion into any user's authoritative project memory.
