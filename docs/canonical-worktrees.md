# Canonical worktree preparation

Linux library service `worktree_preparation::prepare` provisions repository
resources for a reserved, owner-approved launch. The native launch adapter now
continues that original claim into resource creation, gate release and start.
It maps the selected source working directory into its approved checkout and
retains the primary worktree incarnation in runtime ownership. `launch draft` and
`launch reserve` expose the reviewed preparation/admission path; enabled canonical
dispatch invokes worktree preparation before native resource creation. See the
[dispatch audit](dispatch-enablement.md) for the validated scope.

The complete budgeted worker brief includes all checkout paths and branches.
Creation and release verify pristine approved bytes; start pins directory and
Git associations while allowing legitimate worker changes after release.
The linked checkout's HEAD must still name its approved branch; switching to
another branch or detaching HEAD invalidates that proof. New commits on the
approved branch remain valid after release.
Neither continuation nor lost-reply recovery consumes a second approval.
Metadata and checkout files are opened without following symlinks and with
nonblocking semantics, then checked as single-link regular files by descriptor.
A named-pipe replacement cannot stall the reader while it holds root ownership.
Working directories absent from the approved tree are refused before claiming.
Termination preserves checkout files and retained references; comprehensive
artifact acceptance and safe cleanup remain separate unfinished work.

The service derives one path and branch per approved repository from the actual
attempt ID. Callers cannot supply a branch or output directory. Plans appear in
the launch draft for review. Source HEAD movement and dirty source files do not
replace the approved commit/tree; the worker receives a new linked checkout.

Before consuming approval, preparation validates the full retained brief, profile
configuration, executable bytes, execution home, repository objects, empty output
paths and absent branch names. It refuses partial clones, configured filters,
submodules, duplicate common repositories and noncanonical paths. Object reads,
tree/file inventories, subprocess output and deadlines are bounded. Tree paths
must be UTF-8; file bodies can be binary. Checkout transformations that do not
match the approved blob bytes are not accepted as ready worktrees.

Approval consumption, the one-use launch claim and `runtime.worktrees_creation`
commit together before creating directories or running Git. Git hooks, filesystem
monitors, automatic maintenance, replacement refs and lazy fetching are disabled;
no submodule update or network fetch runs. A random creation token is retained
in the intent, local marker and Git worktree lock reason. Git fsync is requested.
The inherited root barrier remains held while supervised Git processes clean up.

Observation checks both directions of the linked-worktree association, common
repository, lock token, branch and approved base. It compares actual regular-file
and symlink bytes against bounded `ls-tree`/`cat-file` output, checks executable
bits, and rejects unexpected files. Git's stat cache and assume-unchanged flags
cannot substitute for those byte checks. Directory descriptors remain pinned
through receipt insertion; filesystem incarnation identities are retained with
`runtime.worktrees_ready`.

A repeat call performs observation only. A lost receipt commit can be recovered
after approval revocation without another `git worktree add`. Missing, partial,
modified or replaced resources retain uncertainty and worker capacity. This
service never removes worktrees, branches, dirty source files or partial results.
The receipts do not certify task completion or artifact preservation.

The bounded root ownership inventory includes every creation intent, even before
Git returns a receipt. It validates content-addressed launch inputs, operation and
claim provenance, consumed approval, deterministic plans and any ready receipts.
Corrupt, orphaned, duplicate or over-budget records refuse inventory collection.
Cancellation, termination or missing filesystem paths do not implicitly release
these references. Adoption includes the staged references in its conflict checks;
new worktree provisioning checks canonical and legacy references before consuming
approval. Equal, parent and child paths conflict, including across projects under
the same root. This inventory is conflict evidence, not permission to remove data.

## Preparation cancellation and expiry

The termination adapter can retire a worktree-only launch after cancellation or
claim expiry. It first reacquires the exclusive root barrier inherited by Git
supervisors, establishing that preparation has stopped. It requires the original
worktree intent, retained approval use and reservation, and refuses this path if
any native launch intent or resource event exists. The controller offers these
attempts as bounded termination jobs without enabling new-launch dispatch.

After those checks, recoverable checkouts are captured with their working files,
approved/current Git history and index state. Capture can recover a partial or
modified checkout without declaring it ready to launch: the creation token,
branch and both Git associations must still match. Existing ready receipts also
require the original directory incarnations. A missing ready checkout, or a
missing path with retained branch/registration, blocks retirement. Plans without
ready receipts can record absence only after checking directory, branch and Git
registration twice. Attempt outputs are captured or recorded absent as well.

Delivery retirement, attempt/task state changes and version-2
`runtime.worktrees_stopped` commit together with ordered snapshot/absence
references and output evidence. Capture or database failure retains capacity and
allows verified retry. Late continuation is fenced; approval is never refunded.
All ownership references remain retained. No checkout or branch is deleted, and
the receipt does not certify artifact acceptance. A live, uncancelled preparation
is left unchanged and produces no snapshots.

Ownership inventory accepts the single epoch advance recorded by lease expiry,
while still requiring the original consumed claim and expiry-event provenance.

## Working-file preservation

`worktree_preservation::capture_stopped_files` explicitly captures checkout files
for an exactly terminated attempt under the exclusive root barrier. It checks
the requested head, retained plans and checkout incarnations before reading.
All repositories share a 50 MiB/10,000-entry source budget, the original deadline,
and cancellation. Reads use pinned directory descriptors and never follow links.
Repository snapshots preserve symlinks as typed entries containing their literal
target bytes, read through pinned link descriptors without following the target.
Manifests containing links use version 3; older regular-file manifests retain
their version. Hard links and special nodes remain refused, leaving the source
in place. Attempt-output snapshots continue to reject symlinks. It records binary bytes, executable bits and empty
directories, including untracked and ignored files.

Two byte-based scans must agree before publication. Files are retained by SHA-256
under `.state/worktree-file-snapshots/<attempt>/<manifest-digest>/`, synced and
read back before `manifest.json` is published as the completion marker. Repeating
capture verifies existing bytes; conflicts are refused rather than overwritten.
Partial directories and interrupted writes remain bounded retained evidence,
never grounds for deleting a source or implicitly reclaiming an older snapshot.

The file-only manifest explicitly declares scope `working_files`. It excludes
the root `.git` association and carries no Git-history/index evidence. Capture
makes no task transition or canonical acceptance event and grants no cleanup
permission. The file-only entry point is separate from the repository-state
capture used by native termination below.

## Repository state preservation

`capture_stopped_repository` adds version-2 `repository_state` manifests in the
same snapshot store. Each includes current HEAD, the approved base, logical index
entries/stages, exact index bytes, any referenced shared-index file, and an object
pack containing history reachable from HEAD/base plus all indexed objects. This
keeps staged-only content distinct from working-file bytes. Git's optional locks,
hooks, automatic maintenance, replacement objects and lazy fetching are disabled;
commands inherit the root execution barrier and original deadline. Index/ref
observations must agree after packing, after the working-file verification pass,
and immediately before manifest publication.

Git data has a separate 50 MiB budget shared across the attempt's repositories;
individual index files and inventories are bounded at 4 MiB and logical entries
at 10,000 per repository. Packs are produced without reuse/deltas and with one
packing thread. Unsupported index objects (including submodules), oversized or
changing sources refuse capture while retaining original resources. Repository
configuration, unrelated refs, reflogs, and unreachable objects outside the
selected roots are not included. This remains preservation evidence, not a
verified task result or authorization to remove the original repository.

Disposable restoration tests import packs into independent repositories after
removing the source object databases. They verify approved/current commits,
staged binary content, split indexes, merge-conflict stages, SHA-256 repositories,
intent-to-add entries and index flags. Restore is tested explicitly; the service
does not automatically replace a user's checkout or Git metadata.

## Preservation before native termination

Staged and started native workers now capture repository state after exact
supervisor/workspace quiescence and before the terminal-state transaction. The
trusted adapter retains the root barrier across capture and commit. Stop receipts
carry the ordered `repository_snapshots` vector; the store requires every approved
plan and a manifest digest before releasing capacity. Missing references, changed
sources, unsupported entries, publication errors, or failed commits retain the
reservation and desired cancellation for retry. Repeated capture verifies existing
bytes, so a lost database commit does not require deleting or replacing a snapshot.

Signalling/exit observation keeps its ten-second cap. The complete termination
job, including capture, uses at most the original 45-second admission budget and
never extends a shorter caller deadline. Current controller jobs use that budget.
The same transaction requires output evidence: a verified snapshot of the attempt
output directory, including partial report/library files, or an explicit absent
source observation. Empty directories are captured distinctly. Output capture
failure retains capacity just like repository capture failure.

Worktree-only preparation retirement uses the same repository/output capture
boundary, with explicit absence evidence for plans without created resources.
Unrecoverable partial metadata or missing recorded resources remain blocked.
Report/library finalization can consume verified native-stop snapshots after
output source loss. Repository restoration remains explicitly tested rather than
an automatic mutation path. Cleanup acceptance remains unfinished.
