# Analytics contracts: registry, query service and aggregate revisions (TM4.1)

Plan card TM4.1 (doc 12), doc 07 §1–§7, doc 08 §4, doc 10 §3–§4. Common rules
are [contracts.md](contracts.md) §0. Code: `src/telemetry/analytics/`
(`registry.rs`, `query.rs`, `lifecycle.rs`, `store.rs`); sidecar stream
`analytics`, migrations under `migrations/telemetry/analytics/`. This is the
**stable read contract** shared by the workspace UI (TM4.2) and exports
(TM4.3). Telemetry never grants launch, changes budgets or accepts results;
nothing here writes `state.db`.

## 1. Metric registry (`analytics-registry.v1`)

`telemetry <slug> metrics registry [--json]` prints one declared table
(`registry.rs`) of every metric `telemetry report` or `query` can name:
M01–M50 and lane C's `flaky_tests`. A change is a new registry version, never
an edit in place of a published definition. Per metric:

| field | meaning |
|---|---|
| `id`, `name`, `unit` | doc 07 identity; units are explicit (`ratio` values are the unreduced string `"n/d"`) |
| `definition` | the current definition `query` serves by default |
| `versions[]` | every servable definition: `provider` (`native`, `central_report`, `lane` + stream, `absent` + reason), `cohorts` (first = default), `window` (`half_open` or `since_only`), `time_basis`, `dimensions` |
| `family` | `lifecycle`, `consumption`, `cost`, `tools`, `attention`, `fleet`, `services`, `review_quality`, `paired_quality`, `seeded_quality`, `proxy`, `replay`, `freshness`; `proxy: true` for the proxy family, which never stands in for a validated-quality metric |
| `certification` | `{status, evidence, restriction}` from [certificate-core.md](certificate-core.md) §5 and [certificate-quality.md](certificate-quality.md) §2: `certified-live`, `certified-fixture`, `restricted`, `unavailable`, `fixture` (TM4.1's own fixtures only) or `absent` (no producer) |
| `active`, `activation` | families activate independently: lifecycle/accounting/operational at TM2.6 (core certificate); proxy at TM3.7; validated quality at TM3.5 (`QUALITY_CERTIFICATE`, now `certificate-quality.md`, a fixture certificate: `activation.production` names the producer certificate production quality still waits for); replay (M49) and freshness (M50) inactive (`awaiting_replay_suite`, `awaiting_configuration_evidence`). Without the quality certificate the quality families answer `unavailable: awaiting_quality_certificate` |

Native definitions added by TM4.1 (the certified `slice-v1` ones stay
servable by name): `M01.cohort-v1` accepted tasks, `M02.cohort-v1`
acceptance rate, `M06.cohort-v1` lead-time p95 (nearest rank, ms, accepted
tasks with both times; failed/open counted, never given a time),
`M07.cohort-v1` attempt amplification. Absent producers: M03, M05, M10, M19,
M30, M49, M50.

## 2. Query service (`analytics-query.v1`)

```
telemetry <slug> query --metric M..[,M..] [--cohort activity_window|terminal_cohort|assignment_cohort]
    [--from MS] [--to MS] [--as-of MS | --as-of-seq N] [--by DIM] [--horizon-ms MS]
    [--drill BUCKET --page-size N --cursor C] [--json]
```

Read-only (opens both stores strictly read-only, creates no file in the project; a multi-page drill-down creates the per-user cursor key under the config directory on first use, [contracts-export.md](contracts-export.md) §4). `--metric`
takes a registry id (current definition) or an explicit definition
(`M02.slice-v1`).

**Request rejections** (exit status 1, stderr `query rejected: {code, ...}`):
`ambiguous_cohort` for `completed_task` (never read as success-only; the
diagnostic lists the accepted enums), `unknown_cohort`, `unknown_metric`,
`unknown_definition` (with the known ones), `empty_window` (`from >= to`),
`page_size_out_of_range` (1–500), `horizon_out_of_range`,
`drill_needs_one_metric`, `invalid_cursor`, `cursor_expired`,
`cursor_foreign_project`, `cursor_revoked`, `cursor_key_unusable`,
`cursor_mismatch`, `restart_required` (cursor codes:
[contracts-export.md](contracts-export.md) §4).

**Per-metric diagnostics** (the result's `status: unavailable` with `reason`
and `diagnostic`): the family's inactive reason; the absent producer's reason;
`cohort_unsupported` (with `supported`); `window_end_unsupported` (a
`since_only` definition given `--to`); `horizon_unsupported`;
`high_cardinality_dimension` (`task_id`, `attempt_id`, `session_id`, ...: use
`--drill`); `dimension_unsupported` (with `supported`); `too_many_cells`
(more than 64); `no_revision_as_of`.

**Response** (`--json`): `{schema_version: 1, contract, request (normalized
echo), query_unix_ms, results: [...], drill?}`. Each result:

| field | meaning |
|---|---|
| `metric_id`, `name`, `definition`, `family`, `proxy`, `unit`, `registry` | registry identity |
| `cohort`, `time_basis`, `window {from_unix_ms, to_unix_ms, semantics}`, `horizon_ms`, `by` | the evaluated cell |
| `status` | `available`, `empty` (a ratio with an empty denominator: `value` null, `reason` `empty_denominator`/`no_samples`), `partial`, `unavailable` |
| `value`, `reason`, `numerator`, `denominator` | unknown is never 0 (§0) |
| `exclusions` | counts by reason (`open`, `outside_window`, `terminal_time_unknown`, `not_assigned`, `assignment_time_unknown`, or the lane's `excluded`) |
| `coverage` | `{state: complete|partial|unknown|unavailable, known, expected, missing, reasons}` for native definitions; a lane definition that does not state its expected population is `unknown` (never 100 %) with the lane's own `coverage` under `lane` |
| `breakdown`, `cells`, `censored`, `provisional`, lane `detail` | native outcome counts; per-dimension cells; assignment-cohort unfinished tasks; the full lane or report body for lane/central definitions (its keys are lane-native fields, never labels) |
| `projection` | live: `{mode: live, revision: null, matches_revision, content_digest}`; as-of: `{mode: revision, revision, kind, supersedes, superseded_by, current_revision, restated, recorded_unix_ms, content_digest}` |
| `source_watermarks` | `canonical {events_head, lifecycle_digest, last_event_unix_ms}`; for lane definitions also `sidecar {streams, last_collect_unix_ms, codex_usage_rowid, valuation, rate_cards}` |
| `event_cutoff_unix_ms`, `observation_cutoff_unix_ms` | latest occurrence time in the cohort; knowledge time (query time live, `recorded_unix_ms` as of) |
| `lag_ms`, `lag_reason` | native: 0 (canonical read directly); lane: observation cutoff − last collect, or null with `collection_not_run`/`no_collect_recorded` |
| `rate_card_revision` | priced metrics (M04, M12, M14, M24, M34, M37): `{valuation_revision, valuation_digest, rate_cards {count, digest}}` or unavailable (`not_priced`, `collection_not_run`); otherwise null |
| `certification`, `activation` | as the registry |
| `as_of` | the requested knowledge time or sequence, as-of results only |

Text form: one line per metric
`<id> <name> <definition> <cohort> <value> numerator=<n> denominator=<d> coverage=<state> <live|revision N>`,
dimension cells indented, then drill rows.

## 3. Cohorts and native lifecycle semantics

Plan doc 07 §1. `T`/`A` evidence is exactly contracts §6 (`metrics::task_evidence`).

- **`terminal_cohort`**: tasks with a terminal disposition (`accepted`,
  `succeeded_without_evidence`, `failed`, `cancelled`), their whole lifecycle
  included; succeeded, failed and cancelled tasks are all members. Terminal
  time: acceptance → when the evidence completed (verify_only: first verified
  result; integrate route: first integrated commit); otherwise the latest
  terminal lifecycle mark of its attempts (the canonical store keeps no task
  terminal time). A bounded window places a task by that time in `[from,
  to)`; a terminal task with no time is excluded as
  `terminal_time_unknown` and makes coverage `partial` (never dropped
  silently, never placed by guess). Open tasks are the `open` exclusion.
- **`assignment_cohort`**: tasks first assigned (earliest dispatch decision,
  else reservation mark) in the window, followed to `--horizon-ms` (or to
  now): an outcome after the horizon, or an open task, is `unfinished`
  (`censored.unfinished`, `provisional: true`), counted in the denominator.
  Tasks never assigned are `not_assigned`.
- **`activity_window`**: events in the window; lane definitions only
  (`since_only`: `--from` is the lane's `--since`).
- Dimensions for native definitions: `route`, `task_class` (latest
  classification, else `unclassified`), `agent_kind` (the attempts' effective
  profile kind, `mixed`, `unknown` or `unassigned`). At most 64 cells.

## 4. Aggregate revisions (stream `analytics`, version 1)

`migrations/telemetry/analytics/0001_aggregate_revisions.sql`:

- `analytics_cells(cell PK, metric, definition, cohort, window_from_unix_ms,
  window_to_unix_ms, horizon_ms, dimension, tracked_unix_ms,
  checked_unix_ms)`: the tracked cells; `cell` is the canonical JSON key
  `{by, cohort, definition, from, horizon_ms, metric, to}`. Only
  `checked_unix_ms` is mutable.
- `analytics_revisions(revision PK, cell, kind initial|restatement,
  supersedes, body, content_digest, watermarks, registry, recorded_unix_ms)`:
  append-only (triggers refuse update and delete). `revision` is the
  projection sequence (`--as-of-seq`), `recorded_unix_ms` the knowledge time
  (`--as-of`). `content_digest` = sha256 of the canonical JSON
  `{body, lineage}` (empty buckets omitted); `watermarks` is provenance, not
  content.
- `analytics_lineage(revision, bucket, ordinal, entity_kind task|attempt,
  entity_id, attrs)`, `WITHOUT ROWID`, append-only: the drill-down lineage of
  each revision.

Commands (writes only this stream's tables; needs an existing sidecar, else
`unavailable: collection_not_run`):

- `analytics refresh [--metric M --cohort --from --to --horizon-ms --by]`:
  evaluates every tracked cell (first run: every active metric's default
  cell) and, in one immediate transaction, appends a revision only for a cell
  whose content changed (incremental: an unchanged cell only gets
  `checked_unix_ms`). A late correction therefore appends a `restatement`
  superseding the previous revision; earlier revisions stay readable byte for
  byte. Racing refreshes append once.
- `analytics rebuild [--verify]`: recomputes every tracked cell from the
  sources and compares it with the latest revision (`identical`), re-digesting
  the stored bytes (`stored_intact`); without `--verify` a differing cell is
  restated.
- `analytics snapshot`: latest content per tracked cell without revision
  numbers or times: two projects rebuilt from the same sources print the same
  bytes.
- `analytics revisions [--metric M]`, `analytics status`, `analytics plans`.
- Ticker: on its telemetry pass (default every 300 s) the lane refreshes
  tracked cells, at most once a minute, and only once an operator has run
  `analytics refresh` (a tracked cell exists).

Restrictions: a revision's content is whatever its sources held when it was
recorded; the sources themselves keep no history (certificate-core R4), so a
rebuild reproduces the latest content, and older content only survives in the
stored revisions. Lane values that depend on the wall clock (e.g. censoring
at a horizon) restate as time passes. As-of answers exist only for cells
refreshed by then (`no_revision_as_of` otherwise).

## 5. Pagination and drill-down

`--drill <bucket>` pages the identities behind one native metric: buckets
`numerator`, `denominator` (M02, M07), `outcome.<disposition>`,
`excluded.<reason>`; rows `{entity, id, ...attrs}` (tasks: disposition,
terminal and assignment times, attempts, route, agent kind, task class;
attempts: task, state, decision time), ordered by id. `--page-size` 1–500
(default 100); `next_cursor` is null on the last page,
`next_cursor_expires_unix_ms` its expiry. The cursor (TM4.3,
[contracts-export.md](contracts-export.md) §4) is authenticated with a keyed
MAC (per-user key under the config directory), scoped to the project and
expires after 30 minutes; it binds the normalized request, the snapshot's
`content_digest`, the pinned revision, the bucket and the position. A first
page whose live content matches a stored revision is pinned to it: later
pages read that revision's lineage and neither duplicate nor skip rows while
new data arrives. An unpinned live snapshot that changed answers
`restart_required`, never a mixed page. Exports (`telemetry export --drill`)
page through this same cursor. High-cardinality
identities appear only in drill rows, never as metric labels or dimension
values. Lane definitions answer `drill_unsupported` and drill through their
lane's ledger commands.

## 6. `telemetry report`, the fleet pane and indexes

`telemetry report` and the fleet pane read through the query service's read
path (`analytics::query::report`: the central slice metrics, then each lane's,
a lane key replacing a central one). Their output is byte-identical to before
TM4.1: the report keeps each lane's own definitions and does not apply the
registry's activation gate (every current family is active). For every
metric the report prints, `query --metric <its definition>` returns that
body as `detail` (`report_and_query_share_one_read_path`). The `analytics`
lane adds no report keys.

`analytics plans` prints `EXPLAIN QUERY PLAN` for the hot reads with a verdict
(`indexed`, `full_scan_inherent` for the table a cohort aggregates in full,
`needs_index` for any other scan or an automatic index):

| query | store | verdict | owner and proposed index |
|---|---|---|---|
| as-of by sequence / time, latest, next, lineage page, buckets | telemetry.db | indexed (`analytics_revisions_cell`, `analytics_revisions_cell_recorded`, lineage primary key) | analytics (this stream) |
| `lifecycle_attempts`, `lifecycle_contracts`, `lifecycle_classes` | state.db | one pass over the aggregated table; every correlated lookup an index search | canonical |
| `lifecycle_acceptance_times` | state.db | `verified_results` by `submission_id` uses an automatic index | canonical steward: `CREATE INDEX verified_results_by_submission ON verified_results(submission_id)` (not added: no canonical schema change in TM4.1) |
| central M08/M13/M15 source scan | telemetry.db | `codex_usage` scanned per source by `path_digest` | codex stream steward: `CREATE INDEX codex_usage_by_path ON codex_usage(path_digest, accepted, reason)` |
| usage by session | telemetry.db | indexed (primary key) | codex |

## 7. Comparisons and experiments (TM4.4)

`telemetry <slug> compare` and `experiments plan|report` are specified in
[contracts-evaluation.md](contracts-evaluation.md). They read cohorts
through this service's lifecycle evaluation (the same membership and
lineage as `query`), add no metric and no sidecar stream, and never write.
The registry's `metrics registry --json` gains one additive key,
`comparison` (`analytics-comparison.v1`: comparable definitions M02/M07,
bootstrap method, seed and level, the 20-task cell minimum, the
`beta_binomial_eb.v1` pooling prior, `hajek_ipw.v1` and the paired M42
reference); the text form and every metric entry are unchanged.
