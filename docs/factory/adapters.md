# Factory adapters

Claude Code (`kind=claude`) is the second adapter candidate. Naming it is not
certification. Codex probe behavior is unchanged. This note does not relax
`profile_config`, does not mark a profile certified, and does not enable a
live launch. `PREPARED_LAUNCH_DISPATCH_ENABLED` is unchanged.
`factory_admission` stays `off` unless an operator installs a signed policy.

## Absent Claude binary

The conformance check writes one `claude-capability-manifest.json`. `status`
`unsupported` and `binary_present` false are written only when no Claude
executable is on `PATH`. CI without Claude records that manifest. The harness
opens a project store and leaves `capability_evidence` empty. A missing
executable is still an error from `probe`, not a certificate.

- `launchable`, `protocol_capable`, `certified`, `workflow_certified`, and `live_launch` false
- `model_mapping`, `effort_mapping`, and `environment_mapping` `refused`
- `capability_levels` empty

## Present Claude binary

When the executable is present, that same file is the live result. It is not
left next to an absent placeholder. `unsupported` is recorded only when `probe`
returns `profile probe cancelled or deadline exhausted; evidence discarded`,
and `binary_present` stays true. A version observation is kept and stays
uncertified. A non-success `--version` is `probe_failed`, not an absent skip.
Executable changes and config changes fail the harness.

The installation probe is `--version` only, with a cleared environment, no
profile arguments, and no agent session. A parsed version is not a model,
effort, or environment mapping.
`ProfileDefinition::validate_gated_preparation` still refuses those fields.
The probe leaves `launchable`, `protocol_capable`, and `certified` false.

## Codex

A Codex prerelease from `codex-cli` is still a version observation. A missing
Codex executable is still an error, not an unsupported certificate. Model,
effort, and environment mappings stay refused for Codex as well.
