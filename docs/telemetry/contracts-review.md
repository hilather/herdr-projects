# Review capture contracts (lane D)

Owned by lane D ([phase2-lanes.md](phase2-lanes.md)); common rules are
[contracts.md](contracts.md) §0 and §7. Plan doc 06 §2 (TM3.1), doc 07 §5
(M20–M24), doc 10 §5. Canonical migrations `0054_review_capture.sql` (schema
54) and `0055_finding_triage.sql` (schema 55, §5); store API
`src/store/review_capture.rs` and `src/store/finding_triage.rs`; CLI
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
nothing touches `telemetry.db`. `show [--since MS]` (by creation), `present`
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
- **M21** (sum of discovery credit) and **M24** (M21 per review cost) are
  `unavailable: discovery_credit_unallocated`: no role-specific credit
  allocation (TM3.3) or review lifecycle cost exists. M21 carries a labeled
  `drilldown.validated_unique_findings` count, never as its value.

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

Not built: finding occurrences on later artifacts and reopen lineage (TM3.3),
discovery or other role credit (M21/M24), conflict records between
submissions, imports of third-party review comments (any such producer can
only write submissions), and a scoped reviewer-authority grant that would let
a principal other than the owner triage.
