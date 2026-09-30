# Health contracts: alerts, notices and advisory recommendations (TM4.5)

Plan card TM4.5 (doc 12), doc 07 §5b/§6 (M50, configuration staleness), doc 08
§6, doc 15 §7 (inbox alerts). Common rules are [contracts.md](contracts.md) §0.
Code: `src/telemetry/health/` (`rules.rs`, `store.rs`, `notify.rs`,
`recommend.rs`, `mod.rs`); sidecar stream `health`, migrations under
`migrations/telemetry/health/`. Tests: `tests/telemetry_health.rs`.

Health states and alerts **report**; they never enforce. Nothing here grants
launch, changes a budget, a worker profile or model access, accepts a result
or changes an acceptance gate. Rules read only through the TM4.1 query
service ([contracts-analytics.md](contracts-analytics.md) §2) and lane read
paths. `health evaluate` and the ticker write only the sidecar `health`
stream; the one `state.db` write of the lane is `health notify` (inbox
notices, §6), on explicit operator command.

## 1. Commands

```
telemetry <slug> health [--json]                 live states of every rule + recorded open alerts (read-only)
telemetry <slug> health evaluate [--json]        evaluate and record states/alerts (writes the `health` stream)
telemetry <slug> health alerts [--since MS] [--json]   open alerts; with --since also alerts opened/resolved since (read-only)
telemetry <slug> health notify [--external]      open alerts → inbox notices once; --external → configured destination
telemetry <slug> health rules                    the declared rule table (read-only)
telemetry <slug> health status                   stream version (read-only)
telemetry <slug> recommend --role CLASS [--metric M02|M07] [--from MS] [--to MS] [--json]   (read-only, §5)
```

`health --json` (`telemetry-health.v1`): `{schema_version, contract,
rules_version, project, evaluated_unix_ms, mode: live_read_only, summary {ok,
warn, critical, unknown}, states[], alerts {open[], last_evaluated_unix_ms}
| unavailable collection_not_run, advisory}`. Each state:

| field | meaning |
|---|---|
| `rule`, `labels` | bounded labels `{project, family, rule, service?, role?}` — never a task, attempt, session, account or configuration identity |
| `state` | `ok`, `warn`, `critical`, `unknown` |
| `reasons[]` | `{code, ...}` operator-visible reasons (e.g. `collector_stale` with `age_ms`, `coverage_loss`, `window_exhausted`) |
| `metric` | the version read: `{metric_id, definition, registry, certification, read: analytics-query.v1}` for query metrics; `{read: "lane:accounting quota", registry}` for lane read paths; M50 + comparison versions for recommendations |
| `evidence_window` | the query window (`from_unix_ms`, `to_unix_ms`, `semantics`, `cohort`, `time_basis`), or the rule's own (observation lag, quota window, open wait, current/prior weeks) |
| `evidence` | the numbers the state rests on (numerator/denominator, age, remaining, longest wait, counts by reason); unavailable values stay `{status: unavailable, reason}` |
| `thresholds` | `{direction, warn, critical, unit, window_ms, cooldown_ms}` from the rule table |

## 2. Rule table (`health-rules.v1`, `rules.rs` `RULES`)

A change of a rule, threshold or read path is a new rules version.

| rule | family / service | read path | warn | critical | cooldown |
|---|---|---|---|---|---|
| `collector_stale` | collection / codex | query `source_watermarks.sidecar.last_collect_unix_ms` | age ≥ 15 min | ≥ 60 min | 1 h |
| `usage_coverage` | consumption / codex | query M13 | < 100 % | < 50 % | 1 h |
| `cost_coverage` | cost / codex | query M14 | < 100 % | < 50 % | 1 h |
| `accounting_conflict` | consumption / codex | `accounting entries` (ledger dispositions) | ≥ 1 `unresolved` | ≥ 1 `conflict` | 1 h |
| `budget_exposure` | cost | `accounting budget-shadow` (canonical policy) | `would_warn` | `would_block` | 1 h |
| `fix_reopened` | review_quality | query M27, integrations of the last 30 days | ≥ 1 reopened | ≥ 3 | 6 h |
| `integration_reverted` | proxy | query M48, last 30 days | ≥ 1 reverted | ≥ 3 | 6 h |
| `latency_shift` | lifecycle | query M06, last 7 days vs the 7 before (≥ 5 samples each) | ≥ 2× prior | ≥ 4× | 6 h |
| `attempt_cost_shift` | lifecycle | query M07, same windows (≥ 5 accepted each) | ≥ 1.5× prior | ≥ 2× | 6 h |
| `service_throttled` | services / codex | query M38 | ≥ 0.1 % | ≥ 10 % | 1 h |
| `quota_headroom` | services / codex | `accounting quota` (current trusted windows) | < 20 % remaining | < 5 % (0 = `window_exhausted`) | 1 h |
| `waiting_on_you` | attention | `accounting attention` (open waits of open attempts) | ≥ 5 min | ≥ 30 min | 15 min |
| `recommendation_stale` | recommendation, per role | TM4.4 `compare` M02 + M50 (§5) | M50 < 1/2 | — | 6 h |

Ratios compare exactly (`n·1000 < t·d`); quota percentages are exact
decimal strings in thousandths; shifts compare `current/prior` as the exact
cross product `a·d / b·c` (reason `ratio_to_prior`, unreduced).

**Unknown is never ok and never zero.** A missing source is `unknown` with
its reason: `collection_not_run`, `no_collect_recorded`, `ledger_not_synced`,
`not_priced`, `attention_not_collected`, `throttling_not_certified`,
`no_budget_policy`, `no_current_window` (every window has reset: the new
window's headroom is unknown), `insufficient_data`, `not_observed`, or
`source_error` when a read failed. Its `evidence.value` is
`{status: unavailable, reason}`. An empty population (M13 with no terminated
Codex attempt) is `ok empty_population`: nothing is missing. Throttling is
not certified for Codex (contracts-accounting §5), so `service_throttled`
stays `unknown`; an exhausted quota window (`remaining 0`) is the certified
throttle signal and makes `quota_headroom` critical.

## 3. Alerts: deduplication, cooldown, outages

At most one **open** alert per rule key (the labels' canonical JSON). On
`health evaluate` (and the ticker):

- `warn`/`critical` without an open alert opens one, unless the rule's
  cooldown since the key's last resolution is still running: then nothing is
  opened and `health_rule_states.suppressed` counts it (`suppressed[]` in the
  output). A condition that persists never re-alerts.
- `warn`/`critical`/`unknown` with an open alert updates it (state,
  reasons, evidence, `occurrences + 1`, `last_seen_unix_ms`): escalation and
  de-escalation stay one alert (`updated[].deduplicated: true`).
- `ok` resolves the open alert (`resolved_unix_ms`).
- `unknown` opens an alert only for a rule that once had its source
  (`known`): an outage, with reason `source_lost` first. A rule that never had
  a source (a fresh project) is `unknown` without an alert. `unknown` never
  resolves an open alert: a lost source does not prove the condition cleared.

Alert JSON: `{alert_id, rule, labels, state, reasons, metric,
evidence_window, evidence, rules_version, opened_unix_ms, last_seen_unix_ms,
occurrences, resolved_unix_ms, status: open|resolved, notified_unix_ms,
notice_id}`.

## 4. Stream `health` (version 1)

`migrations/telemetry/health/0001_health_alerts.sql` (re-runnable,
`IF NOT EXISTS`):

- `health_evaluations(evaluation, evaluated_unix_ms, rules_version, source
  cli|tick, summary)`: the newest 1000 are kept.
- `health_rule_states(rule_key PK, rule, state, reasons, known, suppressed,
  evaluated_unix_ms)`.
- `health_alerts(alert_id, rule_key, rule, labels, state warn|critical|unknown,
  reasons, metric, evidence_window, evidence, rules_version, opened_unix_ms,
  last_seen_unix_ms, occurrences, resolved_unix_ms, notified_unix_ms,
  notice_id)`, unique partial index on `rule_key WHERE resolved_unix_ms IS
  NULL` (dedup is also a schema invariant).

One evaluation reads every rule first (read-only), then writes in one
immediate sidecar transaction. Without a sidecar `health evaluate` records
nothing (`recorded: unavailable collection_not_run`) and creates none.
**Ticker:** the lane `tick` hook evaluates at most once per 300 s, and only
after an operator's first `health evaluate` (a `health_evaluations` row
exists); it never notifies. The lane adds no `telemetry report` keys.

## 5. Advisory recommendations (`telemetry-recommendation.v1`)

`recommend --role CLASS` runs the TM4.4 comparison
([contracts-evaluation.md](contracts-evaluation.md)) for the role — a task
class of taxonomy v1, the only unit TM4.4 ranks — on `--metric` (default
M02). Output: `{contract, role, role_basis: task_class, status, reasons[],
advisory {advisory: true, authority: none, routing, writes: none}, metric
{metric_id, definition, higher_is_better, registry, comparison, freshness},
evidence_window {cohort, from_unix_ms, to_unix_ms, semantics: half_open,
time_basis}, analysis (observational, causal false), population, caveats
(the comparison's note codes), arms[], recommendation, uncertainty,
freshness}`.

- `no_recommendation` whenever TM4.4 does not rank the role's cell:
  `ranking_not_supported` with the comparison's own reasons (`single_arm`,
  `{insufficient_data: [...]}`, `interval_unavailable`,
  `difficulty_mix_differs`, `intervals_overlap`), or
  `no_evidence_for_role`. No recommendation is ever derived from the
  all-classes cell.
- Otherwise the best arm of `ranking.order`: `recommendation
  {configuration_id, label, value, decimal, tasks, pooled}`, `uncertainty
  {estimator (bootstrap method, B, seed, level), min_sample, recommended
  {interval}, runner_up {configuration_id, label, value, interval}, ranking
  {order, scope, observational, causal}}`, reason `intervals_separated` with
  the order as labels.
- **M50** (`M50.recommendation-v1`, registry `FRESHNESS`): supporting
  observations = the recommended arm's tasks in the role's cell. The
  configuration's **lineage** is its dispatched profile names
  (`attempt_inputs … effective_profile.name`; basis `profile`), or its agent
  kind when some decision has no inputs (basis `kind`). The current identity
  is the chosen configuration of the lineage's latest dispatch decision
  (`decided_unix_ms`, then attempt id). `value = under/supporting`, where
  `under` counts supporting observations produced under the current
  identity (all of them while it is the recommended configuration, none
  after a harness, model or profile change gives the lineage a new
  configuration). Below `stale_below = 1/2` the recommendation is
  **`stale`** (reason `configuration_changed`, from/to labels); an unknown
  lineage is stale too (`freshness_unknown`). Otherwise `recommended`.
  Evidence for the new configuration starts empty; old evidence stays
  queryable under its own arm.
- **Advisory only.** `recommend` opens `state.db` only through
  `telemetry::read_only` and the sidecar read-only, writes nothing (tested:
  every file under `.state` byte-identical), and names no store, admission,
  launch-preparation, profile-configuration or budget API (tested by source
  inspection); no dispatch or admission source reads the health lane.
  Adoption goes through the ordinary `thread start` path (doc 15 §6).

`query --metric M50` answers `unavailable: per_recommendation`; the health
view's M50 row reads `n/a (per_recommendation)`.

## 6. Notices

**Inbox (local).** `health notify` writes each open alert without a notice
as one canonical inbox item through the store's stable-id delivery
(`SqliteStore::deliver_telemetry_notice`, the memory-review reminder path):
kind `telemetry-health`, id `telemetry-health-<alert_id>-<opened_unix_ms>`,
subject the rule, empty body, one-line summary of labels and reason codes
only, e.g.

```
telemetry health warn: quota_headroom [services service=codex] headroom_low; advisory — `herdr-projects telemetry demo health alerts`
```

The alert then records `notified_unix_ms`/`notice_id`, so a second run, a
retry after a crash (the store answers `already_delivered`) or a
deduplicated re-evaluation writes nothing. A new episode (after the
cooldown) is a new alert and gets its own notice. `inbox done` acknowledges
the notice only; the condition re-alerts after its cooldown if still true.
The ticker never notifies.

**External (optional integration contract).** `health notify --external`
uses the deployment's own setting `<config_dir>/telemetry-alerts.toml`,
the same rules as the export destination ([contracts-export.md](contracts-export.md)
§6): a regular file of this user, not group/world writable, ≤ 16 KiB,
unknown keys refused, **disabled by default** (no file or `enabled = false`
refuses with `notify rejected: {code: external_notification_disabled}`).

```toml
schema = "telemetry-alerts-config.v1"

[external]
enabled = false               # true to allow `health notify --external`
destination = "directory"     # "directory" or "stdout"; nothing else
directory = "/srv/telemetry-alerts"   # absolute, a real directory of this user, not group/world writable
```

`directory`: one new file per open alert, `<slug>-health-<alert_id>-<opened>.json`
(`telemetry-health-alert.v1`: project, rule, labels, state, reasons,
metric, evidence window, evidence, times), written atomically and never
replaced (a second run skips it: `already_written`). `stdout`: one JSON line
per open alert, for a pipe. There is no network client: forwarding is the
deployment's own shipper under its existing authorization.

## 7. Restrictions

- M38/M39 have no certified producer: `service_throttled` stays `unknown`.
- `budget_exposure` evaluates the canonical policy only (the shadow bridge's
  what-if policies are not read); canonical budget enforcement is unchanged.
- The recommendation role is the task class; roles beyond the taxonomy
  (e.g. reviewer vs implementer) need a TM4.4 comparison unit first.
- Shift rules compare fixed 7-day windows; the thresholds are provisional
  defaults, revisable only as a new rules version.
- Queue age and spool-cap alerts (doc 08 §6, doc 15 §7) have no read path in
  the query service yet.
