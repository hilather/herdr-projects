# ADR 0001: compatibility and runtime boundaries for Phase A

Status: adopted for the initial reliability implementation, 2026-09-19.
This records the Phase A portion of T00.2. It does not accept a database schema,
finish all T00.2 contracts, or authorize a migration/release.

## Authority and storage

Keep `PROJECT.md`, thread TOML, ticker JSON, task Markdown and memory Markdown
authoritative in the existing domains during Phase A. The current work changes
execution and polling behavior without migrating user data.

The later architecture has two explicit cutovers: W03 moves task/runtime/event
state to per-project SQLite; T05.3 separately moves memory authority. One domain
has one writer model at a time. SQLite must live on local storage, with short
transactions, expected revisions and durable operation intent. External commands
and semantic review execute outside transactions. Lost terminal responses are
ambiguous outcomes, not proof that nothing happened; recovery cannot promise
exactly-once agent execution.

Before W03, finish the typed task/attempt/event/result/gate contracts and select,
license-check and validate the SQLite integration against Rust 1.89. This slice
does not introduce an unused database dependency or a provisional schema.

## Execution contract

`Runner::run` is for finite subprocesses. Commands own a process group by default.
The existing detached ticker uses `Command::spawn` with redirected descriptors,
outside this contract. The deadline starts before spawn and covers child exit,
stdin delivery and both output streams. Unix pipes are nonblocking and handled
by one polling loop; no helper I/O threads can remain blocked after return.

On cancellation or deadline expiry, close stdin, signal the owned group with
TERM, allow 200 ms, signal KILL and reap the immediate child. Polling checks occur
at most 20 ms apart, excluding bounded per-iteration I/O and OS scheduling. An
escaped/unowned descendant is outside group termination, but closing its inherited
pipes never requires waiting for its EOF. No user-facing cancellation command or
durable executor queue is introduced yet; the shared token is the Rust API seam.

Each stream retains at most 1 MiB by default (`Cmd::capture_limit` can select a
different limit). Excess bytes are drained and counted but discarded. Raw byte
buffers, total drained byte counts, truncation flags, exit status, elapsed time,
timeout and cancellation are separate observations. Compatibility text views use
lossy UTF-8; they do not replace the raw bytes. Text views can occupy up to three
times the captured bytes when input contains invalid UTF-8. Total counters describe
bytes actually drained, not bytes a producer might emit after termination.

Truncation makes `Output::success()` false even if the child exits zero. Structured
Herdr replies are rejected before JSON parsing if truncated or cancelled; artifact
size checks reject incomplete or failed `du` output. Routine output explicitly
labels capture truncation. The direct socket request adapter is unchanged.

The only added direct dependency is `libc 0.2`, already locked at 0.2.189 through
the test dependency graph. Its cached manifest declares Rust 1.65 and
`MIT OR Apache-2.0`; it supports the existing Unix target boundary. Direct
`fcntl`, `poll` and `kill` calls avoid a shell helper enforcing its own deadline.
The lockfile changes only the root package dependency edge. Linux is tested;
macOS remains a required external acceptance check.

## Poll identity and schedule policy

Remote cadence, backoff and outage observation use a typed tuple of canonical
project directory, recorded session socket and machine label. One project cannot
consume another's eligibility or recovery event. The slow-pass starting project
rotates each tick. Existing cadence remains four ticks, with eight skipped ticks
after failure. This does not introduce parallel command execution or shared
host-wide quotas; bounded execution pools remain W04 work.

Intervals require positive ASCII digits followed by `m`, `h` or `d`. The converted
seconds must fit `i64` (maximum 9,223,372,036,854,775,807). Daily schedules use the
ticker machine's local timezone. A missing local time shifts forward by the gap;
a repeated time uses the first occurrence, independent of the current offset.
Missed occurrences retain the existing coalesced behavior.

## Verification boundary

Phase A recovery uses additive, defaulted fields in ticker JSON and thread TOML.
Merged finalization has its own operation identity and bounded persisted retry;
summary deduplication does not stand in for copy completion. Lifecycle generation,
execution fingerprint and the report PR are checked around the external copy.
The receipt and resolved status share one locked thread update. Partial copies
retain the existing resolve policy with a durable warning. Idle auto-resolution
cannot bypass a pending merged finalization.

Notification delivery is reserved durably before the external request and marked
delivered only after confirmation. PR and outage events have stable IDs and a
persisted delivery obligation, saved with their comparison state. Inbox replay
recognizes both unhandled and handled copies. Machine outage history is persisted
per session/machine in each project, GitHub history per PR URL. Retry delays start
at 15 seconds with deterministic jitter and cap at 300 seconds; actual execution
waits for an eligible ticker pass. Lost delivery responses remain ambiguous, so
external notifications and prompts provide at-least-once retry behavior.

These records rely on the existing single-ticker writer and atomic file writes;
they are not a cross-file database transaction. Automatic corruption repair and retention-aware delivery tombstones remain later
work. Doctor surfaces unreadable/malformed ticker state, and ticker loading now
refuses to replace such state. Local final copies publish bounded verified
snapshots; remote snapshots and writer-exclusion checkpoints remain unfinished.
Worktree removal refuses until those preservation and writer gates can be met. No stronger crash or power-loss guarantee is claimed.

Tests prove the implemented subprocess, parser and polling invariants. Agent idle
state or a report does not prove task correctness. Existing project capacity is
still advisory. New memory snapshots, promotion, acknowledgments, verification
gates and schema migration remain later work. Passing local tests does not certify
live SSH/GitHub, mixed agents, or authorize publishing or destructive cleanup.
