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
through the existing launch path. `tasks.active_attempt` and the scheduler
are unchanged; integration and dependency release hold an arm until it is
the selection (below). Canonical migration `0053_candidate_groups.sql`
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
ordinary verification and integration path (it is only released from the
hold below and queued), and a loser keeps its own outcome.

**Cost** is owned once by each attempt (contracts §4 `usage`). A group
stores no cost and never moves it: a losing, cancelled or unfinished arm
keeps its attempt's usage.

**Commands** (`telemetry <slug> quality groups ...`, JSON). `create` and
`select` are the owner's: they refuse a worker execution context (the
contracts-review.md §9 markers), and `SqliteStore` refuses a `worker:*`
principal, any attempt's identity and an `import:*` principal as creator or
selector (a judge named after an attempt too), writing nothing, so an arm's
worker cannot select its own candidate and lift its hold (TM3.5,
certificate-quality.md):

- `create <task> --arm <profile> --arm <profile> ...` seals a group; each
  `--arm` names a retained native profile (latest retained report of that
  name). Principal `operator:cli`. Writes through `SqliteStore` in one
  canonical transaction.
- `select <group> (--arm N [--submission ID] [--runner-up ARM]... | --none)
  [--reason CODE]` records the selection (`selector_kind = 'operator'`,
  principal `operator:cli`); the default submission is the arm's first
  candidate. With `--arm` evidence adds `rank`: the winner 1, each
  `--runner-up` in the order given 2, 3, ..., any other arm null.
  Runner-ups must be bound arms, distinct and not the winner (card C5).
  `--rule` and `--judge` are §4. Selecting an arm whose chosen submission
  is a registered seeded candidate (contracts-review.md §8) is refused
  before any write (`arm N's candidate … is a seeded candidate: a seeded
  arm is an evaluation artefact and never a group's winner`,
  `candidate_groups::refuse_seeded_winner`): a seeded candidate never
  integrates, so as the winner it would hold every other arm forever
  (TM3.5 finding 3, card D11). The same refusal applies to `--judge`; the
  rule skips such an arm (§4).
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

**Integration and dependency hold** (owner decision 1; no schema change).
A submission whose attempt is bound to an arm of a candidate group and that
is not its group's selected submission is *held* (`candidate_groups.rs`
`HELD_ARM`): every arm while the group has no selection, every arm forever
after `no_selection`, and every loser (and any other submission of the
winning arm) after a selection. A held submission:

- is not eligible for automatic integration: the producer's eligibility
  (`integration_jobs.rs` `ELIGIBLE` + `NOT_SEEDED` + `AND NOT HELD_ARM`,
  the last only when the 0053 tables exist) drops it from the pending
  projection;
- is refused by `begin_integration` before any write (`a candidate-group
  arm integrates only as its group's selection`), so `result integrate`
  fails with no lease, no integration operation and no work directory;
- never satisfies a dependent's `verified_result` edge
  (`satisfaction.rs` `verified_counts`, which every release reads:
  `scheduler inspect` blockers, reservation evidence, waits). The
  satisfaction row may still be recorded; it counts only once the
  submission is the selection.

Recording a `selected` selection (operator, rule or judge) lifts the
winner's hold and, in the same transaction, inserts it into
`pending_integration_work` if it has a verified result and no integration
job or operation, so automatic integration picks it up, and moves its
task's `verified_result` edges to it (below). Tasks outside a group are
unchanged. Raw SQL is not guarded (below). `show` reports `integration_hold: {enforced: true,
integrated_without_selection: [...]}`, the list keeping any arm whose
candidate integrated before the hold existed.

**Selected winner and attempt currency.** A dependent's `verified_result`
edge requires the predecessor's *current* result (`satisfaction.rs`
`current_clause`, read by `verified_result_may_replace`, `current_verified`
and `verified_counts`). Normally that is a result of its current attempt
(active, else latest). Once a group of the predecessor's task and the
result's contract revision has a `selected` outcome, the current result is
exactly that selection's submission, whichever attempt it belongs to: a
winning arm 1 counts after arm 2 ran later, and nothing else of that
revision replaces it (a losing arm verified after the selection, another
submission of the winning arm, or an unbound retry that is now the latest
attempt). Recording the selection moves each consumer's edge to the
winner's current result in the same transaction
(`satisfaction::attach_selected`); a winner verified after the selection
takes the edge when its result is stored. Every other rule is unchanged:
latest contract revision, memory fence and invalidations, policy binding,
barriers, and the seeded guard (a selected seeded candidate still never
counts, contracts-review.md §8). Without a selection (open or
`no_selection`) attempt currency is unchanged, and held arms never count.

**Completion.** `task complete` (`request_completion`) marks the task
succeeded from an accepted submission of its active attempt, which would
end the task's remaining arms. It skips a seeded candidate (`a seeded
candidate never completes its task`) and, when the task has a group for
the submission's contract revision, any submission other than that group's
`selected` one (`a candidate group's task completes only from its selected
submission`, `candidate_groups::completion_hold`); it uses the first
remaining accepted submission, or refuses before any write.

**Raw SQL.** None of these holds is enforced by triggers: a raw
`candidate_selections`, `dependency_satisfactions`, `pending_integration_work`
or `operations` write can still integrate an arm, record a satisfaction or
request completion. The holds live in the store's readers and writers;
triggers would be a schema change (not made; follow-up).

## 4. Selectors and paired outcomes (TM3.8, card C4)

Plan doc 06 §6c, doc 07 M41/M42 and §6, doc 10 §5a. No schema change: the
0053 `selector_kind` check already allows `operator`, `rule` and `judge`, and
each selection is written through the store in one canonical transaction
(`src/store/candidate_groups.rs`). Selection is still not verification (§3).

**Arm outcome** (`arm_outcome.v1`), the verified outcome of one arm now
(one read-only store helper, `store::arm_outcome`, used by the selectors
and by every lane C report):
unbound arm `not_launched`; else, over all of its attempt's submissions by
`(created_unix_ms, rowid)` with the §3 combined verification, `accepted` if
any is accepted; else `pending` if any is pending or the attempt is not
terminal (`completed`, `failed`, `cancelled`, `lost`), since it may still
submit; else `rejected` with a submission, `no_candidate` without.

**Rule selector** `select <group> --rule`: rule
`first_accepted_in_launch_order.v2` (card D11), principal
`rule:first_accepted_in_launch_order.v2`. An arm is *eligible* when its
outcome is `accepted` and its first accepted submission is not a registered
seeded candidate (contracts-review.md §8); an accepted arm whose candidate
is seeded is skipped, with evidence `rule_skip: "seeded_candidate"` and
rank null. The first eligible arm in launch order wins with that
submission, reason `first_passing_verification`; a tie between eligible
arms goes to launch order. It refuses while any arm before the winner
(skipped arms included) is `pending`. With no eligible arm it closes the
group with no selection (`none_acceptable`, or `no_candidate` when no arm
submitted) only when every arm is bound and settled; otherwise it refuses
and the operator can still `--none`. Evidence adds `rank` (the eligible
arms in launch order, winner 1; null otherwise), so the runner-up order is
recorded for rule selections. The same canonical rows always give the same
answer.

Version `first_accepted_in_launch_order.v1` (principal
`rule:first_accepted_in_launch_order.v1`) was the same rule without the
seeded skip: it could select a seeded arm that passed verification, which
never integrates while the clean verified arm stays a held loser (TM3.5
finding 3). `--rule` no longer records v1; selections recorded under it
keep their principal and read, count (M41, M42) and hold exactly as before.
Test: `seeded_recall_and_the_seeded_candidate_guard_end_to_end`
(`tests/quality_certification.rs`): arm 1 seeded and verified, arm 2 clean
and verified; the operator's and a judge's selection of arm 1 are refused,
v2 selects arm 2, which releases the dependent and integrates.

**Judge selector** (no model is called anywhere):
`present <group>` (read-only) prints each bound arm's first candidate as
`{position, submission_id, repository, base_oid, candidate_oid}` — no arm,
attempt, configuration or profile — ordered by
`sha256("candidate_presentation.v1:" + group_id + ":" + submission_id)`.
`select <group> --judge NAME --submission ID [--runner-up ID]...
[--judge-configuration CONFIGURATION_ID]` records the judge's choice of a
presented candidate: `selector_kind = 'judge'`, principal `judge:NAME`,
reason `judge_preference`, evidence adds each arm's `presented` position
(null when it had no candidate), so the blind order the judge saw is stored
with the selection. The judge names its runner-ups by presented submission
(it never sees arms), in order; evidence `rank` is as for `--arm` (§3).
`--judge-configuration` (a contracts §2 ID, `sha256:` + 64 lowercase hex,
checked by format and recorded by value) is stored as
`judge_configuration_id` on every evidence entry (null when not given). No
schema change: `evidence` is JSON, and 0053 caps it at 8 array entries (one
per arm), so selection-level facts ride on each arm entry. Choosing a judge
configuration from another provider family, and running it, is not built.

**Metrics** (`telemetry <slug> quality groups report [--since MS]
[--min-groups N]`, read-only over `state.db`; also in `quality report`,
`telemetry <slug> report` and the fleet pane at the default threshold).
Cohort: closed groups (with a selection row), windowed by
`selected_unix_ms`; `closed_groups` and `open_groups` are reported. Every
sealed arm is a member, launched or not. A cell with fewer than `min_groups`
(the §5 registry minimum; `--min-groups N` overrides it for this report)
closed groups is `unavailable: insufficient_data` with its counts still
shown; each metric and the `groups report` carry `min_sample` (§5); no closed groups is
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
  `both_accepted`, `a_only`, `b_only`, `neither`. The top-level `value`
  lists shown pairs with `a < b`.
- **M42 uncertainty** (`percentile_bootstrap.v1`, card C5; plan doc 07 §6),
  per pair, unavailable (`insufficient_data`) exactly when the value is,
  and `single_task` when all `n` groups are on one task. Clusters are whole
  *tasks* (every paired group of a task, e.g. its groups for several
  contract revisions, moves together), each `(D, N)` = (sum of a − b
  acceptance, paired groups), in task ID byte order; `k` clusters. The
  generator is SplitMix64 (`state += 0x9e3779b97f4a7c15; z = (z ^ z>>30) ×
  0xbf58476d1ce4e5b9; z = (z ^ z>>27) × 0x94d049bb133111eb; z ^ z>>31`,
  wrapping), seeded with the registry seed and restarted for every pair; an
  index in `0..k` is `x mod k` after rejecting `x ≥ 2^64−1 − (2^64−1 mod k)`.
  Each of `B` iterations draws `k` indices and yields `ΣD/ΣN`. The `B`
  fractions are sorted exactly (cross-multiplied integers, ties by
  denominator) and the nearest-rank percentiles `ceil(B × 25/1000)` and
  `ceil(B × 975/1000)` are reported: `lower`/`upper` as percentage points to
  two decimals (integer rounding half away from zero) and
  `lower_difference`/`upper_difference` as the exact `"ΣD/ΣN"`, with
  `method`, `resample: "task"`, `clusters`, `iterations`, `seed` (hex),
  `level` `"0.95"` and `source: "registry.v1"`. No float enters the
  interval. `task_family` is `unavailable: no_task_family_data`: no task
  family is recorded, so tasks are the widest clusters. M42 also carries
  `estimator` {`method`, `resample`, `task_family`}.

## 5. Metric registry (`registry.v1`, card C5)

One declared, read-only table (`src/telemetry/quality/registry.rs`); a
change is a new registry version. Reports show each minimum as
`min_sample: {value, unit, source: "registry.v1"}`; `quality groups report
--min-groups N` shows `{value: N, unit, source: "override", registry:
{value, source}}`. `telemetry <slug> report`, `quality report` and the fleet
pane always use the registry.

| metric | minimum | unit |
|---|---|---|
| M41 | 10 | closed groups (per configuration or pair cell) |
| M42 | 10 | closed groups containing both arms, non-pending |

M42 estimator: `percentile_bootstrap.v1`, `B = 1000`, seed
`0x4d34325f626f6f74`, level 0.95 (§4). The other lane C metrics (M45–M48)
declare no minimum.

## 6. DG6 verification flakes and opt-in policy evidence

DG6a–c are passive telemetry. DG6d/e add opt-in executable policy version 2,
with honest evidence reruns and bounded concurrent stress; see
[verified-results.md](../factory/verified-results.md#evidence-reruns-and-stress-dg6de).
A first failure always blocks acceptance even if an evidence rerun passes.
Stress targets write paths, transactions, locks, migrations and ticker changes;
it is never inferred or enabled by telemetry.
`quality collect` scans at most 10,000 new canonical runs per call into
quality stream version 4 (base migration `0003_verification_flakes.sql`,
observations migration `0004_verification_observations.sql`); the
existing quality lane tick scans at most 256. These are the only paths that
derive flake rows: migrations create empty tables and unrelated commands
never backfill them. A sidecar rebuild must replay the same quality collection
steps before comparing reports. An immutable canonical insertion-row
cursor (`quality_verification_cursor`) advances over excluded runs too,
atomically with the projections, making collection incremental and replay
idempotent without repeatedly scanning infrastructure-failure history. The canonical
connection is read-only. `quality_verification_runs` stores run identity,
source insertion row, tree/object format, policy id/digest, verdict/time,
load and test availability. `quality_test_results` stores only run id,
sanitized name and outcome. No output bodies cross the database boundary.
These tables are `sidecar.derived_projections` in maintenance, retained
without their own TTL, rebuildable from canonical records and included in
ordinary sidecar backups. Canonical evidence follows the canonical lifecycle.

`quality flaky [--since MS]` is read-only. `verification_flip_rate.v1`
(`analytics-registry.v4`, family proxy, activity window, since-only provider)
is also in `quality report`, `telemetry report` and `query --metric
verification_flip_rate`. Group key is `(object_format, tree_oid, policy_id,
policy_digest)` within this project. Numerator: groups containing accepted
and rejected checks. Denominator: groups with at least two eligible runs.
One pair counts once regardless of repeat count or verdict alternations.
No denominator is unavailable, never zero. An absent sidecar or a stream created by another collector remains `verification_not_collected`
until the quality collector records its first scan, even when empty.
`by_policy` carries the exact
ratios separately for each policy id and digest. Windowing uses completion
record time; both runs must be in the requested window. Health uses a
half-open last-30-days window. This is passive evidence conditional on
reruns, not an estimate of all possible flaky candidates.

Eligible runs are accepted or rejected with reason `checks_failed` and a
known candidate tree. Cancelled executions produce no canonical verdict.
Timeouts, isolation/setup failures, policy mismatches, missing outputs,
scope checks and tree-tampering rejections are excluded: they do not attest
a completed execution of the same checks. Different trees, policy ids or
policy digests never combine. A submission with unknown tree is excluded.
`flips` lists up to 100 pairs, each with total completed runs and one
representative run id/verdict of each kind; identities are evidence, never
metric labels. The metric counts all pairs even when evidence is capped.

`failure_rate_by_load` is adjacent to the flip ratio, over the same eligible
runs: buckets `<2`, `2-8` (inclusive 2 and 8), `>8`, and `unknown`; failures
are `checks_failed` / runs, exact `n/d`. Unknown load is not zero. The
verifier samples `/proc/loadavg` and `/proc/pressure/{cpu,io}` immediately
before the isolated command, storing decimal strings (contracts §0), with null and `unreadable_or_invalid` on missing
or malformed input. PSI is `some avg10`, not `full` or the cumulative total.
Concurrency includes this execution and other verifiers for this project
currently holding execution slots (not queued jobs, worker attempts or
other projects). A 1024-slot OFD byte-lock file works across processes and
releases locks on close/crash. Slot exhaustion or unavailable kernel/file
support gives null with `execution_slots_unavailable`; no polling process
or subprocess is introduced. Sampling is an instantaneous observation,
not an average or a claim that host load measures project-local CPU use.
Early setup/policy rejection has no execution metadata.

Test outcomes are parsed by the verifier from command stdout: stable
libtest `test NAME ... ok|FAILED|ignored` (summary lines ignored), libtest
JSON test events, or a restricted JUnit XML document. JUnit combines
`classname::name` when classname is present; failure/error is fail, skipped
is ignored. Failure text and system-out/system-err are discarded. Five
predefined XML entities are supported; DTDs, custom entities, other XML
processing declarations and CDATA are refused. Nesting is capped at 64;
attributes at 64 per element. Unsupported/malformed output is unavailable,
not an empty passing suite, and never changes the verification verdict.

Input is capped at 2 MiB (truncation refuses the entire result set), at most
5,000 unique tests, raw names at most 256 UTF-8 bytes with no controls.
Names use the existing privacy sanitizer (home prefixes, tokens and URL
queries redacted; 160-character excerpt cap) and remain at most 256 bytes.
Duplicate or sanitization-colliding names refuse the entire result set.
The serialized test map is capped at 1.5 MB; canonical metadata at 2 MiB.
No partial prefix of an oversized or malformed suite is reported as complete.
Historical NULL metadata is unavailable. `quality flaky.tests` lists up to
100 names whose pass/fail outcome differs across eligible runs of the same
key; ignored and absent tests do not count as flips. This attributes a
verdict signal; it does not establish the underlying race or cause. The
existing `flaky_tests.proxy-v1` newly-flaky-test metric stays unavailable:
DG6 has no temporal baseline proving a test is *newly* flaky.

### DG6d/e repetition record and projection

Executable policy version 2 accepts `rerun_on_failure` 0–2 (default 0),
`named_checks`, and optional `stress` with named `checks`, `repetitions` 1–6,
`concurrency` 1–5 background processes and optional `load` argv. Including the
foreground execution, at most six check/load processes run concurrently.
Invalid bounds, missing names and use of these additions without explicit
version 2 fail signed contract put. Policies without these three additions
retain historical ingress compatibility, including prose and unversioned JSON
`checks`; their signed bytes and digests are preserved unchanged. Executable
validation still applies when the verifier runs a policy. There are no default
stress steps or quarantine decisions.
All commands retain the original program allowlist, namespace, post-check tree
proof and one shared timeout/cancellation budget.

`verification-metadata.v2` preserves the top-level load sample and primary tests,
and adds `observations` in execution-start sequence order. Each entry carries
`sequence`, `kind`, `check`, `repetition`, `source_sequence`, `outcome` (pass/fail/cancelled or
unavailable), `exit_status`, `load` and `tests`. Flake repetition numbers count
additional attempts; `source_sequence` links them to their first failure
(and is null for other entries). Stress/load repetition numbers count batches. Load context
is sampled before each spawn in the same namespace. The parent's slot remains
held for the whole run; child samples cannot read the project slot lock and
report project concurrency unavailable. The per-observation stdout/parser caps
are 8 KiB / 1 KiB serialized tests, with explicit unavailable reasons. Output
bodies and check stderr never enter metadata. A cancelled caller still records
no verdict; deadline exhaustion retains a timeout rejection and completed or
cancelled observations, excluded from flip statistics.

Quality migration 0004 creates `quality_verification_observations` and
`quality_observation_tests`, plus union views for reports. Both tables belong to
`sidecar.derived_projections`: no independent TTL, rebuildable from canonical
metadata and included in ordinary sidecar backup/restore. Migration resets only
the derived cursor, so explicit collection replays immutable metadata; unique
parent/sequence identities make repeated collection idempotent. It neither
creates observations nor changes canonical verdicts.

Only completed `flake` entries from otherwise eligible runs enter these
projections. Their report identity is `PARENT_RUN_ID:flake:SEQUENCE`; the parent
is a canonical run and the suffix is an observation, never a receipt. Grouping
and time window use the parent's exact tree, object format, policy id/digest and
completion time. The original failure and a passing observation therefore
supply a rejected/accepted evidence pair to the existing flip ratio and test
attribution. Here `accepted` is the derived passing-observation value, not a
canonical acceptance. Primary/stress/load entries do not add duplicate run
exposure to the metric. Readers of older sidecars preserve their existing
reports until explicit collection upgrades the stream.
