# Supervised workspace creation compatibility patch

`workspace-create-command.patch` applies to Herdr 0.9.1 source commit
`065ef9d6a531c49fb8bee7e818ef837065b21ee9`. It is a local compatibility
patch, not an upstream release or an installed runtime update.

The current `workspace.create` API always starts the configured default shell.
Canonical launch then creates a second pane for its supervised worker, leaving
an extra process whose descendants are outside that worker's PID namespace.
The patch adds `workspace.create_command`: a workspace whose first pane starts
literal argv directly through Herdr's existing argv terminal implementation.
A distinct method prevents an older server from silently ignoring a command
field and reporting success after starting its default shell.

The patch also adds optional `capabilities.workspace_create_command` to the
read-only `ping` response. Patched servers advertise `true`; absent fields
deserialize as false and false is omitted when serializing older fixtures. The
canonical adapter requires this advertisement and the expected server version
before consuming approval, including before repository worktree provisioning.

Request parameters:

```json
{"cwd":"/absolute/workdir","command":["/absolute/executable","literal argument"],"focus":false,"label":"optional","env":{}}
```

`cwd` and `command` are required. The executable must be absolute, argv must
contain 1–160 entries with at most 65,536 total string bytes, and cwd must be
absolute and at most 4,096 bytes. NULs and unknown fields are rejected. Existing
launch environment validation also applies. Invalid input and spawn failures
return errors without creating a workspace or falling back to a shell. Success
returns the existing `workspace_created` response. Existing workspace creation
retains its shell behavior; no frozen client codec is changed.

Apply in a separate checkout at the base commit:

```sh
git apply --check /absolute/herdr-projects/patches/herdr/workspace-create-command.patch
git apply /absolute/herdr-projects/patches/herdr/workspace-create-command.patch
cargo build --locked
cargo test --bin herdr workspace_command
cargo test --bin herdr workspace_create_command
```

Building this base requires Zig 0.16.0 and the locked Cargo and vendored Zig
dependencies. Set `ZIG` to the absolute Zig executable if needed. This session
built only in `/tmp/herdr-canonical-source`; it did not replace installed Herdr.
Upstream documentation, if this feature is adopted, belongs in `docs/next/`.

The project live contract uses an isolated home, configuration, and named server:

```sh
HP_LIVE_HERDR=/absolute/patched/herdr cargo test --features state-store --test live_phase_a live_workspace_command_starts_supervised_root_without_bootstrap_shell -- --ignored --nocapture
```

It first proves the configured default shell writes a marker, then verifies the
new method never runs that shell, including empty/relative/nonexistent command
failures. It checks no workspace survives failed creation, observes the exact
production supervisor and waiting gate, releases the gate, and verifies process
termination after workspace closure. It uses sleep, not a vendor agent or task
prompt. Native validation and schema tests also pass.

The canonical adapter now uses this API for new workspaces, with version 2
creation intents, atomic root/workspace ownership and observation-only lost-reply
recovery. Existing workspace launches still use `layout.apply`; historical
bootstrap receipts retain their recovery path. Stock 0.9.1 does not support this
method, and the adapter does not fall back to shell creation. Version alone
cannot certify capability. Server capability admission now refuses unsupported
endpoints before resource effects. Resource cleanup and complete vendor workflow
acceptance remain pending; dispatch is
disabled. Remove this patch only when an upstream method provides the same
verified literal first-pane contract and the adapter's capability evidence and
live tests have been updated for that runtime.

The ignored library test `live_canonical_supervised_root_creation_release_and_termination`
also exercises production adapter calls and SQLite ownership through the real
patched server, then releases and cancels a sleep worker. Run it with
`cargo test --features state-store --lib live_canonical_supervised_root_creation_release_and_termination -- --ignored --nocapture`
and `HP_LIVE_HERDR` set. Its profile/approval are test fixtures, not production
capability certificates. A stripped copy of the debug runtime was tested at
`/tmp/herdr-supervised-root-live` (SHA-256
`01e113274a3a1fe31109f9dba7634896054ee6714415245837d79f3604567d51`).
The full 255 MB debug binary exceeded the adapter's bounded verification deadline;
stripping removed debug data without changing code. The live fixture uses a
60-second worker wall budget; production claim and request deadlines are unchanged.


The capability-advertising build is `/tmp/herdr-capability-live`, SHA-256
`d92024479ef4eab25e4814e3d325705e8150cf20163eb4c663618a8c95100baa`.
The older `/tmp/herdr-supervised-root-live` build does not advertise this field
and is no longer accepted for fresh canonical workspace creation. Neither build
replaces the installed runtime.
