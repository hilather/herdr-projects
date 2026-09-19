# Implementation progress

Updated 2026-09-19. Base: `6e2bd7607d7bc64cf6155beceb299b44566d7f3b`.
The W01 reliability changes and initial regression fixtures were committed as
`af65de4` and pushed to the `hilather/herdr-projects` fork on
`hp-plan/w00/t00-1-baseline`. W02 work below is the next local implementation slice.
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
boundaries and dependency decision. T00.2's full Phase B typed/store contracts
remain open before any migration implementation. No complete-wave acceptance or
independent review is claimed by these local changes.

## In progress: W02 preservation, diagnostics and context

Local and remote library transfers now request content checksums, fixing F01
without relying on size/mtime equality. Local report receipts consume the copy
result; neither local nor remote ticker receipts fall back to an earlier hash
observation when no report was copied. Full-ticker scenarios cover a remote source
disappearing after observation and a transfer returning different bytes from the
observation. The quoted SSH file fallback writes raw bytes and refuses truncated
output before touching its destination.

Local complete final copies now publish verified snapshots under
`.state/artifacts/<thread>/<manifest-hash>/`. Staging uses exclusive files and
bounded streaming hashes (50 MiB total, 10,000 entries, depth 64); manifests record
relative paths, types, byte counts and SHA-256. Independent staged-byte verification
and a source rescan precede publication. Empty directories are preserved. Symlinks,
hard links, unsupported types and non-UTF-8 names are refused. Existing snapshots
are verified before reuse and never overwritten. Live-copy library roots and thread destinations reject symlink substitution.
Failed receipt writes now fail finalization; receipts and resolution reject changed execution identity. Missing
sources with a prior snapshot cannot be silently acknowledged as empty.

**Behavior change:** `--remove-worktree` currently refuses removal. Local preflight
checks active status, execution identity, shared/adopted references, managed panes
and agents, and retained snapshot/source equality. These checks cannot establish
writer exclusion, so even a verified snapshot does not authorize deletion. Remote
cleanup also refuses until verified transport and writer control exist. Plain
resolve and reopen remain available. `--discard-uncopied` cannot bypass these gates.
This deliberately leaves T02.1/T02.3 cleanup completion open rather than treating
an idle pane or two matching manifests as proof that no writer can continue.

T02.4 groundwork: ticker JSON parse/read failures now preserve the file and stop
that project's tick; healthy projects continue. Readable thread records remain
available alongside explicit malformed-record diagnostics in doctor and context.
Automatic quarantine/repair and unique popup handoffs are still pending.

T02.5 context: startup already directs the coordinator to `context`, which now
includes the full current PROJECT.md body, a revision hash and character count.
Each later context refresh repeats current instructions and flags oversized text.
Coordinator guidance distinguishes standing instructions from execution approval,
and documents advisory capacity, shared worker arguments and static worker briefs.
Tests verify changing instructions appear in both coordinator context and worker
briefs while malformed thread records remain visible.

Remote snapshots, robust directory-path transport, writer checkpoints, Git identity
and tombstone recovery, snapshot retention/garbage collection, and the remaining
lifecycle fixes are not complete. Legacy never-created sources can still be treated
as empty when no prior snapshot exists. Live report/library paths remain mutable
compatibility copies; retained snapshot paths are the verified evidence.

## Validation

Rust 1.89 on Linux, using the temporary toolchain described in the
[historical baseline](review-baseline.md):

```sh
cargo test --locked --offline
cargo test --release --locked --offline
cargo build --release --locked --offline
git diff --check
```

Debug and release suites each passed **195 unit/scenario tests plus four CLI
tests (199 total)**, with no ignored tests. The locked
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
2. Finish W02 remote preservation/path transport, writer exclusion and lifecycle
   cleanup/restart, record repair and concurrent popup handoffs. Removal stays
   unavailable until those preservation and lifecycle gates are implemented.
3. Finish Phase B contracts before any database or memory-authority migration.

The current changes preserve legacy state formats. Reverting them restores the
old behavior and its known defects; there is no data migration to reverse.
