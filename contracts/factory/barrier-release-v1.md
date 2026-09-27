# Barrier release authorization version 1

[Fixed JSON bytes](barrier-release-v1.json) and their
[SHA-256 digest](barrier-release-v1.sha256) describe the owner authorization
consumed by the [release service](../../src/authority.rs). The fixture has
synthetic identities and is not an authorization for any real project.

The vector uses UTF-8 JSON, sorted object keys, two-space indentation, decimal
integers, and one final LF. Its hash includes that LF. Production verification
uses the original received bytes, without reserialization; alternate whitespace
therefore changes the authorization digest. Unknown and duplicate fields,
including nested authority fields, are rejected. Documents are limited to
65,536 bytes and signatures to 8,192 bytes.

The signature namespace is `barrier-release@herdr-projects`. Only the current
owner key from the project's externally pinned configuration can prepare the
trusted release capability. A launch, reconciliation, or delegation signature
cannot authorize this action. A request's `authority` reference must match that
key's current policy revision and digest.

The signed fields bind the canonical project-store path, store incarnation,
configuration digest, control revision and epoch, event head, barrier ID,
version-2 memory manifest digest, required-set generation, release policy, and
issuance/expiry milliseconds. Digests use 64 lowercase hexadecimal characters;
revisions and counters fit signed SQLite integers. The only supported action
is `release_barrier`; the only supported policy is `all_members_ready_v1`.
This policy requires every frozen member to satisfy its signed contract's
result/integration route, proposal dispositions, and current memory readiness.
It permits no waiver of those checks.

Release revalidates these references inside an immediate transaction with
active, reconciled project control. The clock is read after acquiring the write
lock and again immediately before committing a new release. Expiry during
readiness checks or receipt insertion rolls back the complete transaction.
Drafting likewise refuses an authorization that expired while its evidence was
being prepared. The exact authorization bytes, digest, release event, and barrier
state commit together. The authorization ledger is immutable. Exact replay returns
the same historical release under the still-matching control identity; it does
not create another release. An unsigned historical release cannot gain signed
evidence retroactively. Schema upgrades create no authorization backfill.

A subsequent revocation is append-only and preserves the signed release.
`FrozenBarrier` reports both historical release and current revocation sequences.
Exact release replay can therefore return a revoked historical receipt; it does
not restore applicability. Current dependency checks include the supplemental
release-revocation ledger and do not accept a revoked superseding release.
Blocking memory invalidations publish cause-linked barrier revocations
transactionally. Routing reads only indexed unrevoked memberships and refuses
runtime fan-out above 1,000 barriers. Upgrade appends a current decision for
historical barriers with retained unresolved invalidations; it does not rewrite
the historical release or claim a retroactive revocation.
Record and policy publishers also route directly through indexed frozen memory
dependencies, including transitive sources, even after worker bindings retire.
Unrelated optional changes and unchanged redelivery do not revoke version-2
barriers. Required/contract and policy changes use the read set's conservative
global scope. Legacy read sets are conservatively invalidated on memory changes.

Parser vectors, real Ed25519 signature checks, transaction faults, and the
published-store service scenario run with:

```sh
cargo test --locked --features state-store --lib -- barrier_authorization barrier_release_signature authorized_barrier --test-threads=1
```

The service's workflow is documented in [memory barriers](../../docs/factory/memory-barriers.md).
Signed release and inspection expose `release_reference` for the version-2
[downstream task contract](task-contract.md). Its launch boundaries revalidate
the exact release and present readiness. Revocation transactionally invalidates
downstream applicability and records stop obligations; retained capacity is
released only after quiescence is proven. Automatic orchestration and live F3
certification remain separate requirements.
