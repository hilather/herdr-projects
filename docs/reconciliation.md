# Runtime observation and reconciliation (partial T03.4)

`reconcile PROJECT` collects a read-only observation batch from schema-v6-or-newer runtime
bindings. `reconcile PROJECT --record` persists that batch and audit events against
the exact event head, binding revisions and task revisions observed. Neither form
launches, prompts, copies, removes or releases capacity. Recording evidence that
invalidates an active ownership claim pauses control and requires reconciliation.
Use `migration PROJECT upgrade-store` explicitly for older published stores.

Pane queries use the recorded socket/machine, never an ambient session. Herdr
version, pane and agent responses must be valid. Duplicate or inconsistent
identities, unsupported versions, missing endpoints and unreachable sessions are
unknown. Successfully queried absence is distinct from unknown. Workspace, tab,
cwd and available agent identity must agree for a present classification. A present
or idle agent is not a writer-quiescence proof.

Local worktree observations compare recorded repository, path and branch against
bounded Git porcelain output. Truncated/contradictory output is unknown; remote
worktrees remain unknown. Local presence additionally requires matching Git common-directory and top-level
paths and a stable directory incarnation; prunable registrations remain unknown.
These checks identify a registration, not preservation
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
attempt termination evidence, repair actions, automatic runtime
execution and integrated restart testing.

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
not selected by `active_attempt`. Schema v9 also refuses changing a binding with
an ownership claim, including coordinator claims. No attempt or reservation is removed.

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

Admission requires ownership of recorded local resources, no unfinished delivery,
and fresh matching observations for every binding. Retained attempts and running
tasks block admission unless they exactly match a live adopted ownership claim.
Unowned resources and remote resources continue to block admission. Observations must be at most 30 seconds
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

## New canonical runtime records (schema v8)

`runtime PROJECT create --route route.json --expected-head H` creates a coordinator
record. Add `--task TASK --task-revision R` to create a task binding. Use `{}` for a
route with no resources; route fields follow the rebind rules above. This command
registers an unverified record only: it does not launch or adopt a pane/worktree.
Existing imported bindings must be rebound instead of duplicated.

Creation fences the head and task revision, refuses retained attempts or duplicate
pane references, increments the linked task revision, and invalidates admission.
New bindings have null import provenance. Imported bindings retain their original
source hashes; legacy files are neither invented nor dual-written. The explicit
schema-v8 upgrade preserves existing binding bytes, observations and foreign keys.

## Explicit local adoption (schema v9)

`runtime PROJECT adopt BINDING --expected-revision R --expected-head H` checks
known projects under the root execution lease, collects fresh resource evidence,
and records an adopted ownership claim. It does not prompt or launch an agent.
The scanner refuses conflicting canonical or legacy references, corrupt inventory,
older canonical stores lacking runtime bindings, and recognizable projects missing
their project marker. Resolved legacy references still count. Inventory is bounded
to 1,024 root entries/bindings and 256 entries per legacy thread directory.

Claims bind the complete runtime identity, config, local socket/worktree device,
inode and birth time, and detected agent kind/name. Unsupported incarnation evidence
refuses adoption. Remote adoption is not supported. A live task agent creates a
retained running attempt and advances its task revision; coordinator or worktree-only
adoption creates no worker attempt. Collect fresh observations after adoption before
requesting active control. Matching repeated adoption is idempotent.

Recorded evidence of replacement or lost ownership pauses active control without
releasing capacity. Pane absence or idle status is never termination evidence.
Adopted resources do not grant destructive cleanup authority. Termination evidence remains to be implemented; retained worker attempts block
relinquishment and rebinding. Conflict protection covers projects in this root only.

## Audited relinquishment

Pause the project, then use `runtime PROJECT relinquish BINDING --reason TEXT
--expected-revision R --expected-head H`, where R is the ownership claim revision
shown by inspect. Archived projects may also relinquish. The command withdraws the
claim and its observation, retains the binding/resource references, audits the entire
claim and reason, and invalidates control in one transaction. It makes no external
calls and does not delete files, close panes or release attempt capacity.

Every retained attempt for a linked task blocks relinquishment, even a lost attempt
that is no longer selected as active. Active task pointers/running tasks must be
reconciled first. Successful task relinquishment advances the task revision. A later
explicit rebind may change the unowned route. Re-adoption uses a new monotonically
increasing claim generation; prior immutable audit events remain intact.
