# Verify a retained result

On Linux with the `state-store` feature, an operator can run the independently
isolated verifier through the CLI after a result has been submitted:

```sh
herdr-projects --root /path/to/projects result demo verify SUBMISSION_ID \
  --policy-id builds \
  --policy-file /path/to/policy.json \
  --idempotency-key verify-submission-1 \
  --work-dir /path/to/new-verification-scratch \
  --timeout-seconds 60
```

The policy file must match the acceptance policy text in the installed signed
contract byte-for-byte, including whitespace. The command cannot substitute a
worker's claimed checks for that policy. Policy input must be a regular,
non-symlink file of at most 4,000 bytes. The check subprocess timeout is 1–300
seconds; retained-object preparation has its existing separate bounds.

The scratch path must be absolute, must not exist, and must have an existing
parent. The command creates it with private permissions and removes its scratch
contents when finished. An existing path is refused without removing its files.
The project store must already be migrated; this command does not upgrade it.

Successful verification prints a JSON outcome with `state: "accepted"` and a
trusted receipt. A recorded rejection prints its JSON outcome and exits nonzero.
Policy mismatch records a rejection without running the substituted policy.
Input/setup errors can fail before an outcome is recorded. Submission by itself
still does not verify work, satisfy prerequisites, or release attempt capacity.

Retrying the same idempotency key and exact inputs returns the historical run
with `replayed: true`; it does not execute the checks or mint a second receipt.
The replay display does not reconstruct a trusted receipt from JSON. A replay also does not certify historical results against checks added after their original run. Changed
inputs with the same key conflict. Reserved attempts remain bound to their frozen
contract revision, including after a newer ordinary contract is installed.

Contracts with explicit path scopes also constrain the candidate's changed paths.
Before running checks, verification compares the signed base and candidate trees.
A changed path must match a literal write path or a declared directory prefix.
Both names of a rename are checked; read scopes and glob patterns do not grant
write permission. An unchanged file outside scope is allowed. Historical
contracts with no path declarations retain their existing semantics; version-3
output declarations require write scope.

A proven violation records `scope_violation`, verifier feedback and cancellation
in one transaction. Existing cancellation logic retains capacity unless it proves
launch never started. Failure to write the stop request rolls back the rejection
and feedback too. Feedback enters the existing pending replan queue; an operator
can use `feedback SLUG replan FEEDBACK_ID`, or the opt-in bounded replan service
can process it. Neither route by itself launches a planner model or worker.

Missing base objects, failed Git comparison or an over-limit diff instead reject
with `scope_diff_unavailable`; no verification receipt is minted. Diff capture is
bounded by the runner and limited to 10,000 changed paths. These are checks of
retained changes, not a worker filesystem sandbox or pre-write containment.

This is operator-driven production ingress. Automatic verifier dispatch and full
planner-driven decomposition remain unfinished. Local E2E coverage uses real
signatures, Git objects and verifier subprocesses with synthetic runtime fixtures;
it does not certify a live model adapter.

Receipt reuse for every contract requires version 2 of the native verifier's
contract-check record, attesting post-execution checkout identity. Older accepted receipts remain
historical facts, but cannot satisfy new dependencies, authorize integration or
release a barrier without that proof. Migration and exact replay do not invent
proof. Run verification with a new idempotency key to perform current checks and
issue fresh evidence. This also applies to contracts without scope/output
declarations. This record is separate from the Linux isolation version.

## Local integration commands

The Linux `state-store` binary exposes operator-driven local integration:
Scheduler inspection reports this capability as `operator_local`; it does not
claim automatic dependency producers or automatic integration scheduling.

```sh
herdr-projects result PROJECT configure-integration \
  --repository /absolute/repo --reference refs/heads/factory-integration
herdr-projects result PROJECT integrate VERIFIED_RESULT_ID \
  --repository /absolute/repo --idempotency-key integration-1 \
  --work-dir /absolute/new-integration-scratch
herdr-projects result PROJECT reconcile-integration \
  --repository /absolute/repo --idempotency-key integration-1
```

Configuration requires an existing branch that is not checked out. The target
is immutable after configuration; repeating the same configuration is safe.
The signed contract must route the result to `verify_then_integrate`. Integration
uses the stored policy and native receipt, checks the combined candidate tree,
and updates only the configured local ref with an expected-old-OID comparison.
Both SHA-1 and SHA-256 scratch repositories preserve the source object format.
These commands neither push a remote ref nor launch workers.

The integration scratch directory must be absolute, new, and have an existing
parent. The command creates it privately and removes it on completion or error.
An existing directory is refused without changing its contents. Stores must
already be explicitly upgraded. Commands print the integration outcome as JSON;
only an `integrated` outcome returns success.

Retry the same result and key to resume a retained candidate after interruption.
Reconciliation uses the stored operation and existing Git evidence without
building another candidate. It can confirm an already published commit or retry
a previously checked candidate under the existing fences; it is not read-only.
Ambiguous target changes remain blocked for reconciliation. The E2E injects a
failed check-state write and a failed receipt write after Git CAS, then proves
resume, confirmation, replay and exactly one integration receipt.

Automatic integration scheduling and the live dependent-worker acceptance gate
remain unfinished. This operator workflow is covered with local disposable Git
repositories, not live provider certification.

Integration also checks the exact merged commit for every required output in the
signed contract. A passing worker candidate or policy command does not establish
that a target-side merge preserved those files. Only regular Git files satisfy
`git_file`; missing entries, directories, symlinks and symlink ancestors fail.
The check runs before ref publication and during pending-operation reconciliation,
including candidates with previously stored passing policy results. A missing
output records `required_output_missing`, blocks publication and preserves worker
capacity. If that candidate has already been published, it requires reconciliation
instead of receiving an integration receipt. Completed historical records are
not retroactively certified by replay.

A completed integration receipt used for a required-output contract also needs
a stored native merged-output check record. Dependency attachment, current
dependency readiness and barrier validation refuse historical receipts lacking
that record. Migration and replay retain history without manufacturing proof.
To obtain fresh evidence, integrate the verified result with a new idempotency
key; the integrator rechecks the current combined tree. A new receipt may then
replace the old dependency evidence. Contracts with no required outputs retain
their prior compatibility behavior.

When a dependency is attached after verification, a newer unusable receipt no
longer hides an older valid receipt for the selected attempt and contract.
Attachment checks candidates under one shared two-second SQL/input budget and
rolls back its queue or contract transaction if that budget expires. It updates
only the consumer being attached. Queue mutation reads only the selected task, its retained-capacity status and
bounded graph identities/edges. Its existing publication checks, graph work and
receipt attachment share the original command deadline. Historical integrated
receipt selection and broad queue reporting still need further scaling work;
this deadline is not a claim that all queue operations are history-independent.

The native verifier now rechecks HEAD, the Git index and tracked worktree after
checks and child cleanup, before accepting a fresh result. A check that changes
those inputs is rejected with `tampered_tree`, even when it exits zero. Ignored
build artifacts may remain in the disposable checkout. Rejected run records
retain the check's actual numeric exit status when available; it is separate
from the supervisor's failure classification. Historical receipts without a
version-2 check record cannot authorize downstream reuse; fresh verification
with a new idempotency key is required. Migration and replay never upgrade them.
