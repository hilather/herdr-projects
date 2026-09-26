# Factory platform boundary

Linux local execution is the only factory target. macOS and live SSH are
unsupported and do not run canonical launch. Doctor prints that boundary.
It does not launch. See [factory operations](operations.md) for restore.

## What is compiled

`Command::Launch` exists only under this cfg in `src/cli.rs`:

```rust
#[cfg(all(feature="state-store", target_os="linux"))]
Launch { slug:String, #[command(subcommand)] command:LaunchCommand },
```

`LaunchCommand` has the same cfg. Its subcommands are `draft` and `reserve`.
On any other target the variant is not compiled, so there is no canonical
launch command. There is no `cfg(target_os = "macos")` factory path.

On a non-Linux build, `dispatch_prepared` in `src/canonical_controller.rs`
does not bail on every call. With an effect queue it calls `offer_next`,
which queues the offer (`offer_canonical_launch` for a deliver-mode
`runtime.launch`, otherwise `offer_canonical_brief`). The queued deliver
step fails later: `canonical_brief_jobs` bails with `canonical launch
requires Linux pidfs` when `launch_advance` is set. Without a queue, the
direct `runtime.launch` arm bails with `canonical resource recovery requires
Linux pidfs`, and `runtime.worker_termination` bails with `canonical
termination requires Linux pidfs`.

## Doctor labels

`herdr-projects doctor` prints the compile target and the two refused routes.
The label function takes the OS string and a live-SSH flag so a Linux test
can assert `unsupported` without a macOS build:

| Input | Printed label |
| --- | --- |
| OS `linux`, not live SSH | `linux` |
| OS `macos` | `unsupported` |
| any OS with live SSH | `unsupported` |
| any other OS | `unsupported` |

A Linux binary prints:

```text
platform: linux
macOS: unsupported; canonical launch does not run
live SSH: unsupported; canonical launch does not run
factory-path: linux local only
```

Any other binary prints `platform: unsupported` and
`factory-path: unsupported; canonical launch does not run`.

`herdr-projects factory status PROJECT` also prints a `platform` field, from
the compile target only (`linux` or `unsupported` in `src/factory_status.rs`).
That field does not make a live SSH route supported. A `user_version` of 0
is the error `unsupported_schema`. That is a store version, not this platform
label.

## Live SSH is not a local binding

Canonical launch requires an unused local binding. `launch_preparation::inputs`
refuses unless `route.machine`, `route.tab_id`, and `route.pane_id` are empty
and `binding.identity.worktree_path` is empty (`new launch requires an unused
local binding`). `seal_admission_inputs` returns the same error when
`binding.identity.machine` is non-empty. `RuntimeRoute.machine` is the saved
machine id. A non-empty machine is the live SSH route.

The same local-only check is on the effect path. Launch creation in
`src/store/launch.rs` conflicts when `intent.route.machine` or
`target.route.machine` is non-empty. The native brief adapter refuses unless
`start.route.machine` is empty (`native brief adapter supports local workers
only`).

## Grants match one store path

`ApprovalGrant::matches_launch` in `src/domain/approval.rs` is necessary and
not sufficient. A later claim must still check current policy and revocation
and consume the grant once. The project path comes from the store, not from
worker input. The predicate returns `approval is stale or bound to a different
action` unless every check below passes:

- `validate()` succeeds (`scope.project_store` is a non-empty absolute path)
- `issued_unix_ms <= now < expires_unix_ms`
- `actual_project_store == scope.project_store`
- `scope == ApprovalScope::for_launch(inputs)`
- `reference() == inputs.approval`

`ApprovalScope::for_launch` copies `inputs.project_store` into the scope and
hashes the launch inputs with the approval reference removed. Draft sets
`project_store` from the canonical absolute path of `.state/state.db`
(`proof.store_path()`). A grant whose scope names a different path does not
match, including after the database file is moved. Content identity is not a
signature. `PreparedDelegation::matches_launch` does not bind a launch.
