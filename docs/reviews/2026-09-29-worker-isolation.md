# Worker filesystem isolation (2026-09-29)

Card: "worker filesystem isolation", branch `security/worker-fs-isolation`.
Owner decision: *isolate workers first*. Hide sensitive paths from workers
with empty mounts inside the worker's namespaces, and run the worker in a
further nested namespace so it cannot remove those mounts.

Follow-up card: "worker write isolation and full coverage", branch
`security/worker-write-isolation` (owner decision: isolate workers rather than
accept the write risk). It adds read-only mounts and private scratch
directories ([Write isolation](#write-isolation-follow-up-card)), narrows the
project exposure, designs the submission spool and coordinator wrapping, and
fixes the isolation test's flake. Residual risks below carry their current
status.

Follow-up card: "submission spool", branch `security/submission-spool`. It
builds the spool designed by the write-isolation card: the worker's project
`.state` becomes read-only, and `result submit` and the review worker channel
reach the store only through the ticker
([Submission spool](#submission-spool-follow-up-card)).

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
common directories, the product binary, and the worker's own project directory
(read-only since the spool card: `result submit`/`review submit` no longer
open `<project>/.state/state.db` inside the worker, they write to the
attempt's spool). Since the write-isolation card most of this is read-only
and `/tmp` is private; see [Write isolation](#write-isolation-follow-up-card)
and [Submission spool](#submission-spool-follow-up-card).

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

## Write isolation (follow-up card)

### Mechanism

`Isolation::for_agent` now also takes the attempt's retained worktree receipts
(worktree, Git directory, common directory from the durable
`runtime.worktrees_ready` event, so creation, gate release and any later
recomputation derive the same argv). The `SANDBOX` script gains two steps
between covering the projects root and hiding:

1. **Private scratch directories.** `/tmp`, `/var/tmp` and `/dev/shm` are each
   replaced by an empty `nosuid,nodev,mode=1777` tmpfs. Entries directly under
   them that hold a path the agent needs (execution home, working directory,
   agent, project/root, repositories, worktree Git directories; name and real
   path) are opened first and bound back recursively (`mount -c --rbind
   /proc/self/fd/N`, so libmount does not re-resolve the name into the new
   tmpfs). A missing or symlinked directory is skipped. At most 7 entries each.
2. **Read-only plan.** Entries sorted parents first; each is bound onto itself
   recursively in one call, either read-only throughout (`mount --rbind -o
   ro=recursive`, which libmount applies with mount_setattr) or writable at
   its top mount (`mount --rbind -o rw`). Missing paths are skipped; an
   entry inside one of the same mode is dropped in Rust (read-only is
   recursive and every submount exists by then), which saves mount execs.

| Path | Mode |
|---|---|
| Each owner home (passwd and controller `HOME`) | read-only |
| Each approved source repository (its working tree) | read-only |
| The worker's project directory (`PROJECT.md`, anything outside `.state`) | read-only |
| `<project>/.state` (store, WAL, locks, `factory-objects/`, other attempts' outputs and spools) | read-only (writable before the spool card) |
| `<project>/.state/worktrees` (other attempts' worktrees) | read-only |
| `<project>/.state/spool/<attempt>` (this attempt's submission spool) | writable (spool card) |
| `<project>/.state/worker-output/<attempt>` (this attempt's output directory) | writable (spool card) |
| The attempt's own worktrees | writable |
| `<worktree>/.git` (gitdir pointer) | read-only |
| Git common directory (`config`, `hooks/`, `info/`, `packed-refs`, other refs, `HEAD`) | read-only |
| `<common>/objects`, `refs/heads`, `logs/refs/heads`, `reftable` | writable |
| `<common>/objects/pack`, `objects/info` (alternates) | read-only |
| `<common>/worktrees/<id>` (index, HEAD, per-worktree logs, `COMMIT_EDITMSG`) | writable |
| `<common>/worktrees/<id>/{commondir,gitdir,locked}` | read-only |
| Execution home, `<root>/.execution.lock` | writable |

The worker (in the nested user namespace U2) cannot remount any of these
writable: the mounts belong to M1, owned by U1.

**What a worktree commit writes (verified, Git 2.55, files backend):** new
loose objects in `objects/xx/` (temporary file + link), the branch ref
`refs/heads/<branch>` via `refs/heads/<branch>.lock` + rename (so the directory,
not the file, must be writable), its reflog `logs/refs/heads/<branch>`
(append), and in `worktrees/<id>/`: `index`/`index.lock`, `logs/HEAD`,
`COMMIT_EDITMSG`, `ORIG_HEAD`. After the commit Git deletes the per-worktree
`AUTO_MERGE` pseudo-ref, which takes `packed-refs.lock` in the common
directory; that fails with a printed `Unable to create
'.../packed-refs.lock': Read-only file system` but the commit exits 0 and the
branch moves. Scratch check of the same layout: `commit`, `commit --amend`,
`reset --soft`, `checkout -b` succeed; `stash` (writes `refs/stash`) and
`branch -D` (rewrites `packed-refs`) fail.

**Needs:** util-linux 2.38+ and Linux 5.12+ for `ro=recursive`; otherwise the
sandbox exits 125 before the agent runs (fails closed). A binding without
repositories whose working directory lies in a read-only anchor (e.g. under
the owner's HOME) is now read-only there; the worker writes to its output
directory.

**Start observation.** The added mounts lengthen the gap between gate release
and the agent's exec. Start confirmation (`canonical_worker/start.rs`) used to
look for the exact agent once, right after release, and fail the pass if the
sandbox was still running (the ticker retried on a later pass; unit tests that
advance once failed). It now waits, at most 10 s and within the operation
deadline, only while `SupervisorObservation::agent_setup_pending` sees the
released gate's sandbox stages (`sh` named `herdr-projects-worker-sandbox`,
`unshare`, `env -i`) as the namespace init's sole child. A gate still waiting
for release, a missing child or any other process fails at once as before;
the observation is evidence for the wait only, never authority.

**Identity impact:** as before, only the supervisor argv (the per-launch
`command_digest`) changes; `LaunchInputs`, `FrozenProfile`, approval digests and
IDs are unchanged. A launch created by the previous binary and released by this
one fails closed at gate release (argv mismatch); terminate and relaunch it.

### Evidence

`an_isolated_worker_cannot_read_owner_secrets_or_lift_the_hiding_but_still_commits_and_submits`
now also plants `~/.bashrc`, `~/.local/bin/`, `~/.config/systemd/user/` in the
temp HOME and a sentinel directory in the host `/tmp`. The probe agent tries to
append to `~/.bashrc`, create `~/.local/bin/x` and
`~/.config/systemd/user/x.service`, create the repository's
`.git/hooks/pre-commit` and `.git/packed-refs`, append to `.git/config`,
`.git/info/exclude`, the repository's working tree (`README`), the project's
`PROJECT.md` and its own worktree's `.git` pointer; every attempt fails
(`ReadOnlyFilesystem`) and the host bytes are unchanged before and after the
submission. The host `/tmp` sentinel is unreadable and absent from the worker's
`/tmp` listing (so is the planted `ssh-*` agent directory); a file the worker
writes in `/tmp` reads back inside and never appears on the host. The worker
still commits `work.txt` on its attempt branch (visible from the owner's
repository) and `result submit` inside the sandbox records the submission.
Observed report excerpt:

```
write <home>/.bashrc ERR:ReadOnlyFilesystem
write <home>/.local/bin/x ERR:ReadOnlyFilesystem
write <home>/.config/systemd/user/x.service ERR:ReadOnlyFilesystem
write <home>/repo/.git/hooks/pre-commit ERR:ReadOnlyFilesystem
write <home>/repo/.git/config ERR:ReadOnlyFilesystem
tmp true private
tmp-list .tmpugx2FX,herdr-projects-host-TTNVQZ-worker
commit true error: Unable to create '<home>/repo/.git/packed-refs.lock': Read-only file system|
```

(`tmp-list` shows only the kept lab HOME and the worker's own file.)

### Flake: `ok_live` timeout in the isolation test

Cause (reproduced with instrumentation, not assumed): the test read
`running = lab.attempt(..)` right after the submission and retried
`cancel-attempt --expected-revision <running.revision>`. The released probe
agent runs immediately, so it can finish and submit before the ticker records
the brief delivery and moves the attempt from `Launching` (revision 2) to
`Running` (revision 3). Every retry then failed with `state store: Conflict`
until the 30 s deadline. Instrumented run:

```
REV 2 Launching head 35 ...           # when `running` was captured
REV 3 Running head 38 [... attempt.changed, runtime.worker_brief_delivered]
STALE exit status: 1 herdr-projects: state store: Conflict
```

Load only widens the window (the ticker's brief delivery lags the agent).
Three runs under bounded extra load (6 busy loops, killed after ≤290 s; host
load average ~20 on 12 cores) did not hit it; the instrumented run above did,
deterministically, once the probe finished before the brief. Fix: wait for
`Running` before cancelling and read the attempt's current revision on every
retry. With the fix the test passed in every later run (two full
`canonical_worker` suites under host load).

## Submission spool (follow-up card)

Owner decision: build the designed spool. Residual risk 1 was that the
worker could write its own project's `.state/state.db` directly, because
`result submit` ran the product binary inside the sandbox.

### Design

- **Exposure.** `Isolation::for_agent` binds the whole project, `.state`
  included, read-only. `Isolation::with_submission_spool(attempt)` (called by
  the canonical launch service, `canonical_worker/resources.rs::command`) adds
  exactly two writable binds under it: `.state/spool/<attempt>` and
  `.state/worker-output/<attempt>`, and puts
  `HERDR_PROJECTS_SUBMISSION_SPOOL=<project>/.state/spool/<attempt>` in the
  agent's baseline environment. Gate release (`release_gate`) creates both
  directories (0700, no link followed: `source_tree::Directory`) before the
  release line is sent; the sandbox binds only existing paths, so a missing
  one stays read-only and submission fails closed. Another attempt's spool
  and outputs are read-only to this worker (it sees `.state` read-only).
- **Worker side** (`src/submission_spool.rs`, `exchange`). With the variable
  set, `result submit`, `review submit`, `review session` and `review
  present` do not open the store. They read the input as today (same bounds
  and errors), build one canonical request
  `{"version":1,"kind":…,"attempt_id":<spool's attempt>,"document"|"argument":…}`
  (exact `serde_json` bytes), write it to a temporary created
  `O_CREAT|O_EXCL|O_NOFOLLOW`, mode 0600, and rename it to
  `<sha256(bytes)>.request`. They remove a stale `<sha256>.receipt` first (so
  a rerun asks again and the store replays), then poll, bounded (180 s), for
  the receipt, read it `O_NOFOLLOW` (regular file, 1 MiB cap), and print its
  `stdout` verbatim or fail with its `error`, exactly as the command prints
  without a sandbox. A timeout leaves the request queued; rerunning waits
  again. The worker may forge its own receipt: that changes only what it
  prints to itself. Outside the sandbox (variable unset) nothing changes.
- **Ticker side** (`submission_spool::ingest`). In every canonical project
  pass (after the controller poll) and, between passes, whenever a cheap
  directory scan (`pending`) finds a request with nothing at its receipt name
  (so a waiting worker is answered within about half a second), the ticker:
  opens `.state/spool` and each attempt directory `O_DIRECTORY|O_NOFOLLOW`
  and reaches entries only relative to that descriptor; reads the attempt's
  state from a read-only store view; takes the project runtime guard that
  `result submit` takes (if an effect or maintenance holds the project, the
  requests stay queued for a later pass); and for each request without a
  regular single-link receipt (at most 8 per attempt and pass, at most 256
  entries per spool) checks, in order: regular single-link file opened
  `O_NOFOLLOW|O_NONBLOCK`, at most 1 MiB; the name is the SHA-256 of the
  content; strict parse (`deny_unknown_fields`) whose re-serialisation equals
  the bytes; `attempt_id` equals the directory; the attempt is live
  (`reserved|launching|running|awaiting_input`, termination not observed);
  the document is the attempt's own (result `attempt_id`; review receipt's
  session launched for this attempt; `review session` asks for this attempt;
  `review present` asks for the opportunity this attempt reviews). Then it
  runs the same code the CLI runs: `SqliteStore::submit_result` through
  `store::submit_untrusted_result_bytes` (idempotency key/digest replay,
  object staging, all refusals unchanged) or the review worker channel
  (`telemetry::review::worker_submit|worker_session|worker_present`, the same
  functions `review submit|session|present` call). The receipt carries the
  command's stdout or its error text; it is written to an exclusive
  `O_CREAT|O_EXCL|O_NOFOLLOW` temporary in the spool directory, fsynced and
  `renameat` over `<sha256>.receipt`, so any entry the worker left there (a
  symbolic link or hard link included) is replaced, never followed or
  truncated; a directory there is removed first, or the request waits.
- **Denials.** A request refused by the spool checks (not a store refusal,
  which is an ordinary answer as today) is answered with `submission spool
  refused the request: <reason>`, logged, and recorded as a
  `spool.request_denied` event of the attempt (payload: request digest, kind,
  reason, time; revision = ordinal). At most 16 are recorded per attempt, so
  a worker cannot grow the store or move the head without bound; later ones
  are logged only.
- **Crash and replay.** The receipt is written after the store commit. A
  crash in between leaves the request without a receipt; the next pass runs
  it again and the store replays it (`replayed: true`, same
  `submission_id`). A request whose attempt ended before it was answered is
  refused (`the attempt is not live`). Once the attempt's termination is
  observed, the ticker removes its spool entries (unlinking, never following)
  and the directory; spools of attempts the store does not know are refused
  and left in place.
- **Output directory.** Gate release now pre-creates
  `.state/worker-output/<attempt>`, so output capture
  (`worktree_preservation/outputs.rs`) records an empty directory as absent
  (`digest: None`), as the design required; a directory with any entry is
  captured as before.
- **Identity.** `LaunchInputs`, `FrozenProfile`, approval digests and all IDs
  are unchanged. Only the supervisor argv changes (one more `rw:` bind pair
  and the environment entry), which is the per-launch `command_digest` fence:
  a launch created by the previous binary and released by this one fails
  closed at gate release (argv mismatch); terminate and relaunch it.

### What else writes `.state` (checked)

Nothing the worker legitimately runs needs `.state` writable any more:

- worktree preparation, gate release, brief delivery, memory snapshots and
  worker-brief rendering, output capture and snapshots, repository capture
  (`result capture`), verification and integration all run in the ticker or
  the owner's CLI, outside the sandbox;
- the worker's own worktree lies under `.state/worktrees/` and keeps its
  existing writable bind (unchanged by this card); its Git writes go to the
  source repository's common directory (unchanged);
- the root lock `<root>/.execution.lock` stays writable (outside `.state`);
  the spooled commands take no lock inside the sandbox.

Exceptions, documented: the worker's own `.state/spool/<attempt>` and
`.state/worker-output/<attempt>` are writable; its worktree under
`.state/worktrees/` is writable as before. Other product commands that
write the store now fail inside the sandbox; they were already refused in a
worker context (§9 of contracts-review.md) or are owner commands. Store
readers that open `state.db` read-write (`SqliteStore::open`) fail on a
read-only mount whenever SQLite's `-wal`/`-shm` files are absent (verified:
`unable to open database file`), which is why `review session` and `review
present` go through the spool as well; readers built on
`telemetry::read_only` (shared lock, `immutable` when no side files) still
work.

### Evidence

- `an_isolated_worker_submits_only_through_its_own_spool`
  (`tests/canonical_worker.rs`, real ticker, Herdr stand-in, compiled probe
  agent): inside the sandbox, opening `state.db` for writing, creating files
  in `.state`, `.state/factory-objects/`, another attempt's spool and another
  attempt's output directory all fail with `ReadOnlyFilesystem`; its own
  output directory and spool are writable (the report reaches the host).
  The probe plants requests by hand in its own spool: a malformed one, an
  oversized one (1 MiB + 1), a canonical request naming another attempt, a
  request whose result names another attempt, a request that is a symbolic
  link, and a symbolic link at the malformed request's receipt name pointing
  at the owner's `~/.bashrc`. Each gets a regular-file receipt with its
  refusal, each reason is recorded as `spool.request_denied`, and `~/.bashrc`
  is unchanged. `result submit` run twice inside the sandbox prints the
  store's receipt (`replayed: false`, then `true` with the same
  `submission_id` and `payload_digest`, which is the SHA-256 of the document
  bytes); `result show` lists one submission. After cancel and termination
  the ticker removes the attempt's spool.
- `a_sandboxed_reviewer_uses_its_worker_channel_through_the_spool` (real
  review launch, blind brief, sandboxed probe): `review session` for its own
  attempt prints its session; for the author's attempt it is refused and
  recorded; `review present` prints its opportunity's blind view; `review
  submit` records a `proposal` as `worker:<attempt>` and the same receipt
  replays (`replayed: true`, same `receipt_digest`).
- `an_isolated_worker_cannot_read_owner_secrets_or_lift_the_hiding_but_still_commits_and_submits`
  keeps passing: its `result submit` now goes through the spool.

## Designed, not built

### Coordinator and legacy thread agents (residual risk 7)

Not built: covering them safely does not fit this card.

- **Why it is not a wrapper change.** Both start through Herdr `agent.start`
  into an existing pane (the coordinator's pane chosen at `open`, a thread's
  pane; remote threads through `--machine`). Herdr resolves the executable
  from the agent kind and returns `agent_started` with the argv the product
  checks; there is no hook for a supervisor argv. The coordinator also runs on
  the owner's own agent login in the real HOME (`~/.claude`, `~/.codex`),
  which isolation hides.
- **Design.** (a) Add an owner-configured coordinator execution home (a new
  `[coordinator]` config key: config digest change, so re-approval, but no
  `LaunchInputs` change for canonical attempts). (b) Start the coordinator
  like a canonical worker: `workspace.create_command` (patched Herdr) or typed
  `launch-exec` (stock Herdr) with `isolated_gated_command`, a coordinator
  `Isolation` whose exposure list is: the projects root read-only except
  writable `<project>/.state` and `<project>` itself for every project it
  plans (it runs `task add`, `launch draft`, `approval import` and memory
  commands, all of which write stores and locks), `<root>/.execution.lock`
  (not the ticker lock: it does not run the ticker), the product binary, the
  approved repositories read-only, and its execution home writable; hidden:
  everything in the worker hide list plus `/run/user/UID`, the Herdr socket
  (it drives panes only through the product CLI, which would then need a
  scoped bridge) and `review-signer/`. (c) Replace pane adoption with a
  created coordinator workspace and adapt `pane_matches`, the launch claim and
  recovery to the canonical creation intent. Legacy local threads either
  retire in favour of canonical attempts or take the same path; remote
  threads cannot be wrapped from here and stay a documented exception.
- **Open question for the owner:** the coordinator currently reaches the Herdr
  socket (it can drive every pane). Isolating it means routing its Herdr use
  through a narrower product command; that is a product decision, not a
  wrapper detail.

## Residual risks

Status after the follow-up card in brackets.

1. **[Closed by the spool card] Writes to the worker's own project store.**
   The worker's project, `.state` included, is read-only; only its own
   spool and output directory (and its worktree, as before) are writable,
   and the ticker ingests spooled submissions through the store's own path
   ([Submission spool](#submission-spool-follow-up-card)). Remaining: the
   worker still *reads* `.state` (the store, other attempts' spools, outputs
   and snapshots), as it did before. Original text: the project directory is
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
2. **[Closed for the listed vectors; remainder open] Write-based escapes.**
   Closed: the owner's HOME (`~/.bashrc`, `~/.profile`, `~/.local/bin`,
   `~/.config/systemd/user`, autostart, other repositories under HOME), the
   source repositories' working trees, `config`, `hooks/`, `info/`,
   `packed-refs`, `objects/info/alternates` and `objects/pack`, the worktree's
   `.git` pointer and `commondir`/`gitdir`/`locked`, and a private `/tmp`,
   `/var/tmp`, `/dev/shm`. Remaining:
   (a) *shared refs*: `refs/heads/` must be writable for the branch update, so
   a worker can move or create any local branch of the source repository
   (e.g. point `main` at its own commit); the owner's next checkout/build of
   that branch runs worker-chosen code. (b) *loose objects*: `objects/xx/`
   must be writable, so a worker can replace an existing loose object's file
   with different bytes (Git does not re-hash on every read). Design for (a)
   and (b): commit into a quarantine object directory
   (`GIT_OBJECT_DIRECTORY` + read-only alternates, like receive-pack
   quarantine) and a per-attempt ref namespace, with the ticker migrating
   verified objects (`index-pack`/re-hash) and moving only the attempt branch
   after the worker ends; this changes where commits become visible and is a
   separate card. (c) If the owner enabled `extensions.worktreeConfig`, the
   worker can write `worktrees/<id>/config.worktree`, which applies to Git run
   inside its worktree (the product's own Git runs override `core.fsmonitor`
   and `core.hooksPath`, not every exec-capable key). (d) Owner-writable
   locations outside HOME and the scratch directories (e.g. removable media
   under `/run/media/<user>`, owner-owned directories elsewhere) are not in
   the plan. Original text: hiding stops reads, not writes. With the owner's
   uid the worker can still write `~/.bashrc`, `~/.profile`, `~/.local/bin/*`,
   `~/.config/systemd/user/*`, autostart entries, and the approved source
   repository's `.git/hooks` and `.git/config` (run by the owner's own git) or
   working tree files, gaining code execution as the owner outside the
   sandbox. *Proposal (next card):* bind the owner's HOME read-only in the
   sandbox with explicit writable exposures (execution home, own worktrees,
   output dir, git common dir with `hooks/`, `config` and `info/` bound
   read-only), and give workers a private `/tmp`.
3. **[Open] Other reachable control channels.** The network namespace is shared:
   abstract Unix sockets (X11 `@/tmp/.X11-unix/*`, some D-Bus), localhost
   services, and sockets reachable through supplementary groups (e.g. a
   `docker` group member's `/run/docker.sock`, which is root-equivalent; the
   owner here is not in `docker`) remain reachable. SSH agent sockets outside
   `/tmp/ssh-*`/`/run/user` (custom `SSH_AUTH_SOCK`) are not hidden.
4. **[Open] Unlisted secret locations** are not hidden (e.g. browser profiles,
   cloud CLIs other than those listed, secrets inside source repositories, or
   an owner key outside `~/.ssh`/`~/.gnupg` that the owner has not declared
   in `[worker_isolation]`). Other repositories under the owner's HOME (e.g.
   `~/git/*`) are readable (now read-only).
5. **[Open] Host-side recreation lifts a hide.** Since Linux 3.18, deleting a
   mountpoint in another mount namespace detaches the mounts on it. If the
   owner deletes and recreates a hidden directory or file (or Herdr recreates
   its socket in a shared directory) while a worker runs, that one path
   becomes visible to that worker. The worker cannot trigger this itself
   (unlinking its own mountpoints fails with EBUSY). Dedicated socket
   directories are hidden whole to avoid the Herdr-restart case.
6. **[Open] Hidden files read as empty.** A hidden file is `/dev/null`, so the worker
   learns it exists (and its path), not its contents.
7. **[Open; designed] Coordinator and legacy thread agents** run from Herdr `agent.start`
   with the owner's full view and real HOME; they can read everything this
   card hides from workers, and the coordinator drives the CLI with owner
   authority. *Proposal:* launch the coordinator through the canonical
   supervisor (`workspace.create_command`/`launch-exec` with the same
   `Isolation`, exposing the whole projects root it plans over but hiding
   keys, the reviewer-signer directory and `/run/user`), and retire or wrap
   legacy thread launches the same way. Stock `agent.start` cannot carry the
   wrapper.
8. **[Closed for writes by the spool card; reads open] Same-project
   neighbours.** Other attempts' worktrees, `PROJECT.md`, `worker-output/`
   directories, spools and snapshots under `.state` are read-only; they stay
   readable. Original text:
   other attempts of the worker's own project
   (worktrees, outputs, snapshots, `PROJECT.md`) remain readable/writable
   (closed by the spool proposal in 1).
9. **[Open] Owner homes are anchors.** Hiding uses the passwd home and the
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

### Tests run (follow-up card)

Host under heavy unrelated load throughout (load average 15–22 on 12 cores).

- `cargo test --features state-store --test canonical_worker --test cli
  --test factory_harness --test scheduling --test recovery --test ticker_jobs
  --test telemetry_review --bin herdr-projects`: all pass (canonical_worker 18,
  cli 81, factory_harness 16, recovery 5, scheduling 7, telemetry_review 20,
  ticker_jobs 8, bin 365).
- `--features state-store --lib`: 612 pass. Before the fixes below, three
  `canonical_worker::tests` failed: the unit fixture built its worker argv
  before the worktree existed (it now predicts the retained receipt the way
  `git worktree add` names it), start confirmation raced the sandbox setup
  (now the bounded setup wait), and
  `launch_advancement_recovers_each_boundary_then_delivers_brief_and_stops`
  ran launch, brief, stop and preservation on one 15 s budget, of which
  launch alone took 5–6.5 s under this load (measured; now 30 s). Unloaded,
  gate release to agent exit is ~40 ms without and ~55 ms with a repository
  worktree plan. One unrelated `admission::tests` timing failure appeared in
  one loaded run and not again.
- Default features, `--test cli --bin herdr-projects`: pass (30, 284). One run
  hit `local_reports::tests::retained_unconsumed_hashes_have_a_global_cache_bound`
  (`Instant::now() < deadline`; unrelated code), which passed on two re-runs.
- No new clippy warnings in changed files.

### Tests run (spool card)

- `cargo test --features state-store --no-fail-fast --test canonical_worker
  --test cli --test factory_harness --test scheduling --test recovery --test
  ticker_jobs --test telemetry_review --test review_signer`: canonical_worker
  20 (two new), cli 81, factory_harness 16, recovery 5, review_signer 3,
  scheduling 7, telemetry_review 20 pass; ticker_jobs 7 of 8 in that run
  (`open_after_a_coordinator_start_keeps_its_claim_and_never_starts_again`,
  a legacy-coordinator timing assertion on a project without a spool), then 8
  of 8 on re-run together with canonical_worker (20) on the final code.
- `--features state-store --bin herdr-projects`: 365 pass.
- `--features state-store --lib`: 612 pass after updating
  `staged_stop_requires_output_evidence_and_records_an_empty_directory_as_absent`
  (was `..._preserves_an_empty_directory`) to the pre-created output
  directory contract.
- Default features, `--test cli --bin herdr-projects`: 30 and 284 pass.
- No new clippy warnings in changed files.
