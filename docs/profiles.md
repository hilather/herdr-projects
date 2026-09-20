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
limits; enforcement awaits budget-policy resolution. Unknown profile fields and
malformed values fail validation with source values withheld.

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
five-second deadline and 4 KiB output limits, and rejects changes to executable
identity or config during observation. Raw command output and errors are withheld;
the report contains parsed versions, output hashes, binary hashes and an aggregate
evidence digest. Prerelease version suffixes are preserved.

Version parsing currently supports Herdr, Claude Code and Codex CLI. Other kinds
report `no_verified_version_adapter` and their executable is not invoked. The
Claude flag is documented in [installation verification](https://code.claude.com/docs/en/setup);
the Codex CLI declares its version option in its [CLI parser](https://github.com/openai/codex/blob/main/codex-rs/cli/src/main.rs).
No vendor model, permission or resume flags are generated.

Probe commands inherit the caller's existing environment but do not apply profile
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
