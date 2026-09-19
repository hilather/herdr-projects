# Implementation progress

Updated 2026-09-19. Base: `6e2bd7607d7bc64cf6155beceb299b44566d7f3b`.
The W01 reliability changes and initial regression fixtures were committed as
`af65de4` and pushed to the `hilather/herdr-projects` fork on
`hp-plan/w00/t00-1-baseline`. The local-preservation/diagnostics/context batch was
committed as `325caa7` and pushed to the same fork branch. The subsequent transport,
popup and lifecycle changes are recorded below and in the accompanying commit.
The user upgraded Herdr and requested that
implementation proceed; `herdr --version` now reports **0.9.1**.

## Implemented: W01 reliability changes

| Task | Behavior and evidence |
| --- | --- |
| T01.1 | Nonblocking subprocess I/O keeps deadlines active after parent exit. Owned groups receive direct TERM/KILL signals; capture is capped, raw bytes retained and cancellation distinct. Real subprocess tests cover inherited/escaped pipes, blocked stdin, large output on both streams, invalid UTF-8, ignored TERM, grandchildren, normal completion and cancellation. |
| T01.2 | UTF-8-safe suffix parsing, positive ASCII interval validation and checked conversion. Tests cover all unit boundaries, overflow in debug/release, a deterministic Unicode sample, per-file diagnostics and spring/fall DST transitions. |
| T01.3 | Poll/backoff/outage keys include project and session. Rotation removes a fixed slow-pass first project. Mocked full ticks cover two projects sharing a label, distinct sockets, launch/prompt/report copy, one failed session, local progress and recovery. |
| T01.4 | Merged final-copy intent and bounded retries persist before copying, independent of summary dedup and GitHub polling. Tests cover restart, stale identity, reopen, changed report PR, pause/resume, failed intent write, partial-copy warnings and crash after thread commit. Idle auto-resolution cannot bypass pending merge work. |
| T01.5 | Notification hashes advance only on confirmed delivery; failed notifications/nudges retain bounded retry across restart without stopping other work. Outage histories are persisted per resource; PR/outage inbox events retain delivery obligations and stable IDs. Tests cover failure, restart, healthy-resource isolation and replay after an item was handled. Doctor exposes pending retry state and malformed state files. |

The F02, F03, F04 and both F05 baseline reproductions pass in the W01 commit.
The initial W02 changes also enable and pass F01; all baseline regressions now run.

See [ADR 0001](adr/0001-phase-a-boundaries.md) for the runtime contract, compatibility
boundaries and dependency decision. ADR 0002 now supplies the Phase B interface
contract and dependency selection before any migration implementation. No complete-wave acceptance or
independent review is claimed by these local changes.

## Latest five-item batch: T02.1–T02.4 and T00.2

Base/head before this uncommitted batch: `30185f8` on
`hp-plan/w00/t00-1-baseline`. No live user sessions or production project roots
were modified. Native Rust tests and disposable Git/filesystem/process fixtures
supply the evidence below.

- **T02.1:** Versioned native remote artifact export/receive with 50 MiB payload,
  4 MiB manifest, 10,000 entries and depth limits. Source recheck, independent
  staged hashes, literal UTF-8 paths, binary payloads and immutable receipt reuse.
  Local Linux cleanup excludes supported managed operations, requires explicit
  known-writer shutdown confirmation, inspects same-user process cwd/descriptors/
  mappings, rechecks source/Git identity and uses non-force removal. Shared/adopted
  references, managed panes, ambiguity and failed verification preserve the source.
- **T02.2:** Runner file sinks stream without filling memory and terminate on the
  byte limit. All remote report paths use bounded staging and atomic publication.
  Remote final projections derive from verified snapshot bytes. Missing/incompatible
  helpers durably block merged finalization with installation/manual-retry guidance.
  Connection errors retain bounded retry. Live rsync projections retain preflight
  size checks but are not hard bounds against a growing source.
- **T02.3:** Removal intent records operation/repository/path/branch/head/snapshot
  before Git removal; a lost acknowledgement is recoverable. Pending removal
  excludes ticker launch and prompting. Logical reopen starts nothing; restart
  validates the retained branch and reattaches it without reset, force or branch
  deletion. Missing legacy creation evidence remains a human-inspection case.
- **T02.4:** `repair PROJECT inspect` emits structured JSON. Explicit restore checks
  the inspected hash and replacement identity/schema, excludes active writers,
  backs up original bytes and atomically replaces the record. Ticker lock prevents
  overwriting pending obligations from its in-memory state. No automatic repair.
  Earlier popup concurrency/replay/root/session/expiry tests remain enabled.
- **T00.2:** [ADR 0002](adr/0002-phase-b-contracts.md) and compiled illustrative
  domain/store interfaces define state, revision, event, approval, ambiguity and
  result/gate boundaries. Rust 1.89 rejects rusqlite 0.40.1; 0.37.0 with system
  SQLite 3.53.4 passes the standalone native transaction fixture. Its old bundled
  SQLite 3.50.2 was tested but rejected for engine maintenance. No application
  database dependency or migration was added.

Real Git tests exercise ignored artifact preservation, dirty/untracked refusal,
retained branch reopen, lost removal acknowledgement, and the entire resolve →
remove → reopen → restart path (Herdr responses mocked). Native CLI tests use a
scrubbed environment for multi-megabyte binary export and explicit repair. Process
fixtures cover sink overflow/exclusive creation, descriptor-based cleanup refusal,
and lifecycle lease exclusion. Stream fixtures reject path traversal, duplicates,
corrupt/truncated/oversize payloads and source changes during export.

## Validation and limits

Commands use Rust 1.89 with locked dependencies:

```sh
cargo test --locked --offline
cargo test --release --locked --offline
cargo build --release --locked --offline
CARGO_TARGET_DIR=/tmp/herdr-sqlite-smoke-target cargo run --locked --offline --manifest-path contracts/sqlite-smoke/Cargo.toml
```

**Results: 222 tests pass in both debug and release** (214 unit/scenario, six
CLI integration and two interface-contract tests), zero ignored. The locked release
build passes without warnings, `git diff --check` is clean, and the standalone
SQLite probe passes against system SQLite 3.53.4 on Rust 1.89. Tests use Linux, Git,
rsync 3.5.0, native subprocesses and disposable roots; Herdr lifecycle calls use
scripted fixtures. Real double-shell remote-path fixtures run locally.

Local command logs: `/tmp/herdr-five-debug.log`, `/tmp/herdr-five-release.log`,
`/tmp/herdr-five-build.log`, `/tmp/herdr-five-sqlite.log`. These temporary logs are
not release artifacts; the commands and fixture sources above are reproducible.

Remote and non-Linux destructive cleanup remain explicitly unsupported. The
checkpoint assumes cooperative same-user agents and stopped known writers; it
cannot contain hostile same-privilege processes or see managed state in other
roots. Live SSH, interactive popup delivery, macOS and independent review remain
Phase A acceptance requirements. Snapshot/repair-backup retention is manual.
Plain resolution retains the worktree unless cleanup is explicitly requested.
No force fallback, automatic merge/push or production data migration is introduced.

## Next work

[Task ledger](task-status.md): twelve locally implemented cards, 29 not started.
Complete the outstanding Phase A acceptance evidence before the W03 store/migration
wave. Proposed memory change: record native transport v1, cooperative checkpoint,
repair workflow and ADR 0002 decisions; no shared-memory promotion was performed.
