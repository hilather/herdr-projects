# Devin CLI 3000.11.3 live certification (`otlp:devin`)

**Certified live on 2026-10-01** by the steward's owner-approved run of
`tests/telemetry_live.rs::devin_live` on branch `telemetry/dg4k-devin`.
There were two headless turns (`-p … --respect-workspace-trust false`, then
`-c -p …` in a new process continuing the same session). OTLP/HTTP protobuf
went directly to the product receiver with a per-attempt token.

Authentication (owner decision): a private 0600 copy of the owner's Devin login
(`credentials.toml`) in the disposable execution home, unread and deleted after
the run.

Export setup: Devin sends OTLP only when `~/.config/devin/config.json` has
`otel.enabled` and short export intervals (`log_export_interval_ms` /
`metric_export_interval_ms` 500), plus `logs_endpoint`. Without short
intervals, a short process exits before it exports anything.

Usage authority: `api_request` log events, one per API call, deduplicated by a
digest of the **raw** `request_id`. Normalization is
`otlp-devin-exclusive-v1`: `input_tokens` excludes cache reads, so total input
= input + cache read + cache creation. The DELTA `devin.token.usage` metric is
reconciliation-only, and it is the independent reference below.

| | Input (incl. cache) | Cache read | Cache write | Output | Total |
| --- | ---: | ---: | ---: | ---: | ---: |
| Request 1 (ledger) | 12,003 | 8,441 | 0 | 35 | 12,038 |
| Request 2 (ledger) | 11,927 | 9,138 | 0 | 34 | 11,961 |
| Ledger total | 23,930 | 17,579 | 0 | 69 | 23,999 |
| Devin's own `devin.token.usage` (DELTA sums) | 23,930 | 17,579 | 0 | 69 | 23,999 |
| Difference | 0 | 0 | 0 | 0 | 0 |

Model `swe-2-high`; binding `bound`. The privacy marker scan
found 0 hits. Reasoning is `not_reported`: Devin
reports no reasoning breakdown.

## Findings the live runs fixed

1. **Export never started** with the guessed config. It needs `otel.enabled`
   plus short export intervals (steward probe).
2. **Exclusive input:** `input_tokens` excludes cache reads (5,885 input vs 5,984
   cache read in one request), unlike Grok.
3. **Resumed sessions lost requests** (DG4k-c): `event.sequence` restarts at 0 in
   each process, so a `(session, sequence)` key collided.
4. **Every request id collided** (DG4k-d): the privacy masker redacts 36-character
   UUIDs, so keying on the stored, masked `request_id` merged all requests. The key
   is now a digest of the raw id, which is never stored.
5. **PII:** `user.id`, `prompt.id` and `message.uuid` appear on every record and
   are never stored.
6. **No local token usage:** `sessions.db` holds only credit/ACU cost, so native
   `devin` stays `none`.

## Scope

`certified_versions` = 3000.11.3. Live fields: `api_request` `model`,
`input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_creation_tokens`.
