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
- Delta, repeated response (TM2.6, [certificate-core.md](certificate-core.md)):
  an otherwise `accepted` record whose session already holds an accepted
  record with the same native `response_id` and the same `payload_digest`
  at an earlier ordinal is `duplicate` / `response_repeated`: one response
  written twice (doc 10 §3 "Replay"). It is not counted (M08/M09), never
  valued (§4, so an estimate stays complete) and adds nothing to its
  rollout's `inclusive_total` (§3). The same `response_id` with another
  payload, or in another session, is another invocation. The Codex row
  itself stays accepted, so the steward's `usage`/`attempts` sums still
  include it (certificate restriction R3).

Sync, the collector's per-rollout pass and reprice take the sidecar write
lock up front (immediate transactions), so a ticker pass and a CLI command
racing on one project wait for each other (busy timeout) instead of failing.

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
time), M34–M37 (§10, derived at read time from canonical rows, the latest
valuation revision and worktree reflogs), M12/M14 (§12, from the latest
stored valuation revision), M11 (§13, from imported provider charges) and
M04 (§14, from the latest valuation revision and canonical task evidence).

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
  all three: `thread_parent_thread_id` (the live guardian's
  `parent_thread_id` is its parent's `session_meta.id`), and since B12
  (§15) `parent_thread_id` (a live `thread_spawn` child) and
  `forked_from_id` (a live `codex exec fork`), codex-live-0.154.0-run2.md
  §1–§2. A named parent
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
  excluded its guardian's 7462, codex-live-0.154.0-a4.md §5; the live
  `thread_spawn` parent's 43951 excluded its child's 15202, run2 §2). For a
  session naming a `forked_from_id` it is read at read time from the root
  rollout's A8 `rollout_forks` row (§15): `separate` when it names
  `history_base.thread_id` (the live fork shape, which replays none of its
  origin's records, run2 §1), with `fork_reconciliation` = the fork's
  `codex_fork_reconciliation` states per reported total (`unavailable:
  not_reconciled` without a row); `unavailable: fork_replay_not_certified`
  when it names none (a shape never observed live); `unavailable:
  predates_collection` without the A8 table; `unavailable: pending_reread`
  while the source has no row. A fork's reported totals (which include its
  origin's) are never added: its total is Σ its own entries. The children
  `total_tokens` is their sum only when every child total is known
  (`unavailable: incomplete` otherwise) and every inclusion is `separate`
  (else the first child's inclusion reason).
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
children (`linked_children`, `unavailable` with the first linked child's
inclusion reason when one is not `separate`) and of unlinked children (`unlinked_children`) separately;
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
rollup 100 / 30 / 0 (`live` since B12). A fork of 40 (`forked_from_id`,
`live`, but no `history_base`: `fork_replay_not_certified`), a subagent of 20
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
revision, and repricing never changes measured tokens. From stream version 9
a revision stores only the rows that changed since the previous one (§12). Basis is always
`published_rate_estimate`; provider charges and invoices are separate bases
(§13), and an estimate is never added to one.

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

Reproducibility across a sidecar rebuild: a `record_time` interval is the
line's own time, so a rebuilt sidecar reprices it identically. The fallback
interval is **not** rebuild-stable: its end is the first observation, and a
rebuild observes every record again (later). For an entry without a line
time a reprice after a rebuild can therefore append different revision
content: `usage_interval.to_unix_ms` changes. The valuation can change too,
when a card boundary (`effective_from`/`effective_to`) falls between the old
and the new observation (`rate_change_within_usage_interval` appears or
disappears). Live Codex lines carry `timestamp` (certified live, A4), and a
rebuild re-reads every line, so a rebuilt sidecar uses the fallback only for
a line with no parsable `timestamp`. The rebuild certificate
(certificate-core §2, "Analytics rebuild") covers `record_time` entries
only. Derived views that must survive a rebuild never read the fallback
interval: M34's allocation (§10, v2) treats it as `usage_time_unknown`.

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
identical to when it was appended (a delta revision is replayed onto the
full copy below it, §12). Per session and per attempt (bound
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
record has no line time, so it keeps the fallback interval); the report's
M12 is that `partial` and M14 `2/6` (§12). Revision 1 is then rewritten as
a pre-version-9 binary left it (a full copy in `valuations`, no delta
tables, stream 8) and reads back byte-identical. A corrected version 3
(output 6 → `0.0035`) and a EUR card (`0.00021`) append revision 2 as a
delta of the 2 changed rows (attempt and M12 `mixed_currency`: USD
`0.0075`, EUR `0.00021`; M14 `3/6`); revision 1 (full copy) and 2 (delta)
read back byte-identical and the ledger is unchanged.

Test `record_times_narrow_rate_card_interval`: one session starting before a
boundary and first observed after it; its record timed before the boundary
prices at `0.004` (version 1), the one timed after at `0.0041` (version 2),
the one without a line time straddles (`rate_change_within_usage_interval`,
basis `session_start..first_observed`). Provider `openai` against the
`synthetic` cards is `provider_mismatch`; no provider prices at `0.00036`
marked `provider_unverified`; attempt `partial` `0.00846`. Version 3 appends
revision 2 (`0.00782`) storing only its 2 changed rows (7 delta rows in
all, none in `valuations`); revision 1 reads back byte-identical.

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
before the earlier one elapsed; it was not observed live. The second live
run ([codex-live-0.154.0-run2.md](codex-live-0.154.0-run2.md) §6) saw the
first **jitter**: `resets_at` 1791049774 in 10 snapshots and 1791049779
(+5 s) in one, within one window, and `plan_type: null` in the snapshots of
`exec` sessions (`pro` elsewhere). Since B12 windows match within
`RESETS_TOLERANCE_MS` = 60000 (below).

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

**Same window (B12).** Observed before the current window's reset elapsed,
a snapshot whose `resets_at` differs from the window's by at most
`RESETS_TOLERANCE_MS` = 60000 ms (either way) is the **same window** (jitter),
never `reset_moved` or `window_regressed`; it follows the same-window rows
(`window_conflict`, `used_decreased_without_reset`, `trusted`). The window
keeps its first `resets_at` (and its `window_id`); each observation keeps its
own reported `resets_unix_ms`. Beyond the tolerance, or observed at or after
the reset, the rows above apply. A real next window lies at least
`window_minutes` later, and every observed window is ≥ 300 minutes. 60 s is
12× the only jitter seen. `plan_type` is never part of a window's identity:
a `null` plan (live: `exec` sessions) neither opens a window nor clears the
window's latest non-null `plan_type`.
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
`window_minutes` and `resets_unix_ms` (within `RESETS_TOLERANCE_MS` of the
group's earliest reset, which the candidate shows, since B12) are a
**shared-window candidate** (evidence `same_limit_kind_minutes_resets`,
`merged: false`); M40's per-decision `shared_window_candidates` use the same
tolerance around the decision's observation. The accounts
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

Test `resets_jitter_and_null_plan_stay_one_window` (B12, run2 §6 shapes):
a 10,080-minute window reads 43 (R, `pro`) → 43 (R + 5 s, `pro`) → 44
(R − 5 s, `null`) → 45.5 (R, `null`): one window (`first_observation`, 4
trusted, increase `2.5`, remaining `54.5`, plan `pro`), each observation
keeping its own reset. Another home's one snapshot at R + 5 s makes one
shared-window candidate (reset R), and A's headroom `54.5` names both. Then
46 at R + 120 s opens a `reset_moved` window (remaining `54`, the new
headroom) and 47 back at R is `window_regressed`.

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
row. Since B12 (§15) the same holds for lane A's A8 tables
(`rollout_forks`, `codex_mcp_calls`, `codex_turn_aborts`,
`codex_tool_namespaces`, `codex_agent_items`): none →
`predates_collection`; a rollout of the session without a `rollout_forks`
row (read before A8, re-read by the next collect) → `pending_reread`. If
any in-scope session is either, M16–M18 are all `unavailable` with
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
  was not seen, not counted as issued). Since B12: `by_namespace` (A8
  `codex_tool_namespaces`; live: `collaboration` for `spawn_agent` /
  `wait_agent`) and `mcp_without_call`. An MCP call is **one** call: in
  0.154.0 it runs inside an `exec` `custom_tool_call` (code mode), which is
  already issued; an MCP item whose carrying call is not matched (below) is
  counted once more in `calls` as `mcp_without_call` (not in `by_name` or
  `by_status`, stage unknown).
- `accepted`: **inferred** (`value.accepted {status: inferred, count,
  unknown}`), never from a typed decision (Codex writes none). A recorded call
  with both times reached the accepted stage when its call → output interval
  `[called, output]` overlaps a B6b `blocked` wait of one of the session's
  bound attempts, taken from its first to its last `blocked` sample (§6;
  closed or censored, counted or not): `human_routed`. Or when a guardian
  session names this session as its parent (native evidence only, §3/§8:
  A5 `thread_source = guardian_review` or A4 `subagent_kind = review`,
  parent in `subagent_parent_thread_id` else `rollout_threads.parent_thread_id`)
  and its session start lies inside that interval (so in the call's turn):
  `auto_review`. Both: `human_routed_and_auto_review`. Every other issued call
  (neither, or no output yet) is `unknown`, never counted as accepted.
  **Aborted (B12, run2 §4).** A call whose turn has a `codex_turn_aborts`
  row with a line time is `declined_or_aborted`, neither accepted nor
  unknown, when its output is the turn's last at or before the abort
  (`last_output_before_abort`: live, a declined approval (`n`) aborts the
  turn right after the call's string output) or it has no output and was
  made at or before the abort (`no_output_before_abort`). This wins over a
  `blocked` wait (live, that wait was the declined prompt). Other calls of an
  aborted turn keep their stage. `value.accepted {status: inferred, count,
  unknown, declined_or_aborted}`; `accepted {label: inferred, calls,
  by_basis, unknown, declined_or_aborted {calls, by_basis}, basis, detail,
  caveat}`; caveat: a denied approval that does not abort the turn also ends
  the wait, so `accepted` means the approval stage completed, not that it was
  approved.
- `executed`: one `CommandExecution` item per execution instance (a repeated
  execution is another instance) and, since B12, one A8 `McpToolCall` item
  per MCP call: `scope: [command_execution, mcp]`, `by_scope` (other tools,
  e.g. `wait`, `spawn_agent`, write no item), `by_source` (command
  executions). Attribution of a command execution to a call is `inferred`
  (no shared key): the latest recorded call of the same session and turn at
  or before the item's `completed_unix_ms` whose output, if any, is not
  before it; `by_call_name` and `unattributed`.
- `mcp` (B12): `{calls, by_server {server: {tool: n}},
  server_or_tool_unreported, carrier {basis: inferred, rule, matched,
  unmatched}, basis}`. The carrying call of an MCP item (its `exec-…` id is
  no call id) is matched only by `(session_id, turn_id)` and line time: the
  latest recorded `custom_tool_call` named `exec` of the same turn at or
  before the item's `completed_unix_ms` whose output, if any, is not before
  it, each call carrying at most one item (items in completion order).
- `collaboration` (B12): `{calls, spawned_threads, collab_items, basis}`:
  calls in namespace `collaboration`, the distinct `agent_thread_id`s of
  `SubAgentActivity` items whose id equals such a call's id (the spawn), and
  the `CollabAgentToolCall` items. Not tool executions.
- `certified {calls: live, call_status, exec_items: live, mcp_calls: live,
  turn_aborts: live, namespaces: live}` (`mcp_calls` was `not_collected`
  before B12).

**M17 `tool_execution_success`** (`M17.tools-v1`): succeeded / (succeeded +
failed) over both scopes, unreduced `"n/d"` (`null` `empty_denominator` when
none is terminal), with `by_scope {command_execution, mcp}` each
`{succeeded, failed, unknown {executions, by_reason}}` and the top-level
`unknown` their sum. `command_execution`: status `completed` with
`exit_code` 0 succeeded, non-zero failed; since B12 status `failed` with a
non-zero `exit_code` failed (certified live, run2 §4: `ls` of a missing
path, exit 2; `custom_tool_call.status` still says `completed`, it only says
the call was made). Unknown, excluded and counted in `unknown.by_reason`:
`exit_code_unknown` (`completed` with `NULL`, never a success),
`status_unreported`, `status_not_certified` (`failed` with exit 0 or `NULL`,
or any other status: cancel or timeout are not certified). `mcp` (B12):
`is_error = 1` failed; `is_error = 0` with status `completed` succeeded;
unknown: `is_error_unreported`, `status_unreported`, `status_not_certified`.
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
37.8 s Herdr `blocked` wait). Per host (`host_basis: execution_home`):
`by_home` keys the same distribution by the `home_digest` of the session's
rollouts; a session whose rollouts lie under several homes goes to
`home_ambiguous`. Since B12 `call_to_output_ms.mcp` is the same
distribution over the carrying calls of matched MCP calls (also inside the
overall and `by_name.exec` numbers), `by_server {server: {tool: …}}` and
`server_or_tool_unreported`, labelled as not the MCP call's run time.
`mcp_duration.value` is `unavailable execution_duration_not_exposed`: the
MCP item's `duration` is measured like an exec item's
(`startup_not_run_time`, certified for a local stub only) and is never read.

**`accounting tools [--json]`** (read-only; `collection_not_run` without a
sidecar): per in-scope session `{session_id, attempt_ids, tools}` with
`tools` `{issued, without_output, outputs_without_call, executed,
attributed, unattributed, mcp_calls, succeeded, failed, unknown,
declined_or_aborted}` or `unavailable` (`executed`, `succeeded`, `failed`,
`unknown` over both scopes; `attributed`/`unattributed` command executions);
`coverage`; `metrics` M16–M18, identical to the report's (lane keys, §2).
Text: a coverage line, one line per session and per metric (M16 `accepted N
inferred (M unknown)`), an `M16 mcp_calls N [server/tool n, …] (counted
once with their exec call), declined_or_aborted K` line, an `M17 by scope:`
line, and the `call_to_output_ms` p95 line labelled as including approval
waits.

Not derived (follow-ups): controller intervals
(TM2.5: Codex writes no typed tool or approval interval, and Herdr samples
only agent state); M18 once a Codex version records an execution end −
start.

Test `tool_volume_success_and_latency_are_honest`: one bound session in two
rollouts (the second resumes the first and adds turn 2) and one unbound
session. 5 issued (`exec` 4, `wait` 1; status unreported 1), 1 without
output, 1 output without a call; 6 executions, 5 inferred to `exec` calls,
1 unattributed; exit 0 ×3, exit 2, `NULL` exit, status `failed` exit 1 →
M17 `3/5` (B12: `failed` with a non-zero exit is a failure; was `3/4`),
unknown 1; M18 unavailable, `call_to_output_ms` 37010, 1000, 2500,
300 → p50 1000, p95 37010 (`exec` 3 samples, `wait` 2500; `by_home` the one
home, 4 samples); accepted `0` with 5 unknown (no wait, no guardian); the
report equals `accounting tools`; before any collect everything is
`collection_not_run`. Test `accepted_stage_is_inferred_from_waits_and_guardians`:
the same session (calls 10→47.01, 50→51, 52→54.5, 55→55.3 s, and 60 without
output) with a live-shape guardian starting at 55.1 s → call-4
`auto_review` (1 accepted, 4 unknown); attention samples working 0 s,
blocked 30 s, working 60 s → call-1 `human_routed`: accepted 2, unknown 3. Test `tool_metrics_before_a6_or_reread_are_unavailable`:
the A6 tables dropped (ingest 5) read as `predates_collection` without
migrating; the next collect restores the output byte for byte; the resumed
rollout gone before its re-read makes the session `pending_reread`.
Test `live_run2_failures_aborts_and_mcp_calls_count_once` (B12, §15).

## 10. Fleet efficiency (B6a, TM2.8; M34–M37)

Plan: doc 07 M34–M37, doc 05 §5a, doc 10 §5a "Fan-out" and "Coordinator
overhead", doc 12 TM2.8. **Derived at read time**: nothing is stored and no
sidecar stream migration is needed. Inputs are canonical `state.db` rows
opened read-only (`telemetry::read_only`); for M34 and M37 the latest
valuation revision (§4) and `rollout_sources` of the sidecar, read-only (no
sidecar is created); for M36's worker-observed scope the attempt worktrees'
reflogs (a gated, read-only `git`). The one canonical write is the owner's
supersession reason (migration 0060, below). Only worker attempts are
counted in M35/M36. The coordinator has no canonical attempt, so its time
and cost never enter a worker figure (M34 below).

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

**Windows and buckets.** Activity windows are fixed windows of
`--window-minutes` (default 60, whole UTC hours; any divisor of 1440, so
windows align to UTC days; anything else is refused), recorded as
`window_ms` and `window_minutes` in `fleet` and M35, that hold active time
or an acceptance. The report hook uses the default. A window's time-weighted
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

**Per configuration.** M35 is also computed per agent configuration (the
dispatch decision's `chosen_configuration_id`, contracts §2) that has active
time or an acceptance: the same windows, buckets, reference and
comparability rule over that configuration's attempts only (their active
time; the acceptances whose first evidence is a result of its attempt; the
unknown spans of its attempts or of attempts without a decision).
`fleet.by_configuration {configurations: {id: {display_label, attempts,
windows, buckets, comparability}}, configuration_unknown {attempts,
accepted}}` and `M35.by_configuration {configurations: {id: {display_label,
value, level, reference_level, reference_per_agent_per_hour,
marginal_per_added_agent_per_hour, label}}, configuration_unknown}`;
`display_label` is `<kind> <agent_version>` from `agent_configurations`
(display only, never an identity; `null` without a row). Attempts or
acceptances without a configuration are counted apart, never assigned.
Text: `M35 configuration <id> (<label>) <value> (<label>)`.

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

`scope: integrator_observed`. `--since` keeps attempts whose first
operation is at or after it.

**Worker-observed scope** (`M36.worker_observed`, apart from the integrator's
figure and never added to it): for each attempt reaching integration (at
most 64 per read), the `HEAD` reflog of each of its worktrees
`<project>/.state/worktrees/<attempt>/repo-NN` (at most 16), read by `git
--no-optional-locks reflog show --date=unix --format=%gd%x09%gs -n 1000
HEAD` through the spawn gate, with `GIT_DIR`-style variables removed. Only
the entry's time and its action (the subject up to the first `:`) are
used; the rest of the subject (a commit message) is dropped unread and
nothing is stored. Kinds: `rebase` (`rebase… (start)`, `pull --rebase
(start)`), `rebase_conflict_resolved` (`… (continue)`), `merge` (`merge
<ref>`, a non-rebase `pull`), `merge_conflict_resolved` (`commit
(merge)`). An event counts when recorded no later than the attempt's first
integrated operation (any time if it never integrated). `worker_observed
{scope: worker_observed, numerator, denominator, value, events, coverage,
event_rule}`: attempts with an event / attempts whose worktree was read;
`coverage` counts `observed`, `worktree_absent` (removed after cleanup),
`reflog_unavailable`, `git_unavailable`, `read_limit`. Residual: a worker
that rewrites or expires its reflog, or works outside the worktree, is not
observed.

**Cost basis (M34, M37).** The latest valuation revision (§4,
`published_rate_estimate`, fixture-only rate cards), per collected session
(`rollout_sources`, current binding). A session is:
- *worker*: a rollout bound to a known canonical attempt;
- *unattributed*: in a task worktree (`cwd_attempt`), ambiguous, or bound to
  an unknown attempt, but not bound to a known one;
- *coordinator* (**`coordinator-scope-v1`**): Codex rollouts collected from
  any scanned execution home (contracts §5), bound to no attempt and outside
  every task worktree, whose `session_meta.cwd` (stored home-prefixed) is the
  project directory: `coordinator.rs open` starts the coordinator agent in a
  pane at `project.canonical_dir()`, and nothing else in the product runs an
  agent there. Its guardian sessions (same cwd) are included;
- otherwise *outside* the project (ignored).

A priced amount is summed per currency (exact rationals, shown as exact
decimals); currencies are never added (`mixed_currency`); an amount that would
overflow is `amount_overflow`. What keeps an estimate from being complete
(`gaps`): `entries_unpriced` (an entry `unavailable` in the revision),
`usage_not_valued` (a session with records the revision has not valued:
collected after the last sync or reprice), `usage_not_observed` (an attempt
that ran, a known running interval, without any bound session: another agent
kind, or not collected). Unknown is never 0: a share with gaps is `{status:
partial, reasons, priced_share}`; with nothing priced `unavailable
no_priced_entries`. An estimate is `complete {currency, amount}`, `partial
{gaps, currency, priced_amount}` or `unavailable`. `--since` keeps sessions
whose earliest rollout started at or after it (as M12) and attempts whose
running interval starts at or after it. Without a sidecar both metrics are
`unavailable collection_not_run`, before any reprice `not_priced`.

**M34 `coordinator_overhead`** (`M34.fleet-v1`, `scope:
coordinator-scope-v1`, `allocation_rule: coordinator-allocation-v2`,
`excluded_from: [M35, per_arm_worker_figures]`). `value` = coordinator
exclusive cost / total project lifecycle cost (coordinator + worker
sessions), an exact rational. Reasons for `partial`: `coordinator_<gap>`,
`worker_<gap>`, `unattributed_worker_usage`. With no session in the scope it
is `unavailable coordinator_usage_not_observed`, never 0: a coordinator run
by another agent kind (the default `coordinator_agent` is `claude`), or from
an execution home that is not scanned, is not observed. Also:
- `coordinator {sessions, estimate, coverage}` and
  `total_project_lifecycle_cost {estimate, worker_attempts, worker_coverage,
  unattributed_sessions}`;
- `per_active_worker_thread_hour {value, currency, active_worker_thread_ms,
  open_censored}`: coordinator cost × 3600000 / Σ known active worker time
  (§10 intervals, open ones to now) in the window; `unavailable
  active_time_unknown` when an unknown span touches the window,
  `predates_lifecycle_log` without marks, `null empty_denominator` for none;
  `partial {gaps, priced_value}` when the coordinator cost has gaps;
- `allocation` under **`coordinator-allocation-v2`**: each priced coordinator
  entry is split evenly across the tasks with an attempt running at its
  record time (`[t, t]`, basis `record_time`, §4, against a run's
  `[from, to)`); none running: `unallocated`; an unknown activity span over
  it: `allocation_unknown` (`activity_unknown`); no record time (basis
  `session_start..first_observed`, a revision without a basis, or no usage
  interval): `allocation_unknown` (`usage_time_unknown`). A run or unknown
  span without a terminal mark (`open_censored`, `end_unknown`, or a
  pre-log attempt still open) is **open-ended** here, not cut at the read's
  horizon. `{rule, rule_text, coordinator_total (the unallocated
  coordinator estimate, always shown), unpriced_entries, currency, by_task,
  unallocated, allocation_unknown, allocation_unknown_entries (entry count
  per reason, nonzero only)}`. The allocation is a view only: no worker,
  task or arm figure (M35, candidate-group arm cost, `accounting cost` per
  attempt) includes coordinator cost.

  **Reproducibility (why v2).** The allocation is derived at read time, so
  it must be the same for the same canonical rows and collected sources,
  whenever it is read and after the sidecar is deleted and rebuilt. Rule
  **v1** (`coordinator-allocation-v1`, TM2.8) broke this in two ways. (1) It
  cut open spans at the read's horizon (now). An entry recorded after an
  earlier read's "now" was `unallocated` then and `allocation_unknown` (or
  allocated) later. That is the certification failure (`assert_eq!(rebuilt.5,
  original.5)`): the fixture's coordinator record is at decision + 1 s, and
  a fast run read the original view before that instant. (2) It placed an
  entry by its fallback interval, whose end is the record's first
  observation. A rebuild re-observes every record, so the entry could move
  in or out of a running interval. v2 reads neither: only line times and
  lifecycle marks. v1 was never stored (the view is derived at read time),
  so only the label changes. M34's `value`, `coordinator`,
  `total_project_lifecycle_cost` and definition `M34.fleet-v1` are
  unchanged. `per_active_worker_thread_hour` still counts open runs up to
  now (by definition active time so far), so it grows with the read time
  while an attempt is open.

**M37 `overlap_waste_share`** (`M37.fleet-v1`). The producer is the owner's
**accepted supersession reason**, canonical migration **0060**
(`attempt_supersessions`, store schema 60): one append-only row per ended
attempt (`completed`, `failed`, `cancelled`, `lost`; a trigger refuses any
other, and UPDATE/DELETE), `{attempt_id, task_id, outcome (superseded |
abandoned), reason (sibling_changed_same_area | duplicate_effort | other),
sibling_attempt_id (required unless other; another attempt), evidence (1–16
typed references <kind>:<id>, kind attempt | task | submission |
verified_result | integration_operation | commit | candidate_group; never
text), principal operator:cli, authority operator_owner.v1, canonical_json,
recorded_unix_ms}`. Written only by `SqliteStore::record_attempt_supersession`
in its own transaction: a worker (`worker:*` or an attempt id) and an import
(`import:*`) are refused, as is any principal but `operator:cli`, and the
schema CHECKs refuse a forged raw row. The same assertion again is a no-op
(`recorded: false`); a different one for the same attempt is refused
(append-only; corrections are a follow-up). CLI: `telemetry <slug>
accounting supersede <attempt> --outcome <o> --reason <r> [--sibling <a>]
--evidence <kind:id>...` prints `{supersession}`; it refuses to run in a
worker execution context (the contracts-review.md §9 markers: cwd in a task
worktree, or HOME a recorded worker execution home).

`value` = lifecycle cost of attempts whose reason is
`sibling_changed_same_area` / total worker lifecycle cost. The population is
every attempt with a worker session in the window, plus every attempt that
ran in it (without usage: `usage_not_observed`). Each attempt is in one
bucket: its recorded reason (`sibling_changed_same_area`,
`duplicate_effort`, `other`), else `unexplained_abandonment` when it is
`cancelled` or `lost`, else `not_superseded`. `buckets {name: {attempts,
estimate}}`, `records` (count per reason), `total_lifecycle_cost {attempts,
estimate, coverage, unattributed_sessions}`. Only
`sibling_changed_same_area` is overlap waste; unexplained abandonment is
never counted as waste. Before 0060: `unavailable
supersession_reason_not_recorded`, `missing: [accepted_supersession_reason]`.

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
- `--window-minutes 120`: hour 0 (23:00 UTC) alone in 22:00–24:00 → level
  2, `1`/hour; hour 1 in 00:00–02:00 → level 4, `3/2`/hour; M35 `3/4`,
  marginal `1/4`. 7, 0 and 2880 are refused.
- Two configurations X (a1, a2, b1–b4, `codex 0.154.0`) and Y (the rest,
  `claude 2.1.0`): X level 2 (2 accepted, reference `1`/agent) and level 4
  (3 accepted) → `3/4`, marginal `1/2`; Y `no_accepted_throughput`; the
  fleet stays `3/4`.

Test `fan_out_buckets_and_integration_conflicts` also plants attempt
worktrees with real Git history: a2 rebases (0:10) and resolves a
conflicting merge (0:20) before integrating at 0:45 (its merge at 0:50 is
not counted), b2 merges its target at 1:10, b3 only commits, b4 has no
worktree → `worker_observed` `2/3`, events `rebase 1, merge 1,
merge_conflict_resolved 1`, coverage `observed 3, worktree_absent 1`; the
integrator figure stays `3/4`. Without a sidecar M34 and M37 are
`collection_not_run`.

Test `coordinator_overhead_and_overlap_waste_from_accepted_reasons`
(invented rates: $4 per 1,000 + 500 record):
- No coordinator session: M34 `coordinator_usage_not_observed`.
- Doc 10 §5a: coordinator $4 (Codex at the project directory from another
  scanned home), four worker sessions $16 → M34 `1/5`; the attempt ran 2 h →
  `2` per worker-thread-hour; rule v1 gives task `work` `4`, unallocated `0`;
  the attempt's `cost` estimate stays `16`. M37 `0` (not superseded).
- The attempt is cancelled and the task re-run ($4): M34 `1/6`; M37 `0` with
  the cancelled attempt in `unexplained_abandonment` ($16).
- Workers, an attempt id and an import are refused by the store, a forged
  row by the schema, a worker HOME by the CLI, a missing sibling and a text
  evidence by validation; the owner's reason (`sibling_changed_same_area`,
  sibling `s1`) → M37 `4/5` ($16 / $20); the same again is a no-op, another
  refused, an UPDATE refused.
- A coordinator record without a rate: M34 `partial` (`priced_share` `1/6`,
  `coordinator_entries_unpriced`), per-hour `partial` `2`.
- Doc 05 §5a: a second task running over the coordinator's record → `2` to
  each task, total `4` still shown, `1` per worker-thread-hour; its usage is
  not observed, so M34 adds `worker_usage_not_observed` and M37 is `partial`
  `4/5` (`usage_not_observed`).

Follow-ups: the factory's scale-trial steps (F4.6/F5.4) as named concurrency
steps; a declared coordinator execution home (today a Codex coordinator is
observed only from a scanned home) and a Claude coordinator adapter;
corrections (retractions) of a supersession reason; allocation by coordinator
turns that reference a task (a rule v2 needs task references in turn metadata).

## 11. Stream 8: superseded projections dropped

Accounting stream 8 (`0008_drop_superseded.sql`) drops `session_graph` (v2),
`quota_observations` (v4) and `session_nodes` (v6). Later versions replaced
them (`session_graph_nodes`, `quota_window_observations`) and nothing reads or
writes them. They were projections rebuilt on every sync, so no source data
is lost. `DROP TABLE IF EXISTS` keeps the migration re-runnable when the
streams table is lost and every migration runs again. Checked end to end in
`attention_intervals_union_and_censor`.

## 12. Delta revisions, ticker reprice and M12/M14 (B10)

Stream `accounting` version 9 (`0009_valuation_deltas.sql`). Plan: doc 05
§5 (valuations append-only, `as_of` reproduces earlier totals), doc 07 M12
and M14.

**Delta revisions.** `accounting reprice` appends a revision only when the
result changed (§4, unchanged). From version 9 the appended revision is
stored as a delta against the previous revision as it reads back:
`valuation_delta_revisions(revision, changed, removed)` and
`valuation_deltas` (the `valuations` columns plus `usage_basis` and
`provider_check`, one row per entry added or different, and a `removed = 1`
tombstone per entry that left the ledger). Reprice prints
`stored {changed, removed}`. Revisions written before version 9 stay full
copies in `valuations` + `valuation_bases`, never moved or dropped. Revision
N reads as the latest full copy at or below N, then every delta above it up
to N, in order; `cost --revision N` is byte-identical to the full-copy
reading. A sidecar read before its upgrade (no delta tables) reads its full
copies as before. All tables are append-only (triggers) and the migration is
re-runnable (`IF NOT EXISTS`, no `ALTER`, nothing dropped).

**Ticker reprice.** The lane `tick` (after attention observation and the
ledger sync, §1, §6) reprices only when a rate card has been imported and
the input fingerprint changed: the digest of the policy, every card
`(card_id, version, digest)` and every ledger row a reprice reads
(`valuation_inputs`, replaced by every reprice, CLI or tick). Inputs whose
serialization exceeds the tick byte budget (8 MiB) are skipped
(`budget_exhausted`) and left to `accounting reprice`. With no rate card it
appends nothing, so M12 stays `not_priced` rather than a revision of
`no_rate_card` rows.

**M12 `repriced_estimated_spend`** (`M12.cost-v1`) and **M14
`cost_coverage`** (`M14.cost-v1`), through the lane `metrics()` hook, from
the latest revision: every valued delta entry (all sessions; with `--since`,
sessions whose earliest rollout started in the window). Both carry `basis:
published_rate_estimate`, `revision`, `policy`, `ledger_synced_unix_ms`,
`rate_cards: fixture_only` and the caveat: rate cards are fixture-only by
owner decision (§4), so an estimate is only as real as the cards imported,
never a provider charge and never added to one (`never_added_to: M11`).
- M12 follows `accounting cost` exactly: `value` is the exact decimal
  `amount` (with `currency`) only when every entry is priced in one
  currency; otherwise the `partial {currency, priced_amount}` (never the
  total) or `unavailable` (`mixed_currency {priced_by_currency}`,
  `no_priced_entries`) estimate object; `estimate` and `coverage` as in
  `cost`.
- M14: priced / valued entries, unreduced `"n/d"` (`null
  empty_denominator` for none), `unpriced` by reason, `unit: entries`. It is
  count coverage, not a share of money: an unpriced entry may be the
  expensive one.
- Before any reprice both are `unavailable not_priced`; without a sidecar
  `collection_not_run`.

Test `ticker_reprices_only_when_inputs_change` (the real `ticker run`): with
no card the pass syncs and appends nothing (`not_priced`); card version 1
→ revision 1, M12 `0.004` USD complete, M14 `1/1`; a pass with nothing
changed syncs again but records no reprice; version 2 → revision 2 `0.006`,
one changed row stored. M12/M14 partial and mixed-currency cases and the
full-copy read-back are in `repricing_uses_rate_effective_at_usage_time`.

## 13. Provider charges, invoice allocation, dated conversion and `as_of` (B11, TM2.3 remainder)

Stream `accounting` version 10 (`0010_charges_fx.sql`). Plan: doc 05 §5
(cost bases, corrections, `as_of`), doc 07 M11, doc 08 §3 (as-of), doc 10
§3. Everything imported here is **fixture-only**, like the rate cards (§4):
an import file must say `synthetic: true` and name its `source`, or it is
refused; the only files in the repo are invented test values
(`tests/fixtures/telemetry/accounting/charges-*`, `fx-*`). Outputs carry
`fixture_only`.

**Charges and invoices** (`provider_charges`, `provider_invoices`) are
appended by `accounting import-charges <file>` (TOML by extension, else
JSON): `provider`, `product`, `source`, `synthetic`, then `charges [{charge_id,
revision, currency, amount, response_id?, session_id?, charged_unix_ms?}]`
and `invoices [{invoice_id, revision, kind: usage|subscription, currency,
amount, period_from_unix_ms, period_to_unix_ms}]` (half-open period).
Amounts are decimal strings (≤ 18 digits, ≤ 12 places, canonicalized; a
number is refused). Both are append-only (triggers): the same revision with
the same digest is a no-op, with another digest refused, and a new revision
must be the next one. A correction is the next revision: `history` lists
every revision with its signed `adjustment` (doc 05 §7: 0.0100 → 0.0080 is
`-0.002`; the current amount is 0.008).

**`accounting charges [--as-of MS]`** (read-only, JSON): basis
`provider_billed`. Each charge's latest revision imported by `as_of` (all
without), reconciled against the valuation revision of that instant
(§12; the latest computed by `as_of`):
- Match: by the native `response_id` against the ledger's delta entries
  (plus `session_id` when given): one entry `matched_by: response_id`; several
  `ambiguous_match`; none `no_matching_usage`. A charge naming only a
  `session_id` matches every entry of that session (`matched_by:
  session_id`). Neither: `no_request_identity`. Two charges claiming one
  entry are both `overlapping_charges` (never charged twice). Before any
  reprice every charge is `not_priced`. Unmatched charges are listed with
  their reason and counted in `reconciliation.unmatched`.
- Matched: the `estimate` of the matched entries (as `cost`: complete,
  partial or unavailable) and `difference` = charge − estimate (signed,
  exact) only when the estimate is complete in the charge's currency; else
  `unavailable` `estimate_partial`, `estimate_unavailable` or
  `currency_differs` (no implicit conversion). A non-zero difference lists
  the evidence that can explain it (`explanations`): what the rate cards
  used exclude (`estimate_excludes_{discounts,taxes,fees}`) and
  `rate_cards_fixture_only`. They are candidates, not proof.
- `reconciliation.uncharged_estimates`: the estimate of the entries no charge
  matched, apart. `provider_billed {currency: sum}`: the current amounts of
  every charge, matched or not. An estimate is never added to a charge.
- `invoices`: each invoice's latest revision with its history.

**Invoice allocation** (`accounting allocate <invoice> [--rule R] [--as-of
MS]`, read-only, JSON, basis `invoice_allocation`): derived at read time from
the invoice revision and the valuation revision as of that instant, both
append-only, so `--as-of` reproduces an earlier allocation. Rules are named
and versioned; the one rule is `by_total_tokens.v1`: share = an attempt's
Codex `total_tokens` (new + cached input + output, from the quantities the
revision copied) of counted entries whose usage interval lies inside the
period / all such tokens. Amounts are computed in units of 10^-12 of the
invoice currency. Floors are taken first, then the remaining units go one each
to the largest remainders (ties by key), so the allocations sum to the
invoice exactly. Tokens of unbound sessions go to `unattributed` (a share, not
an attempt). Entries outside the period are counted in
`coverage.outside_period`. An entry in the period whose usage is not counted
(`usage_not_counted`), without a usage time (`usage_time_unknown`) or
straddling a period boundary (`usage_interval_straddles_period`) makes the
allocation `partial` (`usage_unknown_in_period`): the known shares are
labelled `bound: upper`, and the unknown usage is listed per attempt as
`unavailable`, never 0. With no known tokens the allocation is `unavailable`
(`no_usage_in_period` / `usage_unknown_in_period`). A subscription can be
allocated the same way. The result is never added to estimates or charges.

**Dated conversion** (`fx_tables`, `fx_rates`): `accounting import-fx <file>`
appends an exchange-rate table version (`table_id`, `version`, `source`,
`synthetic`, `rates [{from, to, rate, effective_from_unix_ms,
effective_to_unix_ms?}]`, one unit of `from` in `to`, half-open, no overlaps
per pair within a version; append-only like rate cards).
`accounting fx --to CUR [--revision N | --as-of MS]` (read-only, JSON)
converts the stored estimates of that valuation revision, using tables
imported by `as_of`. It is a separate dated valuation (`valuation:
dated_fx_conversion`), and the stored estimates keep their own currency. Per
priced entry: the same currency is kept (`same_currency`). Otherwise the rate
of `from → to` is the one effective over the entry's whole usage interval
(`dated_by: usage_interval`): the highest version among the overlapping
rates of one table. The result is `amount × rate`, exact, with `rate_id`
(`<table>@<version>:<from>-><to>@<effective_from>`), the table, version and
interval. No inverse or cross rate is inferred. The unavailable reasons are
`no_fx_rate`, `ambiguous_fx_tables`, `fx_rate_change_within_usage_interval`
and `usage_time_unknown`. Unpriced entries keep their reason. Per attempt the
converted `estimate` is complete only when every entry is priced and
converted, else partial or unavailable, as in `cost`.

**Time-based `as_of`.** Valuation revisions carry `computed_unix_ms` (§4).
`accounting cost --as-of MS` shows the latest revision computed at or before
`MS`, byte-identical to `--revision N` of that revision (the two flags
conflict). Before the first revision it shows `unavailable not_priced_as_of`
(`not_priced` when nothing was ever priced). `charges`, `allocate`, `fx` and
`budget-shadow` (§14) take `--as-of` the same way for the valuation revision
and, for charges, invoices and exchange-rate tables, their
`imported_unix_ms`. `as_of` fixes what the sidecar knew, not what producers
had observed (doc 08 §3).

**M11 `reported_spend_subtotal`** (`M11.charges-v1`, report hook): Σ of the
latest revision of every provider charge per currency (corrections as
adjustments), basis `provider_billed`, `charges: fixture_only`,
`never_added_to: M12`, `invoices_separate` (count). One currency gives
`value` + `currency`; several give `unavailable mixed_currency
{by_currency}`; none `no_provider_charges`. With `--since`, charges with a
`charged_unix_ms` before it are excluded, and any charge without one makes M11
`unavailable charge_time_unknown`.

Test `provider_charges_reconcile_allocate_and_convert`: card v2 (input 2,
cache read 0.5, output 8 per 10^6) and a EUR card. Charges:
- ch-1 USD 0.0100 on `resp-c1` (1,000 in + 500 out → estimate 0.006) differs
  by 0.004, with the cards' exclusions as explanations.
- ch-2 0.0041 equals its estimate.
- ch-3 USD against a EUR estimate is `currency_differs`.
- ch-4 (unknown response) and ch-5 (no identity) are unmatched.
- Uncharged estimates are partial USD 0.0016. `provider_billed` and M11 are
  0.5164.

The correction to 0.0080 appends `-0.002` (0.5144), and `--as-of` the first
import shows 0.01. Invoice inv-1 of 12 over 2,980 bound and 500 unbound tokens
is allocated `10.275862068966` and `1.724137931034` (sum exactly 12).
Subscription sub-1 of 7, whose period also holds an uncounted record, is
partial: `5.994252873563` (upper bound), `1.005747126437` unattributed, and
the unknown entry `usage_not_counted`. EUR 0.00021 × 1.1 = USD 0.000231 by
table version 1: version 2's 1.2 starts after the record. The attempt is
partial USD 0.010331, while `cost` stays `mixed_currency`. Before the tables
were imported: `no_fx_rate`. After a version-3 card appends revision 2, `cost
--as-of` each revision's instant is byte-identical to that revision. A time
before revision 1 is `not_priced_as_of`. `charges --as-of` before revision 2
reconciles against revision 1.

## 14. Budget bridge in shadow mode (B11, TM2.4 shadow)

Plan: doc 05 §6 and §7 (budget goldens), doc 12 TM2.4 ("deliver shadow
evidence first"). Owner decision: **shadow only**. Nothing here is enforced.
It writes no canonical row, and admission, scheduling and the budget store
never read it. No stream migration.

**`accounting budget-shadow [--policy FILE] [--as-of MS]`** (read-only,
JSON) opens `state.db` only through `telemetry::read_only` (§0 Reads) and
reports `mode: shadow`, `enforcement: none`, `canonical_writes: none`. It
reads:
- Canonical: the budget policy history (validated as the store does:
  consecutive revisions, `sha256(payload) = payload_hash`; else `policy:
  unavailable policy_record_invalid`); attempts with state, task, agent kind,
  the budget revision their launch inputs pinned (`pinned_policy_revision`)
  and whether the lifecycle log shows they never ran.
- Consumption: the valuation revision (latest, or by `as_of`, §13), per
  attempt over all its sessions: provider tokens = Σ Codex `total_tokens` of
  counted entries; money = Σ priced amounts in the policy currency. This is
  `published_rate_estimate` from fixture-only cards, never canonically
  accepted usage (`consumption_basis`). Unknown parts are counted by reason:
  `usage_not_counted`, unpriced reasons, `currency_differs`, `not_priced`
  (no revision), and for a terminal attempt with no entries,
  `no_usage_observed` (or `adapter_absent`; an attempt the log shows never
  ran is known zero).

The decision is admission's question (doc 05 §6): accepted consumption plus
remaining reserved exposure plus the new request, against the limit, per
unit. An open attempt's in-flight exposure is its reservation estimate minus
its accepted consumption. Without an estimate it is `in_flight_usage_unknown`.
Consumption above the estimate is `reservation_overrun`; it is unknown and
never 0. A new request without a known size is `new_request_usage_unknown`.
Then:
- `would_block` `limit_exceeded`: exposure > limit.
- `would_block` `projected_exposure_exceeds_limit`: exposure + a known request
  > limit.
- `would_block` `no_headroom`: exposure ≥ limit with a request of unknown
  size.
- Any unknown part follows `UnknownUsagePolicy`: `refuse` gives
  `would_block provider_usage_unavailable`; `allow_incomplete` gives
  `would_warn usage_incomplete`.
- Otherwise `allow within_limit`.

Known exposure over the limit blocks whatever is unknown. Each evaluation
carries `limit, accepted, remaining_reserved, exposure, new_request,
projected, unknown [{attempt_id, reason, entries}], unknown_usage, decision,
reason, attempts [{attempt_id, task_id, state, accepted, in_flight}]` (tokens
as integers, money as exact decimal strings with `currency`).

- **Canonical policy** (the latest revision, `policy_source: canonical`):
  `decision_today` repeats what admission decides now (`blockers`,
  `incomplete`, `provider_tokens: unknown`). The shadow evaluates `attempts`
  (count ≥ cap blocks, as admission) and `provider_tokens` (`max_provider_tokens`
  with the policy's `unknown_usage`; the request is unknown unless the what-if
  file sizes it). `differs_from_canonical` compares the two (`null` without a
  policy). Canonical budgets are project-wide:
  `task_budgets: unavailable no_canonical_task_budget`.
- **What-if policy** (`--policy`, `policy_source: what_if`): a synthetic file
  (`synthetic`, `policy_id`, `version`, `source`, `unknown_usage?` (default:
  the canonical policy's, else `refuse`), `currency?`, `project {max_amount?,
  max_provider_tokens?}`, `tasks {<task>: {...}}`, `reservations {<attempt>:
  {amount?, tokens?}}`, `request {task?, amount?, tokens?}`). It is never
  installed and grants nothing. It carries the monetary limits, task limits
  and reservation sizes that canonical policy does not have. A task scope
  covers every attempt of the task, cancelled ones included; the request
  counts in the project scope and in its task's scope.
- `decision {would_block, would_warn, reasons}` over all evaluations;
  `provenance {state_db: read_only, valuation_revision,
  valuation_computed_unix_ms, ledger_synced_unix_ms, sidecar, what_if_policy
  {policy_id, version, digest, synthetic, source}}`.

**M04 `cost_per_accepted_task`** (`M04.cost-v1`, report hook): contracts §6
`T`/`A` (the central `task_evidence`, same window rule). The numerator is the
estimate over every valued entry of every attempt of the tasks in `T`, which
includes failed and cancelled attempts and child sessions. The denominator is
`count(A)`. `value` is `"<amount>/<count(A)>"` with `currency` only when every
such attempt has usage and all its entries are priced in one currency.
Otherwise it is `unavailable lifecycle_cost_incomplete`, with the numerator
estimate (partial or unavailable) and `coverage.attempts_without_usage`.
`count(A) = 0` gives `null empty_denominator`. Open tasks are excluded and
counted (`tasks.open_excluded`). Basis `published_rate_estimate`,
`rate_cards: fixture_only`, `never_added_to: M11`.

Test `shadow_budget_bridge_matches_doc05_goldens` uses a per-token synthetic
card (input 0.05, output 0.02). The cancelled attempt a1 used 1,000 in and 500
out, so $60 is accepted. a2 is reserved at $30 (what-if); the budget is $100
and the request $15:
- Accepted 60, remaining 30, exposure 90, projected 105: `would_block
  projected_exposure_exceeds_limit`.
- After $10 of covered usage on a2: accepted 70, remaining 20, exposure 90,
  still 105. `--as-of` revision 1 reproduces 60. A $10 request gives
  projected 100: `allow`.
- Without a reservation: `refuse` gives `would_block
  provider_usage_unavailable`, and `allow_incomplete` gives `would_warn`.
- A task limit of 80 is `limit_exceeded` (90).
- Canonical policy (planted, `allow_incomplete`): with 5,000 tokens, 1,850
  known and a2 in flight give `would_warn`, the same as today. With 1,000
  tokens it gives `would_block limit_exceeded` while admission allows:
  `differs_from_canonical: true`.
- `state.db` and the files beside it are unchanged by every shadow read.
- M04: `null empty_denominator` while the task is open, then `70/1` USD once
  task `work` has a verify-only verified result (both attempts included).

Follow-ups: enforcement (TM2.4 proper) needs the canonical bridge command,
TM2.6 reconciliation and an approved unknown-usage policy (doc 05 §6);
per-task window consumption (quota) still needs a certified invocation
scope; M34's coordinator allocation could reuse the §13 rule machinery once a
coordinator usage scope exists.

## 15. Live run 2 (B12): failures, aborted turns, MCP calls, forks, quota jitter

Source: [codex-live-0.154.0-run2.md](codex-live-0.154.0-run2.md), the
phase2-lanes card B12 and contracts-collection.md A8 ("Follow-ups for lane
B"). Lane A's A8 tables (ingest 0008) are read by SQL only, at read time,
joined on the rollout's own `session_id`; only ids, tags, flags and line
times are read (never an MCP call's `arguments` or `result`, which are not
stored). **No stream migration**: accounting stays at stream version 10;
nothing new is stored, and `accounting sync` output is unchanged.

- **M17** (§9): `codex_exec_items.status = 'failed'` with a non-zero
  `exit_code` is a certified failure; other statuses stay excluded with
  their counts. MCP calls (`codex_mcp_calls`): `is_error = 1` failed,
  `is_error = 0` with status `completed` succeeded; labelled by scope
  (`command_execution`, `mcp`). Live run 2's session A: `2/3`.
- **M16** (§9): calls ended by `turn_aborted` (`codex_turn_aborts`) are
  `declined_or_aborted` with their basis, never accepted or unknown. MCP
  calls are counted by server and tool once, matched to their carrying
  `exec` call only by turn and line time (`inferred`); `mcp_calls` is
  `live`. `collaboration` calls and spawned agent threads are counted apart,
  never as executions.
- **M18** (§9): still `unavailable`; the MCP item's duration is not run
  time. The carrying call's call → output time is shown apart under
  `call_to_output_ms.mcp`.
- **Session graph** (§3): spawned-subagent (`parent_thread_id`) and fork
  (`forked_from_id`) links are certified `live`. A live-shape fork (A8
  `rollout_forks.base_thread_id`) is `separate` with its reconciliation
  states; a fork's reported totals are never added.
- **Quota** (§5): `resets_at` within 60 s of the window's, before its reset
  elapsed, is the same window (live: +5 s); `plan_type: null` is not another
  window.
- **Pre-A8 sidecars**: without the A8 tables every A8-dependent value is
  `unavailable predates_collection`: M16–M18 (§9) and a fork's inclusion
  (§3). A source not yet re-read (no `rollout_forks` row) makes them
  `pending_reread`. They are never partial and never 0, and the next collect
  restores the output byte for byte.

Tests (tests/telemetry_accounting.rs, the live-2 conformance fixtures):

| Property | Test |
|---|---|
| `live2-tools`: 6 issued (MCP once), accepted 0 / unknown 5 / `declined_or_aborted` 1 (`last_output_before_abort`); executed 3 (2 + 1 MCP); M17 `2/3` (command 1/2, MCP 1/1); call → output 65, 86, 131, 1078, 3060, 8743 ms → p50 131, p95 8743, MCP carrier 65; a `blocked` wait in s1 → `human_routed`, one in the aborted d1 stays `declined_or_aborted`; A8 tables dropped → `predates_collection` (no migration on read), a missing `rollout_forks` row → `pending_reread`, each restored by the next collect | `live_run2_failures_aborts_and_mcp_calls_count_once` |
| `live2-fork` of `complete` (1680): `parent_not_collected` (320 unlinked) before the origin, then `linked_child` (`forked_from_id`, `live`), inclusion `separate`, reconciliation `reconciled` ×2, rollup 1680 / 320 / 0 (never 1680 + 2000), M08/M09 1800/200; without A8 `predates_collection`, then `pending_reread`, children and linked rollup never summed | `live_fork_is_separate_and_never_adds_its_reported_totals` |
| Quota jitter ±5 s and `null` plans: one window; a shared-window candidate across homes within the tolerance; +120 s `reset_moved`, back to R `window_regressed` | `resets_jitter_and_null_plan_stay_one_window` |
| Changed on purpose: spawned and fork links `live` (`child_sessions_link_to_parent_without_double_count`); M17 `3/5` with the `failed` exit-1 item a failure (`tool_volume_success_and_latency_are_honest`); `declined_or_aborted: 0` in every M16 value; the conformance M17 `2/3`, executed 3 (`live_run2_shapes_are_collected_without_content`) | as named |

## 16. Stream 11: quota window lookups (TM5.1)

Stream `accounting` version 11 (`0011_quota_window_lookup.sql`) adds two
indexes and changes no row or answer. M40 reads, for each dispatch decision,
each window kind's latest trusted observation and the other homes that
reported the same window. A kind with no trusted observation (Codex reports
no secondary window) and the shared-window lookup each scanned the account's
observations, or all of them, once per decision. `quota_window_observations_trusted`
(account, limit, kind, trust, observed) and `quota_window_observations_window`
(limit, kind, length, reset) serve them; the reset tolerance is read as a
`BETWEEN` range, and the account's limit ids are walked through the index
instead of a `DISTINCT` over every observation. The sync's session graph now
groups entries by session once, and the sync reuses its insert statements.
`ledger::open_dispositions` counts the synced ledger's `conflict` and
`unresolved` dispositions in SQL; TM4.5's `accounting_conflict` rule reads it
instead of the whole ledger as JSON (same counts). Measurements:
[certificate-scale.md](certificate-scale.md) §5.


## 17. DG2: M10 cache-read share and maintained cache totals

Accounting stream **16**, `0016_cache_read_share.sql`; registry
**analytics-registry.v4**, definition **M10.v1**. Plan doc 07 M10 is
cache-read input tokens / eligible total input tokens. The exact unreduced
ratio `"numerator/denominator"` has integer token `numerator` and `denominator`,
with `cache_write_tokens` shown separately. It makes no monetary savings claim.
Codex input already includes cached reads; Claude and OpenCode normalize
new input + cache reads + cache writes into inclusive input. Only reads
enter the numerator. Provider-native counters and existing M08/M09 answers
are unchanged. Non-Codex adapters retain their fixture-only certification.

`accounting_cache_totals` maintains inclusive input, cache reads, cache writes
and counted record count per session. Sync replaces only affected sessions
in the ledger/frontier transaction; migration or missing cache rows forces
a full replay. Retention purges these rows with their sessions; backup copies
them and restore invalidates the frontier through the existing mechanism.
`accounting_cache_frontier` pins mutation generations of `rollout_sources`,
`codex_usage` and `codex_quarantine` in that same commit. Reads validate those
actual inputs in one pinned snapshot, with no usage-history scan. Unrelated
attention/tool changes and analytics-only schema upgrades preserve the answer;
retention and restore invalidations still require sync. Missing/stale
projections report `unavailable accounting_sync_required`;
`accounting sync` restores them. Rebuild verification independently derives
accepted entries and source certification from the original native rows.

Eligibility is M08's bound known-attempt, accepted-version, non-quarantined,
non-rejected session rule and session-start window. Repeated records,
resumed rollouts and cumulative totals add no usage. `coverage` reports
`certified_sessions`, `accepted_records` and excluded sessions by reason.
Missing native cache counters fail native acceptance (`records_not_accepted`),
and sources without accepted cache usage are `cache_counters_not_reported`;
neither contributes a fabricated zero. Gemini native message updates and SDK
observations are not reconciled additive ledger deltas on main: native
sources are explicitly excluded as `cache_denominator_not_reconciled`.
No eligible session is `unavailable no_eligible_cache_usage`; reported zero
input is `unavailable empty_denominator`. A reported zero cache counter with
positive input is a known `0/input` ratio.

`by_configuration` sums the same eligible sessions under each attempt's
frozen dispatch `chosen_configuration_id`, with unknown configuration apart.
`compare --metric M10 --by configuration [--from MS] --json` exposes these
arms under `analytics-cache-comparison.v1`, cohort `activity_window`, estimator
`ratio_of_token_sums.v1`. It supports the lane's session-start lower bound;
upper windows, task classes, horizons and bootstrap seeds are refused.
Compare M10 separately from terminal/assignment outcomes. These descriptive
observational token ratios have no higher-is-better direction, universal
ranking or implied monetary savings; terminal-task success-rate pooling and
minimum-sample rankings do not apply. Existing M02/M07 comparisons retain
all estimators and exact values. Text compare prints every arm's exact ratio
and separate writes, including unavailable arms.

CLI E2E `cache_read_share_mixed_adapters_and_configuration_comparison` collects
isolated Codex, Claude and OpenCode fixtures: **200/1000 + 240/382 + 30/140
= 470/1522**, writes **42**, four accepted records, three eligible sessions.
A missing-counter Claude source and unreconciled Gemini source are excluded;
per-arm ratios remain 200/1000, 240/382 and 30/140. The public refresh,
verify and rebuild retain identical snapshot bytes.
`cache_share_zero_unknown_and_late_restatement` proves known zero, empty
input, stale-frontier unavailability, late **80/1100** restatement and pinned
as-of value/body/digest/watermarks. The unchanged scale gate remains the
correctness oracle. 100k measurements: certificate-scale §4.15, pending the
steward's serial 1M certification.
