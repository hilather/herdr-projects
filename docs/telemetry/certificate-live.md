# Live adapter certificate (TM5.2): Codex 0.154.0

Card TM5.2 (plan doc 12). Owner decision: Codex is the only
deployment-selected adapter, at the version the product certifies
(`collectors::codex::CERTIFIED = ["0.154.0"]`). This is the third live run.
It is the first with worker isolation, the submission spool and the Git
quarantine active. It follows [codex-live-0.154.0.md](codex-live-0.154.0.md) (S5),
[codex-live-0.154.0-a4.md](codex-live-0.154.0-a4.md) and
[codex-live-0.154.0-run2.md](codex-live-0.154.0-run2.md). It cites
[certificate-core.md](certificate-core.md) (TM2.6) and
[certificate-quality.md](certificate-quality.md) (TM3.5, fixture) and does not
repeat them.

| Item | Value |
| --- | --- |
| Date | 30 September 2026 (UTC) |
| Source | branch `telemetry/tm52-live-certification` from `main` `bf6c127`, release build |
| Host | Linux 7.2.3-arch1-3, rustc 1.98.0 |
| Agent | Codex CLI **0.154.0**, the resolved binary `~/.local/share/mise/installs/codex/0.154.0/bin/codex` (not the mise shim) |
| Installed but uncertified | Codex **0.158.0** is installed beside it, and mise `latest` and `0` point to it. The owner's own Codex app server runs 0.159.2 (seen in `ps` only). Neither was used. See §7, L1. |
| Herdr | stock 0.9.1; two isolated servers per run, with sockets in scratch |
| Account | the owner's Codex login (`plan_type` `pro`). A private 0600 copy lived in each disposable execution home. It was never read and was deleted at the end of each run. |
| Invoice / billing export | **unavailable**: no provider export exists for this account mode (§4.3) |

## 0. Verdict

- **Isolation works with real Codex**, with one Codex-side limitation. The
  worker sandbox, the login copy inside the execution home, the submission
  spool (`result submit`, `review session`, `review submit`) and the Git
  quarantine (commit → import → verify → integrate) all worked with real
  Codex 0.154.0. No isolation was weakened.
  - The limitation: Codex's *own* `workspace-write` sandbox refused the
    worktree commit (EROFS on the linked worktree's gitdir). This happened
    even though the repository's Git common directory was an explicit Codex
    writable root (§2.2).
  - The commit went through when Codex ran `danger-full-access` *inside*
    the product sandbox. There the product sandbox is the only isolation
    layer (run B).
- **Usage reconciles exactly.** For all 4 sessions (17 usage records), the
  collected usage equals Codex's own `thread_token_usage` and
  `token_count.info.total_token_usage` on all six counters. M08/M09 equal
  the independent rollout sums (§4). The difference is 0 tokens.
- **Newly observed live:**
  - resume appends to the same rollout (§3.1);
  - a model switch inside one thread, which gives two model segments;
  - Codex refuses a concurrent resume;
  - `reasoning_output_tokens` > 0, as a subset of `output_tokens` (§4.2);
  - a cancelled worker leaves no final event, so the collector records
    `final_event_missing` (§5).
- **Quality producers.** One real end-to-end flow ran on a disposable
  project:
  1. an isolated Codex author submitted through its spool;
  2. native verification accepted the candidate, and a real integration
     merged it;
  3. a Codex reviewer launched through D9 (blind brief) and submitted its
     receipt through its spool;
  4. the D10 signer accepted it under an owner-signed grant.

  Every other quality producer is still fixture-only (§6).
- **`codex_fields` is unchanged.** No field that is still `fixture`
  (`rate_limits.secondary.*`, `rate_limit_reached_type`,
  `function_call.status`) got a value in this run (§7).
- **Activation:** the families in §8.1 may be activated for Codex 0.154.0.
  §8.2 lists the families blocked by missing live evidence.

## 1. Runs and sessions

Each run used the scratch driver from the A4/run2 live runs, adapted: a
signed v3 contract, then `launch draft` → owner-signed approval → `launch
reserve` → `ticker run`, with the release binary of this branch. The
ticker ran with `HERDR_PROJECTS_TELEMETRY_COLLECT_SECS=10`. Every 4 s the
driver sampled Herdr `agent list` labels and ran `telemetry collect` and
`accounting observe-attention`.

The execution home's Codex config set:

- `approval_policy = 'never'`, effort `low`;
- `sandbox_workspace_write.writable_roots` = the repository's Git common
  directory plus the attempt's spool and output directory;
- `shell_environment_policy.set.PATH` = the product binary's directory
  plus `/usr/bin:/bin` (§2.4).

Each run used one disposable SHA-256 repository and one project.

| # | Run | What | Codex process | Model usage |
| --- | --- | --- | --- | --- |
| 1 | A | author worker **A** (TUI, isolated; Codex `workspace-write`). Brief: write a marker, commit, run a helper that builds the result document from `HEAD` and runs `result submit`. | yes | 4 records |
| 2 | A | `codex exec resume -m gpt-5.6-luna <A>`, while A's TUI was still alive | yes | **none**: refused before any model request (§3.2) |
| 3 | A | reviewer **R** of A's submission (TUI, isolated, launched through D9) | yes | 3 records |
| 4 | B | author worker **A** (TUI, isolated; Codex `danger-full-access` inside the product sandbox), same brief | yes | 4 records |
| 5 | B | `codex exec resume -m gpt-5.6-luna <A>` after the product ended A | yes | 1 record, appended to A's rollout |
| 6 | B | reviewer **R** of A's submission (TUI, isolated, D9) | yes | 5 records |

**Codex sessions: 6 processes (the budget is 6), 5 of them with model
usage.** 4 rollout files hold 17 usage records. No subagent, fork or
guardian occurred. Both runs were finished and cleaned up by the driver.

## 2. Isolation with real Codex

### 2.1 What worked (both runs)

| Mechanism | Evidence |
| --- | --- |
| Launch through the sandbox | All 4 TUI workers started in the product sandbox after gate release (Herdr labels `idle` → `working` → `idle`). There was no `worker isolation refused` exit (125). |
| Login copy in the execution home | Codex authenticated from `<execution home>/.codex/auth.json` and made model calls (§4). The sandbox hid the owner's own login: a probe inside the sandbox reported `~/.codex/auth.json` hidden and the worker's own copy present. |
| Owner secrets hidden | Probe inside the product sandbox (run B, no Codex sandbox): `~/.ssh` lists 0 entries, `~/.codex/auth.json` hidden, the Herdr socket hidden, `state.db` not writable, uid 0 (nested user namespace). |
| Private `/tmp` | Run B probe: `/tmp` holds only `claude-1000`, the first path component of the lab, kept as designed. In run A the probe ran under Codex's own sandbox and also saw Codex's synthetic mount targets. |
| Submission spool: `result submit` | Both authors' `result submit` went through `.state/spool/<attempt>` and printed the receipt. The ticker recorded `result.submitted` (1 per run). `spool.request_denied`: 0. |
| Submission spool: review channel | Both reviewers ran `review session` and `review submit` through their spools. The completion's `recorder_principal` is `worker:<attempt>`, with `trust: proposal` and `coverage_basis: declared`. Spool denials: 0. |
| Git quarantine (run B) | The author's commit landed only in `.git-quarantine/<attempt>/repo-00/upper`. The spool ingest imported the candidate before recording the submission. After termination `import.json` = `{"state":"imported","commit":<candidate>}` and the attempt branch = the candidate. `master` and every other ref are unchanged; `git fsck --strict --no-dangling` is clean. |
| Verification and integration (run B) | `result verify` (the contract's `clean` policy, run in isolation) returned `accepted`. `result integrate` returned `integrated`: `refs/heads/integration` is a merge of the candidate, and M36 is 0/1. |
| Git quarantine (run A) | The worker's commit failed (§2.2), so `import.json` = `{"state":"unchanged"}`. The helper then submitted `HEAD` = the base. The product recorded that submission, and verification rejected it (`required_output_missing`). This is the correct outcome for a no-op candidate. |

### 2.2 Codex's own sandbox refused the commit (run A); not a product break

In run A's author, `git add … && git commit` failed with exit 128:
`Unable to create '<repo>/.git/worktrees/repo-00/index.lock': Read-only file
system`. The failing call produced no `CommandExecution` item; only its
`custom_tool_call_output` carries the exit code. The worker then ran the
submit helper as told.

Codex's recorded `turn_context.sandbox_policy` and `permission_profile`
listed `<repo>/.git` as `write` and the worktree as `write`, with read-only
carve-outs only for `<root>/.git`, `.agents` and `.codex`.

Reproductions without a model call, using `codex sandbox`:

| Setup | Result |
| --- | --- |
| Host, sandbox mode set with `-c`, `writable_roots` from the config file or `-c` | the commit works |
| Host, sandbox mode and `writable_roots` from the config file only | EROFS, the same error |
| Nested user and mount namespaces imitating the product layout (overlay on the common directory, recursive read-only repository, read-only `.git` pointer, private `/tmp`) | the commit works |

So the refusal comes from how Codex 0.154.0 resolves its own sandbox policy
for a linked worktree's gitdir. The product sandbox is not the cause. The
exact trigger inside Codex was not isolated in this run.

**Root cause, found after the run (F2, fixed):** Codex binds its writable
roots with bubblewrap, shallowest first, each followed by its protections,
and it protects the Git directory a linked worktree's `.git` pointer names
(`<common>/worktrees/<id>`) read-only unless that path is itself a writable
root. The attempt worktree lies deeper than the common directory, so the
read-only administrative directory is bound on top of the writable common
directory. The nested-namespace imitation above used a worktree at the same
depth as the repository, which hides the order dependence; the "config file
only" row failed because `codex sandbox` ignores `sandbox_mode` from the
file. The product now names the administrative directory as a Codex writable
root for every isolated Codex attempt. Evidence and the regression test are
in [the worker isolation review](../reviews/2026-09-29-worker-isolation.md#codexs-own-sandbox-inside-the-worker-sandbox-live-run-card-f-commit).

Run B kept the product sandbox unchanged and set Codex to
`danger-full-access`. The commit then landed in the quarantine as designed
(§2.1).

The reviewers (both runs) kept Codex `workspace-write`. They needed no Git
writes, and their spool writes succeeded. Here the spool lies under `/tmp`,
which Codex's `workspace-write` always makes writable. A deployment with
its projects root under `$HOME` depends on the spool being in
`writable_roots`, as configured here; that combination was not exercised.

### 2.3 Isolation limitations observed

- **L-iso-1:** a lab under `/tmp` keeps its whole first-level directory
  (`/tmp/claude-1000`) visible in the worker's private `/tmp`. This is by
  design: "a needed directory is kept by its first component". Writability
  of siblings there was not probed. A deployment root under `$HOME` avoids
  this.
- **L-iso-2:** the product sandbox cannot constrain Codex's sandbox choice.
  With Codex `workspace-write`, a Codex worker cannot commit in an attempt
  worktree (§2.2). With `danger-full-access`, Codex relies on the product
  sandbox alone. *Since fixed for the commit (F2):* with the product's
  writable-roots override, `workspace-write` commits; the mode itself stays
  the deployment's choice.

### 2.4 Product gap: the reviewer brief names a bare `herdr-projects`

The worker environment's `PATH` is `/usr/bin:/bin`
(`worker_supervision`), but the D9 brief tells the reviewer to run
`herdr-projects … review session|submit`. The product binary lives under
`target/release` (a deployment would use `~/.local/bin`), so the bare name
cannot resolve.

This run added the binary's directory to the agent's shell `PATH` through
Codex `shell_environment_policy`, a deployment config in the execution home.
Both reviewers then used the bare name successfully. Without that, a
reviewer would fail or have to search for the binary. See follow-up F1.

## 3. Resume, model visibility, concurrency

### 3.1 Resume appends to the same rollout (live, run B)

`codex exec resume -m gpt-5.6-luna <session>` ran after the product had
ended the attempt (termination observed). It appended to the **same file**:

- no new rollout and no second `session_meta`;
- ordinals continue (0–48 over 49 lines);
- then `event_msg/thread_settings_applied`, a second `world_state`, a new
  `task_started`/`turn_context` (`model` `gpt-5.6-luna`, effort `low`),
  one `token_usage_record` and `task_complete`.

**No replay:** the session holds 5 records with 5 distinct `response_id`s,
and every `thread_token_usage` equals the file's running sum.

The collector counted each record once (attempt 80524 = 61015 + 19509). It
found no discrepancy and no quarantine, and the final event is `complete`
on the resumed turn. `accounting sessions` shows two model segments:
`gpt-6-astra` (4 entries, 61015) and `gpt-5.6-luna` (1 entry, 19509).
`compare` notes `mixed_model_allocation`.

This supersedes certificate-core R10's "resume … not observed live" for the
same-file shape. A resume that writes a new file (the fixture
`resumed.jsonl`) is still not observed live, and an ordinal restart stays
quarantined.

Pinned by the new hand-written fixture
`tests/fixtures/telemetry/codex-conformance/live3-resume.jsonl`
(`LIVE3LEAK_*` sentinels) and the test
`live_run3_same_file_resume_with_a_model_switch_counts_each_record_once`.

**Limitation L2:** the resumed turn ran after the attempt ended and outside
the product, yet it is bound to the ended attempt by execution home and cwd.
Its 19509 tokens count in that attempt's usage and M08/M09, and nothing
flags usage recorded after termination (follow-up F3).

### 3.2 Concurrent resume is refused (live, run A)

While A's TUI still held the thread, `codex exec resume` exited 1 with
`thread/resume failed: thread … already has an active writer` (`code
-32600`). It wrote no rollout line and made no model request. One writer per
thread holds in 0.154.0, so a live TUI and a resumed exec never interleave
records in one file.

### 3.3 Visible models

- The execution home's `models_cache.json` lists 5 `visibility: list` slugs
  (`gpt-6-astra`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`, `gpt-5.5`)
  and 2 hidden ones (`gpt-reserve`, `codex-auto-review`).
- The default model was `gpt-6-astra` in every TUI turn. `-m gpt-5.6-luna`
  was honoured and reported in `turn_context.model`.
- M15 (effective model reported) was 7/7 in run A and 10/10 in run B.
- The model cache is not collected. The requested model is
  `not_in_query_service` in `view models`, so the effective model comes only
  from `turn_context`.

## 4. Reconciliation

### 4.1 Collected usage vs Codex's own totals

For each session, the collected value is the per-attempt usage. Columns are
input / cached / cache_write / output / reasoning / total.

| Session | Records | Rollout sum = last `thread_token_usage` = last `token_count` total | Collected (per attempt) | Difference |
| --- | --- | --- | --- | --- |
| A-author `…5d6059` | 4 | 68890 / 45952 / 0 / 205 / 34 / 69095 | same | 0 |
| A-reviewer `…484d6a` | 3 | 55476 / 41856 / 0 / 1552 / 18 / 57028 | same | 0 |
| B-author+resume `…131ebd` | 5 | 80326 / 58624 / 0 / 198 / 18 / 80524 | same | 0 |
| B-reviewer `…731ee0` | 5 | 98340 / 87808 / 0 / 1579 / 45 / 99919 | same | 0 |

- **M08:** 124366 (run A) and 178666 (run B), each the independent sum of
  inputs.
- **M09:** 1757 and 1777.
- **M13:** 2/2 in each run. `collect` twice gives the same result, and a
  re-read counts 0 new records.
- No `codex_discrepancy` row, no quarantine, no repeated `response_id`.

### 4.2 Counter conventions (live)

- `total = input + output` held in all 17 records.
- `reasoning_output_tokens` was **> 0 for the first time live** (maximum
  45 in one record). It is included in `output_tokens`, never added to it.
- `cached_input_tokens ≤ input_tokens` held everywhere.
- `cache_write_input_tokens` was 0 in every record. Its convention stays
  uncertified (caveat `overlap_with_input_not_certified`).
- Codex reports no cost, credits consumption or currency. `credits` is an
  object that is not collected.

### 4.3 Consumption exports, invoices, prices

- **Invoice or billing data: unavailable.** The account mode (ChatGPT `pro`
  plan) has no per-usage invoice or charge export that the product can
  read.
- There are no rate cards (owner decision: none real), so M04, M11, M12,
  M14, M24, M34 and M37 are `not_priced` / `no_provider_charges`. No
  billing truth is inferred from token counts and no tokens from plan
  units.
- The only consumption export the adapter reads is the rollout itself.
  `rate_limits.primary` is plan units in percent, never tokens: it read
  `used_percent` 46.0, `window_minutes` 10080 and `resets_at` 1791049774
  (unchanged from run2's window) in all 17 snapshots.
- `secondary` and `rate_limit_reached_type` were `null`. M40 for run B's
  reviewer: primary 54 % remaining, age 22 s, `fresh`. For the first
  dispatch of each fresh sidecar M40 is `no_observation`.

## 5. Product surfaces on the real data

All of these ran with exit code 0 on both projects:

- `collect`, `usage`, `attempts`;
- `report` (text and JSON), `query` (M01–M50, text and JSON);
- `view project|models|reviews|cost|health`;
- `export` (JSON and CSV);
- `health evaluate|alerts`, `compare --metric M02,M07`;
- `accounting sync|sessions|entries|tools|quota|attention|cost|charges|fleet`;
- `quality collect|report`, `review show|report|authority show`,
  `analytics refresh`.

Findings:

- **Operator cancel mid-turn leaves no final event.** Run A's reviewer was
  cancelled through the product (`task cancel-attempt`) while its turn was
  still running. Codex wrote neither `task_complete` nor `turn_aborted`.
  The session showed `final_event: open`, and after 600 s idle it showed
  `missing`, with a `final_event_missing` coverage gap. That is a false
  "lost event": the product itself ended the worker. `health evaluate`
  opened no alert. Follow-up F4.
- **`M16` text rendering.** In `report --text` and the `query` text form,
  M16 prints `n/a (unknown)`, while the JSON contract and `accounting tools`
  give `status: available`, `issued` 7 and `executed` 7 (run B), with the
  `accepted` stage `inferred`/`unknown`. The value is correct but the text
  is misleading. Follow-up F5.
- **Review tasks enter lifecycle metrics.**
  - Run B: M02 is 1/2 and M07 is 2/1, because the review task (cancelled
    after its receipt) is a terminal, non-accepted task in the terminal
    cohort.
  - Run A: M02 is 0/2 (the rejected no-op author and the review).

  Use `--task-class` or `--by` to separate them. This is a stated
  limitation, not a defect (L3).
- `compare` reported `insufficient_data` and `ranking not_supported`, with
  one configuration and one task per class. This is correct at this sample
  size, and no ranking was claimed.
- M17: 7/7 per run (only `completed`, exit 0; run2 certified `failed`).
  M18 stays `execution_duration_not_exposed`.
- M31: 0/1 and M32: 0/164391 ms (run B); there were no approvals and no
  `blocked` labels.
- In run A's reviewer one `exec` call ran several commands: 3 calls, 5
  `CommandExecution` items. That is legitimate in code mode; `issued` and
  `executed` are counted apart.

## 6. Quality producers: live versus fixture-only

| Producer (doc 10 §7) | Status in this run | Evidence |
| --- | --- | --- |
| Review launched through D9 with a blind brief, from a `--blind` assignment | **live** (×2) | `review assign --blind --candidate worker`: `blind_cross_provider.v1`, reason `no_cross_provider_eligible` and `same_family: true`, because Codex was the only candidate. Then the snapshot `--review-opportunity`, `launch draft`/approval/`reserve`. The session was started at launch by `service:launch` with `matches_assignment: true`. |
| Reviewer receipt through the spool | **live** (×2) | `review session`/`review submit` inside the sandbox. The completion has `outcome: completed`, `findings_submitted: 0`, `recorder_principal: worker:<attempt>`, `trust: proposal`. M20 = 1/1 (`completed_empty` 1) in each run. |
| Delegated acceptance by the D10 signer | **live** (×2) | `signer init` → the owner signs the grant (`code-review-authority@herdr-projects`) → `authority import` → `signer run --once`. It **accepted** with rule `all_rules_passed` at `ledger_seq` 5, then the grant was exhausted (1/1). The audit line carries the policy digest; the principal is `reviewer:carol` and the authority `delegated_code_review.v1`. |
| Native verification receipt | **live** (run B `accepted`; run A `rejected: required_output_missing`) | `result verify` under the contract's policy, in the isolated verifier. M45 (proxy): 1/1 in run B and 0/1 in run A. |
| Integration receipt | **live** (run B) | `result integrate` → `integrated`; M36 = 0/1. |
| Worker result submission through the spool, with a quarantine import | **live** (run B) | §2.1 |
| Owner triage of findings (M21–M23), duplicates and splits | **fixture-only** | Neither reviewer reported a finding, so nothing was triaged live. |
| Repair binding, verified fixes, reopenings (M25–M27), attribution (M29) | **fixture-only** | No repair attempt was run live. |
| Skeptical second review (M28), protocols and experiments | **fixture-only** | Not run live. |
| Seeded defects (M43/M44) | **fixture-only** | Not run live. The seeds need the owner's evaluation authority and replay tasks. |
| Candidate groups (M41/M42) | **fixture-only** | Not run live. |
| Main-branch check (M46), survival and revert (M47/M48), flaky tests | **no producer / censored** | M46 `no_main_check_producer`. M47/M48 `empty_denominator`: a fresh integration is inside the horizon. |
| Cross-provider reviewer | **unavailable** | Codex-only deployment: every blind assignment is same-family. |

## 7. Field certificate (`collectors capabilities`, codex `rollout_jsonl`)

- "This run" says whether the field had a non-null value in this run's 4
  rollouts.
- `certified` is the product's declared state. It is **unchanged** by this
  run: no field left `fixture`, and nothing was demoted.
- Earlier evidence is in the three live documents.

**Collected and live.** Every collected field below is `certified=live`.

| Field(s) | This run | Limitations (caveats) |
| --- | --- | --- |
| `line.timestamp` | value | `envelope_occurred_unix_ms` |
| `session_meta.{id, timestamp, cwd, cli_version, originator, source, model_provider, session_id, thread_source}` | value (`cli`, `codex-tui`, `openai`, `user`, 0.154.0) | `session_id`: `child_reports_parent_session`; `thread_source`: `observed_user_guardian_review_subagent`; `cwd`: home-redacted |
| `session_meta.{forked_from_id, forked_from_ordinal_exclusive, history_base.*}` | absent (no fork; run2 evidence) | `fork_thread_total_includes_origin`, `origin_file_length_at_fork` |
| `session_meta.{subagent_kind, subagent_detail, subagent_parent_thread_id, subagent_depth, parent_thread_id}` | absent (no child; A4 and run2 evidence) | `from_source_subagent`, `observed_guardian_only`, `observed_guardian_and_thread_spawn` |
| `turn_context.{turn_id, model, effort}` | value; `model` switched inside one thread (`gpt-6-astra` → `gpt-5.6-luna`) | none; model segments are live (§3.1) |
| `task_started.turn_id` | value | none |
| `token_usage_record.{session_id, turn_id, response_id}` | value | `child_reports_parent_session` |
| `token_usage_record.usage.*` (6 counters) | value; reasoning > 0 for the first time | `cache_write_input_tokens`: `overlap_with_input_not_certified` (always 0 live); reasoning is a subset of output |
| `token_usage_record.thread_token_usage.*`, `token_count.info.total_token_usage.*` | value; reconcile with 0 difference (§4.1) | `reconciliation_only` |
| `token_count.rate_limits.{limit_id, plan_type, primary.used_percent, primary.window_minutes, primary.resets_at}` | value (`codex`, `pro`, 46.0, 10080, fixed `resets_at`) | `semantics_not_certified`; plan units, never tokens; `resets_at` jitter tolerance (run2) |
| `task_complete.{turn_id, duration_ms, time_to_first_token_ms}` | value; absent for a turn the product cancelled (§5) | none |
| `turn_aborted.{turn_id, reason, duration_ms}` | absent (run2 evidence) | a product cancel writes **no** `turn_aborted` (§5) |
| `custom_tool_call.{call_id, name, status, …passthrough.turn_id}`, `custom_tool_call_output.call_id` | value (`exec`, `completed`) | `status` says only that the call was made (run2) |
| `function_call.{call_id, name, namespace, …passthrough.turn_id}`, `function_call_output.call_id` | absent (run2 evidence) | none |
| `item_completed.{thread_id, turn_id, item.type, item.id, item.status, item.source, item.exit_code}` | value (`CommandExecution` `completed`/0, `unified_exec_startup`; `AgentMessage`, `Reasoning`, `UserMessage` typed only) | `typed_items_only`, `observed_completed_failed`, `command_execution_only`; a failed call can have **no** item (§2.2) |
| `item_completed.item.duration.{secs, nanos}` | value | `startup_not_run_time` (M18 stays unavailable) |
| `item_completed.item.{server, tool, readOnlyHint, result.isError}` | absent (run2: local stub) | `mcp_tool_call_only` |
| `item_completed.item.{agent_thread_id, sender_thread_id, receiver_thread_ids}` | absent (run2 evidence) | `subagent_activity_only`, `collab_agent_tool_call_only` |

**Collected, certified `fixture` only.** None of these got a value here, so
they stay fixture-only.

| Field | This run | Why it cannot be observed on demand |
| --- | --- | --- |
| `token_count.rate_limits.secondary.{used_percent, window_minutes, resets_at}` | `secondary` is `null` in 17 snapshots | This plan reports no secondary window. |
| `token_count.rate_limits.rate_limit_reached_type` | `null` in 17 snapshots | It needs a real rate-limit hit (no load generators). |
| `function_call.status` | absent: no `function_call` here, and every live one had no status | not written by 0.154.0 so far |

**Not collected.**

- `not_collected`: `session_meta.{agent_nickname, agent_role}`,
  `turn_context.{cwd, approval_policy}`, `task_started.started_at`,
  `token_usage_record.{thread_id, root_turn_id, turn_token_usage}`,
  `token_count.info.{last_token_usage, model_context_window}`,
  `token_count.rate_limits.{limit_name, credits}`,
  `task_complete.{started_at, completed_at}`,
  `turn_aborted.{started_at, completed_at}`,
  `custom_tool_call.{id, …create_time}`,
  `function_call.id`, `*_output.id`,
  `item_completed.{started_at_ms, completed_at_ms, item.process_id, item.cwd,
  item.client_id, item.phase, item.kind, item.agents_states}`.
- `content_forbidden`: `session_meta.base_instructions`,
  `turn_context.{collaboration_mode, user_instructions}`,
  `task_complete.last_agent_message`,
  `response_item.{message, reasoning}`, tool `input`, `arguments` and
  `output`, `item.{command, parsed_cmd, stdout, stderr, aggregated_output,
  formatted_output, content, arguments, result.content, agent_path,
  receiver_agents}`.

Several of these had values in this run, which shows the allowlist dropped
them. Uncollected kinds seen live: `world_state`,
`event_msg/thread_settings_applied`, `response_item/reasoning`.

**Limitations**

- **L1, version.** Only 0.154.0 is certified. The installed default (mise
  `latest`/`0`, and the shim) is **0.158.0**, which is uncertified. A
  profile that resolves the shim, or any worker that runs 0.158.0, has its
  usage gated as `cli_version_uncertified` (excluded from M08, metadata
  kept; contracts-collection.md). Activation applies to profiles pinned to
  the 0.154.0 binary only. Certifying 0.158.0 needs its own live run.
- **L2:** usage after termination (a resume) is attributed to the ended
  attempt (§3.1).
- **L3:** review tasks are in the lifecycle cohorts (§5).
- **L4, sample size.** 4 sessions and 17 records on one account and one
  host. Nothing here is a scale or throughput claim (TM5.1).

## 8. Activation

### 8.1 May be activated (Codex 0.154.0 pinned profiles, with the stated restrictions)

| Family | Metrics | Live evidence |
| --- | --- | --- |
| Consumption | M08, M09, M13, M15 | §4, 0 difference; model segments (§3.1); S5, A4, run2 |
| Tools | M16 (`issued` and `executed`; `accepted` stays `inferred`), M17 | this run 7/7 ×2 and run2 `failed`; M18 stays unavailable |
| Attention | M31, M32 | A4 `blocked`; this run 0 waits; M33 stays unavailable |
| Quota | M40, primary window only | 17 snapshots, `fresh` headroom at dispatch; secondary blocked |
| Lifecycle (canonical, not adapter) | M01, M02, M06, M07 | real attempts here; L3 applies |
| Review completion | M20 (`basis: declared`, `trust: proposal`) | 2 live D9 launches with spool receipts |
| Delegated review acceptance | the D10 signer as the acceptance producer for M20-cohort decisions | 2 live signed acceptances |
| Verification and integration proxies | M45, M36 | live verify accepted and rejected, a live integration |

### 8.2 Blocked by missing live evidence (the selected provider's activation waits)

| Family | Metrics | Missing |
| --- | --- | --- |
| Cost and spend | M04, M11, M12, M14, M24, M34, M37 | real rate cards and a provider charge or invoice export (**unavailable** for this account); R1/R2 |
| Services | M38, M39 | no typed throttling or error fields in 0.154.0 rollouts (run2 §6) |
| Tool latency, prompts | M18, M33 | `execution_duration_not_exposed`, `attention_reason_not_exposed` |
| Quota, secondary window | M40 secondary, `rate_limit_reached_type` | never reported live |
| Review quality from owner triage | M21, M22, M23, M25, M26, M27, M28, M29 | no live findings, triage, repairs or reopenings; producers are fixture-only (§6) |
| Seeded evaluation | M43, M44 | no live seeded trials (needs TM4.6 tooling and ≥ 20 trials) |
| Candidate groups | M41, M42 | no live groups (≥ 10 closed groups) |
| Proxies needing producers or history | M46, M47, M48, `flaky_tests` | no main-check producer; integrations still inside the horizon; no repeat runs |
| Fleet | M35 | `no_complete_window` live |
| Absent producers | M03, M05, M10, M19, M30, M49, M50 | the registry marks them `absent` / `no_producer` |
| Other Codex versions | all | 0.158.0 and later are uncertified (L1) |
| Other adapters | all | owner decision: Codex only; Claude, Devin, OTLP and others are `adapter_absent`, never 0 |

## 9. Privacy and cleanup

- **Canary.**
  - Needles: 3462, made of 40-byte slices of every string in every payload
    of the 4 rollouts, plus the brief markers.
  - Targets: 183 files, namely `telemetry.db` (with WAL/SHM) and every
    product output of both runs. The canonical `state.db` holds the owner's
    retained brief by design and is not a telemetry output.
  - Every hit was a path, attempt, session or object id fragment, possibly
    cut with JSON punctuation. There were **0 content hits**. The markers
    (`LIVE3_*_OK`, `RESUMED`, `SUBMITTED`, brief phrases,
    `review_receipt.v1`) are absent from all targets.
- **Repository.** The repository holds no prompt, response, command or
  output text: only this document and the sentinel fixture. Rollouts,
  driver and outputs stay in scratch.
- **Processes.** Every process started was stopped: the four Herdr servers,
  the tickers, the TUI workers, and `codex exec`. `ps` afterwards shows only
  the owner's pre-existing processes: the Herdr server, the ticker
  (pid 607926) and Codex processes.
- **Login copies.** Both copies were deleted.
  `find /tmp -name auth.json` finds only three 15-byte test fixtures that
  another test run created in `/tmp/.tmp*/agent-home` before this run.
  None is from this run, and none is the real login.
- **Owner state.** `~/.codex` was read only for the copy step.
  `~/.herdr-projects` was not touched.

## 10. Follow-ups

Status as of the live-run findings card (branch `fix/live-run-findings`).
Everything marked fixed is covered by end-to-end tests without model calls;
none of it changes this certificate's measurements.

- **F1 (worker brief): fixed.** The isolated agent's `PATH` is
  `<product binary directory>:/usr/bin:/bin`, so the D9 brief's and a
  worker's bare `herdr-projects` resolves to the product binary the sandbox
  exposes read-only, for `result submit` and the review channel alike. No
  deployment `shell_environment_policy` is needed. Test:
  `an_isolated_worker_cannot_read_owner_secrets_or_lift_the_hiding_but_still_commits_and_submits`
  submits by the bare name.
- **F2 (Codex sandbox): fixed; root cause in §2.2.** Isolated Codex
  attempts get `-c sandbox_workspace_write.writable_roots=[common dir,
  <common>/worktrees/<id>, spool, output]` before the profile's arguments;
  the recommended mode is `workspace-write`, and `danger-full-access` is not
  needed. Verified with the real Codex 0.154.0 `codex sandbox` (no model
  call) inside the product sandbox: EROFS with the run's configuration, a
  quarantined commit with the product's. Test:
  `an_isolated_codex_worker_commits_through_codex_workspace_write_sandbox`
  (the real-Codex part runs with `HP_CODEX_SANDBOX_BIN`). Still open: a live
  run of a Codex TUI author under `workspace-write` with the override.
- **F3 (lane A/B): fixed; flagged, not moved.** `collectors sessions` reports
  `after_termination {terminated_unix_ms, records, first_unix_ms}` per
  rollout bound to a terminated attempt: usage records whose line time is
  after the termination receipt (contracts-collection.md A9). The records
  still count in the attempt's usage and M08 (L2 stands). `usage` exposes
  the per-attempt flag; `report` identifies affected attempts as still counted
  in M08; `usage_after_termination` warns and resolves when none remain
  (health-rules.v2). E2E: `post_termination_usage_is_visible_without_changing_accounting`.
- **F4 (lane A): fixed.** A bound rollout's last turn that was open when the
  product ended the attempt (receipt cause `cancellation` or `completion`)
  is `final_event.state = ended_by_termination`, with the receipt's cause
  and time, never a `final_event_missing` gap (ingest 0009); health never
  alerts on it. Test:
  `a_turn_the_product_ended_is_ended_by_termination_not_a_missing_final_event`.
- **F5 (views): fixed.** `report --text`, the `query` text form and the
  views print M16 as `issued N, accepted K inferred (U unknown), executed E`.
  Test: `tool_volume_success_and_latency_are_honest`.
- **F6 (certification): open, with a guard.** Certifying 0.158.0 needs its
  own bounded live run. Meanwhile `doctor` warns (never fails) and
  `collectors capabilities` lists each retained Codex profile with a
  warning when its recorded agent version is uncertified, when its agent
  path is a launcher such as the mise shim, or when the path resolves to an
  install of another version, and says how to pin: prepare the profile with
  the resolved binary, for example
  `~/.local/share/mise/installs/codex/0.154.0/bin/codex`. Nothing is
  certified automatically. Test:
  `doctor_and_capabilities_warn_on_an_uncertified_or_drifting_codex_profile`.
- **Still open, not code:** L-iso-1 (a lab under `/tmp` keeps its first-level
  directory visible; use a deployment root under `$HOME`), L3 (review tasks
  in lifecycle cohorts; filter with `--task-class`), and the §8.2 blocked
  families, which need live evidence.
