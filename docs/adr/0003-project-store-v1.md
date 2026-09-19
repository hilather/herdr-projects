# ADR 0003: opt-in project store schema v1

Status: independently reviewed T03.1 local implementation, 2026-09-19. Phase A was accepted by
explicit user disposition with its documented platform gaps. W03 is in progress;
this ADR does not authorize a runtime or memory authority cutover.

## Integration and ownership

`state-store` is a disabled-by-default Cargo feature exposing the library's
`domain` and `store` modules. The legacy binary does not initialize or use a DB,
even when this feature is compiled. T03.2 will supply an explicit migration and
its runtime integration. No existing project files or live sessions were migrated.

The store owns `migrations/`, schema versions, transaction boundaries and typed
repositories. T03.2 owns import, backup manifests, format markers and generated
runtime/task projections; it must use this repository rather than put SQL in CLI
handlers. T03.3 owns operation claim/delivery/lease logic and proposes a store-owned
schema migration for mutable operation fields. Both tasks are serial until their
shared schema changes are integrated. T03.4 reconciliation follows both.

Schema v1 contains tasks, attempts, immutable operation intents, events and store
metadata. Future schemas reserve memory/snapshot/proposal/result namespaces but
create no memory authority or projections now. Snapshot references in attempts are
optional opaque identities until W05. IDs have distinct validated Rust types.
Task/attempt state enums implement the T00.2 vocabulary; W04 owns legal policy
transitions and approval evaluation. No API currently dispatches intents or claims
that persisting an intent authorizes its execution.

## Transaction contract

`SqliteStore::create(path)` is explicit and refuses existing files. `open(path)`
never creates or migrates; both require a regular file on local storage. Callers
must provide the local project path; this API does not certify arbitrary mounts
as local or provide hostile same-user containment. Symlink database files refuse.
Schema/application identities are checked before enabling WAL, and again inside
every commit/snapshot. Unknown versions refuse writes. Initialization is one
transaction; an interrupted empty/unrecognized file requires explicit recovery,
not automatic adoption or deletion.

The reviewed rusqlite 0.37.0 dependency is pinned and links system SQLite. An engine
floor check rejects SQLite older than 3.53.4 before database creation/opening.
WAL uses FULL synchronization, foreign keys, a 250 ms busy timeout and automatic
checkpointing at 1,000 pages. A passive checkpoint API supports maintenance.
Transactions expose typed batches, not arbitrary caller callbacks, so external
commands/prompts cannot be held inside the repository transaction.

A commit compares the project event head and every supplied expected revision.
New entities start at revision one; each entity may occur only once per batch,
and updates advance exactly once. Attempts cannot move between tasks. Deferred
foreign keys allow a task and its selected attempt to enter atomically and prevent
cross-task selected-attempt references. Reservations remain unique while
termination is unobserved, including lost/completed attempts.

Every mutation appends a versioned full-record audit event. Events and operation
intents commit or roll back with domain changes. Intents have unique IDs and
idempotency keys, bind the task revision at insertion, and store a SHA-256 hash of
the serialized payload. Hash mismatches surface on read. Idempotency collisions
are conflicts, not silent success; T03.3 must reconcile any duplicate intent.
Batches are capped at 1,000 mutations, 1 MiB per encoded record and 16 MiB total.

Snapshot reads use a single read transaction. Historical heads explicitly return
`HistoryUnavailable`; an event log is not yet a historical reconstruction engine.
The current API materializes the project snapshot and event history; pagination
and retention remain future work before large-scale use. The illustrative
`contracts/phase_b.rs` remains the cross-wave design reference, not the runtime
schema: approval consumption and leased delivery are intentionally absent until
their policy and operation layers exist.

## Validation and limits

Native temporary, file-backed fixtures cover CAS conflicts, duplicate IDs and
intent keys, rollback after earlier writes, deferred foreign keys, reservation
retention, stale operation bindings, concurrent connections, bounded busy errors,
consistent readers, restart, unknown schemas, malformed DB files, payload hash
corruption and ID/integer validation.

A `max_page_count` fixture produces a real SQLite `SQLITE_FULL` failure and checks
that records/events/intents roll back and remain intact after reopen. This simulates
database capacity exhaustion; it is not a physical disk or filesystem fault test.
A native child process is killed with dirty uncommitted WAL pages and separately
after an acknowledged commit; recovery preserves respectively none or all of the
batch. This tests process death, not hardware power loss.

Reproduce with Rust 1.89 and supported system SQLite:

```sh
cargo test --features state-store --locked --offline --lib
cargo test --all-features --locked --offline
cargo test --all-features --release --locked --offline
cargo build --all-features --release --locked --offline
cargo test --no-default-features --locked --offline
```

Linux is tested. macOS builds, packaging and filesystem fault/power-loss coverage
remain untested; the Phase A disposition does not certify new W03 platform work.


## Result and review

The separate review agent approved the final store implementation and ownership
contract with no remaining actionable findings, independently running all 12
store fixtures. This is T03.1 code review, not acceptance of the whole W03 wave.
The combined suite contains 237 passing tests with the feature enabled, including
225 existing tests; three explicit live Phase A fixtures remain ignored by default.
The default-feature legacy suite passes all 225 existing tests. One initial parallel
legacy run hit the existing process-audit transient refusal (“process identity
changed during writer inspection”); the full serial rerun passed without changing
that production guard. The 12 store fixtures also pass separately in debug. The all-feature release
build and `git diff --check` pass. Rustfmt remains unavailable in this toolchain.

Temporary logs: `/tmp/herdr-w03-debug.log`, `/tmp/herdr-w03-store.log`,
`/tmp/herdr-w03-release.log`, `/tmp/herdr-w03-legacy.log` and
`/tmp/herdr-w03-build.log`. The source fixtures and commands above are durable;
temporary logs are not release artifacts. Proposed project knowledge: use the
feature-gated repository and its schema ownership boundary for T03.2/T03.3;
no shared-memory promotion was performed.


## T03.2 schema extension

The subsequent [offline migration foundation](../migration-workflow.md) adds v2
legacy source provenance and an import receipt. At that stage fresh stores used v2; v1 opens
without implicit upgrade and has an explicit transactional upgrade API. The
original v1 transaction contract remains intact. All legacy commands now guard
against a migration journal/format marker, including builds without the feature.

The subsequent [delivery foundation](../operation-delivery.md) adds schema v3.
Explicit upgrades support published v2 projects and preserve their import identity.
The runtime ownership protocol marker remains independent of schema numbering.

Schema v4 adds canonical inbox records and preserves their seen/done state.
Explicit upgrades populate them from immutable database provenance. Fresh stores
now use v4. Snapshots include schema identity, and exports use schema-qualified
revision directories to preserve earlier schema exports at the same event head.
