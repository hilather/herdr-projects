# Task contract byte vectors

These fixtures pin signed bytes for the existing version-1 contracts and the
version-2 barrier requirement:

| Fixture | Git identity | Dependency policy |
| --- | --- | --- |
| [SHA-1 JSON](task-contract-sha1-v1.json), [digest](task-contract-sha1-v1.sha256) | 40 lowercase hex characters, explicit `sha1` | Legacy same-name policy binding |
| [SHA-256 JSON](task-contract-sha256-v1.json), [digest](task-contract-sha256-v1.sha256) | 64 lowercase hex characters, explicit `sha256` | Explicit predecessor policy digest |
| [Barrier v2 JSON](task-contract-barrier-v2.json), [digest](task-contract-barrier-v2.sha256) | 40 lowercase hex characters, explicit `sha1` | Exact signed barrier release required in addition to dependency policy |

Document version 2 requires `required_barrier`, a strict object containing
`schema_version: 1`, `barrier_id`, `release_sequence` and
`authorization_digest`. Both IDs are lowercase SHA-256 digests; the sequence
must be a positive signed-64-bit-compatible integer. Version 1 cannot carry
this object. Existing version-1 bytes and launch-input serialization stay intact.
The launch approval binds the entire contract digest, including this reference.

Draft validation, reservation, launch claim and the pre-effect approval check
require the exact immutable signed release, matching owner policy and config,
current store incarnation/control identity, an unrevoked barrier and current
member/memory readiness. An unsigned historical release is insufficient.
The release signature's expiry governs execution of the release; later use
rechecks current readiness rather than renewing the signature.

New result ingestion and verification, verified-result integration lookup,
integration creation and pre-publication checks also revalidate the barrier.
Version-2 results must match the exact contract bound in their attempt's retained
launch inputs. Replaying an earlier submission or verification record preserves
history rather than granting fresh acceptance or integration authority.

These use synthetic paths and object IDs. They are parser/identity fixtures,
not signed authorizations, valid Git object stores or runnable project requests.
Only trusted signature verification can construct the production install
capability. Parser tests run inside the crate at that boundary.

Fixture encoding is UTF-8 JSON with alphabetically ordered object keys,
two-space indentation, decimal integers, and one final LF. The adjacent digest
is SHA-256 of every file byte, including that LF. Arrays retain their written
order. The current version-1 parser does not require sorted object keys or
canonicalize alternate encodings. A whitespace-only change therefore produces
a different signed identity, even when the parsed meaning stays the same.
Do not reserialize a received signed document before verification or storage.

Git OIDs and content digests are distinct: changing `object_format` without
changing the OID to the corresponding length is rejected. Content/policy
digests remain lowercase SHA-256, regardless of Git object format.

The current document ceiling is 65,536 bytes. Acceptance policies are limited
to 32, dependencies to 64, capability flags to 32, scope paths to 64 and named
resources to 8. Duplicate policy IDs, predecessor IDs, normalized scope paths
and named resources are rejected. Critical struct fields reject duplicates and
unknown names. Version 1 still accepts redundant capability flags and unordered
sets; a stricter future canonical-set format needs an explicit compatibility
decision rather than changing the meaning of retained signed bytes.

[Domain regressions](../../src/domain/factory/contract_tests.rs) check both fixed
digests, raw-byte retention, whitespace identity changes, nested critical-field
rejection, duplicate dependencies, revision bounds and Git-format mismatch.
Run them with:

```sh
cargo test --locked --features state-store --lib domain::factory::contract_tests
```

These vectors complement the [implementation map](catalog.md). They do not
establish complete catalog-wide vectors, signature-service acceptance,
reservation authority, or F0.4 independent review.

Version 3 adds [required Git output declarations](task-contract-outputs-v3.md).
The [JSON vector](task-contract-outputs-v3.json) and
[digest](task-contract-outputs-v3.sha256) retain the same exact-byte signing rule.

## Dependency graph at installation

New signed revisions are checked atomically against the reachable dependency
graph before publication. The proposed revision replaces the target task's old
edges for this check. Other contracted tasks use their latest signed revision;
uncontracted tasks use their queue prerequisites. Cycles are refused without
publishing a contract, scope/resource claims or installation event. Exact retries
of an already installed revision retain their existing read-only semantics.

Validation reads only reachable identities and latest contract bytes. It bounds
work at 10,000 tasks, 100,000 edges, 256 queue edges per uncontracted task,
32 MiB of signed documents and a two-second traversal budget. Each signed
contract retains its existing 64 KiB / 64 dependency limits. These are fail-closed
limits, not a scale-certification claim. The walk and cycle detection are
iterative and do not consume call stack proportional to graph depth.

## Results remain bound to reserved inputs

When immutable `attempt_inputs` include a task-contract reference, a submitted
result must match that exact task, revision and digest, even for a contract with
no barrier. A newer owner-signed revision does not rewrite an existing attempt's
frozen inputs. For ordinary contracts, the original revision remains usable for
that attempt after supersession. Barrier-backed contracts retain their additional
current-contract/release/configuration requirements. Historical manual attempts
without a frozen contract reference retain their existing ingestion semantics.

The shared check runs at result ingestion, verification and later evidence use.
Submitting or replaying a result does not itself mint a verification receipt or
release retained attempt capacity.
