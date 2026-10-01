# Codex 0.159.2 live certification

The owner's installed version is 0.159.2. **Certified live on 2026-10-01** by
the steward's owner-approved run of `tests/telemetry_live.rs::codex_live`:
two turns (`exec --json -s read-only`, then `exec resume --last --json`), model
`gpt-6.1-sol`, reasoning effort low, in a disposable execution home that
held a private 0600 copy of the owner's login (never read; the home was deleted
after the run).

| | Input | Cached | Cache write | Output | Reasoning | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Turn 1 delta (ledger) | 14,345 | 12,288 | 0 | 5 | 0 | 14,350 |
| Turn 2 delta (ledger) | 14,378 | 14,208 | 0 | 5 | 0 | 14,383 |
| Ledger total | 28,723 | 26,496 | 0 | 10 | 0 | 28,733 |
| Codex's own `thread_token_usage` | 28,723 | 26,496 | 0 | 10 | 0 | 28,733 |
| Difference | 0 | 0 | 0 | 0 | 0 | 0 |

Binding was `bound` and unmapped keys were empty. The privacy marker
scan over telemetry.db, including WAL/SHM, found 0 hits. The counts-only
report is retained by the steward. The sections below record the first, failed
report and why it failed: a harness summation bug, not a collector defect.

The initial live report failed: input 28,703, cached 24,576, output 10,
total 28,713 became ledger input 57,406. Binding was bound, unmapped keys
were empty and privacy had zero hits.

## Root cause and correction

`accounting entries` intentionally exposes two delta entries and a secondary
cumulative thread entry. `tests/telemetry_live.rs::live` filtered accepted
provenance but omitted `basis == "delta"` when summing native counters. It
also included the cumulative entry in `usage_records`. Both selections now
require delta basis. The cumulative entry remains available for reconciliation.
The collector, token_count path, fork-point lookup, resume tracking and token
counter semantics did not cause an extra delta. No version-specific parser
rule or schema change is needed; 0.154.0 fixture results remain unchanged.

## Deterministic conformance evidence

`tests/fixtures/telemetry/codex-0.159.2/live-two-turn-resume.jsonl` is the
steward's sanitized second live run: only cwd and session start time are
adapted to an isolated bound attempt during the CLI E2E test.

| Record | Input | Cached | Cache write | Output | Reasoning | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Line 13 delta | 14,329 | 12,288 | 0 | 5 | 0 | 14,334 |
| Line 24 delta | 14,362 | 14,208 | 0 | 5 | 0 | 14,367 |
| Sum / final thread total | 28,691 | 26,496 | 0 | 10 | 0 | 28,701 |

The third accounting entry is created by
`src/telemetry/accounting/ledger.rs::derive_scoped` in its cumulative-thread
loop (`rollout_sources.thread_usage`, secondary basis, precedence 2), not by
a fallback parser. It is `basis=cumulative`, copied from line 24's
`thread_token_usage`. The old harness's accepted-only sum is exactly 57,382
input. The E2E checks the exact deltas, all six reconciled counters, bound
attribution, no discrepancy, ignored snapshots and idempotent collection.

## Shape and evidence scope

`world_state` (line 7) is ignored at its top-level type tag. Its state contains
instructions and environment content and must never be stored.
`thread_settings_applied` (lines 16–17) is uncollected; its settings never enter
source observations. Capabilities explicitly mark both snapshot surfaces
unavailable, with content forbidden.

New `session_meta` fields `history_mode`, `runtime_workspace_roots`,
`context_window`, `creator_user_id` and `creator_account_id` remain uncollected.
`task_started.root_turn_id` remains uncollected. Existing unavailable fields
`token_usage_record.{thread_id,root_turn_id,turn_token_usage}` and
`token_count.info.{last_token_usage,model_context_window}` remain unavailable.
Existing usage and thread counters and token_count total counters retain the
sanitizer's six-counter allowlist. No arbitrary snapshot values are retained.

Capabilities now expose `live_versions` on Codex fields. 0.159.2 evidence
covers usage counters (including reconciliation totals), turn_context model,
and session binding inputs (id, timestamp, cwd, cli_version). Tool, subagent,
fork, duration, quota and other existing field claims retain their 0.154.0
scope. The certification registry cites this report.

## LC1 sandbox validation

All 21 `tests/telemetry*.rs` suites were run with `TMPDIR=$PWD/target/tmp`
and `cargo test --locked --offline -j 3 --features state-store --no-fail-fast`
with each telemetry test target selected. After rerunning the updated
conformance and workspace expectations: 218 passed, 7 failed, 17 ignored.
Conformance: 22/22; certification: 13/13. The four paid live tests and thirteen
large on-disk scale tests remained ignored. The owner-approved live rerun is
still the steward's responsibility.

Socket-only sandbox failures (`Operation not permitted`):

- `telemetry::attempts_show_attention_summary` (Unix listener)
- `telemetry_accounting::attention_intervals_union_and_censor` (Unix listener)
- `telemetry_health::recommendations_and_notices_change_no_canonical_state_and_no_dispatch` (Unix listener)
- `telemetry_workspace::thread_start_records_the_dispatch_reason_and_the_sidebar_suffix` (Unix listener)
- `telemetry_otlp::http_auth_limits_malformed_and_replay` (TCP loopback listener)
- `telemetry_otlp::http_request_rate_is_bounded` (TCP loopback listener)

Non-socket unresolved failure: `telemetry_scale::scale_gates_hold_under_load`
failed its racing-process assertion at line 676 because collect encountered
`database is locked`. A separate focused rerun also failed with collect and
analytics writer lock errors. No concurrency code was changed in LC1.

`cargo clippy --locked --offline -j 3 --features state-store --all-targets`
completed successfully. Existing warnings remain; none point to changed lines.
No schema changes, migrations, crates or production process spawns were added.
