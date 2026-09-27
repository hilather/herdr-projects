# Pulling memory update manifests

On a canonical project, pull the unresolved obligations for an exact consumer
generation using its binding ID or its immutable snapshot ID:

```sh
herdr-projects memory PROJECT package --binding BINDING_ID
herdr-projects memory PROJECT package --snapshot SNAPSHOT_ID
herdr-projects memory PROJECT package --snapshot SNAPSHOT_ID --change CHANGE_ID --change ANOTHER_CHANGE_ID
```

Specify exactly one selector. A snapshot resolves to its recorded binding; it
does not select the newest session or borrow another consumer's obligations.
The command returns `schema_version: 1`, `package` identity/generation/hash fields and ordered
`members`. Each member names a change, record revision, body hash, severity,
triggering sequence and snapshot. It returns references, not the body contents.
No unresolved obligations is an explicit error, not an empty application receipt.

The store creates the package and membership atomically. Identical unresolved
membership returns the same retained package after restart. New obligations can
produce a different package; an older package never covers changes absent from
its membership. The package is reconstructed from canonical obligations, not
from a caller-provided list of records.

Repeat `--change` to select an exact batch of unresolved obligations (at most
10,000). Each ID must belong to the selected binding; duplicates, missing IDs
and already-applied or explicitly superseded changes are refused atomically. Selection uses indexed
lookups instead of loading the entire backlog. Omit `--change` to retain the
full unresolved-package behavior. The package still binds its complete selected
membership; this option does not permit partial acknowledgment of that package.

Use selection when an older, unapplied revision would prevent a package
containing its replacement from being applied. The current revision can then
be acknowledged independently. The obsolete obligation remains visible and
unacknowledged. Selection does not supersede that obligation, erase a mandatory
reconciliation requirement, or claim that the worker applied obsolete content.
Optional updates can be explicitly superseded using the separate protocol below.
Mandatory and successor-generation reconciliation remain separate requirements.

Retrieval does not create `seen` or `applied` receipts, advance an applied cursor,
release a barrier, alter attempt capacity, or prove that referenced bodies remain
available and current. Those checks belong to delivery and application protocols.
Existing individual worker update/acknowledgment commands retain their own exact
identity and evidence checks. A current worker can aggregate already accepted
individual receipts with:

```sh
herdr-projects memory PROJECT package-ack --attempt ATTEMPT_ID --input ack.json
```

The input has `schema_version: 1`, `package_id`, `manifest_hash`, the package's
exact `change_ids`, and `disposition: "seen"` or `"applied"`. First acknowledge
each member through the existing individual worker protocol. Every member must
have an exact receipt for that attempt, snapshot, manifest and disposition.
An exact logical `seen` receipt for every change must precede package `applied`;
it may come from an earlier package containing that identical change. A partial set, another worker's
receipt, a retired/replaced binding, changed membership or unavailable/corrupt
body is refused. Body reads share a 50-MiB limit. The final transaction rechecks
worker identity and supporting receipts before publishing package accounting;
a publication failure rolls back its event and receipts together.

The reply contains `schema_version: 1`, the package `receipt`, and an
`evidence_sequence`. That sequence identifies an immutable
`memory.worker_package_ack` event recording protocol
`worker-change-declaration-v1`, the original attempt/binding/generation, package
receipt sequence, and every supporting change receipt's manifest and sequence.
The retained evidence payload is limited to 8 MiB.
Schema 43 indexes this evidence in `worker_package_acknowledgments`. Publication
of the evidence and package receipt is one transaction; duplicate acceptance
validates and reuses the same evidence instead of appending another event. The
evidence remains auditable after worker replacement. Upgrades leave the index
empty rather than inventing evidence for historical package acknowledgments.

Package acceptance is retained in its own `memory.package_ack` event. Repacking
does not move logical change receipts to the new package or replace their
original event sequence. A new explicit acknowledgment may reuse those exact
receipts and gets its own package event; replay of either package keeps its
original response. Schema 43 prevents all changes to logical receipt rows and
preserves historical source references verbatim, including older retargets.

This command aggregates worker declarations, not proof that a model incorporated
the content. It does not grant authority, certify validation, clear invalidations,
or release attempt capacity. It accepts only current live worker bindings and
their exact starting snapshots; coordinator/native-adapter acknowledgment and
successor-generation delivery require their own protocols. Duplicate acceptance
is idempotent while the original worker remains eligible.

An obsolete optional delivery can be retired after the same worker has explicitly
applied its current replacement:

```sh
herdr-projects memory PROJECT supersede-update --attempt ATTEMPT_ID --input supersession.json
herdr-projects memory PROJECT supersession --binding BINDING_ID --change OLD_CHANGE_ID
```

The request contains `schema_version: 1`, `binding_id`, `change_id` (the old
delivery), `replacement_change_id`, `replacement_manifest_hash` (from the
individual `update` response), and a nonempty `reason` of at most 1,024 bytes.
Both deliveries must belong to the same current live worker binding and exact
starting snapshot. The replacement must be a strictly newer revision of the
same record, with the worker's exact individual `applied` receipt. At first
acceptance it must still be current, active, unexpired and valid, including its
dependencies; the service reads and verifies its body and rechecks the store in
the publication transaction. `seen` alone cannot retire an obsolete delivery.

Only `informational` old deliveries for non-hard records qualify. Constraints,
hard memory, `reconcile_before_completion` and `stop_at_checkpoint` changes are
refused. Acceptance writes an immutable `memory.update_superseded` event and
schema-43 `memory_update_supersessions` row with the exact request, attempt,
generation and supporting receipt sequence. The event and row commit together.
The reply exposes this evidence; the inspection command remains usable after
worker retirement. Upgrades start with no supersession rows.

Future package pulls exclude that retired optional delivery, but the original
obligation, package membership and all receipts remain unchanged. Supersession
does not create an `applied` receipt for the old change, count as applied coverage,
clear an invalidation, or release capacity. Replaying the identical request
returns the retained receipt while the worker is still eligible and the
replacement body remains available, even if a later revision has been published.
A different replacement or reason for the same old obligation conflicts. New
generations cannot reuse this protocol to acknowledge their predecessors.

The service uses project mutation exclusion for package publication. It is a
pull interface with explicit worker receipt aggregation; automated package
dispatch, certified native-adapter acknowledgment, reviewer orchestration and
barrier release workflows remain incomplete. This
command does not establish the live F3 gate.
