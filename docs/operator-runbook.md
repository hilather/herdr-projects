# Operator runbook: canonical workers on a real project

For a project already migrated to the canonical store with control active, this
runs a task on a Codex or a Claude Code worker from the CLI alone: no SQL, no
hand-edited agent configuration. Everything signed is signed by the owner with
`ssh-keygen -Y sign`; the application never reads a private key except through
that command and only for a path you pass explicitly.

## 0. Once per machine

* The owner is logged in with the agent CLIs (`codex login`, `claude`). Workers
  reuse that login (see [Shared login](profiles.md#shared-login)); nothing is
  copied.
* A pinned owner configuration (`~/.config/herdr-projects/config.toml`) with the
  `[authority]` key and one profile per worker setup, for example:

```toml
[profiles.codex-sol]
kind = "codex"
permission_policy = "interactive"
model = "gpt-6.1-sol"
reasoning_effort = "low"
[profiles.codex-sol.budget]
max_wall_seconds = 3600
unknown_usage = "allow_with_warning"

[profiles.claude-sonnet]
kind = "claude"
permission_policy = "interactive"
model = "claude-sonnet-5-5"
reasoning_effort = "low"
[profiles.claude-sonnet.budget]
max_wall_seconds = 3600
unknown_usage = "allow_with_warning"
```

  Changing this file changes its digest: acknowledge it (`runtime PROJECT state
  active ...`) as usual after editing.
* A dedicated, owner-controlled execution home per profile, outside the project
  and outside the owner's real agent directories (for example
  `~/.herdr-projects-homes/codex-sol`).
* An existing branch of the repository, not checked out, for results to integrate
  into (for example `git branch integration`).

## 1. Verify each profile (once, and after any agent or Herdr upgrade)

```sh
HERDR=/absolute/path/to/herdr          # the exact binary you will run the ticker with
herdr-projects profile verify-interaction PROJECT codex-sol \
  --herdr-executable "$HERDR" --agent-executable /absolute/path/to/codex \
  --execution-home ~/.herdr-projects-homes/codex-sol --retain
herdr-projects profile verify-interaction PROJECT claude-sonnet \
  --herdr-executable "$HERDR" --agent-executable /absolute/path/to/claude \
  --execution-home ~/.herdr-projects-homes/claude-sonnet --retain
```

Each starts the real agent in a disposable Herdr server inside the execution
home's `.hp-verify-work` directory (trusted for you), checks readiness and the
pinned model, sends one fixed diagnostic prompt and stops it. The output's
`preparation.launchable` must be `true` and `evidence.interaction.pinned` must
show your model and effort. It uses a little account usage. Use the actual
executable, not a version-manager shim.

## 2. Run a planning task per worker

A new runtime binding pauses the project until no attempt is unfinished, and
parallel workers need disjoint write scopes (each planning task writes only its own
`docs/...md`). So prepare every task first, then reserve them. Write the task text
(`prompt.md`), then:

```sh
export HERDR_BIN_PATH="$HERDR"
common="--repository /path/to/repo --prompt-file prompt.md --integration-ref refs/heads/integration \
  --sign-with ~/.ssh/owner_key --max-active-workers 2"
# 1. prepare both: contract, queue, capacity, Herdr server, binding, reconcile, activate
herdr-projects launch PROJECT run --task plan-codex  --profile codex-sol      --plan-output docs/plan-codex.md  $common --prepare-only
herdr-projects launch PROJECT run --task plan-claude --profile claude-sonnet  --plan-output docs/plan-claude.md $common --prepare-only
# 2. reserve both: knowledge snapshot, draft, signed approval, import, reserve
herdr-projects launch PROJECT run --task plan-codex  --profile codex-sol      --plan-output docs/plan-codex.md  $common
herdr-projects launch PROJECT run --task plan-claude --profile claude-sonnet  --plan-output docs/plan-claude.md $common
```

Per run it: finds the retained launchable evidence for the profile; makes the project
active (an owner configuration acknowledged by an active project is what lets a signed
contract be installed); adds the task; builds and signs (namespace
`contract@herdr-projects`) the planning contract (the single deliverable `docs/...md`,
write scope exactly that file, acceptance: the file exists and has content, route
`verify_then_integrate` with `--integration-ref`); installs it; queues the task; sets
scheduler capacity; configures the integration target and turns verify+integrate
automation on; starts a dedicated Herdr server for the task under
`PROJECTS_ROOT/.herdr-run/PROJECT-TASK/` (or uses `--herdr-socket` for a server you run);
binds the task to it, reconciles and activates. Without `--prepare-only` it then
retains the knowledge snapshot (PROJECT.md followed by your prompt and the
deliverable instruction), drafts the launch, signs the approval
(`approval@herdr-projects`), imports it and reserves the attempt. The JSON report lists
every step as `done` or `already_done`, the attempt id and its worktree.

It is idempotent: rerun the same command after fixing a failure and finished steps are
skipped; once a task has its attempt a rerun only reports it. A failure names the step
(`launch run stopped at step N (...)`) and what completed before it. A task that needs a
new binding while another attempt is unfinished is refused before anything changes (the
project would be paused under a live worker). Without `--sign-with` it stops at the first
document needing a signature and tells you how to sign it by hand. A task with your own
contract uses `--contract-file` instead of `--plan-output`.

`HERDR_BIN_PATH` must be the absolute path of the Herdr the profiles were verified with,
both for this command and for the running ticker (start the ticker with it set; the
controller launches the reserved attempts).

## 3. Watch and collect

```sh
herdr-projects scheduler PROJECT inspect
herdr-projects operations PROJECT inspect
herdr-projects result PROJECT capture ATTEMPT      # after the worker has written its document
herdr-projects result PROJECT jobs                 # verification and integration
herdr-projects telemetry PROJECT attempts
```

The worker writes `docs/plan-*.md` in its worktree and does not commit; the
controller's `result capture` commits it on the attempt branch, verification runs
the signed acceptance policy, and integration lands it on the integration branch.
