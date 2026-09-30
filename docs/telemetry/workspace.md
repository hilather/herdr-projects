# Herdr workspace: fleet pane, actions, digest section and checks (TM4.8)

Plan card TM4.8 (doc 12), doc 15 (Herdr workspace integration). Common rules
are [contracts.md](contracts.md) §0. Code: `src/telemetry/workspace/`
(`mod.rs` snapshot, watch, doctor checks; `render.rs` pane text and digest
section; `owner.rs` owner-command routing), the plugin actions and panes in
`src/actions.rs` and `herdr-plugin.toml`, the sidebar suffix and
`thread start --reason` in `src/threads.rs`, the digest section in
`src/coordinator.rs` (legacy) and `src/cli.rs` (`context` of a migrated
project), the checks in `src/doctor.rs`. Tests:
`tests/telemetry_workspace.rs`.

Every surface here is advisory. None can launch, reserve, select, change a
budget, accept a finding or write project memory. Interactive writes are the
three owner popups, and they only run an existing owner command after an
explicit `y`. That command keeps its own authority checks. The signed routines in §10
also write operator reports or queue replay tasks under existing routine authority.

## 1. One snapshot, every surface

`telemetry <slug> workspace show --json` prints the fleet snapshot
(`telemetry-workspace.v1`). Every surface renders this snapshot, so for the
same read they show the same values. Each value is copied from an existing
read path and never recomputed:

| section | source (the command that prints the same values) |
|---|---|
| `coverage`, `services.M38/M39/M40`, `replay` | the TM4.1 query service, one request for M13, M38, M39, M40 and M49 (`telemetry <slug> query --metric M13,M38,M39,M40,M49 --json`): `definition`, `status`, `value`, `reason`, `numerator`, `denominator`, `coverage.state`, `lag_ms`, `lag_reason` verbatim |
| `services.quota_at_last_dispatch` | M40's latest decision per service, verbatim (`detail.decisions[]`: windows, remaining %, freshness) |
| `active` | the TM1.8 attempt projection (`telemetry <slug> attempts --json`): each record whose `terminal_state` is `open`, with its `usage` and `attention` verbatim. `state` is `running` when the record has a running mark, `launching` when it has a launching mark, else `reserved`. `configuration_label` is `"<kind> <agent_version>"` of the dispatched configuration, the same label `compare` uses. `coverage` is `complete` when usage is bound, else `unavailable` |
| `active[].waiting` | the projection's closed waiting time (`waiting_ms`), plus the open wait from the attention lane (`telemetry <slug> accounting attention --json`), taken from the interval that ends `open_at_horizon` of an attempt the lane sees as open. `open_since_unix_ms` is when the wait opened. `open_observed_ms` is last observation − opened: observed only, never extrapolated to now |
| `configurations` | the TM4.4 comparison `telemetry <slug> compare --metric M02 --json`, per task class: each arm's `tasks`, `status` (`shown`, `suppressed`, `empty`), `value`, `decimal`, `interval`, `pooled`, `min_sample` verbatim. Cells below 20 tasks stay suppressed. The pane prints no ranking |
| `candidate_groups` | `telemetry <slug> quality groups show`: groups are tagged `race#1`, `race#2`, … in creation order, and each arm has its `outcome`. `awaiting_selection` is set when the group is open and every arm is `candidate` or `failure_no_candidate` |
| `alerts` | the TM4.5 recorded open alerts (`telemetry <slug> health alerts --json` → `open`): id, rule, labels, state, reason codes, opened, occurrences and notice id. The inbox notices are that lane's own `health notify` |
| `needs_you` | derived from the sections above, most urgent first: open alerts (critical, then warn, then unknown), attempts waiting on you now (longest observed wait first), and candidate groups awaiting selection |

**Degrading.** If the query service or the attempt projection cannot answer,
for example because `state.db` or the telemetry sidecar is unreadable, the
whole snapshot is `{status: unavailable, reason: query_service_down |
attempt_projection_down, error}`. It carries no section and no number, and
every surface says so once. If only the comparison, the groups or the alerts
cannot be read, that section alone reads `n/a (<reason>)`.

Switching `[telemetry] views = false` in `config.toml`
([operator-views.md](operator-views.md) §6) also turns off every surface
here. The workspace CLI then refuses with the switch's message, the pane
prints it, the digest section is omitted, and `doctor` reports the switch.

## 2. Commands

```
telemetry <slug> workspace show [--json]        the snapshot (text = the pane body). Read-only
telemetry <slug> workspace digest               the coordinator digest section, as `context` appends it. Read-only
telemetry <slug> watch [--interval-secs N] [--iterations N]   the pane, re-rendered every N s (1-300, default 5). Read-only
thread start <slug> ... [--reason CODE] [--note TEXT]         dispatch reason capture (§6)
```

Each workspace command reads exactly one project: the same slug and
symlink checks as the views ([operator-views.md](operator-views.md) §5).

## 3. Plugin panes and actions

| action id (`herdr-plugin.toml`) | opens | what it does |
|---|---|---|
| `fleet` | popup `fleet` | one-shot fleet view: report, active attempts, the five operator views, then the workspace snapshot (§7) |
| `fleet-watch` | split pane `fleet-watch` | `telemetry <slug> watch` for the invoking workspace's project until the pane is closed (`HERDR_PROJECTS_FLEET_INTERVAL` overrides the 5 s interval) |
| `fleet-race` | popup `fleet-race` | proposes a candidate group (§4) |
| `fleet-select` | popup `fleet-select` | records a candidate-group selection (§4) |
| `fleet-replay` | popup `fleet-replay` | launches a replay-suite run (§4) |

Every action binds its popup to the invoking workspace's project and the
identity (`device:inode`) of that project's store. It uses the same one-use
handoff as `fleet`. A handoff that names another project, or a store that
changed, is refused. Without a workspace project the popup asks for the
project.

**Keybindings.** A plugin cannot bind keys. You add bindings in your own
Herdr configuration as `[[keys.command]]` entries of type `plugin_action`,
for example:

```toml
[[keys.command]]
key = "prefix+f"
type = "plugin_action"
command = "herdr-projects.fleet-watch"
description = "Projects: fleet pane"

[[keys.command]]
key = "prefix+F"
type = "plugin_action"
command = "herdr-projects.fleet"
description = "Projects: fleet at a glance"

[[keys.command]]
key = "prefix+r"
type = "plugin_action"
command = "herdr-projects.fleet-race"
description = "Projects: propose a candidate group"

[[keys.command]]
key = "prefix+s"
type = "plugin_action"
command = "herdr-projects.fleet-select"
description = "Projects: select candidate"
```

The panes read no keys themselves. Inside the pane, doc 15 §3's `tab`,
`enter`, `r`, `e` and `?` are not built. Their actions are the Herdr actions
above, and a user binds those actions to keys.

## 4. Owner popups: proposals routed through owner commands

Each popup asks its questions and prints the exact owner command it would
run (`Runs: herdr-projects …`). It runs that command only when the answer to
`Confirm (y/N)` is `y`. The command is parsed by its own `clap` definition
and runs through its own entry point (`workspace::owner`). Only
`quality groups create|select|show` and `replay run|subset|report|show` can
be routed:

- **race**: `telemetry <slug> quality groups create <task> --arm <profile> ...`.
  This seals a group. Each arm is still an ordinary attempt that goes
  through the launch path under its own profile's budget, and the group
  grants no launch (contracts-quality.md §3).
- **select**: `telemetry <slug> quality groups select <group> (--arm N | --none) --reason CODE`.
  The popup lists the open groups by `race#N` tag. A selection verifies,
  integrates and moves nothing.
- **replay**: previews `replay <slug> subset …`, then runs `replay <slug> run
  --suite V --configuration C --subset S --seed N --expected-head H`, where
  H is the head read at preview time. The run creates replay tasks only.
  Each task still needs its own signed contract and approval
  (contracts-replay.md §3).

Those commands refuse a worker execution context before any write. The
markers are that HOME is a recorded execution home, or that the working
directory is a task worktree (contracts-review.md §9). The store also
refuses worker, attempt and import principals. So a popup run by a worker
writes nothing, and a popup answered `n` writes nothing
(`owner_popups_refuse_worker_context_and_write_only_through_owner_commands`).

## 5. Coordinator digest section

`context <slug>` prints the coordinator digest. For a legacy project with a
canonical store, and for every migrated project, it ends with this section.
The section is bounded to 40 lines and 4096 bytes by top-N selection: 5
waiting attempts, 4 task classes × 4 arms, 5 candidate groups and 5 alerts,
each with a `+N more` count. A final guard truncates with a marker. When
the query service is down the section is one line. The section is advisory
and says so: it grants nothing, asks for a dispatch reason code, and tells
the coordinator not to copy it into memory. Telemetry never writes
`MEMORY.md` or `memory/`. Not built: doc 15's `unchanged since` marker,
which would need a write on every turn.

## 6. Sidebar suffix and dispatch reason

**Sidebar.** Each thread's pane reports a `telemetry` token beside
`project`, `thread`, `review` and `rank`. It holds the agent label, the
usage-coverage glyph and, while the thread waits on you, how long:
`codex ○ 6m`. A thread has no usage collector binding, so its coverage is
always `○` (unavailable), never a number. The suffix shows no ranking and no
cost. Herdr shows the token only where your sidebar format names it, so
leaving it out of the format disables the suffix. Resolving or pausing a
thread clears the token with the others.

Running canonical attempts report the same suffix to the pane named by their
retained runtime binding. Their glyph comes from the attempt telemetry read
path: `●` for complete usage coverage, `◐` for partial, and `○` for unavailable
or unknown. An open wait adds its observed duration, without extrapolating
beyond the last attention observation. There is no ranking or cost. Refreshes
check the binding and ownership revisions, recorded socket incarnation and
native pane/agent identities. Termination, runtime ownership or usage collector
binding revocation, and project pause clear the token on the unchanged route. A replaced binding receives no
update; native token expiry bounds decoration on a gone or changed pane.
These metadata jobs are advisory and create no launch, reservation, approval
or budget effect.

Canonical token admission and rechecks read only selected runtime bindings,
launch receipts, ownership, collector binding and lifecycle observations; they
never read the whole store snapshot. Live Running/AwaitingInput attempts may
publish. Cleanup is offered only within 300 seconds of termination,
relinquishment, collector revocation or project pause; a missing pane during
cleanup is already clear. Native expiry handles older observations. Each
project offers at most 16 attempt jobs per tick, rotating through attempts;
these offers never evict other queue entries. An unchanged suffix refreshes
at most once per 100 seconds per ticker process, while changes publish promptly.

The effect guard is acquired nonblockingly for short store and identity checks
and released before native I/O. A busy guard skips that decoration tick.
Binding/ownership generations, project/configuration identity and socket
incarnation are checked immediately before and after metadata publication;
native pane and agent identities are observed around it. A pause, revocation,
route replacement or native terminal replacement can occur between the final
check and the send. The post-send checks detect those changes; they cannot
undo an already delivered suffix. This is safe for advisory metadata because
it grants no execution authority, records no durable success and expires
within 300 seconds even if the old route cannot be reached for cleanup.

**Reasons.** `thread start --reason CODE` accepts the canonical dispatch
log's operator reasons: `operator_selected`, `recommended`,
`operator_preference`, `availability`, `exploration`, `replay`,
`continuation`, `unspecified` (the default). `--note` takes one line of at
most 160 characters. Both are checked before anything is recorded or
created. They are kept on the thread record (`dispatch_reason`,
`dispatch_note`) and echoed in the command's JSON. Canonical launches
already record their reason in the `DispatchDecision`, through the launch
selection's `reason` (TM1.8).

## 7. `doctor` telemetry checks

For each migrated project with a canonical store, `doctor` adds these
checks. They never fail `doctor`: each prints `ok` or `warn`, and a warning
names the command to run.

- `telemetry query service`: `ok` with the time the snapshot took, or
  `failed (<reason>)`, in which case every workspace surface shows
  unavailable.
- `telemetry ingestion lag`: time since the last collect; degraded above
  15 minutes or when nothing was ever collected.
- `telemetry collector coverage`: open attempts without bound usage, with
  their reasons.
- `telemetry digest section`: its size against 4096 bytes and 40 lines.
- `telemetry alerts`: no open alert, the number of open alerts, or `n/a`.

Not built: the OTLP receiver, spool and certificate-version checks of
doc 15 §9, because this build has no OTLP receiver or spool to check.

## 8. Recorded terminal captures

These are real outputs of `tests/telemetry_workspace.rs`'s `live_fleet`
fixture, and `workspace_doc_captures_are_real_outputs` checks every line
below. The fixture's fleet is:

- one running Codex attempt that has been waiting on you for 6 minutes, and
  that TM4.5 recorded as a `waiting_on_you` warn alert. Its quota was
  62.5 % remaining at dispatch.
- 20 accepted `code` tasks on `claude 1.0`.
- 3 failed `code` tasks on `gemini 2.0`, suppressed because they are below
  20 tasks.
- one sealed race with no arm launched yet.

Identifiers that differ per run are shown as fixed placeholders. The query
time and each attempt's elapsed time are measured at each read.

<!-- capture: workspace show -->
```text
─ NEEDS YOU (2)
  ! alert #1 warn waiting_on_you [attention] waiting_on_you
  ! attempt-3f5c6a8e task work waiting on you 6m00s so far
─ ACTIVE (1)
  attempt-3f5c6a8e task work running config codex 0.154.0 [acc318d1] · elapsed 0s · waiting 6m00s so far (waiting now) · usage ○ (not_bound)
─ SERVICES
  M38 throttled time share: n/a (throttling_not_certified)
  M39 provider error rate: n/a (provider_errors_not_certified)
  codex quota at last dispatch (attempt-3f5c6a8e): primary 62.5% remaining (window 300m, fresh) · secondary n/a (not_reported)
─ CONFIGURATIONS · M02 acceptance · terminal_cohort · observational · 95% interval · min 20 tasks per cell · never a routing decision
  code
    claude 1.0 [c1a0de10]  20/20 (1.0000) [20/20–20/20] n=20 pooled 1
    gemini 2.0 [9e3141a2]  insufficient data n=3 (min 20)
─ CANDIDATE GROUPS (1)
  race#1 task work open: arm 1 codex 0.154.0 [acc318d1] not_launched · arm 2 codex 0.154.0 [5b0e7c44] not_launched
─ REPLAY
  M49 replay suite pass rate: n/a (no_replay_suite)
─ ALERTS (1 open; `health notify` leaves inbox notices)
actions: fleet-race · fleet-select · fleet-replay (each shows the owner command and runs it only on confirmation)
```

The first line of `workspace show` reads `demo · fleet · query <UTC time> ·
usage coverage n/a (empty_denominator) · advisory, read-only`. Each alert
line under `─ ALERTS` also carries its opening time and occurrence count.

<!-- capture: workspace digest -->
```text
## Fleet (advisory · as of 13:27 UTC · telemetry-workspace.v1)
Active attempts: 1 (running 1, launching 0, reserved 0); bound usage 0 of 1
Waiting on operator: attempt-3f5c6a8e task work (6m00s so far)
Services: throttled n/a (throttling_not_certified); errors n/a (provider_errors_not_certified); codex quota at last dispatch (attempt-3f5c6a8e): primary 62.5% remaining (window 300m, fresh) · secondary n/a (not_reported)
Routing evidence (M02 acceptance, terminal_cohort, 95% interval, n; below 20 tasks insufficient):
  code: claude 1.0 20/20 [20/20–20/20] n=20; gemini 2.0 insufficient (n=3)
Health alerts (1 open): warn waiting_on_you [attention] waiting_on_you
Replay M49: n/a (no_replay_suite)
Evidence only: it grants no launch, budget or selection. Give a dispatch reason code (launch selection `reason`, `thread start --reason`). Do not copy this section into memory.
```

The fleet popup prints the same lines after its report and view sections:

<!-- capture: pane fleet -->
```text
active attempts (1)
views (as `telemetry demo view <name>`; live, all time)
─ NEEDS YOU (2)
  ! attempt-3f5c6a8e task work waiting on you 6m00s so far
    claude 1.0 [c1a0de10]  20/20 (1.0000) [20/20–20/20] n=20 pooled 1
  race#1 task work open: arm 1 codex 0.154.0 [acc318d1] not_launched · arm 2 codex 0.154.0 [5b0e7c44] not_launched
```

The following outputs are asserted verbatim by
`surfaces_degrade_to_unavailable_when_the_query_service_is_down`. With the
telemetry sidecar unreadable, `workspace show` prints this, and the popup,
`watch` and the digest print the same single line:

```text
demo · fleet · unavailable (query_service_down): nothing numeric is shown
## Fleet (advisory): unavailable (query_service_down); no telemetry evidence this turn
[warn] project demo: telemetry query service: failed (query_service_down); every workspace surface shows unavailable; run `telemetry demo query --metric M13` for the error
```

## 9. Restrictions

- The configuration view compares M02 only, the default cohort, over all
  time. For M07, other cohorts and windows, use `telemetry <slug> compare`.
- Services come from M38/M39, which are uncertified (`n/a`), and from M40,
  which is quota at dispatch rather than live headroom. Live window headroom
  is TM4.5's `quota_headroom` rule, and it appears here as an alert.
- Weekly report and replay routines use the existing signed routine path
  (§10).

## 10. Owner-signed telemetry routines

The existing `routine-store import`, `schedule`, `execute` and ticker path
accept two typed command templates. The script file starts with exactly
`# herdr-telemetry-routine.v1` followed by a newline and one JSON object:

```text
# herdr-telemetry-routine.v1
{"kind":"weekly_report"}
```

```text
# herdr-telemetry-routine.v1
{"kind":"replay","suite":"v1","configuration":"worker","subset":"stratified:2","seed":"weekly"}
```

These files are inputs to the existing signed `RoutineDefinition`, not
standalone shell scripts. Set `script` to the absolute template path,
`script_sha256` to its exact bytes' SHA-256, `cwd` to the project, and an
existing schedule such as `every 168h`, timezone `UTC`, `deadline_ms` at most
60000, and the usual output cap. Sign the definition with the owner's key
in namespace `routine@herdr-projects`, then install with
`routine-store <slug> import DOCUMENT SIGNATURE --expected-head H`.
The configured owner must enable `routine_commands` for this project.
The existing signature, script digest, config identity, enabled state,
overlap, missed-run, claim and receipt checks all apply. Editing the
JSON withdraws its approval. No new scheduler or signer exists.

**Weekly report.** Reads the workspace snapshot (refuses a whole-snapshot
outage or disabled views) and the existing JSON export service for M01,
M02, M13, M38, M39, M40 and M49. The Markdown copies each export value,
cohort/time basis, coverage and denominator as sample size; a value or
sample size that does not exist is `n/a (reason)`. This is an all-time
fleet evidence report generated weekly, not a new weekly cohort estimator.
The companion is the exact `export.v1` manifest with a `report` extension
containing the Markdown filename, byte length and SHA-256 digest.
No external export is sent. Only `library/` receives report artifacts;
no project memory is written.

Names use the UTC ISO week at execution: `library/fleet-YYYY-Www.md` and
`library/fleet-YYYY-Www.manifest.json`. A second successful execution in
the same week chooses `-v2`, then `-v3`, up to 1000. Neither file is ever
replaced. Each file is synced and atomically published with the export
writer; the manifest is published first, so an interruption can leave an
orphan manifest, whose version is skipped on retry. A symlink `library/`
is refused. Worker briefs render immutable selected instructions and scoped
memory objects plus worktree/output framing (`memory/worker_brief.rs`);
they do not scan `library/`. The E2E test retains new worker knowledge and
reserves an approved attempt after the report is written, then verifies
its public rendered brief contains no report content or report filename.

**Replay.** `configuration` must name a profile in the signed owner config;
the suite must exist and `subset` must be `stratified:N`, N from 1 to 16.
These checks precede the execution claim. The template uses the ordinary
replay run implementation with the current head under the routine's existing
exclusive project ownership (equivalent to passing that head as
`--expected-head`). Git repository creation checks the absolute routine
deadline and cancellation and kills/reaps a child that exceeds it.
The routine creates/queues only the selected replay tasks. It installs no
contract, approval, attempt or reservation and does not enable admission.
The owner still signs the contracts and launch approvals, and ordinary
profile budgets apply at launch. As for explicit replay runs, a failure
partway through repository/task creation can leave recorded partial work;
a claimed occurrence is never automatically replayed.

E2E evidence: `tests/telemetry_routines.rs`, using the shared signed replay
workflow fixture in `tests/support/replay.rs`.
