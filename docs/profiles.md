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

Named profiles are not yet selected by legacy thread/coordinator launches. Those
paths retain the explicit kind bindings described in [operations](operations.md).
Remaining T04.3 work includes version probes, capability evidence, kind-default
selection and immutable per-attempt profile resolution. W08 supplies live workflow
certification; no listed agent kind is automatically certified.

## Observed compatibility baseline

On 2026-09-19, the local `herdr --version` reported `0.9.1` and
`herdr agent start --help` advertised the following planned kinds. This is a CLI
surface check only. It does not test authentication, the running server version,
agent readiness, task delivery, stop, resume, usage or checkpoint acknowledgment.

| Kind | Advertised by Herdr 0.9.1 | Agent version | Workflow certified |
| --- | --- | --- | --- |
| claude | Yes | Not probed | No |
| codex | Yes | Not probed | No |
| devin | Yes | Not probed | No |
| muse | Yes | Not probed | No |
| grok | Yes | Not probed | No |

The inspection command does not import this manual observation as runtime evidence.
Version-bound adapter probes and W08 workflow tests must supply that evidence.
