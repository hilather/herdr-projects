# Operations and development

How Herdr Projects works, what it writes where, what its safety settings do and don't stop, and how to run threads on other machines.

## How it works

- **The coordinator is an ordinary agent** in a Herdr pane that follows a skill (`herdr-projects skill` prints it). Plugin code does not route messages, plan work or decide anything.
- **The binary does mechanics.** Starting or restarting a thread, copying reports, marking inbox items handled: each is one deterministic subcommand. It talks to Herdr through Herdr's CLI. The one exception is `focus`/`unfocus`: Herdr 0.9.1 has no CLI command for `agent.view.set`, so those two send one JSON line to the project's socket.
- **Files are the record, prompts are nudges.** Threads write a report file, the ticker writes events to an inbox folder, and the coordinator re-reads state with `context` at the start of every turn. A missed prompt loses nothing.
- **One ticker per projects root** checks every 15 seconds: thread state and groups, pending prompts, changed reports, pull requests (every two minutes), routines, auto-resolve. Remote machines are polled once a minute.
- **Tools are found even under a bare `PATH`.** A Herdr server started outside a login shell gives its plugins a minimal `PATH`; the binary appends `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin` and `~/.cargo/bin` to its own, so the ticker finds `gh`, `rsync` and friends. `ticker status` and `doctor` show what resolved.
- **Nothing destructive is automatic.** The binary never removes a worktree, deletes a branch, merges or pushes on its own. Text from reports, pull requests and command output is never placed in a prompt.

## Where things live

```
~/.herdr-projects/<project>/
  PROJECT.md              settings (TOML between +++ lines) and your standing instructions; yours
  MEMORY.md, memory/      project memory; the coordinator's
  TASKS.md                the task list; the coordinator's
  routines/<name>.md      routines; the coordinator's
  scratch/                the coordinator's temporary files
  threads/<id>.toml       thread record          threads/<id>.md   home copy of its report
  threads/<id>.task.md    the task as given      threads/<id>/     working folder of a tab thread
  inbox/, inbox/done/     events for the coordinator
  library/<id>/           home copy of files a thread produced
  .state/                 status, coordinator pane, ticker state, lock
~/.herdr-projects/.ticker.lock  .ticker.log  .trash/
~/.config/herdr-projects/config.toml             yours, edited by hand
~/.config/herdr-projects/approved-routines.json  written only by `routine approve`
```

Every thread works from `<its working directory>/.herdr-project/<project>-<id>/`: `brief.md` (written by the binary), `report.md` and `library/` (written by the agent). In a git repository that folder is added to `info/exclude`, so nothing in it is committed. **Git therefore treats it as clean: removing a worktree deletes it**, so a copy alone is insufficient to justify removal. `--remove-worktree --writers-stopped` can remove an exclusively owned local Linux worktree after verified preservation, managed-operation exclusion and process checks. Stop all known artifact writers before asserting `--writers-stopped`; idle alone is insufficient. Remote and unsupported-platform cleanup refuse. Plain resolve keeps the worktree and branch. `--discard-uncopied` cannot bypass writer or ownership checks.

`PROJECT.md` settings: `name`, `goal`, `repos` (`path`, optional `machine`), `coordinator_agent`, `thread_agent` (default `claude`), `max_parallel_threads` (3), `auto_resolve_days` (7), `nudge` (`false`).

## Commands

| Command | What it does |
| --- | --- |
| `new <name> [--goal] [--repo PATH[@MACHINE]]...` | Create a project folder. |
| `list [--all]` | Projects with status and thread counts by group. |
| `open <project> [--reprime] [--session N \| --socket P] [--rebind]` | Workspace, coordinator tab and coordinator agent; focuses it when it already runs. |
| `context <project> [--peek]` | The digest the coordinator reads every turn. `--peek` records nothing. |
| `inbox done <project> <item>... \| --all` | Mark inbox items handled. |
| `thread start <project> --title T [--repo PATH] [--machine M] [--agent KIND] [--base REF] --task-file F` | New thread; `-` reads the task from standard input. Returns before the agent is up. |
| `thread restart`, `thread prompt`, `thread adopt`, `thread list`, `thread show`, `thread ack`, `thread resolve` | See `--help` on each. |
| `overview [<project>] [--wait]`, `focus [<project>]`, `unfocus` | Threads grouped by what needs you, as text and in the sidebar. |
| `routine list`, `routine approve`, `safety show` | Routines and safety settings. |
| `pause`, `resume`, `archive`, `unarchive`, `delete [--force]` | Project lifecycle. `delete` moves the folder to `.trash/`. |
| `ticker start \| run \| stop \| status`, `doctor`, `skill` | Housekeeping. |

Groups, first match wins: Resolved; Working while starting; **Waiting on you** (failed, a launch stuck for 60 seconds, a pane gone with no report, or blocked for 30 seconds); **Working**; **Landing** (pull request open and approved); **Ready for review** (a report exists and either its pull request is open or you haven't acknowledged it); Idle. Threads idle for `auto_resolve_days` are resolved after a final copy home.

`focus` replaces any sidebar view another tool has set, and `unfocus` clears whatever view is set, because Herdr holds a single one. `focus` covers local threads only.

## Safety settings

Set per project in `~/.config/herdr-projects/config.toml`; `safety show <project>` prints the table header to use.

```toml
[safety."/Users/you/.herdr-projects/billing"]
start_threads = "propose"          # or "auto": the coordinator starts threads without asking
coordinator_agent_args = []        # arguments for the bound coordinator kind
coordinator_agent_args_kind = "claude" # required when the array is nonempty
thread_agent_args = []             # arguments for the bound worker kind
thread_agent_args_kind = "claude"   # required when the array is nonempty
routine_commands = false           # true lets approved routines run shell commands
```

Nonempty argument arrays now require their matching `*_agent_args_kind` field.
When upgrading an existing config, set that field to the kind the existing flags
were originally written for. Do not infer it from a newly changed default agent.
Unbound or mismatched arrays refuse launch; no flags are automatically translated
or discarded. Empty arrays remain usable across kinds. Unreadable, non-regular,
oversized or invalid config refuses safety-dependent launch instead of substituting
defaults. `safety show PROJECT` displays the current bindings.

The table is keyed by the project folder's canonical path. It stays when you delete the project and applies to a new project at the same path.

## The allow-list for your coordinator

The coordinator runs the binary every turn, so allow-list it in your agent **by subcommand, never the bare binary**. `context` prints the exact prefix (`Commands: <binary> --root <root>`); the patterns must start with it. For Claude Code, in the project folder's `.claude/settings.local.json`:

```json
{ "permissions": { "allow": [
  "Bash(<binary> --root <root> skill:*)",
  "Bash(<binary> --root <root> context:*)",
  "Bash(<binary> --root <root> inbox done:*)",
  "Bash(<binary> --root <root> list:*)",
  "Bash(<binary> --root <root> overview:*)",
  "Bash(<binary> --root <root> safety show:*)",
  "Bash(<binary> --root <root> routine list:*)",
  "Bash(<binary> --root <root> thread list:*)",
  "Bash(<binary> --root <root> thread show:*)",
  "Bash(<binary> --root <root> thread prompt:*)",
  "Bash(<binary> --root <root> thread ack:*)",
  "Bash(<binary> --root <root> thread restart:*)"
] } }
```

These patterns also cover the here-document form the coordinator uses to pass text on standard input (checked with Claude Code 2.1). A root with spaces is printed shell-quoted; write the pattern for that quoted form.

- **Allow `thread start` only where you've set `start_threads = "auto"`.** Left off the list, every thread start meets your agent's own permission prompt, which turns "propose first" from skill text into a real confirmation.
- **Never allow** `thread resolve` (with any flag), `thread adopt`, `delete`, `archive`, `pause`, `routine approve`, `new`, `open` or `ticker stop`.

For other agents the principle is the same: allow reading and steering, keep anything that starts, ends or deletes on a prompt.

## What the safety settings do and don't stop

- **They are soft.** Agents have a shell. The guards are the skill text, your agent's permission prompts, keeping `config.toml` and approvals outside every agent's working directory, and `routine approve` refusing without a terminal and a typed confirmation. None of this stops an agent that runs with skip-permission arguments from editing those files directly.
- **A thread can impersonate you.** Any thread agent can prompt the coordinator's pane through Herdr, and that message carries no ticker marker. The skill's rule that a go-ahead must name the threads lowers the risk; it does not remove it.
- **An approved routine command covers the command text only.** `./check.sh` keeps its hash while the script changes.
- **Prompt injection is reduced, not removed.** The coordinator reads reports and may choose to fetch pull request comments itself. Memory is a carrier: whatever it writes there is inlined into every later brief.
- **Agent variety.** The skill and briefs are agent-neutral, but only Claude Code has been exercised.
- **Cost.** Every thread is a full agent session, and each nudge and each `context` spends coordinator tokens.

## Nudges and notifications

`nudge = false` is the default, because on Herdr 0.9.1 a prompt that arrives while you are typing in the coordinator **is merged with, and submits, your half-typed text**. With it off, the ticker shows one Herdr notification per set of new inbox items ("3 new inbox items") and the coordinator picks them up at its next turn. Set `nudge = true` in `PROJECT.md` to have the ticker prompt the coordinator when it is idle; the message always begins `[herdr-projects ticker: automated, not the user, approves nothing]` and never carries outside text.

On Linux, inbox notifications and nudges use a supervised worker. It freezes the
actual sorted unseen item IDs, delivery mode, payload, configuration, project and
socket identities before submitting. Inbox and seen-state reads are bounded and
strict; malformed, aliased, special or oversized files refuse delivery. Nudge mode
also requires a fully primed, freshly ready coordinator with matching ownership,
agent kind and terminal identity. Toast mode uses the recorded session and does not
require a ready agent.

A correlated native acknowledgement confirms submission. A toast reply that
explicitly reports `shown: false` with a known rejection reason permits automatic
retry, with increasing backoff capped at five minutes and no attempt cap. Queue
polling has a 30-second cooldown, so the initial 15–19 second retry reservation runs
on the next eligible poll. A missing, wrong or contradictory reply is uncertain,
including a lost reply after an actual send. New inbox items or configuration/mode
changes cannot bypass that uncertainty. Recovery records a diagnostic instead of
adding another inbox item that could trigger another notification.

Use `doctor` or the inspection command below, then choose acknowledgement or an
explicit retry using the reported sequence:

| Action | Command | Result |
| --- | --- | --- |
| Inspect | `herdr-projects notification PROJECT inspect` | Shows claim, affected IDs, outcome and retry state. |
| Acknowledge | `herdr-projects notification PROJECT acknowledge --sequence N` | Suppresses the claimed IDs without another send; newer IDs remain eligible. |
| Retry | `herdr-projects notification PROJECT retry --sequence N --accept-possible-duplicate` | Authorizes a new linked request, frozen to the current batch, mode, configuration and session incarnation. |

Reading all affected items through `context`, or handling them with inbox `done`,
also permits later work to progress. Mere file disappearance is not consumption
proof. Acknowledged IDs remain suppressed until consumed; the worker prunes those
receipts on a later eligible poll. Migration refuses unresolved delivery claims or
remaining suppression IDs. Legacy reserved retries without a durable receipt are
imported as uncertain and require explicit reconciliation. These worker guarantees
apply to Linux; macOS acceptance remains untested.

## Interrupted agent starts

On Linux, legacy thread agent starts now share the bounded terminal worker queue
for local and saved-machine sessions. The worker checks exclusive recorded pane
ownership, fresh agent absence and exact pane/terminal identity before recording
a launch claim. It parses launch arguments from the same bounded configuration
bytes whose digest matches admission; kind-bound argument rules still apply.
Claims retain argument and route digests, terminal identity and lifecycle generation,
without copying launch arguments into the record.

Both transports use the JSON API bridge and require a typed `agent_started`
acknowledgement naming the same terminal and agent name, with the expected command
arguments. Native pending-start replies may omit detected kind; an explicit kind
must match, and briefs still require a fresh exact kind. This confirms submission,
not interactive readiness. A trust dialog or startup delay can leave the agent
blocked while the brief remains pending; fresh readiness observations still govern
brief delivery. A confirmed launch is never repeated for the same lifecycle
generation, including after PR metadata changes. If no agent is observed after a
confirmed start, status shows **Waiting on you** and asks for inspection before
explicit restart. Observation alone does not certify termination.

If the owning worker exits without confirmation, recovery marks the start
uncertain, fails the current generation and emits one inbox notice. Inspect the
pane before explicitly restarting; an existing live agent prevents restart.
Pending claims block briefs, new copies and restart. Migration refuses pending or
unresolved uncertainty; resolved acknowledged history is retained. The root/project
locks survive ticker death until supervised local descendants are cleaned up;
remote agent effects are not rolled back by local process cleanup.

The built ticker is tested through local and remote confirmed/lost restart cases.
macOS and real SSH-host acceptance remain untested. Canonical launch
certification and profile preparation remain separate prerequisites.

On Linux, coordinator starts also use the supervised queue. `open` records the
request and pane before returning; the ticker checks fresh agent absence, exact
pane/terminal identity, root ownership and kind-bound configuration before claiming
and submitting startup. A native launch-pending acknowledgement confirms only the
start submission. Priming waits for a later exact-kind readiness observation.

An interrupted coordinator start becomes uncertain and emits one inbox notice.
It blocks both automatic startup and priming for that request. Plain `open` cannot
turn a missing-agent observation into another launch after a durable claim; inspect
the pane and use `open --reprime` to explicitly request new work. Migration refuses
pending or uncertain coordinator starts, including historical uncertainty.

## Interrupted coordinator priming

On Linux, `open` and `open --reprime` queue priming for the supervised ticker
worker. The worker requires an existing ready coordinator of the configured kind,
an unambiguous pane and terminal identity, exclusive recorded ownership, unchanged
socket/configuration/project settings, and the native JSON API bridge. It records
a durable claim before sending and clears `prime_pending` only after a correlated
`agent_prompted` reply names the expected agent and terminal.

A lost reply leaves the result uncertain. Recovery emits one inbox notice and
holds that logical request without automatic replay, even if route or settings
metadata changes. Inspect the pane before explicitly running `open --reprime`;
that command allocates a new request while retaining old claim history until the
new claim is durable. A pending prime also defers inbox nudges. Migration refuses
pending or uncertain prime claims. Corrupt or oversized coordinator records are
preserved and require repair; ordinary updates cannot silently reset them.

Priming and restart recovery are tested with disposable Linux subprocesses and the
built ticker; macOS acceptance remains untested.

## Sidebar metadata refreshes

On Linux, the ticker queues token refreshes for open local and saved-machine
threads and the local coordinator. Workers use bounded, cancellable subprocesses
under inherited project ownership, validate the recorded project/socket incarnation
and configuration, and check fresh pane and agent inventories. Remote workers use
the frozen saved profile ID, SSH target and named session, with no mutable machine
fallback. Conflicting references or changed targets refuse the refresh.

The worker recomputes the thread group from fresh agent observations and persisted
report state. It sends only the fixed project/thread/review/rank tokens (project,
thread and rank for coordinators), source and five-minute expiration. Native `ok`
can acknowledge an ignored update; it does not prove metadata application, agent
activity or delivery. Before/after observations check the terminal identity.
Refresh failures create no execution claim or inbox notice. Repeating an attempt,
including after ticker restart, is safe for this expiring display metadata.

Refreshes share bounded queue admission and have a 30-second cooldown after each
attempt. Under saturation, tokens may expire before their next refresh; the ticker
does not guarantee refresh within five minutes for every pane. Interactive
foreground commands retain their immediate metadata updates. These changes do
not complete W04; canonical finalization workers and other planned work remain.

## Local session observations

The Linux ticker collects local agent and pane inventories through the shared
bounded control pool. One queued collection has a 30-second deadline; both lists
must succeed and agree before any status update. Initial status and dependent
work wait for the next 15-second ticker pass. Missing recorded sockets are
explicitly unavailable. Partial, malformed, truncated, cancelled or timed-out
collections produce no disappearance, idle or session-loss transitions.

Observations bind the project/socket incarnation, executable selection,
configuration and local thread/coordinator execution inventory. Application
rechecks those identities under the existing project/root guards, then uses current
records. Changed bindings discard old samples. Concrete effect workers still
perform their own checks before starting agents, sending input or updating metadata.
Read-only subprocesses use owned process groups and cancellation; they hold no
project effect lock while waiting for Herdr.

A sample expires 60 seconds after admission, independently of its shorter command
deadline. This allows a collection finishing after the first 15-second tick to be
consumed on the next pass without extending its age. Unused successful report-hash
samples have the same fixed lifetime while awaiting a session sample; consuming
them clears them on the next pass. The retained hash cache is capped at 128, and
changed execution/copy receipts invalidate its entries. A retained hash is an
observation, never proof of copied bytes. Source changes during retention may take
up to that lifetime to be detected.

Pending/unclassified observations veto automatic idle exit without resetting the
reachability clock. Retrying a known failure retains its classification; verified
negative observations permit ordinary five-minute idle exit when no other work
keeps the ticker alive. Never-opened projects are ineligible. The classification
cache is bounded at 128; untracked eligible identities conservatively veto idle
exit, so saturation may require an explicit ticker stop. Binding/configuration
inventory reads remain synchronous and bounded by entry/byte limits with deadline
checks; filesystem latency itself is not hard-bounded. Canonical observations use
the separate path below; canonical finalization still needs queue integration.


## Canonical session observations

With `state-store` on Linux, automatic canonical reconciliation uses the shared
control pool with a 15-second admission-to-collection deadline. The worker retains
project ownership from its initial snapshot through observation commit, marker
publication and claim expiry. Probes hold no SQLite transaction. Cancellation or
an elapsed deadline before commit discards the collection; a lost completion reply
does not undo a successful database commit. Other projects remain available, while
mutations of the project being observed defer until collection finishes.

The queue admits at most 16 jobs from 128 fair offers. Every observation batch must
drain before the next is admitted, so existing effects that require exclusive root
ownership get a full ticker pass between batches. Scheduling also runs before
admission. Queue replies carry only reachability and the committed event revision;
they cannot authorize effects, release capacity or certify completion.

Negative liveness results expire 60 seconds after admission and must match the
current project, configuration and canonical event revision. A changed revision,
failed probe or untracked identity vetoes idle exit until classified. Revision
checks use a read-only publication-checked query, with a 2 MiB publication budget,
100 ms cooperative deadline, SQLite progress cancellation and 10 ms busy timeout.
They do not run database-wide integrity checks or materialize event payloads.
Filesystem latency itself is not hard-bounded.

Canonical finalization remains synchronous. Collection and canonical effect
selection still read full snapshots; bounded historical-state materialization
and the remaining effect workers are separate unfinished W04 work. macOS and real
SSH-host acceptance remain untested.


## Canonical notification delivery

On Linux with `state-store`, the ticker queues previously accepted
`runtime.notification` operations through the shared effect queue. It does not
create notification authority from an inbox observation. The concrete worker checks
the frozen project, configuration, operation, delivery revision and local socket
identity, then retains project and inherited execution locks through a durable
claim, supervised send and outcome commit. A raw native JSON acknowledgement must
match the request ID, result type and shown/reason fields.

A verified native `disabled`, `rate_limited`, `no_foreground_client` or `busy`
response records proven no effect and uses the canonical operation's existing
retry budget/backoff. Lost, malformed, foreign or contradictory acknowledgements
remain ambiguous. Confirmed delivery is not repeated after restart. If the ticker
dies, supervision keeps the execution locks until its bounded local descendants
are gone; this does not undo a notification that may already have been shown.
Claim expiry records ambiguity and does not resend.

A new batch cannot overlap another claimed or ambiguous canonical notification,
even after changing its task, route or configuration. Consume the older inbox
items to permit a disjoint batch, or inspect the possible effect and explicitly
retire its operation. Retirement retains the historical possible-effect outcome.
Malformed unresolved notification history refuses delivery. Foreground notification
commands use the same overlap rule and retain their synchronous adapter.

Effects, canonical routines and canonical observations defer same-project queue
admission while another of those jobs is pending. Other projects can continue
status observation. Queue completions only clear volatile bookkeeping; only the
concrete worker can publish a canonical notification outcome.

## Interrupted brief delivery

On Linux, local and saved-machine thread briefs use the shared bounded queue
and a concrete supervised sender. It validates the recorded socket identity, configuration,
execution and fresh unambiguous agent/pane observations before claiming delivery.
Only a typed acknowledgement naming the same agent confirms delivery. Remote
workers freeze the saved profile ID, literal SSH target and session from the
observation, then recheck the current profile before claiming, before sending and
after acknowledgement. Incomplete saved-session contracts refuse automatic briefs.
The remote executable defaults to `herdr`; `HERDR_PROJECTS_REMOTE_HERDR_BIN` may
name its installed path. It must support `remote-api-bridge` and an existing server
in the saved session. No install, startup or automatic retry occurs. Delayed shell observations queue thread launches; workers revalidate before dispatch.

The sender checks other project and coordinator references, including resolved
threads and socket aliases. If either reference is remote, a duplicate pane ID
refuses even across different machine labels or sessions: SSH aliases and loopback
connections do not prove distinct terminal servers. This can conservatively block
unrelated servers with colliding pane IDs. Corrupt or oversized inventories refuse.
State-store builds inspect active canonical neighbors through a bounded read-only
identity reader. Interrupted migrations, corrupt publication/provenance, dangling
resource references and oversized inventories refuse. Default builds still refuse
canonical neighbors because they cannot inspect their SQLite authority.

A pending claim
recovered after its owning worker exits becomes uncertain: the prompt may already
have reached the agent. Recovery marks the matching open execution failed and
writes one replay-safe inbox notice before session checks. Inspect the agent,
then explicitly restart or resolve the thread; recovery never resends the prompt.
Historical uncertainty does not fail a replacement execution. Pending claims block
new copy projections, and retained projections block new brief claims. Migration
refuses unresolved claims; confirmed history or notified uncertainty on an
explicitly resolved thread can pass validation.

## Merged pull request recovery

The ticker saves final-copy intent before copying a merged thread's report and
library. On Linux, idle and merged final copies use the shared bounded background
queue. Retained projections are offered before session checks; automatic brief
prompts and agent starts wait until pending projections finish. Idle resolution
also requires fresh, unambiguous observations from the recorded session. Missing
or changed eligibility can finish the copy while leaving the thread open. Failed
copies leave the thread open and retry with bounded backoff, independently of GitHub polling and across restarts. A paused project
waits until resumed. A retained projection must finish before reopen or restart.
Outside a pending projection, reopening or restarting invalidates the old retry
and prevents the same merged PR from immediately closing it again; a new PR can
be followed normally. Changed execution identity is checked before and after
copying. Finalization never removes the worktree.

Existing partial-copy policy remains: skipped symlinks or an oversized library
may resolve the thread, with a durable inbox warning (included in the finalization
notice for background copies). Failed copies
do not resolve it. Content checksums prevent stale equal-size/equal-mtime library transfers.
On Linux, background live copies use the shared bounded executor and a surviving
process supervisor. Local copies invoke this binary's native artifact sender. Remote
copies require a compatible `herdr-projects artifact-stream --probe` response containing
`live_versions: [1]`; install the matching helper remotely or select its executable
with `HERDR_PROJECTS_REMOTE_BIN`. Unsupported helpers refuse instead of falling back
to a different transfer method. Local namespace supervision is required; it does not
claim termination of a remote helper after an SSH connection is lost.

Local report hashes run in read-only helper jobs after each full project pass.
Reads have a 10-second execution limit, a 30-second total queue lifetime, and a
50 MiB report limit. At most 16 reads are outstanding; late or changed-execution
results cannot trigger copies or new review announcements. Hash completion is
observed on a subsequent ticker pass, so copy/announcement discovery may take
an additional pass. These observations never certify a copy receipt.

Copies recheck execution, configuration and routing before publishing. The exact
staged bytes and recovery intent survive interruption in `.state/live-copies`; the
next active ticker resumes them without downloading again. Configuration or routing
withdrawal blocks that recovery while preserving the intent. At most 16 stage/spool
entries are retained per project. Before a new download, a worker holding project
ownership can reclaim recognized unreferenced staging directories from a full
inventory. It first reads every thread record within bounded limits; corrupt or
unknown records retain staging. Referenced live/final projections and unknown
directory names are preserved. Cleanup never follows links or crosses devices.
Cancellation can leave part of an unreferenced temporary directory for a later
attempt. Recovery bypasses this cleanup, including when unrelated records or the
inventory prevent new downloads. New review announcements wait for pending copies. Copies remain
additive and per-file atomic, so an interrupted projection may expose some new library
files before its report and receipt are committed.

Live ticker copies also save a receipt and any partial-copy warning together with
the copied report hash. Warnings deliver independently of review readiness and retry
after restart with the same inbox identity, including when the item is already in
`inbox/done`. An undelivered warning prevents another live copy from replacing its
receipt. Historical warnings identify their original execution and report; delivering
one does not change a replacement execution. Saved notes remain available for the
later review announcement. A library failure does not advance the copied report hash.
Review announcements now save their full payload before inbox delivery. Retries reuse
the same ID and acknowledge only the matching execution and copy receipt. A pending
announcement prevents another live copy from replacing its report; historical notices
still deliver after execution replacement without acknowledging the replacement.
The notice body records the original report hash because `threads/<id>.md` can later
change. Legacy reports without typed receipts retain their existing hash-based
deduplication. Already-prepared notices can drain even when the session is unavailable.
Complete local and supported remote final copies also retain content-addressed snapshots in
`.state/artifacts/<thread>/<manifest-hash>/`, with a verified `manifest.json`.
Snapshots contain the report and library, including empty directories; the combined
limit is 50 MiB, 10,000 entries and 64 directory levels. Symlinks, hard links,
special files and non-UTF-8 names are refused. Staging failures and changed sources
leave previous snapshots intact. Snapshot retention is currently manual: no
background process removes them. Source reads traverse opened directory descriptors
and refuse symlinks in every source-path component, including ancestor aliases
(for example, macOS `/var` paths must use their physical `/private/var` spelling).
Reads enforce byte and entry limits as they proceed, with a cooperative ten-second
deadline per scan; this does not interrupt a blocked filesystem syscall. Local
live-report reads also enforce the 50 MiB limit and preserve the last home copy on
failure. Live `threads/<id>.md` and `library/<id>/` remain
compatibility copies, separate from these retained snapshots. Remote finalization requires the native helper described in [remote transport](remote-transport.md). Local Linux cleanup also checks Git registration/branch/commit, cross-project references, managed panes and process descriptors/cwd/mappings. Any uncertainty keeps the worktree. These checks assume cooperative same-user agents; they are not hostile-process containment.

GitHub outage streaks are tracked per project and PR URL, and machine streaks per
project/session/machine. Healthy resources do not clear another resource's
outage. Streaks and pending outage/PR inbox events survive ticker restarts;
replaying an event already in the inbox or handled folder does not duplicate it.
`doctor` reports pending finalizations, notification retries, outage counts and
unreadable or malformed ticker state in `.state/ticker.json`.

## Routines

A file `routines/<name>.md`: TOML front matter with `schedule` (`every <N>m|h|d` or `daily HH:MM`, local time), optional `command`, `enabled`; the body is the prompt the coordinator receives as an inbox item when it is due. A routine with a `command` runs (`sh -c`, in the project folder, 60 second timeout) only when `routine_commands = true` **and** you have run `herdr-projects routine approve <project> <name>` in a terminal; its output reaches the coordinator capped at 4,000 characters inside a fence labelled as untrusted. Edit the command and it stops until approved again.

On Linux, approved legacy commands run through the shared background executor,
sharing its 128-offer inventory and project/operation rotation with live copies.
Only one legacy copy/command or canonical routine ticket is admitted per root,
after a full project pass. Commands retain a 60-second execution limit within a
95-second queue/execution budget. Namespace supervision and inherited ownership
keep a surviving command from overlapping replacement project effects.

The worker rechecks the project identity, active state, exact routine definition,
current enablement/approval and schedule cursor. It durably advances the cursor
and saves a claim before running. After interruption, a claim without a saved
result produces an unknown-outcome inbox item; that occurrence is never rerun.
Completed output is saved before inbox delivery, which retries with the same ID
even if the item was already handled. Later scheduled occurrences may still run.
This does not roll back command effects or extend command-text approval to scripts
and files referenced by the command. Legacy scheduling still requires a reachable
project session; other platforms retain the prior synchronous path and are untested.

**Environment compatibility:** supervised commands receive only bounded `HOME`,
`USER`, `LOGNAME`, `PATH`, locale variables, `TZ`, `TMPDIR`, SSH agent variables and
XDG config/cache/data/runtime paths. Arbitrary inherited variables, including
provider credentials, are no longer passed automatically. Existing commands that
rely on them need an explicitly configured environment in their approved script.
Nonzero exits are reported as supervisor status 200, not the original exit code.

Routine discovery accepts at most 4,096 directory entries and 128 KiB per definition.
Approval records are limited to 1 MiB; ticker state and individual thread records
are limited to 16 MiB. Private control
files must be regular, single-link files; FIFOs and symlinks refuse promptly.
These local filesystem reads are bounded but do not promise deadlines for a stalled
filesystem. Unreadable approval records cannot authorize commands or be overwritten
by a new approval.

Intervals require positive ASCII digits; the converted seconds must fit a signed
64-bit integer (at most 9,223,372,036,854,775,807 seconds). Invalid Unicode suffixes
and oversized intervals produce a routine configuration diagnostic. Other routines
continue running. Daily schedules use the ticker machine's local timezone: a time
in a daylight-saving gap shifts forward by the gap, and a repeated time runs only
at its first occurrence. Missed daily occurrences coalesce into one run on return.

The command deadline includes input delivery and output draining after its parent
exits. Timeout cleanup allows a 200 ms TERM grace period before KILL. Each output
stream is captured up to 1 MiB, with excess drained and discarded; routine reports
label capture truncation. The inbox display limit remains 4,000 characters.

## Threads on other machines

Save the machine with `herdr machine add --label <label> <ssh target>` (both machines need Herdr 0.9.1), then list a repo as `--repo /path/on/machine@<label>` or pass `thread start --machine <label>`. The home machine owns the project; only outbound SSH from home is needed, in batch mode, so set up key-based login first.

- The worktree, the brief and the report live on the remote machine. The home ticker polls it once a minute. On Linux, changed reports and libraries use the supervised native live-copy stream described below. Other platform paths retain the existing `scp`/`rsync` behavior. Symbolic links are never followed or copied; a library over 50 MiB is omitted with a durable copy warning.
- A machine that doesn't answer is left alone: no state is read, threads keep their last group, and that project's session is skipped for about two minutes. After ten minutes you get one `outage` inbox item, and one more when it is back. Polling and backoff are tracked separately for each project/session, so projects sharing a machine label all receive poll opportunities. The first project in the slow pass rotates each tick.
- A blocked remote thread needs you in its pane on that machine: select the machine in Herdr's sidebar, or run `herdr --remote <ssh target>`.
- `focus` does not cover remote threads: their sidebar tokens are set on the remote Herdr server. They appear in `overview`, `thread list` and inbox items.
- Tasks with no repository always run locally, as tabs.

## Laptop-closed operation

No plugin code is involved: install Herdr and this plugin on an always-on machine, keep the projects root there, open the project there, and attach from your laptop with `herdr --remote <ssh target>` (add `--session <name>` for a named session). The ticker runs on that machine. If Herdr asks whether to restart a remote server "that may not survive SSH connection loss", answering `n` keeps its panes. Checked on a Linux (aarch64) machine from a Mac.

## Development

```bash
cargo test                       # unit tests and scenarios against a scripted fake runner
scripts/dev-server               # a throwaway `hp-dev` Herdr session with a scratch root
scripts/dev-hp <subcommand>      # the binary against <repo>/.dev-root; pass --session hp-dev to open/doctor
scripts/dev-herdr <args>         # herdr against that session
```

Never develop against your default session or `~/.herdr-projects`. [`herdr-notes.md`](herdr-notes.md) records what was verified about Herdr stage by stage, and [`manual-test.md`](manual-test.md) lists the acceptance checks, including the visual ones only a person can confirm. [`going-public.md`](going-public.md) is the checklist for the public release.

## Repair diagnostics and standing instructions

Unreadable or malformed `.state/ticker.json` stops that project's ticker work
without replacing its saved retry obligations. Other projects continue. Preserve
the original file and inspect `doctor` before restoring a valid backup or repairing
it. The binary does not automatically delete or quarantine malformed state.
Malformed thread records are named in `doctor` and `context`; readable threads
remain visible.

Coordinator startup reads `context`, which now includes the full current
`PROJECT.md` instructions with a content revision and character count. Refreshing
context picks up edits. Worker briefs receive instructions and memory at start or
restart; existing workers do not automatically receive edits. The capacity limit
is advisory and worker arguments are shared, not isolated by agent kind.

## Popup context and inactive projects

Each action popup receives its own single-use context ID, valid for ten minutes.
A popup with expired, mismatched or already consumed context asks you to reopen the
action. It never falls back to another action's most recent context. The old shared
`handoff.json` is no longer read. Expired recognized handoffs are pruned as new
handoffs are created; corrupt handoff files are left for inspection.

Paused and archived projects refuse coordinator open, thread restart, follow-up
prompts and adoption. Resume or unarchive the project first. Restart also refuses
conflicting pane ownership instead of reusing another thread's pane.

Remote directory transfers support literal spaces, Unicode and shell characters
when both hosts support rsync protected arguments. Capability checks run before
transfer and give an installation diagnostic when support is missing. See
[remote transport](remote-transport.md) for the tested boundary. Invalid or missing
remote library-size observations refuse transfer. Verified remote snapshots use the bounded native stream; remote removal still lacks a writer checkpoint adapter and is refused.


## Removal recovery and explicit repair

Removal writes its operation ID, canonical repository/path, retained branch/head,
generation and snapshot before calling non-force Git removal. Untracked/dirty
content remains protected by Git. Managed launches/prompts/ticks share an operation
lease; pending removal records are excluded from automatic launches. Removal never
deletes the branch. `thread resolve PROJECT ID --reopen` changes logical status;
`thread restart PROJECT ID` verifies the receipt and retained branch, then reattaches
it at the recorded path. Branch changes, conflicting registrations or replaced
paths require inspection. A legacy half-created branch without a removal record
is still refused. After a crash between removal and acknowledgement, restart uses
the persisted intent and Git evidence; it never resets or overwrites a branch.

`repair PROJECT inspect` prints structured JSON diagnostics and SHA-256 hashes for
malformed thread, ticker and inbox records. It does not modify them. Prepare a
valid replacement, stop the ticker, then use:

```sh
herdr-projects repair PROJECT restore threads/t-0001.toml --from /path/to/replacement.toml --expected-hash HASH_FROM_INSPECTION
```

Restore validates the replacement and filename identity, holds ticker/lifecycle/
project locks, rechecks the original hash, and saves the exact original bytes under
`.state/repair-backups/` before atomic replacement. Changed records, unsafe paths,
links and invalid replacements are refused. This is an explicit restore tool;
it cannot reconstruct missing obligations or certify the semantics of a supplied
replacement. Keep backups and review the restored record before resuming work.
