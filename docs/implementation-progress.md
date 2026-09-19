# Implementation progress

Updated 2026-09-19. Base: `6e2bd7607d7bc64cf6155beceb299b44566d7f3b`.
Changes are an uncommitted local candidate on `hp-plan/w00/t00-1-baseline`, retaining
the initial regression fixtures. The user upgraded Herdr and requested that
implementation proceed; `herdr --version` now reports **0.9.1**.

## Implemented: W01 reliability changes

| Task | Behavior and evidence |
| --- | --- |
| T01.1 | Nonblocking subprocess I/O keeps deadlines active after parent exit. Owned groups receive direct TERM/KILL signals; capture is capped, raw bytes retained and cancellation distinct. Real subprocess tests cover inherited/escaped pipes, blocked stdin, large output on both streams, invalid UTF-8, ignored TERM, grandchildren, normal completion and cancellation. |
| T01.2 | UTF-8-safe suffix parsing, positive ASCII interval validation and checked conversion. Tests cover all unit boundaries, overflow in debug/release, a deterministic Unicode sample, per-file diagnostics and spring/fall DST transitions. |
| T01.3 | Poll/backoff/outage keys include project and session. Rotation removes a fixed slow-pass first project. Mocked full ticks cover two projects sharing a label, distinct sockets, launch/prompt/report copy, one failed session, local progress and recovery. |
| T01.4 | Merged final-copy intent and bounded retries persist before copying, independent of summary dedup and GitHub polling. Tests cover restart, stale identity, reopen, changed report PR, pause/resume, failed intent write, partial-copy warnings and crash after thread commit. Idle auto-resolution cannot bypass pending merge work. |
| T01.5 | Notification hashes advance only on confirmed delivery; failed notifications/nudges retain bounded retry across restart without stopping other work. Outage histories are persisted per resource; PR/outage inbox events retain delivery obligations and stable IDs. Tests cover failure, restart, healthy-resource isolation and replay after an item was handled. Doctor exposes pending retry state and malformed state files. |

The F02, F03, F04 and both F05 baseline reproductions now run normally and pass.
F01 artifact preservation remains the sole ignored, known-failing counterexample
assigned to W02; it has not been declared fixed.

See [ADR 0001](adr/0001-phase-a-boundaries.md) for the runtime contract, compatibility
boundaries and dependency decision. T00.2's full Phase B typed/store contracts
remain open before any migration implementation. No complete-wave acceptance or
independent review is claimed by these local changes.

## Validation

Rust 1.89 on Linux, using the temporary toolchain described in the
[historical baseline](review-baseline.md):

```sh
cargo test --locked --offline
cargo test --release --locked --offline
cargo build --release --locked --offline
git diff --check
```

Debug and release suites each passed **180 unit/scenario tests plus four CLI
tests (184 total)**, with one explicitly ignored defect fixture. The locked
release build and `git diff --check` also passed. The new `libc` dependency uses
the version already present in the lockfile; no package version was upgraded.

Herdr version compatibility is confirmed locally. Live Herdr sessions, SSH,
GitHub, mixed-agent execution and macOS were not exercised. Fairness/launch/copy
scenarios use FakeRunner and disposable project directories; process and parser
tests exercise real local behavior. No default sessions, production projects,
remote repositories or user worktrees were modified.

## Next work

1. Complete the W01 acceptance evidence that requires independent review and
   external environments (including macOS and live Herdr/SSH/GitHub).
2. W02 artifact preservation, remote transport, lifecycle, corruption recovery
   and coordinator-context fixes. Ticker loading still has the legacy default-on-
   corruption behavior; doctor now detects it, but recovery is not implemented.
3. Finish Phase B contracts before any database or memory-authority migration.

The current changes preserve legacy state formats. Reverting them restores the
old behavior and its known defects; there is no data migration to reverse.
