# Admission budgets

Schema 14 stores immutable, owner-signed budget policy revisions. This is the first
T04.4 increment: durable admission limits, not provider billing enforcement or
running-worker interruption. Canonical launch dispatch remains disabled.

`budget PROJECT inspect` reports the current policy, lifetime admitted-attempt count,
explicitly unknown provider tokens, incomplete-budget status and admission blockers.
The scheduler also exposes budget blockers. No policy means no additional budget
restriction; upgrading never invents owner approval or usage measurements.

`max_attempts` limits the project's lifetime attempt count. Imported, cancelled,
lost, completed and unterminated attempts all count. Cancellation does not refund an
admission. A zero limit blocks new attempts. This differs from concurrent capacity
and per-task retry limits; all applicable limits must pass. Reservation checks run
in the attempt-creation transaction. An already-reserved attempt can claim at the
count limit, while another admission is blocked. Lowering a policy never releases
retained capacity.

Provider tokens currently have no trusted collector and stay `unknown`, even with
no recorded attempts. A positive `max_provider_tokens` with `unknown_usage: "refuse"`
blocks admission. `"allow_incomplete"` permits admission with the explicit incomplete
flag. A zero token threshold blocks under either policy. Missing usage is never
measured zero; no dollar ceiling is claimed. Native usage, versioned estimates,
wall-time actions, durable routines and broader telemetry remain T04.4 work.

Policies use the pinned owner key described in [authority](authority.md), with a
separate `budget@herdr-projects` SSH signature namespace. Example document (replace
the path and authority reference using `approval PROJECT policy`):

```json
{
  "version": 1,
  "project_store": "/absolute/project/.state/state.db",
  "revision": 1,
  "authority": {"id": "owner-approval-policy", "revision": 1, "digest": "OWNER_POLICY_SHA256"},
  "limits": {
    "max_attempts": 20,
    "max_provider_tokens": null,
    "unknown_usage": "refuse"
  }
}
```

```sh
ssh-keygen -Y sign -f /path/to/owner-key -n budget@herdr-projects budget.json
herdr-projects budget demo import budget.json budget.json.sig --expected-head H
herdr-projects budget demo inspect
```

Import requires the current head, acknowledged owner config and next sequential
policy revision, beginning with 1. Replay, cross-project, changed-document and
wrong-namespace requests cannot change history. Policies do not expire automatically;
replace them with a signed revision to change limits. Both limits `null` explicitly
removes them without deleting history. New reservations pin the current policy in
immutable launch inputs; claim and pre-effect checks revalidate it. Policy changes
invalidate older launch authorizations without releasing capacity or preservation
obligations. Same-OS-user filesystem bypass remains outside the threat boundary.
