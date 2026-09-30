# Evaluation contracts: configuration comparisons and experiment reports (TM4.4)

Plan card TM4.4 (doc 12), doc 07 §2 and §6, doc 08 §6, doc 10 §5a. Common
rules are [contracts.md](contracts.md) §0. Code:
`src/telemetry/analytics/compare.rs`, `experiments.rs`, `estimators.rs`;
declared estimators in `registry.rs` (`COMPARISON`). Tests:
`tests/telemetry_compare.rs`. Everything here is read-only (both stores
opened strictly read-only), writes no sidecar stream and is **never read by
dispatch or admission**: a measured association never becomes a routing
decision or a causal claim.

## 1. `telemetry <slug> compare` (`analytics-comparison.v1`)

```
telemetry <slug> compare --metric M02[,M07] [--by configuration]
    [--cohort terminal_cohort|assignment_cohort] [--from MS] [--to MS] [--horizon-ms MS]
    [--task-class CLASS] [--seed N|0xHEX] [--json]
```

**Rejections** (exit 1, stderr `compare rejected: {code, ...}`):
`dimension_unsupported` (`--by` other than `configuration`),
`comparison_unsupported` (a metric or definition the registry does not
declare comparable: only `M02.cohort-v1` and `M07.cohort-v1`),
`ambiguous_cohort` (`completed_task`), `unknown_cohort`, `cohort_unsupported`
(`activity_window`), `empty_window`, `horizon_unsupported`,
`horizon_out_of_range`, and the registry's `unknown_metric` /
`unknown_definition`.

**Cohort.** Membership is exactly the TM4.1 query service's
(`lifecycle::evaluate`, the lineage `query --drill outcome.<d>` pages):
terminal cohort by default, failed, cancelled and succeeded-without-evidence
tasks included; assignment cohort with unfinished tasks in the denominator.
Open tasks and the query's other exclusions are reported, plus
`task_class_filter` for `--task-class`.

**Arm.** The unit is the task. Its arm is the one `AgentConfiguration` every
attempt was dispatched on (`dispatch_decisions.chosen_configuration_id`). A
task dispatched on several configurations (`mixed_configuration`), with an
attempt that has no decision (`configuration_unknown`, e.g. before
migration 0050) or with no attempt (`not_assigned`) is a cohort member in no
arm (`population.unallocated`, note `unallocated_tasks`): its success and
cost are never counted in several arms.

**Output** (`--json`): `schema_version 1`, `contract`, `registry`,
`request` (normalized), `analysis {kind: observational, causal: false,
routing: never ...}`, `estimators` (§2, with the seed and its `source`),
`population {cohort, members, allocated, unallocated, exclusions,
coverage, censored}`, `configurations[]`, `results[]`, `paired`, `notes[]`,
`source_watermarks {canonical {lifecycle_digest, decisions,
configurations}, sidecar}`. Text: one line per arm cell and the notes.

`configurations[]`, one per arm (config ID order):

| field | meaning |
|---|---|
| `label`, `product`, `environment` | `"<kind> <agent_version>"`; adapter id/revision; `environment_names`, permission policy id/revision (contracts §2 components; digests are not repeated) |
| `declared_capabilities` | `requested_model`, `reasoning_effort`: `{status: declared, value}` or `unavailable` with the configuration's own reason (`mapping_unverified`) |
| `model_identity` | §4 |
| `assignments` | `tasks`, `attempts`, `decisions`, `chooser_kind` counts |
| `outcomes`, `attempt_states` | every disposition (accepted, failed, cancelled, succeeded_without_evidence, unfinished) and every attempt state, not only successes |
| `task_classes`, `difficulty` | class counts; preassignment rubric band counts (`task_classifications.band`, latest revision) |
| `cost` | bound usage (contracts §5) summed over **all** the arm's attempts, failed and cancelled included: `state` complete / partial / unavailable, `known`, `missing`, `reasons`, `total_tokens` (a partial value is a labelled `subtotal`, never the total). Monetary valuation stays M04/M12 (fixture-only rate cards) |
| `review_coverage` | tasks with at least one review opportunity / tasks, or `unavailable: review_capture_absent` |
| `model_allocation` | from the sidecar session graph and model segments: `single_model_tasks`, `mixed_model_tasks` (a session switched or mixed models), `unobserved_tasks`, flag `mixed_model_allocation`; `unavailable` (`collection_not_run`, `accounting_not_synced`) otherwise. Model names are withheld |

`results[]`, per metric: `cells[]`, one per task class present (cells never
mix classes), each with `arms[]`, `propensity` and `ranking`; and
`all_classes {arms, ranking}` (raw and interval only; never ranked). An arm
cell: `tasks`, `numerator`, `denominator`, `breakdown`, `difficulty`,
`status` (`shown`, `suppressed`, `empty`), `value` (unreduced `"n/d"`),
`decimal` (4 places), `interval`, `pooled`, `propensity_weighted`.

**Notes** (`notes[].code`): `failures_included` (with the cohort
breakdown), `unallocated_tasks`, `uneven_review_coverage` (arms' coverage
fractions differ, compared exactly), `cost_partial`, `mixed_model_allocation`,
`dispatch_log_absent`.

## 2. Declared estimators (`registry.rs` `COMPARISON`, also `metrics registry --json` → `comparison`)

- **Uncertainty** `percentile_bootstrap.v1` (card C5's generator and rank
  rule, contracts-quality.md §4), `B = 1000`, seed `0x544d345f636d7072`,
  level 0.95. Clusters are whole tasks: each task contributes
  `(numerator, denominator)` — M02 `(accepted, 1)`, M07 `(attempts,
  accepted)` — in task-ID order, so a five-attempt task moves as one unit
  and cannot shrink the interval. SplitMix64 restarts from the seed for every
  cell; each iteration draws `k` tasks and yields `ΣN/ΣD`; draws are sorted
  exactly (cross-multiplied integers; a zero denominator — an undefined M07 —
  sorts last and reads `"unbounded"`); bounds are the nearest-rank draws
  `ceil(B·25/1000)` and `ceil(B·975/1000)`, as `"ΣN/ΣD"` plus a 4-place
  decimal. Fewer than two tasks: `unavailable: single_task`. `--seed`
  overrides the seed, recorded as `{source: override, registry: {...}}`.
  Same sources and seed give the same bytes.
- **Minimum sample**: 20 terminal tasks per configuration × class cell
  (plan doc 07 §6). Below it the cell is `suppressed`: `value`, `interval`,
  `pooled` and `propensity_weighted` are `unavailable: insufficient_data`,
  counts and breakdown still shown, `min_sample {value, unit, source}`.
  Pooling never un-suppresses a cell.
- **Pooling** `beta_binomial_eb.v1` (M02 only; M07 answers
  `pooling_not_declared`): per arm, prior mean = the arm's own all-class rate
  `Y/N`, prior strength `κ = 10` pseudo-tasks; the class estimate is
  `(y + κ·Y/N)/(n + κ) = (y·N + κ·Y)/((n + κ)·N)`, exact, with `shrinkage =
  κ/(n + κ)`. It is shown beside the raw rate, never instead of it.
- **Propensity** `hajek_ipw.v1`: only when every task in the class cell has
  a first dispatch decision (earliest `decided_unix_ms`, then attempt ID)
  whose `eligible[].probability_ppm` is positive for every compared arm.
  Per arm `Σ wᵢ·nᵢ / Σ wᵢ·dᵢ` over its tasks with `wᵢ = L/pᵢ` (`L` the least
  common multiple of the arm's probabilities in ppm, exact integers), with
  `effective_sample_size = (Σw)²/Σw²`. Otherwise the cell's `propensity` is
  `unavailable`: `deterministic_assignment` (every decision chose its arm
  with probability 1 — the rule's, contracts §3; a policy assigning under
  §9 logs positive propensities) or `positivity_violated`. Weighted estimates stay
  `observational`.
- **Paired analysis** `paired`: lane C's M42 (`percentile_bootstrap.v1`
  over tasks, its own minimum of 10 closed groups, `registry.v1`) from the
  same read path as `telemetry report`, restricted to pairs of the compared
  arms, groups selected at or after `--from`.

## 3. Rankings

A class cell's `ranking` is `intervals_separated` with `order` (best first
by the metric's direction: M02 higher, M07 lower is better), `scope: "this
task class and cohort only"`, `universal: false`, `observational: true`,
`causal: false`, `routing: never ...` only when all hold: at least two arms,
every arm shown (none suppressed), every interval available, the arms'
difficulty-band mixes equal (exact proportions), and consecutive arms'
intervals separate strictly. Otherwise `{status: not_supported, reasons}`
with `single_arm`, `{insufficient_data: [arms]}`, `interval_unavailable`,
`difficulty_mix_differs` or `intervals_overlap`. `all_classes.ranking` is
always `not_supported` with `universal_ranking_not_supported` (plus
`task_class_unmatched` when classes differ): incomparable units never
produce a ranking. A ranking is descriptive evidence for a person; nothing
consumes it.

## 4. Product-level results and hidden model identities

The arm is the product configuration. `model_identity` is `{status:
declared, requested_model}` only when the configuration's canonical JSON
declares `requested_model`; otherwise `{status: opaque, reason}` (today
always `mapping_unverified`): the result is about the product
configuration, not a model. Effective model names observed in sessions
(sidecar `model_segments`) are **never** listed per arm — only counts of
single- and mixed-model tasks — so a comparison cannot reveal or rank a model
a product hides; M15 remains the place for effective-model coverage.

## 5. Observational scope

`analysis.kind` is always `observational`, `causal: false`: production
assignment is not randomized, harder tasks may go to stronger arms, and
controlling recorded covariates does not remove selection bias (doc 07 §6).
Causal labels exist only in §7.

## 6. `telemetry <slug> experiments plan` (`experiment-planning.v1`)

```
experiments plan --metric M --baseline-rate p1 --min-detectable-effect d
    [--alpha 0.10|0.05|0.01] [--power 0.80|0.90|0.95] [--discordance psi]
    [--cluster-size m --icc rho] [--json]
```

Reads nothing. `M` must be a registry metric of unit `ratio`
(`not_a_rate_metric`); `p1`, `d`, `psi`, `rho` are decimals with at most
nine places; `p2 = p1 + d < 1` (`baseline_out_of_range`,
`effect_out_of_range`, `unsupported_alpha`, `unsupported_power`,
`discordance_out_of_range`, `icc_out_of_range`). z quantiles (two-sided α,
one-sided power) are fixed nine-place constants: 1.644853627, 1.959963985,
2.575829304; 0.841621234, 1.281551566, 1.644853627.

- **Unpaired** (two proportions, equal arms):
  `n = ⌈(z_α·√(2·p̄·q̄) + z_β·√(p₁q₁ + p₂q₂))² / d²⌉` per arm, `p̄ = (p₁ + p₂)/2`;
  `total_tasks = 2n`.
- **Paired** (candidate groups containing both arms; McNemar, Connor 1987):
  `n = ⌈(z_α·√ψ + z_β·√(ψ − d²))² / d²⌉` groups, `ψ` the discordant-pair
  share; without `--discordance`, `ψ = p₁(1 − p₂) + p₂(1 − p₁)`
  (`assumed_independent_arms`, the no-pairing-benefit bound). Positive
  within-task correlation lowers `ψ` and the paired size.
- **Clustered** (optional): design effect `1 + (m − 1)ρ`; each size
  multiplied and rounded up.
- **Arithmetic**: integers in units of 10⁻⁹; every square root is
  `floor(√·)` (integer Newton), every product floors, only the final size is
  a ceiling. Golden: `p₁ 0.5, d 0.1, α 0.05, power 0.80` → 388 per arm, 391
  pairs; `ψ 0.2` → 155; `m 5, ρ 0.05` → 466 per arm, 186 pairs.

The output repeats the registry display minima (20 tasks per cell, 10 paired
groups) as floors, not power guarantees, and states that a plan is not
evidence: the study must be preregistered (§7) before any outcome.

## 7. `telemetry <slug> experiments report [--as-of SEQ]` (`experiment-report.v1`)

Reads the D4 preregistered experiments (contracts-review.md §7,
`store::protocol_state`, replayed to `--as-of`) and adds, per experiment:
`preregistered` (the frozen definition), `assignment_seed`,
`analysis: intention_to_treat` (units stay in their assigned arm;
`crossover` and `treatment_not_received` counted, never moved), `arms` (the
D4 per-arm counts: assigned, analyzable, pending, censored, excluded by
reason, crossover, outcome total, mean), `units.by_arm_status`, and per
non-reference arm the D4 `value` plus:

- `interval`: randomized — `percentile_bootstrap.v1` stratified by arm,
  clusters = tasks of the arm's analyzable units (a unit's task is its
  review opportunity's task), each iteration drawing `k₀` reference then
  `k₁` treatment clusters from one SplitMix64 stream, difference
  `(Y₁N₀ − Y₀N₁)/(N₁N₀)` exact, `resample: task_within_arm`; matched —
  complete blocks grouped by the reference unit's task, `ΣD/ΣN`,
  `resample: block_by_task`. Registry seed and `B`; `single_task` with fewer
  than two clusters per arm; `insufficient_data` exactly when the D4 value
  is.
- `causal`: `{status: causal_estimate, design, analysis, scope}` only for a
  randomized or matched design whose difference meets the preregistered
  `min_units`; otherwise `unavailable: insufficient_data` with `min_units`.
  The scope is the assigned arm's effect on the preregistered primary
  outcome among eligible units — not a model ranking, never read by
  dispatch.
- `label`: `causal` when any difference is a causal estimate, else
  `descriptive`.

D4's own `experiments show` keeps `uncertainty: unavailable:
interval_not_computed`; the interval lives in this report.

## 8. Restrictions and not built

- Only the native lifecycle definitions M02 and M07 are comparable; review,
  paired and cost metrics are compared through their lanes (M42 is carried
  in `paired`). Registering another comparable metric is a new
  `analytics-comparison` version.
- Positive propensities are logged only by decisions a policy assigned
  (§9, operator switch on and an owner grant); rule, operator and delegated
  decisions log probability 1 for their choice.
- Arms are not matched on role, repository or tool versions beyond the
  configuration identity and task class; difficulty mismatch blocks a
  ranking, the other covariates are shown, not adjusted.
- No M50 (evidence freshness): it is per recommendation, and recommendations
  are TM4.5.
- Reports are computed live; no revisioned projection is stored.

## 9. Assignment policies (TM4.7, `assignment-policy.v1`)

Code: `src/domain/assignment_policy.rs` (pure policies),
`src/store/assignment_policy.rs` (switch, grants, assignment record,
canonical migration 0065), `src/admission.rs` (`decide_held`, `weigh`,
`assign`, `policy_suggestion`), `src/telemetry/policies.rs` (inputs, shadow
stream `policies`, CLI). A policy never builds launch inputs, approvals,
contracts or review/verification policy: it only picks one of the profiles
automatic admission already sealed and found approved for the task, and the
reservation then runs every ordinary check (budget, capacity, contract,
knowledge, review launch).

**Policies** (versioned; a spec is canonical JSON with defaults filled, its
identity `sha256:` of those bytes). Input: the approved arms in evaluation
order, each with quota headroom, decisions under the current settings
revision (caps) and recorded outcomes in the task's class; a seed. Output:
per-arm `probability_ppm` (exact integers summing to 1000000) and the choice.
- Constraints first: an arm whose `arm_caps` entry is reached
  (`arm_cap_reached`) or whose known remaining quota is at or below
  `min_headroom_percent` (default 0; `quota_exhausted`) has probability 0.
  Unknown headroom never excludes. No arm left: `abstained:
  no_arm_within_constraints`, no choice.
- `deterministic.v1`: the first allowed arm, 1000000.
- `uniform.v1`: `1000000 / k` each, the remainder one ppm each to the first arms.
- `epsilon.v1 {epsilon_ppm}`: `epsilon_ppm` split as uniform, plus
  `1000000 − epsilon_ppm` to the greedy arm (highest posterior mean
  `(s+1)/(s+f+2)`, exact, earliest on ties).
- `thompson.v1 {prior [α, β] (default 1/1), floor_ppm (default 10000)}`:
  1000 seeded posterior draws per decision; a Beta(a, b) draw with integer
  parameters is the a-th smallest of a+b−1 uniform u64s (integer-only);
  counts above 200 outcomes are scaled to 200. Each allowed arm gets
  `floor_ppm` plus a largest-remainder share of the rest by wins.
- Seed: first 8 bytes (big-endian) of
  `sha256("assignment-seed.v1\0<policy digest>\0<task>\0<task revision>")`;
  the draw is SplitMix64's first output mod 1000000; the choice is the first
  arm whose cumulative probability exceeds the draw, so the logged
  probabilities are exactly the ones the choice was made with.
- Outcomes: the query service's lifecycle tasks (`lifecycle::load`); a task
  counts for the one configuration all its attempts were dispatched on
  (mixed/unknown arms skipped), accepted = success, other terminal
  dispositions = failure, open tasks not counted. Quota headroom: the
  smallest fresh remaining percent of the arm's execution home at the
  decision (contracts-accounting.md §5 M40), else unknown.

**Switch** (`assignment_policy_settings`, append-only; no row = `off`):
`telemetry <slug> policies configure --mode off|shadow|suggest|assign
[--policy SPEC]... [--grant ID]`. The first policy is primary.
- `shadow`/`suggest`: admission weighs every matched profile, but the
  canonical decision is byte-for-byte the rule's (first approved chosen,
  later profiles `not_evaluated`); each configured policy's evaluation is
  appended to sidecar `policy_shadow_decisions` after the commit.
- `assign`: requires an installed owner-signed grant, re-verified at
  enablement, permitting the primary policy now. Admission evaluates the
  primary with effective caps = min(spec cap, grant cap) (an arm the grant
  does not list: cap 0), logs its probabilities on the decision
  (`DispatchContext::Assigned`) and writes `dispatch_policy_assignments`
  (policy, digest, spec, seed, draw, grant, constraints). The reservation
  transaction re-checks: settings revision still current `assign` with the
  same grant and primary; grant valid and permitting; chosen arm within its
  cap (decisions choosing it since the settings revision ≤ cap); a trigger
  requires the logged probabilities to sum to 1000000 with the chosen entry
  positive. When the primary abstains or the grant no longer permits, the
  candidate is not reserved (`policy_abstained`); switching `off` returns
  to the rule.

**Authority** (`randomized_assignment_authority.v1`, namespace
`randomized-assignment@herdr-projects`, factory F2.5): `{schema, scope:
randomized_assignment, issuer: owner, project_store, policies (sorted),
max_exploration_ppm (bounds epsilon_ppm), arm_caps {configuration_id: 1–100000}
(1–16 arms), valid_from_unix_ms, expires_unix_ms (≤ 366 days),
prohibited_effects [alter_review_policy, alter_verification_policy,
choose_outside_eligible_set, exceed_budget, increase_permissions],
authority}`. `policies authority import DOC SIG` verifies the owner
signature first; it enables nothing.

**Reports** (read-only): `policies show` (switch, history, grants,
assigned count); `policies simulate --input FILE [--seed N] [--sweep K]`
(what-if on stated arms; a sweep counts choices and `outside_eligible`);
`policies suggest --task T [--json]` (`assignment-policy-suggestion.v1`:
the task's weighed profiles and each policy's evaluation;
`default_suggestion` in `suggest`/`assign`); `policies shadow [--json]`
(`assignment-policy-shadow.v1`: per policy decisions, agreements,
disagreements, abstentions, `disagreement_rate` `"d/n"` over non-abstained
decisions, per class; canonical decisions by chooser as `shadowed`,
`single_profile_chooser` (operator/delegated), `policy_assigned` or
`policies_off`).
