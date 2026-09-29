# Review capture contracts (lane D)

Owned by lane D ([phase2-lanes.md](phase2-lanes.md)); common rules are
[contracts.md](contracts.md) §0 and §7. Plan doc 06 §2–§6a (TM3.1–TM3.4,
TM3.6), doc 07 §5/§5b (M20–M29, M43, M44), doc 10 §5. Canonical migrations
`0054_review_capture.sql` (schema 54), `0055_finding_triage.sql` (schema 55,
§5), `0056_fix_attribution.sql` (schema 56, §6) and
`0057_review_protocols.sql` (schema 57, §7), `0058_seeded_defects.sql`
(schema 58, §8) and `0059_review_ledger.sql` (schema 59, §9); store API
`src/store/review_capture.rs`, `src/store/finding_triage.rs`,
`src/store/fix_attribution.rs`, `src/store/review_protocols.rs`,
`src/store/seeded_defects.rs` and `src/store/review_ledger.rs`; CLI
`telemetry <slug> review ...` (`src/telemetry/review/`). Sidecar stream
`review` has no tables yet. Nothing about reviews comes from the Codex
adapter (contracts-collection.md A3).

## 1. Records (TM3.1, card D1)

Four separate, append-only canonical records (every UPDATE/DELETE aborts), so
"no findings" differs from "no review happened", and planning, execution and
the end of a review never overwrite one another. Metadata and references
only: no source, diff, brief, prompt or review text is stored anywhere.

- **Opportunity** `review_opportunities`: a planned review of one exact
  candidate. `submission_id` (a `result_submissions` row) fixes `task_id`,
  `contract_revision` and `candidate_oid` (the commit, which fixes the tree);
  a trigger refuses any other combination. Also `scope`
  (`candidate_diff`, `candidate_tree`, `contract_scope`), `kind` (method, not
  rank: `code`, `skeptical`, `security`, `test`, `architecture`), `role`
  (`gate`, `evaluation`, `advisory`: evaluation cohorts are what the blind
  policy governs), `protocol` (a lowercase versioned identifier, e.g.
  `review-protocol.v1`), `prior_findings` (sorted distinct `finding:<token>`
  references known before the review, ≤64), `budget_ms` (nullable),
  `creator_principal`. `opportunity_id` = `sha256:` over the canonical JSON
  `{budget_ms, candidate_oid, contract_revision, created_unix_ms, kind,
  prior_findings, protocol, role, schema:"review_opportunity.v1", scope,
  submission_id, task_id}`. Acceptance criteria are the contract revision's
  acceptance policies, bound through `contract_revision`.
- **Assignment** `review_assignments`: at most one per opportunity, before
  any session (sessions reference it). Reviewer `configuration_id`
  (contracts §2) and profile digest, the author attempt (the submission's)
  and its configuration from `dispatch_decisions` (null when it predates the
  log), provider families, `same_family`, `blind`, `policy`, `reason`,
  `eligible` (each weighed profile with its status), principal.
- **Session** `review_sessions`: one execution by one attempt, recorded by
  the controller, never declared by the reviewer. `ordinal` per opportunity,
  `attempt_id` (must exist), its `configuration_id` from `dispatch_decisions`
  (null if none), `matches_assignment` (null when the configuration is
  unknown), `same_attempt_as_author`. A restart is a new session of the same
  opportunity, allowed only when every earlier session has a completion that
  is not `completed`; nothing starts after a completed session.
- **Completion** `review_completions`: at most one per session, from the
  reviewer's receipt `review_receipt.v1` `{schema, session_id, submission_id,
  candidate_oid, outcome, reason?, findings[], evidence[]}` (unknown fields
  refuse the receipt, ≤64 KiB). Since 0055 a `findings` entry may also be
  `{ref, title}` (§5); any other key in it refuses the receipt. `outcome` ∈ `completed`, `incomplete`,
  `failed`, `timed_out`, `interrupted`; a completed review has no reason,
  others one of `budget_exhausted`, `reviewer_error`, `scope_unavailable`,
  `operator_stopped`, `unspecified` (default). `findings` are
  `finding:<token>` references (≤256); `findings_submitted` is their count,
  and `0` is a valid completed review. `evidence` is `sha256:<hex64>`
  (content-addressed evidence held elsewhere) or
  `verification_run:<hex64>` (≤64). Every completion is `trust = proposal`
  with `coverage_basis = declared`. `receipt_digest` is over the normalized
  receipt: the same receipt replays (`replayed: true`), a different one for
  the same session refuses.

**Exact candidate.** A receipt whose `submission_id` or `candidate_oid`
differs from its session's opportunity is refused (store check and
trigger), and writes nothing. Review of a later candidate needs its own
opportunity.

## 2. Proposal versus acceptance

A completion (and anything a worker writes) is a proposal. Acceptance is the
separate table `review_acceptances(session_id, decision, authority_principal,
authority_ref, decided_unix_ms)`. No canonical reviewer-authority producer
exists (the only delegated review action is `review_memory`), so acceptance
is **inactive**: a trigger aborts every insert, and
`SqliteStore::accept_review` / `review accept` refuse every caller, writing
nothing. A worker principal (`worker:*`, the reviewing attempt or the
authoring attempt) is refused as a worker before that. A receipt carrying an
acceptance field (`accepted`, `trust`, ...) is refused as an unknown field.
`show` reports `acceptance {active: false, reason:
no_reviewer_authority_producer}` and each completion's `acceptance` as
unavailable with that reason. Activating acceptance needs the authority
producer (scoped grant, principal distinct from the proposer) and a new
migration that replaces the trigger.

## 3. Blind cross-provider assignment (`blind_cross_provider.v1`)

Deterministic, no model call. `review assign <opportunity> --blind
--candidate P ...` weighs 1–16 retained native profiles (latest retained
report of each name; distinct configurations). Family (`product_family.v1`)
is the configuration's product vendor: `codex` → `openai`, `claude` →
`anthropic`, any other kind unknown; model providers are not collected, so
this is not a model-family claim. With the author family known, the pool is
the candidates of a known, different family; if none, every candidate. The
chosen reviewer is the pool member with the least
`sha256("review_assignment.v1:" + opportunity_id + ":" + configuration_id)`.
`reason`: `cross_provider`, `no_cross_provider_eligible` or
`author_family_unknown`. `same_family` is `true`/`false` when both families
are known, else `null`; it is a covariate, and same-family reviews are
excluded from blind comparisons unless an analysis names them. `blind` is
`true`; `review present <opportunity>` is the blind view (exact candidate,
repository, base, scope, kind, protocol, prior findings, budget; no author
attempt or configuration). No review brief builder exists yet, so blindness
of an actual brief is not enforced. `--reviewer P` records an operator
assignment (`policy = operator`, `blind = false`, reason
`operator_selected`), with the same covariates.

## 4. Commands and metrics

`telemetry <slug> review open|assign|start|complete|accept` write through
`SqliteStore`, one `state.db` transaction each, principal `operator:cli`;
nothing touches `telemetry.db`. Since 0059 `start` and `complete` also
append one shared-ledger row each (§9), and every `review` command but
`present` refuses to run in a worker execution context (§9). `show [--since MS]` (by creation), `present`
and `report [--since MS]` read `state.db` with contracts §0 reads.

Per opportunity `status`: `unassigned`, `no_session`, `in_progress` (a
session without completion), `completed` (some session `completed`),
`ended_without_completion`. `findings_submitted` is the completed session's
count, else `unavailable` with the status as reason: a missing session is
never 0.

- **M20 review completion** `M20.v1` (`basis: declared`, `trust:
  proposal`): completed / assigned opportunities, windowed by
  `assigned_unix_ms`, as `"n/d"`; `status` counts (`completed`,
  `completed_empty`, `ended_without_completion`, `in_progress`,
  `no_session`), `unassigned` (outside the denominator, windowed by
  creation), `by_kind_protocol` `"kind/protocol"` → `"n/d"`. Incomplete,
  failed, timed-out or interrupted sessions never count as completed; an
  empty denominator is `null` with `empty_denominator`; a store before 0054
  `unavailable: review_capture_absent`.
- **M22 proposal validation rate** and **M23 duplicate-report share**
  (`M22.v1`, `M23.v1`, `basis: owner_triage`, `trust: operator_owner.v1`):
  §5.
- **M21** (sum of discovery credit), **M25**–**M27** and **M29**: §6.
- **M28** skeptical incremental yield: §7.
  **M24** (M21 per review cost) is `unavailable: review_cost_unallocated`:
  review lifecycle cost (reviewer sessions, zero-find and failed reviews,
  triage) is not allocated to review opportunities, the accounting lane's
  per-attempt estimates are published-rate estimates from fixture rate cards,
  and non-Codex reviewers have no collector, so no honest cost of `Q` exists.

Also in `telemetry <slug> report` and the fleet pane through the lane metrics
hook.

## 5. Finding triage and duplicate history (TM3.2, card D2)

Canonical migration `0055_finding_triage.sql`; store API
`src/store/finding_triage.rs`; CLI `telemetry <slug> review findings
show|validate|reject|duplicate|reset|split|restore|merge|unmerge`.

**Authority decision.** A reviewer's report is always a proposal. The only
triage principal is `operator:cli`, the project owner at the CLI, recorded
with authority `operator_owner.v1`. The owner already holds every project
decision at the CLI (operator dispatch, operator candidate selection), so
this needs no delegation. No delegated or scoped reviewer authority exists
(§2); until one does, `SqliteStore` refuses every other principal, writing
nothing: a worker (`worker:*` or any attempt's identity, including the
reviewing and authoring attempts), an import (`import:*`), or any other name.
A trigger and CHECKs repeat the rule on raw rows. Review acceptance (§2)
stays inactive: triage decides findings, not reviews.

**Records**, all append-only (every UPDATE/DELETE aborts). One ledger,
`finding_log(seq, kind, principal, authority, expected_seq,
recorded_unix_ms)`, orders every change. Its `seq` is the replay watermark.
`kind` `submitted` has `authority = proposal`; every other kind needs
`operator:cli` with `operator_owner.v1`. Detail rows share the ledger's `seq`:

- **Submission** `finding_submissions`: one per `finding:<ref>` of a review
  completion, written in the completion's transaction (`submitted`).
  Completions recorded before 0055 are backfilled in completion order.
  `(session_id, finding_ref)` is unique. The reference must be in that
  completion's `finding_refs` (trigger). `source = review_receipt`, `trust =
  proposal`. The optional `title` is a contracts §7 excerpt (rules 1–5,
  `crate::domain::excerpt`, ≤160) of the receipt's `{ref, title}` entry. A
  title enters the receipt digest only when present, so untitled D1 receipts
  keep their digests. `review complete` returns `finding_submissions` ids.
- **Claims** `finding_claim_sets` + `finding_claims`: revisioned atomic claims
  beneath an unchanged submission. `initial` is revision 1 with one claim.
  `split` makes a new revision of 2–32 claims, each with an optional title
  excerpt. `restore` makes an earlier initial or split revision current again
  with its same claims, so their decisions return: a split is reversible.
  Only the current revision's claims can be decided.
- **Canonical finding** `canonical_findings`: identity `finding:canonical-<seq>`
  (the minting decision's `seq`, usable as a `finding:` reference in
  `prior_findings`). It is minted by a `validated` decision with `--new`, and
  its title is the owner's excerpt or the claim's.
- **Decision** `finding_decisions`: `validated` (a finding, ≥1 evidence
  reference, `severity` ∈ critical/high/medium/low/informational under
  `finding_severity.v1`), `rejected` (`insufficient_evidence`,
  `intended_behavior`, `out_of_scope`), `duplicate` (of an existing finding),
  or `pending` (a correction back to pending: `reopened` or
  `decided_in_error`). Evidence uses the §1 reference forms. A later decision
  on a claim supersedes the earlier one (`supersedes`) and never deletes it.
- **Relationship** `finding_relationships`: `merge` source → target (same
  root cause). It is refused when the source is already merged or both are
  already one group, so no cycle forms. `unmerge` reverses exactly one active
  merge.

`--expect-seq N` refuses any write unless the ledger head is still `N`.

**Derived state** (`finding_state(db, as_of)`, `review findings show
[--as-of SEQ]`) replays the ledger up to `SEQ` (default the head, refused
beyond it) on a strictly read-only connection. The same ledger gives the
same state at every sequence, so the current view and every historical view
are reproducible.

- Each submission's claims are its current claim revision at `SEQ`. Each
  claim's decision is its latest decision at `SEQ` (none means `pending`).
- Groups: a finding's root follows the merges active at `SEQ`. Among the
  claims validated into one root, the earliest (submission `seq`, then claim
  ordinal) is the discovery and stays `validated`. Every later one is
  derived `duplicate`, so several titles for one defect give one unique
  finding. `decided` keeps the recorded decision; `outcome` is the derived one.
- A submission is `pending` while any claim is pending. Otherwise it has one
  exclusive outcome: `validated_only`, `rejected_only`, `duplicate_only` or
  `mixed`. `has_validated_claim` means at least one validated claim, so a
  mixed submission can count. A split never adds submissions.
- A finding's `status` is `validated` (a root with a discovery claim),
  `merged`, or `unvalidated`. `unique_findings` counts the `validated` ones.
- `history` lists every ledger row up to `SEQ` with its subject.

**Metrics** (`review report`, `telemetry <slug> report`), over original
submissions windowed by arrival (`--since`):

- M22 = submissions with `has_validated_claim` / adjudicated submissions.
- M23 = `duplicate_only` / adjudicated submissions.

Both show `buckets` (the four outcomes), `pending` (outside the
denominator), `claim_drilldown` (labeled claim counts) and `as_of_seq`. An
empty denominator is `null` with `empty_denominator`, and a store before
0055 gives `unavailable: finding_triage_absent`. Merge, unmerge, split and
restore recompute the buckets without changing the number of submissions.

0056 extends this ledger's ordering (§6): `fix_log` rows take the next
`seq` of the same sequence, so `--expect-seq`, `--as-of` and `head_seq` of
`findings show` refer to the one ledger head, and `history` here still lists
only `finding_log` rows. Since 0059 a review session's start and its
completion are rows of the same sequence (§9): a completion's row precedes
the submissions written with it. Each claim also shows `seed_linked` at the
watermark, and M22/M23 leave seed-linked claims out (§9).

Not built here: conflict records between
submissions, imports of third-party review comments (any such producer can
only write submissions), and a scoped reviewer-authority grant that would let
a principal other than the owner triage.

## 6. Fix attribution, regressions and role credit (TM3.3, card D3)

Canonical migration `0056_fix_attribution.sql` (schema 56); store API
`src/store/fix_attribution.rs` (`fix_state`); CLI `telemetry <slug> review
fixes show|open|bind|propose|verify|integrate|close|reopen|introduce|credit|retract`.

**One ordering.** `fix_log(seq, kind, principal, authority, expected_seq,
recorded_unix_ms)` shares its sequence with `finding_log` (§5): every new row
of either ledger takes `head + 1`, where the head is the larger of the two,
and a trigger on each refuses a row that does not follow the other's head.
`fix_state(db, as_of)` / `fixes show [--as-of SEQ]` replays both ledgers to
one watermark, so a triage correction and an attribution decision are
ordered, and every earlier view is reproduced exactly (reads are contracts
§0 reads). Every table is append-only (every UPDATE/DELETE aborts).

**Authority.** As §5: every `fix_log` row is `operator:cli` with
`operator_owner.v1` (CHECKs), and `SqliteStore` refuses workers (`worker:*`
or any attempt's identity), imports (`import:*`) and any other principal
before writing. Each detail row needs its own `fix_log` row of the matching
kind (triggers).

**Records** (each row's `seq` is its `fix_log` row):

- **Repair opportunity** `repair_opportunities(seq, finding_id, assignment,
  configuration_id, profile_digest, policy, horizon_ms)`, `open <finding>
  (--assign PROFILE | --unassigned) [--horizon-days N]` (default 14). The
  finding must be a validated group root (§5) that is not currently resolved,
  with no other open opportunity. The initial assignment group (a retained
  profile's contracts §2 configuration, or explicitly `unassigned`, policy
  `repair_assignment.v1`) and horizon are frozen before any repair runs.
- **Attempt binding** `repair_attempts(seq, repair_seq, attempt_id UNIQUE,
  ordinal, configuration_id)`, `bind <repair> --attempt A`: only before the
  attempt's outcome is known (store: the attempt is not `completed`,
  `failed`, `cancelled` or `lost`; store and trigger: it has no
  `result_submissions` row) and only into an open opportunity. Ordinal 1 is
  the initial attempt; later ones are reassignments (`reassignment: true`),
  provenance only: the opportunity never leaves its initial group. The
  configuration is the attempt's dispatch decision (null if it predates it).
  Binding at launch would need launch/scheduler changes and is not built;
  it is recorded explicitly.
- **Fix proposal** `fix_proposals(seq, repair_seq, submission_id UNIQUE,
  attempt_id, candidate_oid)`, `propose <repair> --submission S`: a result
  submission of an attempt bound to that opportunity, at its exact candidate
  (trigger).
- **Verified fix** `fix_verifications(seq, proposal_seq UNIQUE, run_id,
  result_id, commit_oid, assurance, evidence_refs)`, `verify <proposal>
  --run R --assurance regression_reproduced|approved_alternative --evidence
  ...`: the owner's decision that an accepted native `verification_runs` row
  **of the proposal's own submission at its candidate commit**, with its
  `verified_results` row, repairs the finding (≥1 evidence reference, §1
  forms). A run of another submission or commit, a rejected run, or a run
  without a verified result refuses (store and trigger): passing checks on
  one commit never verifies another, and worker claims are never evidence.
- **Integration** `fix_integrations(seq, proposal_seq UNIQUE,
  verification_seq, integrated_id UNIQUE, commit_oid, integrated_unix_ms)`,
  `integrate <proposal> --integrated ID`: only a verified proposal, and only
  an `integrated_commits` row whose operation integrated a verified result of
  the same submission and candidate and whose integration candidate's
  `parent_verified` is that candidate (store and trigger). A fix verified
  only on its branch is not integrated.
- **Closure** `repair_closures(seq, repair_seq UNIQUE, outcome)`: `fixed`
  (needs a verified fix), `no_fix` or `cancelled` (need none). A closed
  opportunity takes no more attempts, proposals or verifications, and stays
  in its cohort.
- **Reopening** `fix_reopenings(seq, finding_id, integration_seq, reason,
  observed_oid, evidence_refs)`, `reopen <finding> --reason
  regression|reverted --observed OID --evidence ...`: an accepted occurrence
  at an exact revision (the defect remains or returned, or the revert
  commit) of a currently resolved finding. It ends the current resolution
  (`ended_by: reopened`) and deletes nothing: the fix's proposal,
  verification and integration stay. C2's proxy revert observation
  (contracts-quality.md §2) is never read by this path; the owner may cite
  it as evidence. A new repair cycle is a new opportunity.
- **Introduction** `introduction_decisions(seq, finding_id, status, method,
  introducing_oid, evidence_refs)` + `credit_shares`, `introduce <finding>
  (--commit OID --method M [--contributor ATTEMPT=N/D]... |
  --unattributable) --evidence ...`. Without a decision a finding is
  `unattributed` (unknown, never exoneration). `method` is
  `controlled_reproducer`, `reliable_bisect` or `minimized_patch`; `blame`,
  `last_editor`, `temporal_proximity` and `fixer` are refused as inference. A
  contributor must be an attempt with a result submission at exactly the
  introducing commit, and never through its own fix proposal for this
  finding: the fixer is not charged with introducing what it repaired.
  `detection_oid` (shown) is the candidate the discovering review examined.
- **Credit allocation** `credit_allocations(seq, finding_id, role,
  proposal_seq, policy, evidence_refs)` + `credit_shares(seq, attempt_id,
  share_num, share_den)`, `credit <finding> --role discovery|implementation
  [--proposal P] --share ATTEMPT=N/D ... --evidence ...` (policy
  `owner_allocation.v1`). Shares are exact fractions with denominators 1–16;
  one allocation sums to at most 1 (store exactly; trigger exactly in units
  of 1/720720); the remainder stays unallocated. Discovery contributors are
  reporters of the finding's validated or duplicate claims; implementation
  contributors are attempts bound to the verified fix's opportunity. The
  latest unretracted allocation of a (finding, role, proposal) applies.
- **Retraction** `fix_retractions(seq, reverses UNIQUE)`, `retract <seq>`:
  reverses one credit allocation, reopening or introduction decision recorded
  in error (not a reopening followed by a later integration of the finding).

**Derived state** (`fixes show`), per canonical finding at the watermark:
`status` (§5), `remediation` (`resolved`, `fix_verified`, `fix_proposed`,
`reopened`, `repair_open`, `unrepaired`; the fix states count only after the
last reopening), `verified` and `integrated` (ever: historical),
`currently_resolved`, `resolutions` (one interval per integration, ended by
`reopened` or `superseded` by a later integrated fix), `reopenings` (with
`retracted_seq`), `repairs`. Per repair: assignment, attempts,
proposals with verification and integration, `closure` and `outcome`
(`currently_resolved`, `integrated`, `verified`, `proposed`,
`no_candidate`). Fixes stay with the finding they were recorded on: a merge
or unmerge never copies a fix onto another finding.

**Role credit** (validated roots only; merged and unvalidated findings have
none). Each role is `{policy, source_seq, shares[{contributor, attempt_id,
configuration_id, share}], allocated, unallocated, unallocated_reason}`,
contributors `attempt:<id>`, `principal:<name>` or `service:<name>`; one
role of one finding never exceeds 1.

- discovery: `earliest_validated.v1` (§5's discovery claim's reporter, 1)
  unless the owner allocated shares (reason for a remainder
  `shared_discovery_unallocated`).
- validation: `triage_decision.v1`, the validating decision's principal.
- implementation, of the fix resolving the finding now, else its latest
  verified fix: `sole_attempt.v1` gives 1 to the submitting attempt only when
  it is the only attempt bound before the proposal; otherwise nothing is
  allocated (`mixed_contribution_unallocated`) until the owner allocates.
- verification `service:native_verifier` (`native_verifier.v1`) and
  integration `service:integrator` (`integrator.v1`): services, never the
  implementer's model.
- introduction: as above; `unattributed` or `unattributable` leave 1
  unallocated.

**Metrics** (`review report [--since MS] [--horizon-days N]`, `telemetry
<slug> report`; `basis: owner_attribution`, `trust: operator_owner.v1`;
store before 0056: `unavailable: fix_attribution_absent`). `F` = validated
unique findings, windowed by their discovery submission's arrival; exact
credit sums are reduced fractions (`"3/2"`).

- `F` leaves out seed-linked findings (§9); M21, M25, M26 and M29 report
  them as `seeded_evaluation`.
- **M21** `M21.v1`: sum of discovery credit over `F` (`"0"` with no
  finding), `unallocated`, `by_configuration` (reporter attempts' dispatch
  configurations, `unknown` without one), `participation` (shares, never
  counted as discoveries), `drilldown.validated_unique_findings`,
  `observational: true`.
- **M25** `M25.v1` verified-fix rate and **M26** `M26.v1` currently resolved
  rate: `value` / `findings` are finding outcomes over `F` (ever verified;
  currently resolved). `by_assignment` are the initial-assignment cohorts
  `R_g` (repairs windowed by opening; group = frozen configuration or
  `unassigned`): a repair is eligible once closed or past its horizon, else
  `censored`; numerator: a verification (M25) or a current resolution
  integrated (M26) recorded within the horizon. Failed, no-fix, cancelled and
  reassigned repairs stay in their initial group (`not_achieved`,
  `reassigned`); a configuration that only appears as a reassigned attempt's
  gets `value: null` with `no_assigned_opportunities`. The two denominators
  are never combined.
- **M27** `M27.v1` reopen rate: integrations (windowed by integration)
  reopened within the horizon (default 14 days) of `integrated_unix_ms` /
  integrations reopened within it or observed for the whole horizon;
  `censored` the rest.
- **M29** `M29.v1` attribution coverage: allocated / eligible role credit per
  role (`by_role`: eligible = findings of `F` with that role) and overall, as
  exact reduced fractions.

Doc 07 labels these observational: assignment is not randomized, and M21 is
not model ability.

Not built: binding repair attempts at launch (needs launch/scheduler
changes); finding occurrences as a separate record beyond reopenings;
mixed-model segment splits within one attempt; artifact-only publication
contracts; M24 (review cost). M28 is §7.


## 7. Review protocols and experiments (TM3.4, card D4)

Canonical migration `0057_review_protocols.sql` (schema 57); store API
`src/store/review_protocols.rs` (`protocol_state`); CLI `telemetry <slug>
review protocols register|bind|retract|show` and `review experiments
register|assign|exclude|retract|show`.

**One ordering.** `protocol_log(seq, kind, principal, authority, expected_seq,
recorded_unix_ms)` takes the next `seq` of the §5/§6 sequence (triggers on all
three ledgers refuse a row that does not follow the others' head), so
`--expect-seq`, `--as-of` and `head_seq` of `findings show`, `fixes show`,
`protocols show` and `experiments show` refer to one head. Every table is
append-only. Every row is `operator:cli` with `operator_owner.v1` (CHECKs);
`SqliteStore` refuses workers (`worker:*` or any attempt's identity), imports
and any other principal before writing, as §5.

**No routing.** Nothing here opens, assigns, starts or launches a review, or
changes a budget. An experiment arm is a label on an opportunity the owner
opened; the owner still opens and runs any second review with the D1
commands. No metric here is read by dispatch, and none claims a causal effect
unless it comes from a preregistered comparison (below).

**Protocol** `review_protocols` (`protocols register --input-file F`, JSON
`review_protocol.v1`, unknown fields refused, ≤8 KiB). Tokens, enums and
numbers only: no prompt, brief or prose. A registered protocol never changes;
a changed method is a new versioned identifier. Example (the one the tests
register):

```json
{"schema": "review_protocol.v1", "protocol": "skeptical-challenge.v1",
 "kind": "skeptical", "scope": "candidate_diff", "role": "evaluation",
 "challenges": ["unsupported_claims", "missed_edge_cases", "unsafe_concurrency",
                "missing_acceptance_criteria", "evidence_gaps"],
 "failure_classes": ["logic", "boundary", "concurrency", "security",
                     "test_weakening", "requirement_omission"],
 "permitted_tools": ["read", "test"], "budget_ms": 1800000, "evidence_min": 1,
 "stopping_rule": "checklist_complete", "prior_disclosure": "withheld",
 "reviewer_profile": null,
 "outcome": {"primary": "new_validated_unique_findings.v1",
             "adjudication": "owner_triage.v1",
             "severity_policy": "finding_severity.v1", "min_severity": "low"}}
```

`kind`, `scope` and `role` are §1's enums; `challenges` (1–16) and
`failure_classes` (1–16) and `permitted_tools` (0–16) are lowercase
identifiers; `stopping_rule` ∈ `budget_exhausted`, `checklist_complete`,
`first_blocking_finding`; `prior_disclosure` ∈ `withheld`, `disclosed`
(whether the reviewer sees prior conclusions; disclosure changes the task, so
M28 reports it per protocol); `reviewer_profile` (optional) is a retained
profile whose contracts §2 configuration the pass's reviewer must be.
Stored canonically (sorted, the profile resolved to `reviewer_configuration_id`)
with `definition_digest`. A second example, same method with the prior
conclusions shown, is a different protocol:
`{"protocol": "skeptical-challenge-disclosed.v1", "prior_disclosure":
"disclosed", ...}`.

**Pass** `skeptical_passes` (`protocols bind <opportunity> --prior O ...`):
a D1 opportunity run under a registered protocol, declared as the second
review after 1–16 ordinary review opportunities of the same task. Refused
unless the opportunity's `protocol` is registered and its `kind`, `scope`,
`role` and `budget_ms` equal the protocol's (store and trigger), it was
opened after the protocol was registered, it has no session yet (store and
trigger: the cutoff precedes the review), and it is not already a pass.
Frozen at binding: `cutoff_seq` (the ledger head: every finding validated by
then, and the opportunity's declared `prior_findings`, is **known**); per
prior `{opportunity_id, submission_id, candidate_oid, scope, artifact,
status}`, with `artifact` `same_artifact` (same submission and candidate, same
scope), `changed_artifact` (another submission or candidate) or
`different_scope`; `comparability` (the worst of those); `prior_coverage`
`complete` only when every prior's §4 status was `completed`.

**Exact candidate.** A pass is an opportunity, whose identity includes its
exact candidate (§1). A later artifact therefore always has another
opportunity id, and its receipt cannot name the earlier candidate (§1
refusal); binding it after reviews of the earlier artifact labels it
`changed_artifact`, which M28 excludes.

**Incremental yield** (`protocols show [--as-of SEQ]`, per pass at the
watermark). Over the claims of the pass's completed session, by their §5
derived outcome, `incremental` is:

- `new`: the claim is its group's discovery (§5 `validated`), the group root
  was not known at the cutoff (known findings are re-rooted at the
  watermark), and its severity is at least the protocol's `min_severity`;
- `rediscovered`: a derived `duplicate`, including a reworded report the owner
  marked duplicate, or validated as new and later merged into an earlier
  finding. It shows reproducibility and is never a new discovery;
- `known` (validated into a group known at the cutoff), `below_severity_floor`,
  `rejected`, `pending`.

`new_unique_findings` are the distinct roots of `new` claims. A pass is
eligible for M28 unless (first failing rule is the `exclusion`):
`changed_artifact` / `different_scope`; `incomplete_prior_coverage`;
`retracted` (the owner retracted the binding, §9); `not_completed` (no
completed session: a timed-out pass has no yield);
`reviewer_mismatch` (the protocol names a reviewer configuration the
assignment does not match); `evidence_requirement_unmet` (the completion cites
fewer than `evidence_min` evidence references); `pending_triage` (a claim is
still pending). Review session statuses, findings, decisions, bindings and
retractions all replay to the watermark (sessions and completions are ledger
rows since 0059, §9).

**Experiment** `review_experiments` (`experiments register --input-file F`,
JSON `review_experiment.v1`), preregistered and frozen (no update path;
UPDATE/DELETE abort). Example:

```json
{"schema": "review_experiment.v1", "experiment": "skeptical-vs-standard.v1",
 "design": "randomized", "seed": "<64 lowercase hex>",
 "eligibility": {"kind": "code", "scope": "candidate_diff",
                 "role": "evaluation", "protocol": "review-protocol.v1"},
 "arms": [{"arm": "standard", "protocol": null},
          {"arm": "skeptical", "protocol": "skeptical-challenge.v1"}],
 "primary_outcome": "validated_unique_findings.v1",
 "adjudication": "owner_triage.v1", "horizon_days": 14, "min_units": 10,
 "stopping_rule": "fixed_horizon", "planned_units": 40}
```

A matched study sets `"design": "matched"`, no seed, and `"match_on"`
(1–16 identifiers the blocks are matched on, e.g. `["task_class",
"repository"]`). Rules: 2–4 arms, distinct names and protocols, arm protocols
registered, the first arm is the reference (typically the standard-only
control, protocol `null`); eligibility role is `evaluation` or `advisory`,
never `gate` (a required gate review is never withheld); `min_units` ≥ 2
(default 10, plan doc 07 §6), frozen before any outcome so it cannot be tuned
afterwards; `horizon_days` 1–3650.

**Unit** `experiment_units` (`experiments assign <experiment> <opportunity>
[--block B --arm A]`): one base review opportunity per exact artifact
(unique submission per experiment). **Exact eligibility**: the opportunity's
`kind`, `scope`, `role` and `protocol` equal the preregistration's (store and
trigger). **Before outcomes**: refused once any session of the opportunity has
a completion (store and trigger), so the unit's `seq` precedes every finding
submission of it. Randomized: the arm is `arms[u64(first 16 hex of
sha256("review_experiment.v1:" + seed + ":" + submission_id)) mod n]`
(reproducible from the recorded seed; `--block`/`--arm` refused). Matched:
the owner names `--block` and `--arm`; a block holds at most one unit per arm.

**Exclusion** `experiment_exclusions` (`experiments exclude <experiment>
<opportunity> --reason R`, R ∈ `ineligible_discovered`, `artifact_withdrawn`,
`protocol_violation`, `operator_error`): once per unit; the unit stays listed
in its arm with the exclusion, and leaves only the estimate. The exclusion
(`{seq, reason, retracted_seq}`) can be retracted (§9); a retracted
exclusion is not recorded again for the same unit.

**Crossover** is derived, never rewritten: a unit's passes are the passes
naming its opportunity as a prior. `treatment_received` (arms with a
protocol): a pass under the arm's protocol exists; `crossover`: a pass under
another protocol, or any pass in an arm without one. Analysis is by
intention to treat: a unit is always analyzed in its assigned arm.

**Unit outcome** (`experiments show [--as-of SEQ]`): the distinct §5
discoveries (derived `validated` claims) among the submissions of the unit's
base opportunity and its passes (all sessions), whose root was not known at
the unit's assignment. Status: `excluded`; `analyzable` when the base and
every pass have ended (`completed` or `ended_without_completion`), no claim
of theirs is pending, and a treatment arm's pass exists or the horizon has
passed; otherwise `pending`, or `censored` after the horizon. Pending and
censored units are outside the estimate, never 0.

**Estimate** (preregistered comparisons only): per arm `assigned`,
`analyzable`, `pending`, `censored`, `excluded` (by reason), `crossover`,
`treatment_not_received`, `outcome_total`, `mean` (exact `"y/n"`).
Randomized: each arm's difference from the reference, `y₁/n₁ − y₀/n₀` as an
exact reduced fraction, only when both arms have at least `min_units`
analyzable units. Matched: over complete blocks (one analyzable unit per
arm), the mean within-block difference, only with at least `min_units`
complete blocks. Otherwise `unavailable: insufficient_data` with the counts.
`uncertainty` is `unavailable: interval_not_computed` (no task-clustered
bootstrap yet, TM4.4).

**M28** `M28.v1` skeptical incremental yield (`review report [--since MS]`,
`telemetry <slug> report`; `basis: owner_triage`, `trust:
operator_owner.v1`; store before 0057: `unavailable:
review_protocols_absent`): over eligible passes bound in the window, `value`
= Σ `new_unique_findings` / eligible passes as `"n/d"` (findings per
opportunity; empty: `null`, `empty_denominator`), with `rediscovered`,
`excluded` (by reason), `by_protocol` (`value`, `prior_disclosure`),
`control_opportunities` (unexcluded units of arms without a protocol) and
`as_of_seq`. It is always `estimate: descriptive`, `observational: true`,
`causal: unavailable: not_randomized`: sequential reviews without a control
support descriptive yield, not an improvement percentage. `experiments`
lists each preregistered experiment's `estimate` (`randomized` or
`matched`), `analysis`, `reference_arm`, `differences` and `uncertainty`
beside it, never merged into the descriptive value.

Not built: assignment-cutoff binding at launch or by a scheduler (needs
routing changes); disclosure of prior conclusions in a review brief (no
brief builder, §3); blinding adjudicators to arm; task-clustered intervals,
sample-size planning and propensity weighting (TM4.4); enforcement of the
`fixed_horizon` stopping rule (recorded; `planned_units` is enforced, §9).

## 8. Seeded defects, recall and the integration guard (TM3.6, card D6)

Canonical migration `0058_seeded_defects.sql` (schema 58); store API
`src/store/seeded_defects.rs` (`seed_state`); CLI `telemetry <slug> review
seeds show|register|detect|retract|reveal|dispose|report`
(`src/telemetry/review/seeds.rs`).

**Authority.** Only the evaluation authority writes: `operator:cli`, the
project owner, recorded as `evaluation_owner.v1` on every `seed_log` row
(CHECKs). `SqliteStore` refuses a worker (`worker:*` or any attempt's
identity, so never a reviewer or the implementing worker), an import
(`import:*`) and any other principal, writing nothing.

**One ordering.** `seed_log(seq, kind, principal, authority, expected_seq,
recorded_unix_ms)` shares the sequence of `finding_log`, `fix_log` and
`protocol_log`: every new row of any ledger takes the common head + 1
(`finding_triage::head` over all four), and triggers refuse a row that does
not follow the others' head. `--expect-seq` and `--as-of` refer to that one
head. Every table is append-only (every UPDATE/DELETE aborts), and each
detail row needs its own `seed_log` row of the matching kind.

**Records.**

- **Evaluation candidate** `seeded_candidates(seq, submission_id UNIQUE,
  candidate_oid, arm, reveal_policy)`, `register <submission> (--seed
  CLASS=sha256:<hex64> ... | --control)`: the submission's exact candidate
  (trigger) as `seeded` or `clean_control`, policy `reveal_after_close.v1`.
  Registration is refused once any review opportunity exists on the
  submission (the arm is fixed before review) or once any integration job
  or operation names it (store and triggers).
- **Seed** `seeded_defects(seed_id, seq, ordinal, seed_class,
  reproducer_ref)`, 1–16 per seeded candidate, written with its
  registration. Class ∈ `logic`, `boundary`, `concurrency`, `security`,
  `test_weakening`, `requirement_omission` (plan doc 06 §6a). The minimal
  reproducer is only a `sha256:<hex64>` reference to content held
  elsewhere; a seed's source, patch or reproducer text is never stored or
  shown.
- **Detection** `seed_detections(seq, seed_id, claim_id, evidence_refs)`,
  `detect <seed> --claim C --evidence ...`: the owner's accepted link of one
  finding claim to one seed. The claim must be in a current claim set, its
  current triage decision must be `validated` or `duplicate` (§5), and it
  must come from a review of the seed's own candidate (store and trigger).
  A claim links at most one seed at a time. `retract <seq>` reverses a
  detection recorded in error.
- **Reveal** `seed_reveals(seq, submission_id UNIQUE)`: only when the
  candidate has at least one opportunity and every opportunity has
  sessions, each with a completion (store and trigger). After a reveal no
  opportunity or session on the candidate can be recorded (store and
  triggers).
- **Disposal** `seed_disposals(seq, submission_id UNIQUE, disposition)`:
  `discarded` or `repaired` (a repair is another submission), only after a
  reveal. Either way the seeded submission never integrates.

**Integration guard.** A submission registered with arm `seeded` never
reaches an integration target: the automatic producer's eligibility
(`integration_jobs.rs` `ELIGIBLE` plus `NOT_SEEDED`) drops it from the
pending projection, `begin_integration` refuses it before any write (`a
seeded candidate never integrates`), and triggers refuse any
`integration.run` or `integration.lease` operation and any
`integration_operations` row for it, so raw SQL cannot either. The guard
only removes eligibility; nothing else about integration changes. Clean
controls are not guarded (plan doc 06 §6a guards seeded candidates only).

**Dependency guard.** A seeded candidate's verified result never satisfies
a dependent task's `verified_result` edge: `satisfaction.rs`
`verified_counts`, the one check every release reads (`scheduler inspect`
blockers, reservation dependency evidence, dependency waits), returns
false for it (`seeded_defects::seeded_result`). The satisfaction row may
still be recorded when the result is verified, including before
registration; it never counts, even when the seeded submission is its
candidate group's selection (the selected-winner currency relaxation of
contracts-quality.md §3 does not lift this guard). `integrated_commit`
edges need an integration, which the integration guard already refuses.

**Completion guard.** `task complete` never marks a task succeeded from a
seeded candidate: `request_completion` skips it (`a seeded candidate never
completes its task`) and refuses before any write when no other accepted
submission of the active attempt remains. Clean controls complete
normally. Like the dependency guard, this is enforced in the store, not
by triggers.

**Blindness.** Reviewers never see seed state: `review present` reads no
registry table and refuses to print any field outside its twelve blind
fields; `review show` and blind assignment carry no seed state either.
Only the owner's `seeds show`/`seeds report` read the registry.

**Derived state** (`seeds show [--as-of SEQ]`), replayed with §5 triage to
one watermark: candidates with seeds, detections (`active` = unretracted
and the claim still `validated`/`duplicate` at the watermark), reveal and
disposal; the review opportunities of registered candidates (assigned
reviewer configuration, `completed` = some session completed, submission
outcomes); and the M43 trials: one per seed per completed opportunity of
its candidate, `detected` (an active detection by a claim of that
opportunity), `pending` (not detected while a submission of the
opportunity is pending), else `missed`. Since 0059 `--as-of` replays review
completions too (§9); opportunities and assignments carry no sequence, so
`not_completed` at an earlier watermark also counts opportunities opened
later.

**Metrics** (`seeds report [--since MS] [--min-trials N] [--as-of SEQ]`;
`review report` and `telemetry <slug> report` at the default `N` = 20, plan
doc 07 §6; `basis: owner_triage`, `trust: evaluation_owner.v1`, `scope:
seeded_work_only`; store before 0058: `unavailable:
seeded_defects_absent`). Windowed by the opportunity's assignment. Every
cell shows its counts and `pending` (outside the denominator); a zero
denominator is `null` with `empty_denominator`; fewer than `N` is
`unavailable: insufficient_data`; else `value` `"n/d"` and `percent` (two
decimals, half up).

- **M43** `M43.v1` seeded recall: `detected` / `trials`, overall,
  `by_configuration` (the assigned reviewer configuration, each with
  `by_seed_class`), `by_seed_class`, `by_kind_protocol`, and
  `not_completed` opportunities on seeded candidates (no trial, never 0).
  It says nothing about recall on unseeded, production defects.
- **M44** `M44.v1` clean-control false-alarm rate: completed clean-control
  opportunities with at least one `rejected_only` submission (§5) /
  completed clean-control opportunities; `pending` when none is
  rejected-only and one is pending. A validated incidental finding on a
  control is not a false alarm.

Incidental findings on seeded candidates follow the ordinary §5/§6 path:
they are triaged, counted in M20–M29 and repaired like any finding;
detection adds no triage row. A claim linked to a seed is an evaluation
artefact and leaves discovery and validation credit (§9).

**Starter seed set** (tests only): `tests/fixtures/telemetry/seeds/
starter-seed-set.json`, one synthetic seed per class over a tiny clean
source. Tests inject seeds only into disposable repositories they create;
no tool here modifies a real project repository.

Not built: injection tooling for replay-suite tasks (TM4.6); a reviewer
brief builder (§3).

## 9. Review ledger, seed-linked credit and the worker guard (card D7)

Canonical migration `0059_review_ledger.sql` (schema 59); store API
`src/store/review_ledger.rs`; CLI `telemetry <slug> review protocols
retract` and `review experiments retract`.

**Review lifecycle in the one ordering.** `review_log(seq, kind, principal,
authority, expected_seq, recorded_unix_ms)` takes the next `seq` of the
sequence shared with `finding_log`, `fix_log`, `protocol_log` and
`seed_log` (`finding_triage::head` over all five; triggers on each refuse a
row that does not follow the others' head). `kind` `started` and
`completed` are review capture's own records (`authority =
review_capture.v1`, the recorder's principal); `pass_retracted` and
`exclusion_retracted` are the owner's (`operator:cli`, `operator_owner.v1`,
CHECKs). `review_session_events(seq, session_id, event, backfilled)`:
`review start` appends `started` in the session's transaction and `review
complete` appends `completed` before the completion's finding submissions
(a completion follows its start: trigger). Views replay review status to
the watermark (`Lifecycle`): a session is present from its `started` row,
its completion from its `completed` row, so a restart, a timed-out session
and a completion show exactly where they happened relative to triage,
bindings and detections: §4 status in `protocols show`, `experiments show`
(unit settlement) and `seeds show`/`seeds report` (trials). Opportunities
and assignments carry no sequence (an assignment always precedes its
sessions). Every table is append-only.

Every review now adds two rows to the sequence before its submissions, so
sequence numbers and `finding:canonical-<seq>` identities of a store's later
history differ from what the same history gave before 0059; rows recorded
before the upgrade keep theirs.

**Upgrade.** Sessions and completions recorded before 0059 are sequenced
after the ledger head at upgrade, in start/completion order (by time, a
start before its completion, then session id), with `backfilled = 1`. Their
true place among earlier rows is unknown, so replay keeps them visible at
every watermark, which is the pre-0059 stored-status view; a trigger
refuses any later `backfilled` row.

**Seed-linked claims.** A claim with a detection (§8) recorded by the
watermark and not retracted by it is `seed_linked` (`findings show`). Such
claims are evaluation artefacts, not discoveries:

- M22/M23 derive each submission's outcome again from its other claims; a
  submission whose every current claim is linked leaves the denominator.
  `seeded_evaluation: {submissions, claims}` counts what was left out.
- `F` (§6) leaves out a validated finding whose §5 discovery claim is
  linked (`fixes show` `seeded_evaluation`), so M21, M25, M26 and M29 do
  too; each reports `seeded_evaluation` (findings). A linked duplicate claim
  of a real finding does not remove that finding.
- Incidental (unlinked) findings on seeded candidates stay ordinary.

A retraction (§8 `seeds retract`) returns the claim to ordinary credit from
its `seq` on; earlier views keep the link.

**Retractions** `protocol_retractions(seq, reverses UNIQUE)`: `protocols
retract <seq>` reverses a `pass_bound` row, `experiments retract <seq>` a
`unit_excluded` row, each recorded in error (trigger: the log kind matches
the reversed row's). A retracted pass stays listed with `retracted_seq` and
exclusion `retracted` (outside M28, counted in its `excluded`), and no
longer counts as a unit's pass (treatment, crossover, outcome). A retracted
exclusion stays listed (`retracted_seq`) and the unit returns to its
status and the estimate. Owner only (`SqliteStore` refuses workers, imports
and other principals); nothing is deleted.

**Stopping rule.** An experiment whose `stopping_rule` is `planned_units`
must name `planned_units`; assignment is refused once that many units exist
(store and trigger). `fixed_horizon` stays recorded, not enforced.

**Worker guard.** The review CLI records every row as `operator:cli`, so
the store cannot tell a worker at the CLI from the owner. Every `review`
command except `present` (the blind reviewer view) therefore refuses to
run when either marker the product sets for a canonical worker is present:
`HOME` is an execution home recorded in the project's retained native
profiles or collector bindings (canonical launch always runs the agent with
`HOME` set to its profile's execution home, `isolated_gated_command`), or
the working directory is inside `<root>/<project>/.state/worktrees/` (a
worker's task worktree). They are markers, not authority: a process that
rewrites its own environment and directory evades them, and `SqliteStore`
keeps refusing every worker principal. The aggregate `telemetry <slug>
report` is not guarded.

Not built (needs routing, launch or scheduler changes, or new authority):
binding repair attempts or skeptical passes at launch; a scheduled or
launch-time review; a reviewer brief builder and disclosure or blinding of
an actual brief (§3, §7); review acceptance (§2, needs a reviewer-authority
producer); a scoped reviewer-authority grant for triage; conflict records
and third-party review imports (§5); finding occurrences beyond reopenings,
mixed-model segment splits and M24 (§6); adjudicator blinding,
task-clustered intervals and `fixed_horizon` enforcement (§7); seeding
replay-suite tasks (TM4.6); sequencing opportunity creation and
assignment.
