# Factory adapters

Claude Code (`kind=claude`) is the second adapter candidate. Naming it is not
certification. Codex probe behavior is unchanged. This note does not relax
`profile_config`, does not mark a profile certified, and does not enable a
live launch. `PREPARED_LAUNCH_DISPATCH_ENABLED` is unchanged.
`factory_admission` stays `off` unless an operator installs a signed policy.

## Absent Claude binary

The conformance check in `src/agents/probe.rs` skips when the Claude executable
is not on `PATH`. It writes `claude-capability-manifest.json` with:

- `kind` `claude` and `status` `unsupported`
- `launchable`, `protocol_capable`, `certified`, `workflow_certified`, and `live_launch` false
- `model_mapping`, `effort_mapping`, and `environment_mapping` `refused`
- `capability_levels` empty

CI without Claude records `unsupported`. The skip does not write a
`workflow-certified` or `launchable` row. Schema 33 `capability_evidence`
accepts only `native` and `fake` adapters, and a `workflow-certified` level
must be live. This harness inserts neither. A missing binary is not a
substitute vendor. `probe` itself still errors on a missing executable; that
error is not turned into a certificate.

## Present Claude binary

A machine with Claude runs the same check. The installation probe is `--version`
only, with a cleared environment, no profile arguments, and no agent session.
A parsed version is not a model, effort, or environment mapping.
`ProfileDefinition::validate_gated_preparation` still refuses those fields.
The probe leaves `launchable`, `protocol_capable`, and `certified` false.
If the 20-second probe budget discards the evidence, the manifest stays
`unsupported` and uncertified.

## Codex

A Codex prerelease from `codex-cli` is still a version observation. A missing
Codex executable is still an error, not an unsupported certificate. Model,
effort, and environment mappings stay refused for Codex as well.
