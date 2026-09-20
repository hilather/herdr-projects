# Operation-scoped authority (partial T04.5)

The initial approval contract defines inert grant records. It does not yet expose
issuance, import or approval CLI commands, and matching a grant is not authorization.
Trusted control ingress, durable storage, revocation and atomic claim-time consumption
are still required. Canonical launch dispatch remains disabled.

A launch approval scope binds the exact project store, task and post-reservation
task revision, runtime target and a digest of the intended action. The action digest
covers all version-2 launch inputs: control/policy revisions, config, frozen profile,
binary/version/capability evidence, repository vectors and other input references.
Only the grant reference itself is excluded, avoiding a circular hash. Final attempt
and operation identities still include that reference. The matcher separately checks
the exact final grant reference and the actual store identity supplied by the caller.

Action encoding is versioned JSON, with recursively sorted object keys and preserved
array order. Values are not normalized or silently dropped. Grants additionally bind
a versioned policy reference and a half-open validity interval: issuance is inclusive,
expiration exclusive. Content changes, another project, changed revisions, expired
grants or substituted references fail matching. Error messages withhold action data.

There is no deserializable actor credential. Unknown fields such as `actor=human`
and unsupported operation classes are rejected. An approval JSON document remains
untrusted data even if structurally valid. The future ingress must authenticate its
control route and evaluate policy; workers cannot supply their own trusted role.

Grant use must be consumed exactly once in the same transaction as claiming the
operation, after checking current policy and revocation. A no-effect retry needs an
explicit reuse policy; a pending or ambiguous operation is not proof of unused
approval. These are outstanding requirements, not guarantees of this initial data
contract. The same-OS-user shell bypass remains outside application-level authority.
