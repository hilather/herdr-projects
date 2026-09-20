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

## Bounded live-copy staging protocol

`artifact-stream --probe` retains its existing `schema: 1` preservation capability
and additionally advertises `live_versions: [1]`. `artifact-stream --live --path PATH`
emits the separate `HPLV` version-1 format: eight-byte magic, a big-endian u32 JSON
length, a manifest, then concatenated included-file bytes. The manifest is capped at
4 MiB. Report and library each have a separate 50 MiB cap; the wire cap is 104 MiB
plus 12 bytes. Library traversal is bounded to 10,000 entries and 64 directory levels.

Live manifests distinguish included bytes from omissions. Links, hard links and
special files are omitted. If the library exceeds byte, entry, depth, omission or
path-representation limits, the entire library is omitted; a valid report still
transfers. Omissions are limited to 128 paths and 16 KiB of path text, with at most
3,000 bytes per omission path. Notes state that old home content may be retained;
the eventual projection is additive. Permission failures, changing sources and
read deadlines are errors, not evidence that an artifact is absent.

The receiver validates schema, paths, sizes, hashes, complete payload framing and
absence of trailing bytes. Private staging lives under `.state/live-copies`, is
removed on failure/drop, and cannot be used as a preservation snapshot. Cooperative
filesystem deadlines do not interrupt blocked native syscalls. Source reads retain
the physical-path/ancestor-symlink restriction documented in operations.

This protocol and receiver are prerequisites: the ticker does not yet use them.
The upcoming transfer adapter must require successful supervised sender completion,
revalidate execution/config/routing before publication, and commit the copy receipt
only after successful projection. Receiving a valid stream alone grants no such
authority. Unsupported live helpers must refuse without an unbounded transfer fallback.

The private publication API now retains the exact verified stage and a manifest
before saving a per-thread projection intent. Stage and ancestor directories are
flushed first. Publication writes included library files atomically, then the report,
verifies the home bytes, and commits the copy receipt while clearing the intent.
The whole library/report update is not atomic; the intent makes interruptions
recoverable from the same staged bytes, even if the source disappears or changes.
Missing/corrupt staging or changed execution/config/routing authority refuses recovery.

Omitted and unrelated home files remain untouched. Destination traversal uses open
directory descriptors; symlink/special destinations refuse. Temporary files have
exclusive creation and names distinct from the destination. Retained staging is
retired only after the receipt commit is durable. A crash before retirement can leave
identifiable orphan staging. Admission caps temporary/retained stage entries at 16
per project; existing-intent recovery remains possible at that limit.

Pending projections block conflicting copies, review preparation, execution/lifecycle
replacement, deletion and migration. This is still an internal publication/recovery
API: the ticker's supervised sender and executor admission are not wired yet.

## Saved-machine JSON bridge prerequisite

`remote_api` freezes a validated saved profile ID, literal SSH target and session.
The supervised `remote-api-bridge` transport uses strict host verification and no
TTY, bounded stdin JSON and output, exact response IDs, cancellation and the
caller's original deadline. It connects an existing server only; it neither
bootstraps nor retries. Labels and selected-profile UI state are not destination
authority. Matching disabled or ambiguous profiles refuse.

The contract follows [Herdr's saved-machine CLI](https://github.com/herdrdev/herdr/blob/d59d0603d53bb88c5320ea508a4fb9858b61af68/src/cli/machine.rs)
and [remote bridge implementation](https://github.com/herdrdev/herdr/blob/d59d0603d53bb88c5320ea508a4fb9858b61af68/src/remote.rs).
Installed 0.9.1 was verified with a disposable named-session JSON ping. Real SSH
acceptance remains untested. This transport alone grants no sending authority:
remote brief admission and ownership/claim integration are still pending.
