# Worker filesystem isolation (2026-09-29)

Card: "worker filesystem isolation", branch `security/worker-fs-isolation`.
Owner decision: *isolate workers first*. Hide sensitive paths from workers
with empty mounts inside the worker's namespaces, and run the worker in a
further nested namespace so it cannot remove those mounts.

## Finding (before this change)

Canonical workers run as the owner's own uid. The supervisor argv was
`unshare --user --map-root-user --pid --fork --mount-proc --kill-child=KILL --
timeout ... <gate sh> -> env -i HOME=<execution home> PATH=/usr/bin:/bin ... <agent>`.
`--mount-proc` creates a mount namespace, but only `/proc` is remounted, so
the worker (uid 0 in its user namespace, mapped to the owner) reads every file
the owner can read. Scratch reproduction with planted files in a temp HOME
(no real secrets):

```
$ unshare --user --map-root-user --pid --fork --mount-proc --kill-child=KILL -- timeout ... 5s \
    env -i HOME=/nonexistent PATH=/usr/bin:/bin sh -c 'id; cat h/.ssh/id h/secretdir/k'
uid=0(root) gid=0(root) groups=0(root),65534(nobody)
SSHSECRET          # 0600 file in a 0700 directory
DIRSECRET
```

## Threat model

The worker is a model agent running owner-approved but untrusted actions. It
has the owner's uid (mapped), the network, and its approved inputs. We want it
unable to read or use the owner's credentials and control material: SSH and
GnuPG keys, the owner's own agent logins (`~/.codex/auth.json`), the product
configuration (`owner.toml`/`config.toml` with the owner policy, a future
`review-signer/` key directory), the owner's approval-signing key, other
projects' state under the projects root, and the Herdr control socket (which
drives every pane, including the unsandboxed coordinator). It must not be able
to undo the hiding.

Out of scope for this card (see residual risks): write-based escapes through
files the owner later executes, the network, and agents started by Herdr
outside this wrapper.

## Process map (evidence: code, not assumption)

| Process | Started by | Wrapper | Filesystem view after this change |
|---|---|---|---|
| Canonical worker (patched Herdr) | `workspace.create_command` with the literal argv (`src/canonical_worker/resources.rs`) | `worker_supervision::isolated_gated_command` | **Isolated** |
| Canonical worker (stock Herdr) | typed `exec herdr-projects launch-exec <spec>`, which execs the same argv (`src/canonical_worker/launch_spec.rs`) | same argv | **Isolated** |
| Reviewer worker | ordinary canonical attempt | same | **Isolated** |
| Native profile probe (`profile verify-native`/`verify-interaction`) | private lab Herdr, `src/profile_preparation/native.rs` | same | **Isolated** (lab socket file hidden) |
| Coordinator agent | Herdr `agent.start` in a pane (`src/coordinator_start_jobs.rs:44`, `herdr.rs:342`) | none on the agent (only the short bridge client uses `supervision::run`) | Owner's full view, real HOME |
| Legacy thread agents | Herdr `agent.start` (`src/launch_jobs.rs:35`, `ticker.rs:873`) | none on the agent | Owner's full view, real HOME |
| Signed routines | `routines/execution.rs:44` | `unshare --user --pid --mount-proc`, env cleared | Owner's full view (not a model agent; owner-signed script) |
| Legacy routine commands | `legacy_routine_jobs.rs:84` | `supervision::run` (inherits HOME, SSH_AUTH_SOCK, XDG_*) | Owner's full view (owner-approved command) |
| Verification checks | `verification/supervise.rs` + `verification-setup` | `pivot_root` into a copied checkout | Already isolated (no network namespace) |
| Herdr server, ticker, CLI | owner | none | Owner |

`worker_supervision::command` is used only by canonical workers and the native
probe. Everything the coordinator and legacy threads run is started by the
owner's Herdr server with the pane shell's environment; this wrapper cannot
reach them.

## Mechanism

`isolated_gated_command` now takes an `Isolation`, derived by
`Isolation::for_agent` from retained inputs only: the project, execution home,
working directory, agent executable, approved repositories, the pinned owner
config path, the Herdr socket, owner-declared extra paths and the controller's
owner identity. The released gate execs, in the supervisor's user/PID/mount
namespaces:

```
env -i /bin/sh -c <SANDBOX> herdr-projects-worker-sandbox <root> expose:<p>... hide:<p>... -- \
  /usr/bin/env -i HOME=<execution home> PATH=/usr/bin:/bin LANG=... <agent> <args>
```

`SANDBOX` (single literal line in `src/worker_supervision.rs`), running as
root of the supervisor user namespace U1, which owns mount namespace M1:

1. Opens each exposed path (own project dir, `<root>/.execution.lock`, and any
   needed path under the root such as an execution home there) on fds 3..9.
2. Mounts an empty tmpfs over the projects root, recreates mountpoints and
   binds each exposed path back from `/proc/self/fd/N` (so the bind reaches
   the real inode: the root barrier lock stays the same file and still
   excludes the ticker's exclusive root operations), then remounts the root
   tmpfs read-only.
3. Mounts an empty read-only tmpfs over every hidden directory and binds
   `/dev/null` read-only over every hidden file; missing paths are skipped.
4. Owner sockets in `/tmp` (`/tmp/ssh-*` agent dirs and `/tmp/tmux-*` server
   dirs owned by the owner) are covered the same way, enumerated at setup.
5. Re-enters the working directory by path through the new mount tree, so a
   stale cwd reference cannot `..` its way into the covered root.
6. `exec unshare --user --map-root-user -- env -i ... <agent>`: the agent runs
   in a nested user namespace U2. It is root there, but has no capability
   over M1 (owned by U1), so `umount`, `mount`, remount and move all fail with
   EPERM. If it creates its own user+mount namespace, the copied mounts are
   locked (`MNT_LOCKED`): they cannot be unmounted, and a bind of a parent
   directory refuses or carries the covering mount along.

Every step is `|| exit 125`: the agent never runs with a partial view.
The process chain is exec-only, so the agent is still the direct child of
namespace PID 1 with exactly its approved argv, as `agent_process` requires.
The owner-side observer still reads the agent's `/proc` entries (the owner is
the creator of U1 and so has all capabilities in U1 and U2).

### What is hidden

For each owner home (the passwd entry for the euid and the controller's
`HOME`): `.ssh`, `.gnupg`, `.codex`, `.claude/.credentials.json`,
`.config/herdr-projects` (includes `config.toml`/`owner.toml` and
`review-signer/`), `.config/herdr` (Herdr session sockets), `.config/gh`,
`.aws`, `.docker`, `.kube`, `.password-store`, `.local/share/keyrings`,
`.git-credentials`, `.netrc`. Also: `/run/user/<uid>` (gpg-agent, keyring,
D-Bus, systemd ssh-agent sockets), the pinned owner config file and
`<its dir>/review-signer`, the route's Herdr socket (its directory when that
directory is dedicated, else the socket file), `/tmp/ssh-*` and `/tmp/tmux-*`
owned by the owner, and everything under the projects root except the
worker's own project and the root lock.

**Owner signing key.** The product never opens the owner's private key and
does not know its path (docs say "held outside worker execution"). If it lives
in `~/.ssh` or `~/.gnupg` it is hidden. Otherwise the owner declares it in the
pinned config:

```toml
[worker_isolation]
hide = ["~/keys/herdr-owner", "/secure/elsewhere"]   # absolute, or ~/ per owner home; at most 16
```

These bytes are covered by the config digest the approval already pins, so the
extra list is as immutable as the profile.

### What stays visible

System directories, tool binaries, the execution home (with its own auth copy),
the working directory/worktree, the approved source repositories and their git
common directories (objects, refs, `worktrees/<id>` for commits), `/tmp`, the
product binary, and the worker's own project directory (needed by
`result submit`/`review submit`, which open `<project>/.state/state.db`,
`factory-objects/` and the project locks).

### Fails closed

- A hidden path that would cover a needed path (execution home, cwd, agent
  executable, project, repository) or lie inside the execution home or project
  is refused before any Herdr request (`worker isolation would hide ...`).
  E2E: `a_hidden_path_covering_the_execution_home_refuses_the_launch_before_creation`.
  An execution home equal to the owner's real HOME (which contains `.codex`)
  is therefore refused: workers need a dedicated execution home.
- A working directory under the projects root but outside the worker's own
  project is refused.
- Kernel/tool failure (no mount namespace, `mount` failing, nested user
  namespaces unavailable, e.g. `max_user_namespaces` exhausted or an LSM that
  restricts unprivileged user namespaces) exits 125 (or unshare's 1) before
  the agent runs; the supervisor then exits and the ticker observes an
  unexpected worker exit (task blocked, capacity retained until termination).
  Scratch evidence with a nested-namespace limit of 1 in an enclosing
  namespace:

  ```
  == nested user namespace unavailable
  unshare: unshare failed: No space left on device
  exit=1          # agent command (touch AGENT-RAN) never ran
  == control: same argv without the limit
  cat: .../h/.ssh/k: No such file or directory    # hidden
  exit=0, AGENT-RAN created
  ```
- A missing `unshare`/`timeout` or kernel without user/PID/mount namespaces
  already failed before this change (the outer supervisor never starts).

### Approval and identity impact

The wrapper is **not** part of any approval digest. `LaunchInputs`,
`FrozenProfile` (agent path, `arguments_digest`, `execution_home`, config
reference) and all attempt/operation IDs are unchanged. The supervisor argv is
recorded only as the per-launch `command_digest` in the creation intent (a
recovery fence) and is recomputed at gate release and compared with the live
gate process. Consequences:

- A launch created by an older binary and released by this one fails closed
  at gate release (argv mismatch); terminate and relaunch it.
- The argv depends on the controller's `HOME`. Restarting the ticker with a
  different `HOME` between creation and release fails that release closed.
- Adding `[worker_isolation]` changes the config digest, which (as for any
  config edit) withdraws existing launch authority until re-approved.

## Evidence

- `an_isolated_worker_cannot_read_owner_secrets_or_lift_the_hiding_but_still_commits_and_submits`
  (`tests/canonical_worker.rs`): full ticker path on the Herdr stand-in; the
  agent is a compiled probe. Planted sentinels in the temp HOME's `.ssh`,
  `.gnupg`, `.codex/auth.json`, `.config/herdr-projects/{notes,review-signer/}`,
  the owner config, another project (`root/other`), the Herdr stand-in's
  directory, a `/tmp/ssh-*` agent directory, and the owner signing key
  declared in `[worker_isolation]`. Asserted: every read fails (the declared
  key file reads as empty `/dev/null`); no sentinel appears anywhere in the
  report, including output from a user+mount namespace the agent creates;
  `umount2` (plain and `MNT_DETACH`) and `mount` on each hidden path and the
  root fail with EPERM; the root lists only `.execution.lock,demo`; the Herdr
  socket is unreachable; the agent reads its own `agent-home/.codex/auth.json`;
  it writes and commits `work.txt` on its attempt branch (visible in the
  source repository's object store); and `herdr-projects result demo submit`
  run inside the sandbox records the submission. The owner's files are
  unchanged.
- Existing canonical worker, controller and native-probe tests run the new
  argv end to end (gate observation, exact-agent observation, termination).

## Residual risks

1. **Writes to the worker's own project store.** The project directory is
   exposed read-write because `result submit`/`review submit` run inside the
   worker and open `state.db`. A worker can therefore write its own project's
   SQLite store, objects and other attempts' worktrees/outputs directly,
   bypassing the CLI (signature-verified approvals and contracts still refuse
   forged authority at their own checks, but raw rows are not authority-proof).
   *Proposal:* a submission spool. Expose to the worker only its worktree(s),
   its output directory and `.state/spool/<attempt>/` (write-only drop box);
   the ticker ingests spooled `result`/`review` documents under its own locks
   and the store is no longer visible. `result submit` inside a worker writes
   the spool file instead of the store.
2. **Write-based escapes.** Hiding stops reads, not writes. With the owner's
   uid the worker can still write `~/.bashrc`, `~/.profile`, `~/.local/bin/*`,
   `~/.config/systemd/user/*`, autostart entries, and the approved source
   repository's `.git/hooks` and `.git/config` (run by the owner's own git) or
   working tree files, gaining code execution as the owner outside the
   sandbox. *Proposal (next card):* bind the owner's HOME read-only in the
   sandbox with explicit writable exposures (execution home, own worktrees,
   output dir, git common dir with `hooks/`, `config` and `info/` bound
   read-only), and give workers a private `/tmp`.
3. **Other reachable control channels.** The network namespace is shared:
   abstract Unix sockets (X11 `@/tmp/.X11-unix/*`, some D-Bus), localhost
   services, and sockets reachable through supplementary groups (e.g. a
   `docker` group member's `/run/docker.sock`, which is root-equivalent; the
   owner here is not in `docker`) remain reachable. SSH agent sockets outside
   `/tmp/ssh-*`/`/run/user` (custom `SSH_AUTH_SOCK`) are not hidden.
4. **Unlisted secret locations** are not hidden (e.g. browser profiles,
   cloud CLIs other than those listed, secrets inside source repositories, or
   an owner key outside `~/.ssh`/`~/.gnupg` that the owner has not declared
   in `[worker_isolation]`). Other repositories under the owner's HOME (e.g.
   `~/git/*`) are readable.
5. **Host-side recreation lifts a hide.** Since Linux 3.18, deleting a
   mountpoint in another mount namespace detaches the mounts on it. If the
   owner deletes and recreates a hidden directory or file (or Herdr recreates
   its socket in a shared directory) while a worker runs, that one path
   becomes visible to that worker. The worker cannot trigger this itself
   (unlinking its own mountpoints fails with EBUSY). Dedicated socket
   directories are hidden whole to avoid the Herdr-restart case.
6. **Hidden files read as empty.** A hidden file is `/dev/null`, so the worker
   learns it exists (and its path), not its contents.
7. **Coordinator and legacy thread agents** run from Herdr `agent.start`
   with the owner's full view and real HOME; they can read everything this
   card hides from workers, and the coordinator drives the CLI with owner
   authority. *Proposal:* launch the coordinator through the canonical
   supervisor (`workspace.create_command`/`launch-exec` with the same
   `Isolation`, exposing the whole projects root it plans over but hiding
   keys, the reviewer-signer directory and `/run/user`), and retire or wrap
   legacy thread launches the same way. Stock `agent.start` cannot carry the
   wrapper.
8. **Same-project neighbours.** Other attempts of the worker's own project
   (worktrees, outputs, snapshots, `PROJECT.md`) remain readable/writable
   (closed by the spool proposal in 1).
9. **Owner homes are anchors.** Hiding uses the passwd home and the
   controller's `HOME`; a secret under a third home-like path needs
   `[worker_isolation]`.

## Tests run

- `cargo test --features state-store --test canonical_worker --test cli
  --test factory_harness --test scheduling --test recovery --test ticker_jobs
  --bin herdr-projects --lib`: all pass (lib 612, bin 365, canonical_worker 18,
  cli 81, factory_harness 16, recovery 5, scheduling 7, ticker_jobs 8).
  `canonical_worker`, `ticker_jobs` and `--lib` were re-run after the `/tmp`
  socket-directory addition and pass.
- Default features, `--test cli --bin herdr-projects`: pass (30, 284). One run
  hit a timing flake in `local_reports::tests::unused_hash_survives_pending_session_passes_without_renewing_expiry`
  (executor timing; the code is unrelated to this change), which passed on re-run.
- No new clippy warnings in changed files (`tests/live_phase_a.rs` call sites
  updated to the new signature).
