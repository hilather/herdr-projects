# Telemetry quality contracts (lane C)

Owned by lane C ([phase2-lanes.md](phase2-lanes.md)); common rules are
[contracts.md](contracts.md) §0. Sidecar stream `quality`, migrations under
`migrations/telemetry/quality/`.

## 1. Proxy signals (TM3.7, card C1)

Plan doc 06 §6b, doc 07 M45. Every row has `source_trust = 'proxy_observed'`.
Proxies are analytics only: they never accept, verify, integrate, reopen or
create a finding, and no canonical code path reads them.

**Observation.** `telemetry <slug> quality collect` (and the ticker, at most
16 diffs per pass, only when the sidecar exists) reads `state.db` strictly
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

Not yet in `telemetry <slug> report`: the fleet pane must render every
reported metric and `metrics::text` lists only central metrics (steward).
