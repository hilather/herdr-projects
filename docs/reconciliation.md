# Runtime observation and reconciliation (partial T03.4)

`reconcile PROJECT` collects a read-only observation batch from schema-v6-or-newer runtime
bindings. `reconcile PROJECT --record` persists that batch and audit events against
the exact event head, binding revisions and task revisions observed. Neither form
launches, prompts, copies, removes, releases capacity or changes lifecycle state.
Use `migration PROJECT upgrade-store` explicitly for older published stores.

Pane queries use the recorded socket/machine, never an ambient session. Herdr
version, pane and agent responses must be valid. Duplicate or inconsistent
identities, unsupported versions, missing endpoints and unreachable sessions are
unknown. Successfully queried absence is distinct from unknown. Workspace, tab,
cwd and available agent identity must agree for a present classification. A present
or idle agent is not a writer-quiescence proof.

Local worktree observations compare recorded repository, path and branch against
bounded Git porcelain output. Truncated/contradictory output is unknown; remote
worktrees remain unknown. These checks identify a registration, not preservation
or a tested commit. At most 128 bindings, 16 session endpoints and 16 repositories
are queried per batch. Excess binding counts refuse an incomplete collection;
excess endpoint/repository queries produce explicit unknown observations.

Config fingerprints and the project head are rechecked after collection. Persistence
requires complete binding coverage, rejects stale revisions or older timestamps,
and is atomic. Repeating an identical batch at its current head adds no events.
Each observation retains its collection time and config fingerprint. Historical
observations are not automatically current authorization after state/config changes.
Raw external command output is not stored in the evidence table.

Schema v6 adds hash-checked observation rows and includes them in schema-qualified
runtime exports. Schema v7 adds the guarded lifecycle control described below. This increment interleaves the observation dependency of the
remaining W03 adapters; it does not close T03.2, T03.3 or T03.4. Remaining work includes
resource ownership acquisition, attempt termination evidence,
repair actions, safe adoption/reuse and integrated restart testing.

## Explicit session rebinding

`runtime PROJECT inspect` shows current bindings and observations. To replace an
imported session route, write a JSON object containing `machine`, `socket`,
`workspace_id`, `tab_id`, `pane_id` and `cwd`, then run:

```sh
herdr-projects runtime demo rebind thread:t-0001 --route route.json \
  --expected-revision 1 --expected-head 42
```

Omitted route fields are empty, so supply the complete intended replacement.
A nonempty pane requires an absolute socket/cwd plus workspace and tab identities.
The update requires the current head and binding revision, rejects duplicate pane
references within the project, clears prior observations, and increments a linked
task's revision so old claims/results cannot commit against the changed routing.
It refuses every retained attempt for the task, including lost attempts that are
not selected by `active_attempt`. No attempt or reservation is removed.

Rebinding retains immutable import provenance and legacy files. Its new routing
is unverified, the old execution fingerprint is cleared, and the event records
the replacement. Original source/session hashes describe import history, not proof
of ownership of the replacement. Cross-project ownership, adoption and dispatch
remain separate requirements. A coordinator rebind changes its binding revision;
future coordinator adapters must fence that revision explicitly.

## Canonical lifecycle control (schema v7)

`runtime PROJECT admission` reports blockers. `runtime PROJECT state
paused|active|archived --expected-head H --expected-revision R` changes canonical
control without dual-writing legacy status. Inspect provides the control revision
and epoch. Restore an archived project to paused before requesting active.

Admission currently requires no existing pane, worktree or remote resource needing
adoption, no retained attempts or running tasks, no unfinished delivery, and fresh
matching observations for every binding. Observations must be at most 30 seconds
old and match the current config fingerprint. Active control pins that fingerprint;
future effect adapters must additionally check typed safety, scoped authority,
binding/task revisions and resource ownership. This is not an automatic scheduler.

Pause/archive increment the fence epoch and require reconciliation; they remain
available if external configuration is malformed. Rebinding also invalidates
admission. The format marker reflects committed control. A crash between the DB
commit and marker publication blocks ordinary opens; `migration PROJECT recover`
republishes the marker from canonical state without discarding committed edits.

`operations PROJECT retire ID --reason TEXT --expected-revision R
--expected-head H` stops retries of pending or ambiguous intents with an audited
permanent outcome. An owned claim must expire first. Retirement does not prove
an earlier effect absent and does not release attempt capacity or resources.
