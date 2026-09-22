# Independent review of the memory implementation

Reviewed commit: `e5e2818`, against `9aa4990` (2026-09-20).

This is historical evidence for that commit. Subsequent fixes and current test
locations are tracked in [repair progress](../memory-repair-progress.md).
The reproduction commands below target the reviewed commit, not the revised APIs.

**Do not accept the W05/T06.1/T06.2 completion claims yet.** The implementation has
useful foundations, but core guarantees fail on ordinary supported inputs. Seventeen
independent acceptance probes reproduced failures. This is a review, not a repair;
production code and the task ledger have not been changed.

## Scope and evidence

The review covers the two new commits `e5b78a9` and `e5e2818`: 64 files,
4,584 added and 201 removed lines. It traces their CLI, domain, service, store,
migration, object-file, worker-brief, coordinator, authority and executor changes,
including affected existing callers and tests.

Requirements used: the original package at
`/home/brewerm/Downloads/herdr-projects-architecture-and-implementation-plan/herdr-projects-design-plan/`,
especially `09-task-cards.md`, `04-memory-fan-out-and-fan-in.md`,
`05-memory-schema-and-protocol.md`, `08-implementation-waves.md`,
`10-migration-and-rollout.md`, and `12-requirements-traceability.md`;
plus `docs/task-status.md`, `docs/memory-store.md`, the ADRs, and Grok's detailed
draft revision 4 at `/tmp/grok-brewerm/grok-design-doc-d6ba6f99.md`.
The original package was located in Downloads after the user supplied its location.
All original card definitions were read; implementation verification concentrates
on the new changes and their dependencies, not recertifying every pre-existing card.

The existing 251 library tests passed in debug with all features. The independent
probes are in `2026-09-20-memory-audit-probes.rs`; **all 17 fail at their intended
acceptance assertions**, rather than fixture setup. They use disposable roots,
real SQLite and, for cutover, a real disposable Ed25519 signature. Results are in
`2026-09-20-memory-audit-probes.log`. Broad binary/CLI validation is recorded below.

## Findings

### 1. P1 — Promotion does not fence the review's authority and validity state

`src/memory/review.rs:41,67,80` records a reviewed event head but never enforces
it. `src/store/reviews.rs:43` does not recheck the stored review, relevant policy,
validity, dependency heads or reviewed event head inside the promotion transaction.
Revision CAS protects changed record bodies, but not a change to `is_hard` on the
same revision. Reproduction: submit and approve an informational change; mark its
record hard; promote the old decision. It succeeds and replaces the hard rule.

Required: bind review to all relevant record/policy/validity/dependency inputs and
revalidate those bindings atomically at promotion. Retain unrelated-head tolerance
only if the exact dependency set is checked. Test policy changes between validation,
review, preparation and transaction entry, as well as competing record writes.

### 2. P1 — Snapshot budgets omit the memory bodies

`src/store/memory.rs:68,187` counts metadata containing a body hash, not the body
that an agent receives. Optional packing uses the same incomplete size. The
existing overflow test uses a cap so small that metadata alone exceeds it.
An 8,000-character mandatory rule passes a 1,000-character envelope. Coordinator
rendering then returns 9,007 characters without error (`src/memory/checkpoint.rs:89`).

The worker path also loses mandatory/optional roles when returning plain files to
`compose_brief` (`src/thread.rs:290`). That function always packs MEMORY.md first.
An optional index can therefore consume the cap and cause a mandatory rule file to
be omitted even though the snapshot renderer's mandatory-only size check passed.

Required: one bounded, verified serialization and accounting contract for snapshot
selection and final rendering, including instructions, memory contents, headings,
task text and mandatory blockers. Pack optional records whole; refuse mandatory
overflow before publishing a snapshot/checkpoint. Test realistic caps and bodies.

### 3. P1 — Cached snapshots can return expired facts and demote newly hard rules

`src/store/memory.rs:36,234` fingerprints head revision/status, but omits validity,
expiry evaluation and `is_hard`. Selection is recomputed, then discarded when the
old cache ID exists. A snapshot created before expiry is returned after expiry;
a newly hard record remains labeled optional. `ImportAck` and validity changes
have the same cache-identity weakness. Budget changes are also absent from the
cache key unless callers happen to change the profile digest.

Required: cache only across identical effective selection state, including current
validity and authority. Include the resulting manifest and effective envelope or
validate them before reuse. Expiry must be evaluated on every new selection.

### 4. P1 — Worker snapshots are selected by profile, not task/attempt

`src/store/memory.rs:329` selects the newest non-coordinator snapshot for a profile
across the entire project. `src/memory/import.rs:422` and `src/thread.rs:378` pass
only `thread.agent_name`; no task, attempt, scope or exact snapshot ID reaches
the lookup. Two tasks using the same profile can receive each other's memory.
Profile definition/config changes are not checked by that lookup either.

Required: render the immutable snapshot explicitly bound to the task/attempt and
profile identity. Apply the documented no-snapshot fallback only to that same
task, never by borrowing a neighboring task's snapshot.

### 5. P1 — Missing/corrupt object bytes are accepted or silently truncated

`src/memory/checkpoint.rs:31` uses `fs::read(...).unwrap_or_default()` for mandatory
rules: missing bytes silently become empty text. Neither this reader nor
`src/memory/import.rs:175` verifies SHA-256 against the requested object. Both
missing-rule and corrupted-body probes return successful context. The latter
reader also reads only 65,537 bytes, while ingestion accepts 16 MiB, and returns
success without testing for overflow. A 100,000-byte rule renders as 65,537 bytes.

`src/memory/mod.rs:72` trusts an existing destination without checking its bytes.
`active_facts` checks database availability, not physical content integrity.

Required: a shared bounded regular-file reader with no-follow, byte count, UTF-8
where applicable, and digest verification. Missing, truncated or corrupt mandatory
evidence must fail explicitly; never substitute empty text or accept a prefix.
Object publication must also verify existing bytes and fsync destination directories.

### 6. P1 — Cutover publishes authority before verifying imported objects

`src/memory/import.rs:374` advances Imported → Verified without verifying the
imported store's inventory, counts or object bytes. It installs policy, switches
`format.memory` and marks the journal Active before rendering projections.
The signed-cutover probe deletes an imported body: cutover returns an error but
has already switched authority to `sqlite-v1`. An interrupted publication has
similar risks. There is no memory recovery/rollback entry point; retry after the
marker switch is rejected because the owner is no longer legacy. A crash after
policy commit also invalidates the old signed expected head/revision.

Additionally, `plan` hashes the source inventory, but `import_plan_held` compares
only the supplied digest to a newly computed digest, without hashing the supplied
inventory itself. Clearing `plan.sources` while retaining the original digest is
accepted and records an Imported journal with no imported records; reproduced.
Other plan fields also need canonical binding rather than trusting an echoed digest.

Required: verify all imported records/provenance/objects before publication, then
make each durable phase resumable using the recorded authorization and receipt.
Recompute and validate the complete supplied plan identity before any import writes.
Implement explicit forward recovery and the designed pre-write rollback boundary.
Exercise process death at policy commit, marker rename, journal update and every
projection publication, including partial multi-file publication.

### 7. P1 — The fallback reintroduces editable/stale Markdown as authority

`src/memory/import.rs:432` reads projection files without validating their header,
fingerprint or relationship to SQLite. `src/runtime.rs:53` still inlines MEMORY.md
as legacy text after cutover. Projection rendering happens only during cutover;
promotion/import/policy changes do not refresh it. Consequently fallback briefs
can miss newly promoted knowledge or use manually edited/previously revoked text.
The renderer only permits replacement of the original backup bytes, so it cannot
generally refresh a valid previous generated revision either.

Required: generate fallback contents from an authoritative read, or validate and
refresh a revision-bound projection before reading it. Preserve divergent files
for review while refusing to consume them as canonical knowledge. Include tests
for promotion, revocation, deliberate edit, missing projection and refresh.

### 8. P1 — Garbage collection ignores accepted proposals and their evidence

`src/store/objects.rs:37,42` recognizes only immutable memory-revision references.
Proposal insertion (`src/store/proposals.rs:12`) establishes no pins or object
references. The probe submits an accepted proposal and runs GC: its body is
deleted. Evidence referenced only in proposal JSON has the same problem, including
after promotion, whose provenance stores proposal/decision IDs rather than pins
to all evidence objects.

Required: transactional object-reference ownership for proposals, validations,
reviews and retained evidence, with explicit retention/release policy. GC must
honor all such references through acceptance, rejection, review and promotion.

### 9. P1 — GC and object ingestion do not recover safely across interruption

`src/store/objects.rs:27` restores availability only for `gc_pending`, not purged
rows. Reingesting collected bytes returns success but leaves the object purged.
`claim_gc` never picks up stranded `gc_pending`/`gc_deleting` rows; reopening after
begin-delete leaves the object permanently stuck. Both cases are reproduced.
`src/memory/mod.rs:112` ignores lock acquisition and unlink errors before recording
purge; ingestion does not share that I/O lock. Its temporary filename uses only
PID, so independent ingests in one process can collide.

Required: unique exclusively created staging files, shared enforced object I/O
locking, checked unlink results, durable directory updates, and restart
reconciliation for every collection state. Test actual concurrent ingest/reference/
delete interleavings, not just sequential calls on two connections.

### 10. P1 — Coordinator deltas omit changed standing project instructions

`src/memory/checkpoint.rs:89` includes the base PROJECT.md text only in full
context. Deltas include stored hard facts and blockers, but discard the freshly
read base text. Editing PROJECT.md after acknowledging a checkpoint produces a
delta without the new instructions; the probe reproduces this. The design requires
current mandatory constraints in every delta.

Required: bind instruction revision/content to checkpoints and always emit current
mandatory instructions, or require a full checkpoint when they change. Read the
checkpoint's facts, runtime state and event boundary consistently so its cursor
cannot acknowledge changes absent from the text.

### 11. P2 — Coordinator identity and acknowledgment are insufficiently fenced

`src/store/memory.rs:234` omits session identity from coordinator snapshot IDs.
The cache hit returns before creating a subscription. Creating the same snapshot
for session B returns session A's subscriber; reproduced. Also,
`src/store/checkpoints.rs:60` unconditionally resets the cursor and latest ID when
an older checkpoint is acknowledged. Session identity is only a Herdr session
string (`src/cli.rs:874`), without agent/restart generation, so replacement within
the same session can incorrectly qualify for delta context.

Required: session/generation-bound subscriptions and checkpoint acknowledgments;
monotonic cursor CAS; full reconstruction after restart, replacement or uncertainty.
If snapshots are shared, create a separate subscription on every cache hit and
make the read API explicit about which subscriber is being requested.

### 12. P2 — Retrieval scope does not reliably filter unrelated knowledge

`src/store/memory.rs:204` tests total score > 0 when both domains and paths are
provided. Every observation/contract has a positive kind weight, so unrelated
records qualify. The UI-domain/UI-path probe includes an unrelated infra record.
Path-only requests admit everything. `sensitivity` is never evaluated; task-local
records have no producer-task isolation in selection. Dependency traversal uses
all historical revisions and undirected edges rather than current justifications.

Required: separate eligibility from ranking. Define global, domain, path,
task-local and sensitivity eligibility; rank only eligible records and explicitly
admitted dependencies. Use current revision-bound dependencies and test combined
scopes, historical edges, cycles and deterministic ties.

### 13. P1 — Proposal ingress is not yet a complete validated worker workflow

`src/memory/proposals.rs:71` checks that attempt and snapshot belong to the same
task, but never checks that this attempt actually consumed this snapshot. Observed
revisions need only exist; they need not occur in the supplied snapshot. Repository
commit/tree values are length-checked, not bound to evidence. `validation_id` is
never resolved, and evidence objects are checked only for database availability.
No supported CLI stages a worker's new body/evidence objects; tests inject them
through `MemoryStore::ingest_object`. The only production ingestion path imports
project Markdown directly into revision heads, which is not untrusted proposal
staging. The coordinator skill describes a flow the CLI cannot complete cleanly.

Required: an end-to-end bounded proposal/object ingress with attempt-input binding,
typed evidence resolution and scope checks. Keep missing production launch bindings
visibly unsupported rather than certifying caller-supplied identities. Test the
actual CLI from worker output through candidate validation without direct DB writes.

### 14. P2 — Review/promotion drops semantic changes and has unstable receipts

`src/memory/review.rs:35,88` equates compatibility with matching body hashes and
skips a change entirely if its body is unchanged, even when applicability or
dependencies changed. `narrow` is accepted but has no narrowed-change payload and
cannot promote. `record_id_for` replaces '/' with '.', causing collisions between
distinct accepted keys such as `a/b` and `a.b`, and accepts proposal keys that may
later exceed identifier limits.

`src/store/reviews.rs:76` stores the pre-`memory.promoted` sequence but returns the
post-event head. Replay returns a different sequence (6 first, 5 replay in the
probe). Invalidations target only the proposing task and put `record_key` into a
field named `record_id`; no durable delivery intent is created. T06.3 remains open,
but the T06.2 receipt/intent boundary must already be sound.

Required: review the complete semantic change, support explicit narrowing or reject
it as unsupported, use collision-free IDs, store exactly the returned receipt,
and define an atomic change/intent schema that downstream delivery can consume.

### 15. P2 — New mutation paths bypass the project's operation exclusion

The Snapshot/Propose/Review/Promote branches in `src/cli.rs:596` call `open_active`
directly without `runtime_mutation` or retained project guards. Existing runtime
mutators acquire those guards to exclude maintenance and same-project effects.
SQLite serializes a single transaction, but not filesystem object ingestion or
multi-transaction review/preparation/cutover sequences. Snapshot creation also
accepts caller profile/config digests without comparing the supplied configuration
to project control. The review CLI checks `--proposal` only after persisting the
decision, so mismatched input can return failure after mutating another proposal.

Required: acquire the established ownership guard for the complete operation,
check command identity before effects, and validate current profile/config authority
under that guard and inside the transaction where appropriate.

### 16. P2 — Existing context CLI acceptance is broken by the new mandatory profile

`src/cli.rs:861` resolves a planner profile before validating or reading the
existing migrated context. Existing projects without named profiles can no longer
read task/inbox context, and an invalid/interrupted format now receives a profile
configuration error before the established ownership diagnostic. Three existing
CLI tests fail on this path: `legacy_commands_refuse_store_ownership_even_without_feature`,
`migrated_task_commands_use_revisions_and_do_not_touch_legacy_task_file`, and
`migrated_inbox_cli_drains_once_marks_seen_and_keeps_legacy_files_untouched`.

The available design explicitly requires a named profile for checkpoints, so this
is an unresolved compatibility/acceptance change, not evidence that the profile
requirement should simply be removed. Validate project authority first; define
the upgrade/configuration path for existing runtime-only projects; update fixtures
for the intended new contract and test actionable missing-profile errors explicitly.

### 17. P1 — Reviewer authority is absent from the supported promotion route

The original T04.5 requires separate worker proposal, scoped reviewer promotion,
and user policy rights. Protocol sections 2, 5 and 7 require a reviewer authority
reference and a trusted control context. `src/cli.rs:610` calls `memory.review`
and `memory.promote` directly; neither resolves or checks actor/reviewer authority.
`ReviewDocument`, `ReviewDecision` and the schema-22 review table have no reviewer
authority reference or domain grant. The methods also accept no trusted context.
Rejecting a `--role` flag and telling workers not to promote do not implement the
specified application boundary. By contrast, hard-rule/cutover ingress actually
verifies signed owner-control documents.

Required: resolve trusted attempt/control identity at ingress, restrict each
operation by scoped policy, persist the applicable reviewer authority and recheck
it at commit. This concerns the supported CLI/service path, not direct same-user
SQL/file tampering. **T04.5 must also reopen**, especially after T06.2 introduced
the previously unavailable promotion command.

### 18. P1 — Manual imports directly replace approved heads instead of staging candidates

The original T05.3 and protocol section 1 explicitly require manual edits to become
expected-revision candidates for user/control review. `src/memory/import.rs:207`
defaults a missing expected revision to the current head, then calls
`insert_revision`, advancing that head immediately. It does not preserve the
approved head until review. Its new revision is stale/unverified, so importing an
edit to a previously hard valid record removes that rule from active-fact retrieval
until somebody accepts it. Matching imported bytes can also return the old receipt
without enforcing a supplied stale expected revision.

Required: a separate candidate/staging record with the exact supplied base, a
preview diff and explicit control review before changing canonical heads. Do not
silently fill in the current head for a stale or omitted manual-edit base. Keep
initial shadow migration separate from post-cutover candidate import.

### 19. P1 — Historical snapshots cannot reconstruct their original instructions

The original fan-out sections 2 and 4 require exact historical instructions and a
reconstructible immutable knowledge input. `src/store/memory.rs:233` hashes the
instructions only as part of the snapshot ID's cache preimage. Schema 19 does not
store their text/object identity, and the manifest contains only memory entries.
The task declaration is retained only as a scope digest; its domains/paths/sensitivity
cannot be reconstructed for future selective routing. `brief_for` rereads the
current PROJECT.md, so an older manifest cannot reproduce the original full brief.

Required: persist immutable instruction and request/config identities with retained
verified content, plus the serialized manifest/selection receipt. Render historical
snapshots exclusively from their recorded inputs; changing policy produces a new
snapshot and reconciliation requirement, not a silent hybrid of old/new input.

### 20. P1 — T06.2 lacks the required atomic affected-task and routing obligations

The original T06.2 explicitly requires heads, sequence, invalidation and delivery
intent in one transaction. Protocol section 5 requires looking up affected tasks
through snapshot membership, subscriptions and semantic dependencies (or a blocking
generation barrier). The implementation writes an invalidation only for the
proposal's producing task (`src/memory/review.rs:108`), not the affected consumers,
and `src/store/reviews.rs:57` creates no operations/outbox routing obligation.
The code's rollback test verifies a record and receipt, not the missing required
outbox/affected-task set. T06.3 can implement transport later; it cannot retroactively
make an earlier promotion's missing obligation atomic.

Required: resolve a bounded affected set under the promotion transaction and commit
stable change IDs, blocking invalidations and durable routing work together. Add
multi-consumer, transitive dependency, missing-outbox rollback and restart tests.

## Other implementation and coverage observations

- The W04 snapshot-accounting work improves coverage and reuses already decoded
  inputs. However `src/store/approvals.rs:22` still materializes revocation/use
  rows without charging the shared read budget. New memory APIs use unbounded
  ordinary store reads; full bounded-memory operation is not established.
- Executor metrics are useful and avoid payload/config disclosure; named profile
  resolution keeps launch capability unverified and production dispatch disabled.
  The new metrics reader checks file length before an unbounded `read_to_end`, so
  concurrent growth still bypasses the intended read cap. Change to a bounded read.
- SQLite immutable-revision triggers, foreign keys, record CAS, signed memory
  policy namespaces and promotion rollback tests are worthwhile foundations.
  Passing these tests does not cover the failing lifecycle and evidence cases above.
- Most new memory tests call library constructors directly. There are no new
  complete CLI acceptance tests for import/cutover/recovery/checkpoints/propose/
  review/promote; the only new CLI test in this diff exercises profile resolution.
- The ledger still contains contradictory inherited prose/counts. Original-card
  acceptance and independent wave gates must be reconciled before claiming closure.

## Recommended card disposition

| Card | Review disposition | Required closure evidence |
| --- | --- | --- |
| T04.2 | Metrics/executor addition useful; preserve platform limits | Existing queue/fault acceptance plus bounded metrics read |
| T04.3 | Remains partial, correctly | Real sealed per-attempt profile producer |
| T04.5 | Reopen as partial | Enforced attempt/reviewer/user separation, scoped reviewer identity and promotion fencing |
| T05.1 | Reopen as partial | Verified object I/O, concurrent GC/reference fencing and restart recovery |
| T05.2 | Reopen as partial | Actual payload budgets, valid cache identity, task binding and scope tests |
| T05.3 | Reopen as partial | Verified pre-publication cutover, crash recovery, authoritative projection refresh |
| T05.4 | Reopen as partial | Complete bounded context, session/subscription fencing, mandatory instructions |
| T06.1 | Reopen as partial | Real proposal/evidence ingress and attempt/snapshot validation |
| T06.2 | Reopen as partial | Atomic review-state checks, correct semantic changes and stable receipts/intents |
| T06.3–T06.5 | Still unimplemented | Selective delivery/ack, invalidation/forget/purge, barriers per original cards |

The six claimed memory cards and T04.5 cannot count as complete against the original
plan. Keeping T04.2's local closure provisionally gives **17 implemented, 10 partial,
14 unstarted**, not 24 implemented. This is a recommended correction, not an edited
ledger or a new certification of the pre-existing cards. W05/W06 wave acceptance
remains open.

## Original-plan alignment and deliberate deferrals

| Original requirement | Current evidence |
| --- | --- |
| T04.5 worker/reviewer/user command boundary | Missing scoped reviewer authorization; finding 17 |
| T05.1 stable hashes/evidence, safe object lifecycle | SQL identities exist; actual byte integrity, proposal retention and recovery fail; findings 5, 8, 9 |
| T05.2 deterministic relevant complete knowledge input | Metadata manifests exist; budgets/cache/task scope/instruction retention fail; findings 2–4, 12, 19 |
| T05.3 reviewed imports and explicit verified reversible cutover | Basic importer/marker exist; candidates, digest binding, recovery and projections incomplete; findings 6, 7, 18 |
| T05.4 full reconstruction and acknowledged deltas with current rules | Basic checkpoints/acks exist; content, session identity and compatibility incomplete; findings 10, 11, 16 |
| T06.1 evidence-bearing validated worker proposals | JSON/idempotency foundation exists; trusted attempt binding, evidence validation/ingest incomplete; finding 13 |
| T06.2 semantic review plus atomic policy/base/invalidation/outbox transaction | Record CAS/rollback exists; authority, semantic changes, stable receipts and obligations incomplete; findings 1, 14, 17, 20 |
| T06.3 selective fan-out and exact applied acknowledgments | Explicitly unstarted; checkpoint ack is not this worker protocol |
| T06.4 transitive invalidation, revoke/forget and retention | Explicitly unstarted; a revoke-head policy and orphan GC are not the full protocol |
| T06.5 consistent wave-memory barriers | Explicitly unstarted |

The original traceability document explicitly defers embeddings, an MCP facade,
hostile-agent OS isolation, cross-root quotas and automatic merge/push. Their
absence is not a defect. Correctness does not require a vector database.

The later draft records a user waiver for starting W05 before the W04 wave exit,
and intentionally defers commit/tree ranking to W07 and production launches to
crash certification/profile work. These are recorded scope decisions, not passed
original requirements. The waiver does not erase the original memory correctness
criteria above. W06.1/T06.2 likewise do not establish a passed W05/W06 wave gate.

## Finite repair sequence

1. Repair object integrity, GC reference ownership and promotion fencing. Pass the
   reproduced data-loss/authority tests before adding more memory features.
2. Repair snapshot identity, eligibility, body budgeting and exact task binding;
   use the same verified rendering contract for workers and coordinators.
3. Implement the cutover recovery state machine and projection freshness checks;
   kill/restart at every durable transition in disposable CLI fixtures.
4. Complete proposal evidence ingestion, attempt binding and semantic promotion.
   Require a full CLI flow for each reopened card, including failure and replay.
5. Then implement T06.3–T06.5. Top-quality shared memory also needs consumer
   acknowledgments, dependency invalidation and completion barriers; revision storage
   and promotion alone do not keep running workers consistent.

## Existing-suite validation

- Default-feature `cargo check --locked --offline`: passed.
- All-feature library suite: **251 passed**, zero failures.
- All-feature binary suite outside the socket-restricting sandbox: **505 passed,
  3 failed** because host `/proc` permissions prevented cleanup quiescence checks.
  A focused rerun in a disposable user/PID namespace passed all three, plus two
  other matching tests (**5 passed**). Thus all 508 binary tests passed across
  the main run and isolated rerun, not in one clean full-suite run.
- Contract integration suite: **2 passed**, zero failures.
- All-feature CLI suite: **42 passed, 4 failed**. Three failures are the existing
  context/profile acceptance break described in finding 16. The fourth, native
  artifact copy, reported the running executable as `herdr-projects (deleted)`:
  a concurrent validation build replaced it. A focused rerun without builds
  **passed** (1 passed, 45 filtered out). That first failure is not evidence of
  a memory defect. The three context/profile failures remain unresolved.
- Independent memory probes: **17 failed at the intended assertions**; see the
  accompanying saved probe source and result log.

Raw existing-suite logs are in `/tmp/herdr-review-lib.log`,
`/tmp/herdr-review-existing-unsandboxed.log`, `/tmp/herdr-review-cleanup-isolated.log`,
`/tmp/herdr-review-contracts.log`, `/tmp/herdr-review-cli.log`,
`/tmp/herdr-review-copy-rerun.log`, and `/tmp/herdr-review-default-check.log`.
These host-local logs are not repository artifacts. `git diff --check` passed;
production files and the ledger remain unchanged.

## Reproduction

The probe source is outside Cargo's automatic test discovery intentionally: it
captures currently failing acceptance requirements without making ordinary test
runs fail solely because this review added evidence.

```sh
cp docs/reviews/2026-09-20-memory-audit-probes.rs tests/review_memory_audit.rs
CARGO_HOME=/tmp/herdr-projects-cargo cargo test --locked --offline --all-features --test review_memory_audit -- --test-threads=1
rm tests/review_memory_audit.rs
```

Use the normal Cargo cache instead of the `/tmp` cache on another machine.
All probes use disposable projects; no live project is migrated or modified.
The original tests were run separately from these expected failures. No release,
macOS or real SSH certification was performed by this review.
