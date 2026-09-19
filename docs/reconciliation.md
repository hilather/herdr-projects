# Runtime observation and reconciliation (partial T03.4)

`reconcile PROJECT` collects a read-only observation batch from schema-v6 runtime
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
runtime exports. The migration marker stays reconciliation-required and execution
remains frozen. This increment interleaves the observation dependency of the
remaining W03 adapters; it does not close T03.2, T03.3 or T03.4. Remaining work includes
resource ownership acquisition, session rebind, attempt termination evidence,
repair actions, safe adoption/reuse and integrated restart testing.
