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
(§5), M31–M33 (§6), M16–M18 (§9, derived from A6 tool metadata at read
time) and M34–M37 (§10, derived from canonical rows at read time).

## 3. Session graph and model segments (B2, TM2.2)

Stream `accounting` version 2 (`0002_session_graph.sql`), rebuilt by the same
sync and transaction as the ledger (sync adds `sessions` and `model_segments`
counts); migrating to 2 clears `usage_ledger`, so a ledger synced before reads
`ledger_not_synced` until the next sync. `accounting sessions` prints it
read-only.

`session_graph`, one node per rollout source (table `session_nodes` from
stream version 6, §7, and `session_graph_nodes` from version 7, §8; the
earlier tables are no longer written).
Links come only from native evidence:

- Resume: rollouts of one `session_meta.id` form one session. The rollout with
  the most `records` (then lowest `path_digest`) is the root; every other is
  `included` under it (`evidence = same_session_prefix`) when the root observed
  each delta entry it did (same key and payload, §1). Its `inclusive_total` is
  covered by the root and never added: a root of 200 over an epoch of 50
  stays 200. Any `conflict` in the session (quarantine), or an epoch the root
  does not cover, makes every such node `unresolved`: the session total and
  its parent are `unavailable: inclusion_unknown`.
- Role, from the root rollout's A4 `rollout_metadata` and A5
  `rollout_threads` first: `guardian` when `subagent_kind = review`, when
  `thread_source = guardian_review` (A5, `live`; the live guardian's
  `subagent_kind` is `other`) or when a record carries model
  `codex-auto-review`; guardian evidence always wins over the generic
  subagent kind. `subagent` for any other `subagent_kind` (or, without A4
  metadata, `source = subagent`); `fork` when the session names a
  `forked_from_id` and is no subagent; else `primary`.
- Child link (A4 §7, A5 §8): a session whose root rollout names, other than
  itself, `subagent_parent_thread_id`, else `rollout_threads.parent_thread_id`,
  else `forked_from_id`, equal to a collected `rollout_sources.session_id` is
  a `linked_child` of that session, with `link_basis` `parent_thread_id` /
  `thread_parent_thread_id` / `forked_from_id`. `certified` is `live` for
  `thread_parent_thread_id` (the live guardian's `parent_thread_id` is its
  parent's `session_meta.id`) and `fixture` for the other two. A named parent
  that was not collected makes it `unlinked_child` with parent `unavailable:
  parent_not_collected` (and the named `session_id`); a guardian or subagent
  naming none is `unlinked_child` with `unavailable:
  no_native_parent_evidence`. Never by inference (cwd, attempt, time), and
  never by either `session_id` field (`session_meta.session_id`,
  `token_usage_record.session_id`): a guardian reports its parent's id there,
  and nodes and usage are keyed by the rollout's own `session_meta.id`
  (contracts.md §5).
- A linked child is **never** added to its parent's total. The parent lists
  it under `children {sessions: [{session_id, role, link_basis, certified,
  total_tokens, inclusion}], total_tokens}`: `inclusion` is `separate` for a
  spawned subagent and a guardian (the live parent's thread total 29760
  excluded its guardian's 7462, codex-live-0.154.0-a4.md §5), and `unavailable: fork_replay_not_certified` for a
  session naming a `forked_from_id` (a fork may replay its parent's records,
  `forked_from_ordinal_exclusive` is not collected, live probe step 3). The
  children `total_tokens` is their sum only when every child total is known
  (`unavailable: incomplete` otherwise) and none is a fork
  (`fork_replay_not_certified`).
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

The rollup sums root totals of `primary` sessions (`sessions`), of linked
children (`linked_children`, `unavailable: fork_replay_not_certified` when
one is a fork) and of unlinked children (`unlinked_children`) separately;
with any incomplete or unresolved session all three are `unavailable:
incomplete_sessions`, never a partial sum. The three are never added
together. M08/M09 (§2) count each record once per session regardless;
whether a child session's records replay its parent's is the open live-probe
question.

Test `model_switch_splits_segments_not_task`: gpt-5.5 50 then gpt-5.5-mini
150 → two segments, total 200; the resumed epoch of 50 stays inside the root
of 200; a live-shape guardian of 50 (unallocated 10, `codex-auto-review` 30,
mixed 10) naming an uncollected parent thread is reported apart
(`parent_not_collected`); a rewritten record makes the session unresolved.

Test `child_sessions_link_to_parent_without_double_count`: a parent of 100
and a spawned subagent of 30 naming it → the child is linked
(`parent_thread_id`, `fixture`), the parent stays 100 with `children` 30,
rollup 100 / 30 / 0. A fork of 40 (`forked_from_id`), a subagent of 20
naming an uncollected parent (`parent_not_collected`) and a live-shape
guardian of 50 naming the parent by thread lineage (linked,
`thread_parent_thread_id`, `live`, `separate`) join: `children` and
`linked_children` become `fork_replay_not_certified`, `unlinked_children`
20; M08/M09 stay 190/50 (each record once).

Test `guardian_links_to_parent_by_thread_lineage` (§8): two guardians of one
parent, 50 (`codex-auto-review` records) and 12 (only `thread_source =
guardian_review`, `subagent_kind = other`): without the parent both are
`parent_not_collected` naming it (rollup 0 / 0 / 62); with the parent (100)
both are linked (`thread_parent_thread_id`, `live`, `separate`), parent 100
with `children` 62, rollup 100 / 62 / 0; ledger entries stay under each
rollout's own id (parent 1, guardians 1 and 4); M08/M09 130/32. A pre-A5
sidecar (no `rollout_threads`) synced again reads as before A5 (both
`no_native_parent_evidence`, the 12 a `subagent`), and the next collect
restores the linked graph byte for byte.

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
`provider` is recorded as asserted by the card and checked against the
rollout's reported A4 `model_provider` (§7): applicability is `product` +
`model`, then, when the storing rollout reports a provider, only cards of
that provider (none left: `provider_mismatch`). Without a reported provider
the result is unchanged but a priced entry is marked `provider_check:
provider_unverified` (`matched` otherwise).

**Valuations** (`valuation_revisions`, `valuations`): `accounting reprice`
values every delta entry of the synced ledger (§1; `ledger_not_synced`
before) and appends revision *n+1* only when the result's digest differs
from revision *n* (so repeated reprices and re-syncs append nothing). Each
row copies the quantities it priced, so later syncs never change an earlier
revision, and repricing never changes measured tokens. Basis is always
`published_rate_estimate`; there are no provider charges, and an estimate is
never added to one.

Usage time: an entry's usage interval is its A4 record time
(`codex_usage_times.record_unix_ms`, the `token_usage_record` line
`timestamp`, certified `fixture`) as `[t, t]`, basis `record_time`. When that
is `NULL` or absent (no line time, or a row stored before A4), it falls back
to its storing rollout's session start (`rollout_sources.session_unix_ms`)
and its first observation (`codex_usage.observed_unix_ms`), `[min, max]` of
the two, basis `session_start..first_observed`. Each valuation records its
basis (`usage_interval.basis`). Policy
`usage_interval=record_time|session_start..first_observed;split=none;provider=checked_when_reported`
(revisions appended before A4 keep
`usage_interval=session_start..first_observed;split=none`). The card
versions of the entry's product and model overlapping that interval are the
candidates; the highest version must cover the whole interval. Unpriced
reasons (the entry is `unavailable`, never 0):

| Reason | When |
| --- | --- |
| `usage_not_counted` | not counted (no `accepted` disposition) or not normalized |
| `model_unknown` | no reported model |
| `usage_time_unknown` | no record time and no session start |
| `cache_write_convention_unknown` | cache writes > 0 (codex-v1 does not certify their overlap with input) |
| `no_rate_card` | no card for the product and model overlaps the interval |
| `provider_mismatch` | the rollout reports a model provider and no overlapping card is of that provider |
| `ambiguous_rate_cards` | cards of more than one `card_id` overlap it |
| `rate_change_within_usage_interval` | the winning version does not cover the whole interval (no evidence to split) |
| `<category>_rate_missing` | a category with tokens has no rate (e.g. cached input and no `cache_read` rate) |
| `cache_tier_unknown` | several cache tiers for a category; Codex reports none |
| `amount_overflow` | the exact sum exceeds fixed-point range (never wrapped) |

Amount = Σ over categories with tokens of `tokens × rate / rate_unit`, exact
(fixed-point `i128`, trimmed decimal string), with per-category components.

Reproducibility across the A4 change: stream version 6 records each new
valuation's `usage_basis` and `provider_check` in the append-only side table
`valuation_bases` (not new `valuations` columns, so the migration can re-run);
earlier revisions have no rows there and `cost --revision N` omits both, so
they read back byte-identical.
A reprice compares against the latest revision as it was computed: against a
pre-A4 revision, a result whose rows all use the fallback interval (or none),
report no provider and keep the same roles is the same result and appends
nothing; anything else (a record time, a provider check, a session now
`fork`/linked) appends a revision.

**`accounting cost [--json] [--revision N]`** (read-only; `not_priced`
before the first reprice) shows the latest revision or revision N, byte-
identical to when it was appended. Per session and per attempt (bound
attempt of the storing rollout; every non-`primary` session, guardian,
subagent or fork, linked or not, summed apart under the key
`unlinked_children`, §3; the key predates A4 linking and is kept for
compatibility): `estimate` is `complete {currency, amount}` when
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
their reasons, leaving the attempt `partial` at `0.0081` (the straddling
record has no line time, so it keeps the fallback interval). A corrected
version 3 (output 6 → `0.0035`) and a EUR card (`0.00021`) append revision 2
(attempt `mixed_currency`: USD `0.0075`, EUR `0.00021`); revision 1 reads
back byte-identical and the ledger is unchanged.

Test `record_times_narrow_rate_card_interval`: one session starting before a
boundary and first observed after it; its record timed before the boundary
prices at `0.004` (version 1), the one timed after at `0.0041` (version 2),
the one without a line time straddles (`rate_change_within_usage_interval`,
basis `session_start..first_observed`). Provider `openai` against the
`synthetic` cards is `provider_mismatch`; no provider prices at `0.00036`
marked `provider_unverified`; attempt `partial` `0.00846`. Version 3 appends
revision 2 (`0.00782`); revision 1 reads back byte-identical.

## 5. Quota windows (B4, TM2.7)

Stream `accounting` version 4 (`0004_quota_windows.sql`), rebuilt whole by
the same sync and transaction as §1–§3 from `codex_rate_limits` (SQL only;
sync adds a `quota_windows` count). Migrating to 4 clears `usage_ledger`
(`ledger_not_synced` until the next sync). Plan: doc 05 §5b, doc 07
M38–M40. The live run observed the fields but certified no semantics
([codex-live-0.154.0.md](codex-live-0.154.0.md)); every output says
`semantics: not_certified`. The A4 live run
([codex-live-0.154.0-a4.md](codex-live-0.154.0-a4.md) §2) observed
`resets_at` **fixed within a window**: 21 snapshots over 3 h 50 min and two
homes all reported the same `resets_at`, with no drift or jitter. The
`reset_moved` evidence below stays as the fallback for a later reset seen
before the earlier one elapsed; it was not observed live.

**Observations** (`quota_window_observations` from stream version 6, §7;
one per `codex_rate_limits` row and window kind):
service `codex`; account = the rollout's `home_digest`, and every output
says `account_basis: execution_home` (a session seen under two homes is
`account_ambiguous`); `limit_id`;
`window_kind` `primary`, or `secondary` from the snapshot's A4
`codex_rate_limit_windows` row (certified `fixture`; the live run saw
`null`); unit `percent`; `window_minutes`; `resets_unix_ms` (`resets_at`
seconds × 1000); `used` and `remaining = 100 − used` as exact trimmed decimal
strings (`42.0` → `42`); `plan_type`; `observed_unix_ms` (the line's
`timestamp`); `trust`; `window_id`. Per (account, limit), in
`(observed, session, ordinal)` order against the current window:

| Trust | When | Window |
| --- | --- | --- |
| `not_reported` | `secondary` only: the snapshot's secondary window was `null` | none |
| `incomplete` | `limit_id`, `used_percent`, `window_minutes` or `resets_at` missing (e.g. `primary: null`) | none |
| `unparseable` | `used` not a plain decimal in 0–100, or `window_minutes` ≤ 0 | none |
| `account_ambiguous` | the session's rollouts lie under several homes | none |
| `trusted` | first snapshot (window `first_observation`); a later `resets_at` (new window: `reset_elapsed` if observed at or after the previous reset, else `reset_moved`); or same window and `used` ≥ its high-water mark | the window |
| `window_regressed` | `resets_at` earlier than the current window's | none |
| `window_conflict` | same `resets_at`, different `window_minutes` | none |
| `used_decreased_without_reset` | same window, `used` below its high-water mark | the window (counted in `flagged`) |

Trust is tracked per (account, limit, window kind): a secondary window has
its own `window_minutes`/`resets_at` and the same rules. A snapshot without
an A4 row (stored before A4) has no secondary observation. Each observation
keeps the snapshot's `rate_limit_reached_type` as evidence only.

A reset starts a new window identity; nothing is ever subtracted across one
or within one. **Windows** (`quota_windows`): `window_id =
codex:<account>:<limit_id>:<window_kind>:<resets_unix_ms>`, window start =
reset − `window_minutes`, first/last trusted observation, `first_used`,
`used` (high-water), `remaining`, `observed_increase = used − first_used`
(account-wide: never attributed to a task, as the window's invocation scope
is not certified), latest `plan_type`, trusted `observations` and `flagged`.

**Account identity.** The account is the execution home, not the provider
login: credentials are never read. In the A4 live run, two homes holding
copies of one login reported identical windows at overlapping times, so the
provider window belongs to the login. Within one home, "one home = one
login" held. Herdr does not merge such accounts. It names them: windows of
different accounts with the same `limit_id`, `window_kind`,
`window_minutes` and `resets_unix_ms` are a **shared-window candidate**
(evidence `same_limit_kind_minutes_resets`, `merged: false`). The accounts
may be one login. Two unrelated logins whose resets fall on the same second
would also match, so this is a candidate, not proof. Values of different
accounts are never summed, averaged or used for each other: each window
keeps its own `used`, `remaining` and `observed_increase`. Adding two homes'
increases would count the same consumption twice.

**`accounting quota [--json]`** (read-only; `collection_not_run` without a
sidecar, `ledger_not_synced` before a sync): `account_basis:
execution_home`, `windows`, `shared_window_candidates` (per candidate
`{limit_id, window_kind, window_minutes, resets_unix_ms, accounts,
window_ids, evidence, merged: false}`, accounts in account order; text: one
`shared window candidate …` line each), `observations`
(count per window kind and trust), `evidence.rate_limit_reached_type`
(`snapshots` per value, `semantics: not_certified`, `certified: fixture`),
and `metrics`:

- M38 `throttled_time_share`: `unavailable throttling_not_certified`. Codex
  rollouts carry no throttled intervals; `rate_limit_reached_type` is a point
  tag kept as evidence only until its semantics are certified.
- M39 `provider_error_rate`: `unavailable provider_errors_not_certified`. No
  typed provider error field is collected or certified; human-readable
  messages never become certified fields.
- M40 extended (`M40.quota-windows-v1`): per dispatch decision (attempt
  order), `{attempt_id, decided_unix_ms, service, account, account_basis:
  execution_home, windows}` (`account_basis` also on the decision-level
  `no_observation`/`no_trusted_observation` entries); per
  limit, `primary` = the latest trusted observation of the attempt's account
  with `observed ≤ decided` (ties: session, ordinal descending) with
  `window_id, window_minutes, resets_unix_ms, observed_unix_ms, age_ms`, then
  `value` = remaining percent, `used`, and `freshness` (`stale` when
  `age_ms > stale_after_ms` = 900000, value still shown); if the window reset
  at or before the decision, `value` is `unavailable
  window_reset_since_observation` (the new window's value is unknown).
  When other accounts reported a trusted observation of the same window
  (same limit, kind, minutes and reset) by the decision, the entry has
  `shared_window_candidates` = every such account, the decision's included,
  in account order. The value is still the decision account's own; a newer
  value from another home is never substituted.
  `secondary` follows the same rules from the secondary observations; with
  no trusted one it is `unavailable not_reported` (the latest snapshot's
  window was `null`), `not_collected` (no snapshot carries the kind, e.g.
  stored before A4) or `no_trusted_observation`. Decision-level reasons:
  `adapter_absent`, `execution_home_unknown`, `no_observation` (no snapshot
  of the account by then), `no_trusted_observation`. Native units; never
  summed or averaged across accounts, limits or services.

M38 and M39 also reach `telemetry <slug> report` through the lane
`metrics()` hook. The report's M40 (contracts §6) is this extended form: the
report and `accounting quota` give identical per-decision entries.

Test `window_reset_starts_new_window_not_negative`: a 300-minute `primary`
window reads 40 → 55.5 → 50 (flagged, not subtracted) → 60 one minute before
dispatch (headroom `40`, age 60000, fresh, increase `20`); after its reset,
5 → 12.25 under a later `resets_at` open a second window (`reset_elapsed`,
increase `7.25`, remaining `87.75`), never −55. A snapshot 20 minutes old is
`stale` (`62.5`, age 1200000); one whose window reset before the decision is
`window_reset_since_observation`; no snapshot is `no_observation` and one
with `primary: null` is `incomplete` → `no_trusted_observation`, never 0.
Its rollouts report `secondary: null`: `not_reported`.

Test `shared_window_across_homes_is_flagged_not_summed`: home A reads
37.5 one minute before dispatch; home B reads 37.5 then 40 (two minutes and
half a minute before) for the same 300-minute window, and a secondary window
that A reports as `null`. Three windows: A primary (increase `0`, remaining
`62.5`), B primary (increase `2.5`, remaining `60`), B secondary (`10`). One
candidate names A's and B's primary windows; the secondary is not shared.
A's headroom is `62.5` (its own), not B's newer `60` and not
100 − (37.5 + 40). The report's M40 equals `accounting quota`'s.

Test `secondary_window_is_tracked`: a 10,080-minute secondary window reads
10 → 12.5 before dispatch (headroom `87.5`, age 120000, increase `2.5`),
11 afterwards (flagged), then `null` (`not_reported`) while the primary
resets (20 → 30, then 5 → 6 in a new window); one `primary` reached type
is evidence; M38 stays unavailable. With the A4 rows removed (as stored
before A4), secondary is `not_collected`.

## 6. Human attention intervals (B6b, TM1.8 remainder "S4")

Stream `accounting` version 5 (`0005_attention.sql`). Plan: doc 03
`HumanAttentionInterval`, doc 07 §5a M31–M33, doc 10 §5a. Replaces the
central `attention_not_collected` M31–M33 (contracts §6) through the lane
`metrics()` hook once any sample exists; the outcome record's `attention`
field (contracts §4) shows this lane's per-attempt summary once any sample
exists.

**Signal.** Stock Herdr's `agent list` (`result.agents[]`), field
`agent_status`: the waiting state is `blocked`; `working`, `idle`, `done`
are not waiting. Certification basis: **`live` for Codex**, `fixture` for
every other agent kind. In the A4 live run
([codex-live-0.154.0-a4.md](codex-live-0.154.0-a4.md) §3; herdr 0.9.1,
codex 0.154.0), a Codex approval prompt routed to the human showed as
`blocked` from the first sample after the prompt for 10 consecutive 4 s
samples. It then went `working` after the approval key and `idle` after
that. The product measured one closed wait of 37848 ms. For claude,
`blocked` was observed live only for its trust dialog (docs/herdr-notes.md,
stages 2 and 3), not for its approval prompts, so it stays `fixture`.
**Caveat:** Codex's automatic reviewer (`approvals_reviewer =
auto_review`, the guardian child session) decides an approval without the
human, and such an attempt never shows `blocked` (the review appears as
`working`). M31/M32 therefore cover **human-routed waits only**
(`scope: human_routed_waits`); auto-reviewed approvals are not waits and are
not counted. `signal` carries `certified: live` (the signal as certified),
`certified_by_agent_kind {codex: live}`, `other_agent_kinds: fixture`,
`scope` and `caveat`. Each attempt entry of `accounting attention` carries
its own `certified` (`live` for kind `codex`, else `fixture`). The outcome
record's `attention.basis` (contracts §4) reads `signal.certified`. It shows
`live` for every attempt until the steward takes the attempt's own
`certified`.
Herdr gives no timestamp for a state change and no typed reason, so every
interval has reason type `blocked_untyped` and sample resolution. The
existing thread group rule (`blocked` ≥ 30 s → waiting-on-you) is finer than
the sampling interval and is not applied.

**Observation** (`accounting observe-attention`, and the ticker pass through
the lane `tick` hook before the ledger sync, at the telemetry pass interval
`HERDR_PROJECTS_TELEMETRY_COLLECT_SECS`, default 300 s). The canonical binding
of an attempt is its latest `runtime.launch_started` receipt (route socket,
workspace, tab, pane, cwd; agent kind and name). Every attempt with a receipt
that is not terminated and is `launching`, `running` or `awaiting_input` gets
one `attention_samples` row per pass: the label, or a gap reason. Herdr is
queried read-only, one `herdr agent list` per distinct socket
(`HERDR_BIN_PATH` else `herdr`; `HERDR_SOCKET_PATH` set, `HERDR_SESSION`
removed, as the CLI's client), through the gated runner, 5 s timeout, at most
4 sockets per pass and each reply ≤ min(1 MiB, remaining budget). Nothing is
ever sent to an agent or pane. The agent must be the only one on the recorded
pane and match the receipt's workspace, tab, cwd, kind and name. Gap reasons:
`herdr_unreachable` (socket missing, no reply, timeout), `herdr_error`,
`herdr_reply_invalid`, `agent_absent`, `identity_mismatch`, `state_unknown`
(`unknown` or empty), `state_unrecognized`, `remote_route` (a machine route
is not observed), `route_unrecorded`, `budget_exhausted`. Only labels, gap
codes, timestamps and the sampling interval are stored; never pane text,
titles, cwd or names. A pass needs an existing sidecar (`collection_not_run`).

**Intervals** (derived at read time, per attempt, samples in time order; a
label holds until the next sample). Consecutive successful samples are
continuous when no failed sample lies between them and they are at most
twice the sampling interval apart; otherwise the span is a gap (the first
failure's reason, else `not_observed`, e.g. the ticker not running), and so
are launch → first sample and last sample → terminal mark when longer. An
open attempt whose last sample is older than twice the interval has a gap
`not_observed` with `to_unix_ms: null`. A wait opens at the first `blocked`
sample: `start` is `observed_transition` (after a continuous non-waiting
sample), `first_observation`, or `after_gap`. It ends `closed` at the next
continuous non-waiting sample, or is censored: `observation_gap` (with
`gap_reason`), `attempt_ended` (still waiting at the last sample before the
terminal mark), `open_at_horizon`. Only `observed_transition` + `closed`
has a `duration_ms`; censored waits are listed and counted apart, never
closed at a guessed time. An `after_gap` wait following a wait censored by
that gap may be the same wait: kept, `counted: false`
(`uncertain_starts`). Per attempt: `waiting_ms` = Σ durations;
`observed_ms` = continuous non-waiting time + `waiting_ms` (time inside
censored waits is excluded). An attempt with no successful sample is
`unavailable not_observed` with its gaps, never 0.

**`accounting attention [--json]`** (read-only): `signal`, per launched
attempt `{attempt_id, task_id, state: open|ended, launched_unix_ms,
ended_unix_ms, certified, attention}`, `orphan_samples`, `fleet {waiting_union_ms,
waiting_sum_ms, interventions}` (overlapping waits of different attempts
counted once in the union), and `metrics`:

- M31 `human_interventions_per_accepted_task` (`M31.attention-v1`): counted
  wait starts of launched attempts of `T` / `count(A)` (contracts §6 cohort,
  same window rule). Needs every such attempt observed without gaps; else
  `unavailable incomplete_observation` with `observed_interventions` (a lower
  bound), `uncertain_starts` and `denominator`. `coverage {attempts, complete,
  not_observed, with_gaps}`, `scope: human_routed_waits`.
- M32 `waiting_on_you_share` (`M32.attention-v1`): Σ `waiting_ms` / Σ
  `observed_ms` over launched attempts decided in the window (all without
  `--since`), unit ms, unreduced `"n/d"`; `waiting_union_ms` shows the fleet
  union; `coverage {attempts, observed, not_observed, with_gaps,
  censored_intervals}`, `scope: human_routed_waits`. No observed attempt →
  `unavailable not_observed`.
- M33 `permission_prompts_per_attempt`: `unavailable
  attention_reason_not_exposed` (`blocked` has no typed reason).
- Before any sample exists (or without a sidecar) all three stay
  `unavailable attention_not_collected`.

Test `attention_intervals_union_and_censor` (Herdr stand-in, 60 s interval,
passes re-timed to fixed minutes): a1 waits 1–3 (120000) and again at 4,
ending at 4.5 → `attempt_ended`; a2 waits 2–6 (240000), at 7 (censored by a
`herdr_unreachable` gap 7–9), at 9 (`after_gap`, not counted), then gaps
10–13 and from 13 (`not_observed`); a3 cancelled before any pass is
`not_observed`. Union 300000, sum 360000; M31 `2/1`; M32
`360000/660000`; every Herdr call is `agent list`; no screen text stored;
the signal is `live` for its Codex attempts.

## 7. A4 metadata (B7) and stream version 6

Stream `accounting` version 6 (`0006_a4_metadata.sql`) consumes lane A's A4
tables (ingest 0004, contracts-collection.md A4), read by SQL only (the
A4 live run certified `model_provider`, `subagent_kind` and record line
times `live`; `secondary` windows, `rate_limit_reached_type`,
`forked_from_id` and spawned-subagent links stay `fixture`, see
[codex-live-0.154.0-a4.md](codex-live-0.154.0-a4.md) §1): `rollout_metadata`
(§3 roles and child links, §4 provider check), `codex_usage_times` (§4 usage
interval) and `codex_rate_limit_windows` (§5 secondary window and reached
type). It adds the derived tables `session_nodes` (replacing
`session_graph`: roles `fork`, linkage `linked_child`, `parent_session_id`,
`link_basis`, `parent_reason`, `claimed_parent_session_id`, `forked`) and
`quota_window_observations` (replacing `quota_observations`: keyed by window
kind, trust `not_reported`, `rate_limit_reached_type`), and the append-only
`valuation_bases(revision, entry_id, usage_basis, provider_check)`. Like
every stream migration it is re-runnable (`IF NOT EXISTS`, no `ALTER`). The superseded tables
are left as they were (no longer written or read; nothing is dropped).
Migrating to 6 clears `usage_ledger`, so everything reads
`ledger_not_synced` until the next sync.

## 8. A5 thread lineage (B9) and stream version 7

Stream `accounting` version 7 (`0007_thread_lineage.sql`) consumes lane A's
A5 `rollout_threads(path_digest, parent_thread_id, session_id,
thread_source)` (ingest 0005, contracts-collection.md A5, certified `live`),
read by SQL only with a `LEFT JOIN` on `path_digest` beside
`rollout_metadata`. A sidecar without the table (before A5) reads with no
lineage, exactly as version 6 did. `rollout_threads.session_id` is never
read.

- `parent_thread_id` is parent evidence when `subagent_parent_thread_id` is
  `NULL` and it is not the session's own id (§3, `link_basis`
  `thread_parent_thread_id`, `certified: live`).
- `thread_source = guardian_review` is guardian role evidence (§3).
- A linked guardian's inclusion is `separate`; it is never inside its
  parent's total (live: parent 29760, guardian 7462, attempt 37222).

`session_nodes`' `link_basis` CHECK cannot hold the new basis, so version 7
adds `session_graph_nodes` (the same columns, `link_basis` also
`thread_parent_thread_id`), written and read in its place (also by §4's
role lookup). `session_nodes` is left as it was (no longer written or read;
nothing is dropped). Re-runnable (`IF NOT EXISTS`, no `ALTER`). Migrating to
7 clears `usage_ledger`, so everything reads `ledger_not_synced` until the
next sync.

## 9. Tool calls and executions (B5, TM2.5; M16–M18)

Plan: doc 07 M16–M18, doc 05/12 TM2.5. Consumes lane A's A6 metadata
(contracts-collection.md A6; ingest 0006 `codex_tool_calls`,
`codex_exec_items`, `codex_tool_sources`), read by SQL only and **derived at
read time**: no stream migration, nothing stored, no sync needed. Only
identifiers, tags, exit codes and line times are read; the tables hold no
content. Codex 0.154.0 writes no approval request or decision and no
execution run time (codex-live-0.154.0-a4.md §3–§4), so the design reports
what those fields honestly give and names the rest `unavailable`.

**Scope and coverage.** A session (the rollout's own `session_meta.id`) is
in scope when one of its rollouts is `bound` (attempt ids listed); sessions
without one are counted in `coverage.excluded` by binding (`unbound`, …),
and a session with a bound rollout of an uncertified `cli_version` as
`cli_version_uncertified`. The report hook's `--since` keeps sessions whose
earliest `session_unix_ms` is in the window. Per session, tool metadata is
`unavailable: predates_collection` when the sidecar has no A6 tables (a
read-only pre-A6 sidecar; nothing is migrated) and `unavailable:
pending_reread` while any rollout of the session has no `codex_tool_sources`
row. If any in-scope session is either, M16–M18 are all `unavailable` with
that reason (`predates_collection` first) and the `coverage`, never a
partial count and never 0. No in-scope session: `unavailable
no_bound_session`; no sidecar: `collection_not_run`. `coverage {sessions,
observed, pending_reread, predates_collection, excluded}`.

**M16 `tool_call_volume`** (`M16.tools-v1`), stages reported separately,
`value {issued, accepted, executed}`:
- `issued`: distinct `(session_id, call_id)` rows with a recorded call
  (`call_kind` not `NULL`). A call replayed by a resumed rollout or a retried
  record is the same key, so it is one logical call. `issued` also gives
  `by_name`/`name_unreported`, `by_status`/`status_unreported`
  (`function_call.status` certified `fixture` only), `without_output` (no
  output yet: open or lost) and `outputs_without_call` (an output whose call
  was not seen, not counted as issued).
- `accepted`: `unavailable approval_decision_not_exposed`. There is no typed
  decision; auto-approved and human-approved calls look the same.
- `executed`: one `CommandExecution` item per execution instance (a repeated
  execution is another instance), `scope: command_execution` (other tools,
  e.g. `wait`, write no item), `by_source`. Attribution to a call is
  `inferred` (no shared key): the latest recorded call of the same session
  and turn at or before the item's `completed_unix_ms` whose output, if any,
  is not before it; `by_call_name` and `unattributed`.
- `certified {calls: live, call_status, exec_items: live, mcp_calls:
  not_collected}`.

**M17 `tool_execution_success`** (`M17.tools-v1`): succeeded / (succeeded +
failed) of exec items, unreduced `"n/d"` (`null` `empty_denominator` when
none is terminal). Status `completed` (the only status certified live) with
`exit_code` 0 succeeded, non-zero failed. Unknown, excluded and counted in
`unknown.by_reason`: `exit_code_unknown` (`NULL`, never a success),
`status_unreported`, `status_not_certified` (any other status, e.g.
`failed`: its meaning, including cancel or timeout, is not certified).
`cancelled` and `timed_out` are `unavailable` (`cancellation_not_exposed`,
`timeout_not_exposed`). An exec item is written only on completion, so a
pending execution is not observable: `pending_calls` counts calls without
an output.

**M18 `tool_latency_p95`** (`M18.tools-v1`): `value` `unavailable
execution_duration_not_exposed`. The exec item's `duration` and start/end
times are the unified exec startup (`startup_not_run_time`) and are never
read as run time. `queue_time` `unavailable approval_decision_not_exposed`,
`timed_out` `unavailable timeout_not_exposed`, `pending_calls`. Shown apart,
**never as M18's value**: `call_to_output_ms`, the distribution of
`output_unix_ms − called_unix_ms` over calls with both times (`samples`,
nearest-rank `p50_ms`, `p95_ms`, `max_ms`; `null` without samples), overall,
`by_name` and `name_unreported`, with `negative_intervals` (excluded),
`caveat: includes_approval_wait` (live: 36.9 s call → output against a
37.8 s Herdr `blocked` wait).

**`accounting tools [--json]`** (read-only; `collection_not_run` without a
sidecar): per in-scope session `{session_id, attempt_ids, tools}` with
`tools` `{issued, without_output, outputs_without_call, executed,
attributed, unattributed, succeeded, failed, unknown}` or `unavailable`;
`coverage`; `metrics` M16–M18, identical to the report's (lane keys, §2).
Text: a coverage line, one line per session and per metric, and the
`call_to_output_ms` p95 line labelled as including approval waits.

Not derived (follow-ups): the accepted stage inferred from a call overlapping
a B6b `blocked` wait (human-routed only) or a same-turn guardian session
(auto-review); per-host (execution home) breakdown; controller intervals
(TM2.5: Codex writes no typed tool or approval interval, and Herdr samples
only agent state); MCP calls (shape unobserved, not collected); M18 once a
Codex version records an execution end − start.

Test `tool_volume_success_and_latency_are_honest`: one bound session in two
rollouts (the second resumes the first and adds turn 2) and one unbound
session. 5 issued (`exec` 4, `wait` 1; status unreported 1), 1 without
output, 1 output without a call; 6 executions, 5 inferred to `exec` calls,
1 unattributed; exit 0 ×3, exit 2, `NULL` exit, status `failed` → M17
`3/4`, unknown 2; M18 unavailable, `call_to_output_ms` 37010, 1000, 2500,
300 → p50 1000, p95 37010 (`exec` 3 samples, `wait` 2500); the report
equals `accounting tools`; before any collect everything is
`collection_not_run`. Test `tool_metrics_before_a6_or_reread_are_unavailable`:
the A6 tables dropped (ingest 5) read as `predates_collection` without
migrating; the next collect restores the output byte for byte; the resumed
rollout gone before its re-read makes the session `pending_reread`.

## 10. Fleet efficiency (B6a, TM2.8; M34–M37)

Plan: doc 07 M34–M37, doc 10 §5a "Fan-out", doc 12 TM2.8. **Derived at read
time** from canonical `state.db` rows only, opened read-only
(`telemetry::read_only`): no stream migration, nothing stored, no sidecar
needed or created. Only worker attempts are counted. The coordinator has no
canonical attempt, so its time and cost never enter a worker figure (M34
below).

**Active intervals.** An attempt is active from its `running` lifecycle mark
to its terminal mark (contracts §4, the `active_ms` interval). Per attempt,
counted in `coverage {attempts, running_intervals, open_censored,
never_running, predates_lifecycle_log, end_unknown}`:
- Running and terminal marks: a known interval.
- Running mark, attempt open: active up to the read's horizon (now),
  `open_censored`. Its outcome is not known yet. It touches only windows up to
  the horizon, and the current window is never bucketed.
- Logged (`reserved` mark) but never `running`: not active (`never_running`).
- Running mark, terminal attempt without a terminal mark: the end is unknown
  (`end_unknown`). Every window from `running` to the horizon has unknown
  concurrency.
- No `reserved` mark (predates the log, `predates_lifecycle_log`): if the
  attempt is terminal without a terminal mark, it ended before the log and is
  ignored. Otherwise it was active at an unknown time from the log's first
  mark to its terminal mark (or the horizon), and those windows have unknown
  concurrency.
- A store without `attempt_lifecycle`: M35 and M36 `unavailable
  predates_lifecycle_log`. No `state.db`: `no_state_store`.

**Windows and buckets.** Activity windows are whole UTC hours (`window_ms`
3600000) that hold active time or an acceptance. A window's time-weighted
active-attempt count is Σ overlap / `window_ms`. Its level `k` is that count
rounded half up (`round_half_up(time_weighted_active_attempts)`). Some
windows are not bucketed. They are counted in `windows.excluded` as
`incomplete` (ends after the horizon), `concurrency_unknown` (overlaps an
unknown span) or `outside_window` (starts before `--since`). Accepted
throughput counts each task of contracts §6 `A` once, at its first acceptance
evidence: `verified_results.created_unix_ms` for `verify_only`, and
`integrated_commits.created_unix_ms` for `verify_then_integrate`. The
evidence lands in the window that contains that time. Per bucket:
`{level, windows, window_ms, active_ms, mean_active, accepted,
accepted_per_hour, per_agent_per_hour, m35, marginal_per_added_agent_per_hour,
mix, mix_tvd}`.

All quantities are exact reduced rationals as strings (`"3/4"`, `"2"`), never
floats.
- `accepted_per_hour` = accepted × 3600000 / bucket `window_ms`.
- `per_agent_per_hour` = that / `k`.
- The **reference** level is the lowest level ≥ 1 with an accepted task.
- `m35(k)` = `accepted_per_hour(k)` / (`k` × the reference's per-agent rate).
- Marginal = the difference in `accepted_per_hour` from the previous level ≥ 1
  bucket, divided by the difference in levels (`null` for the first).
- A level-0 bucket carries `unavailable level_zero` for these fields.

**M35 `fan_out_efficiency`** (`M35.fanout-v1`): `value` = `m35` of the
highest level, with `level`, `reference_level`,
`reference_per_agent_per_hour` and `marginal_per_added_agent_per_hour`.
Otherwise it is `unavailable`:
- `no_complete_window`: no bucketed window.
- `no_accepted_throughput`: no reference level.
- `single_concurrency_level`: the highest level is the reference.

**Comparability** (`class_band_active_time_tvd`). A bucket's task mix is the
share of its active time per decision classification `class/band`
(contracts §1/§3). An attempt without one counts as `unclassified`. M35 is
`label: comparable` only when two conditions hold. First, no bucket at
level ≥ 1 holds `unclassified` time (else reason `classification_unknown`).
Second, each such bucket's total variation distance from the reference
bucket's mix (`mix_tvd` = ½ Σ |share difference|) is at most `max_tvd`
`1/10` (else `task_mix_differs`). Otherwise it is `label: descriptive`, and
`value` is unchanged. The label is on the metric and in `comparability
{test, max_tvd, reference, label, reasons}`.

**M36 `integration_conflict_rate`** (`M36.integration-v1`). An attempt
reaches integration when it has an `integration_operations` row, joined
through `verified_results` to `result_submissions.attempt_id`.

The integrator records two conflict/rebase events:
- `blocked` / `merge_conflict`: the merge onto the target conflicted.
- `discarded` / `stale_base`: the target moved after the candidate was
  built, so the candidate must be rebuilt.

An event counts when it is on an operation that did not integrate and was
created no later than the attempt's first integrated operation (either state
`integrated` or an `integrated_commits` row). The formula is attempts with at
least one event / attempts reaching integration, as an unreduced `"n/d"`
(`null empty_denominator` for none). It carries `events` (count per kind) and
two splits:
- `by_target`: per `ref_name`, over that ref's operations.
- `by_bucket`: the attempt's own concurrency level, time-weighted active
  attempts over its active interval, rounded as above. The bucket is
  `unknown` without a known interval or when an unknown span overlaps it.

`scope: integrator_observed`. A worker rebasing inside its worktree is not
observed (`not_observed: [worker_side_rebase]`). `--since` keeps attempts
whose first operation is at or after it.

**M34 `coordinator_overhead`**: `unavailable
coordinator_usage_not_attributed`, `missing: [coordinator_usage_scope,
coordinator_allocation_rule]`. The coordinator has no canonical attempt. Its
Codex rollouts are unbound (contracts §5 binding rule), so no usage is
recorded under a `coordinator` role scope. No versioned allocation rule
exists either, and costs are published-rate estimates only (§4). The doc 10
fixture (coordinator $4, workers $16 → 20%) needs both producers. Until
then, no worker or per-arm figure includes any coordinator cost.

**M37 `overlap_waste_share`**: `unavailable supersession_reason_not_recorded`,
`missing: [accepted_supersession_reason]`. No canonical record says an
attempt was superseded or abandoned because a sibling changed the same area.
Candidate selections (contracts-quality.md §3) name a winner, not why
another attempt was superseded.

**`accounting fleet [--json]`** (read-only) prints `{fleet: {window_ms,
horizon_unix_ms, coverage, windows {bucketed, excluded}, buckets,
comparability}, metrics: {M34, M35, M36, M37}}`. The metrics are identical to
the ones `telemetry <slug> report` shows through the lane hook. The text form
has a windows line, one line per bucket (`bucket k=8 windows=1 accepted=3
per_hour=3 per_agent=3/8 m35=3/4 marginal=1/4`) and one line per metric (M35
followed by its label).

Test `fan_out_buckets_and_integration_conflicts`:
- Hour 0: 4 agents, 2 accepted → `2`/hour. Hour 1: 8 agents, 3 accepted →
  `3`/hour. Reference `1/2` per agent, M35 `3/4`, marginal `1/4`,
  `comparable`.
- An open attempt from the current hour is censored (`incomplete` 1).
- A pre-log attempt that ended before the log is ignored.
- M36 `3/4`: a merge conflict before integration, a clean integration, a
  stale base before integration, and a conflict on `refs/heads/release`
  that never integrated. By target main `2/3` and release `1/1`; by
  bucket 4 `1/1` and 8 `2/3`. The report equals `accounting fleet`.
- `--since` hour 1: M35 is `single_concurrency_level` and M36 is `2/3`.
- The same rows with docs tasks in hour 1 give `descriptive`
  (`task_mix_differs`, `mix_tvd` `1`), with the value unchanged. Adding an
  unclassified attempt adds `classification_unknown`.
- An open pre-log attempt makes both hours `concurrency_unknown`: M35 is
  `no_complete_window`, never 0, and M36's bucket is `unknown`.

Follow-ups: coordinator usage scope and allocation rule (M34); a supersession
reason producer (M37); worker-side rebase capture; the factory's scale-trial
steps (F4.6/F5.4) as named concurrency steps; per-configuration fan-out.

## 11. Stream 8: superseded projections dropped

Accounting stream 8 (`0008_drop_superseded.sql`) drops `session_graph` (v2),
`quota_observations` (v4) and `session_nodes` (v6). Later versions replaced
them (`session_graph_nodes`, `quota_window_observations`) and nothing reads or
writes them. They were projections rebuilt on every sync, so no source data
is lost. `DROP TABLE IF EXISTS` keeps the migration re-runnable when the
streams table is lost and every migration runs again. Checked end to end in
`attention_intervals_union_and_censor`.
