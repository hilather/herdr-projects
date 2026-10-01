# Grok Build 1.0.46 live certification (`otlp:grok`)

**Certified live on 2026-10-01** by the steward's owner-approved run of
`tests/telemetry_live.rs::grok_live` on branch `telemetry/lc4-grok-live`.
Two headless turns (`-p … --output-format json --tools "" --no-subagents
--disable-web-search`, then `-c -p …`) exported OTLP/HTTP protobuf directly
to the product receiver (DG4h) with a per-attempt token.

Authentication (owner decision): the owner's Grok subscription login was used
**in place** (`GROK_HOME=~/.grok` for the Grok process only). Nothing was
copied, and no API key was used. The two test sessions appear in the owner's
Grok history.

Usage authority (LC4): `grok_code.api_request` log events, one per API call,
deduplicated by session and event sequence. `input_tokens` includes cache
reads. The `grok_code.token.usage` metric is kept for reconciliation only and
never counted.

| | Input (incl. cache) | Cache read | Cache write | Output | Reasoning | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Request A (OTLP) | 15,561 | 640 | 0 | 63 | 62 | 15,624 |
| Request B (OTLP) | 15,669 | 5,120 | 0 | 21 | 20 | 15,690 |
| OTLP usage total | 31,230 | 5,760 | 0 | 84 | 82 | 31,314 |
| Grok's own stdout usage, normalized | 31,230 | 5,760 | 0 | 84 | 82 | 31,314 |
| Difference | 0 | 0 | 0 | 0 | 0 | 0 |

Model `grok-4.5-build`; binding `bound`. The privacy marker scan over
telemetry.db, including WAL/SHM, found 0 hits. Grok's stdout `input_tokens`
excludes cache reads, so the harness adds them before comparing.

## Findings this certification fixed

1. **Version gate:** the resource has `service.version = "1.0.46 (<build hash>)"`
   and `client.version = "1.0.46"`. The exact-match gate treated every point as
   an uncertified version. It now uses `client.version`, else the leading semver.
2. **Usage source:** live usage arrives per request as `grok_code.api_request`
   log events with input, output, reasoning, cache read/creation and cost.
   The metric is a per-export delta sum and is reconciliation-only.
3. **PII:** every Grok record carries `user.email`, `user.id`, `team.id` and
   `client_identifier`. None is stored, not even hashed (planted-email E2E).
4. **One bad event no longer rejects a batch** (LC4-b): invalid usage counters
   demote only that record to an unmapped diagnostic.

## Scope and known gap

`certified_versions` = 1.0.46. Live fields: `grok_code.api_request` `model`,
`input_tokens`, `output_tokens`, `reasoning_tokens`, `cache_read_tokens` and
`cache_creation_tokens`. Everything else stays fixture-certified.

**Ledger (DG4j): addressed and live-verified 2026-10-01.** A second owner-approved
two-turn run on branch `telemetry/dg4j-otlp-ledger` reconciled the ledger itself:
31,225 input (incl. cache) / 5,248 cache read / 75 output / 73 reasoning, equal
to Grok's own totals on every counter; bound; privacy 0 hits.
Certified, bound API-request OTLP usage now feeds the accounting ledger,
M08/M09, attempt usage and ledger-based cost. The updated live harness derives
`ledger_totals` from counted ledger entries and compares them against Grok's
own totals; reconciliation-only metrics remain excluded.
