# Operation-scoped authority (partial T04.5)

Schema 13 stores immutable grants, revocations and one-time uses. Grant installation
requires a sealed internal capability; there is no issuance/import CLI or raw-JSON
approval route. Trusted control ingress and policy resolution are still required.
Canonical launch dispatch remains disabled.

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

Launch claims consume a grant exactly once in the claim transaction. The pre-effect
fence rechecks the matching consumption, expiry, revocation, control/config/scheduler
revisions, runtime binding and the exact still-reserved attempt with retained capacity.
Revocation does not claim to stop an effect already in progress, and does not release
capacity. An outcome may still record what happened after revocation.

A no-effect retry cannot reuse the consumed grant; the operation remains blocked
pending an explicit recovery policy. Pending or ambiguous state is not proof of
unused approval. Other operation kinds retain their existing delivery contracts.
Grant records and use history appear in canonical snapshots and schema-13 runtime
projections. Corrupt/mismatched approval history refuses inspection or launch use.

Upgrade from schema 12 preserves existing input, delivery and event records. It
does not invent grants for pending or already-claimed launches: those claims and
pre-effect checks refuse while their attempts retain capacity. No actual user store
is upgraded automatically. The same-OS-user shell bypass remains outside
application-level authority. Trusted issuance, policy-change ingress, denial audit
and command-path coverage beyond launches remain outstanding T04.5 work.
