# Replay suite contracts (TM4.6)

Plan card TM4.6: turn accepted historical tasks into replay cases and measure
a new configuration on a reproducible subset through the ordinary launch,
verification and budget paths. Code: `src/replay.rs` (CLI `replay <slug> ...`),
`src/store/replay_cases.rs`, migration `0064_replay_suite.sql` (schema 64).
End-to-end evidence: `tests/replay_suite.rs`.

## 1. Registry (append-only, `replay_owner.v1`)

Only `operator:cli` (the project owner) writes; a worker principal or an
attempt id is refused, and every replay CLI command is refused inside a
worker execution context (the review CLI's markers).

| Table | Row |
|---|---|
| `replay_log` | `suite`, `retired` or `run` history row (principal, authority, time) |
| `replay_suites` | immutable suite version, extractor, manifest digest (sha256 of the canonical case list and exclusions), extraction exclusion counts |
| `replay_cases` | recorded with its suite in one transaction: case id `<task>.r<revision>`, source task, accepted submission and verified result, contract revision and digest, source repository, base oid (the integration tip the accepted change was based on), reference oid (the accepted candidate), integrated oid, hidden-check reference and list (policy id, target path, sha256, size), solution paths, classification, stratum, contamination scan, `eligible` or `contaminated` |
| `replay_retirements` | one per case whose checks no longer apply; the case stays readable and leaves every later draw and report |
| `replay_runs` | configuration label, subset, seed and the drawn case ids |
| `replay_candidates` | the fresh task a run created for one case, and its replay repository |

Triggers refuse updates and deletes on all of them, a case outside its
suite's transaction, a candidate that is not an eligible unretired case of
its run, and a candidate on a task that already has a contract, attempt,
submission or dependent.

## 2. Extraction (`replay extract --suite V`, extractor `accepted-history.expected-output.v1`)

Sources: every task (never a replay candidate) with an integrated
submission; the latest integration per task, in task-id order. For each,
`git diff base..reference` in the source repository:

- Hidden checks: added or modified `tests/expected/<path>` files of the
  accepted change. Each asserts that the candidate's `<path>` equals the
  file's bytes (at most 8 per case, 64 KiB each). The bytes are written to
  `<projects root>/.replay/<slug>/checks/<sha256>` (directory 0700, file
  0600); the store holds only paths and digests. A source without such a
  file is excluded as `no_hidden_check`; without a non-test change as
  `no_solution_paths`.
- Classification: the TM0.6 row recorded for the source contract revision,
  else `classify_task` over the source contract's scope (`derived_from_contract`).
  The class is the stratum.
- Contamination (`distinctive-lines.v1`, metadata only): the accepted
  change's added lines (trimmed, 16+ characters, not already in the base
  versions of the changed files) are looked for in PROJECT.md, every retained
  memory object under `.state/objects`, every worker brief payload and the
  source contract's deliverable and non-goals; a retained worktree or Git
  quarantine of a source attempt is a hit too. The record keeps counts per
  source and ids, never lines. A case with any hit is recorded
  `contaminated` and never drawn; the suite's exclusions count it.

Extraction is deterministic: the same history yields byte-identical case
records under any suite version. A suite version is immutable.

## 3. Runs (`replay run --suite V --configuration C --subset stratified:N --seed S --expected-head H`)

The draw orders each stratum by `sha256(seed, suite, case)` and takes one
case per stratum in turn (strata in name order) until N; the same inputs give
the same subset (`replay subset` previews it). Before writing anything the
run refuses when a drawn case's source repository is unavailable, or a
hidden check file is missing or no longer has its digest (retire the case).
No owner-wide `[worker_isolation] hide` is needed: each candidate's own
sandbox hides the source repository at launch (§4), so ordinary launches on
that repository are unaffected.

Per drawn case it creates a replay repository
`<projects root>/.replay/<slug>/repos/<suite>/<run seq>/<case>` that holds
only the base commit's history (`pack-objects --revs <base>` into a fresh
repository; no later commit, so neither the reference solution nor its
tests, is reachable), then an ordinary task (`task add`), its replay
registration, and `task queue` with no dependency. Nothing else: no
contract, approval, reservation or operation.

`replay contract TASK --output F` drafts the task's unsigned contract at the
current head: the source contract's deliverable, scope, outputs (tests
removed), profile kind and result schema, over the replay repository at the
base, route `verify_only`, one acceptance policy per hidden check:

```json
{"version":1,"checks":["/usr/bin/git","diff","--no-index","--quiet","--","<check store>/<sha256>","<target>"],
 "hidden":[{"path":"<check store>/<sha256>","sha256":"<sha256>"}]}
```

The owner signs and installs it with `task contract put`, binds and launches
the task with `launch draft`, an owner-signed approval and `launch reserve`
(selection reason `replay`), exactly as for any task: same admission, same
profile budget, same dispatch decision.

## 4. Hidden checks in the verifier

A policy's optional `hidden` list names owner-held inputs. The verifier
refuses the run (`hidden_check_unavailable`, before any checkout) unless each
path is canonical, lies in the project's own check store, is a regular
non-symlink file of at most 64 KiB and still has its digest. The isolated
child binds each read-only at the same path in its private root, rechecks
the digests after `pivot_root` (else `policy_digest_mismatch`), and runs the
check there. The policy digest pins the hidden digests, so a verified result
is bound to the exact check content. The integration-time policy recheck
binds no hidden input (a replay candidate never integrates).

What the candidate can read: the contract and the store (paths and digests
only), its own project, its worktree and the replay repository. The check
store and the source repository are outside its sandbox view: the projects
root is covered except its own project, and the candidate's launch hides
its case's source repository and the check store
`<projects root>/.replay/<slug>/checks` (`canonical_worker::resources::launch_hides`).
These per-launch hides are derived from the append-only replay registry
(`replay_candidates` → `replay_cases.repository`) at resource creation and
again at gate release, so both derive the same supervisor argv, which the
launch's `command_digest` fences; they enter no approval, `LaunchInputs` or
id. Any other task's launch hides nothing extra, so an ordinary task on the
source repository still launches and reads it. `tests/replay_suite.rs`
probes both from inside launched workers
(`launched_replay_candidate_cannot_read_hidden_checks_and_is_verified_by_them`,
`ordinary_task_on_the_source_repository_still_launches_without_replay_hides`). Residual: a source attempt's retained worktree or quarantine inside
the project would be readable, so the scan flags it as contamination.

## 5. Guard: a replay candidate is an evaluation artefact

Like a seeded candidate, a replay candidate never integrates and never
releases a dependent, whatever its contract route:

- `begin_integration` refuses its verified result before any write;
- the integration producer's eligibility excludes it (`NOT_REPLAY`);
- `task queue` refuses it as a predecessor;
- triggers refuse `integration.run` / `integration.lease` operations,
  `integration_operations` rows, `task_dependencies` edges and
  `dependency_satisfactions` rows that name it.

Completion is not refused: the task may end normally.

## 6. Report and M49

`replay report --suite V [--since MS]`: each candidate is `passed` (one
submission of its current contract revision has an accepted verification for
every hidden policy), `failed` (its task is terminal or every attempt ended,
without a pass), `pending`, `not_launched` (no dispatch decision) or
`retired`. Candidates group by the configuration of their first dispatched
attempt (the content-addressed `AgentConfiguration` from the dispatch log),
with the run labels, suite version, numerator/denominator (passed /
passed + failed), n, pending and a per-stratum split. Exclusions: the suite's
extraction counts, contaminated and retired cases, and candidates not
launched, retired or outside the window. Uncertainty: raw rates with n
(`raw_with_n`); no shared bootstrap estimator exists on this branch.

M49 (`M49.v1`, central provider, family `replay`, active at TM4.6,
certification `fixture`): the latest suite version's report, with the pooled
numerator/denominator on top and the per-configuration cells beside it.
Replay candidates are measured by M49 only: the lifecycle metrics
(M01/M02/M06/M07, every cohort of the query service, and the central
report's M02/M07 and `tasks` summary) exclude them with an explicit
`replay_candidate` exclusion count (contracts-analytics.md §3); assignment
policy outcomes skip them too.

## 8. Scheduled replay runs (TM4.8)

The owner-signed replay template in [workspace.md](workspace.md#10-owner-signed-telemetry-routines)
uses this same run path under the routine execution lock, with the current
head as its expected head. It requires a named profile in the signed owner
configuration, an existing suite, and 1–16 stratified cases. It grants no
launch authority; every resulting task still needs the contracts, owner
approvals and ordinary profile budget checks described in §3.
