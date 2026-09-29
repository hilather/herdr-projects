# Collection contracts (lane A)

Owned by lane A ([phase2-lanes.md](phase2-lanes.md)). Extends
[contracts.md](contracts.md) §5; §0 rules apply unchanged.

## A1 (TM1.1): canonical collector bindings

**Canonical table** `collector_bindings` (migration 0052): append-only
revisions `(attempt_id, revision)` with `state` ∈ `active`, `revoked`,
`predates_binding`, `collector` (the launched profile kind), the profile's
`execution_home`, `unix_ms` (controller `now`) and `source`. Updates and
deletes abort. Analytics only: never read to grant launch.

- Revision `active` is written in the `apply_launch_started` transaction from
  the attempt's retained effective profile (no `LaunchInputs` change).
- 0052 gives every attempt that exists at migration one `predates_binding`
  revision (`source = migration_0052`).
- `herdr-projects telemetry <slug> collectors revoke <attempt>` appends a
  `revoked` revision (copying collector and home) and prints
  `{binding, written}`. Revoking a revoked binding writes nothing
  (`written: false`); an attempt without a binding or with `predates_binding`
  is refused.
- `telemetry <slug> collectors bindings` (read-only) prints every revision and,
  per rollout source, `{session_id, binding, attempt_id, basis}`.

**Codex binder.** Contracts §5 rules 2 (cwd under the attempt's worktree) and
3 (`session_meta.timestamp ≥ decided_unix_ms`) select candidate attempts; the
latest binding revision then decides:

| Latest revision | Rule 1 home | Bound when | `basis` |
|---|---|---|---|
| `active` | binding's `execution_home` | always | `collector_binding` |
| `revoked` at *r* | binding's `execution_home` | session timestamp < *r* | `collector_binding`, else `binding_revoked` (unbound) |
| `predates_binding` (or a store before 0052) | retained inputs | rules 1–4 | `predates_binding` |
| none (never launched) | — | never | `no_binding` (unbound) |

No candidate → `no_match`; several → `ambiguous` (rule 4). A revocation never
erases accepted usage: rollouts that started before it stay bound with all
their records; rollouts that start at or after it are not bound. The binding
is per rollout, so records appended later to an earlier bound rollout still
count. `basis` is kept in the sidecar stream `ingest` (migration
`ingest/0001_source_bindings.sql`, table `source_bindings`), recomputed with
`rollout_sources.binding` on every collect.
