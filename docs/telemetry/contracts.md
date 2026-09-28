# Telemetry slice contracts (W0)

Status: frozen contracts for the thin slice only (outcome record, Codex usage
adapter, basic workspace panel). Base `main` `ebfe10e`, store schema 45.
Evidence for every "at main" statement is in [baseline.md](baseline.md);
cards in [slice-cards.md](slice-cards.md). Plan IDs (M-numbers, payload names)
follow `herdr-telemetry-plan-v1.1` docs 03 and 07. Changing any contract here
needs a new reviewed revision, not a silent reinterpretation.

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
- **Stores.** Canonical: `<project>/.state/state.db`, migrations 0049–0051.
  Sidecar: `<project>/.state/telemetry.db`, own sequence under
  `migrations/telemetry/`, mode 0600, created on first collect. No
  cross-database transaction or foreign key; sidecar rows reference canonical
  IDs by value and record `orphan` when the canonical row is missing.
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
| `eligible` | Canonical JSON array ordered as evaluated: `{configuration_id, profile_digest, status, probability_ppm}`; `status` ∈ `chosen`, `no_knowledge`, `no_approval`, `not_evaluated`; `probability_ppm` integer, chosen = 1000000, others 0 (no stochastic policy exists) |
| `chooser_kind` | `operator` (`launch_preparation::reserve`), `automatic_admission` (`admission::decide_held`), `delegated` (`reserve_delegated`) |
| `chooser_principal` | `approval:<approval digest>` / `rule:automatic-admission.v1` / `grant:<grant_id>` |
| `reason_codes` | Sorted unique array, ≤4, from: `operator_selected`, `only_eligible`, `first_matching_approval`, `delegated_grant`, `recommended`, `operator_preference`, `availability`, `exploration`, `replay`, `continuation`, `unspecified` |
| `note` | Optional operator note, excerpt rules §7, ≤160 chars; `null` if absent |
| `policy`, `seed` | Always `null` in the slice |
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
| `active_ms` | `terminal − running`; open → `censored`; no running mark → `unavailable` |
| `queue_to_launch_ms` | `launching − reserved` |
| `result` | earliest `result_submissions` for the attempt: `submission_id`, `candidate_oid`, `created_unix_ms`; none → `not_submitted` |
| `verification` | latest `verification_runs` for that submission: `accepted`/`rejected` + `reason` excerpt; none → `pending` if submitted |
| `integration` | route `verify_only` → `not_applicable`; else latest `integration_operations` state via `verified_results`; `integrated` needs an `integrated_commits` row |
| `accepted` | `true` iff the task's acceptance rule (§6 `A`) is met by evidence produced from this attempt |
| `usage` | §5 sums if a certified bound rollout exists, else `unavailable` with reason `adapter_absent` (non-Codex kind), `not_bound`, `cli_version_uncertified`, `quarantined`, `collection_not_run` |
| `attention` | always `unavailable`, reason `attention_not_collected` (S4 deferred) |

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
  candidate_oid, created_unix_ms}`; `verification` `{state:
  accepted|rejected|pending|not_submitted, reason?}` (latest run of the
  earliest submission, reason excerpted with the reader's `HOME`);
  `integration` `{state}` from the same submission: `not_applicable` unless the
  route is `verify_then_integrate`, then `not_submitted`, `pending` (no
  operation), the latest operation state, `integrated` (with an
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

## 5. Codex usage (sidecar)

**Source.** Rollout JSONL files under
`<execution_home>/.codex/sessions/**/rollout-*.jsonl` for each Codex profile's
`execution_home`. The collector reads only `session_meta`, `turn_context`,
`token_usage_record`, `event_msg` of type `token_count`, `task_started`,
`task_complete`. All other record types are skipped by type tag without
retaining any field. Read only complete lines (ending `\n`); a partial last
line is left for the next pass and the file offset is not advanced past it.

**Allowlisted fields.** `session_meta`: `id`, `timestamp`, `cwd`,
`cli_version`, `originator`, `source`. `turn_context`: `turn_id`, `model`,
`effort`. `token_usage_record`: `session_id`, `turn_id`, `response_id`,
`usage.{input_tokens, cached_input_tokens, cache_write_input_tokens,
output_tokens, reasoning_output_tokens, total_tokens}`, final
`thread_token_usage` (for reconciliation only). `token_count`:
`rate_limits.{limit_id, primary.{used_percent, window_minutes, resets_at},
plan_type}` and `info.total_token_usage` (discrepancy only). `task_complete`:
`turn_id`, `duration_ms`, `time_to_first_token_ms`. Never:
`last_agent_message`, instructions, messages, tool calls/outputs, reasoning.

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
- Rate limits: `codex_rate_limits(session_id, ordinal, limit_id,
  used_percent` as decimal string, `window_minutes, resets_at, plan_type,
  observed_ts)`; used for M40 only; no semantics certified beyond storage.
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
  `telemetry <slug> usage` (read-only); both print `{attempts, sessions}`,
  collect adds `collected{files, bytes, records, reevaluated, budget_exhausted}`. Budget:
  256 MiB per CLI collect, 8 MiB per ticker collect (once per 300 s per project,
  `HERDR_PROJECTS_TELEMETRY_COLLECT_SECS`, `0` off; no sidecar is created for a
  project without a Codex home). Lines over 16 MiB are skipped whole.
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
  `cli_version_uncertified` (optional `detail: "rollout_unavailable"`), else
  sums of accepted records plus `records`.
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
| M31–M33 | deferred | `unavailable: attention_not_collected` |
| M40 Quota headroom at dispatch | per decision: `100 − used_percent` of the latest `codex_rate_limits` row from the same execution home with `observed_ts ≤ decided_unix_ms`, plus age ms | none → `unavailable: no_observation`; percent of the native window; never summed or averaged across services |

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
  by `unbound`, `ambiguous`, `orphan`, `quarantined`, `cli_version_uncertified`.
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
- M40: `decisions` in decision order, each `{attempt_id, decided_unix_ms,
  value}`; with an observation `value` is the exact decimal string plus
  `age_ms`, `limit_id`, `window_minutes`. Same home = the rollout's
  `home_digest` equals the digest of the attempt's `execution_home`; ties on
  `observed_ts` take the higher ordinal. Unavailable reasons: `adapter_absent`,
  `collection_not_run`, `no_observation`, `unparseable_observation`.
- `--text` prints one line per metric (M40 one per decision); unknown is `n/a
  (<reason>)`. Text is the default; `--json` prints the object above.

## 7. Privacy allowlist and excerpts

Default: metadata only (IDs, digests, enums, counters, timestamps, durations).
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
arguments/output, diffs, file contents, reasoning text, environment values.
Paths in the sidecar are stored as digests except `cwd`, which is stored with
rule 2 applied. Project opt-in for richer content is not built; until it is,
no code path may read it.

## 8. Deferred (explicit)

Attention intervals and M31–M33 (S4); transactional outbox and
`CollectorBinding` process (TM0.4/TM1.1); spool; budget bridge and any
change to `UnknownUsagePolicy` (TM2.4); cost, rate cards, M04/M11/M12;
Claude and OTLP adapters; candidate groups and races; stochastic assignment
policies; proxy signals; review/finding attribution; coordinator digest,
sidebar suffix and inbox alerts; retention jobs and restore; configuration
staleness (M50); resume/compaction reconciliation beyond quarantine.
