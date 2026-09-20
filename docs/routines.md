# Durable routine scheduling

Schema 16 adds signed routine revisions and durable scheduling decisions. The
`routine-store` commands require the `state-store` feature and a migrated project.
Explicit `execute` dispatch is available on Linux; automatic ticker dispatch is
pending. Legacy `routine` commands retain their behavior; their
approvals and last-run state are not automatically migrated.

The owner signs a complete definition using the migration-pinned key described in
[authority](authority.md), in the separate `routine@herdr-projects` namespace.
Enabled definitions also require `[safety."<canonical project path>"].routine_commands
= true` in the acknowledged owner config. Exact permission bytes are checked against
the signed config digest before parsing; actor fields cannot grant authority.

Example definition (replace paths, hashes and the start instant before signing):

```json
{
  "version": 1,
  "name": "check",
  "revision": 1,
  "project_store": "/absolute/project/.state/state.db",
  "authority": {"id": "owner-approval-policy", "revision": 1, "digest": "OWNER_POLICY_SHA256"},
  "config": {"path": "/absolute/owner/config.toml", "digest": "CONFIG_SHA256"},
  "enabled": true,
  "schedule": "every 5m",
  "timezone": "UTC",
  "start_unix_ms": 1790000000000,
  "missed": "coalesce_latest",
  "overlap": "skip",
  "script": "/absolute/project/check.sh",
  "script_sha256": "SCRIPT_SHA256",
  "cwd": "/absolute/project",
  "deadline_ms": 60000,
  "output_cap_bytes": 4000
}
```

Use the authority reference from `approval PROJECT policy`. Config and script hashes
cover exact file bytes. The working directory must resolve to this project.

```sh
ssh-keygen -Y sign -f /path/to/owner-key -n routine@herdr-projects routine.json
herdr-projects routine-store demo import routine.json routine.json.sig --expected-head H
herdr-projects routine-store demo inspect
herdr-projects routine-store demo schedule check --expected-head NEW_HEAD
herdr-projects routine-store demo execute OCCURRENCE_OPERATION --expected-head NEXT_HEAD
```

Import refuses replay, wrong project/namespace, stale head/config, invalid schedules
or timezones and changed script bytes. Revisions begin at 1 and advance by one per
name. A signed revision with `enabled: false` withdraws future scheduling without
discarding uncertain prior work.

Scheduling uses controller wall-clock time. One immediate transaction commits the
occurrence, cursor advancement, event and project-scoped outbox intent. Identity binds
project, routine name, approved revision and scheduled instant. Repeated scans and
restarts cannot enqueue the same occurrence twice; new revisions have new identities.

Intervals anchor to the approved start. Daily times use the named timezone and shared
gap/repeat rules. `coalesce_latest` records the missed window and enqueues its latest
instant. `skip` records the window without enqueueing when multiple slots are due.
Inspection exposes first/latest instants, slot count, decision and operation identity.
Calendar slots can share an instant after a skipped date. Historical instants remain
authoritative across timezone-data updates.

Overlap supports only `skip`. Pending or ever-claimed work blocks overlap until a
typed completion receipt proves cleanup for its exact claim epoch. Generic confirmation
or retirement does not certify cleanup. Replacing a revision retires only Pending intents with zero
lifetime claims, proving no supported adapter entered. Claimed/ambiguous history stays.
Claim/pre-effect checks revalidate the current signed revision, config and script;
project control still fences pause and reconciliation.

Bounds per project: 128 names, 10,000 immutable revisions and 100,000 decisions.
Reaching a bound refuses further recording without pruning history. Scripts are at
most 64 KiB without NUL (execution also requires UTF-8), deadlines 1–60,000 ms,
output caps 1–65,536 bytes per stream. Script
hashes pin those bytes only; external commands, interpreter, environment and referenced
files are not an exhaustively pinned dependency bundle.

## Explicit execution and cleanup

Execution retains root/project mutation ownership from claim through receipt commit.
This synchronous command can therefore hold up other guarded mutations for its bounded
duration; it is not called from the ticker scan. It runs the exact verified script
bytes through stdin, without reopening the script pathname after checking its hash.
Configuration, routine revision, project control and the claim are rechecked before
entry. A routine can be claimed only once, including after generic retry advice.

Linux execution requires system-owned `/usr/bin/unshare`, `/usr/bin/timeout`, and
`/bin/sh`, and permission to create user, PID and mount namespaces. Helpers must not
be group/world writable. The environment is cleared before the first exec and sets
only `PATH=/usr/bin:/bin`, `LANG=C` and `LC_ALL=C`; shells may add their own standard
variables. Caller credentials and shell startup environment are not inherited.

A fixed PID1 shell waits for GNU timeout and maps command success/failure to reserved
supervisor exits 0/200. Timeout kills the script at the signed deadline. Namespace init
exit then kills remaining descendants, including detached process groups. The outer
runner has a five-second bootstrap/cleanup allowance. Only a normally reaped wrapper
with those reserved exits certifies cleanup. Bootstrap errors, outer timeout,
cancellation, I/O failure or process death remain uncertain. Exit 200 means failed
execution; it does not distinguish deadline expiry from an ordinary script error.
These contracts follow [util-linux's wait behavior](https://raw.githubusercontent.com/util-linux/util-linux/v2.42.3/sys-utils/unshare.c)
and [Linux PID namespace teardown](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html).

The receipt, delivery outcome and an output inbox item commit in one transaction.
Schema 16's existing event log stores a digest-bound `routine.completed` event; no
schema migration is necessary. Inspection exposes typed receipts, including capped
raw output bytes, totals and truncation flags. Inbox text JSON-escapes terminal controls
and labels script output as untrusted data. Output never authorizes task completion.

Verified cleanup releases overlap for a later occurrence even if the script failed.
No cleanup receipt, a failed receipt commit, or an expired claim leaves overlap blocked.
Restart never replays that script. Process cleanup does not undo filesystem, network
or other external effects; this is not a security sandbox against a hostile same-user
script or host service. Non-Linux execution refuses before claiming.

Automatic ticker scheduling/dispatch and asynchronous ownership remain integration
work. The same-OS-user bypass limitation still applies.
