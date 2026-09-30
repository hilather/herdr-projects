# Review capture contracts (lane D)

Owned by lane D ([phase2-lanes.md](phase2-lanes.md)); common rules are
[contracts.md](contracts.md) §0 and §7. Plan doc 06 §2–§6a (TM3.1–TM3.4,
TM3.6), doc 07 §5/§5b (M20–M29, M43, M44), doc 10 §5. Canonical migrations
`0054_review_capture.sql` (schema 54), `0055_finding_triage.sql` (schema 55,
§5), `0056_fix_attribution.sql` (schema 56, §6) and
`0057_review_protocols.sql` (schema 57, §7), `0058_seeded_defects.sql`
(schema 58, §8), `0059_review_ledger.sql` (schema 59, §9),
`0061_reviewer_authority.sql` (schema 61, §10) and `0062_review_launch.sql`
(schema 62, §11); store API
`src/store/review_capture.rs`, `src/store/finding_triage.rs`,
`src/store/fix_attribution.rs`, `src/store/review_protocols.rs`,
`src/store/seeded_defects.rs`, `src/store/review_ledger.rs`,
`src/store/review_authority.rs` (signature checks in `src/authority.rs`) and
`src/store/review_launch.rs`; CLI
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
separate table `review_acceptances`, a decision (`accepted` or `rejected`) by
a delegated reviewer under an owner-signed `code_review` grant, and only that
(§10). Before 0061 a trigger aborted every insert. A receipt carrying an
acceptance field (`accepted`, `trust`, ...) is still refused as an unknown
field, and a completion keeps `trust = proposal` after a decision. `show`
reports `acceptance {active: true, authority: delegated_code_review.v1}` and
each completion's `acceptance` (the decision, or `null` while undecided).

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
attempt or configuration). Since 0062 a launched reviewer's brief is built
from this view only (§11). `--reviewer P` records an operator
assignment (`policy = operator`, `blind = false`, reason
`operator_selected`), with the same covariates.

## 4. Commands and metrics

`telemetry <slug> review open|assign|start|complete|accept` write through
`SqliteStore`, one `state.db` transaction each, principal `operator:cli`;
nothing touches `telemetry.db`. Since 0059 `start` and `complete` also
append one shared-ledger row each (§9), and every `review` command but
`present` refuses to run in a worker execution context (§9); since 0062 the
worker's `session` and `submit` are allowed there too (§11); since 0063
`open` and `assign` append one shared-ledger row each too (§9). `show
[--since MS] [--as-of SEQ]` (by creation; `--as-of` since 0062, §11),
`present` and `report [--since MS]` read `state.db` with contracts §0
reads. `report` reads every metric in one read transaction, so the whole
report is at one watermark of the shared ledger, stated as its top-level
`as_of_seq` (also on M20, M22, M23, M24, M28, M43 and M44); a concurrent
write lands wholly before or after it (card D11).

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
- **M24** review discovery efficiency: §10.

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
this needs no delegation. Delegated `code_review` authority (§10) decides
review completions, never findings; `SqliteStore` refuses every other
principal, writing
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
only write submissions), and delegated triage (a `code_review` grant cannot
permit it, §10).

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
contracts. M24 is §10, M28 §7.


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
routing changes); disclosure of prior conclusions beyond the prior finding
references a `disclosed` protocol shows in the brief (§11); blinding adjudicators to arm; task-clustered intervals,
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
contracts-quality.md §3 does not lift this guard). Since card D11 a seeded
candidate cannot become a group's selection at all: the operator's and a
judge's selection of it are refused and the rule skips it
(contracts-quality.md §3–§4); this guard stays for selections recorded
before. `integrated_commit`
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
completions too (§9), and since 0063 opportunities (listed from their
opening) and assignments (reviewer configuration and window from their
assignment row), so `not_completed` at an earlier watermark counts exactly
the opportunities open and not completed then.

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

Not built: injection tooling for replay-suite tasks (TM4.6). The reviewer
brief (§11) reads no seed state.

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
(unit settlement) and `seeds show`/`seeds report` (trials). Since 0063
opportunities and assignments are ledger rows too (below). Every table is
append-only.

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
binding repair attempts or skeptical passes at launch; a scheduled review
(a review task still needs its own queueing; launch-time sessions and the
blind brief are §11); delegated triage (§10); conflict records
and third-party review imports (§5); finding occurrences beyond reopenings
and mixed-model segment splits (§6); adjudicator blinding,
task-clustered intervals and `fixed_horizon` enforcement (§7); seeding
replay-suite tasks (TM4.6).

**Opportunities and assignments in the one ordering (card D11).**
Canonical migration `0063_review_opportunity_ledger.sql` (schema 63).
`review_opportunity_log(seq, opportunity_id, event, principal, authority,
recorded_unix_ms, backfilled, UNIQUE(opportunity_id, event))`: `review open`
appends `opened` and `review assign` appends `assigned`, each at the next
`seq` of the one ordering in the opening's or assignment's transaction
(`authority = review_capture.v1`, the recorder's principal).
`finding_triage::head` is now over seven ledgers (`finding_log`, `fix_log`,
`protocol_log`, `seed_log`, `review_log`, `review_decision_log`,
`review_opportunity_log`); triggers on all seven refuse a row that does not
follow the others' head. Triggers: a row repeats its recorded opening or
assignment (principal and time), an assignment follows its opening, and a
new session's `started` row follows its opportunity's `assigned` row.
Append-only. Every `--as-of` view replays them (`Lifecycle`,
`review_visibility`): `review show` lists an opportunity from its `opened`
row (`opened_seq`) and its assignment from its `assigned` row
(`assignment.assigned_seq`), so its status is `unassigned` before then;
`seeds show`/`seeds report` list a registered candidate's opportunities and
their reviewer configuration the same way (M43/M44 `not_completed`);
`protocols show` and `experiments show` recompute §4 status with the
assignment's row. The rebuild checks of `tests/quality_certification.rs`
replay `review show`, `seeds show` and `seeds report` exactly at every
captured watermark.

Every review now adds two more rows (opening, assignment) before its
session, so sequence numbers and `finding:canonical-<seq>` identities of a
store's later history differ from what the same history gave before 0063;
rows recorded before the upgrade keep theirs.

*Upgrade.* Opportunities and assignments recorded before 0063 are
sequenced after the ledger head at upgrade (after 0059's and 0062's
backfills) in time order (by time, an opening before an assignment, then
opportunity id; an assignment never before its own opening) with
`backfilled = 1` and `opened_seq`/`assigned_seq` null. Their true place is
unknown, so replay shows them at every watermark (the pre-0063 view); a
trigger refuses any later `backfilled` row. Test
`review_ledger_upgrade_backfills_sessions_in_completion_order`.

## 10. Delegated review authority, acceptance and review cost (card D8)

Canonical migration `0061_reviewer_authority.sql` (schema 61); store API
`src/store/review_authority.rs`; signature checks `src/authority.rs`
(`import_review_authority`, `revoke_review_authority`, `accept_review`); CLI
`telemetry <slug> review authority import|revoke|show` and `review accept`
(`src/telemetry/review/acceptance.rs`). Factory plan F2.5 (docs 03, 05, 07):
bounded delegated authority is its own signed policy with explicit
revocation, derived through the established validator, and a delegate can
never approve its own results, change requirements or widen permissions.

**Grant** (`code_review_authority.v1`, signed by the owner's pinned key,
namespace `code-review-authority@herdr-projects`, verified with
`ssh-keygen -Y verify` exactly like approval, contract and delegation
imports; parsed only after the signature check; unknown fields refused):

```json
{"schema": "code_review_authority.v1", "scope": "code_review", "issuer": "owner",
 "subject": "reviewer:carol", "subject_public_key": "ssh-ed25519 AAAA...",
 "subject_configurations": [], "project_store": "/abs/.state/state.db",
 "repositories": ["/repo"], "tasks": [{"task_id": "work", "contract_revision": 1}],
 "kinds": ["code"], "review_configurations": [],
 "actions": ["accept_review_completion"], "max_decisions": 2,
 "valid_from_unix_ms": 1790000000000, "expires_unix_ms": 1790003600000,
 "prohibited_effects": ["alter_requirements", "approve_author_attempt",
   "approve_own_work", "child_delegation", "increase_permissions"],
 "authority": {"id": "owner-approval-policy", "revision": 1, "digest": "<hex64>"}}
```

- `subject` is a reviewer principal `reviewer:<token>`: never `worker:*`,
  an import or the operator. Its key must not be the owner's (no
  self-delegation). `subject_configurations` (0–16) are the agent
  configurations the reviewer acts as, when it is an agent.
- Scope: this project's store, 1–32 repositories (the submission's
  `repository`), 1–128 task contract revisions, review kinds, and optionally
  1–16 reviewer configurations whose sessions it may decide (empty: any). All
  lists sorted and distinct; a listed review configuration may not be one of
  the subject's own.
- `actions` is exactly `["accept_review_completion"]`: triage, requirement
  changes and permissions stay the owner's. `prohibited_effects` must list
  the five effects above; the owner signs them explicitly. A grant is valid
  for at most 366 days and decides at most `max_decisions` (1–1024)
  completions. `authority` must be the current owner policy.
- Import (`review authority import DOC SIG`) verifies the signature, the
  policy and the owner configuration, refuses an expired grant or another
  project's, and stores the exact bytes and signature
  (`review_authority_grants`, append-only). The same bytes replay; a longer
  or wider grant is a new document the owner signs. `grant_id` is
  `sha256:` of the bytes.

**Nothing else mints or extends a grant.** The planner, workers and the
reviewer hold no owner key; a grant signed by any other key, under another
namespace, or edited after signing is refused, and so is any `review`
command (except `present`) inside a worker execution context (§9).

**Decision** (`review accept SESSION --document D --signature S`): a
`review_acceptance.v1` request `{schema, grant_id, subject, project_store,
session_id, receipt_digest, decision, reason?}` signed with the grant
subject's key (namespace `review-acceptance@herdr-projects`). The grant ID
only selects the key: the stored grant's owner signature is verified again
against the current owner policy (a changed policy refuses), then the
request against the subject's key; the owner's key cannot stand in for it.
`rejected` needs a reason (`evidence_missing`, `insufficient_coverage`,
`protocol_violation`, `wrong_scope`). Refused, writing nothing, unless all
hold at the decision time:

- the grant is unrevoked and `valid_from ≤ now < expires`, and has decided
  fewer than `max_decisions` completions;
- the session's completion is `completed` (an incomplete review is already
  closed), its receipt digest is the request's, and it precedes the decision;
- the session's repository, task contract revision, kind and (when listed)
  reviewer configuration are in scope;
- independence: the subject is neither the reviewing attempt
  (`reviewer:<attempt>`) nor the author attempt, the session is not
  `same_attempt_as_author`, and neither the session's configuration nor the
  author attempt's dispatch configuration is one of `subject_configurations`.

One decision per session (`review_acceptances`, append-only); the same
request replays, any other is refused. Since 0062 each decision is also the
next row of the shared ledger (`review_decision_log`, §11). Each row keeps the request bytes,
signature, `authority = delegated_code_review.v1`, the principal and grant.
A trigger (`review_acceptances_authorized`) repeats every rule except the
signatures on raw rows: raw SQL without a covering grant aborts.

**Revocation** (`review authority revoke DOC SIG`):
`code_review_revocation.v1 {schema, grant_id, project_store, reason,
authority}`, owner-signed (namespace `code-review-revocation@herdr-projects`),
reason `compromised`, `issued_in_error`, `reviewer_retired` or
`scope_changed`. It stops later decisions (store and trigger); earlier
decisions stay. The same revocation replays. `review authority show` lists
each grant's scope, `decisions`, `status` (`active`, `exhausted`,
`not_yet_valid`, `expired`, `revoked`) and revocation, and every decision.

**Triage stays the owner's.** Delegated triage (`delegated_code_review.v1`
on `finding_log`) is not built: `finding_log`'s CHECKs admit only
`operator:cli`/`operator_owner.v1`, and widening them rebuilds the shared
ledger. A decision never changes a finding.

**Metrics.** Doc 07 computes M21–M23 from accepted triage decisions, which
stay the owner's: their values are unchanged. Each carries
`review_acceptance {accepted, rejected, undecided, authority}`: M22/M23 over
the window's submissions by their session's decision, M21 over `F` by its
discovery claim's session.

**M24** `M24.v1` review discovery efficiency (`review report`, `telemetry
<slug> report`): validated unique findings from `Q` / lifecycle review cost
of `Q`, `unit` `findings/<currency>`, an exact reduced fraction.

- Cohort: opportunities assigned in the window. `Q` = the closed ones: a
  completed review with a delegated decision (`accepted` or `rejected`), or
  every session ended without completing (`unsuccessful`).
  `awaiting_acceptance` and `open` are outside `Q`. An accepted review with a
  pending claim is `awaiting_adjudication` (outside `Q`) until the horizon
  (`--horizon-days`, default 14) after its decision, then in `Q` with the
  pending claims counted as not validated (`triage_partial`).
- Numerator: validated finding groups (§5) whose discovery claim is from an
  accepted review of `Q`; seed-linked ones are `seeded_evaluation`, those
  from rejected reviews `excluded_rejected_review`.
- Cost: every session of `Q` (restarts, failed, timed-out, rejected and
  zero-find reviews), each its attempt's primary estimate in the latest
  valuation revision (`accounting cost`, read-only). A session whose attempt
  reviewed more than once or also authored is `shared_attempt_unallocated`;
  one without bound usage (e.g. a non-Codex reviewer) `no_usage_bound`;
  unpriced estimates keep their reason. Never 0: `sessions {total, priced,
  partial, unavailable}` and `cost {status, currency, amount}` (exact).
  Unlinked child sessions and the owner's triage time are `not_included`.
- `value`: the fraction when every session is priced and no triage is
  partial; else `{status: partial, value, reasons}`; `null`
  (`empty_denominator`) without `Q`; unavailable `collection_not_run`,
  `not_priced`, `review_cost_unavailable`, `mixed_currency` or `zero_cost`.
  `basis: published_rate_estimate`, `rate_cards: fixture_only`,
  `observational: true`.

Test `review_cost_and_acceptance_metrics`: synthetic card (input 2, output 4
per 10^6). Accepted O1 (finding a, 0.004) and O2 (none, 0.008), timed-out O3
(0.002), rejected O4 (finding b, 0.004), undecided O5 (finding c): `Q` =
O1–O4, cost 0.018 USD, numerator 1 (b excluded) → `500/9`. An accepted O6
by a reviewer without bound usage leaves the cost at 0.018 and makes M24
`partial` `500/9` (`no_usage_bound` 1). Drill-downs: M22/M23 accepted 1,
rejected 1, undecided 1; M21 `"2"` with accepted 1, rejected 1.

Built by D9 (§11): launching reviewer attempts from an assignment with the
session recorded at launch, the blind brief, the worker receipt channel,
decisions in the shared ledger and the request draft command. Built by
D10 (§12): the trusted reviewer-signer process. Not built: per-attempt usage for non-Codex reviewers (their cost stays
`no_usage_bound`); delegated triage.

## 11. Review launch, the blind brief and the worker receipt channel (card D9)

Canonical migration `0062_review_launch.sql` (schema 62); store API
`src/store/review_launch.rs`; CLI `memory <slug> snapshot --worker
--review-opportunity O`, `telemetry <slug> review session|submit`, `review
accept draft` and `review show --as-of`. Plan doc 06 §2 (the controller
records assignment and session identity; the review brief omits the author's
model or configuration and self-assessment) and factory doc 03 (one admission
path; a runtime adapter validates and freezes the complete brief before
reservation).

**A review is an ordinary task attempt.** Nothing new schedules, ranks or
reserves. The owner adds and queues a task (the *review task*) as for any
work, binds its runtime binding, and makes it a review task with one worker
snapshot:

```
herdr-projects memory <slug> snapshot --task R --profile P --input-file SCOPE --worker --review-opportunity O
```

The snapshot's retained instructions are then the blind brief of O instead of
`PROJECT.md`, and the snapshot is bound to O (`review_briefs(snapshot_id,
opportunity_id, task_id, brief_schema, brief_digest, prior_disclosure,
principal, recorded_unix_ms)`). The ordinary `launch draft` → owner-signed
approval → `launch reserve` (or automatic admission) launches it; `LaunchInputs`,
the attempt and operation identities and approval scopes are unchanged (the
brief is the knowledge the launch already freezes, contracts §3 unchanged).
Binding is refused, writing nothing, unless all hold:

- O is assigned (a review launches from an assignment), and the snapshot is
  a worker snapshot (`char-count-worker-brief-v2`) of R made for the assigned
  reviewer's profile (its definition digest is the assignment's
  configuration's);
- its retained instructions are byte-for-byte O's current brief;
- it selects no task memory: an empty scope (no domains, paths or pinned
  keys) and no optional entries. Mandatory project constraints stay: they are
  the owner's rules for every worker;
- its complete retained rendering (instructions, R's title, memory) contains
  none of the author's identities recorded for O: the author attempt, its
  assignment and dispatch configuration and dispatch profile digest
  (identities shorter than 12 characters are not scanned, they would match
  ordinary words). R's title is the owner's text and is scanned too;
- R is not O's reviewed task, has run nothing but launched sessions of O,
  and is bound to no other opportunity; O has no other review task (trigger).

The same binding replays. A later snapshot of R (a restart needs one: a
snapshot binds the task revision) is another row for the same R and O.

**The blind brief** (`review_brief.v1`, the `Project instructions` of the
worker prompt, digest `brief_digest`) is built only from the §3 blind view:
opportunity, reviewed task and contract revision, repository, base and
candidate commits, object format, scope, kind, protocol and budget, as JSON
data; plus, when the protocol is registered (§7), its method fields
(`challenges`, `failure_classes`, `permitted_tools`, `stopping_rule`,
`evidence_min`, `prior_disclosure`, `outcome`, never its reviewer
configuration). `prior_findings` appear only under a registered protocol with
`prior_disclosure: disclosed`; otherwise (and for an unregistered protocol)
they are withheld and the binding records `withheld`. The brief never names
the author attempt, configuration or profile, prior reviewers or sessions,
seed state (§8) or the submission id, and the builder refuses a brief that
would contain an author identity. It ends with the receipt instructions
(below) and states that the receipt is a proposal.

**Session at launch.** Two version-gated calls in `admit_prepared`
(`src/store/reservations.rs`), for every launch path (operator, delegated,
automatic, and `launch draft`'s dry run):

1. after the knowledge check, `review_launch::check`: for a review task,
   refuse unless the preparation's knowledge snapshot is one bound to its
   opportunity (`a review task launches only with its blind review brief
   snapshot`), its effective profile's configuration is the assignment's
   (`... with its assigned reviewer configuration`), and a session may start
   now (§1 restart rule, not revealed, §8). It only refuses; it grants
   nothing and is a no-op for any other task;
2. after the attempt row, its dispatch decision and candidate-group binding,
   `review_launch::start`: the session start (§1, recorder `service:launch`,
   configuration from the dispatch decision, so `matches_assignment` is
   true) and its shared-ledger `started` row (§9), plus
   `review_session_launches(session_id, attempt_id UNIQUE, snapshot_id)`
   (trigger: the session's attempt, launched with that brief). All in the
   reservation's transaction: a reserved review attempt always has its
   session, and nothing is recorded for a refused one.

Automatic admission selects only a bound brief snapshot for a review task
(`worker_knowledge_selection`, schema 62). An attempt that ends without a
receipt leaves its session open; the owner records its end with `review
complete` (e.g. `interrupted`) before a restart, per §1.

**Worker receipt channel.** Like `result submit`, the reviewing worker uses
the CLI; both commands are allowed in a worker execution context (§9):

- `review session --attempt A` (read-only) prints A's launched session:
  `session_id`, `opportunity_id`, `ordinal`, `submission_id`,
  `candidate_oid`, `completed`, `receipt_schema` (never the author);
- `review submit --input-file F` records a `review_receipt.v1` (§1, unchanged
  schema and refusals) for a session recorded at launch while its attempt has
  not ended (state `reserved`, `launching`, `running` or `awaiting_input`,
  termination not observed), as principal `worker:<attempt>`. It is a
  completion like any other: `trust: proposal`, `coverage_basis: declared`,
  its findings pending submissions (§5), replayed or refused as §1. After the
  attempt ends only the owner's `review complete` records it.

The worker can never accept, reject or triage: a receipt with an acceptance
field is refused (unknown field); `review accept`, `accept draft`,
`complete`, findings triage and every other owner command refuse the worker
context (§9); the worker holds no signing key; a decision needs a
reviewer-signed request under an owner-signed grant (§10), whose subject can
never be the reviewing or authoring attempt. Like `result submit`, the
channel trusts the CLI caller's claim to be the worker (markers, §9, are not
authority); what it records is only a proposal.

**Inside the worker sandbox: the submission spool.** An isolated worker sees
its project's `.state` read-only (docs/reviews/2026-09-29-worker-isolation.md,
"Submission spool"). With `HERDR_PROJECTS_SUBMISSION_SPOOL` in its
environment, `review session`, `review present` and `review submit` do not
open the store: each writes one canonical request into the attempt's spool
`.state/spool/<attempt>/` and prints the receipt the ticker writes back, which
carries exactly the stdout or error of the same command run outside the
sandbox (`telemetry::review::worker_session|worker_present|worker_submit`).
The ticker answers a spooled request only while the attempt is live, and only
for the attempt's own work: `review session` for that attempt, `review
present` for the opportunity it reviews, `review submit` for a session
launched for it. Anything else is refused (`submission spool refused the
request: …`) and recorded as a `spool.request_denied` event. Receipts,
replays and refusals of the store are unchanged (`replayed: true` for the
same receipt). Test
`a_sandboxed_reviewer_uses_its_worker_channel_through_the_spool`.

**Decisions in the shared ledger.** `review_decision_log(seq, session_id
UNIQUE, decision, principal, authority, recorded_unix_ms, backfilled)`: each
§10 decision takes the next `seq` of the one ordering (`finding_triage::head`
now over six ledgers, seven since 0063 (§9); triggers on all of them refuse a
row that does not follow the others' head), in the decision's transaction; trigger: the row repeats
its `review_acceptances` row. `review accept` returns `ledger_seq`.
Decisions recorded before 0062 are sequenced after the head at upgrade (by
decision time, then session) with `backfilled = 1` and are visible at every
watermark, as §9's backfill. `review show --as-of SEQ` replays sessions (from
their `started` row), completions (from their `completed` row) and decisions
(from their decision row) to `SEQ`, recomputes each opportunity's status,
shows each decision's `ledger_seq`, and reports `head_seq`/`as_of_seq`; `SEQ`
beyond the head is refused. Since 0063 it also lists an opportunity only
from its opening and shows its assignment only from its assignment row (§9).
M24's closed cohort still reads the current decisions.

**Request draft.** `review accept draft SESSION --grant G [--reject REASON]
--output FILE` writes (new file only) the exact canonical `review_acceptance.v1`
request (contracts §0 canonical JSON: sorted keys, compact, `reason` null
when accepted), e.g.

```json
{"decision":"accepted","grant_id":"sha256:…","project_store":"/abs/.state/state.db","reason":null,"receipt_digest":"sha256:…","schema":"review_acceptance.v1","session_id":"sha256:…","subject":"reviewer:carol"}
```

and prints `request_digest`, `signer` (the grant's subject) and the
namespace `review-acceptance@herdr-projects`. It checks the grant is
installed for this project and the session has a completed, undecided
review; it signs nothing, writes nothing to the store and decides nothing.
The reviewer signs the file's exact bytes offline (`ssh-keygen -Y sign -f KEY
-n review-acceptance@herdr-projects FILE`) and submits them with `review
accept SESSION --document FILE --signature FILE.sig` (§10, unchanged checks).

**Reviewer key custody (owner decision: option 2, built by D10, §12).**
Offline signing (option 1) stays available. Options considered:

1. *Reviewer-held key, offline signing* (built: `accept draft` + `accept`).
   The human or agent-operator holding `reviewer:<name>` signs each request
   outside the product, on a machine no worker can reach. Strongest
   separation and simplest trust story; every decision needs a person or an
   out-of-band signer, so decisions are slow and do not scale with review
   volume.
2. *Separate trusted signer process.* A long-running service outside every
   worker sandbox holds the reviewer key and signs requests that pass its own
   policy (e.g. accept completed reviews meeting evidence rules), audited per
   decision. Scales and keeps the key out of the model's reach, but that
   process becomes a new trust root: its host, policy and logs need the same
   protection as the owner key, a compromise lets it accept anything in the
   grant's scope until revoked (bounded by `max_decisions`, expiry and scope),
   and "accept automatically" moves the judgment from a reviewer to code.
3. *Owner-signed per decision.* The owner signs each decision with the owner
   key (no reviewer key at all). Needs a schema change (§10 refuses the
   owner's key standing in for the subject, by design) and collapses
   delegated review into owner review: no delegation, and the owner becomes
   the bottleneck; simplest custody (one key).
4. *Hardware-backed reviewer key* (a variant of 1 or 2: `ssh-keygen -Y sign`
   with an `-sk` key or an agent-held key). Non-exportable key, touch per
   decision; same flow as 1, stronger against key theft, still manual.

Whatever is chosen, grants stay short-lived and narrow (§10), and revocation
stops later decisions without touching earlier ones.

Test `review_assignment_launches_with_blind_brief_and_records_session`
(`tests/canonical_worker.rs`, the real launch path on the Herdr stand-in):
PROJECT.md, the authored task's title, the author attempt, its configuration
and profile digest and a prior finding are planted; the delivered prompt is
exactly the drafted brief, its view is the eleven blind fields of O, its
digest is the binding's, and none of the sentinels appear; the session is
`service:launch`, ordinal 1, `matches_assignment`, ledger seq 1; a PROJECT.md
snapshot of the review task is refused at `launch draft`.
`reviewer_worker_submits_proposal_receipt_but_cannot_accept` and
`acceptance_decision_replays_in_the_ledger_as_of` (start 1, completion 2,
finding 3, decision 4, owner triage 5; draft bytes compared literally).

Not built: a planner or scheduler that opens, assigns and queues review
tasks by itself (the owner still adds and queues R); worktrees checked out at
the candidate (R's worktree is its ordinary base; the candidate commit is in
the shared object store and named in the brief); session start at the
worker's actual start rather than its reservation; a guard on raw SQL
inserts of attempts for review tasks (store-enforced only). Custody of a
reviewer key by the product: built by D10 (§12).

## 12. Trusted reviewer-signer process (card D10)

Owner decision on §11's key custody: option 2, a *trusted signer process*.
No migration (schema 62 unchanged). CLI `telemetry <slug> review signer
init|run|status` (`src/telemetry/review/signer.rs`); candidate list
`SqliteStore::review_signer_candidates` (`src/store/review_authority.rs`,
sharing the scope and independence check with `accept_review`); grant check
`authority::verify_installed_review_grant`. The signer is a CLI the owner
runs or schedules as the operator (not a ticker job).

**Custody.** One signer per reviewer principal, in
`<config_dir>/review-signer/<token>/` (`config_dir` is
`~/.config/herdr-projects`, the directory of the pinned owner
configuration): `id_ed25519` (0600), `id_ed25519.pub`, `policy.json` (0600),
`audit.jsonl` (0600, append-only) and a transient `work/` for the request
being signed. `review-signer/` and the signer directory are 0700. The signer
never sees the owner key and cannot mint, widen, extend or revoke a grant:
it only signs `review_acceptance.v1` requests, which count only under an
installed grant the owner signed for exactly its public key.

**Init** (`signer init --subject reviewer:<token> --repository R... --task
T:REV... [--kind K...] [--max-decisions N] [--valid-days D] --output FILE`)
refuses a worker execution context, an existing signer (a key is never
replaced in place) and an existing output file; checks the draft's scope
(`code_review_authority.v1` shape, §10) before any key exists; creates the
directories (0700) and the key (`ssh-keygen -t ed25519`, through the gated
runner); writes the default policy (below); and writes the draft grant
(`subject_public_key` the new key, `project_store` this store, the current
owner policy reference, `valid_from` now, `expires` now + D days (default 7),
`max_decisions` default 16, the five prohibited effects). It prints the
public key, the policy version and the draft's `grant_id`. The owner signs
the draft's exact bytes offline (`ssh-keygen -Y sign -f OWNER_KEY -n
code-review-authority@herdr-projects FILE`) and imports it with `review
authority import` (§10); until then the signer decides nothing.

**Decision policy** (`review_signer_policy.v1`, owner-edited, versioned):

```json
{
  "schema": "review_signer_policy.v1",
  "revision": 1,
  "require_worker_receipt": true,
  "min_evidence_refs": 0,
  "on_failure": "reject"
}
```

Unknown fields refuse; `revision` ≥ 1, `min_evidence_refs` 0–64,
`on_failure` `reject` or `leave_undecided`. Its version is `{schema,
revision, digest}` with `digest` the sha256 of the file's bytes, recorded on
every decision. Rules are mechanical, over stored facts only (never a
finding, title or the candidate's content), in order; the first that fails
decides (`reject`: that reason code; `leave_undecided`: no request):

| rule | passes when | reason |
|---|---|---|
| `well_formed_receipt` | completion `trust: proposal`, `coverage_basis: declared`, findings count = references, receipt digest `sha256:<hex64>` | `protocol_violation` |
| `exact_candidate` | the receipt's submission and candidate are the opportunity's | `wrong_scope` |
| `launched_to_assigned_reviewer` | the session was recorded at launch (§11: `review_session_launches` row for its attempt, recorder `service:launch`, `matches_assignment`, configuration = the assignment's) | `protocol_violation` |
| `receipt_from_reviewer_worker` | when required: the receipt was recorded by `worker:<attempt>` (the launched reviewer itself) | `protocol_violation` |
| `min_evidence_refs` | at least that many evidence references | `evidence_missing` |

All pass: `accepted` (rule `all_rules_passed`). The completion is always
`completed` (only completed reviews are candidates, §10).

**Run** (`signer run --subject S [--once] [--max N] [--interval-secs I]`):
each pass checks the key (below) and the policy, then lists this subject's
installed grants. A grant naming another public key is `other_key`; an
active grant whose owner signature does not verify under the current owner
policy is `unverified`; neither is used. For each active grant (installation
order), the candidates are the completed, undecided sessions it may decide
now (the §10 scope and independence rules, the grant unrevoked, valid and
under `max_decisions`), oldest completion first. For each, the policy
decides; the signer drafts the exact D9 canonical request (`review accept
draft` bytes), checks the key again, signs the bytes (`ssh-keygen -Y sign -n
review-acceptance@herdr-projects`, environment cleared, no agent) and
submits them through `review accept`'s path (`authority::accept_review`),
which verifies the grant's owner signature and the request's signature again
and records the decision and its ledger row (§10, §11). At most `--max`
(default 16, at most 128) decisions per pass and never past the grant's
remaining decisions. Idempotent: a decided session is no longer a candidate
and the same request replays. `--once` prints the pass; otherwise a pass
runs every I seconds (default 60) and each prints one JSON line (a refused
pass prints its error and decides nothing).

**Safety checks, before every use** (`run` per pass and again before each
signature, `status`): `review-signer/` and the signer directory are real
directories (not symlinks) owned by this user, mode 0700; the key is a
regular file (not a symlink), owned by this user, one link, mode exactly
0600; the public key is a regular file not group/world writable; the policy
is a regular owner-only file of at most 4 KiB; the directory is under the
pinned owner configuration's directory (so the worker sandbox's
`<config dir>/review-signer` hide covers it whatever `HOME` the controller
has), not inside the projects root (any project or task worktree), and
neither inside nor containing any execution home retained by any project
under the root. Every signer command refuses a worker execution context
(§9's markers, plus `HOME` under the projects root).

**Audit.** `audit.jsonl` (opened append-only, `O_NOFOLLOW`, owner-only, one
link, fsynced per line): `review_signer_audit.v1` lines with `subject`,
`recorded_unix_ms` and `event` `init` (public key, policy version, draft
grant id) or `decision` (grant, session, decision, reason, rule, policy
version, the facts it read (`checks`), and `result` `recorded` with
`request_digest` and `ledger_seq`, or `refused` with the error). A decision
is also the §10 `review_acceptances` row (request bytes and signature) and
the §11 `review_decision_log` row; the ledger row is unchanged (no policy
column): the audit line joins it by session, `request_digest` and
`ledger_seq`. `leave_undecided` outcomes are reported in the pass, not
audited.

**Status** (`signer status --subject S [--last N]`, read-only): directory,
public key, `key_check` (`ok` or the refusal), policy version, this
subject's grants (`status` including `other_key`/`unverified`,
`max_decisions`, `decisions`, `remaining`, validity, revocation) and the
last N audited decisions.

**Trust boundaries.**

- *Canonical and reviewer workers cannot reach the key.* They run in the
  isolated sandbox (`worker_supervision::Isolation`,
  docs/reviews/2026-09-29-worker-isolation.md): `~/.config/herdr-projects`
  of every owner home and `<pinned config dir>/review-signer` are empty
  read-only mounts they cannot lift, so the key, policy and audit log are
  unreadable; the signer CLI also refuses their context. The signer
  requires its directory under the pinned configuration's directory, so
  this holds even if the controller's `HOME` differs from the operator's.
- *The coordinator stays trusted (owner decision, accepted).* The
  coordinator agent and legacy thread agents run from Herdr `agent.start`
  with the owner's full view (isolation review, residual risk 7): they can
  read the signer key and could sign decisions. The owner accepts this. It
  is bounded by the grant: only sessions in its scope (store, repositories,
  task contract revisions, kinds, reviewer configurations), never the
  reviewer's own or the author attempt's work, at most `max_decisions`,
  only between `valid_from` and `expires` (at most 366 days; the draft
  defaults to 7), and stopped at once by an owner-signed revocation (§10);
  a decision never triages, changes a requirement or permission, or mints
  a grant, and every decision is attributable (`reviewer:<token>`, grant,
  request bytes and signature, ledger row).
- *Facts come from the project store.* Until the write-isolation card
  lands, a worker can write its own project's `state.db` (isolation review,
  residual risk 1), e.g. raw review rows the policy then reads. Triggers
  repeat the session/launch/completion bindings on raw rows, and an
  unsigned grant row is never used (`unverified`), so this can at most
  steer a decision within an owner-signed grant's bounds above, never
  create authority.
- *Automatic acceptance moves judgement to code* (§11 option 2): the policy
  checks provenance and form, not review quality. Keep grants short and
  narrow and `max_decisions` small; use `on_failure: leave_undecided` to
  keep doubtful reviews for the owner.

Tests (`tests/review_signer.rs`, real `ssh-keygen` keys in temporary homes,
the real D9 launch path, `launch draft` → owner approval → `launch
reserve`): `signer_accepts_in_scope_reviews_under_policy_and_audits` (S1
launched with the reviewer's own receipt accepted at ledger 7 under policy
revision 1, digest `sha256:e35de0ca…d4f6`; S2 owner-started rejected
`protocol_violation` by `launched_to_assigned_reviewer` at ledger 8 under
revision 2; S3 `security`, outside the grant, never decided; `--max 1`
bounds a pass, the third pass decides nothing; request digests and audit
lines compared to hand-built literals),
`signer_refuses_bad_key_permissions_worker_context_and_revoked_grants`
(key 0644, directory 0755, symlinked key, group-readable policy, worker
`HOME` for `run`/`status`/`init`, `other_key` and unsigned `unverified`
grants, a revoked grant, an execution home inside the signer directory:
nothing signed or decided), `isolated_worker_cannot_read_the_signer_key`
(the launch service's sandbox argv from the retained profile, route and
pinned configuration: key, policy and audit log unreadable, the directory
absent, `signer status`/`run` refused; the same probe outside reads the
key).

Not built: a ticker hook (by owner decision the owner runs or schedules the
CLI); hardware-backed signer keys (§11 option 4); isolating the coordinator.
