# Canonical worker launch implementation

Canonical prepared-worker dispatch is enabled after the authorized live controller
workflow and production-default controller regression tests. This page tracks
runtime work, not packaging or general release acceptance. See the current
[dispatch audit](dispatch-enablement.md) for evidence and unsupported capabilities.
Later dated sections preserve the implementation history; earlier disabled-gate
statements describe their original validation point.

## Implemented safeguards

- Reservation checks the exact stored approval, its validity interval, permission
  policy, revocation/use state and current on-disk configuration before committing
  capacity, immutable task/profile/configuration/memory inputs, the task pointer,
  and launch intent atomically. Approval consumption remains a claim-time write;
  admission does not spend the grant. New reservations require schema 13 or newer.
- Launch claims consume exact owner approval. Claim history independently prevents
  replay after any previous claim, including a claimed operation subsequently
  reported as having no effect.
- Generic completion and observation reject caller-written launch confirmations.
  The typed lifecycle receipt service atomically supplies worker identity,
  publishes ownership and changes the attempt to `launching`. Its prepared receipt
  cannot be constructed by deserializing JSON. Generic attempt mutation cannot rewrite a sealed launch's state,
  knowledge binding, reservation, or termination flag.
- Cancellation frees capacity only when the launch was demonstrably never claimed.
  Uncertainty, missing panes, and cancellation requests do not prove termination.
- SIGKILL fixtures cover the uncommitted reservation, committed reservation,
  committed claim, and a simulated external start with a lost acknowledgment.
  Reopening SQLite preserves atomicity, consumed approval, retained capacity and
  refusal to replay. The simulated start is a filesystem marker; this is not live
  Herdr crash certification.
- Receipt commit has separate SIGKILL coverage before and after commit. A lost
  start response can be reconciled using a freshly verified exact worker receipt
  without invoking start again. Recovery preserves pause/revocation, task state
  and capacity; it does not renew execution authority. This receipt version covers
  local non-repository starts; repository/worktree receipts remain separate work.
- A sealed target must be recorded before submission. Receipts must match its
  exact terminal and socket incarnation; a replacement socket at the same path
  cannot be accepted during recovery. Target replay is read-only, target changes
  are rejected, and retained attempts cannot share a staged pane in one store.
- `memory PROJECT attempt-brief --attempt ATTEMPT` renders a complete versioned
  worker prompt without sending it. It uses the attempt's retained instructions,
  task and snapshot, verifies memory bytes, and reports the exact prompt digest
  and character count. It cannot accept replacement knowledge or a larger budget.
- `MemoryStore::create_worker_snapshot` reserves protocol, identity and output-path
  framing before selecting optional memory. Its estimator is
  `char-count-worker-brief-v2`, included in snapshot identity. Older snapshots
  remain readable, but the final worker renderer refuses their obsolete output
  contract and requires a new worker snapshot and approved attempt. The complete
  prompt must also fit its captured envelope.

## Gated resource creation

`canonical_worker::create_resource` now creates a new tab through Herdr 0.9.1
`layout.apply` from an existing sealed reservation for a local, prepared workspace.
It validates the retained full brief, exact configuration/profile/argument digests,
executable identity, route and approval before taking the one-use claim and making
one creation request. It never replaces an existing tab. Unsupported repository,
model, reasoning and environment mappings are rejected before creation. Profile
inspection and resource preparation share definition validation. Preparation also
requires a supported wall deadline, checks the entire retained prompt against the
profile's character-estimated input envelope, and refuses profiles that block
unavailable provider usage. An allowed usage limitation is explicitly retained in
the creation intent. These failures occur before approval consumption or a native
creation request.

The created process is a literal-argv gate inside the bounded Linux supervisor.
The approved executable cannot run until the exact release line arrives; wrong
input or EOF exits. The wall deadline includes time waiting at the gate. Native
pane/process observations establish the exact socket incarnation, terminal and
supervisor identity before recording a version-2 launch target. No start receipt,
ownership or brief delivery is inferred from resource creation. A failed or lost
creation reply retains the original claim and cannot trigger another creation.

Before the request, a durable creation intent records the socket incarnation,
prepared route and command digest. `canonical_worker::reconcile_resource` scans
bounded native pane/process inventories and accepts only one exact live supervised
command in the original socket and workspace. It records a target without changing
the delivery, approval, attempt or ownership. This observation works after claim
expiry, approval revocation or a configuration change; it does not release the gate
or renew authority. Duplicate candidates, changed routes/commands and replacement
sockets are refused. A failed target commit is recoverable through observation.
Creation and recovery compare the stable pane/terminal identity before and after
process observation, refusing replacement during that interval. Once an exact
supervisor has been observed, its pidfds remain held through target commit; an
exit during that interval no longer discards the identity. The termination service
can then confirm quiescence and release capacity without asserting agent start.
Processes that exited before any identity was observed remain uncertain; older
attempts without a pre-effect creation intent cannot use this recovery path.

Version-2 selected targets also support cancellation and process-exit recovery
before a start receipt exists. Confirmed quiescence atomically retires the launch,
releases capacity and clears the task pointer; a failed commit retains capacity
and is recoverable. Selected resources remain in the cross-project inventory even
after this staged termination, until a future explicit cleanup service resolves
them. Termination never silently forgets a pane or certifies artifact preservation.

This service is not yet wired into new-launch controller dispatch. Trusted
profile/preparation ingress, gate-release/start production and workspace creation
now exist. Worktree creation and recovery when no live matching process remains
are still open. Resource recovery
runs through the bounded controller queue and synchronous observation path; neither
path enables launch creation or gate release.

## Gate release boundary

The sealed `record_launch_release` transaction records one input-submission
opportunity for the exact version-2 target, original consumed launch claim and
creation intent. It rechecks current approval, lease, control, configuration,
knowledge, budget and reservation before recording. It refuses any prior release
record, including identical replay after an uncertain input result. A failed commit
records nothing. Release recording alone does not change delivery, attempt or
ownership and does not establish that input arrived or an agent started.

Version-2 gated start receipts now require that exact retained release intent,
with observation times in order. Version-1 historical target handling and exact
replay of previously confirmed receipts remain unchanged.

`SupervisorObservation::waiting_gate` pins and checks the exact canonical shell
child of namespace PID 1. Its live proof fails after the shell execs the agent,
even if the outer supervisor and namespace remain alive. This is process evidence,
not launch authority or a sender.

`canonical_worker::release_gate` now supplies the native sender. It requires an
explicit `execution_home` in the frozen profile and a command that clears inherited
variables before agent execution. The fixed environment contains only HOME, PATH,
LANG, LC_ALL and TERM; credentials stay in the selected owner's credential store.
The home must be a canonical owner-controlled directory outside the project.
The home reference is part of the profile and therefore the signed launch identity;
older profiles remain readable but cannot use the sender without explicit environment
preparation. Arbitrary environment values are not stored in the profile or ledger.

The sender checks retained brief/budgets, exact executable, live waiting gate,
terminal identity and cross-project references, then repeats terminal/process and
approval checks immediately before committing the one-use release record and making
one `pane.send_input` request. A lost reply retains that record and prevents replay.
An OK acknowledgment does not publish ownership or confirm a start. The sender is
not yet wired into new-launch controller admission; trusted profile preparation and
production reservation ingress still need implementation.

`canonical_worker::reconcile_start` now produces a typed start receipt through
observation only. It requires a durable matching release intent, the retained live
supervisor, an exact direct executable and argument digest under namespace PID 1,
and one matching Herdr agent with the deterministic attempt name, exact terminal,
route and no pending launch. It checks the process again after native observation
and cross-project inventory. Script/interpreter indirection and agent argv changes
are deliberately unsupported by this direct-executable observer. It neither renames
agents nor supplies a readiness or prompt-delivery receipt.

The controller's bounded launch-observation path now rotates released unconfirmed
workers alongside uncertain resource creation and termination. Start observation
can confirm an existing process after approval revocation, configuration changes or
claim expiry without renewing authority. Failed receipt commits retain the original
state and can be reobserved; confirmed receipt replay is read-only.

## Concrete brief and termination services

`memory::enqueue_attempt_brief` renders the retained complete prompt and queues only
its digest, character count and immutable identities. `canonical_worker::deliver_brief`
rechecks configuration, approval, knowledge, budget, executable hashes, socket
incarnation, exact supervised worker and readiness before claiming and sending.
The final claim/configuration/knowledge check runs after executable hashing and
request serialization, immediately before native command execution; it is not
reused from before those potentially expensive steps. Only a matching native
acknowledgment moves the attempt to `running`. A lost or
foreign acknowledgment retains the claim and cannot cause a second send. Typed
store recovery can acknowledge an existing delivery without renewing authority;
a native producer for an uncertain prompt receipt is still absent.

`canonical_worker::reconcile_termination` observes or stops the exact recorded Linux
supervisor. Cancellation remains effective while paused or after approval revocation.
Persistent identities bind both pidfs inodes to the boot and observer PID namespace;
recovery refuses unsupported kernels, inaccessible observations and changed views.
New version-2 supervisor identities also retain a domain-separated SHA-256 of
the local `/etc/machine-id`. Reconnect, stop and exit recovery require that host
identity to match, including when the recorded boot differs. Version-1 identities
remain readable and usable in their original boot but cannot prove termination
after a boot change. Missing or changed machine identity retains uncertainty.
This assumes machine IDs are unique to each installation; cloning a machine ID
does not establish a distinct trusted host. It is not remote-host attestation.
Stop recovery reopens each recorded process independently, so an exited namespace
init does not prevent stopping its surviving outer supervisor. Signals use pidfds,
never an unverified numeric PID. Graceful stop uses at most half the remaining
budget (capped at two seconds), leaving time for forced exit observation even when
the original deadline is short. This requires 64-bit Linux with
pidfs and user/PID namespaces, not merely the presence of the helper executables.

Verified quiescence atomically retires outstanding brief obligations, releases
capacity and clears the task's active attempt. Cancellation marks the task cancelled;
unexpected process exit blocks it. Neither establishes task success. Files, panes,
runtime bindings and ownership remain recorded in place; immutable artifact
finalization remains a separate obligation. Failed commits retain capacity and can
be recovered from the persistent process identities after controller restart.

Both synchronous and queued controller paths rotate brief preparation, delivery and
termination work. Confirmed supervised starts without any initial brief operation
produce a preparation hint. A restart re-renders the retained snapshot and queues
exactly one obligation with head/revision and authority checks. Existing operations,
including ambiguous or permanently failed deliveries, prevent replacement hints.
Preparation checks cancellation and the original deadline before publication.
Termination hints are scheduling data, not outbox entries or signal authority.
Root conflict checks now include selected launch targets before their start receipt,
including canonicalized aliases of sockets referenced by legacy threads. Malformed
legacy routing fields block submission rather than becoming empty identities;
missing or invalid thread IDs return errors instead of panicking. Selected-target
retention follows the stored operation-to-attempt relationship before validating
event JSON, so a corrupt payload cannot hide a live resource behind a terminated
attempt. The bounded reader also verifies content-derived attempt/operation IDs,
full retained-input validation and the matching launch operation's task, target,
revision, payload, hash, payload version and idempotency key. Rehashing edited
inputs alone cannot make an inconsistent ownership record pass this scan.

## Work required before enabling dispatch

New resource creation now commits the operation claim, one-use approval consumption
and creation intent in one transaction. A failed intent insert cannot leave a
consumed approval and claimed launch without its recovery identity. The native
request runs only after that transaction returns successfully. SIGKILL coverage
checks both uncommitted boundaries and the committed boundary; trigger failures
verify that no native request escapes rollback. Historical split claims are not
retroactively treated as proof that no resource was created. Exits before live
process observation still require additional evidence and retain capacity.

1. Trusted named-profile preparation: installed executable identities, explicit
   arguments/environment, permission policy and version-bound adapter capabilities.
   Installation version probes alone must not assert launch/readiness/stop support.
   Preparation, native verification, project-bound retention and bounded current
   revalidation now exist. Authenticated live interaction/workflow acceptance is open.
2. Production preparation ingress: repository commit/tree selection, canonical route,
   complete worker snapshot, budget and exact approval scope. No public constructor
   may turn deserialized `FrozenProfile` or `LaunchInputs` into authority.
   `launch draft` and `launch reserve` now provide this ingress and share transactional
   admission checks. Signed fixture acceptance passes; controller connection remains open.
3. Supervised native resource creation and launch: durable effect boundaries for
   worktree/pane creation and agent start; exact typed acknowledgments; persisted
   resource ownership; no blind replay after a lost response. The atomic start
   receipt/recovery store service and gated creation in an existing local workspace
   now exist, with live-target recovery after uncertain creation and direct-executable
   start observation. Native gate submission now exists with an explicit clean
   environment. New workspace creation now has separate retained ownership and
   layout boundaries. Worktree creation, workspace recovery after bootstrap exit, and
   recovery without a previously observed matching process remain open.
4. Add native evidence production for uncertain prompt recovery; never resend it.
   The controller now recovers missing brief preparation after a confirmed start.
5. Extend termination/resource recovery to creation without a recorded target and
   repository worktrees. Recorded version-2 gated targets and confirmed starts now
   have concrete stop/recovery services; retained resources are not finalized artifacts.
6. Integrate preparation/start with bounded controller admission, then certify the
   complete disposable Herdr agent workflow and crash boundaries. Current tests
   establish separate contracts, not the full launch-to-result workflow.

Verified predecessor evidence remains a separate W07 requirement. Tasks with such
dependencies must remain blocked until that producer exists. Nothing in this work
authorizes fallback to legacy thread records or launches in a user's live project.

## Native contract investigation

Inspected upstream v0.9.1, commit `065ef9d6a531c49fb8bee7e818ef837065b21ee9`,
downloaded to `/tmp/herdr-canonical-source`. No upstream source was changed.

- `src/app/agents.rs::start_agent` builds argv from the kind's canonical executable
  name and submits a shell command. It does not take an absolute executable override.
  Agent names are limited to 32 characters; `worker_agent_name` now maps full
  content-addressed attempt IDs to bounded deterministic native names.
- `src/cli/pane.rs::pane_run` joins arguments into shell source and returns a silent
  submission result. It is not an argv execution API or typed start receipt.
  The new POSIX command encoder quotes every argument, including metacharacters.
- `src/app/api/layouts.rs::handle_layout_apply` accepts literal command vectors through
  `layout.apply`. Supplying a workspace and omitting a tab creates a new tab;
  supplying a tab replaces it and is unsuitable for unowned resources. An isolated
  live test verified exact argv, shell metacharacter preservation, supervisor
  observation, persistent-identity reconnect and exit recovery. This removes the
  need to use `pane.run` for exact-executable launch, but does not supply durable
  pre-start target selection or a launch receipt producer by itself.
- `src/pane.rs::shutdown_pane_processes` snapshots session PIDs and signals them
  with bounded grace periods. It does not produce durable quiescence evidence and
  must not by itself release a canonical reservation.

`worker_supervision` constructs a Linux user/PID-namespace command with a required
wall deadline and a five-second forced-stop grace. Detached descendants remain
inside that namespace. This is a command builder, not an execution authority or
an automatic capability certificate. Native admission must still verify helpers,
the installed agent, the terminal's shell/environment and the exact route.

`SupervisorObservation` checks the generated command, executable inode identities,
parent/child relationship and nested namespace PID 1. It retains pidfds for both
processes and an open namespace descriptor through exit and reaping. The live
closure test now exercises this implementation, as well as checking the namespace
has no remaining processes and its detached writer stops. The observer does not
grant stop authority or satisfy artifact preservation, and its handles alone are not
restart-recovery evidence. The new version-2 start receipt additionally retains
boot-, namespace- and pidfs-bound identities used by the concrete termination service.
The OS contract is described in [pid_namespaces(7)](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html)
and [pidfd_open(2)](https://man7.org/linux/man-pages/man2/pidfd_open.2.html).

The native socket transport now applies one absolute deadline to nonblocking
connect, write and read, and bounds both request and reply sizes. It rejects
incomplete or invalid UTF-8 replies and never retries a potentially submitted
request. Tests cover stalled submission, trickling replies, oversized output,
invalid framing and a real temporary socket exchange.

Three opt-in tests passed against the installed Herdr 0.9.1 in temporary homes and
isolated servers: exact process observation/submission/closure, and closure of a
supervised worker with a `setsid` descendant. The latter checks the namespace is
empty and its detached writer has stopped. A library fixture also verifies wall
deadline cleanup independently of controller stop. These establish these specific
contracts, not a completed agent workflow or canonical dispatch certification.


Latest validation (2026-09-20): 329 library tests, 12 memory acceptance tests,
2 contract tests and 9 focused controller tests passed. Three live native contract
tests passed on the installed Herdr 0.9.1 in disposable homes/servers. Native brief
adapter tests use a fixture executable plus real Linux supervisors; they do not
certify an authenticated coding-agent conversation. Logs:
`/tmp/herdr-lifecycle-{library,memory,contracts,controller,live}.log`.

Broader regression run: 504 of 508 binary tests passed. Three cleanup/preservation
tests refused to establish writer quiescence because this host denies inspection
of `/proc/32390/cwd`; the safety check was not weakened. One remote controller
timing scenario failed in the parallel suite and passed when rerun alone. Two
compile-fail doctests also passed. Details are in
`/tmp/herdr-lifecycle-binary.log`, `/tmp/herdr-lifecycle-remote-recheck.log` and
`/tmp/herdr-lifecycle-doctests.log`. The full binary suite is not claimed green.

### Native naming boundary

`canonical_worker::name_started_agent` assigns the deterministic attempt name to
an unnamed native agent after the gate has executed the exact retained executable
and arguments. The request requires the original live claim and current authority,
a matching native route/terminal/kind, an exact supervised process, and a committed
`runtime.launch_name` one-use event. Existing foreign names are refused. The event
does not prove the rename or process start; fresh native and process observations
are required before recording the start receipt.

A lost acknowledgment leaves the naming intent intact. `reconcile_start` can
observe the expected name without renewed authority, but neither recovery nor the
explicit naming service repeats an uncertain rename. An already-confirmed launch
now reconciles its receipt directly instead of re-entering creation recovery.
Fixture tests cover naming success, lost reply, foreign-name refusal, failed event
commit before submission, and refusal to retry when the name disappears. Live
vendor-agent naming and controller admission remain pending.

### Bounded launch advancement

`canonical_worker::advance_launch` connects the concrete creation, gate release,
naming and start observation services for an explicitly selected retained launch
operation and delivery revision. It validates the explicit execution home before
creating a resource, then revalidates it before release. It shares one deadline and cancellation signal
across stages. It creates only a never-claimed delivery, expects precisely the
revision produced by that claim, and recovers later stages from durable events.
Uncertain creation is discovered rather than repeated; unknown absence retains
capacity. Recorded release and naming requests are never repeated. Expired claims
and uncertain naming take the observation-only start path. Each effect service
revalidates authority and identity immediately before its own durable boundary.

A complete fixture workflow now exercises this service, initial brief preparation
and delivery, cancellation, confirmed process termination, capacity release and
artifact preservation. Lost acknowledgments at creation, release and naming all
recover with exactly one request per boundary. This does not enable automatic
launch admission or replace the pending live vendor-agent acceptance tests.

### Direct-launch readiness

Herdr 0.9.1's `interactive_ready` describes its managed-launch phase and is not
set for the canonical literal-command path. Initial brief delivery instead
requires a positive visible idle rule from the pinned binary's bundled detector
through `agent.explain`, together with exact native agent identity and idle state.
Fallback idle, blocked/busy screens, skipped detection, external detector sources
and warnings refuse delivery. The adapter checks identity again after detection
and repeats readiness immediately before submission. A failure after claiming
retains the one-use claim rather than opening a retry opportunity.

See [the live readiness review](reviews/2026-09-21-native-readiness.md) for the
verified Codex startup/naming/termination contract and remaining certification gap.

### Creating a worker workspace

New launches without a workspace use `workspace.create_command` from the locally
validated Herdr compatibility patch in `patches/herdr/`. The first pane itself
runs the bounded, gated PID-namespace supervisor. The adapter never falls back
to shell creation when this method is unavailable. The installed stock Herdr
binary has not been replaced, and production capability certification remains
required before admission.

Version 2 `runtime.launch_creation` intents distinguish this path from historical
bootstrap launches. The exact command digest and server session are retained
before submission. A native root-pane response plus exact live supervisor
observation establishes the target. `runtime.launch_workspace` (target version 2)
and `runtime.launch_target` commit atomically with the same payload; no separate
layout request is made. Lost replies or commit failures are recovered by bounded
workspace/pane/process discovery, exact argv digest and OS process identity, and
terminal incarnation checks. Labels only narrow discovery. Duplicate or changed
layouts fail closed. Observation may recover ownership after revocation/expiry,
but cannot renew authority or release the gate. Missing process evidence remains
uncertain and retains capacity.

The existing-workspace route still creates a gated tab with `layout.apply`.
Repository/worktree provisioning and resource cleanup remain open. Workspace
ownership survives worker termination and does not authorize deletion.

### Historical bootstrap workspace recovery

Historical version 1 creation used
one explicit `workspace.create` request before creating the gated worker tab.
The native response and a fresh pane observation establish the workspace ID and
bootstrap terminal identity. `runtime.launch_workspace` retains that resource
receipt. A separate one-use `runtime.launch_layout` event precedes `layout.apply`;
the layout always specifies the exact workspace and never defaults to the active
workspace or replaces an existing tab.

If the workspace receipt commits but the layout boundary does not,
`continue_workspace_layout` (also used by `advance_launch`) can continue under the
original live claim after rechecking configuration, command, socket, and bootstrap
identity. A lost layout reply uses exact command/process discovery within the
recorded workspace. Neither workspace nor layout requests are blindly replayed.
Loss of the workspace reply or failure to commit its receipt retains uncertainty
and capacity until the exact live bootstrap marker can be observed. A missing,
exited or unverifiable bootstrap remains uncertain; labels alone do not suffice.

Workspace bootstrap receipts participate in bounded cross-project resource
inventory and remain after worker termination. They do not assert that the
bootstrap shell stopped, authorize workspace deletion, or release artifact
preservation obligations. Cleanup must separately prove bootstrap quiescence.
Both staged-stop and started-worker termination transactions refuse to release
capacity when historical root-creation or bootstrap quiescence is unresolved. A
cancellation may stop the exactly identified worker, but its stop alone cannot
mark the attempt terminated or free its reservation. Repeating reconciliation
keeps canonical state unchanged and reports unresolved bootstrap quiescence.
This restriction does not apply to current supervised-root creation or launches
in an existing workspace that did not create a bootstrap shell for the attempt.

For version-2 host-bound supervisor records, local observation of a different
boot on the same recorded host can resolve historical bootstrap quiescence.
The termination receipt retains `host_reboot` evidence with the host hash and
both boot IDs. The store verifies this evidence against the exact retained
supervisor before atomically releasing capacity. Same-boot worker exit, missing
host identity, a different host, or historical version-1 supervisor records do
not establish bootstrap termination. A bootstrap-only creation without a worker
supervisor receipt also remains unresolved. Recovery retains files and resource
references; it neither deletes a workspace nor certifies artifact preservation.
Repository/worktree provisioning is still unsupported by this creation service.

### Recovering a historical bootstrap acknowledgment

Historical marked workspace creation intents contain a random 256-bit marker, committed before
submission and passed as `HP_WORKSPACE_CREATION` only to the bootstrap terminal.
The actual worker still executes with its isolated environment. Recovery examines
a bounded native workspace/pane/process inventory on the pinned server session.
The expected label narrows discovery; a live process must carry the exact marker,
have the expected owner and working directory, and remain pinned by a pidfd while
the native terminal identity is rechecked. Environment contents are not returned,
logged or stored by the observer. Duplicate or altered workspace layouts refuse.

The observation-only transaction retains the bootstrap resource under the original
consumed launch claim, including after expiry or approval revocation. It neither
renews authority nor creates a layout. Continuing the acknowledged workspace still
requires the original live claim and current policy/configuration checks. Missing
markers (including historical intents), bootstrap exit before observation, or
unavailable process evidence continue to retain uncertainty and capacity.

The disposable real-Herdr `live_workspace_bootstrap_marker_contract` verifies marker
propagation, wrong-marker refusal and invalidation after workspace closure. Fixture
regressions additionally cover expired/revoked claims, label-only refusal, duplicate
inventory and receipt-commit rollback without any workspace recreation.

### Native adapter integration evidence

`live_canonical_supervised_root_creation_release_and_termination` runs the actual
canonical services against a disposable patched Herdr server: new root creation,
atomic SQLite workspace/target ownership, exact gate release, process observation
and staged cancellation/termination. It uses a sleep process and fixture approval
and capability records; it is not vendor workflow or production admission evidence.
This test found and fixed the adapter's incorrect `pane.info` call: the Herdr JSON
method is `pane.get`. Fixture dispatch now uses that same method.


### Connected-server creation capability

Before a fresh direct-root launch consumes approval, the adapter sends a bounded
read-only `ping` to the exact socket incarnation. It requires the expected Herdr
version and boolean `capabilities.workspace_create_command: true`. Repository
launches perform this admission before the worktree claim and repeat it before
native creation. Missing, false, malformed or version-mismatched advertisements
refuse creation without consuming the approval or creating checkout paths.

The local compatibility patch advertises the capability. The earlier patch build
without advertisement is deliberately refused for fresh workspace creation.
Recovery of already-recorded native resources still uses their retained identities;
this check does not retroactively remove resources or replay requests. Existing
workspace launches retain their `layout.apply` path. The advertisement establishes
API availability, not agent workflow certification.


### Prepared-launch controller wiring (production enabled)

`read_controller_dispatch_hint` optionally includes already-reserved launch
operations in the bounded controller rotation. Due, unclaimed launches and live
original claims can advance; cancelled attempts, expired claims, paused projects
and projects requiring reconciliation cannot receive launch-effect hints.
Recovery-only hints continue independently. A launch qualifying for both recovery
and advancement appears once in the rotation. Selection never reserves a task,
consumes an approval, reconstructs a profile or changes canonical state.

The foreground controller routes advancement to `advance_launch`; its executor
uses a separate typed launch action with the original queue deadline and
cancellation token. Worker ingress still validates current signatures, profile,
repository/memory inputs, capacity and every one-use boundary. Launch job failures
back off one second rather than the generic thirty seconds, allowing lost-reply
observation within the original claim lease. This does not renew that lease or
permit effect replay. Other projects retain their admission turns.

`PREPARED_LAUNCH_DISPATCH_ENABLED` is true following the authorized live controller
workflow. Ordinary production polling offers prepared launches alongside
observation, brief and termination work. Current capability evidence, exact signed
approval, capacity and retained inputs remain mandatory. See the current
[enablement audit](dispatch-enablement.md); historical entries below describe the
evidence as it accumulated and do not certify optional agent protocols.


### Creation replies after authority changes

After native creation, storing an exact observed supervisor uses observation
admission rather than requiring a still-live effect claim. The adapter obtains
the current delivery revision/head and retains the typed observation against the
original creation intent and consumed approval. Expiry, revocation or cancellation
can therefore block gate release without discarding the identity needed to stop
the already-created namespace. This write does not renew a claim or consume
another approval. A changed binding or released reservation still rejects it.

The case where a supervisor exits before any exact observation remains unresolved;
an absent pane or process list is not accepted as termination evidence.


### Attempt output and preservation contract

Version 2 worker briefs include `output_directory`, deterministically derived as
`<project>/.state/worker-output/<attempt-id>`. Workers create that directory when
needed, write `report.md` there, and place supporting files under `library/`.
This keeps reports separate across concurrent attempts and from repository source
files. Start records the same directory in the runtime binding's artifact source.
The preview and retained rendering use the same path and include it in prompt
budget/hash accounting. Worker snapshots now use `char-count-worker-brief-v2`,
reserving exact path/instruction framing before optional memory selection. Older
snapshot contracts require a new snapshot and approved attempt.

After exact worker termination, explicit finalization uses receipt-bound output
snapshots when available, or the recorded local source for historical bindings
without snapshot evidence, through the bounded, verified artifact pipeline. Cancelled tasks
remain cancelled, with no task revision or disposition change; successful capture
adds preservation evidence only. Other eligible tasks still become awaiting-review.
This captures report/library bytes. Repository-state snapshots described below
cover checkout changes; worktrees and branches remain retained until separate
cleanup acceptance requirements have been satisfied.

Native termination now preserves repository state before committing either a
staged stop or started-worker termination. The receipt binds every approved
checkout to its retained manifest. Capture or receipt-commit failure keeps the
attempt nonterminal and capacity reserved, while retry observes the already
stopped worker and reuses verified snapshots. The same boundary now captures the
attempt output directory into `.state/worker-output-snapshots/<attempt>/<digest>`.
The stop receipt records either its manifest digest or an explicit observation
that the source directory was absent. An existing empty directory has a manifest;
it is distinct from an absent source. Partial reports, binary library files,
ordinary `.git` output data, executable bits and empty directories are retained.

Output capture uses bounded descriptor-based traversal, two byte scans, durable
content-addressed writes and readback, with the manifest published last. Unsafe
source/destination paths and publication failures keep capacity reserved for
retry. Foreground and queued finalization verify the receipt-bound manifest and every
referenced blob, then publish report/library bytes through the established
artifact receipt pipeline. They work after the original output directory is lost
and do not recreate it. Corrupt or absent recorded outputs fail closed; they do
not fall back to new source bytes. Historical bindings without output receipts
retain their existing local-source path. Neither snapshot format authorizes
source removal or proves task success.


### Enabled controller effect-path acceptance

Run `scripts/test-canonical-dispatch` on Linux to build both test executables and
exercise the controller's normal effect-selection, queue and executor path with
launch selection enabled only inside the test. Cargo runs locked and offline;
its cache must already contain the dependencies. `/usr/bin/cc` compiles the small
fixture process; Git and ssh-keygen are also required. Disposable sockets and Linux
process namespaces must be permitted. The installed CLI and production dispatch
gate are unchanged.

The library fixture generates a disposable Ed25519 owner key, pins its public key
in the project authority configuration, and installs launch grants through the
production signature importer. A tampered grant must be refused without advancing
the project head or installing approval. Installation preparation, retained-report
revalidation, launch drafting and reservation all use their production APIs. A
changed Herdr executable after signing must prevent reservation without changing
state. The fixture retains repository inputs and supervised test processes while
a separate binary-test process drives the controller. Two
projects launch, receive the exact retained brief once, then stop and preserve
repository/output evidence through a fresh queue. A lost naming acknowledgment
must allow another project's admission before retry and must not repeat creation,
gate input or naming. A third launch is cancelled after queue offer but before
admission; it must not consume approval or create native resources. The disabled
production selector is separately checked to leave fresh launches untouched.

The script also runs the real ticker body with its production background services
against three projects sharing one root, using production polling delays. It
requires positive Herdr/Git observations, brief delivery, a full executor restart,
and cancellation with verified preserved outputs. Exclusive worker effects and
shared-root observation batches alternate so maintenance cannot continually block
launch preparation. Active canonical work uses a 250 ms service interval;
successful termination and observation-only recovery checks cool down for 15
seconds, and idle polling retains
its usual interval. These are scheduling hints; workers still enforce ownership
and claim validity at execution ingress.

Both tests still supply synthetic native capability evidence at an explicit,
compile-time test boundary. The retained profile is not vendor-certified, and the
fixture Herdr protocol does not prove real vendor readiness or prompt acceptance.
The production preparation/draft/reservation-to-ticker chain is covered with those
fixtures; the equivalent live vendor workflow remains required before enabling
the production gate.

### Creation without a recorded process identity

`herdr-projects reconcile <slug> --plan` now reports `inspect_launch_identity`
for retained attempts whose native creation intent exists but whose exact target
or started process was never recorded. It also identifies the affected runtime
binding instead of describing it as an unused binding. A still-live original
claim keeps `wait` advice for the operation; once that claim expires, the report
requires inspection of creation evidence rather than giving generic expiry advice.

The report is read-only. An empty process inventory is not proof that all owned
processes stopped, and neither expiry nor cancellation authorizes recreation or
capacity release. Recovery without any durable process identity remains blocked;
this diagnostic does not provide the missing termination evidence.

Observation-only recovery uses a separate volatile queue identity from active
launch advancement and cools down for 15 seconds after success. Repeated empty
inventories therefore do not monopolize exclusive root access or keep the ticker
at its fast interval. A live original claim can still advance using its own queue
identity and one-second failure backoff. Both current supervised-root creation and
historical tab creation are tested for exit before process identity observation.

### Legacy cleanup beside canonical projects

With `state-store` enabled, legacy worktree cleanup now reads the bounded root
identity inventory, including canonical bindings, launch targets and retained
worktree plans. An unrelated canonical project no longer causes blanket refusal.
Any overlapping local worktree, working-directory or output-directory reference
blocks removal, including missing descendants beneath existing aliases. Dangling
aliases, malformed project metadata and incomplete inventory also block removal.
The check runs under the root execution lease and at the cleanup ingress before
Git commands or a removal reservation.

This does not authorize deleting canonical worktrees. Legacy cleanup still
requires its verified preservation snapshot, writer checkpoint, current Git
registration and non-force removal. Retained canonical references continue to
protect resources after an attempt has stopped.

Reopening an intentionally removed legacy worktree uses the same root inventory.
The target may be absent: existing ancestors are resolved before comparing paths.
A canonical reference acquired after removal blocks worktree recreation, including
references through aliases or to not-yet-created descendants. An unrelated
canonical project permits reopening the retained branch. Conflict refusal leaves
the removal record, Git registration and preservation snapshot unchanged.

### Live native preservation after source loss

The two `live_canonical_*` library tests now verify retained evidence after actual
Herdr supervised creation, gate release and namespace termination. They write a
report and binary library output, stop the worker, delete only the disposable
output source, and load the exact bytes from the receipt-bound output snapshot.
The repository variant also removes its disposable source report and checks the
retained working-file blob, repository manifest identity and Git-pack digest.

Run these live contracts with an optimized build, `state-store`, and
`HP_LIVE_HERDR` pointing to the capability-enabled Herdr binary:

```
cargo test --release --locked --offline --features state-store --lib \
  canonical_worker::tests::live_canonical_ -- --ignored --nocapture --test-threads=1
```

Operation deadlines match the production controller's 45-second limit; the
original 30-second launch claim and bounded termination are unchanged. On this
machine the unoptimized repository fixture exhausted its claim budget; both
optimized cases passed in approximately seven seconds total. These tests use
isolated homes and sleep processes, not authenticated vendor agents, and do not
certify vendor readiness, prompt completion or the complete worker workflow.

The same live-test filter also runs
`live_canonical_lost_creation_and_gate_replies_preserve_one_use`. Its transparent
CLI proxy forwards each request to the real server, then drops one successful
`workspace.create_command` or `pane.send_input` reply. Creation recovery must
observe the existing exact supervisor; creation cannot be replayed. A lost gate
reply retains the one-use release record and a second release attempt must refuse.
Both cases prove exact process execution and safe cancellation, and run the same
receipt-bound source-loss preservation checks. Only method names are logged by
the proxy; no authenticated vendor prompt is involved. This is acknowledgment-loss
acceptance, not a substitute for controller/host-crash or vendor workflow tests.

`live_canonical_caller_crash_after_creation_and_gate_input` runs each effect in a
separate test process. After the real server returns success, the reply proxy holds
the response while the parent sends SIGKILL and reaps that caller. Only then is
the proxy released. The test waits for inherited root-lock cleanup before opening
the durable state for recovery. Both creation and gate-input cases retain exactly
one request and one durable intent, refuse replay, and pass exact-process stop and
source-loss preservation checks. This covers caller crashes at those two native
effect boundaries; it does not claim host reboot, Herdr restart, or authenticated
vendor workflow acceptance.

`live_canonical_server_restart_preserves_old_outputs_and_new_worker` kills and
reaps the disposable Herdr server, then starts a replacement at the same socket
path. A new supervised worker deliberately receives the old pane ID. Its session
birth identity and supervisor identity must differ. Cancelling/reconciling the old
attempt must settle only the old supervisor, preserve its outputs and leave the
replacement session/worker alive. Repeating reconciliation must leave the snapshot
unchanged. The test then explicitly stops its replacement worker for teardown.
This supplies live Herdr-restart evidence for staged canonical attempts; it does
not establish authenticated vendor readiness or started-worker workflow recovery.

### Live exit before recorded launch identity

`live_canonical_unobserved_exit_keeps_capacity_and_refuses_replay` forwards a
real `workspace.create_command` request and drops its successful reply. Only the
disposable harness retains that request/reply to locate and stop the exact native
supervisor. Its identity is never supplied to the production store. Recovery
then sees the exited worker without a recorded launch target.

Repeated creation is refused, observation cannot reconstruct an identity, and
termination refuses the missing staged target. The same checks run after explicit
cancellation. Each pass must leave the full store snapshot unchanged, keep capacity
reserved, retain one consumed approval/creation intent, send no gate input and
preserve the approved checkout bytes. This covers externally induced native exit
before durable identity capture; it does not certify a spontaneous vendor crash
or authorize automatic cleanup of ambiguous resources.

All six optimized live canonical contracts passed together on 2026-09-22
(21.26 seconds), with the same documented capability-enabled Herdr binary and no
credentials. Production dispatch remains disabled pending the remaining audit.

### Live historical bootstrap refusal

`live_canonical_historical_bootstrap_blocks_release_after_worker_stop` uses the
existing test-only historical creation ingress against a real disposable Herdr
server. It recreates the non-repository bootstrap-plus-worker layout, releases
the gate and observes the exact worker executable. After cancellation, production
termination stops the supervised worker but refuses capacity release because the
bootstrap has no durable descendant-quiescence evidence. The test independently
identifies that still-live bootstrap by its creation marker and working directory.

Two termination attempts leave the full store unchanged, capacity retained and
report/library bytes intact. No cleanup or successful preservation receipt is
fabricated. This validates the visible blocked disposition permitted by T03.4;
it does not add automatic historical cleanup. Current production creation uses
one supervised root and cannot create this historical bootstrap layout.

All seven optimized native contracts passed together on 2026-09-22 (25.19 seconds).
No authenticated vendor prompt or real host reboot was involved.

### Authorized interaction diagnostic

On 2026-09-22 the explicitly authorized isolated Codex 0.154.0 diagnostic passed
(16.99 seconds). It copied the login into a disposable home, trusted only the
exact disposable work directory, kept the read-only sandbox, submitted the fixed
no-tools token prompt once, verified the native acknowledgment and supervised
termination, and asserted removal of the temporary home/login copy. It did not
wait for a token reply and does not establish task or memory protocol behavior.

Initial attempts failed before prompt submission: extra temporary-path nesting
caused workspace creation failure; the original shorter layout worked, but the
broad `/tmp` trust entry did not establish trust for the specific working directory.
The fixture now provisions that exact path before calling the shared verifier.
Production verification does not change the user's trust configuration. API errors
now identify the failed method without exposing output; readiness timeouts retain
the last validation error. Test-only screen diagnostics emit fixed categories,
not terminal contents or credentials.
