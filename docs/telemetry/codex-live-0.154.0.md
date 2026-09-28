# Codex 0.154.0 live certification (telemetry S5)

Date: 28 September 2026 (UTC). Branch `telemetry/codex-live-cert` from `main`
`5797b12`. Linux 7.2.3-arch1-3; Codex CLI 0.154.0 (resolved binary
`~/.local/share/mise/installs/codex/0.154.0/bin/codex`); stock Herdr 0.9.1
(isolated disposable servers). Codex only; a private login copy in the
disposable execution home, never read, removed once the workers stopped.

Command: `HP_LIVE_F1_KEEP=1 scripts/test-live-f1 --herdr ~/.local/bin/herdr
--agent <resolved codex> --auth-file ~/.codex/auth.json
--allow-authenticated-workflow` (workers under `workspace-write`, controller
capture). `HP_LIVE_F1_KEEP` keeps the disposable project and execution home
for these checks; both were deleted afterwards. Then
`herdr-projects --root <disposable>/root telemetry demo collect|usage|attempts|report`.

**Decision: `0.154.0` certified.** Home location, cwd equality, both subset
rules and ordinal continuity held for every rollout. Live attempts: 3 of 3
(details in [the F1.7 gate note](../reviews/2026-09-27-f1-live-gate.md)).
Evidence run: attempt 2 (workers A and B both ran); attempt 3 repeats A.

## Live items (baseline §2)

| Item | Result |
| --- | --- |
| Rollouts under `<execution_home>/.codex/sessions/YYYY/MM/DD/` | observed: 2/2 (attempt 2), 1/1 (attempt 3); no rollout elsewhere |
| `session_meta.cwd` = attempt worktree `…/.state/worktrees/<attempt>/repo-00` | observed, all 3; all bound, none unbound or ambiguous |
| `cached_input ≤ input`, `reasoning_output ≤ output`, `total = input + output` | observed on all 11 records |
| Σ `token_usage_record.usage` = final `thread_token_usage` (all six fields) | observed, all 3; no `thread_total` discrepancy |
| Ordinals do not restart within a file | observed: each `thread_token_usage` equals the running sum at that record; no resume/compaction occurred |
| `token_count.info.total_token_usage` = Σ | observed; no `token_count_total` discrepancy |
| Guardian / auto-review rollouts | not observed (no `codex-auto-review` model; one rollout per worker) |
| Rate limits | observed: `limit_id codex`, `primary{used_percent 42.0–43.0, window_minutes 10080, resets_at}` (Unix seconds), `plan_type`; `secondary` null; semantics not certified |
| Per-turn model | observed one turn per session, `gpt-6-astra` / `low`; a model change within a session not observed |
| Incremental flush | observed: records appeared one by one while the worker ran (A: 0→1→2→3→4 over ~20 s) |
| Partial last line after kill | not observed: every rollout ended with `\n` after the workers were stopped |

## Token counts (attempt 2)

| Session (attempt) | Records | input | cached | output | reasoning | total |
| --- | --- | --- | --- | --- | --- | --- |
| `…b9074` (A) | 4 | 60628 | 56576 | 402 | 14 | 61030 |
| `…a3832` (B) | 3 | 45410 | 41472 | 407 | 0 | 45817 |

Attempt 3, A (`…ea067`): 4 records, 60631 / 56576 / 404 / 18 / 61035.

**Independent sum.** A standalone Python script (no production code) summed
the raw `token_usage_record.usage` counters: identical to `telemetry usage`
and `collect` for all three sessions, field by field; `report` M08 = 106038
(= 60628 + 45410), M09 = 809, M13 1/1, M15 7/7, no quarantine.

**Canary.** Needles: the worker prompt markers (`F17_LIVE_A_OK`, task
wording, `reply DONE`) plus 40-byte slices of every string in `response_item`,
`world_state`, `item_completed`, `task_complete` and `base_instructions`
(76–102 needles per run). Targets: `telemetry.db` (+ `-wal`/`-shm` when
present), the pre-certification sidecar, and every CLI output. Content hits:
**0**. Only identifiers (session UUIDs, attempt IDs) and the `~`-redacted
worktree path matched.

## Findings

- Certifying exposed a zero leak: rows collected while `0.154.0` was
  uncertified keep `NULL` counters, and after certification `usage` summed
  them to `0` (records 0). Fixed as glue: a session holding any
  `cli_version_uncertified` row stays `unavailable: cli_version_uncertified`
  in `usage` and in the S6 report (test
  `records_collected_before_certification_stay_unavailable`). Such rows are
  not re-read; a fresh sidecar (as done here) recovers the counters.
  Upgrading them in place is a follow-up. (Later done: collect re-reads such
  rollouts, `records_collected_before_certification_are_reread`.)
- `telemetry attempts` still prints `usage: collection_not_run` for Codex
  attempts even with a sidecar: the outcome `usage` wiring (S5 card) is not
  done. Follow-up. (Later done: `attempts_show_bound_usage_or_its_reason`.)
- A worker that never received its brief writes no rollout: attempt 3's B is
  `not_bound`, as expected.
