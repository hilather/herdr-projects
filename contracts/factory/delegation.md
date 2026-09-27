# Delegation contract mapping and byte vector

This is one entry in the factory contract catalog mapping. Exact subject-signed
reservation is implemented; automated planner signing/dispatch and live F2
certification remain incomplete.

| Contract boundary | Current implementation |
| --- | --- |
| Owner-signed grant ingress | `authority::prepare_delegation`, namespace `delegation@herdr-projects`; raw-byte signature verification precedes parsing. |
| Trusted capability | `domain::PreparedDelegation`; callers cannot construct it from JSON. |
| Immutable storage | Schema 34 `delegation_grants`; `raw_bytes` and SHA-256 remain authoritative. Projection columns do not replace the signed document. |
| Version 1 | Historical repository/ref and profile-kind scope; remains non-executing. It has no exact task/profile/budget/lifetime-attempt bounds. |
| Version 2 | Adds required `reservation_scope`, validated and retained with the signed bytes. Currently supports `reserve_attempt` only. |
| Reservation/claim | `PreparedDelegatedReservation` authenticates the subject's exact request. `store/delegated_reservation.rs` derives an action-specific approval inside ordinary reservation, atomically retains quota usage, and rechecks delegation at claim/use. Neither raw grant version itself satisfies `ApprovalGrant::matches_launch`. |
| Revocation | Existing revocation records refuse subsequent scope checks and retain a stop obligation. They do not claim to undo a started provider effect. |

Version-2 `reservation_scope` requires:

- `task_contracts`: 1–128 exact `{id, revision, digest}` references, sorted by task
  ID with one reference per task. A different contract needs a new signed scope.
- `profiles`: 1–8 exact references, sorted by ID with no duplicates.
- `budget`: an exact budget-policy reference; no implicit unrestricted budget.
- `repository_bases`: one exact repository/ref/commit/object-format tuple for
  every signed repository/ref scope, sorted by repository then ref. SHA-1 and
  SHA-256 Git OIDs retain their distinct formats. This does not authorize ref
  publication or repository integration.
- `max_total_attempts`: 1–1,024, at least `max_concurrent_attempts`. Confirmed
  termination frees concurrent capacity without refunding this lifetime count.
  Cancellation or lease expiry without termination evidence retains capacity.

Reference revisions are positive SQLite-range integers; content digests are
lowercase SHA-256. Unknown fields, duplicate/unsorted exact references, missing
bounds, inconsistent versions and format-mismatched OIDs are rejected. Existing
expiry, policy, issuer/subject, revocation and no-redelegation rules remain.

`delegation-v2.json` is a synthetic, non-executable raw-byte vector. Its SHA-256 is
in `delegation-v2.sha256`. The final newline is included. It contains fixture
paths, digests and a placeholder subject key; it has no owner signature and is
not usable authority. The example uses compact sorted JSON object keys; the
signature and stored ID always cover original bytes, not reserialized output.
Changing only whitespace changes the signed identity.

The domain tests load these exact files. Store tests verify round-trip retention
and refusal when a prepared capability's bounds differ from its raw bytes. The
authority tests use real disposable Ed25519 keys to reject limit changes under
the original signature:

```sh
cargo test --locked --features state-store --lib delegation -- --test-threads=1
```

## Exact delegated reservation

```sh
herdr-projects delegation PROJECT import owner-grant.json owner-grant.json.sig
herdr-projects delegation PROJECT draft GRANT_ID --idempotency-key request-1 > request.json
ssh-keygen -Y sign -f SUBJECT_KEY -n delegated-reservation@herdr-projects request.json
herdr-projects delegation PROJECT reserve request.json request.json.sig
```

The owner signs the grant under `delegation@herdr-projects`. The subject uses a
different namespace and its pinned key for the request. A grant ID, claimed actor,
owner signature on the subject request, or signature under the wrong namespace
cannot replace subject authentication. All signatures cover exact original bytes.

Drafting selects the next ready candidate and refuses it if outside the grant's
scope; it does not search for a different candidate or install authority. The
version-1 request envelope contains `grant_id`, `subject`, `store_incarnation`,
`idempotency_key`, `expected_head`, `issued_unix_ms`, and exact `inputs`. Its
`schema_version` is independent of the grant's version. The draft's placeholder
approval is replaced by an exact derived approval; all other launch inputs stay
bound. The request is bounded to 65,536 bytes and rejects unknown/duplicate fields.

Schema 43's immutable `delegated_reservations` ledger commits with the derived
approval, attempt, task update and launch operation. A failed reservation rolls
all of them back. Same grant/key and byte-identical request returns the retained
response, including after termination or revocation; changed bytes conflict.
Replaying a response never repeats a launch or refunds quota. An independently
installed approval with the same derived identity is refused rather than adopted
into delegated accounting.

Reservation uses the ordinary signed-contract, dependency, knowledge, profile,
budget, resource and capacity checks. Before launch claim and use, derived
authority also rechecks grant expiry/revocation, scope and request incarnation.
Current owner configuration is checked before accepting a new subject request.
No delegation authorizes integration/ref publication or redelegation.
Turning automatic admission off does not waive retained resource claims: explicit
owner/delegated reservations and drafts still reject overlap. Candidate
preparation also skips resource-blocked tasks while that switch is off.

To stop subsequent delegated actions:

```sh
herdr-projects delegation PROJECT revoke GRANT_ID --expected-head HEAD --reason 'stop campaign'
```

Revocation does not prove a started worker stopped. The legacy `reserve_attempt`
store method remains a compatibility scope probe; production reservation goes
through the subject-signed `reserve_delegated` service. The draft/CLI workflow is
local and explicit; no automated planner key handling, live campaign or restore
certification is claimed. Canonical recovery must still demonstrate incarnation
rotation before old external identities can be safely reused.

The [signed CLI test](../../tests/delegated_reservation.rs) creates disposable
owner/subject keys and a migrated store, then uses the built executable for
contract/budget/grant imports, drafting, signature refusal, scope/quota refusal,
cancellation, revocation and immutable replay. Its native profile is an explicit
synthetic fixture with `certified: false`; it does not launch a worker or replace
native adapter/live acceptance:

```sh
cargo test --locked --features state-store --test delegated_reservation -- --test-threads=1
```
