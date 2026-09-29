# Telemetry quality contracts (lane C)

Owned by lane C ([phase2-lanes.md](phase2-lanes.md)); common rules are
[contracts.md](contracts.md) §0. Sidecar stream `quality`, migrations under
`migrations/telemetry/quality/`.

## 1. Proxy signals (TM3.7, card C1)

Plan doc 06 §6b, doc 07 M45. Every row has `source_trust = 'proxy_observed'`.
Proxies are analytics only: they never accept, verify, integrate, reopen or
create a finding, and no canonical code path reads them.

**Observation.** `telemetry <slug> quality collect` (output key
`proxy_signals`; the ticker runs at most 16 diffs per pass, only when the
sidecar exists) reads `state.db` strictly
read-only and writes stream `quality` table `proxy_signals`, one row per task
with `kind = 'first_candidate_ci'`:

- *First candidate*: the task's first `result_submissions` row by
  `(created_unix_ms, rowid)`.
- *Pinned CI result*: that submission's first `verification_runs` row by
  `(created_unix_ms, rowid)`; its `state`, `run_id` and `policy_digest` (the
  pinned CI definition) are copied by value. Later runs and later submissions
  never change the signal.
- *Test weakening* (rule `tests-net-removal.v1`): `git diff --numstat
  --no-renames base_oid candidate_oid -- :(top)tests/` in the submission's
  `repository`, spawned through `execution_guard::GatedSpawn`. Stored:
  `tests_added_lines`, `tests_deleted_lines`, `tests_binary_files` — counts
  only, never paths or diff text. `flagged` when deleted > added, else
  `clear`. When the diff cannot run the counts are `NULL`, `weakening =
  'unavailable'` with `weakening_reason` (`repository_missing`,
  `git_unavailable`, `diff_failed`, `diff_unparseable`) and the next collect
  retries it; settled rows are never rewritten.

**M45 first-candidate CI pass (proxy)**, definition `M45.proxy-v1`, reported
by `telemetry <slug> quality report [--since MS]` (read-only):

- numerator: tasks whose first candidate's pinned-CI run is `accepted` and
  whose weakening check is `clear`; denominator: tasks with a first-candidate
  run and a `clear` check. Value `"n/d"`; empty denominator is `null` with
  `empty_denominator`; no sidecar is `unavailable: collection_not_run`.
- `excluded`: `test_weakening` (flagged, listed in `flagged` with counts),
  `weakening_unavailable`, `not_collected` (a canonical first run with no
  signal yet). `pending`: first candidates not yet verified (unwindowed only).
- `--since` windows by the first run's `created_unix_ms`.
- Labeled `proxy: true`, `source_trust: proxy_observed`; never replaces M30.

Also reported by `telemetry <slug> report` and the fleet pane, with M46–M48.

## 2. Integration outcomes (TM3.7, card C2)

Plan doc 06 §6b, doc 07 M46–M48 (numbering per doc 07: M47 survival, M48
revert). Rows have `source_trust = 'proxy_observed'`; the §1 analytics-only
rules apply.

**Observation.** `quality collect [--horizon-days N]` (default 14, 1–3650;
output key `integration_outcomes`) reads `integrated_commits` read-only. An
integration younger than the horizon (`now < created_unix_ms + horizon`) is
*censored*: not observed, counted. Each other one without a settled row gets
one row in stream `quality` table `integration_outcomes`, keyed
`(integrated_id, horizon_ms)`, by git in its `repository` on its `ref_name`
(rule `outcomes.v1`), all through `execution_guard::GatedSpawn`, stdout
capped at 4 MiB per call:

- *Own commits*: `expected_old_oid..commit_oid` (the merge and its branch);
  the parent tree is `expected_old_oid`'s.
- *Revert* (`reverted`): among commits on the ref after `commit_oid` whose
  committer time is at most the horizon end (at most 512), `trailer` if one
  whose message has `This reverts commit <sha>` naming an own commit, else
  `tree_restore` if one whose tree equals the parent tree, else `none`. A
  revert itself reverted within the horizon (by trailer) does not count
  (lineage).
- *Survival*: the horizon commit is the ref's first-parent commit at the
  horizon end (must be `commit_oid` or after it). For each file the
  integration added lines to (`diff --numstat --no-renames`; binary,
  `*.lock`, `vendor/` and `third_party/` excluded; at most 32 files):
  `added_lines` = lines `git blame --incremental` attributes to own commits
  at `commit_oid`; `surviving_lines` = the same at the horizon commit (0 if
  the file is gone). `churn_added_lines`/`churn_deleted_lines` = numstat of
  those files from `commit_oid` to the horizon commit.
- Counts and object IDs only; no path, message or line text is stored. A
  failure stores `unavailable_reason` (`repository_missing`,
  `git_unavailable`, `git_failed`, `git_unparseable`, `output_limit`,
  `history_limit`, `too_many_files`, `not_on_ref`) and is retried; settled
  rows are never rewritten.
- Bounds: at most `6 + 2 × 32` git calls per integration; `collect` observes
  at most 16 integrations, the ticker (default horizon, existing sidecar
  only) one; the rest are `deferred`.

**Metrics** (`quality report [--since MS] [--horizon-days N]`; `telemetry
<slug> report` uses the default horizon). `--since` windows by
`created_unix_ms`. Both carry `proxy: true`, `horizon_days`, `censored`,
`not_collected` (matured, no row), `unavailable`; no table is `unavailable:
collection_not_run`, an empty denominator `null` with `empty_denominator`.

- **M48 revert rate (proxy)** `M48.proxy-v1`: integrations reverted within
  the horizon / observed integrations, with `reverted_by` {`trailer`,
  `tree_restore`}.
- **M47 code survival (proxy)** `M47.proxy-v1`: surviving lines / added
  lines, with `area_churn` {`added_lines`, `deleted_lines`} beside it.
- **M46 main breakage** is `unavailable: no_main_check_producer`;
  **`flaky_tests`** (newly flaky tests) is `unavailable: no_repeat_runs`.
