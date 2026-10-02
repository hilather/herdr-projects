# Telemetry slice contracts (W0)

Status: frozen contracts for the thin slice only (outcome record, Codex usage
adapter, basic workspace panel). Base `main` `ebfe10e`, store schema 45.
Evidence for every "at main" statement is in [baseline.md](baseline.md);
cards in [slice-cards.md](slice-cards.md). Plan IDs (M-numbers, payload names)
follow `herdr-telemetry-plan-v1.1` docs 03 and 07. Changing any contract here
needs a new reviewed revision, not a silent reinterpretation.

## Index

- §0–§8 below: the thin-slice contracts.
- Sidecar streams (phase 2, card S0): `telemetry.db` is versioned per stream
  in `telemetry_streams(stream, version)`: `codex` (§5, migrations under
  `migrations/telemetry/`, also `user_version`; 3 adds TM5.1's read indexes,
  certificate-scale.md §5; 4 adds lossless storage compaction, §4.15), and
  one stream per lane under
  `migrations/telemetry/<stream>/`: `ingest`, `accounting`, `quality` (0004 adds DG6d/e rerun observations; contracts-quality.md §6),
  `review`,
  `analytics` (TM4.1 aggregate revisions), `health` (TM4.5 alert
  state), `otlp` (DG4a sanitized native records, migration 0001;
  DG4c Gemini SDK file cursors, migration 0002;
  DG4h hashed per-attempt tokens and revocations, migration 0003;
  contracts-collection.md DG4a/DG4c/DG4h). A reader refuses only a
  stream newer than it knows. Plan:
  [phase2-lanes.md](phase2-lanes.md).
- Lane contracts, each owned by its lane: collection
  ([contracts-collection.md](contracts-collection.md)), accounting
  ([contracts-accounting.md](contracts-accounting.md)), quality
  ([contracts-quality.md](contracts-quality.md)), review
  ([contracts-review.md](contracts-review.md)).
- Metric registry, query service and the read contract shared by
  `telemetry report`, the fleet pane, views and exports (TM4.1):
  [contracts-analytics.md](contracts-analytics.md).
- Portable JSON/CSV exports, authenticated page cursors and the optional
  external export setting (TM4.3): [contracts-export.md](contracts-export.md).
- Health rules, deduplicated alerts, inbox notices and advisory
  recommendations with M50 evidence freshness (TM4.5):
  [contracts-health.md](contracts-health.md).
- Retention classes (`retention.v1`), holds, tombstoned deletion, sidecar
  backup and restore (TM5.3): the operations store
  `<project>/.state/telemetry-ops.db` (tombstones, holds, runs, backup
  inventory, restore reports; not a sidecar stream) and
  [operations-runbook.md](operations-runbook.md).

Sidecar first-open concurrency: migrations acquire `BEGIN IMMEDIATE` before
reading stream versions inside the write transaction. The standalone
`journal_mode=WAL` pragma may return immediate `SQLITE_BUSY` while upgrading
its internal read lock, bypassing SQLite's busy timeout. Open retries that
pragma outside a transaction for up to five seconds, releasing the failed
statement's lock between attempts; concurrent collectors then join the same
migrated sidecar.

## 0. Common rules

- **Authority.** Telemetry never grants launch, changes budgets, accepts
  results or writes memory. Canonical rows below are written by existing
  controller transactions; the sidecar is analytics only.
- **Unknown is not zero.** Every value that can be missing is either a value
  or `{"status":"unavailable","reason":<code>}`. Numeric `0` means observed
  eligible exposure with no events. Ratios with a zero denominator are `null`
  with reason `empty_denominator`.
- **Canonical JSON.** Sorted object keys, compact UTF-8, no trailing newline,
  integers only (no floats; decimals as strings), `null` never omitted
  (RFC 8785 subset, as doc 03 §4). Digests are `sha256:` + lowercase hex.
- **Time.** Unix milliseconds from the controller's `now` at the transition.
  Codex times are kept as reported and never reorder canonical events.
- **Stores.** Canonical: `<project>/.state/state.db`, migrations 0049–0068 (0052 collector bindings, contracts-collection.md; 0053 candidate groups, contracts-quality.md §3; 0054 review capture, contracts-review.md; 0055 finding triage and duplicate history, contracts-review.md §5; 0056 fix attribution, regressions and role credit, contracts-review.md §6; 0057 review protocols, passes and preregistered experiments, contracts-review.md §7; 0058 seeded defects, recall and the seeded-candidate integration guard, contracts-review.md §8; 0059 review ledger, contracts-review.md §9; 0060 accepted supersession reasons, contracts-accounting.md §10; 0061 delegated code_review authority, review acceptance decisions and revocations, contracts-review.md §10; 0062 review launch: blind review briefs bound to review tasks, review sessions recorded by the reservation that launches them, and delegated decisions in the shared review ledger, contracts-review.md §11; 0063 review opportunity openings and assignments in the shared review ledger, so every `--as-of` review, seed and protocol view replays them, contracts-review.md §9; 0065 assignment-policy settings, owner-signed `randomized_assignment` grants and the policy record of assigned decisions, contracts-evaluation.md §9; 0064 replay suite registry, hidden checks and replay candidates, contracts-replay.md; 0066 owner-signed revocations of randomized-assignment grants, contracts-evaluation.md §9; 0067 read indexes for the telemetry projections, `verified_results(submission_id)` and `result_submissions(attempt_id)`, certificate-scale.md §5; 0068 verifier-owned run metadata: load context and bounded test names/outcomes, contracts-quality.md §6).
  Sidecar: `<project>/.state/telemetry.db`, own sequence under
  `migrations/telemetry/` (per-lane streams; see the lane contracts: e.g.
    `ingest` 0006 tool/exec metadata, 0007 subagent detail, per-source ingest
  state, envelope `measurement.certified` and `final_event_missing` gaps,
  0008 MCP calls, subagent and collab items, aborted turns, function call
  namespaces, fork points and fork reconciliation, 0009 terminated turns,
  0010 Claude Code message identity and reported tool outcomes (DG4b);
  0011 OpenCode native message/tool metadata (DG4d);
  accounting 0013 adds Claude ledger source/cache normalization, 0014 adds OpenCode,
  0015 adds maintained read aggregates,
  contracts-collection.md A6–A9 and DG4b/DG4d), mode 0600, created on first collect. No
  cross-database transaction or foreign key; sidecar rows reference canonical
  IDs by value and record `orphan` when the canonical row is missing.
- **Sidecar durability and bounded collection (P4).** Writer connections use
  WAL with `synchronous=FULL`. `NORMAL` was evaluated and rejected: the core
  TM2.6 certificate's R4 says valuation revision history and live attention
  samples cannot be regenerated from rollouts, and imports must be retained
  separately. Losing acknowledged transactions on power loss would weaken
  that contract and earlier as-of evidence. Canonical commits are unchanged.
  The ticker collector commits at complete-line boundaries after at most
  2,000 input lines or reaching 8 MiB for ordinary first-collection and append
  tails; one complete line can cross the byte threshold
  (the existing 16 MiB parse/whole-line skip rule stays intact). This is a
  bound on collector input per transaction, not a claim about WAL size or all
  lane transactions. Foreground CLI collection retains its whole-rollout
  transaction (256 MiB total budget). Byte-zero re-reads retain the original
  budget-bounded transaction so `predates_ingest` gaps, which have no prefix
  envelopes, cannot be incorrectly recovered after a kill. Each batch atomically stores
  the observations, native ordinals/model/turn state and both cursors. A kill rolls back only the
  unfinished batch; earlier prefixes remain durable and resume without
  duplicate acceptance. Failed-write coverage remains pending until its
  entire range has committed, even across batches. Binding updates commit
  at most 1,000 sources (two writes each) under an immediate lock. Ledger
  projections, quota state and their shared watermark remain atomic; analytics
  revisions and lineage remain in their contracted immediate transaction.
  Those larger semantic transactions are a residual contention risk.
  The ticker worker alone uses Linux `SCHED_IDLE`, nice 19 and I/O idle class;
  lane subprocesses inherit these priorities through the existing spawn gate.
  Setting priority is advisory, with one failure log per process; collection
  and every lane continue. Foreground CLI calls keep their caller's priority.
- **Reads.** `attempts`, `usage`, `report`, the fleet pane, `doctor` and the
  collector's canonical read open `state.db` and `telemetry.db` strictly
  read-only and create no file (`telemetry::read_only`): first an advisory
  read lock on SQLite's SHARED byte range of the main file (held until the
  connection closes, so no writer can take EXCLUSIVE to checkpoint-on-close or
  delete `-wal`/`-shm`); then, when both `-wal` and `-shm` exist, a plain
  read-only open that reads committed WAL frames through the existing `-shm`;
  otherwise (no connection has the file open) an `immutable` open, because a
  plain read-only open would create `-wal`/`-shm` that only a writer removes.
  Residual: in the `immutable` case a writer that opens mid-read and
  auto-checkpoints (≥1000 WAL pages) could show one display inconsistent
  pages; the stores are never affected. A sidecar whose schema this binary
  does not know is refused, never migrated, by a reader.

## 1. TaskClassification (taxonomy v1)

Written by the controller in the `admit_prepared` transaction
(`src/store/reservations.rs`) before the dispatch decision, once per
`(task_id, contract_revision, taxonomy)`; later attempts reuse it. Inputs are
contract properties only, never outcomes (tokens, success, findings).

Inputs: `task_contracts.route`; write/read rows of `contract_scope_paths`
(with `certainty`); `contract_named_resources`; `task_dependencies`
(`requirement`); `LaunchInputs.repositories` count. A task with no
`task_contract` gets class `unscoped`, band `unknown`.

**Class** (first rule that matches):

| Order | Class | Rule |
| --- | --- | --- |
| 1 | `schema_change` | named resource `schema` with `write` |
| 2 | `dependency_change` | named resource `lockfile` with `write` |
| 3 | `read_only` | no write path and no write named resource |
| 4 | `docs` | every write path under `docs/` or ending `.md` |
| 5 | `tests` | every write path under `tests/` |
| 6 | `code` | otherwise |

**Difficulty points** = `min(write_paths, 8)` + `2 × uncertain_write_paths`
+ `3 × write_named_resources` + `dependencies` + `2 × (repositories − 1)` +
`1 if route = verify_then_integrate`. Band: `small` ≤ 3, `medium` 4–8,
`large` ≥ 9. Worked example: 3 exact write paths, 1 uncertain write path, no
named writes, 2 dependencies, 1 repository, `verify_then_integrate` →
`min(4,8)=4 + 2 + 0 + 2 + 0 + 1 = 9` → `large`.

Record: `classification_id` (digest of the canonical record without the ID),
`task_id`, `contract_revision` (nullable), `taxonomy` `"task-taxonomy.v1"`,
`class`, `band`, `features` (the five integer inputs above plus `route`),
`classifier` `"rule:task-taxonomy.v1"`, `revision` 1, `created_unix_ms`.
Reclassification appends a revision with a reason; analysis uses revision 1.
Coordinator/operator classification and agreement sampling are deferred.

S1 refinements: a task with no contract stores `route`, `write_paths`,
`uncertain_write_paths` and `write_named_resources` as `unavailable:
no_contract`, never 0, and its band is `unknown`. Classes `docs` and `tests`
require at least one write path, so a contract that only writes a named
resource is not `docs`.

## 2. AgentConfiguration

Content-addressed comparison arm derived from the attempt's `FrozenProfile`
(`src/domain/profile.rs`). Canonical JSON, exactly these keys:

```json
{"adapter":{"digest":"<hex64>","id":"canonical-local-herdr-0.9.1","revision":3},
 "agent_digest":"<hex64>","agent_version":"0.154.0",
 "arguments_digest":"<hex64>","definition_digest":"<hex64>",
 "environment_names":["A","B"],"kind":"codex",
 "permission_policy":{"digest":"<hex64>","id":"...","revision":1},
 "reasoning_effort":null,"reasoning_effort_reason":"mapping_unverified",
 "requested_model":null,"requested_model_reason":"mapping_unverified",
 "schema":"agent_configuration.v1"}
```

(shown wrapped; stored compact). `environment_names` sorted. `configuration_id`
= `sha256:` over those bytes. Excluded on purpose: `name`, `agent.path`,
`execution_home`, `config`, `herdr` identity, `capabilities`,
`workflow_certificate` (host paths and evidence churn are not arm changes).
`requested_model`/`reasoning_effort` stay `null` with reason
`mapping_unverified` while `validate_gated_preparation`
(`src/profile_config.rs`) refuses those mappings; when a mapping is certified
they become strings and the reason becomes `null` — a new arm by construction.
Display label `"<kind> <agent_version>"` is derived, never an identity.

Canonical table `agent_configurations(configuration_id PK, canonical_json,
first_decided_unix_ms)`, immutable; an insert with an existing ID must carry
identical bytes or the transaction fails.

## 3. DispatchDecision

One row per canonical attempt, inserted in the same `admit_prepared`
transaction as the `attempts` row, after it and before commit. Never part of
`LaunchInputs` (attempt and operation IDs derive from inputs via
`record_ids`; approvals match inputs). Drafts (`validate_launch_draft`) write
nothing. A delegated replay returns the existing decision.

| Field | Contract |
| --- | --- |
| `attempt_id` | PK; FK `attempts(id)` |
| `task_id`, `task_revision` | Bound task revision (`inputs.task_revision + 1`) |
| `contract_revision` | From `inputs.task_contract`, nullable |
| `classification_id` | §1 row, or `null` only when no contract (class `unscoped` still written) |
| `chosen_configuration_id` | §2 of `inputs.effective_profile` |
| `eligible` | Canonical JSON array ordered as evaluated: `{configuration_id, profile_digest, status, probability_ppm}`; `status` ∈ `chosen`, `no_knowledge`, `no_approval`, `not_evaluated`, and for a policy-assigned decision `eligible` (approved, weighed, not chosen); `probability_ppm` integer: chosen = 1000000, others 0, except a policy-assigned decision (contracts-evaluation.md §9), which logs the policy's per-entry probabilities (summing to 1000000, positive only on `chosen`/`eligible`) |
| `chooser_kind` | `operator` (`launch_preparation::reserve`), `automatic_admission` (`admission::decide_held`), `delegated` (`reserve_delegated`) |
| `chooser_principal` | `approval:<approval digest>` / `rule:automatic-admission.v1` / `grant:<grant_id>` |
| `reason_codes` | Sorted unique array, ≤4, from: `operator_selected`, `only_eligible`, `first_matching_approval`, `delegated_grant`, `recommended`, `operator_preference`, `availability`, `exploration`, `replay`, `continuation`, `unspecified` |
| `note` | Optional operator note, excerpt rules §7, ≤160 chars; `null` if absent |
| `policy`, `seed` | Always `null` (the 0050 check); a policy-assigned decision's policy, spec, seed and draw are in `dispatch_policy_assignments` (0065), keyed by `attempt_id`, written in the same transaction; its chooser is `automatic_admission` with principal `policy:<policy>@<settings revision>` and reason `exploration` (stochastic) or `recommended` (`deterministic.v1`), plus `only_eligible` |
| `decided_unix_ms` | The transaction's `now` |

Eligible set per path: operator and delegated = the single selected profile
(`status: chosen`); automatic admission = every profile returned by
`binding_profiles` (`src/admission.rs`) in order, with the status reached in
the sealing loop. Operator reason codes come from a new optional `reason` /
`note` in `LaunchSelection` (`src/launch_preparation.rs`, currently
`deny_unknown_fields`); absent → `unspecified`. Automatic and delegated paths
add their fixed code (`first_matching_approval` / `delegated_grant`), plus
`only_eligible` when the eligible set has one entry.

Invariant: `count(attempts) = count(dispatch_decisions)` for attempts created
at or after migration 0050; older attempts report decision `unavailable`,
reason `predates_dispatch_log`.

S2 refinements:
- `profile_digest` is the bare hex `FrozenProfile::reference()` digest (equal
  to `inputs.profile.digest`), not `sha256:`-prefixed, so it joins to
  `native_profiles` and attempt inputs.
- `classification_id` is always set: S1 writes an `unscoped` row for a task
  without a contract. The column stays nullable.
- `agent_configurations` receives every configuration in `eligible`, not only
  the chosen one; `first_decided_unix_ms` is the first decision that weighed it.
- Operator `reason` is one code from `operator_selected`, `recommended`,
  `operator_preference`, `availability`, `exploration`, `replay`,
  `continuation`, `unspecified`; any other value refuses the reservation.
  Operator decisions never add `only_eligible`. `LaunchSelection.reason`/`note`
  are serde-default and skipped when absent; the grant is derived from
  `LaunchInputs` only, so its digest is unchanged.
- The note excerpt is applied by the store before the insert, with the
  process `HOME` as the home prefix; a note empty after the excerpt is `null`.
  Rule 3 applies to words that start with `/` or `~` or contain `://`. Rule 4's
  run class includes `/`, so a ≥20-char path segment run with letters and
  digits is masked too (conservative).
- Automatic admission: statuses follow the sealing loop (`no_knowledge` when no
  worker snapshot seals, `no_approval` when none matches, `not_evaluated` for
  profiles after the chosen one). The context is descriptive: the store refuses
  it unless exactly one entry is `chosen` and it is the reserved profile. A
  candidate with no chosen profile reserves nothing and writes nothing.
- Entry points: `SqliteStore::reserve_prepared` keeps its signature as an
  operator reservation without reason; `reserve_prepared_dispatched`,
  `reserve_prepared_controlled` and the controlled `reserve_prepared` take a
  `DispatchContext`; drafts pass a fixed operator context and return first.
- A store at schema 49 reserving before upgrade writes the classification only.

## 4. AttemptOutcome record

**Lifecycle marks** (migration 0051): `attempt_lifecycle(attempt_id,
state, attempt_revision, unix_ms, source)`, PK `(attempt_id, state)`, written
in the same transaction as each transition: `reserved`
(`admit_prepared`), `launching` (`apply_launch_started`), `running`
(`apply_worker_brief`), terminal `completed|failed|cancelled|lost`
(`record_worker_termination_with_budget`, `record_launch_stopped_with_budget`,
`stop_worktree_preparation_with_budget`, `cancel_attempt_in_transaction`).
`source` names the function. A mark missing because the attempt predates 0051
reads `unavailable: predates_lifecycle_log`.

**Outcome** is a read-only projection (no table), one per attempt:

| Field | Source / rule |
| --- | --- |
| `attempt_id`, `task_id`, `configuration_id`, `classification` | decision row |
| `reserved/launching/running/terminal_unix_ms` | marks; absent later marks are `null` with `state: open` |
| `terminal_state` | terminal mark, else `open` |
| `active_ms` | `terminal − running` wall time, idle time at the prompt included (not an activity signal; attention is S4); open → `censored`; no running mark → `unavailable`. The text form labels it `wall_ms` |
| `queue_to_launch_ms` | `launching − reserved` |
| `result` | latest submission meeting §6 `A` for the current contract (including confirmed integration when required), otherwise latest submission; ties use insertion order. `submission_id`, `candidate_oid`, `created_unix_ms` identify the chosen submission; `submissions` counts all submissions (0 when `not_submitted`) |
| `verification` | per acceptance policy of that submission, its latest `verification_runs` row: `rejected` (+ `reason` excerpt) if any policy's is, `accepted` only if every policy's is, else `error` (+ diagnostic excerpt) for a permanently failed verification job without a run, otherwise `pending`; `policies` lists each |
| `integration` | route `verify_only` → `not_applicable`; else confirmed integration preferred, otherwise latest `integration_operations` state via `verified_results`; `integrated` needs an `integrated_commits` row |
| `accepted` | `true` iff the task's acceptance rule (§6 `A`) is met by evidence produced from this attempt |
| `usage` | §5 sums if a certified bound rollout exists, else `unavailable` with reason `adapter_absent` (non-Codex kind), `not_bound`, `cli_version_uncertified`, `quarantined`, `records_not_accepted`, `collection_not_run` |
| `attention` | B6b's per-attempt summary (contracts-accounting.md §6) once any attention sample exists; before that `unavailable`, reason `attention_not_collected` |

S3 refinements:
- Marks cover canonical attempts only (an `attempt_inputs` row). Adopted
  attempts and generic `commit` writes (baseline §1) have no decision and are
  not projected. A state keeps its first mark (`ON CONFLICT DO NOTHING`), so a
  mark never blocks its transition. `cancel_attempt_in_transaction` marks
  `cancelled` only when it releases the attempt; otherwise the termination
  path writes the terminal mark later. Task `complete` writes no mark itself:
  its terminal `completed` comes from `record_worker_termination_with_budget`.
- An attempt without a `reserved` mark predates the log: its missing marks,
  `active_ms` and `queue_to_launch_ms` are `unavailable:
  predates_lifecycle_log` (a later mark written after the upgrade is shown).
  For a logged attempt a mark not yet or never reached is `null`.
- `terminal_state` is the attempt row's state when terminal, else `open`, so a
  pre-0051 terminal attempt still reports its state. `active_ms`: open with a
  running mark → `censored: open`; no running mark → `unavailable:
  not_running`. `queue_to_launch_ms` without a launch mark → `censored` with
  reason `open` or the terminal state (e.g. `cancelled`).
- Shapes: `result` `{state: submitted|not_submitted, submission_id,
  candidate_oid, created_unix_ms, submissions}`; `verification` `{state:
  accepted|rejected|error|pending|not_submitted, reason?, policies?}` for the
  chosen submission (text lines add `submissions=N submission=ID`), combined over its contract's acceptance policies and
  any other policy with a run: each policy is decided by its latest run
  (`{policy_id, state, reason?}`, `error` for a permanently failed current job without a run, otherwise `pending`); the submission is
  `rejected` if any policy is (reason of the first by `policy_id`),
  `error` if any remaining policy has a permanently failed job without a verdict,
  `accepted` only if every policy is, else `pending`. A later retry verdict wins
  over job failure; reset jobs return to pending. Reasons are excerpted
  with the reader's `HOME`;
  `integration` `{state}` from the same submission: `not_applicable` unless the
  route is `verify_then_integrate`, then `not_submitted`, `pending` (no
  operation), `not_applicable` with reason `verification_rejected` (no
  operation and the verification is rejected, so the submission is not
  eligible), confirmed integration preferred, otherwise the latest operation state, `integrated` (with an
  `integrated_commits` row) or `integrated_unconfirmed` (state without a
  commit row); `classification` `{classification_id, class, band}`. A missing
  decision makes `configuration_id` and `classification` `unavailable:
  predates_dispatch_log`.
- `accepted` checks any submission of the attempt against the task's current
  contract revision and route. `usage` is `unavailable: adapter_absent` for
  non-Codex kinds; for Codex it is `collection_not_run` without a sidecar,
  else the sidecar's attempt usage (§5 "Attempt `usage`", the same value
  `telemetry usage` prints), read without creating or migrating the sidecar.
- Verifier reasons are fixed codes today; the excerpt test adds a later run
  with a free-text reason to show display-time redaction.
- `attention` (read from the sidecar's `attention_samples`, never written by
  `attempts`): without a sidecar or before any sample exists every record is
  `unavailable: attention_not_collected`. Afterwards an attempt without a
  `runtime.launch_started` receipt is `unavailable: not_launched`; a launched
  attempt with no successful sample is `unavailable: not_observed` with
  `gaps`; otherwise `{interventions, uncertain_starts, waiting_ms,
  observed_ms, intervals, censored_intervals, gaps, reason_type}`, the
  counters of its `accounting attention` entry (counted waits, Σ closed wait
  durations, observed time, waits without a duration). Every non-default value
  carries `gaps` (count per gap reason, `{}` when none), `basis` (the signal's
  certification, `fixture` today) and `source` (`herdr-agent-list-v1`). The
  text form prints `attention=waits=<n> waiting_ms=<n> censored=<n>
  gaps=<total>` or `attention=unavailable:<reason>`. Test
  `attempts_show_attention_summary`.

## 5. Codex usage (sidecar)

**Source.** Rollout JSONL files under
`<execution_home>/.codex/sessions/**/rollout-*.jsonl` for each Codex profile's
`execution_home`. The collector reads only `session_meta`, `turn_context`,
`token_usage_record`, `event_msg` of type `token_count`, `task_started`,
`task_complete`, `turn_aborted` (A8), `item_completed`, and `response_item`
of type `custom_tool_call`, `function_call`, `custom_tool_call_output`,
`function_call_output` (A6: tool metadata only, through a typed allowlist).
All other record types are skipped by type tag without retaining any field. Read only complete lines (ending `\n`); a partial last
line is left for the next pass and the file offset is not advanced past it.

**Allowlisted fields.** `session_meta`: `id`, `timestamp`, `cwd`,
`cli_version`, `originator`, `source`, `model_provider`, `forked_from_id`,
and from `source.subagent` its variant and, for `thread_spawn`,
`parent_thread_id` and `depth` (A4), and for the `other` variant its string
tag as `subagent_detail` (A7, sidecar stream `ingest` 0007
`rollout_subagents`; live: `guardian`); and the thread lineage
`parent_thread_id`, `session_id` and `thread_source` of `session_meta`
(A5, sidecar stream `ingest` 0005 `rollout_threads`).
A fork's `forked_from_ordinal_exclusive` and `history_base.{thread_id,
end_ordinal_exclusive, end_byte_offset}` (A8, sidecar stream `ingest` 0008
`rollout_forks`). Usage is keyed by the
rollout's own `session_meta.id`, never by a record's `session_id`, which a
guardian reports as its parent's. `turn_context`: `turn_id`, `model`,
`effort`. `token_usage_record`: `session_id`, `turn_id`, `response_id`,
`usage.{input_tokens, cached_input_tokens, cache_write_input_tokens,
output_tokens, reasoning_output_tokens, total_tokens}`, final
`thread_token_usage` (for reconciliation only), and the line `timestamp`
(A4). `token_count`:
`rate_limits.{limit_id, primary.{used_percent, window_minutes, resets_at},
secondary.{used_percent, window_minutes, resets_at}, rate_limit_reached_type,
plan_type}` (secondary and reached type: A4) and `info.total_token_usage` (discrepancy only). `task_complete`:
`turn_id`, `duration_ms`, `time_to_first_token_ms`.
`turn_aborted`: `turn_id`, `reason`, `duration_ms` and the line `timestamp`
(A8: the aborted turn's final event). Tool calls
(`custom_tool_call`, `function_call`): `call_id`, `name`, `status`,
`internal_chat_message_metadata_passthrough.turn_id`, the line `timestamp`,
and for a `function_call` its `namespace` (A8); their outputs (`*_call_output`): `call_id` and the line
`timestamp`; `item_completed`: `thread_id`, `turn_id`, `item.type`, and for
a `CommandExecution` item `item.{id, status, source, exit_code,
duration.{secs, nanos}}` (the exec startup, not the command's run time;
certified statuses `completed` and `failed`) and the line `timestamp` (A6,
sidecar stream `ingest` 0006 `codex_tool_calls`, `codex_exec_items`,
`codex_tool_sources`; lenient like A4); for an `McpToolCall` item
`item.{id, server, tool, status, readOnlyHint, result.isError,
duration.{secs, nanos}}`; for a `SubAgentActivity` item `item.{id,
agent_thread_id}`; for a `CollabAgentToolCall` item `item.{id, status,
sender_thread_id, receiver_thread_ids[]}` (A8, sidecar stream `ingest` 0008
`codex_mcp_calls`, `codex_agent_items`, `codex_turn_aborts`,
`codex_tool_namespaces`, `rollout_turn_ends`). Never:
`last_agent_message`, instructions, messages, reasoning, tool `input`,
`arguments` and `output`, and an item's `command`, `cwd`, `parsed_cmd`,
`stdout`, `stderr`, `aggregated_output`, `formatted_output`, `process_id`,
`content`, `client_id` or `phase`, an MCP call's `arguments` or `result`
content, a subagent's `agent_path`, or a collab call's `receiver_agents` or
`agents_states`.
A4 metadata (sidecar stream `ingest` 0004: `rollout_metadata`,
`codex_usage_times`, `codex_rate_limit_windows`) is read leniently: a value of
another type is stored as `NULL` and never makes its record malformed. It is
outside `payload_digest`; the first stored value stays. All A4 fields are
certified `fixture` until a live run (contracts-collection.md A4).

**Usage record** (`codex_usage`): key `(session_id, ordinal)` where
`session_id` = `session_meta.id` and `ordinal` = 1-based position of the
`token_usage_record` in that rollout file. Payload = canonical JSON of
`{response_id, turn_id, model, effort, usage{...}}` (model/effort from the
latest preceding `turn_context`, `null` if none); `payload_digest` over it.

- Same key, same digest → no-op (replay).
- Same key, different digest → keep the first row, write
  `codex_quarantine(session_id, ordinal, first_digest, new_digest,
  observed_unix_ms)`, mark the session `quarantined`; its usage becomes
  `unavailable: quarantined` until resolved. (A resumed session that restarts
  ordinals in a new file lands here: safe failure pending live evidence.)
- Validation per record: non-negative integers ≤ 2^53; `total = input +
  output`; `cached_input ≤ input`; `reasoning_output ≤ output`. Failure →
  record kept with `accepted = 0`, reason `invariant_violation`.
- Certified versions: a list in code, empty until the S5 live run; the S5
  card adds `0.154.0` only on passing evidence. Uncertified `cli_version`: the
  source row and record count are stored, counters are not persisted
  (`accepted = 0`, reason `cli_version_uncertified`).
- Reconciliation: Σ accepted `usage` per session vs the last
  `thread_token_usage`; any difference → `codex_discrepancy(kind =
  'thread_total')`. Last `token_count.total_token_usage` differing from Σ →
  `codex_discrepancy(kind = 'token_count_total')` (expected after
  compaction; informational, never used for sums).
  A fork (`history_base.thread_id`) reports both totals including its
  origin's thread total at the fork point: that total (the origin's last
  certified `thread_token_usage` before `history_base.end_byte_offset`) is
  subtracted first. An origin not collected up to the fork point records
  `codex_fork_reconciliation.state = 'origin_not_collected'` and no
  discrepancy (A8, sidecar stream `ingest` 0008).
- Rate limits: `codex_rate_limits(session_id, ordinal, limit_id,
  used_percent` as decimal string, `window_minutes, resets_at, plan_type,
  observed_ts)`; read only by the quota windows behind M40 (contracts-accounting.md §5); no semantics certified beyond storage.
- Turns: `codex_turns(session_id, turn_id, model, effort, duration_ms,
  time_to_first_token_ms)`.

**Binding rule.** A rollout binds to attempt A iff all hold: (1) its path is
under `<A.effective_profile.execution_home>/.codex/sessions/`; (2)
`session_meta.cwd` equals, or is a descendant of,
`<project>/.state/worktrees/<A.attempt_id>/` (textual comparison after
rejecting `..` components; `src/domain/worktrees.rs` layout); (3)
`session_meta.timestamp ≥ A.decided_unix_ms`; (4) exactly one attempt
satisfies 1–3. Zero matches → `unbound`; more than one → `ambiguous` (both
recorded, counted in no attempt). Guardian rollouts (`model =
codex-auto-review`) follow the same rule; if bound they form a separate model
segment of that attempt, otherwise they stay unbound. Binding is recomputed,
never guessed from PID, title or file time.

S5 refinements:
- CLI `telemetry <slug> collect` (creates the sidecar, then prints) and
  `telemetry <slug> usage` (read-only; one text line per attempt and per
  rollout unless `--json`); both print `{attempts, sessions}` as JSON,
  collect adds `collected{files, bytes, records, reevaluated, budget_exhausted}`. Budget:
  256 MiB per CLI collect, 8 MiB per ticker collect (once per 300 s per project,
  `HERDR_PROJECTS_TELEMETRY_COLLECT_SECS`, `0` off; no sidecar is created for a
  project without a Codex home). Lines over 16 MiB are skipped whole. The
  ticker's telemetry pass (collect, then every lane's tick) runs on its own
  thread, one project at a time; the ticker's pass never waits for it
  (certificate-scale.md §5).
- Homes scanned = `execution_home` of Codex `effective_profile`s in
  `attempt_inputs` plus Codex `native_profiles`; symlinks are not followed,
  depth ≤ 4. Paths are stored as `sha256:` digests (`path_digest`,
  `home_digest`).
- File position: `collect_offsets(path_digest, device, inode, byte_offset,
  records, rate_limits, model, effort)`, advanced in the same sidecar
  transaction as the rows. A changed device/inode or a file shorter than the
  offset re-reads from 0; existing keys then dedupe or quarantine.
- Only the first `session_meta` of a file counts; records before it advance
  the ordinal but are not stored. `source` keeps a string, or the first key of
  an object source. Rule 2 is evaluated at read time on the raw `cwd` into
  `cwd_attempt` (the path component after `<project>/.state/worktrees/`); the
  raw `cwd` is not stored.
- Precedence: `invariant_violation` before `cli_version_uncertified`; any
  record not accepted stores `NULL` counters. `payload_digest` is stored for
  every record (needed for replay/quarantine). Rate limits and turns are
  stored for uncertified versions (metadata, no counters); `thread_usage` /
  `token_count_usage` and discrepancies only for certified versions.
- Discrepancies compare all six usage fields; the row keeps
  `summed_total`, `reported_total` and the differing field names, and is
  deleted when the difference disappears. `used_percent` is the JSON number's
  text (`37.5`). Rate-limit `ordinal` = position among `token_count` events
  with `rate_limits`; `observed_ts` = the line's `timestamp`.
- Attempt `usage`: `collection_not_run` without a sidecar, `adapter_absent`
  for non-Codex kinds, then `not_bound`, `quarantined`,
  `cli_version_uncertified` (optional `detail: "rollout_unavailable"`),
  `records_not_accepted` (any bound record failed validation), else sums of
  accepted records plus `records`.
- Certified: `0.154.0` (live run, [codex-live-0.154.0.md](codex-live-0.154.0.md)).
  Re-evaluation: every collect (CLI and ticker) re-reads from byte 0 each
  rollout whose `cli_version` is now certified and that holds rows stored
  while it was not (at or before its offset). A row with the same key and
  `payload_digest` is evaluated again (validation above) and its counters
  stored; any other key dedupes or quarantines as usual, so nothing is counted
  twice. `collected.reevaluated` counts rows accepted this way; migration 0002
  adds `rollout_sources.reevaluation`. A source whose rollout is no longer
  found under a scanned home is marked `reevaluation = 'rollout_unavailable'`;
  until re-read, a session holding such rows keeps attempt usage (with
  `detail: "rollout_unavailable"` when its rollout is gone) and the S6
  metrics at `cli_version_uncertified`, never `0`.
- Rule 4 `ambiguous` cannot arise within one project because worktree
  directories are attempt-unique; it stays as a guard. Several rollouts bound
  to one attempt (guardian, resume) all count.

## 6. Metric subset

Definitions follow doc 07; adaptations are marked. Window = activity or
terminal cohort as stated; every result carries `numerator`, `denominator`,
`coverage`, `excluded` counts and `definition` `"<Mnn>.slice-v1"`.

Let `T` = tasks with a terminal disposition in the window: acceptance
evidence (below), or `tasks.state ∈ {succeeded, failed, cancelled}`. `A ⊆ T`:
route `verify_only` → a `verified_results` row for the current contract
revision; route `verify_then_integrate` → an `integrated_commits` row reached
from such a verified result. Tasks with neither are **open** and reported
separately. `succeeded` without evidence stays in `T \ A` and is counted as
`succeeded_without_evidence`.

| ID | Formula | Unknown vs zero |
| --- | --- | --- |
| M02 Task acceptance rate | `count(A) / count(T)` | `count(T)=0` → null `empty_denominator` |
| M07 Attempt amplification | attempts of tasks in `T` / `count(A)` | `count(A)=0` → null; attempts without a decision counted and flagged |
| M08 Input consumption | Σ accepted `input_tokens` of bound records, activity window | No certified bound session → `unavailable: no_certified_source`; 0 only if a certified bound session had zero records |
| M09 Output consumption | Σ accepted `output_tokens`; `reasoning_output_tokens` shown as subset, not added | same as M08 |
| M13 Usage coverage (adapted: attempt-level) | terminated Codex attempts with complete usage / terminated Codex attempts in window | complete = ≥1 bound session, none quarantined/ambiguous, all records accepted, certified version. Non-Codex attempts reported as `adapter_absent` count, not in denominator |
| M15 Effective-model coverage | accepted records with a non-null reported `model` / accepted records | requested model (null) never qualifies |
| M31–M33 | lane B attention (contracts-accounting.md §6) | before any sample `unavailable: attention_not_collected` |
| M40 Quota headroom at dispatch (extended, `M40.quota-windows-v1`) | per decision and limit window: remaining percent of the latest trusted quota observation of the attempt's account with `observed ≤ decided_unix_ms` (contracts-accounting.md §5), with age and freshness | decision- or window-level `unavailable` with a reason; native units; never summed or averaged across accounts, limits or services |

Worked M02/M07 example (golden E2E): tasks t1 (verify_only, verified), t2
(verify_then_integrate, verified and integrated), t3 (verify_then_integrate,
verified, integration `blocked`), t4 (`failed`), t5 (queued, no evidence).
Attempts: t1 ×1, t2 ×2, t3 ×1, t4 ×2, t5 ×1. `T = {t1, t2, t4}` (t3 and t5
open) → M02 = 2/3; M07 = (1+2+2)/2 = 5/2.

S6 refinements:
- Shape: `{metrics: {Mnn: {...}}, since_unix_ms, tasks: {terminal, accepted,
  open, succeeded_without_evidence}}`. Every metric has `definition` and
  `name`; `value` is a value, `null` with `reason: empty_denominator`, or
  `unavailable`. Ratios carry `numerator`, `denominator` and `value` as the
  unreduced string `"n/d"` (no floats). Sums (M08, M09) carry `value` and
  `coverage {certified_sessions, excluded}`, where `excluded` counts sources
  by `unbound`, `ambiguous`, `orphan`, `quarantined`, `cli_version_uncertified`,
  `records_not_accepted`.
- M02 `excluded {open, outside_window}`; M07 counts every attempt of tasks in
  `T` and flags `attempts_without_decision`.
- Window (`--since`, absent = all): tasks with an attempt decided at or after
  it; M13 and M40 by `decided_unix_ms`; M08/M09/M15 by `session_unix_ms`.
- M08/M09/M15 source = bound, non-quarantined, certified rollouts of a known
  attempt; none → `unavailable: no_certified_source` (also without a sidecar).
  M15 counts accepted records of that source with a non-null `model`.
- M13: without a sidecar `unavailable: collection_not_run`; otherwise
  terminated (`completed|failed|cancelled|lost`) Codex attempts, with
  `adapter_absent` (terminated non-Codex) and `incomplete` by first failing
  reason: `not_bound`, `quarantined`, `cli_version_uncertified`,
  `records_not_accepted` (a source's `records` ≠ its accepted rows).
- M40 is the extended form of `accounting quota` (contracts-accounting.md
  §5): `definition` `M40.quota-windows-v1`, `stale_after_ms` 900000,
  `decisions` in attempt order, each `{attempt_id, decided_unix_ms, service,
  account?, windows | value}`. Each window entry is `{limit_id, window_kind,
  unit, window_id, window_minutes, resets_unix_ms, observed_unix_ms, age_ms,
  value, used, freshness}` (`value` = remaining, exact decimal string;
  `freshness` `fresh|stale`), or `value: unavailable
  window_reset_since_observation` (with its age), or `secondary`
  `unavailable not_collected`. Decision-level reasons, first match:
  `adapter_absent` (non-Codex kind), `execution_home_unknown`,
  `collection_not_run` (no sidecar), `ledger_not_synced`, `no_observation`,
  `no_trusted_observation`. The report reads the quota tables built by the
  last `accounting sync` (the ticker syncs on every telemetry pass); it never
  derives or writes them, so before the first sync every Codex decision is
  `unavailable: ledger_not_synced`, and rate-limit rows collected after the
  last sync appear after the next one. With a sync, each decision equals the
  one `accounting quota --json` prints.
- `--text` prints one line per metric; M40 one per decision and limit window
  (`M40 quota_headroom_at_dispatch <attempt> <limit> <kind> remaining <v>%
  age_ms=<n> <freshness>`, or `... <limit> <kind> n/a (<reason>)`), or one per
  decision without windows (`... <attempt> n/a (<reason>)`); the fleet pane
  shows the same lines. Unknown is `n/a (<reason>)`. Text is the default; `--json` prints the object above.

## 7. Privacy allowlist and excerpts

Default: metadata only (IDs, digests, enums, counters, timestamps, durations,
provider and parent-session identifiers, tool call ids, tool names and
namespaces, call and exec statuses, exit codes and exec startup durations,
MCP server and tool names with their read-only and error flags and
durations, subagent and collab item ids and statuses, turn abort reasons,
and fork points).
Allowed free text is limited to **excerpts** of: verification/integration
`reason` (≤128 already), operator dispatch `note`, finding titles when that
producer exists. Excerpt rule, applied before any write or display:

1. Take the first line only; replace control characters with space.
2. Replace the user's home directory prefix and any `/home/<user>` or
   `/Users/<user>` prefix with `~`.
3. Strip URL query strings and fragments (`?…`, `#…` after a URL).
4. Mask token-like strings with `[redacted]`: runs of ≥20 chars from
   `[A-Za-z0-9_\-+/=]` containing both letters and digits; `sk-…`, `ghp_…`,
   `github_pat_…`, `xox?-…`, `AKIA…`; `Bearer <x>`; `key=`/`token=`/
   `secret=`/`password=` values.
5. Truncate to 160 Unicode scalar values (append `…` within the limit).

Never collected: prompts, briefs, transcripts, agent messages, tool
input/arguments/output (MCP `arguments` and `result` content included),
subagent paths and collab agent records, commands and their working directories, parsed
commands and output (stdout, stderr, aggregated or formatted), diffs, file
contents, reasoning text, environment values. Tool metadata is read from
`response_item` and `item_completed` only through a typed allowlist that
never deserializes these fields.
Paths in the sidecar are stored as digests except `cwd`, which is stored with
rule 2 applied. Project opt-in for richer content is not built; until it is,
no code path may read it.

## 8. Deferred (explicit)

M33 typed attention reasons; transactional outbox (TM0.4);
budget bridge enforcement and any change to `UnknownUsagePolicy` (TM2.4;
the shadow bridge landed, contracts-accounting.md §14);
candidate-group races; resume/compaction
reconciliation beyond quarantine.

Planned, no longer deferred (owner decision 2026-09-30): harness coverage beyond Codex: DG4a has landed a fixture-certified OTLP/HTTP JSON receiver (contracts-collection.md DG4a); next are launch-env wiring and native adapters for Claude Code, Gemini CLI, OpenCode and Grok, and a second wave (Cursor, GitHub Copilot CLI, Amp, Aider). Cards DG4a–DG4f in [phase2-lanes.md](phase2-lanes.md).

Landed since this list was first written: the worker submission spool and
Git quarantine (docs/reviews/2026-09-29-worker-isolation.md), randomized
assignment policies (`deterministic.v1`, uniform, epsilon-greedy, Thompson;
contracts-evaluation.md §9, migrations 0065–0066), configuration staleness
(M50) with health rules and alerts (contracts-health.md), and the TM4.8
workspace: coordinator digest, inbox alerts, the fleet pane, the signed
weekly-report and replay routines, and the `telemetry` sidebar suffix for
legacy threads and canonical attempt panes (workspace.md §6, §10).

Landed since phase 1 (see the lane contracts): retention, holds,
tombstoned deletion, backup and restore (TM5.3,
[operations-runbook.md](operations-runbook.md)), collector bindings
(contracts-collection.md), usage ledger, session graph and rate-card
estimates (contracts-accounting.md §2–§4), proxy signals and integration
outcomes, candidate groups and selection (contracts-quality.md §1–§4),
attention intervals (contracts-accounting.md §6), provider charges, invoice
allocation, dated currency conversion, `as_of`, the shadow budget bridge and
M04/M11/M12 (contracts-accounting.md §12–§14),
delegated code_review authority, reviewer-signed acceptance decisions and
M24 review discovery efficiency (contracts-review.md §10), review launch
from an assignment with the blind brief and the worker receipt channel
(contracts-review.md §11), the trusted reviewer-signer process
(contracts-review.md §12, no migration), Codex session metadata
(contracts-collection.md A4), Codex tool/exec metadata (contracts-collection.md
A6), Codex MCP, subagent, aborted-turn and fork metadata and fork
reconciliation (contracts-collection.md A8), review opportunities, sessions
and completions,
finding triage, claims and duplicate merge/unmerge history, repair
opportunities, exact-candidate fix verification and integration links,
reopen lineage, causal introduction decisions and fractional role credit,
versioned review protocols, second-review passes with incremental yield
(M28, descriptive), preregistered randomized/matched review experiments
with exclusions and crossover, the seeded-defect registry, clean controls,
reveal/discard lifecycle, the seeded-candidate integration guard and
M43/M44 (contracts-review.md §5–§8), fleet efficiency M34–M37 with the
coordinator scope and allocation rule v1, owner-recorded supersession
reasons (0060) and worker-observed rebases (contracts-accounting.md §10).

## 9. Verifier-owned run metadata (DG6, canonical 0068)

Migration 0068 (`SCHEMA = 68`) adds nullable `verification_runs.metadata`.
The verifier writes `verification-metadata.v1` in the same immutable run
transaction. Historical rows remain NULL; exact replay never executes again
or invents metadata. This is the verifier's own evidence, not a telemetry
write: collectors still open canonical state read-only. It belongs here
because a later collector cannot reconstruct execution-time host load or
per-test outcomes from a canonical verdict. It is advisory metadata, outside
the trusted receipt digest and every acceptance/reuse decision.

Allowlisted fields: sample time, host one-minute load average, executing
project verification count, Linux CPU/IO PSI `some avg10`, fixed absence
reasons, sanitized test name and pass/fail/ignored outcome. Output bodies,
failure descriptions and XML attributes other than test identity are never
persisted. Bounds and semantics: [contracts-quality.md §6](contracts-quality.md#6-dg6-passive-verification-flakes).
Canonical metadata follows canonical retention/backup, never telemetry
pruning. Empty `verification-load.lock` is an ephemeral verifier-owned lock
file; live kernel locks are not backup data. No telemetry backup includes
canonical metadata; sidecar derived copies follow their surviving source.

Flake projections are derived only by quality collect or the quality lane tick,
never by migration or unrelated commands. Rebuilding the sidecar requires
replaying quality collection before comparing its verification metrics.

LC1 certifies Codex 0.159.2 alongside 0.154.0 without a schema change. See
[codex-live-0.159.2.md](codex-live-0.159.2.md) (draft pending steward live
reconciliation) and the installed-version scope in contracts-collection.md.
The sanitizer excludes world_state and thread_settings_applied snapshots and
new session metadata; only existing allowlisted counters are retained.

DG4i adds sidecar `ingest` **0013_muse.sql** (`muse_events`, `muse_parents`), a per-native-session
completion identity table. It is included in session retention, backup and
restore tombstone filtering. No canonical schema change.
See [contracts-collection.md DG4i](contracts-collection.md#dg4i--muse-native-sessions-fixture-certification).
DG4i also adds `accounting` **0018_muse.sql** to admit `muse` ledger sources
and their inclusive read/write normalization while preserving existing entries
and dispositions. These projections retain their existing follows-sources class.

DG4j adds sidecar `accounting` **0019_otlp_ledger.sql**: certified exact-bound
OTLP request usage projects into the existing normalized-session tables,
with stable identity, native-first attempt/harness precedence and change
capture. OTLP-derived sessions retain `sidecar.otlp` retention and full-backup
coverage; ledger/aggregate rows follow sources. No canonical schema change.
See [contracts-collection.md DG4j](contracts-collection.md#dg4j-certified-otlp-usage-in-the-accounting-ledger).

DG4k adds sidecar `accounting` **0020_otlp_devin.sql**: admits the `otlp:devin`
ledger source (cache-exclusive input normalized to `otlp-devin-exclusive-v1`)
and the Devin `api_request` clause of `otlp_ledger_sources`; rows preserved, no
new table, `sidecar.otlp` retention/backup coverage unchanged. No canonical
schema change. See [contracts-collection.md DG4k](contracts-collection.md#dg4k-devin-cli-3000113-otlp-mapping-fixture-certification).
