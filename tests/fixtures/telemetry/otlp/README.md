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
