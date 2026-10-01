Synthetic fixture certificate, DG4a (2026-09-30). No live observations.

Reviewed native names/attributes:
- https://code.claude.com/docs/en/monitoring-usage
- https://geminicli.com/docs/cli/telemetry/

The envelope is OTLP/HTTP JSON. `@ATTEMPT@` is replaced by the isolated
project's real canonical attempt ID. Receipt timestamps are deterministic;
all `OTLP_SECRET_CONTENT` values are forbidden canaries. Claude metrics
exercise delta token counters and a cumulative cost snapshot; Gemini
metrics exercise cumulative token snapshots. They are independent evidence
from the request logs, never a combined accounting total. Codex has no
certified OTLP fixture; its rollout certificate is unchanged.

DG4e Grok fixture certificate (2026-10-01): `grok-metrics.json` is synthetic,
shaped from read-only strings in the installed `@xai-official/grok` 1.0.46
native binary. Resource service is `grok-cli`, version `1.0.46`; forbidden
canary is `GROK_SECRET_CONTENT`. See contracts-collection.md DG4e for the
executable/documentation conflict on cost and cache creation, external JSON
conversion and exact binding requirements, and uncertified native files.

DG4k Devin fixture certificate (2026-10-01): `devin-logs.json` and
`devin-metrics.json` carry the real shapes of a live Devin CLI `3000.11.3`
export, with the redacted live structure in
`devin-3000.11.3-live-structure.txt`. Resource service is `devin-local`, version
`3000.11.3` (no build suffix); events carry both `eventName` and an `event.name`
attribute; `devin.token.usage` is a DELTA Sum. All strings except identifiers
are synthetic; forbidden canary is `DEVIN_SECRET_*`. See contracts-collection.md DG4k.
