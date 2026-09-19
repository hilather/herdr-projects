# ADR 0002: Phase B domain/store contract and SQLite selection

Status: implementation contract, 2026-09-19; independent wave review remains open.
Task T00.2. No application storage migration or new application dependency.

## Ownership and transaction boundary

[Compiled illustrative types](../../contracts/phase_b.rs) separate task state,
attempt state and terminal presentation. They are included by Cargo integration
tests, and deliberately do not introduce a second runtime authority in Phase A.
Identifiers for tasks, attempts, operations, snapshots, proposals, results and
approvals are distinct Rust types. Production constructors and schemas belong to
W03/W05; these illustrative wrappers do not validate untrusted strings.

`StateStore` targets one project on local storage. A commit compares the expected
project event head and each changed entity revision, then atomically applies the
whole batch: domain records, approval consumption, events and operation intents.
Insert-only uses `None`; updates must increment revision exactly once. The store
allocates strictly increasing event sequences at commit. Conflicts roll back the
whole batch, including approval use counts and outbox intents. Sequence values
are project-local and cannot be compared across projects. Historical reads must
fail explicitly when the requested history is unavailable.

Short synchronous transactions contain no external commands, terminal prompts,
semantic reviews or network I/O. Dispatch claims and fencing epochs protect the
supported worker path; an ambiguous external response requires observation and
cannot authorize blind retry. Only evidence of no effect permits ordinary retry.
W03 implements persistence/reconciliation; the interface does not promise exactly
once execution or turn a database commit into proof of external success.

## State and approval boundaries

Task states: draft, queued, ready, running, awaiting_review, blocked, succeeded,
failed, cancelled. Attempts: reserved, launching, running, awaiting_input,
completed, failed, cancelled, lost. W04 validates transitions and dependencies
against revisions. Lost or completed attempts retain capacity until termination
is observed; a report or an idle UI state cannot release the reservation.

Approval binds actor/channel, operation class, target, payload hash, revision,
expiry and remaining uses. Execution must compare every binding and consume use
in the same transaction as the operation claim. W04 implements policy evaluation.
Same-OS-user agents remain cooperative participants, not a containment boundary.

Result evidence binds repository, commit/tree, integration base, memory snapshot,
criteria, commands/tool versions, exit codes, artifact hashes and disposition.
Changed bindings make evidence stale. Fixture evidence, explicit user disposition
and verifier evidence stay distinct. The interface helper only checks binding and
labels provenance; it does not verify a result. W07 supplies automated verification.
Passing a gate never grants permission to merge, push or delete.

## SQLite dependency and engine decision

Select **rusqlite =0.37.0**, linked to a separately maintained **system SQLite
3.53.4 or later**, for the first W03 implementation. Pin the dependency/lockfile
when that implementation lands; require a runtime engine-version check and reject
older engines before opening project state. Linux packaging needs SQLite and its
build headers/pkg-config; macOS packaging must supply a supported SQLite rather
than assume the OS copy is sufficiently recent. Mac packaging remains untested.

Actual Rust 1.89 probes on Linux:

| Candidate | Result |
| --- | --- |
| rusqlite 0.40.1, bundled | Compilation fails on unstable `cfg_select`; rejected for this MSRV. Cargo's compatible-version resolver did not catch it. |
| rusqlite 0.37.0, bundled SQLite 3.50.2 | Compiles and passes the native transaction fixture, but the old engine is rejected for production selection. |
| rusqlite 0.37.0, system SQLite 3.53.4 | Compiles and passes CAS, event, transaction, rollback and integrity checks. Selected integration. |

rusqlite and libsqlite3-sys 0.35.0 declare MIT licenses in their downloaded
manifests; SQLite is public domain. The selected feature set excludes SQLCipher
and OpenSSL. The inspected transitive manifests are MIT/Apache-2.0 for
hashlink, hashbrown, smallvec, the fallible iterators, pkg-config and vcpkg; foldhash
uses Zlib. Preserve applicable notices when distributing. The wrapper API and engine upgrade are deliberately separate: the
[upstream wrapper](https://github.com/rusqlite/rusqlite) remains actively released,
while [SQLite's release history](https://sqlite.org/changes.html) documents recent
engine fixes, including the WAL-reset corruption fix in 3.51.3/3.53.0. Compiling an
old bundled engine is not a maintenance review. Recheck upstream fixes at each
packaging/release gate; the minimum above is the reviewed floor, not a claim that
future versions can be shipped without testing.

The reproducible [standalone native probe](../../contracts/sqlite-smoke/) has its
own lockfile and no connection to user state:

```sh
CARGO_TARGET_DIR=/tmp/herdr-sqlite-smoke-target cargo run --locked --manifest-path contracts/sqlite-smoke/Cargo.toml
cargo test --locked --test contracts
```

This is dependency/interface evidence, not W03 concurrency, WAL recovery or
power-loss acceptance. W03 must add file-backed transaction/crash tests, busy
handling, foreign keys, migration locks, schema refusal, backup and rollback.

## Authority cutovers

Keep current Markdown/TOML/JSON authoritative until the explicit W03 runtime,
task and event migration. Memory Markdown remains authoritative through W03 and
moves only at T05.3. Never dual-write two authorities. Exported legacy projections
must identify their owner/version and cannot silently become authoritative again.
No database creation, schema migration, model-specific flags, automatic approval
or release authorization is introduced by this contract batch.
