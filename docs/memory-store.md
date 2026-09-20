# Memory store (T05.1–T05.4)

Schema 18 adds content-addressed `objects`, immutable `memory_revisions`, heads,
validity and dependencies. Markdown (`MEMORY.md` / `memory/*.md`) remains the
memory authority until `memory cutover` sets `Format.memory=sqlite-v1`.

Object hashes are 64 lowercase hex in SQL and `sha256:<hex>` at the CLI boundary.
Bytes live at `.state/objects/sha256/<prefix>/<digest>` (0600, nofollow, temp +
rename). Only `availability=available` objects may be referenced. GC claims
unreferenced, unpinned objects as `gc_pending` with a fencing token; a new
reference cancels that claim in the same transaction. The collector rechecks,
moves to `gc_deleting`, unlinks, and records `purged`.

`is_hard` is not inferred from Markdown. `memory@` `hard_rule` / `import_ack` /
`revoke_head` apply through `install_memory_policy` in the same transaction.
`hard_rule` is sufficient without a prior `import_ack`. Cutover is not applied
as a row mutation; `memory cutover` switches `format.memory`. Active facts
require an active head, `validity.state=valid`, available body bytes, and an
unexpired timestamp.

`MemoryError::Storage` wraps `StoreError`. Head CAS misses become
`RevisionConflict` at the service edge. Missing objects become
`EvidenceUnavailable`, not active facts.

`herdr-projects memory PROJECT inspect` reports `authority` from `format.json`
(`legacy-markdown` or `sqlite-v1`).

## Snapshots (T05.2)

`herdr-projects memory snapshot PROJECT --task ID --profile NAME --input-file scope.json`
builds a deterministic snapshot inside a write transaction. Selection policy
`memory-selection` version 1 uses integer weights (domain 100, path 40, kind
contract/observation/assumption 10/6/2, lexical cap 10, dependency `15/(1+hops)`).
Changing weights requires bumping `SELECTION_POLICY_VERSION`.

Mandatory content is constraints, `is_hard` active facts, and pinned keys. Optional
active facts are packed whole into the remaining `ProfileBudgetEnvelope.soft_input_chars`.
Overflow of mandatory content returns `RequiredContentTooLarge` instead of truncating.
`--task coordinator` is rejected; the coordinator constructor is internal (T05.4).
`reserve_prepared` still refuses `inputs.memory`. Estimator is labeled
`char-count-v1` (estimated when tokens×4).

## Import and cutover (T05.3)

Memory uses a separate journal at `.state/migration/memory-journal.json`. It does
not reuse the W03 `.state/migration/journal.json`. Phases are Prepared → Imported
→ Verified → CutoverPending → Active. `Format.migration` stays the W03 plan
digest. `open_active` and `publish_control_marker` accept `legacy-markdown` or
`sqlite-v1` and preserve the memory owner.

Inventory covers `MEMORY.md` and `memory/*.md`: regular files only, nofollow,
depth 1, 64 KiB/file, 1 MiB total, UTF-8, no NULs. Symlinks, traversal, FIFOs,
hidden files and oversized payloads are refused. Import writes `kind=observation`,
`is_hard=0`, validity `stale`/`unverified_import`, and pins body and provenance
objects. The same memory-plan digest plus source digest reuses identities.

`herdr-projects memory PROJECT plan --output FILE` writes the plan.
`memory PROJECT import --file PATH` imports a candidate.
`memory PROJECT preview --file PATH` reports digest conflict without printing bodies.
`memory PROJECT cutover --plan FILE --writers-stopped` plus a signed `memory@`
cutover document (`memory_plan_digest`, `expected_memory_owner=legacy-markdown`)
sets `format.memory=sqlite-v1` without changing `runtime` or `Format.migration`.
Imported Markdown cannot become approvals, argv, or shell commands.

After cutover, `MEMORY.md` / `memory/*.md` are projections with revision headers.
The renderer never overwrites a divergent file. `brief_for` uses a snapshot when
one exists for the thread profile; otherwise it inlines generated projections and
does not fail solely because no snapshot row exists. Rendering a snapshot whose
mandatory content exceeds the brief cap fails closed (`RequiredContentTooLarge`).

## Coordinator checkpoints (T05.4)

Schema 20 adds `coordinator_sessions` and `coordinator_checkpoints`. A checkpoint
row foreign-keys a coordinator snapshot (`task_id='coordinator'`, no `tasks` row,
`subscriber='coordinator:'||session_id`). New, restarted, or unacked sessions
render a **full** checkpoint. After `context PROJECT --ack CHECKPOINT`, later
`context` may render a **delta** from `cursor_seq` exclusive through the current
head. Deltas still include current mandatory constraints and unresolved blockers.
A failed snapshot or context read does not advance `cursor_seq` or mark inbox
seen. Stale or missing checkpoint ids fall back to full.

`context PROJECT` requires `profiles.planner` or `--profile NAME`. It does not
kind-default. `herdr-projects doctor` prints the last checkpoint `full_chars` /
`delta_chars` / `created_unix_ms` when the store is at schema 20. This is local
Linux card acceptance, not the W05 wave gate. Worker `brief_for` is unchanged.

## Worker proposals (T06.1)

Schema 21 stores untrusted `memory_proposals` and immutable `proposal_validations`.
`herdr-projects memory propose PROJECT --input proposal.json` validates identity,
schema, producer task/attempt, input snapshot, object hashes, expected bases and
scope. It never promotes heads or marks hard rules. Same proposal ID and digest
returns the prior receipt; the same ID with different bytes is
`IdempotencyKeyConflict`. Stale bases, missing evidence, malformed JSON, oversized
payloads and permission elevation (`constraint`/`hard_memory`, unknown fields such
as `is_hard`, informational impact on a hard record) are rejected with repairable
diagnostics. Semantic review and promotion remain T06.2. Remember-section findings
enter this route rather than canonical Markdown or store writes. This is local
Linux card acceptance, not the W06 wave gate. Production launches stay disabled.
