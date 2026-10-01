# Muse Code 1.4.0-R4161.1 live certification (native `muse`)

**Certified live on 2026-10-01** by the steward's owner-approved run of
`tests/telemetry_live.rs::muse_live` on branch `telemetry/dg4i-muse-native`.
Two `muse exec --json --provider meta --reasoning-effort low` turns shared one
`--session-id`. The installed release binary
(`muse-bin-1.4.0-R4161.1`) was run directly, not the self-updating launcher.
Muse also spawned two background subagents, whose sessions sit under the
parent's `subagent/` directory.

Authentication (owner decision): the owner's Muse login was used **in place**.
`XDG_CONFIG_HOME=~/.config` was set for the Muse process only; the release
binary ignores the launcher's `MUSE_AUTH_PATH`. Nothing was copied. Sessions and
data went to the throwaway home, which was deleted after the run.

Version identity: `muse --version` prints `Muse Code 1.4.0 (1.4.0-R4161.1)`. The
certified identity is the parenthesized build id, as in
`.muse-release-info.json`. The product's `observed_version` has no Muse branch
yet, because Muse is not a launchable worker kind (a separate follow-up).

| | Input (incl. cache) | Cache read | Output | Reasoning | Total |
| --- | ---: | ---: | ---: | ---: | ---: |
| Record 1 | 20,070 | 0 | 87 | 76 | 20,157 |
| Record 2 | 20,193 | 14,193 | 19 | 8 | 20,212 |
| Record 3 | 3,179 | 2,801 | 608 | 471 | 3,787 |
| Record 4 | 3,263 | 2,929 | 689 | 564 | 3,952 |
| Ledger total | 46,705 | 19,923 | 1,403 | 1,119 | 48,108 |
| Muse's own `model_completed` usage | 46,705 | 19,923 | 1,403 | 1,119 | 48,108 |
| Difference | 0 | 0 | 0 | 0 | 0 |

Binding was `bound`. The privacy marker scan over telemetry.db,
including WAL/SHM, found 0 hits.

## How this adapter came to be

DG4g reviewed only the installed binary's strings, which didn't establish a
local usage schema, so it declared native `muse` as `none`. The steward's first
owner-approved live run showed `session.jsonl` `model_completed` events carrying
`usage.{input,output,cached,cache_read,cache_write,reasoning}_tokens` and the
model, for the parent session and for subagents. DG4i built the native adapter
from the steward's sanitized skeletons of those files (structure and numbers
only). It counts `model_completed` once; `goal_usage_attribution` repeats the
same quantities and is not counted.

## Scope

`certified_versions` = 1.4.0-R4161.1. All declared `model_completed` fields are
live: id, timestamps, workspace root (binding), model, the five usage counters
and the parent link from the subagent path. `otlp:muse` remains fixture-only
(the public build's OTLP export was not observed).
