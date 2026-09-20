# Operation-scoped authority (T04.5 local close)

Schema 13 stores immutable grants, revocations and one-time uses. Grant installation
requires a sealed internal capability produced by owner-signature verification.
`approval PROJECT policy/inspect/import/revoke` provides the owner-control route.
Unsigned JSON and caller-supplied roles grant nothing. Production profile preparation
and canonical launch dispatch remain disabled.

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
untrusted data even if structurally valid. Signed import verifies exact document bytes
against the pinned owner key; workers cannot supply their own trusted role or key.

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
application-level authority. Isolated-service OS boundary remains documented;
this is not an OS isolation guarantee.

## Signed owner-control import

The owner config path recorded at migration supplies this policy:

```toml
[authority]
version = 1
revision = 1
approval_public_key = "ssh-ed25519 BASE64_PUBLIC_KEY"
```

The key must be one Ed25519 public key without comments or options. Configuration
must be outside the project, owned by the current OS user and not group/world
writable. Symlink files refuse. Legacy migrations without a pinned config path cannot
use this route. Caller `HOME`, grant fields and CLI arguments cannot replace that
pinned key source. Project control must acknowledge the current config fingerprint;
edits withdraw old launch authority immediately, even before control is updated.

`approval PROJECT policy` prints the policy reference a grant must name. The grant
contains the exact reviewed action scope and validity interval described above;
automatic grant drafting awaits production profile preparation. Sign its exact bytes
using an owner key held outside worker execution:

```sh
ssh-keygen -Y sign -f /path/to/owner-key -n approval@herdr-projects grant.json
herdr-projects approval demo import grant.json grant.json.sig --expected-head H
herdr-projects approval demo inspect
herdr-projects approval demo revoke APPROVAL_ID --expected-head H --reason "withdrawn"
```

Import uses `/usr/bin/ssh-keygen` with the fixed `owner` principal and
`approval@herdr-projects` namespace. The existing bounded command runner enforces a
five-second timeout, process-group cleanup and 4 KiB capture limits. Verification
files are copied into a private temporary directory and removed afterward; raw
verifier errors are withheld. Document/signature bounds are 64 KiB/8 KiB. Wrong keys,
namespaces, changed bytes and invalid signatures cannot install grants. The app
never opens private keys or invokes signing. Tests use disposable keys only.

This route authenticates possession of the configured signing key; it does not
certify agent capabilities or create a launch preparation. Keep that private key
outside worker access. A hostile process sharing the owner's OS identity may bypass
application controls by changing files or using accessible keys; this is not an OS
isolation guarantee.

Budget policies now use the same pinned owner key with a separate
`budget@herdr-projects` namespace and sequential project-bound revisions. See
[admission budgets](budgets.md).

Routine revisions now use `routine@herdr-projects` signatures with explicit config,
script-byte and project bindings. See [durable routine scheduling](routines.md).
Permission lookup hashes the exact parsed config bytes, closing replacement races
between surrounding path fingerprint checks.

Memory policy revisions use `memory@herdr-projects` and schema 17 `memory_policies`.
`memory PROJECT import document sig --expected-head H` authenticates and stores
sequential documents (`hard_rule`, `import_ack`, `revoke_head`, `cutover`). This
slice does **not** apply those ops to memory rows or `format.memory`; that is
T05.1/T05.3. Snapshot creation does not consume a memory policy or an
`ApprovalGrant`. `ApprovalClass` remains launch-only.

Refused mutating `approval` / `budget` / `routine-store` / `memory` commands write
`authority_denials` with `actor_channel` `cli-owner` (or `unknown-rejected`). Request
bodies are never stored. `approval PROJECT denials` lists them. An unopened store
cannot record a row. Production launch dispatch remains disabled.
