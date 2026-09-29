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

## 3. Candidate groups (TM3.8, card C3)

Plan doc 05 §5a, doc 06 §6c, doc 03 `CandidateGroup`/`CandidateSelection`.
Owner decision 1 ([phase2-lanes.md](phase2-lanes.md)): arms are
**sequential**; each arm is an ordinary attempt of the group's task, reserved
through the existing launch path. `tasks.active_attempt`, the scheduler and
integration are unchanged. Canonical migration `0053_candidate_groups.sql`
(schema 53); store API `src/store/candidate_groups.rs`.

**Tables** (canonical, append-only: every UPDATE/DELETE aborts except the one
seal below; no cost column anywhere):

- `candidate_groups(group_id, task_id, contract_revision, arm_count,
  creator_principal, canonical_json, created_unix_ms, sealed_unix_ms)`.
  `group_id` = `sha256:` over `canonical_json` = canonical JSON
  `{arms:[{arm, configuration_id, profile_digest}], contract_revision,
  created_unix_ms, schema:"candidate_group.v1", task_id}`. One group per
  `(task_id, contract_revision)` (NULL revision: task without a contract);
  `contract_revision` is the task's latest at creation. 2–8 arms.
- `candidate_group_arms(group_id, arm, configuration_id, profile_digest)`:
  `arm` 1..`arm_count` is the launch order; configurations are distinct
  (contracts §2 IDs, inserted into `agent_configurations`). *Seal*: the group
  row, all its arms and `sealed_unix_ms` are written in one transaction; a
  trigger allows the seal only once and only with exactly `arm_count` arms, and
  refuses any arm insert into a sealed group. Per-arm budgets are not recorded.
- `candidate_arm_attempts(group_id, arm, attempt_id UNIQUE, bound_unix_ms,
  source)`: written in the `admit_prepared` reservation transaction, right
  after the attempt's dispatch decision (`source = 'admit_prepared'`). The
  attempt binds the *next* arm (lowest unbound arm of the task's sealed,
  unselected group for the attempt's contract revision) only when its chosen
  configuration is that arm's; otherwise nothing is written and the
  reservation proceeds unchanged (a group never grants or refuses launch).
  A trigger also refuses a bind into an unsealed or selected group, to an
  attempt of another task, revision or configuration, or to an attempt that
  already has a result. Attempts reserved before the seal, out of order, or
  after the selection are not arms. Drafts and delegated replays bind nothing.
- `candidate_selections(group_id PK, outcome, arm, attempt_id, submission_id,
  selector_kind, selector_principal, reason, evidence, selected_unix_ms)`: at
  most one per group; it closes the group. `outcome = 'selected'` names a
  bound arm, its attempt and one of that attempt's `result_submissions`
  (trigger-checked); `no_selection` names none. Reasons: selected
  `operator_judgment`, `first_passing_verification`, `unspecified`;
  no selection `no_candidate`, `none_acceptable`, `unspecified`; `rule` and
  `judge` selections (§4) add `first_passing_verification` and
  `judge_preference`. `evidence` is computed by the store at selection
  time, per bound arm: `{arm, attempt_id, submission_id (first by
  created_unix_ms, rowid, or null), verification, arm_outcome}` with
  `verification` the first candidate's combined verification (each
  acceptance policy of its task revision and each policy with a run,
  decided by its latest run: `rejected` if any, `accepted` if all, else
  `pending`; `no_candidate` without a submission) and `arm_outcome` as in
  §4. IDs and states only, no text.

**Selection is not verification.** Recording a selection verifies,
integrates, completes or reassigns nothing; the winner still needs the
ordinary verification and integration path, and a loser keeps its own
outcome.

**Cost** is owned once by each attempt (contracts §4 `usage`). A group
stores no cost and never moves it: a losing, cancelled or unfinished arm
keeps its attempt's usage.

**Commands** (`telemetry <slug> quality groups ...`, JSON):

- `create <task> --arm <profile> --arm <profile> ...` seals a group; each
  `--arm` names a retained native profile (latest retained report of that
  name). Principal `operator:cli`. Writes through `SqliteStore` in one
  canonical transaction.
- `select <group> (--arm N [--submission ID] | --none) [--reason CODE]`
  records the selection (`selector_kind = 'operator'`, principal
  `operator:cli`); the default submission is the arm's first candidate.
  `--rule` and `--judge` are §4.
- `show` (read-only, contracts §0 reads): per group `status`
  (`open`/`closed`), `selection`, and per arm `attempt_id`, `role`
  (`open`, `selected`, `not_selected`), `candidate`, `outcome` (`candidate`,
  `running`, `failure_no_candidate`; an unbound arm is `not_launched` while
  open and `failure_no_candidate` once closed), and the attempt's
  `terminal_state`, `verification`, `integration`, `accepted` and `usage`
  exactly as `telemetry attempts` reports them. `cost.arms_total` sums
  every launched arm's usage field by field, or is `unavailable:
  arm_usage_unavailable` (listing each arm's reason) when any launched arm's
  usage is unknown; `arms_launched`, `arms_not_launched` (no attempt, so no
  consumption of its own) and `winner_usage` (drill-down only).

**Integration is not held by C3.** Owner decision 1 wants integration held
until a selection exists, which needs integration edits outside lane C
(`src/store/integration_jobs.rs` eligibility and `begin_integration`), so it
is escalated, not built. Until then `show` reports `integration_hold:
{enforced: false, integrated_without_selection: [...]}` listing arms whose
candidate integrated without being the selection.

## 4. Selectors and paired outcomes (TM3.8, card C4)

Plan doc 06 §6c, doc 07 M41/M42 and §6, doc 10 §5a. No schema change: the
0053 `selector_kind` check already allows `operator`, `rule` and `judge`, and
each selection is written through the store in one canonical transaction
(`src/store/candidate_groups.rs`). Selection is still not verification (§3).

**Arm outcome** (`arm_outcome.v1`), the verified outcome of one arm now:
unbound arm `not_launched`; else, over all of its attempt's submissions by
`(created_unix_ms, rowid)` with the §3 combined verification, `accepted` if
any is accepted; else `pending` if any is pending or the attempt is not
terminal (`completed`, `failed`, `cancelled`, `lost`), since it may still
submit; else `rejected` with a submission, `no_candidate` without.

**Rule selector** `select <group> --rule`: rule
`first_accepted_in_launch_order.v1`, principal
`rule:first_accepted_in_launch_order.v1`. The first arm in launch order whose
outcome is `accepted` wins with its first accepted submission, reason
`first_passing_verification`; a tie between accepted arms goes to launch
order. It refuses while any earlier arm is `pending`. With no accepted arm it
closes the group with no selection (`none_acceptable`, or `no_candidate` when
no arm submitted) only when every arm is bound and settled; otherwise it
refuses and the operator can still `--none`. Evidence adds `rank` (the
accepted arms in launch order, winner 1; null otherwise), so the runner-up
order is recorded for rule selections. The same canonical rows always give the
same answer.

**Judge selector** (no model is called anywhere):
`present <group>` (read-only) prints each bound arm's first candidate as
`{position, submission_id, repository, base_oid, candidate_oid}` — no arm,
attempt, configuration or profile — ordered by
`sha256("candidate_presentation.v1:" + group_id + ":" + submission_id)`.
`select <group> --judge NAME --submission ID` records the judge's choice of a
presented candidate: `selector_kind = 'judge'`, principal `judge:NAME`,
reason `judge_preference`, evidence adds each arm's `presented` position
(null when it had no candidate), so the blind order the judge saw is stored
with the selection. Choosing a judge configuration from another provider
family, and running it, is not built.

**Metrics** (`telemetry <slug> quality groups report [--since MS]
[--min-groups N]`, read-only over `state.db`; also in `quality report`,
`telemetry <slug> report` and the fleet pane at the default threshold).
Cohort: closed groups (with a selection row), windowed by
`selected_unix_ms`; `closed_groups` and `open_groups` are reported. Every
sealed arm is a member, launched or not. A cell with fewer than `min_groups`
(default 10, plan doc 07 §6) closed groups is `unavailable:
insufficient_data` with its counts still shown; no closed groups is
`unavailable: no_closed_groups`, a store before 0053 `unavailable:
candidate_groups_absent`. Never 0.

- **M41 candidate win rate** `M41.v1`, `by_selector` {`all`, `operator`,
  `rule`, `judge`}: per configuration `selected / groups` (as `"n/d"`) with
  `no_selection` and `other_selected`; `head_to_head` per ordered pair
  `(a, b)`: `wins / (wins + losses)` over closed groups containing both, with
  `ties` {`no_selection`, `other_selected`} (neither a nor b selected), or
  `unavailable: no_decisive_groups`. The top-level `value` lists the shown
  `all` cells as `sha256:<12 hex>=n/d`.
- **M42 paired acceptance difference** `M42.v1`, per ordered pair
  `(a, b)`: over closed groups containing both, groups where either arm is
  `pending` are excluded (`pending`); over the remaining `n`, acceptance is
  `arm_outcome = accepted` (1) or not (0; `not_launched`, `no_candidate` and
  `rejected` arms are failures, not missing), never the selection.
  `value` = `(a_only − b_only) / n × 100` percentage points (rounded to
  0.01; `difference` is the exact `"(a_only − b_only)/n"`), with
  `both_accepted`, `a_only`, `b_only`, `neither`. `uncertainty` is
  `unavailable: clustered_interval_not_computed`: no bootstrap by task
  family is computed. The top-level `value` lists shown pairs with `a < b`.
