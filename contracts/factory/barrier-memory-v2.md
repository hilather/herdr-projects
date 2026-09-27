# Barrier memory read-set version 2

[Fixed bytes](barrier-memory-v2.json) and their [SHA-256](barrier-memory-v2.sha256)
pin the encoding used by [the collector](../../src/store/barriers/memory.rs).
The fixture uses a synthetic store incarnation and snapshot hashes; it grants
no release authority.

The payload is compact UTF-8 JSON, with recursively sorted object keys, decimal
integers, and no trailing newline. Arrays preserve the query ordering described
below. SQL NULL becomes JSON null; text remains text, including applicability
JSON, whose retained bytes are not parsed or normalized. The lowercase SHA-256
digest covers every payload byte. The digest file ends with a newline.

Root fields are `schema_version` (2), `kind` (`barrier-memory`),
`required_set_generation`, `control`, `policy`, `scopes`, `required`, and
`members`. Query rows use positional arrays:

| Field | Columns in order |
| --- | --- |
| `control` | Store incarnation, control revision, epoch, state, reconciliation-required flag, configuration digest |
| `policy` | Latest memory-policy revision, payload hash; empty if no policy exists |
| `scopes` | Scope ID, generation; ordered by scope ID |
| `required` | Record ID, record key, scope ID, kind, hard flag, head revision, head status, head row revision, validity state, validity reason, expiry milliseconds, evaluated sequence, body hash, provenance hash, applicability text, body availability |
| Member `identity` | Task revision, task state, attempt revision, attempt state, termination-observed flag, snapshot ID, snapshot manifest hash, snapshot scope digest |
| Member `consumed` | Record ID, consumed revision, record key, scope ID, kind, hard flag, current head revision, head status, head row revision, validity state, validity reason, expiry milliseconds, evaluated sequence, body hash, provenance hash, applicability text, body availability |
| Member `dependencies` | Derived record, derived revision, source record, source revision, dependency kind |

Members follow canonical barrier member order and include `task_id` and
`attempt_id`. Required records are ordered by record ID, consumed revisions by
record ID and revision, and dependencies by all five columns. Required records
include active hard records, constraints, hard memory, and contracts; a missing
required head fails collection. Consumed revisions use the readiness check's
closure of starting snapshots, individually applied updates, package-applied
updates, and transitive sources. Transport acknowledgments alone do not count.

The scope vector conservatively includes the entire scope catalog. Unconsumed
optional observations do not change this vector. Changes to consumed evidence,
catalog generations, policy, project control, or member identity require a new
freeze. Release also rechecks current readiness, including time-sensitive
validity, and exact signed-contract/result/policy provenance. During this check,
the collector derives the earliest expiry of consumed revisions/transitive sources
and the global mandatory set used by readiness. Signed drafting and release carry
that deadline through prerequisite-barrier checks and refuse if it has elapsed
before returning the draft or committing the new release. Unconsumed optional
observations and optional contracts do not impose an expiry gate. The derived
deadline is transaction-local; it adds no payload field and changes no digest.

Collection has one shared 50-MiB weighted read budget, a 100,000-row budget,
a 16-MiB field limit, and a 10,000-row limit per section with one lookahead
row. Controlled CLI services share their original two-second request deadline;
raw trusted store collection uses a ten-second SQL deadline. Serialized payloads may not exceed 8 MiB.
These limits fail closed; they never truncate accepted evidence. Payload,
barrier header, membership, and freeze event commit in one transaction.
Retained payloads cannot be updated or deleted.

New freezes require schema 43. Upgrading a historical barrier does not invent
a read set. A pending version-1 barrier requires a new version-2 freeze and a
new token before its first release. A previously released historical receipt
remains inspectable and replayable as that same historical receipt.

The fixed-vector, mutation, rollback, payload-limit, and actual-prefix upgrade
regressions run with:

```sh
cargo test --locked --features state-store --lib store::barriers::tests -- --test-threads=1
```

This read set binds evidence; the deterministic release token is not an
authorization. The [signed release service](barrier-release-v1.md) supplies
explicit owner authorization and policy. Version-2 task contracts bind downstream
admission to the exact signed release. Automated orchestration and live F3
certification remain separate work.
