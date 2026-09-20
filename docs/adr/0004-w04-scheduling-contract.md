# ADR 0004: W04 scheduler, executor and profile interfaces

Status: frozen implementation contract for W04, following independent review.
Implementation is partial; see [scheduling status](../scheduling.md).

The scheduler owns project capacity policy, task dependency edges and queue order.
The store reserves capacity, creates an immutable attempt input record, updates the
task and enqueues its launch intent in one immediate transaction. Every retained
attempt counts, including adopted, lost, completed-but-unquiesced and awaiting-input
attempts. The coordinator does not have a worker attempt. Reducing capacity drains
naturally; no reservation is revoked to fit a lower cap.

Task edits use the existing task revision and event head. Policy edits also fence
its revision. Dependency requirements are verified_result, integration_candidate or
landed_commit; task status or report text alone cannot satisfy them. W07 supplies
verified evidence. Until then dependency-bearing tasks remain blocked without
fabricated verification. Priority is bounded and aged against original enqueue
time so old work eventually outranks a continuing stream of higher-priority work;
deterministic creation sequence breaks ties. Requeueing cannot reset age. Mutation
and bulk import reject missing predecessors, duplicates, self-dependencies and cycles.

The scheduler accepts a sealed preparation from the profile/authority path, binding
profile, config, runtime route, control/policy/task revisions and immutable inputs.
Inputs pin exact predecessor result/candidate/landed identities, repository commit/tree
vectors, snapshot/memory-validity references, approval identity and budget revision.
Unsupported prerequisites block admission; W05/W07 references remain unavailable
until their producers exist.
A launch intent is not external authorization by itself. T04.3 resolves profiles;
T04.5 validates/consumes approval with the claim. Until those paths and the carried
W03 launch crash gate pass, the controller must not dispatch runtime.launch.

Executor queues own bounded external command concurrency, deadlines and cancellation.
They receive immutable operation IDs and expected revisions; they never hold a
SQLite transaction across commands. Control and transfer/verification lanes have
separate bounds and project/machine fairness. These are root-local command bounds,
not machine-wide agent quotas. Existing execution guards remain until replaced by
an equally strong explicit resource-serialization contract.

Canonical mutations and routine execution now retain a shared root barrier on the
existing `.execution.lock` inode, exclusive project `.state/effect.lock` ownership,
then the short `.state/lock` record lock. Locks are regular no-follow files and never
upgraded in place. The shared lock implementation also supplies root-exclusive
cleanup/maintenance ownership. A failed acquisition drops all earlier ownership.
Canonical observation commit, control-marker publication and claim expiry retain
project ownership together. Different canonical projects can refresh status and edit
tasks concurrently; the same project remains excluded during a routine.

Root-wide adoption/conflict scanning, migration, legacy ticker passes and existing
external-effect adapters remain exclusive. Legacy ticker status still includes prompt
and token-metadata effects, so it must be separated before routine automatic dispatch.
Concurrent terminal effects additionally require cross-project endpoint/pane exclusion;
project guards alone do not authorize aliased terminal access.

Profiles bind executable kind, verified capabilities, explicit argv/environment
references, config identity and installed versions. They do not inherit another
kind's vendor flags. Unknown capabilities remain unavailable. Secrets are not copied
into task briefs or snapshots. Effective profile identity is immutable per attempt.

Cancellation is desired state first. A reservation whose launch is still Pending
and has never been claimed may be released atomically with retirement. The exact
attempt–launch relationship, zero monotonic claim attempts/epoch, and absence of
another launch or externally running/adopted worker must be checked in that same
transaction. Pending alone is insufficient: no-effect retries may return to Pending.
Upgrade paths preserve claim history. Retained inert files are not themselves a
live worker. With those constraints, the durable
record proves no supported external adapter was entered. Claimed/ambiguous/running
work retains capacity until exact termination evidence. Cancellation cannot promote
success, erase attempts or revoke preservation obligations. Operator overrides must
be separately audited and must never fabricate verified dependency evidence.
