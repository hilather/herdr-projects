# Durable waits and replan requests

A wait belongs to a task, optional attempt, condition and plan revision. It is
advisory: waking never proves a dependency, releases capacity, grants approval,
or starts a worker. Admission still checks the current signed contract, evidence,
profile, resources and authority.

Register and inspect a subscription on an already canonical project:

```sh
herdr-projects plan wait PROJECT register --task PARENT --condition dependency_evidence
herdr-projects plan wait PROJECT register --task TASK --attempt ATTEMPT --condition validation_completion
herdr-projects plan wait PROJECT register --task TASK --condition user_decision --deadline 2026-12-01T18:00:00Z
herdr-projects plan wait PROJECT register --task TASK --condition user_decision \
  --approval-id approval-SHA256 --approval-task-revision REVISION
herdr-projects plan wait PROJECT register --task PARENT --condition resource_availability \
  --capacity-attempt BLOCKING_ATTEMPT --capacity-after-revision REVISION
herdr-projects plan wait PROJECT register --task PARENT --condition adapter_recovery \
  --recovery-binding BINDING --recovery-binding-revision REVISION \
  --recovery-ownership-revision OWNERSHIP_REVISION
herdr-projects plan wait PROJECT replay WAIT_ID
herdr-projects plan wait PROJECT rearm WAIT_ID --deadline 2026-12-02T18:00:00Z
```

Registration and its event cursor commit together. Repeating the same task,
attempt, condition, plan revision, optional deadline and typed trigger returns the same wait.
Omitting a deadline preserves the original registration identity. A deadline is
an immutable RFC 3339 timestamp with an explicit timezone, supported on schema 43.
Changing it creates a separate subscription; it does not cancel the earlier one.
Replay processes at
most 1,000 events, keeps unresolved subscriptions open, and ignores wakes addressed
to another wait. A terminal wake remains idempotent across restart.

After reevaluating an advisory wake, use `rearm` if another subscription is
needed. On schema 43 this creates one successor with the same task, attempt,
condition and current plan revision, preserving the predecessor's terminal
receipt. Repeating the command with the same deadline returns that successor;
changing the deadline on a retry is rejected. Omitting `--deadline` gives the
successor no deadline, even if the predecessor expired. Unresolved waits and
waits from superseded plans cannot be rearmed. Registration, the successor link
and any wake from currently retained evidence commit together. A wake addressed
to the predecessor cannot wake its successor. Rearming is explicit; it neither
dispatches a planner nor grants authority, and automatic renewal still needs
production orchestration.

Ordinary controller passes service at most eight pending waits from the current
plan, with a shared input budget and a two-second deadline. A durable rotation
cursor prevents the first unresolved subscriptions from hiding later ones across
restart. This is local database maintenance, not repeated model polling.

An elapsed subscription deadline generates one addressed advisory wake when the
wait is serviced, even if unrelated events remain ahead of its cursor. The inbox
notice states that the deadline expired. The timeout is not proof of success or
failure and does not release attempt capacity. It is not a real-time timer: the
controller's bounded rotation determines when it is serviced. Superseded plan
subscriptions are not automatically serviced. Notification failure rolls back
both the deadline event and replay state so a later service can retry safely.

Current automatic producers are:

- `runtime.observed` for an explicit owned-runtime recovery trigger. The three
  recovery flags are required together and cannot be combined with approval or
  capacity flags. Registration requires the exact retained local binding and
  ownership revisions. A wake requires a v2 collector observation no older than
  30 seconds, matching the current task revision and the claim's session,
  worktree, agent and configuration identities. Unknown, absent, mismatched,
  future-dated or stale observations do not wake it. Empty bindings and remote
  routes are excluded. A replacement binding or ownership claim needs a new
  subscription. Already-retained matching observations are checked on registration
  and renewal through an indexed publication lookup. A wake does not adopt a
  worker, clear reconciliation, resume a project or establish dispatch authority.
- Worker, staged-launch and worktree-preparation termination receipts, and a
  cancellation that proves a launch was never claimed, for an explicit attempt
  capacity trigger. Both capacity flags are required together and cannot be
  combined with approval flags. `REVISION` is the last observed attempt revision;
  the referenced attempt must exist and the revision cannot be in its future.
  A wake requires a newer canonical revision with termination observed, plus the
  matching committed receipt. Completion reports, uncertain cancellation and
  failed termination commits keep the subscription pending. Prior termination is
  checked at registration and renewal. This concerns one attempt's retained
  capacity; retained worktrees, panes and other conflicts still need admission's
  resource checks. The wait itself never releases anything.
- `approval.installed`/`approval.revoked` for a `user_decision` wait with an
  explicit approval trigger. Both CLI flags are required together. Use the exact
  content-addressed approval ID and the task revision in its signed scope. The
  retained grant must belong to the waiting task, and its scope revision must
  still match the task's current or next reservation revision. Registration can
  precede installation; a decision already retained at registration or renewal
  also requests reevaluation. The trigger is immutable and inherited on renewal.
  Unrelated approvals and inbox acknowledgments do not wake this subscription.
  Approval installation uses the existing authenticated import service; a wait
  cannot import, consume or authenticate an approval itself. Revocation and an
  expired grant may still request reevaluation: a wake is no claim of current
  authority. Admission must revalidate the complete grant and policy.
- `verification.accepted`/`verification.rejected` for validation completion,
  matching the owner task, optional attempt and current contract revision.
- `verification.accepted` and `integration.wake` for dependency evidence,
  matching the consumer's retained satisfaction and revalidating all its signed
  dependency policies. Partial or wrong-policy evidence does not close this wait.
- The consumer's contract installation or queue update, which rechecks dependency
  evidence that may have arrived earlier.

Registration also observes already retained matching receipts and queues an
addressed wake in the same transaction, avoiding completion-before-subscription
loss. A triggered replay stores one `wait-wake` inbox notice and `wait.notified`
event atomically with replay progress. Failure to insert the notice rolls back
replay; retry cannot silently lose it. The notice asks for reevaluation and
explicitly does not claim proof. It does not execute a parent model itself.

A feedback-backed replan request is available through:

```sh
herdr-projects feedback PROJECT show
herdr-projects feedback PROJECT replan FEEDBACK_ID
```

Only retained verifier/integrator/invalidation feedback is eligible. A pull-request
poll or arbitrary string cannot trigger a replan. The existing per-blocker,
per-plan budget records two automatic requests, then one inbox escalation. This
budget is delimited by plan revision, not wall-clock timestamps; clock rollback
does not refund requests already charged to that revision. This
command atomically records the request, acknowledges transfer of the feedback,
and creates one `replan-request` inbox item. Repeating it returns the same request
and does not duplicate delivery. This acknowledgment is not an accepted planner
answer. The historical `proposal_id` in the request receipt is the reserved
response key, not a row already present in `plan_proposals`.

On schema 43, enable automatic feedback-to-request servicing explicitly:

```sh
herdr-projects plan auto-replan PROJECT on --expected-head HEAD
herdr-projects plan auto-replan PROJECT off --expected-head HEAD
```

The switch defaults to off and requires the current event head. While enabled on
an active project, each controller pass selects at most eight pending feedback
items with a two-second work budget. Live claims owned by another consumer are
respected. A durable rotation cursor prevents a leased or failing item from
monopolizing selection. Request, feedback transfer and inbox publication commit
together; a failed publication can be retried. Pausing the project or disabling
the switch retains pending feedback. This switch controls request generation,
independently of factory admission; it does not start inference or workers.

Each processed feedback item retains an immutable link to its decision, including
later items coalesced into the same escalation. Escalated feedback remains open
for a human decision but is no longer selected by this service. Restart or a new
plan cannot reinterpret already-linked feedback as a new request. New feedback
in a new plan revision has a fresh budget.

Read the request with `herdr-projects inbox list PROJECT`. On schema 43, submit
the answer using the request's `expected_plan_revision` and `idempotency_key`:

```sh
herdr-projects plan propose PROJECT --input-file proposal.json \
  --expected-plan-revision REVISION --idempotency-key REPLAN_ID
```

The proposal transaction records the actual response link and marks the request
notice done. Exact repeats return the accepted response; changed bytes conflict.
A stale request cannot be rebased by supplying a newer parent revision. A new
plan revision resets the request budget. Proposal acceptance still does not
install signed task contracts or grant launch authority. No planner process is
started by these commands.

Remaining F2.4 work includes typed references for the other conditions, broader
user-decision records (scope/priority choices), other resource/recovery cases
(including runtimes without retained ownership), automatic renewal after an
advisory wake becomes stale, and automated planner dispatch consuming the durable
inbox request. Those
condition names are retained by the store, but registration alone does not supply
their missing producers. Untyped `user_decision`, `resource_availability` and
`adapter_recovery` waits have no condition-specific producers; they can still
expire or receive an explicitly addressed wake. The simulator and these local APIs do not certify the
live planning pilot.
