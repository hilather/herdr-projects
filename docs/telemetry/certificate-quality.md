# Quality and attribution certificate (TM3.5)

Plan card TM3.5 (plan doc 12), doc 10 §5 and §5a, doc 06 §7, doc 07 M20–M29
and M41–M48. Independent verifier's certificate over
[contracts-review.md](contracts-review.md) (lane D) and
[contracts-quality.md](contracts-quality.md) (lane C).

**What this certifies.** The *fixture certificate*: on deterministic fixtures
with a real SHA-256 Git repository, the canonical quality decisions and every
quality view derived from them agree with values computed by hand from the
plan's definitions, rebuild identically, and cannot be raised by a worker.
It does **not** certify any real producer (a live reviewer model, live
repairs, a real CI system or integration traffic). That is the separate
*producer certificate* (plan doc 10 §7, the TM5.2 quality-producer section),
and production-quality activation waits for it (§4).

- Source: branch `telemetry/tm35-quality-gate` from `main` at `3a8bb3e`, plus
  the fix in §3.1. Canonical schema 62, sidecar stream `quality`.
  Follow-ups (card D11, branch `telemetry/d11-quality-followups`, canonical
  schema 63): §3.2 lifted, §3.3 fixed, §3.5 fixed; the suite re-run green
  with the literals recomputed by hand for the two new ledger rows per
  review.
- Metric registry `registry.v1` (contracts-quality.md §5); review metric
  definitions `M20.v1`–`M29.v1`, `M43.v1`, `M44.v1`; lane C `M41.v1`,
  `M42.v1`, `M45.proxy-v1`–`M48.proxy-v1`.
- Evidence class: *deterministic fixture* (doc 10 §1). No live run, no account
  usage, no Herdr server other than the test stand-in.

## 1. Method

Suite `tests/quality_certification.rs` (8 tests), run with `cargo test
--features state-store --test quality_certification` (about 40 s at
`RUST_TEST_THREADS=6`). Rules the suite follows:

- **End to end.** Every write goes through the compiled CLI or the public
  store API: signed task contracts (`task contract put`), `result submit`,
  real isolated `result verify` runs whose acceptance policies really pass or
  fail on the candidate's content (`git grep` for the guard), real `result
  integrate` merges onto `refs/heads/integration`, a real regression commit,
  the `review`, `review findings|fixes|protocols|seeds`, `review authority`,
  `review accept` (reviewer keys and owner keys made with `ssh-keygen`) and
  `quality groups` commands. Only launch records that need a live agent are
  planted (reviewer and repair attempts, their dispatch decisions, candidate
  arm bindings), exactly as `admit_prepared` writes them. The malicious-worker
  test uses the real launch (`launch draft` → owner approval → `launch
  reserve`), the ticker, the worker sandbox and the submission spool
  (`tests/quality_certification/worker_lab.rs`, a trimmed copy of
  `canonical_worker.rs`'s lab).
- **Independent expectations.** Each test's doc comment lists the one
  ordering (ledger `seq` by `seq`) and the expected metric values, computed
  by hand from doc 07; the assertions compare the product to those literals.
  No expected value is read back from a product aggregate.
- **Rebuild check** (`assert_rebuilds`). At every checkpoint the head views
  (`findings show`, `fixes show`) and `review report` are captured. Afterwards
  each captured view is replayed with `--as-of SEQ` in the same project **and**
  in a fresh copy of the canonical store (`VACUUM INTO`, a new process, no
  sidecar), and must be identical. M22, M23, M25 and M26 are then recomputed
  from the replayed view by the plan's definitions and must equal the report
  captured at that watermark, and attribution must reconcile (per finding and
  role, shares sum to `allocated`, `allocated + unallocated = 1`; M21 = the
  discovery credit of F, `value + unallocated = |F|`, its configuration cells
  sum to its value). Proxy rebuild: `quality collect` twice, deleting
  `telemetry.db` in between, gives identical M45–M48, and `state.db` is
  byte-identical before and after collection.
- **Assertions are not outcomes.** Worker claims (`claimed_checks`, receipt
  fields, a worker's completion) are checked to stay proposals in every view
  until an owner decision, a native verifier receipt or a delegated
  acceptance exists.

| test | doc 10 / card item |
|---|---|
| `repair_lifecycle_on_a_real_repository_matches_hand_computed_views` | §5 real repository with known defects, fixes, integrations and a regression; duplicate reports under different titles (one finding, shared discovery credit); fix verification never transfers to a changed branch; a failed candidate; a verified-but-unintegrated fix; verify, integrate, reopen after a real regression and a new repair cycle; separate role credit, fixer not the introducer, mixed credit split; a completed security review with zero findings versus missing review data; partial (timed-out) review; rebuild at 8 watermarks; proxies M45–M48 |
| `split_merge_and_unmerge_corrections_recompute_denominators_and_keep_history` | §5 first denominator fixture (mixed 1 / rejected-only 1 → M22 50 %, M23 0 %; merge → 0 %/50 %; unmerge restores under a new as-of); three validated claims in one report = one validated submission; a verified fix never copied to split or merged findings; restore reverses a split; rebuild at 9 watermarks |
| `repair_cohorts_keep_failed_cancelled_and_reassigned_opportunities` | §5 second denominator fixture (A 1/2, B null not 100 %), cancelled and censored opportunities, effective-model history, earned credit never moving eligibility |
| `worker_assertions_and_self_approval_never_become_accepted_outcomes` | §5 forged verifier receipt kept as an assertion; forged receipt fields; worker, reviewer, author and import principals; delegated acceptance scope (kind), subject key, self- and author-approval |
| `seeded_recall_and_the_seeded_candidate_guard_end_to_end` | §5a seeded review (M43 75 %, M44 50 %) with the starter seeds; seeded candidate never integrates, releases or completes; a seeded group arm is never selected (operator and judge refused, rule v2 picks the clean arm, which integrates and releases); exact as-of replay of seeds, seed report and review views, opportunities included (D11) |
| `selection_that_disagrees_with_verification_verifies_and_releases_nothing` | card: candidate group where selection and verification disagree; M41 counts selections, M42 verified outcomes; a worker selecting its own arm (§3.1) |
| `skeptical_yield_counts_only_new_findings_on_the_same_artifact` | §5 skeptical review against prior coverage of the exact artifact; changed artifact, timed-out pass, severity floor, retraction |
| `a_sandboxed_worker_cannot_elevate_its_own_report` | card: malicious worker elevating its own report, through the real isolated wrapper and spool |

The lane suites remain the lanes' own evidence and were re-run unchanged
(`telemetry_review`, `telemetry_quality`, `review_signer`); this certificate
does not rely on their expected values.

## 2. Metric families

Status: **certified-fixture** (fixture certificate passes; production use
still waits for §4), **restricted** (fixture-certified with a named gap that
limits what the value may claim), **unavailable** (no producer; the product
reports `unavailable`, never 0).

| metric | status | evidence (hand-computed, asserted) | production gate |
|---|---|---|---|
| M20 review completion | certified-fixture; basis `declared`, trust `proposal` | 3/5 with completed-empty 1, timed-out 1, no-session 1, unassigned outside; a timed-out session never counts | live reviewer producer (D9 launch with a real agent); M20 counts worker-declared completions without acceptance, so it stays labelled `declared` |
| M21 validated unique findings (discovery credit) | certified-fixture | 2 (C 2), shared 1/2 + 1/2 keeps 2 (B 1/2, C 3/2, participation 3), retraction restores; 3 with 3 seeded-evaluation findings left out; reconciliation at every watermark | real triage volume; observational only |
| M22 proposal validation rate | certified-fixture | 2/4, 1/2 → 0/2 → 1/2 (merge, unmerge), 2/3 after a three-way split, 3/4 without seed-linked claims; pending always outside | real reviewers' reports |
| M23 duplicate-report share | certified-fixture | 1/4, 0/2 → 1/2 → 0/2 | as M22 |
| M24 review discovery efficiency | restricted: cost | closed cohort and numerator (accepted O1 only, 1; awaiting acceptance 2, awaiting adjudication 1) | TM2.6 accounting certificate and non-fixture rate cards; non-Codex reviewers stay `no_usage_bound` |
| M25 verified-fix rate | certified-fixture | findings 1/2 → 2/2; A's cohort 1/1 → 2/2, reassigned 1; fixture B: A 1/2, B null (`empty_denominator`, censored 1), C 0/1 (cancelled) | repair attempts are bound by the owner (`fixes bind`), not at launch |
| M26 currently resolved rate | certified-fixture | 1/2 → 0/2 after a real regression reopen; A's cohort 1/2 → 0/2; the reopened repair stays in its cohort | as M25; a reopening needs the owner's accepted occurrence |
| M27 reopen rate | certified-fixture | censored 1 within the horizon, 1/1 after the reopen | real integration history |
| M28 skeptical incremental yield | certified-fixture; descriptive only | pending-triage exclusion, 1/1 (1 new, 1 rediscovered, 1 below floor), not-completed and changed-artifact exclusions, retraction → null; `causal: unavailable` | a preregistered randomized experiment for any causal claim (TM4.4) |
| M29 attribution coverage | certified-fixture | 8/11 → 9/11 (introduction) → 10/11 (mixed implementation split) | real triage and repair volume |
| M41 candidate win rate | certified-fixture | A 1/2, B 1/2, head-to-head 1/2, counted from selections (one disagreeing with verification) | min 10 closed groups (registry) |
| M42 paired acceptance difference | certified-fixture | A−B −100 pp, B−A +100 pp over verified outcomes, never the selection; interval method from the lane test | `task_family` unavailable; min 10 groups |
| M43 seeded recall | certified-fixture | 3/4 = 75.00 (R 3/4), suppressed below the default 20 trials | seeds on replay-suite tasks (TM4.6 injection tooling) |
| M44 clean-control false-alarm rate | certified-fixture | 1/2 = 50.00; a validated incidental finding is no false alarm | as M43 |
| M45 first-candidate CI pass (proxy) | certified-fixture (proxy) | 2/3 on real runs; a worker's claimed check is `pending`, then 1/1 after the native run | pinned CI on real tasks; proxy only |
| M46 main breakage (proxy) | unavailable: `no_main_check_producer` | reported unavailable, labelled proxy | a main-branch check producer |
| M47 code survival (proxy) | restricted: censoring only | both real integrations censored (value null); survival arithmetic on matured history is lane evidence only (`revert_within_horizon_counts_recent_censored`) | integrations older than the horizon |
| M48 revert rate (proxy) | restricted: censoring only | as M47 | as M47 |
| `flaky_tests` (proxy) | unavailable: `no_repeat_runs` | reported unavailable | repeated pinned-suite runs |

Every proxy carries `proxy: true` and `source_trust: proxy_observed`;
collecting them never wrote `state.db` and never changed a fix, finding or
acceptance view.

## 3. Disagreements and resolutions

1. **Product bug, fixed: a worker could select its own candidate-group arm.**
   `SqliteStore::select_candidate` (and `create_candidate_group`) accepted any
   principal, so `worker:<attempt>` or the arm's own attempt id recorded a
   selection (observed: `selector_principal: "worker:g-a1"`), which lifts the
   integration and dependency hold (contracts-quality.md §3) and queues the
   arm for integration. `quality groups create|select` also had no worker
   execution-context guard, unlike every owner `review` command
   (contracts-review.md §9). Plan: selection only by an authorized selector
   (doc 06 §6c), and a worker never elevates its own work (doc 06 §2). Fix
   (lane C, minimal): `candidate_groups::selector_authority` refuses
   `worker:*`, any attempt's identity and `import:*` as creator, selector or
   judge name, before any write; `quality groups create|select` refuse the §9
   worker markers (`review::refuse_owner_cli_in_worker_context`, shared with
   the review CLI, messages unchanged). Regression tests:
   `a_worker_cannot_create_a_group_or_select_its_own_arm`
   (`tests/telemetry_quality.rs`) and
   `selection_that_disagrees_with_verification_verifies_and_releases_nothing`;
   documented in contracts-quality.md §3.
2. **Restriction (lifted by card D11): opportunities and assignments carried
   no sequence.** As-of views (`review show --as-of`, `seeds show --as-of`,
   `seeds report --as-of`) listed opportunities opened after the watermark
   (never completed there), so a historical M43 `not_completed` could
   include later opportunities. *Resolution* (canonical migration 0063,
   schema 63, contracts-review.md §9): `review open` and `review assign`
   each take the next `seq` of the shared review ledger
   (`review_opportunity_log`, `backfilled` at upgrade and then visible at
   every watermark, like 0059's sessions), and every `--as-of` review, seed
   and protocol view lists an opportunity from its opening and its
   assignment from its assignment row. The certification test now asserts
   exact replay instead of the restriction: `seeds show`, `seeds report`
   and `review show` replayed at 6, 9, 12, 37, 44 and 47 equal the views
   captured there, with M43/M44 `(trials, pending, not_completed)` by hand
   (as of 9, S1's opportunity alone, open: M43 `not_completed` 1 where the
   restricted view counted 4), and `assert_rebuilds` replays `review show`
   exactly at every watermark of the lifecycle and denominator fixtures.
3. **Finding (fixed by card D11): the rule selector could select a seeded
   arm.** `first_accepted_in_launch_order.v1` picked the first accepted
   arm; a registered seeded arm that passed verification won, could never
   integrate (seeded guard), and the verified clean arm became a held loser
   forever (checked on the real integration path: both `result integrate`
   calls were refused). Safety held; liveness of the group's task was lost.
   *Resolution* (contracts-quality.md §3–§4): rule version
   `first_accepted_in_launch_order.v2`, the one `--rule` now records, skips
   an accepted arm whose candidate is a registered seeded candidate
   (evidence `rule_skip: "seeded_candidate"`, no rank); v1 selections stay
   readable as recorded. An operator's or judge's selection of a seeded arm
   is refused before any write (a seeded arm is an evaluation artefact,
   never a winner). Test `seeded_recall_and_the_seeded_candidate_guard_end_to_end`:
   arm 1 seeded and verified, arm 2 clean and verified; `--arm 1` and a
   judge's choice of arm 1 are refused with nothing written; v2 selects
   arm 2 (rank 1; arm 1 skipped), which releases the dependent
   (`admission_disabled:verified_result` only) and integrates on the real
   integration branch, while arm 1's `result integrate` is still refused.
4. **Observation: a merge never carries a fix.** Merging a finding with a
   verified fix into another leaves the fix on the merged source, so the
   group root counts as unverified (M25 fell from 1/5 to 0/4 in the fixture)
   until the owner records a fix on the root. This is contracts-review.md §6
   by design and matches doc 06 §3 (no copying without coverage validation);
   the repair opportunity stays visible in its cohort.
5. **Observation (fixed by card D11): `review report` read each ledger view
   on its own read-only connection**, so a write between the reads could
   give one report two watermarks. `review report` (and the lane metrics
   hook of `telemetry <slug> report`) now reads every review metric in one
   read transaction and states its one watermark as the top-level
   `as_of_seq` (also on M20, M22–M24, M28, M43, M44); every captured report
   of the certification suite asserts `as_of_seq` = the ledger head it was
   captured at, on M20 and M22 alike.

No disagreement was found in M20–M29, M41–M45 or M43/M44 values: every
hand-computed value matched, and every captured view rebuilt identically.

## 4. Activation conditions for production quality metrics

Accepted production-quality activation (plan doc 06 §7, doc 10 §7) waits for
all of the following; until then quality values are fixture-certified and
shown with their `basis`/`trust` labels, and missing producers stay
`unavailable`:

1. **Producer certificate** (TM5.2 quality-producer section, separate from
   this certificate): real review sessions launched through the D9 path with
   real agents and blind briefs; real reviewer receipts through the spool;
   real delegated acceptances (offline or the D10 signer) under narrow,
   short-lived grants; native verification receipts under real pinned CI;
   real integration receipts; observed owner triage and repair decisions on
   that traffic. Each field reported with its evidence class.
2. **Accounting certificate** (TM2.6) before M24 carries any value beyond
   `partial`/`unavailable`, with non-fixture rate cards; non-Codex reviewers
   need bound usage.
3. **Repair binding**: either binding at launch (not built) or the owner's
   `fixes bind` procedure recorded as the repair producer, so M25/M26 cohorts
   are complete.
4. **Minimum samples** from `registry.v1` (M41/M42 10 closed groups, M43/M44
   20 trials) and the observational labels (M21, M25–M29) kept on every
   surface; M28 stays descriptive without a preregistered randomized
   experiment.
5. **As-of history**: met on fixtures by card D11 (§3.2, schema 63).
   Opportunities and assignments recorded before 0063 are backfilled and
   visible at every watermark, so historical opportunity counts are exact
   only for history recorded at schema 63 or later.
6. **Seeded groups**: decided by card D11 (§3.3): a seeded arm is never a
   winner (rule v2 skips it; operator and judge selections are refused).
7. **Worker isolation**: canonical workers run isolated (read-only store,
   spool-only writes); the coordinator and legacy thread agents stay trusted
   by owner decision (contracts-review.md §12) and are outside this
   certificate.
8. **Proxies** (M45–M48) remain a separate, labelled family and never stand
   in for a validated-quality metric; M46 and `flaky_tests` need their
   producers.
