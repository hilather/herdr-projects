# Accounting contracts (lane B)

Owned by lane B ([phase2-lanes.md](phase2-lanes.md)); common rules are
[contracts.md](contracts.md) §0. Plan: doc 05 §2–§3 and §7.

## 1. Usage ledger (B1, TM2.1)

Sidecar stream `accounting` version 1
(`migrations/telemetry/accounting/0001_usage_ledger.sql`). The ledger is
derived: `telemetry <slug> accounting sync` (and the ticker pass after its
Codex collect) rebuilds it whole, in one sidecar transaction, from the Codex
tables (§5) read by SQL only. It never writes `state.db` or Codex tables.
`accounting entries` prints it read-only (`ledger_not_synced` before the first
sync; `collection_not_run` without a sidecar).

`usage_entries`, one per effective invocation or cumulative observation:

| Basis | Scope | Precedence | Identity (`entry_id`) | Position |
| --- | --- | --- | --- | --- |
| `delta` (a `codex_usage` row) | `request` | 1 (counted) | `codex:<session>:<ordinal>` | ordinal |
| `cumulative` (`rollout_sources.thread_usage`) | `thread` | 2 (reconciliation only) | `codex:<session>:thread:<path_digest>` | the source's `records` |

Each keeps `native` (the six Codex counters as stored, `null` when absent) and
normalized fields under `normalization_version = codex-v1`: all six native
counters present, non-negative, ≤ 2^53, `cached ⊆ input`, `reasoning ⊆
output`, `total = input + output`; then `input_tokens` (inclusive),
`cache_read_tokens`, `new_input_tokens = input − cached`,
`cache_write_tokens` (as reported; overlap with input not certified),
`output_tokens` (inclusive), `reasoning_tokens` (subset, never added),
`total_tokens`. Otherwise every normalized field is `NULL`
(`normalized: unavailable not_normalized`), never 0.

`usage_dispositions`, one per entry and rollout that observed it
(`path_digest`):

- Delta: the first-storing rollout is `accepted` (Codex row accepted),
  `unresolved` (Codex `reason`, or `normalization_refused`), or `conflict`
  (`payload_digest_mismatch`, a `codex_quarantine` row for the key; then every
  rollout is `conflict`). Every other rollout of the session whose `records`
  reach the ordinal is `duplicate`: one invocation, several provenance rows.
- Cumulative, per session in `(position, path_digest)` order against the
  high-water total: first or higher `accepted`; equal `duplicate`; lower
  `unresolved` / `regression_without_reset` (Codex reports no reset evidence);
  counters failing `codex-v1` `unresolved` / `invariant_violation`.

`usage_ledger(singleton, normalization_version, synced_unix_ms)` records the
last sync.

Golden (`tests/telemetry_accounting.rs`): input 1000 incl. cached 200, output
300 incl. reasoning 80 → total 1300, new input 800; the same record in two
rollouts is one entry with `accepted` + `duplicate`; a thread total falling
1300 → 1000 is `unresolved`.

## 2. Metrics

The lane provides M08 and M09 (contracts §6, `definition` still
`Mnn.slice-v1`, same shape and numbers as the central slice): Σ normalized
`input_tokens` / `output_tokens` (`reasoning_output_tokens` as a subset) of
counted entries (delta, an `accepted` disposition) of certified sessions, with
the §6 `coverage`. They are derived from the Codex tables at report time, so
they need no prior sync. It also provides M38 and M39, both `unavailable`
(§5).

## 3. Session graph and model segments (B2, TM2.2)

Stream `accounting` version 2 (`0002_session_graph.sql`), rebuilt by the same
sync and transaction as the ledger (sync adds `sessions` and `model_segments`
counts); migrating to 2 clears `usage_ledger`, so a ledger synced before reads
`ledger_not_synced` until the next sync. `accounting sessions` prints it
read-only.

`session_graph`, one node per rollout source. Links come only from native
evidence:

- Resume: rollouts of one `session_meta.id` form one session. The rollout with
  the most `records` (then lowest `path_digest`) is the root; every other is
  `included` under it (`evidence = same_session_prefix`) when the root observed
  each delta entry it did (same key and payload, §1). Its `inclusive_total` is
  covered by the root and never added: a root of 200 over an epoch of 50
  stays 200. Any `conflict` in the session (quarantine), or an epoch the root
  does not cover, makes every such node `unresolved`: the session total and
  its parent are `unavailable: inclusion_unknown`.
- Role: `guardian` when a record carries model `codex-auto-review`,
  `subagent` when `source = subagent`, else `primary`. The Codex tables keep no
  parent id (`source` stores only its first key), so a guardian or subagent
  root is `unlinked_child` with parent `unavailable:
  no_native_parent_evidence`: reported apart and never added to any other
  session. Linking needs a native parent id captured by lane A (A4).
- `inclusive_total` = Σ normalized `total_tokens` of the delta entries the
  rollout observed (accepted or duplicate); `NULL` when it observed one that
  is not counted and normalized (session total `unavailable: incomplete`).

`model_segments` over a session's counted delta entries by position: the
model is the latest preceding `turn_context` (`codex_usage.model`); a model
switch opens the next `model` segment (1, 2, ...). Records of a turn whose
records or completion (`codex_turns`) carry more than one model go to the
`mixed` bucket; records without model evidence to `unallocated` (segment 0).
Buckets do not break a segment. Each row keeps entries, first/last position
and Σ input, output, reasoning (subset) and total, so segments + mixed +
unallocated = the session total. The requested model is not recorded; the
model is as reported by `turn_context`.

The rollup sums root totals of `primary` sessions (`sessions`) and of
unlinked children (`unlinked_children`) separately; with any incomplete or
unresolved session both are `unavailable: incomplete_sessions`, never a
partial sum.

Test `model_switch_splits_segments_not_task`: gpt-5.5 50 then gpt-5.5-mini
150 → two segments, total 200; the resumed epoch of 50 stays inside the root
of 200; an unlinked guardian of 50 (unallocated 10, `codex-auto-review` 30,
mixed 10) is reported apart; a rewritten record makes the session unresolved.

## 4. Rate cards and published-rate estimates (B3, TM2.3)

Stream `accounting` version 3 (`0003_rate_cards.sql`). Plan: doc 05 §5,
doc 07 M12, doc 09 corrections, doc 10 cost tests. The owner decided that no
real prices ship: the only cards in the repo are invented synthetic test
fixtures (`tests/fixtures/telemetry/accounting/rates-*`), marked as such.

**Rate cards** (`rate_cards`, `rate_card_models`, `rate_card_rates`) are
imported by `accounting import-rate-card <file>` from a local TOML (`.toml`)
or JSON file: `card_id`, `version` (> 0), `provider`, `product` (the usage
source it applies to, `codex`), `models` (exact reported model names),
`currency` (ISO 4217), `rate_unit` (tokens per quoted rate, a power of ten
1–10^9), half-open UTC-ms `[effective_from_unix_ms, effective_to_unix_ms)`
(`to` absent = open), `includes {discounts, taxes, fees}`, `source`, and
`rates [{category, cache_tier?, rate}]`. Categories are disjoint: `input`
(new, uncached input), `cache_read`, `cache_write`, `output` (includes
reasoning). A rate is a decimal *string* (≤ 18 digits, ≤ 12 places); a JSON or
TOML number is refused, so no float touches money. The card is canonicalized
(sorted, decimals trimmed: `"4.00"` → `"4"`) and digested. Cards are
append-only (SQL triggers refuse UPDATE/DELETE): the same version with the
same digest imports as a no-op, with a different digest it is refused; a
changed price is a new version. `accounting rate-cards` lists them. The card
`provider` is recorded as asserted by the card: Codex rows carry no model
provider, so applicability is `product` + `model`.

**Valuations** (`valuation_revisions`, `valuations`): `accounting reprice`
values every delta entry of the synced ledger (§1; `ledger_not_synced`
before) and appends revision *n+1* only when the result's digest differs
from revision *n* (so repeated reprices and re-syncs append nothing). Each
row copies the quantities it priced, so later syncs never change an earlier
revision, and repricing never changes measured tokens. Basis is always
`published_rate_estimate`; there are no provider charges, and an estimate is
never added to one.

Usage time: Codex rows carry no per-record timestamp, so an entry's usage
interval is bounded by its storing rollout's session start
(`rollout_sources.session_unix_ms`) and its first observation
(`codex_usage.observed_unix_ms`), `[min, max]` of the two (policy
`usage_interval=session_start..first_observed;split=none`). The card
versions of the entry's product and model overlapping that interval are the
candidates; the highest version must cover the whole interval. Unpriced
reasons (the entry is `unavailable`, never 0):

| Reason | When |
| --- | --- |
| `usage_not_counted` | not counted (no `accepted` disposition) or not normalized |
| `model_unknown` | no reported model |
| `usage_time_unknown` | no session start |
| `cache_write_convention_unknown` | cache writes > 0 (codex-v1 does not certify their overlap with input) |
| `no_rate_card` | no card for the product and model overlaps the interval |
| `ambiguous_rate_cards` | cards of more than one `card_id` overlap it |
| `rate_change_within_usage_interval` | the winning version does not cover the whole interval (no evidence to split) |
| `<category>_rate_missing` | a category with tokens has no rate (e.g. cached input and no `cache_read` rate) |
| `cache_tier_unknown` | several cache tiers for a category; Codex reports none |
| `amount_overflow` | the exact sum exceeds fixed-point range (never wrapped) |

Amount = Σ over categories with tokens of `tokens × rate / rate_unit`, exact
(fixed-point `i128`, trimmed decimal string), with per-category components.

**`accounting cost [--json] [--revision N]`** (read-only; `not_priced`
before the first reprice) shows the latest revision or revision N, byte-
identical to when it was appended. Per session and per attempt (bound
attempt of the storing rollout; guardian/subagent sessions summed apart as
`unlinked_children`, §3): `estimate` is `complete {currency, amount}` when
every entry is priced in one currency; `partial {currency, priced_amount}`
(labeled, never the total) when some are not; `unavailable mixed_currency
{priced_by_currency}` when priced in several currencies (never added, no
conversion); `unavailable no_priced_entries` when none is. `coverage` counts
entries, priced, and unpriced by reason; sessions list `rate_cards` used.
JSON amounts are exact; the text view is the only rounding (half-up, 6
places).

Test `repricing_uses_rate_effective_at_usage_time`: version 1 (before the
boundary; input 2, output 4 per 10^6, no cache-read rate) prices doc 10's
1,000 input + 500 output at exactly `0.004`; version 2 (from the boundary;
input 2, cache read 0.50, output 8) prices doc 05's 800 new + 200 cached +
300 output at `0.0041`; gpt-5.5-mini (no card), a cache write, a cached read
under version 1 and a session straddling the boundary are unavailable with
their reasons, leaving the attempt `partial` at `0.0081`. A corrected
version 3 (output 6 → `0.0035`) and a EUR card (`0.00021`) append revision 2
(attempt `mixed_currency`: USD `0.0075`, EUR `0.00021`); revision 1 reads
back byte-identical and the ledger is unchanged.

## 5. Quota windows (B4, TM2.7)

Stream `accounting` version 4 (`0004_quota_windows.sql`), rebuilt whole by
the same sync and transaction as §1–§3 from `codex_rate_limits` (SQL only;
sync adds a `quota_windows` count). Migrating to 4 clears `usage_ledger`
(`ledger_not_synced` until the next sync). Plan: doc 05 §5b, doc 07
M38–M40. The live run observed the fields but certified no semantics
([codex-live-0.154.0.md](codex-live-0.154.0.md)); every output says
`semantics: not_certified`.

**Observations** (`quota_observations`, one per `codex_rate_limits` row):
service `codex`; account = the rollout's `home_digest` (one execution home is
one login; a session seen under two homes is `account_ambiguous`); `limit_id`;
`window_kind` `primary` (the collector allowlist, contracts §5, keeps only
`primary`); unit `percent`; `window_minutes`; `resets_unix_ms` (`resets_at`
seconds × 1000); `used` and `remaining = 100 − used` as exact trimmed decimal
strings (`42.0` → `42`); `plan_type`; `observed_unix_ms` (the line's
`timestamp`); `trust`; `window_id`. Per (account, limit), in
`(observed, session, ordinal)` order against the current window:

| Trust | When | Window |
| --- | --- | --- |
| `incomplete` | `limit_id`, `used_percent`, `window_minutes` or `resets_at` missing (e.g. `primary: null`) | none |
| `unparseable` | `used` not a plain decimal in 0–100, or `window_minutes` ≤ 0 | none |
| `account_ambiguous` | the session's rollouts lie under several homes | none |
| `trusted` | first snapshot (window `first_observation`); a later `resets_at` (new window: `reset_elapsed` if observed at or after the previous reset, else `reset_moved`); or same window and `used` ≥ its high-water mark | the window |
| `window_regressed` | `resets_at` earlier than the current window's | none |
| `window_conflict` | same `resets_at`, different `window_minutes` | none |
| `used_decreased_without_reset` | same window, `used` below its high-water mark | the window (counted in `flagged`) |

A reset starts a new window identity; nothing is ever subtracted across one
or within one. **Windows** (`quota_windows`): `window_id =
codex:<account>:<limit_id>:primary:<resets_unix_ms>`, window start =
reset − `window_minutes`, first/last trusted observation, `first_used`,
`used` (high-water), `remaining`, `observed_increase = used − first_used`
(account-wide: never attributed to a task, as the window's invocation scope
is not certified), latest `plan_type`, trusted `observations` and `flagged`.

**`accounting quota [--json]`** (read-only; `collection_not_run` without a
sidecar, `ledger_not_synced` before a sync): `windows`, `observations`
(count per trust), and `metrics`:

- M38 `throttled_time_share`: `unavailable throttling_not_certified`. Codex
  rollouts carry no throttled intervals; the allowlist keeps no availability
  events (`rate_limit_reached_type` is a point flag, not collected).
- M39 `provider_error_rate`: `unavailable provider_errors_not_certified`. No
  typed provider error field is collected or certified; human-readable
  messages never become certified fields.
- M40 extended (`M40.quota-windows-v1`): per dispatch decision (attempt
  order), `{attempt_id, decided_unix_ms, service, account, windows}`; per
  limit, `primary` = the latest trusted observation of the attempt's account
  with `observed ≤ decided` (ties: session, ordinal descending) with
  `window_id, window_minutes, resets_unix_ms, observed_unix_ms, age_ms`, then
  `value` = remaining percent, `used`, and `freshness` (`stale` when
  `age_ms > stale_after_ms` = 900000, value still shown); if the window reset
  at or before the decision, `value` is `unavailable
  window_reset_since_observation` (the new window's value is unknown).
  `secondary` is `unavailable not_collected`. Decision-level reasons:
  `adapter_absent`, `execution_home_unknown`, `no_observation` (no snapshot
  of the account by then), `no_trusted_observation`. Native units; never
  summed or averaged across accounts, limits or services.

M38 and M39 also reach `telemetry <slug> report` through the lane
`metrics()` hook. The report's central M40 (contracts §6) is unchanged:
replacing it with the extended form would change the report shape asserted
by `tests/telemetry.rs` and `tests/cli.rs`, so that is a steward change.

Test `window_reset_starts_new_window_not_negative`: a 300-minute `primary`
window reads 40 → 55.5 → 50 (flagged, not subtracted) → 60 one minute before
dispatch (headroom `40`, age 60000, fresh, increase `20`); after its reset,
5 → 12.25 under a later `resets_at` open a second window (`reset_elapsed`,
increase `7.25`, remaining `87.75`), never −55. A snapshot 20 minutes old is
`stale` (`62.5`, age 1200000); one whose window reset before the decision is
`window_reset_since_observation`; no snapshot is `no_observation` and one
with `primary: null` is `incomplete` → `no_trusted_observation`, never 0.
