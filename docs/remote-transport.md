# Remote transport compatibility

Library copies require SSH with a POSIX remote shell and rsync on both hosts.
Before transfer, both `rsync --help` replies must confirm `--protect-args` or
`--secluded-args`. The command uses the compatible short spelling `-s` and
`--checksum`. This is the rsync 3.0+ protected-argument protocol; older rsync and
implementations without confirmed support are refused. See the
[official rsync manual](https://download.samba.org/pub/rsync/rsync.1#opt--secluded-args).

Protected arguments alone still permit rsync wildcard expansion in a source
argument. The remote server command therefore changes directory using a shell-
quoted literal path, while the source argument sent through the protocol is `./`.
Relative paths are prefixed with `./` before quoting so leading hyphens cannot
be interpreted as `cd` options. SSH targets are validated separately from paths.

| Environment | Evidence |
| --- | --- |
| Linux, local rsync 3.5.0, real shell and rsync sender/receiver | Automated loopback fixture passes spaces, Unicode, quotes, dollar signs, backticks, wildcard characters, leading hyphens and newlines. Binary bytes and empty directories survive; symlinks are skipped. |
| Installed Herdr 0.9.1 | `plugin pane open --help` confirms per-popup `--env` support. No live popup or remote session was opened for these tests. |
| Older rsync without protected arguments | Capability refusal before transfer. |
| rsync 3.0–3.4, macOS, real SSH endpoints | Protocol capability is probed, but these combinations have not been exercised in this workspace. |

All report paths use quoted SSH file streams into exclusive local staging files.
The runner writes directly to disk (50 MiB cap), terminates an overflowing producer,
and publishes only on success. Truncation, cancellation and failed transfers leave
the previous report intact. Binary data never passes through lossy UTF-8 capture.

Finalization requires a compatible native `herdr-projects artifact-stream` helper
on the remote host. Set `HERDR_PROJECTS_REMOTE_BIN` to its literal path if needed.
The helper can run without project configuration. A schema probe precedes transfer.
Missing/unsupported helpers durably block merged finalization; after installing
one, run `thread resolve <project> <thread>` to retry explicitly. Connection failures
retain bounded retry. Doctor exposes the blocked record's installation diagnostic.

Protocol v1: eight-byte `HPAR` magic/version, big-endian u32 JSON length, manifest,
then concatenated file payloads. The sender rescans the source before successful
exit. The receiver requires that successful exit, validates paths/counts/depth,
limits manifests to 4 MiB and aggregate file bytes to 50 MiB, and independently
checks staged file hashes before immutable publication. The stream file is capped
at 54 MiB plus 12 bytes; extracted staging adds at most 50 MiB temporarily. Empty
directories survive. Symlinks, hard links, special files, traversal, duplicates,
truncation and corrupt bytes fail closed. Final report/library projections derive
from the verified local snapshot. Old snapshots remain intact on failure.

The live library rsync copy remains a compatibility projection with preflight size
checks, not an immutable receipt or a hard bound against growth during transfer.
It cannot authorize cleanup. Remote worktree deletion remains refused because no
remote writer checkpoint adapter has been validated. Native sender/receiver tests,
a real CLI binary fixture and real double-shell path tests are automated; real SSH
endpoints and macOS still require separate acceptance.

Streaming sinks assume responsive local storage; regular-file writes and fsync
are synchronous OS I/O and are not preempted by the subprocess deadline.
