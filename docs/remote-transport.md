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

Ordinary file paths retain the existing scp path. Paths requiring quoting use
`ssh cat` with raw-byte capture; output above the runner's 1 MiB limit is refused
before publishing that file. That fallback is not yet a streaming large-file
transport. Non-UTF-8 payload bytes are supported; paths enter the public API as
UTF-8 strings. Remote snapshots and total transfer-byte enforcement during source
mutation remain unfinished. Live library transfers are compatibility copies,
not independently verified remote preservation receipts.

Unsupported capabilities produce a clear error but remain subject to the existing
bounded finalization retry schedule; a permanent blocked-operation state is not
yet implemented. No live host or agent certification is implied by these fixtures.
