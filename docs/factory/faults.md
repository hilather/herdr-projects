# Factory fault campaign

Rehearsal of faults this design already implements. It is not a live
40-worker certificate, not a dollar ledger, and not a remote merge.
`factory_admission` defaults to `off`. Fresh stores keep `max_active_workers` at
0. `PREPARED_LAUNCH_DISPATCH_ENABLED` stays true. Correction fixtures use current
schema 43; this rehearsal does not upgrade real project stores. No
`recovery_epoch`. Effects are not replayed. `.state/state.db` is not
overwritten in place.

## Latency

`not frozen`. This is not a measured bar and not a lowered bar. It is not a
pass of the provisional targets: running-worker observation age 5 s p99,
other active bindings 15 s, and a targeted decision 250 ms p95. Those
targets were not claimed.

250 ms in `next_tick_delay` (`src/ticker.rs`) is a tick while canonical
root-exclusive work is pending. It is not a decision SLA. When that work is
not pending the cadence is `TICK` (15 s).

## Coverage

A stop without termination evidence does not free a slot. Incomplete
coverage is not success. A reconcile page that does not reach the end does
not release capacity.

`herdr-projects factory status PROJECT` prints counters. It does not launch
or admit. There is no `factory PROJECT status` command.

`counters.active_inventory_page_rows` measures only the first inventory page
read by that status request. It is not a measurement of controller or admission
work. `counters.rows_decoded` is currently `null`: complete decision-path row
instrumentation is still pending. Neither a small inventory count nor an unknown
value establishes that the factory scale gate passed.

Admission ticker logs now include `sql_work` from the connection used for that
decision, on both success and error. `sqlite_rows_returned` counts SQLite row
notifications (including scalar queries); `sqlite_vm_steps` counts instructions
in completed or reset statements, including interrupted statements. These are
not decoded-object counts, elapsed SQL time, or a memory measurement. Opening
checks after hook installation are included; filesystem/Git work and the earlier
controller enabled probe are excluded. `duration_ms` covers the observed admission
call. `connection_observed: false` means the call ended before the SQL observer
was attached; zero counts then do not establish zero database work. A log without
an observation has `sql_work: null`. No SQL text or row values enter these logs.

## Faults

| Fault | Rehearsal | Not a substitute |
| --- | --- | --- |
| Duplicate envelopes | One operation idempotency key, one result key, one verification key, and one proposal id. The same bytes replay. A conflicting payload is rejected. One verifier outcome inserts one feedback row. | Not a second accepted transition. |
| Crash windows | The claimed intent remains. Lease expiry marks that delivery ambiguous and does not claim it again. | `herdr-projects operations PROJECT expire` records ambiguity and does not replay. |
| Old attempt ids | The old attempt id is kept. It does not mint a replacement id, satisfy a dependency, or finish a claim bound to the previous task revision. | An infrastructure retry of that same attempt is not a new attempt. |
| Alias exclusion | One canonical git directory has one shared-guard owner. A symlink to that directory is the same resource. A project lock on another project does not take it. A symlinked `.state` is not a project. | Project locks alone are not enough. |
| Memory revocation | Revoking one barrier invalidates that release. Another barrier stays unrevoked. Attempts stay without `termination_observed`. | Revocation is not a slot release. |
| Authority mistakes | A contract file signed in the approval namespace is a denial. No contract row is installed. | `herdr-projects approval PROJECT import` checks the owner signature. The process does not sign. |
| SQLite busy | A write past the busy retry bound returns busy, leaves committed rows unchanged, and pauses admission. The pause does not delete attempts. | Not a rollback and not a launch. |

## Restore

Restore is a new root.

```sh
herdr-projects migration PROJECT restore --destination DIR
```

`DIR` must not already exist. The command copies hash-checked pre-cutover
backup bytes. It does not write `.state/state.db` and it does not replace
the live root.

`LaunchInputs.project_store` is the canonical absolute path of
`.state/state.db`. `ApprovalGrant::matches_launch` fails when
`actual_project_store` is not that path. A grant for the old root does not
match the new root and must be reissued. Do not replay a consumed grant.
Do not make the old grant succeed on the new path.

Reissue on the new root, on Linux with `state-store` compiled. `launch` is
compiled there. A non-empty `route.machine` is refused before launch
(`new launch requires an unused local binding`). macOS and live SSH stay
unsupported. `herdr-projects doctor` prints that. This binary does not omit
`launch` in order to refuse SSH.

```sh
herdr-projects launch PROJECT draft --selection FILE --expected-head N
ssh-keygen -Y sign -f /path/to/owner-key -n approval@herdr-projects grant.json
herdr-projects approval PROJECT import grant.json grant.json.sig --expected-head N
herdr-projects launch PROJECT reserve --selection FILE --approval-digest DIGEST --expected-head N
```

`herdr-projects migration PROJECT recover --writers-stopped` resumes the
journal. It does not resend an external effect. There is no `recovery_epoch`.

`herdr-projects repair PROJECT restore` replaces one inspected legacy record
after `--expected-hash`. It is not this restore, and it refuses
`.state/state.db`.

## Deleting an ownership marker is not a rollback

`.state/format.json` is the published ownership marker. Its runtime owner is
`sqlite-v2`. Deleting that file does not roll the store back, does not
remove `.state/state.db`, does not reissue grants, and does not authorize
legacy execution. `open_active` and
`herdr-projects migration PROJECT recover --writers-stopped` fail closed
while the marker is gone and the published database is still there.
`herdr-projects migration PROJECT abort` still refuses. The journal remains,
so legacy execution stays disabled. After publication, recover forward or
restore pre-cutover bytes into a separate directory.

See [factory operations](operations.md) and [platform](platform.md).
