# Phase A acceptance evidence

Updated 2026-09-19; candidate based on `c6a49c2`, with the review fixes and
fixtures in the accompanying working-tree changes. **Accepted by explicit user disposition on 2026-09-19.**
The user confirmed that no disposable macOS host is available. macOS remains untested.
The user subsequently instructed “lets accept it and move to next wave.” This
accepts Phase A with the documented gaps and authorizes W03; it does not turn
untested platforms or agent combinations into passing evidence.

## Independent review

A separate review agent initially rejected three issues:

- Ticker passes could execute after a pause between the outer status check and
  acquisition of the execution lease. Each pass now checks status inside its lease.
- Unreadable or malformed lifecycle state appeared active. Invalid state is now
  visible in listing/doctor/repair and refuses execution or ordinary status changes.
  Only a missing legacy state file defaults to active.
- A missing legacy artifact source could be treated as preserved. Finalization now
  refuses missing sources unless the operator explicitly requests `--skip-copy`.

The reviewer independently approved these fixes after inspecting the revised code
and running focused regressions. A follow-up review also approved the live fixtures
after SSH startup-hook isolation and injection-marker checks were corrected. This approves the code-review component, not all
platform or release acceptance requirements.

## Live Linux evidence

Fixtures use isolated temporary HOME, config, projects, sockets and named Herdr
servers. They do not discover or control existing user sessions or launch coding
agents. Tested with Herdr 0.9.1, Rust 1.89, Git 2.55 and OpenSSH 10.5p1.

| Check | Result and boundary |
| --- | --- |
| Real Herdr/Git lifecycle | Workspace/worktree creation and attachment, resolve, snapshot preservation, logical reopen and restart pass. Coordinator/thread records are seeded from real placement responses; this does not exercise every interactive creation/adoption path. |
| Desktop cleanup refusal | Protected same-user `/proc` entries prevent proving quiescence. Cleanup refuses and preserves source bytes; ordinary resolve/reopen succeeds. |
| Positive physical cleanup | The same fixture passes with `HP_LIVE_REQUIRE_REMOVAL=1` inside a disposable user/PID namespace with its own `/proc`. Worktree removal and restart from the retained branch succeed. Host process visibility restrictions remain. |
| Real loopback SSH | Ephemeral unprivileged sshd and pinned host key transfer the native artifact stream. Independent decoding verifies hashes, a 2 MiB binary report, empty directories and shell-sensitive Unicode/newline paths. This tests the real sender/SSH boundary; production receiver rejection paths remain covered by native tests. |
| Interactive popup input | A real PTY client invokes pause/resume, chooses one of two projects and verifies requested status changes while the other project's state bytes remain unchanged. This is behavioral automation, not a human visual/layout certification. |
| macOS | Untested: no host available. Remote and non-Linux destructive cleanup remain unsupported and must refuse. |

The SSH fixture disables StrictModes only in its disposable daemon configuration
because its key directory lives beneath `/tmp`; it allows only the current user,
disables password/PAM authentication, and uses disposable keys. It never edits
system SSH configuration. User SSH rc execution is disabled and the fixture refuses
hosts with a system SSH rc file. Popup testing omits plugin build/startup hooks to isolate
the action contract.

## Reproduction

The regular debug and release suites each pass 225 tests (216 unit/scenario, seven
CLI and two contract tests), keeping the three live fixtures ignored by default.
All three live Linux fixtures pass when explicitly enabled; the stricter namespace
cleanup run also passes. The locked release build and `git diff --check` pass. Rustfmt was unavailable in the
installed Rust 1.89 toolchain, so no formatter check is claimed.
Explicit Linux acceptance commands (with the project's Rust toolchain configured):

```sh
cargo test --locked --offline
cargo test --release --locked --offline
cargo build --release --locked --offline
HP_LIVE_HERDR=/absolute/path/to/herdr cargo test --locked --offline \
  --test live_phase_a -- --ignored --nocapture --test-threads=1
HP_LIVE_HERDR=/absolute/path/to/herdr HP_LIVE_REQUIRE_REMOVAL=1 \
  unshare --user --map-root-user --pid --fork --mount-proc \
  cargo test --locked --offline --test live_phase_a live_herdr -- --ignored --nocapture
```

The SSH fixture expects `/usr/bin/sshd` (override `HP_LIVE_SSHD`), `ssh` and
`ssh-keygen`. PTY access, loopback binding and user/PID namespaces may require
execution outside the agent sandbox. No macOS success is inferred from Linux.
Temporary evidence logs: `/tmp/herdr-phase-a-live-all.log`,
`/tmp/herdr-phase-a-ns.log`, `/tmp/herdr-phase-a-debug.log` and
`/tmp/herdr-phase-a-release.log`. The fixture sources are the durable evidence.

## Deferred acceptance evidence

Obtain macOS execution evidence and complete the supported platform/agent matrix
before claiming tested support across that matrix. Actual coding-agent combinations and
GitHub delivery remain untested live; scripted regressions are labeled accordingly.
W03 proceeds under this disposition: T03.1 store first, then T03.2 migration,
T03.3 outbox and T03.4 reconciliation. See the [task ledger](task-status.md) for
current implementation status.
