# Factory operations

Linux local is the only factory target. macOS and live SSH print `unsupported`
and do not run canonical launch. The label rules and
`ApprovalGrant::matches_launch` are in [platform](platform.md). This file is
the restore and incident runbook. It does not add a `recovery_epoch`.

Names below are the real `herdr-projects` subcommands. There is no `rollback`
command, no `doctor --platform`, and no in-place restore of `.state/state.db`.

## Restore

Restore is a new root.

`herdr-projects migration PROJECT restore --destination DIR` calls
`restore_backup`. `DIR` must not already exist. The command copies
hash-checked pre-cutover backup bytes into that new directory and prints
`Reconcile before running workers.` It does not replace the live project root
and it does not write or overwrite `.state/state.db`.

A published store is recovered forward, not replayed:

```sh
herdr-projects migration PROJECT recover --writers-stopped
```

That resumes the migration journal. It does not resend an external effect.
`herdr-projects operations PROJECT expire` marks expired claims ambiguous and
does not replay them either.

Do not copy `state.db` onto the live file. Restore does not overwrite
`.state/state.db` in place. `herdr-projects repair PROJECT restore` is a
different command: it replaces one inspected legacy record (thread TOML,
`.state/project.json`, `.state/ticker.json`, or inbox Markdown) after
`--expected-hash`, with the ticker stopped. Any other path, including
`.state/state.db`, is refused.

### Grants do not follow a new path

`LaunchInputs.project_store` is the canonical absolute path of
`.state/state.db`. `ApprovalScope::for_launch` stores that string and hashes
the inputs. `ApprovalGrant::matches_launch` fails when
`actual_project_store != scope.project_store`. A new root is a different
path, so grants from the old root do not match and must be reissued. Do not
replay a consumed grant or a launch that already ran.

Reissue on the new root, on Linux with `state-store` compiled. `launch` is
absent on macOS and on a build without `state-store`.

`herdr-projects launch PROJECT draft --selection FILE --expected-head N` prints
a `LaunchDraft` (`head`, `inputs`, `approval`, `brief`, `worktrees`) and
retains no database rows. The signable document is the `approval` object, an
`ApprovalGrant`, not the wrapper. Sign those exact grant bytes:

```sh
ssh-keygen -Y sign -f /path/to/owner-key -n approval@herdr-projects grant.json
herdr-projects approval PROJECT import grant.json grant.json.sig --expected-head N
herdr-projects launch PROJECT reserve --selection FILE --approval-digest DIGEST --expected-head N
```

The binary never signs. Import parses the file as an `ApprovalGrant` and checks
the owner signature in the `approval@herdr-projects` namespace. Signing the
draft wrapper fails closed (`invalid approval document`). `reserve` calls
`matches_launch` against the installed, unrevoked, unconsumed grant and sends
no agent input.

There is no `recovery_epoch`. Do not add one. `control_epoch` on
`LaunchInputs` is the project-control epoch. It is inside the hashed action,
so changing it invalidates the grant. It is not a recovery generation.

### Deleting an ownership marker is not a rollback

`.state/format.json` is the published ownership marker. Its runtime owner is
`sqlite-v2`. Deleting that file does not roll the store back, does not reissue
grants, and does not authorize legacy execution.

`open_published` requires the marker to match the active journal. `ensure_legacy`
still refuses while any of `.state/format.json`,
`.state/migration/journal.json`, or `.state/migration/memory-journal.json`
exists. Removing the marker and pointing an older binary at the root is not
recovery. After publication, recover forward or restore pre-cutover bytes into
a separate directory.

## Incidents

| Incident | Command | Not a substitute |
| --- | --- | --- |
| Which target is this binary? | `herdr-projects doctor` | No `doctor --platform`. Read `platform`, `macOS`, `live SSH`, and `factory-path`. Doctor does not migrate or launch. |
| Counters, admission, schema | `herdr-projects factory PROJECT status` | Does not launch, admit, or print environment values. `unsupported_schema` is `user_version` 0, not the platform label. |
| Queue without preparing a launch | `herdr-projects scheduler PROJECT inspect` | Read-only. It does not reserve. |
| Unsigned grant for this store | `herdr-projects launch PROJECT draft --selection FILE --expected-head N` | Linux and `state-store` only. Does not reserve or start an agent. |
| Reserve an installed grant | `herdr-projects launch PROJECT reserve --selection FILE --approval-digest DIGEST --expected-head N` | Sends no agent input. A grant for another `project_store` does not match. |
| Policy a grant must name | `herdr-projects approval PROJECT policy` | A matching hash is not a signature. |
| Installed grants | `herdr-projects approval PROJECT inspect` | Does not reissue or replay. |
| Install a reissued grant | `herdr-projects approval PROJECT import DOCUMENT SIGNATURE --expected-head N` | The process does not sign. Sign with `ssh-keygen -Y sign -n approval@herdr-projects`. |
| Withdraw a grant | `herdr-projects approval PROJECT revoke ID --expected-head N --reason TEXT` | Revoke is not a store rollback. |
| Journal phase | `herdr-projects migration PROJECT status` | Does not recover and does not delete the marker. |
| Published store needs forward recovery | `herdr-projects migration PROJECT recover --writers-stopped` | Not a replay. Not an in-place `state.db` overwrite. |
| Pre-cutover bytes | `herdr-projects migration PROJECT restore --destination DIR` | `DIR` must be new. Does not overwrite the live root or `.state/state.db`. |
| Explicit schema upgrade | `herdr-projects migration PROJECT upgrade-store` | Does not migrate implicitly and does not enable launch. |
| After restore, before workers | `herdr-projects reconcile PROJECT` | `--plan` is a dry plan. `--record` persists evidence and does not dispatch. Neither reissues grants. |
| Ownership and bindings | `herdr-projects runtime PROJECT inspect` | Does not grant ownership. |
| Drop an ownership claim | `herdr-projects runtime PROJECT relinquish ID --expected-revision N --expected-head N --reason TEXT` | Does not stop or delete the resource. Deleting `.state/format.json` is not this command and is not a rollback. |
| Damaged legacy record | `herdr-projects repair PROJECT inspect`, then `herdr-projects repair PROJECT restore RELATIVE --from FILE --expected-hash SHA` | Stop the ticker first (`herdr-projects ticker stop`). Not `state.db`. |
| Ambiguous delivery | `herdr-projects operations PROJECT inspect` | `herdr-projects operations PROJECT expire` records ambiguity and does not replay. |

macOS and live SSH stay on the doctor lines above. Do not point canonical
launch at either one. `launch` is not compiled for those targets, and a
non-empty `route.machine` is refused as not an unused local binding.
