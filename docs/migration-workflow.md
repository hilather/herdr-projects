# W03 migration workflow (partial T03.2)

Implemented 2026-09-19 after the reviewed T03.1 commit `52884bf` was pushed to the
fork. Independent review approved this conservative offline migration subset.
T03.2 remains partial and W03 acceptance remains open.

## What is available

Build with `--features state-store`. The disabled-by-default feature adds:

```sh
herdr-projects migration demo inspect
herdr-projects migration demo preflight
herdr-projects migration demo plan --output /outside/project/plan.json
herdr-projects migration demo apply --plan /outside/project/plan.json --writers-stopped
herdr-projects migration demo status
herdr-projects migration demo recover --writers-stopped
herdr-projects migration demo upgrade-store
herdr-projects migration demo export
herdr-projects migration demo abort
herdr-projects migration demo restore --destination /new/recovery/directory
```

Inspect and plan are dry runs. They fingerprint sources and report blockers without
creating a store. CLI inspect/plan produce version-2 plans binding the external
config path and its absence or content fingerprint into the migration ID. Config
values are not included. A plan must be written outside the source project. Apply requires
an unchanged plan, a paused/archived project, explicit stopped-writer confirmation,
and exclusive ticker, root execution and project locks. It does not stop processes
on the operator's behalf or migrate any actual user project during implementation.

This importer deliberately blocks active/uncertain threads, coordinator pane
identities, remote identities, removal receipts and unsupported ticker state. Supported pending
inbox/finalization/notification obligations now convert into ambiguous durable
intents; see [delivery semantics](operation-delivery.md). Do not erase blocked
records to bypass preflight: unsupported state and live identities still need
explicit conversion or reconciliation. Known field types, enums and identities are
validated; unsupported shapes block rather than being silently dropped. Unknown
thread/settings fields are preserved in the original bytes.

## Ownership and recovery

Schema v2 added source provenance and an import receipt; v3 adds durable delivery
state and the imported-operation count; v4 adds canonical inbox records and v5
adds typed, unverified runtime identities. Opening an older supported schema does
not upgrade it. Use `upgrade-store` explicitly. Fresh stores use v9 (observations,
lifecycle control, canonical bindings and adopted ownership); the runtime
ownership marker remains `sqlite-v2`. Newer unknown schemas refuse writes.
Import stores task mappings, raw runtime/thread/inbox/task bytes, hashes, supported
operation intents and audit events in one transaction. It verifies exact bytes, identities, counts, task states,
integrity and foreign keys against the backup before publication.

Checkbox tasks import as draft or awaiting_review. Resolved thread narratives are
awaiting_review, not verified success. Arbitrary task Markdown remains intact in
source provenance. No attempt or operation is dispatched. Existing seen IDs stay
unchanged; imported inbox records are not marked handled or delivered.

The fsynced journal advances through prepared → imported → verified →
cutover_pending → active. Recovery imports an empty initialized staging store,
recognizes an already published store, and verifies the marker and unchanged
import receipt. The store is moved only after its last connection closes and no
WAL remains. Marker and projection publication use complete temporary files;
projection publication never replaces an existing final file. Same-process
exporters are serialized. Edited projections are preserved and reported as conflicts.

An interrupted initial directory reservation can be retried by apply. A malformed
or incompletely initialized staged DB can be explicitly aborted before cutover;
abort archives all staged bytes/backups and releases the legacy guard without
replacing original records. After publication, recover forward or restore into a
new directory. Do not remove the marker and run an older binary against the root.

The format marker declares runtime owner `sqlite-v2`, memory owner
`legacy-markdown`, and initially `reconciliation_required=true`. Schema v7 derives
the latter from canonical lifecycle control; see [admission and recovery](reconciliation.md). Legacy commands in both
feature-enabled and default builds refuse mutation/execution after preparation;
list displays store/maintenance status. The ticker can deliver accepted canonical
notifications/local finalizations through guarded adapters. Worker launching and
unsupported imported/remote effects remain blocked; see the
[W03 handoff](w03-acceptance.md). Store-backed `task PROJECT
list/show/add/rename`, `operations PROJECT inspect` and `context PROJECT` are now
available; task mutations require expected revisions/event heads. `active` means ownership was
published, not that the scheduler is enabled.

`MEMORY.md`, project instructions and other source files remain untouched. Backups
include their original bytes; no DB memory authority or memory projections are
created. Worktrees, branches and external agent resources are never altered.
Generated views live under `.state/projections/schema-<schema>-revision-<event-head>`;
legacy files are retained as pre-cutover originals rather than dual-written views.
Coordinator execution adapters remain unavailable. Store-backed context prints
current task revisions and the ownership/dispatch restrictions alongside user-owned
project and memory text. Recovery after publication preserves new task edits.

## Backup, bounds and limits

Backups, plans, journals and projections contain private project text. New files
use mode 0600 and enclosing migration/export/recovery directories use mode 0700.
Verify/restore checks hashes and rejects symlinks in controlled ancestors as well
as source files. Restore targets a previously nonexistent directory and never
replaces the live root. Backups and aborted staging directories are retained for
manual retention management.

Inventory is bounded to 16 MiB per file, 128 MiB total, 10,000 files and depth 64.
It covers project-local files, not remote worktrees or external user configuration;
those are not edited. The operator must supply local storage and sufficient space.
Apply checks the filesystem type and estimated free space on the destination
`.state` directory, and rejects staging on a different device. The estimate is four
times source bytes plus 64 MiB. A recognized Linux filesystem type (including
overlay) does not certify its physical backing or durability. Non-Linux types
remain unverified.

Read-only `preflight` fingerprints external config with bounded, no-follow reads
and reports top-level keys without values. For recorded live identities it checks
Herdr compatibility and queries the saved socket/machine, pane and agent lists.
Local worktree identity checks compare path and branch. Missing, mismatched, idle
and unreachable resources never authorize cutover: existing identity blockers
remain. Preflight report fingerprints are diagnostic; CLI inspect/plan separately
bind the config reference into the apply plan. Complete profile resolution and
live ownership reconciliation remain unfinished.
Unknown/live states block rather than receiving fabricated success evidence.

Migration does not retroactively teach older installed binaries about the marker.
Keep old executables and manual writers stopped. Locks coordinate cooperating
current commands, not hostile same-user processes. Real disk/power-loss and macOS
validation remain untested.

## Evidence and next work

Native fixtures cover byte-exact import/export/restore, changed plans, malformed
settings/runtime/inbox records, held locks, source and backup symlinks, explicit
v1 upgrade, edited projections, incomplete temporary publication, abort, and
recovery after process death at every durable journal phase. CLI tests exercise
the full offline workflow and verify legacy guards with and without the feature.
The initial offline subset passed 251 feature-enabled tests. The subsequent
[delivery and task-adapter foundation](operation-delivery.md) adds further tests;
see implementation progress for current totals. The default-feature suite
retains its 226 tests. Three explicit live fixtures remain ignored by default. The
locked all-feature release build and `git diff --check` pass. Rustfmt remains
unavailable in the installed toolchain.

```sh
cargo test --all-features --locked --offline
cargo test --all-features --release --locked --offline
cargo build --all-features --release --locked --offline
cargo test --no-default-features --locked --offline -- --test-threads=1
```

Temporary logs use `/tmp/herdr-migration-{debug,release,build,legacy}.log`.
Remaining T03.2 work: complete runtime execution adapters and profile resolution,
and integrate live ownership reconciliation. Supported pending-operation conversion
and basic task commands now exist against T03.3's delivery contract. T03.4 must reconcile before dispatch can resume. No
complete T03.2 or W03 acceptance is claimed. Proposed project knowledge: retain
this freeze/ownership distinction while integrating those layers; no shared-memory
promotion was performed.

Schema v4 imports inbox items and seen/done state from immutable database source
provenance, including during explicit upgrades. Store-backed inbox list/done and
context now use that authority. Ordinary context marks the displayed unseen IDs
with an event-head check; `--peek` leaves them unchanged. See the internal delivery
adapter in [operation delivery](operation-delivery.md).

## External config binding

CLI apply requires a version-2 plan for the current resolved config path. A config
file appearing, disappearing or changing after planning blocks apply before any
migration reservation. Resumed pre-cutover phases recheck the recorded reference
before publishing ownership; mismatched CLI config locations require the original
location. Settings remain user-owned and are not copied or overwritten. Restore
recreates project-local bytes only; the journal retains the external fingerprint
for comparison, not an external config backup.

Old version-1 plans must be regenerated for a new CLI apply. Already-prepared
version-1 journals remain recoverable under their original project-only contract.
After the journal reaches active, config edits do not prevent store reads or
forward recovery. Root resolution also bounds config reads to 16 MiB and rejects
special files without blocking, even when no explicit root is supplied.


## Runtime identity records (schema v5)

`migration PROJECT bindings` reads typed thread/coordinator identities from the
store with their source fingerprints and binding revisions. Thread sockets come
only from the preserved coordinator record; its separate fingerprint is retained.
An absent socket stays empty and never falls back to an ambient Herdr session.
All bindings are explicitly unverified. These records grant no ownership, do not
release reservations, and do not enable external effects.

Explicit upgrades populate bindings from hash-checked database provenance in one
transaction, preserving current tasks and event head. Unknown legacy fields remain
in the original source bytes. Reads check payload hashes, referenced source bytes,
session provenance and inventory completeness, including on an already-open store.
Corruption is reported rather than producing an empty identity list. Schema-v5
runtime exports include typed bindings; older schema exports remain unchanged.
Full runtime mutation, resource adoption and live reconciliation remain unfinished.

Schema v6 adds durable runtime observations. See [reconciliation](reconciliation.md)
for the read-only collector, recording protocol and remaining execution boundary.
