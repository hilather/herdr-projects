# Agent profiles (W04 work in progress)

User-owned profiles live in `~/.config/herdr-projects/config.toml`. Inspect a
definition with `herdr-projects profile inspect implementation`. This command reads
configuration only; it does not start an agent, read environment values or create
project state. Output is JSON with redacted argument/intent/environment counts,
config and profile SHA-256 identities, and unresolved requirements.

```toml
[profiles.implementation]
kind = "codex"
permission_policy = "interactive"
extra_args = []
environment = []

[profiles.planner]
kind = "claude"
permission_policy = "interactive"
extra_args = []
```

Each profile owns its argument array. Arguments are literal user passthrough;
inspection does not verify vendor flags. `environment` contains variable names,
never `NAME=value` entries. Credentials should stay in the CLI's credential store.
Avoid secrets in argument arrays: they are redacted from inspection but still live
in your configuration. The permission policy is an unresolved reference, not a
permission grant.

Optional `model` and `reasoning_effort` strings express intent. They remain blocked
until an adapter verifies a mapping for the installed version. Optional `[profiles.NAME.budget]`
accepts positive `max_wall_seconds`, `soft_input_tokens` and `soft_output_tokens`,
with required `unknown_usage = "allow_with_warning"` or `"block"`. These are requested
limits. The gated resource adapter now requires a wall deadline of at most seven
days and checks the complete retained brief against `soft_input_tokens * 4`
characters (32,000 characters when absent), in addition to the retained snapshot's
own budget. This is a character estimate, not an exact provider token count.
Because that adapter has no provider usage collector yet, `unknown_usage = "block"`
refuses resource creation; `allow_with_warning` records `provider_usage_unavailable`
in the durable creation intent. Provider output/token accounting and broader
budget-policy resolution remain unfinished. Unknown profile fields and malformed
values fail shared validation with source values withheld.

The config digest covers exact config bytes; the profile digest covers the parsed
definition with defaults and stable field ordering. Neither is an immutable attempt
record or approval. Changing config alone cannot create launch authority.

The current implementation reports launch, readiness observation, prompt submission,
stop, checkpoint acknowledgment, structured usage and resume separately as `unknown`.
Agent and Herdr versions are `null`. `launchable`, `protocol_capable` and `certified`
are all false until the respective evidence paths exist. These are conservative
admission results, not claims that an installed agent lacks support.

`herdr-projects profile resolve NAME` prints a redacted budget envelope together
with inspection. It does not launch, write the store, or emit argv/environment
values. `soft_input_chars` is `soft_input_tokens * 4` when that field is set,
otherwise 32 000 (the worker brief cap). The estimator is `char-count-v1`.
`frozen` is always null: a sealed per-attempt `FrozenProfile` producer remains
T04.3 remaining-acceptance. Live `profile probe` still leaves `launchable`,
`protocol_capable` and `certified` false; distinguishing those flags is a mock
path only. W08 supplies live workflow certification.

`--agent KIND` selects the **unique** named profile whose `kind` field equals
KIND (`profiles.implementation` for `codex` and `profiles.planner` for `claude`
in the example above). Zero or multiple matches are errors. Resolution never
looks up `profiles.<kind>` and never copies another kind's `extra_args`. This
selector is documentation and the `profile resolve` CLI only; it is not wired
into ticker launch or coordinator checkpoints.

Named profiles are not yet selected by legacy thread/coordinator launches. Those
paths retain the explicit kind bindings described in [operations](operations.md).
Remaining T04.3 work is the sealed per-attempt producer (still undispatched) and
live W08 certification. No listed agent kind is automatically certified.

## Observed compatibility baseline

On 2026-09-19, the local `herdr --version` reported `0.9.1` and
`herdr agent start --help` advertised the following planned kinds. This is a CLI
surface check only. It does not test authentication, the running server version,
agent readiness, task delivery, stop, resume, usage or checkpoint acknowledgment.

| Kind | Advertised by Herdr 0.9.1 | Agent version | Workflow certified |
| --- | --- | --- | --- |
| claude | Yes | Not probed | No |
| codex | Yes | 0.154.0 (explicit local version probe) | No |
| devin | Yes | Not probed | No |
| muse | Yes | Not probed | No |
| grok | Yes | Not probed | No |

The inspection command does not import this manual observation as runtime evidence.
Version-bound adapter probes and W08 workflow tests must supply that evidence.

## Explicit local version probes

`profile probe NAME --herdr-executable /absolute/herdr --agent-executable /absolute/agent`
runs the supported installation-version probes. Choose the installed executable,
not a version-manager shim that might install or update tools. The command hashes
regular executable files (up to 512 MiB), runs fixed `--version` commands with a
five-second per-command timeout and 4 KiB output limits. A single 20-second budget
covers hashing, both commands and final validation; cancellation or expiry discards
the evidence and prevents follow-up execution. Commands run from `/` with a cleared
environment and only a fixed `/usr/bin:/bin` path and `C` locale. They do not inherit
HOME, credentials, loader hooks, session selectors or controller configuration.

Both executable selections are pinned before either command runs. Hashing checks
file identity, size, mode and modification/change timestamps before and after the
read. Final checks retain the original selected paths to catch symlink retargeting;
replacement with identical bytes also invalidates an in-flight observation. Config
changes invalidate the complete probe. Raw command output and errors are withheld;
the report contains parsed versions, output hashes, binary hashes and an aggregate
evidence digest. Prerelease version suffixes are preserved.

Version parsing currently supports Herdr, Claude Code and Codex CLI. Other kinds
report `no_verified_version_adapter` and their executable is not invoked. The
Claude flag is documented in [installation verification](https://code.claude.com/docs/en/setup);
the Codex CLI declares its version option in its [CLI parser](https://github.com/openai/codex/blob/main/codex-rs/cli/src/main.rs).
No vendor model, permission or resume flags are generated.

Probe commands use the explicit environment described above and do not apply profile
arguments or environment references. They execute external code: the explicit path
must identify a program you trust. File hashes cover the named executable, not
its interpreter, dependencies or remote servers. Reports are local installation
observations, not authenticated executable attestations or durable launch authority.
They do not contact Herdr sessions or start agent conversations. Inspection's
capabilities and launch admission remain unknown/blocked until verified adapter
and immutable attempt-resolution paths consume suitable evidence.

## Canonical attempt evidence (schema 12)

New sealed reservations now require version-2 launch inputs with an embedded frozen
profile. Its content-addressed reference covers kind, definition/config identity,
argument digest, environment names, versioned permission/adapter references, exact
agent/Herdr executable identities and seven separate capability evidence states.
Raw argv and environment values stay outside this record. A dispatcher must recover
arguments only from matching user-owned config; changed config must not substitute
new flags for an existing attempt.

Launch, readiness, prompt and stop evidence must be supported. Missing checkpoint,
usage or resume evidence remains explicitly unknown and does not fabricate support.
A workflow certificate is a separate optional reference. Evidence references must
come from the trusted adapter/policy producer; deserializing these records grants
no authority. That production producer and canonical dispatch remain unavailable.

The store checks the frozen profile digest, config identity and any recorded runtime
agent kind in the reservation transaction. Existing schema-11 version-1 records
remain readable with unchanged content IDs, while schema 12 refuses new reservations
in that old format. Upgrade is explicit and does not rewrite historical payloads.

## Planner profile for migrated context

After runtime migration, `context PROJECT` produces a budgeted coordinator
checkpoint and requires a named planner profile. Configure it before using
checkpoint context (or pass `--profile NAME` for another configured profile):

```toml
[profiles.planner]
kind = "claude"
permission_policy = "interactive"
```

This selects a context budget; it does not launch a model or certify launch
capabilities. Invalid/interrupted project ownership is checked before profile
resolution. A missing profile produces `context requires profiles.planner or
--profile NAME`; it does not consume inbox items or acknowledge a checkpoint.


## Frozen execution environment

The canonical gate sender requires an explicit `execution_home` retained in the
frozen profile. It is an owner-controlled canonical directory outside the project,
used to locate the agent's configuration and credential store. Changing that path
changes the profile identity and requires newly bound approval. No credential values
are copied into frozen inputs, prompts or the ledger.

The supported gate executes the agent through `env -i` with only HOME, a fixed
`/usr/bin:/bin` PATH, C.UTF-8 locale and xterm-256color TERM. Arbitrary inherited
variables do not reach the agent. Custom environment/model/effort mappings still
require their own verified preparation support. Old profiles lacking an explicit
home remain readable for recovery but are refused by the native gate sender.
The trusted CLI producer of this frozen environment remains part of dispatch
preparation work; `profile resolve` does not grant it.

## Project-bound preparation

With the `state-store` build, prepare immutable installation inputs using the
migrated project's pinned owner configuration:

```sh
herdr-projects profile prepare PROJECT PROFILE \
  --herdr-executable /absolute/path/to/herdr \
  --agent-executable /absolute/path/to/agent \
  --execution-home /absolute/owner-controlled/agent-home
```

This command executes bounded `--version` probes with a clean environment. Use the
actual executable, not an updater or tool-manager wrapper. It records canonical
paths, binary hashes, exact versions, the configuration/definition/argument hashes,
the signed-owner policy reference, and the explicit execution-home path. It never
prints argument contents or reads credentials from that home. The home must be
canonical, owned by the current user, not group/world writable, and outside the
project. Configuration, policy and installation changes during probing discard
the result. It neither reserves work nor changes project state.

The current preparation mapping requires Herdr 0.9.1, a recognized Claude or Codex
version format, `permission_policy = "interactive"`, and a bounded wall-time
budget. The interactive policy selects the project's owner-approval authority;
it does not certify vendor permission flags. Unverified model, effort, environment
and usage-blocking mappings refuse preparation. Arguments remain bound by digest
and are not passed to the version probe.

The returned frozen profile has **Unknown** transport capabilities and reports
`launchable`, `protocol_capable`, and `certified` as false. Installation evidence
is insufficient for dispatch: a trusted native capability/workflow producer must
still supply real evidence before launch preparation can reserve this profile.
JSON output is inspection evidence, not an importable launch authority.

## Native launch and termination verification

On Linux with the `state-store` build:

```sh
herdr-projects profile verify-native PROJECT PROFILE \
  --herdr-executable /absolute/patched/herdr \
  --agent-executable /absolute/native/agent \
  --execution-home /absolute/owner-controlled/agent-home
```

This command performs installation preparation, then starts a disposable Herdr
server with its own temporary home, configuration, socket and working directory.
It uses `workspace.create_command`, so stock Herdr 0.9.1 without the compatibility
patch is unsupported. The current verified mapping requires empty `extra_args`; positional prompts and
vendor subcommands must not run a task during this transport check. The exact
configured agent runs with the explicit execution home; the agent can read its credentials/configuration there.
No task prompt is submitted. Use an empty temporary home for an unauthenticated
transport check. The command does not reserve a task or mutate project records.

A successful check verifies the exact gated supervisor, executable/argument
identity, native agent kind and terminal incarnation, and termination of the
supervised process tree. It sets only launch and stop capabilities to Supported,
with a digest referencing the returned evidence and original prepared profile.
Readiness, prompt submission, checkpoint acknowledgment and workflow certification
remain unresolved. Consequently `launchable`, `protocol_capable` and `certified`
remain false. The typed result cannot be constructed by JSON deserialization.

The probe is capped at 120 seconds, honors a shorter configured worker wall budget,
and uses independent worker/server watchdogs plus bounded cleanup. Cancellation,
identity changes, native rejection, missing process evidence or unproven stop
produce an error rather than a capability grant. Exact policy, configuration and
binary identities are rechecked. Use an optimized build for large executables;
repeated identity checks remain subject to the ordinary deadlines.

The ignored `live_production_native_profile_verification` test passed with the
patched Herdr runtime and Codex 0.154.0 in an empty home, without credentials or
any task prompt. This is transport evidence, not authenticated workflow evidence.

## Readiness and prompt submission verification

`profile verify-interaction PROJECT PROFILE` accepts the same executable and
execution-home options as `verify-native`. It additionally waits for positive
bundled-detector readiness, rechecks the exact native agent and kernel process,
and submits one fixed diagnostic prompt asking for an opaque token without tools
or file changes. The prompt is never retried after a lost or rejected response.
An `agent_prompted` reply must identify the exact workspace, tab, pane, terminal,
working directory and agent kind. Generic success text is insufficient.

Only after those checks and proven termination can this command return
`launchable=true`. It still reports `protocol_capable=false` and `certified=false`:
a native prompt acknowledgment does not prove a model response, checkpoint,
memory write, artifact preservation or worker workflow completion. The evidence
contains a prompt digest, native session/terminal identity and detector version;
it does not include credentials or model output. Authentication/onboarding or
trust dialogs cannot be treated as readiness.

The authenticated positive live test is implemented but has not run: automatic
approval review requires explicit user authorization before using the existing
Codex auth cache to submit the diagnostic prompt. The test would copy that cache
into a private temporary home, use isolated read-only settings, and remove the
home afterward. It does not modify the user's normal configuration or auth file.
See the official [authentication cache documentation](https://learn.chatgpt.com/docs/auth)
and [trusted project configuration](https://learn.chatgpt.com/docs/config-file/config-reference).

Native agent observation accepts background or orphaned children only when there
is still exactly one matching agent. Parent, PID namespace, pidfd lifetime,
executable inode and literal arguments are checked; duplicate matching agents
remain ambiguous. This avoids treating every additional namespace-init child as
loss of the worker while retaining the exact-process fence.

## Retaining native verification evidence

Pass `--retain` to `profile verify-native` or `profile verify-interaction` to store
the verifier's result in the project's canonical database. Schema 25 is required;
the command checks this before starting a probe, and never upgrades implicitly.
Without the option, verification continues to print its report without retention.

Retention accepts only the sealed in-process verification result, checks the
original database path and file identity plus current owner policy, and commits
the immutable report and audit event together. Repeating retention of the same
result is idempotent. A different project or replaced database is refused.
Reports include complete credential-free profiles and their native evidence;
arguments remain digests and authentication material is never stored.

`profile retained PROJECT DIGEST` retrieves a report using the final profile's
`preparation.reference.digest`. Retrieval verifies the report hash, profile
reference and original database identity. Copied reports remain bound to their
original store. This historical evidence does not establish that the current binaries
or configuration still match, and the JSON cannot be imported as launch authority.
Current-input revalidation and trusted preparation/reservation ingress are
available below. Controller scheduling of new launches remains disabled.


## Revalidating retained inputs

`profile revalidate PROJECT DIGEST` loads a retained native report from the
canonical store under a bounded database read, then re-observes the exact current
executable hashes and versions, named owner configuration, policy and execution
home. It refuses changed executable bytes before running their version commands.
It starts no native agent session and sends no prompt.

The service reconstructs the unknown-capability baseline, checks its evidence
reference, and derives the same supported capabilities as the native verifier.
Rehashed reports with invented workflow, resume, protocol or certification claims
are rejected. Launch/stop-only evidence remains unlaunchable. Interaction evidence
can retain launchability, but cannot establish workflow certification.

The library returns `RevalidatedProfile`, which cannot be deserialized. It pins
the source database and execution-home inodes and holds the root maintenance
barrier until dropped. `launch_profile()` rechecks the current inputs, cancellation
and original deadline before returning frozen launch inputs. The proof is bounded
to twenty seconds, including database reads and version probes; retrieving or
checking it never renews that deadline. Printed JSON is a report, not a reusable
proof. Owner approval, task/repository/memory binding and reservation remain
separate requirements.

## Preparing and reserving a launch

On Linux, `launch PROJECT draft --selection selection.json --expected-head N`
revalidates a retained launchable profile and prints the exact inputs, complete
worker brief and unsigned `approval` document. Optional `--validity-seconds`
defaults to 900 (maximum 86400). Drafting writes no canonical state.

Repository-backed drafts also include `worktrees`: deterministic per-attempt
checkout paths under the project's `.state/worktrees` and fresh `hp-…` branches.
The plans derive from the exact approved inputs and eventual attempt identity.
The worktree provisioning service creates and observes these resources, and
the native adapter carries their paths and ownership through worker start. See
[worktree preparation](canonical-worktrees.md).

The selection contains `task`, `binding`, `profile`, `knowledge`, and optional
`repositories` (canonical absolute paths). Both references contain `id`,
`revision`, and `digest`; `profile` is the retained native preparation reference,
and `knowledge` uses the worker snapshot ID, revision 1, and manifest hash.
Create that snapshot with `memory PROJECT snapshot --worker --task TASK
--profile NAME --input-file scope.json`. Ordinary task snapshots do not reserve
space for the full worker protocol and cannot substitute for worker snapshots.

Review the full draft, extract its `approval` document, and use the existing
owner-signature and `approval PROJECT import` flow. Then run
`launch PROJECT reserve --selection selection.json --approval-digest DIGEST
--expected-head N`, using the head after approval import. Reservation reconstructs
all inputs and requires the exact installed signature. It atomically creates one
reserved attempt and launch intent. The approval is consumed only at the later
external-effect claim. Neither command creates native resources or sends input.

The service holds the root maintenance barrier and original twenty-second
deadline across revalidation, repository observation, memory rendering and SQL
admission. Repository observation uses local commit/tree objects, disables Git
replacement refs and lazy fetching, and rejects partial clones. Required memory
and evidence objects must be present; instructions come from the retained snapshot.
The preview and eventual attempt brief use the same content-derived attempt ID.
Changed tasks, policy, binaries, repositories, snapshots, budgets or approval
prevent reservation. Printed launch inputs cannot be deserialized as authority.

New-launch controller dispatch, complete worktree preservation and live workflow
acceptance remain outstanding in [the dispatch audit](dispatch-enablement.md).


Worker brief version 2 also previews an attempt-specific `output_directory` under
`.state/worker-output`. Report and library instructions, including the exact path,
count toward the complete prompt budget. Create a new `--worker` memory snapshot
and approved attempt for the `char-count-worker-brief-v2` contract; old snapshots
are not silently given new output instructions.
