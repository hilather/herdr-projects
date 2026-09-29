# Review capture contracts (lane D)

Owned by lane D ([phase2-lanes.md](phase2-lanes.md)); common rules are
[contracts.md](contracts.md) §0 and §7. Plan doc 06 §2 (TM3.1), doc 07 §5
(M20–M24), doc 10 §5. Canonical migration `0054_review_capture.sql` (schema
54); store API `src/store/review_capture.rs`; CLI
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
  refuse the receipt, ≤64 KiB). `outcome` ∈ `completed`, `incomplete`,
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
- **M21–M24** need accepted decisions: `unavailable:
  no_reviewer_authority_producer`.

Also in `telemetry <slug> report` and the fleet pane through the lane metrics
hook.
