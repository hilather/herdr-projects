# Operator views: project, models, reviews, cost and health (TM4.2)

Plan card TM4.2 (doc 12), doc 08 §2–§3, doc 15 §3. Common rules are
[contracts.md](contracts.md) §0; the read contract is
[contracts-analytics.md](contracts-analytics.md) §2. Code:
`src/telemetry/views/` (`mod.rs`, `render.rs`), `src/telemetry/panel.rs`
(pane sections), the fleet pane in `src/actions.rs`. Tests:
`tests/telemetry_views.rs`.

The views are a presentation layer over the TM4.1 query service. They never
open a store themselves: every value, numerator, denominator, coverage, lag,
projection and rate-card revision is the query service's
(`analytics::query::{request, run}`). They write nothing, launch nothing,
release no budget and grant no authority.

## 1. Commands

```
telemetry <slug> view project|models|reviews|cost|health [--json] [--from MS] [--to MS] [--as-of MS]
telemetry <slug> view <view> --drill <metric> [--bucket B] [--page-size N] [--cursor C] [--json]
```

| view | rows (metric: label) | extra section |
|---|---|---|
| `project` | M01 accepted tasks, M02 acceptance rate, M06 lead time p95, M07 attempt amplification, M36 integration conflict rate | — |
| `models` | M15 effective model reported, M08/M09 tokens, M13 usage coverage, M41/M42 paired quality | M02 and M07 per requested agent (`agent_kind`); requested / reported / unknown model identity |
| `reviews` | M20–M23, M25 fix verified, M26 currently resolved, M27 reopen rate, M29 attribution coverage; proxies M45/M47/M48 labelled `[proxy]` | fixes: verified \| integrated \| currently resolved |
| `cost` | M11 provider-billed spend, M12 estimated spend, M14 cost coverage, M04 cost per accepted task, M34, M37 | — |
| `health` | M13, M15, M14, M29 coverage; M38–M40 services; M49 replay; M50 freshness | source watermarks (canonical head, sidecar streams, last collect, valuation revision) and per-family lag |

Each view makes at most two query requests (its metrics; for `models` also
M02/M07 `--by agent_kind`). The window flags pass through unchanged: lane
metrics are since-only, so `--to` makes them `n/a (window_end_unsupported)`.
`--as-of` answers from analytics revisions (`n/a (no_revision_as_of)` for a
cell never refreshed by then).

## 2. Reading a row

```
  <id> [proxy] <label>: <value> · basis <basis> · coverage <coverage> · n=<sample> · lag <lag> · <projection>
```

- **value**: unknown is `n/a (<reason>)`, never 0; a 0 is an observed value
  (`0/1 (0.0%)`, `0 tasks`). Ratios keep the exact `n/d` with a percentage;
  other fractions add the decimal in the metric's unit.
- **cost basis**: estimates read `est. <currency> <amount>` (basis
  `published_rate_estimate`), provider charges `billed <currency> <amount>`
  (basis `provider_billed`). They are separate rows and never added. A
  partial estimate reads `est. USD x observed portion (partial: <reason>)`.
- **basis**: `canonical_lifecycle` for native rows, otherwise the lane's own
  `basis`/`source_trust`/`trust`, `central_report`, or `no_producer`.
- **coverage**: native `complete|partial known/expected`; lanes do not state
  their expected population, so `unknown (lane counts)`, never 100 %.
- **n**: the sample the value rests on (denominator, samples, placed tasks,
  valued entries, charges counted, certified sessions or decisions); `n/a`
  for an unavailable value. JSON names where it came from (`sample.basis`).
- **lag**: observation lag (0 s for native rows; lanes: time since the last
  collect, or `n/a (collection_not_run)`).
- **projection**: `live`, `revision N` (`(restated)` when superseded) or
  `no revision`.

`--json` (`telemetry-views.v1`) carries each row's query fields verbatim
(`value`, `numerator`, `denominator`, `status`, `coverage`, `lag_ms`,
`projection`, `rate_card_revision`, `certification`) beside the presentation
(`display`, `basis`, `cost_basis`, `basis_tag`, `sample`, `text`), plus the
normalized query request and `query_unix_ms`.

Fixes are three outcomes from three metrics: **verified** (M25 finding
outcomes), **integrated** (M27's integrations: observed for the reopen
horizon plus censored) and **currently resolved** (M26). A reopened fix keeps
its verification and integration and loses current resolution.

Model identity: the requested agent is the dispatched profile's kind
(`agent_kind` cells of the native metrics); the requested model *name* is not
in the query service and reads `n/a (not_in_query_service)`; the reported
effective model is M15's `reported of records`, the rest `unknown`. Hidden
identities stay unknown.

## 3. Drill-down

`--drill <metric>` pages the identities behind one of the view's native
metrics through the query service's drill-down (`--bucket` default
`numerator`; page size default 20, at most 500). The cursor is the query's
own: it binds the request and the snapshot, so later pages neither skip nor
repeat rows; a changed unpinned snapshot is `restart_required`. Lane metrics
answer `drill: n/a (drill_unsupported)` (drill through their ledger commands).
A metric that is not in the view is refused.

## 4. The fleet pane

The `fleet` popup (action `fleet`, pane `fleet` in `herdr-plugin.toml`)
prints, per project, the report and active attempts as before, then
`views (...)` with one section per view. Each row line is byte-identical to
the CLI's (the pane asks the query service for all five views at once: the
same two requests one view makes). Bounded: five fixed row lists, at most 11
rows each. It is a one-shot read: no polling of controller snapshots.

## 5. Isolation

A view reads exactly one project:

- the slug is validated (lower-case letters, digits, hyphens; at most 40);
- `<root>/<slug>`, its `.state` and its `state.db`/`telemetry.db` must be the
  project's own directory and regular files: a symlink (for example a project
  directory or sidecar pointing into another project) is refused;
- the `fleet` handoff binds the named project's store identity
  (`device:inode` of `state.db`) when the action runs; the pane refuses a
  handoff whose slug is invalid, whose store differs, or that names a project
  other than the popup's own workspace project, and renders nothing then.

## 6. The switch

```toml
# ~/.config/herdr-projects/config.toml
[telemetry]
views = false
```

turns off `telemetry <slug> view` (refused with the reason) and the fleet
pane (prints only that it is disabled). Default: on. Only the views and the
pane read it: collection, the ticker's telemetry passes, admission and the
budget bridge do not (`switching_views_off_leaves_collection_ticker_authority_and_budgets_alone`
compares their answers with the switch off and on). Any value but `true` or
`false` is refused.

## 7. Operator examples

Real outputs of the contracts §6 worked example (`tests/telemetry_views.rs`
plants it; `operator_doc_examples_are_real_outputs` checks every line below):
t1 verified (verify-only), t2 integrated after a failed attempt, t3 verified
but not integrated, t4 failed twice, t5 running; no telemetry collected yet.

<!-- example: view project -->
```text
demo · project view · window [-inf, +inf) · live
  M01 accepted tasks: 2 tasks · basis canonical_lifecycle · coverage complete 3/3 · n=3 · lag 0s · live
  M02 acceptance rate: 2/3 (66.7%) · basis canonical_lifecycle · coverage complete 3/3 · n=3 · lag 0s · live
  M06 lead time p95: 1500 ms · basis canonical_lifecycle · coverage complete 2/2 · n=2 · lag 0s · live
  M07 attempt amplification: 5/2 (= 2.5 attempts per accepted task) · basis canonical_lifecycle · coverage complete 3/3 · n=2 · lag 0s · live
  M36 integration conflict rate: 0/1 (0.0%) · basis lane_accounting · coverage unknown · n=1 · lag n/a (collection_not_run) · live
```

<!-- example: view models -->
```text
demo · models view · window [-inf, +inf) · live
  M15 effective model reported: n/a (no_certified_source) · basis central_report · coverage unavailable · n=n/a · lag n/a (collection_not_run) · live
  M08 input tokens: n/a (no_certified_source) · basis lane_accounting · coverage unavailable · n=n/a · lag n/a (collection_not_run) · live
    M02 agent_kind=unknown: 2/3 (66.7%) · n=3
    M07 agent_kind=unknown: 5/2 (= 2.5 attempts per accepted task) · n=2
  identity requested agent (profile kind): unknown 3 tasks; requested model name: n/a (not_in_query_service)
  identity reported effective model: n/a (no_certified_source)
```

<!-- example: view reviews -->
```text
demo · reviews view · window [-inf, +inf) · live
  M25 fix verified: n/a (empty_denominator) · basis owner_attribution · coverage unknown · n=0 · lag n/a (collection_not_run) · live
  M26 currently resolved: n/a (empty_denominator) · basis owner_attribution · coverage unknown · n=0 · lag n/a (collection_not_run) · live
  M45 [proxy] first-candidate CI pass: n/a (collection_not_run) · basis proxy_observed · coverage unavailable · n=n/a · lag n/a (collection_not_run) · live
  fixes verified: n/a (empty_denominator) | integrated: 0 integrated (0 observed for the reopen horizon, 0 censored) | currently resolved: n/a (empty_denominator)
```

<!-- example: view cost -->
```text
demo · cost view · window [-inf, +inf) · live
  M11 provider-billed spend: n/a (collection_not_run) · basis provider_billed · coverage unavailable · n=n/a · lag n/a (collection_not_run) · live
  M12 estimated spend: n/a (collection_not_run) · basis published_rate_estimate · coverage unavailable · n=n/a · lag n/a (collection_not_run) · live
```

With a collected, repriced session (doc 10 golden: 1,000 input + 500 output
at $2/$4 per million) and the synthetic charges file imported, the same rows
read (from `cost_view_keeps_estimates_and_billed_apart`; the lag varies):

```
  M11 provider-billed spend: billed USD 0.5164 · basis provider_billed · coverage unknown · n=5 · lag …
  M12 estimated spend: est. USD 0.004 · basis published_rate_estimate · coverage unknown (entries=1 priced=1) · n=1 · lag …
```

and after a verified, integrated, then reverted fix
(`reviews_view_separates_verified_integrated_and_resolved`):

```
  fixes verified: 1/1 (100.0%) | integrated: 1 integrated (1 observed for the reopen horizon, 0 censored) | currently resolved: 0/1 (0.0%)
```

<!-- example: view health -->
```text
demo · health view · window [-inf, +inf) · live
  M40 quota headroom at dispatch: n/a (no_decisions) · basis central_report · coverage unknown · n=0 · lag n/a (collection_not_run) · live
  M49 replay suite pass rate: n/a (awaiting_replay_suite) · basis no_producer · coverage unavailable · n=n/a · lag n/a (collection_not_run) · live
  sources canonical: events_head=0 last_event=3100
  sources sidecar: n/a (collection_not_run)
  family services: 3 metrics, 2 unavailable, lag n/a (collection_not_run)
```

<!-- example: view project --drill M02 --bucket denominator --page-size 2 -->
```text
demo · project view · drill M02
  bucket denominator rows 1-2 of 3 · snapshot live
  task t1 accepted
  task t2 accepted
… next: --cursor <opaque>
```

<!-- example: pane -->
```text
M02 task_acceptance_rate 2/3
views (as `telemetry demo view <name>`; live, all time)
 project
  M02 acceptance rate: 2/3 (66.7%) · basis canonical_lifecycle · coverage complete 3/3 · n=3 · lag 0s · live
 cost
  M12 estimated spend: n/a (collection_not_run) · basis published_rate_estimate · coverage unavailable · n=n/a · lag n/a (collection_not_run) · live
```

The next page: `telemetry demo view project --drill M02 --bucket denominator --page-size 2 --cursor <next>`
prints `bucket denominator rows 3-3 of 3` and `task t4 failed`.

## 8. Restrictions

- The views read what the query service serves: a metric without a producer
  or an inactive family is `n/a` with the registry's reason.
- Requested model *names* and per-model usage are not in the query service
  (`not_in_query_service`); TM4.4 comparisons own configuration-level evidence.
- The pane is a popup snapshot, not a refreshing split pane (doc 15 §3's
  `telemetry watch` is TM4.8).
