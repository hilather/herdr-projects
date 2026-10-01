# Telemetry operations runbook (TM5.3)

Retention, deletion, holds, backup and restore of the telemetry sidecar and
of the on-disk artefacts that telemetry and the worker sandbox leave behind.
Policy source: plan doc 09 ("Privacy, retention, and operations"); card:
plan doc 12 TM5.3. Code: `src/telemetry/maintenance/` (`mod.rs` retention
classes, plan and apply; `store.rs` the operations store; `backup.rs`
backup and restore). Tests: `tests/telemetry_operations.rs`.

The transcripts below are real output of the fixture run in
`runbook_transcripts_are_the_fixture_run`, normalized only for values that
change per run: `sha256:<digest>`, `<unix_ms>`, SQLite file sizes
(`<bytes>`), the temporary root (`<tmp>`) and the attempt id (`<attempt>`).
The test fails when a transcript here differs from the product's output;
`HERDR_RUNBOOK_WRITE=1 cargo test --features state-store --test
telemetry_operations runbook` rewrites them.

## 1. Authority and scope

- **Owner only.** `maintenance apply`, `maintenance hold add|release`,
  `backup create` and `backup restore` record the project owner
  (`operator:cli`) and refuse to run inside a worker execution context (the
  working directory is a task worktree, or `HOME` is a worker execution
  home). `maintenance classes|plan`, `maintenance hold list`, `backup
  verify|list` are read-only.
- **Destructive maintenance needs the plan digest.** `apply` recomputes the
  plan; when anything destructive is due it refuses without `--confirm
  <plan digest>`, and refuses a digest that no longer matches (the set of
  items changed). Only derivable items (health evaluations, superseded
  analytics revisions) are deleted without it.
- **Runtime lock.** `apply` holds the project's runtime mutation lock (the
  one the ticker's spool ingestion and every effect take), so it never races
  the ticker's cleanup of spools or quarantines, and the telemetry
  maintenance lock exclusively, so no collect works from older tombstones.
- **Offline restore.** `backup restore` takes the project maintenance barrier
  (`.ticker.lock`, the root barrier and `.state/lock`): stop the ticker
  first. It is refused while any effect or the ticker holds the project.
- **Canonical state is out of scope.** Nothing here writes `state.db`.
  Canonical workflow history, dispatch decisions, accepted usage and budget
  receipts, verified findings, seeded-defect and replay registries follow the
  canonical lifecycle. The tests assert `state.db` stays byte-identical
  through apply, rebuild, backup and restore.

## 2. Retention classes (`retention.v1`)

`telemetry <slug> maintenance classes [--json]` prints the declared table with
this deployment's overrides. Age counts from durable acceptance (or the
column named in `age_from`).

| Class | Store | Default | Basis | Action |
| --- | --- | --- | --- | --- |
| `sidecar.normalized_sessions` | `telemetry.db`, per native session: `claude_messages`, `claude_tool_results`, `codex_*`, `rollout_*`, `collect_offsets`, `codex_tool_sources`, `source_bindings`, `source_observations`, `ingest_quarantine`, `coverage_gaps`, `source_cursors`, `usage_entries`, `usage_dispositions`, `model_segments`, `quota_window_observations`, `session_graph_nodes` | 90 d | derivable from the native rollout (tombstoned: never again) | prune, destructive |
| `sidecar.attention_samples` | `attention_samples` | 90 d | source of truth (sampled live, R4) | prune, destructive |
| `sidecar.health_evaluations` | `health_evaluations` | 90 d | derivable | prune |
| `sidecar.analytics_revisions` | superseded `analytics_revisions`, their `analytics_lineage` and `analytics_workspace_metrics`; `analytics_workspace_comparisons` outside the window (latest kept) | 365 d | derivable | prune (the current revision of every cell is kept) |
| `sidecar.derived_projections` | ledger, graph, quota windows, proxy signals, integration outcomes, verification runs/test results, policy shadow, health states and alerts, analytics cells | — | derivable | follows its sources |
| `sidecar.accounting_imports` | rate cards, charges, invoices, FX tables | — | source of truth (R4: re-import needs the files) | retain |
| `sidecar.valuation_history` | valuation revisions, deltas, bases, inputs | — | source of truth (as-of views) | retain |
| `ops.tombstones` | `telemetry-ops.db` | 400 d | source of truth | listed when expired, never pruned by `retention.v1` |
| `ops.audit` | holds, maintenance runs, backup inventory, restore reports | — | source of truth | retain |
| `artefact.git_quarantine` | `<project>/.git-quarantine/<attempt>` | 7 d after the terminal mark | source of truth | prune, destructive |
| `artefact.submission_spool` | `<project>/.state/spool/<attempt>` | 7 d after the terminal mark | source of truth | prune, destructive |
| `artefact.replay_repos` | `<root>/.replay/<slug>/repos/<suite>/<seq>/<case>` | 30 d after the run | source of truth | prune, destructive |
| `artefact.backups` | backup directories in the inventory | 30 d | source of truth | prune, destructive |
| `optin.external_export_files` | the enabled external export directory | 7 d (a project override may only shorten it) | source of truth | prune, destructive |
| `optin.captured_evidence` | — | 7 d | — | not built (contracts §7: no capture store exists) |
| `canonical.state` | `state.db` | canonical | canonical | external lifecycle |
| `native.codex_rollouts` | `<execution_home>/.codex/sessions` | owned by Codex | — | external; their availability is the replay horizon |
| `secret.cursor_key` | `<config_dir>/telemetry-cursor.key` | — | secret | never backed up with telemetry; deleting it revokes every cursor |

Preconditions (an item that fails one is listed as `blocked` with the reason
and stays):

- sessions: every bound attempt terminal (`attempt_not_terminal`); no
  quarantined record (`quarantine_unresolved`); accepted usage synced into
  the ledger after its acceptance (`ledger_not_synced`) with no `unresolved`
  or `conflict` disposition (`accounting_disposition_pending`). Required
  accounting intents therefore stay until their disposition.
- attention samples: the attempt terminal.
- Git quarantines: attempt terminal with its termination observed
  (`termination_not_observed`), and an import verdict (`import.json`) beside
  every quarantine (`import_verdict_missing`).
- spools: attempt terminal with its termination observed, and a receipt for
  every request (`request_without_receipt`): an unanswered request is a
  required intent; the ticker answers or denies (and records) it first.
- replay repositories: candidate task terminal, no live attempt
  (`attempt_live`), the path inside the replay root (`outside_replay_root`).
- backups: the directory's manifest still has the inventoried digest
  (`manifest_changed`).

The deletion scope of a session is declared: `apply` refuses a sidecar
holding a table with a `session_id` or `path_digest` column outside it (a
future migration must extend `retention.v1` first). Priced history
(`valuations`, `valuation_deltas`, `valuation_bases`, `provider_charges`)
keeps the session id as a reference: an as-of view still shows what was
priced, and the next reprice restates without the expired entries.

**Overrides** are explicit deployment policy in
`<config_dir>/telemetry-retention.toml` (a regular file of this user, not
group/world writable, at most 16 KiB):

```toml
schema = "telemetry-retention.v1"
[days]
"sidecar.attention_samples" = 30
[projects.demo.days]
"optin.external_export_files" = 3
```

Days are 0 to 3650; tombstones are kept at least 400 days; `optin.*` classes
may only be shortened.

<!-- transcript: classes -->
```text
$ herdr-projects telemetry demo maintenance classes
retention.v1
sidecar.otlp retain - source_of_truth destructive
secret.otlp_tokens external_lifecycle - source_of_truth destructive
sidecar.normalized_sessions prune 90d derivable_from_native_source destructive
sidecar.attention_samples prune 90d source_of_truth destructive
sidecar.health_evaluations prune 90d derivable
sidecar.analytics_revisions prune 365d derivable
sidecar.derived_projections follows_sources - derivable
sidecar.accounting_imports retain - source_of_truth destructive
sidecar.valuation_history retain - source_of_truth destructive
ops.tombstones listed_not_pruned 400d source_of_truth destructive
ops.audit retain - source_of_truth destructive
artefact.git_quarantine prune 7d source_of_truth destructive
artefact.submission_spool prune 7d source_of_truth destructive
artefact.replay_repos prune 30d source_of_truth destructive
artefact.backups prune 30d source_of_truth destructive
optin.external_export_files prune 7d source_of_truth destructive
optin.captured_evidence not_built 7d source_of_truth destructive
canonical.state external_lifecycle - canonical destructive
canonical.verification_execution_slots external_lifecycle - derivable
native.codex_rollouts external_lifecycle - source_of_truth destructive
secret.cursor_key external_lifecycle - source_of_truth destructive
```

## 3. Plan, holds and apply

`maintenance plan [--json] [--now <unix ms>] [--forget-session <id>]...`
lists, per class, what is eligible, blocked (with the reason) and held (with
the hold), the quotas and the plan digest. `--now` is a what-if; `apply`
always uses the current time. `--forget-session` is an operator deletion of
a Codex session regardless of its age (holds and accounting preconditions
still apply; the tombstone reason is `operator_deletion`).

<!-- transcript: plan -->
```text
$ herdr-projects telemetry demo maintenance plan
plan retention.v1 demo items=1 destructive=1 digest=sha256:<digest>
sidecar.normalized_sessions 90d destructive: eligible=1 blocked=0 held=0
  delete session:00000000-0000-4000-8000-00000000c0de
sidecar.attention_samples 90d destructive: eligible=0 blocked=0 held=0
sidecar.health_evaluations 90d: eligible=0 blocked=0 held=0
sidecar.analytics_revisions 365d: eligible=0 blocked=0 held=0
artefact.git_quarantine 7d destructive: eligible=0 blocked=0 held=0
artefact.submission_spool 7d destructive: eligible=0 blocked=0 held=0
artefact.replay_repos 30d destructive: eligible=0 blocked=0 held=0
artefact.backups 30d destructive: eligible=0 blocked=0 held=0
optin.external_export_files 7d destructive: eligible=0 blocked=0 held=0
```

<!-- transcript: apply-unconfirmed -->
```text
$ herdr-projects telemetry demo maintenance apply
herdr-projects: apply would delete 1 destructive item(s); review `maintenance plan` and pass --confirm sha256:<digest>
```

A hold blocks deletion of its class (or `all`) and optional scope (a
session, attempt, task or backup id). Holds are never invented
automatically. The reason is stored as a contracts §7 excerpt (secrets
masked); the scope is stored as a salted digest, so a hold never keeps a
deleted session's id; a scope shaped like a credential is refused.

<!-- transcript: hold-add -->
```text
$ herdr-projects telemetry demo maintenance hold add --class sidecar.normalized_sessions --scope 00000000-0000-4000-8000-00000000c0de --reason "litigation hold 42"
{
  "class": "sidecar.normalized_sessions",
  "hold_id": "hold-1",
  "placed_unix_ms": <unix_ms>,
  "principal": "operator:cli",
  "reason": "litigation hold 42",
  "scope": "00000000-0000-4000-8000-00000000c0de",
  "scope_digest": "sha256:<digest>"
}
```

<!-- transcript: plan-held -->
```text
$ herdr-projects telemetry demo maintenance plan
plan retention.v1 demo items=0 destructive=0 digest=sha256:<digest>
sidecar.normalized_sessions 90d destructive: eligible=0 blocked=0 held=1
  held session:00000000-0000-4000-8000-00000000c0de (hold-1)
sidecar.attention_samples 90d destructive: eligible=0 blocked=0 held=0
sidecar.health_evaluations 90d: eligible=0 blocked=0 held=0
sidecar.analytics_revisions 365d: eligible=0 blocked=0 held=0
artefact.git_quarantine 7d destructive: eligible=0 blocked=0 held=0
artefact.submission_spool 7d destructive: eligible=0 blocked=0 held=0
artefact.replay_repos 30d destructive: eligible=0 blocked=0 held=0
artefact.backups 30d destructive: eligible=0 blocked=0 held=0
optin.external_export_files 7d destructive: eligible=0 blocked=0 held=0
```

<!-- transcript: hold-release -->
```text
$ herdr-projects telemetry demo maintenance hold release hold-1 --reason "released by counsel"
{
  "hold_id": "hold-1",
  "release_reason": "released by counsel",
  "released_unix_ms": <unix_ms>
}
```

`apply --dry-run` prints what would be deleted and writes nothing.
`apply --confirm <digest>` writes one tombstone per key **before** deleting
it, then deletes, then reapplies every tombstone to the sidecar (which also
finishes an interrupted earlier run), and records the run in
`maintenance_runs`.

<!-- transcript: apply -->
```text
$ herdr-projects telemetry demo maintenance apply --confirm sha256:<digest>
applied digest=sha256:<digest> tombstones_added=2
  deleted sidecar.normalized_sessions session:00000000-0000-4000-8000-00000000c0de
```

**Tombstones** (`telemetry-ops.db`, append-only by trigger) hold the class,
an opaque key (sha256 over a random per-store salt, the class and the key;
never the key, a label or an unsalted hash), an optional age cutoff, a typed
reason, the run and the principal. They are applied:

- by every collect: a tombstoned rollout path is never read, a copy of a
  tombstoned session under another file name is dropped before its pass
  commits, and leftover rows are purged;
- after a rebuild (`telemetry.db` deleted and collected again): the
  tombstones live outside the sidecar, so the rebuilt sidecar never holds a
  deleted session while its rollout is still on disk;
- on restore, before the restored copy is exposed.

Deleting a row does not erase it from WAL files, filesystem snapshots or
backups: expire affected backups (`artefact.backups`, or delete them by
hand and let the inventory entry go at the next apply) and rely on
encrypted storage with separated keys for media erasure; `DELETE`/`VACUUM`
are not secure deletion.

## 4. Artefact cleanup

- **Git quarantines** (`.git-quarantine/<attempt>`, isolation review item:
  never cleaned before TM5.3). The controller writes `import.json` after
  each import verdict; seven days after the attempt's terminal mark, with its
  termination observed, the whole attempt directory is removed (read-only
  Git directories included, links never followed). A quarantine without a
  verdict stays and is listed `import_verdict_missing`.
- **Submission spools.** The ticker removes a spool once the attempt's
  termination is observed; maintenance removes leftovers (ticker stopped)
  only when every request has its receipt. **Premature retirement:** a spool
  removed out of band before the ticker answered makes the worker's
  command fail at once with `lost request <digest> before the ticker
  answered it: nothing was submitted` instead of waiting for a receipt that
  never comes; nothing reached the store, so the operator reruns the command
  (test `a_spool_retired_before_ingestion_is_reported_not_lost`).
- **Replay repositories** of finished candidates, thirty days after the run.
  The replay registries and hidden-check store are canonical and kept.
- **Quotas.** `plan` reports `telemetry.db` and WAL sizes, the operations
  store, each spool's entries against the ticker's 256-entry cap (a spool
  over it is refused by the ticker: backpressure on that worker), each Git
  quarantine against the 1 GiB import copy limit, and the inventoried backup
  bytes. The text plan prints any spool over its cap
  (`quota spool <attempt> entries=257 limit=256 over_limit`).
- **Opt-in export evidence.** With the external export enabled for a
  directory, exported pages `<slug>-<id>-<offset>.json|csv` (and a CSV
  page's `.manifest.json`) expire after seven days or a shorter project
  override. Other files and other projects' pages are never touched; with
  the export disabled the directory is no longer the deployment's and
  nothing is deleted.

## 5. Backup

`telemetry <slug> backup create --out DIR [--encrypt-to <age recipient>]`

- DIR must not exist (created 0700) or be an empty directory of this user.
- `telemetry.db` and `telemetry-ops.db` are copied with SQLite's online
  backup API from a read connection (a consistent snapshot including
  committed WAL frames; copying the file alone can omit WAL state), turned
  into self-contained files (mode 0600) and integrity-checked.
- The manifest (`manifest.json`, schema `telemetry-backup.v1`) lists each
  file's size and sha256, the stream versions, the watermark (the latest
  `*_unix_ms` any row records), row counts, what is excluded and why, and
  its own `backup_id` (sha256 of the manifest without it). The backup is
  recorded in the inventory (`backup list`).
- **Encryption.** `--encrypt-to` runs the operator's installed `age` for
  every file (plaintext copies stay in a private scratch directory under
  `.state` and are removed); the backup directory then holds only
  `*.age` files and the manifest (with both digests). Without `age` on
  `PATH` the command refuses before writing anything. Without
  `--encrypt-to`, keep backups on encrypted storage (full-disk or the
  backup tool's encryption) with keys separated from the host.
- **Excluded:** `state.db` (canonical, §7), the cursor key (a secret, stored
  separately), Codex rollouts (native sources), spools, quarantines and
  replay repositories (workflow artefacts).

<!-- transcript: backup-create -->
```text
$ herdr-projects telemetry demo backup create --out <tmp>/backup-2026-09-30
{
  "backup_id": "sha256:<digest>",
  "canonical_store": "not included: back up state.db with the canonical procedure",
  "encrypted": false,
  "files": [
    {
      "bytes": <bytes>,
      "name": "telemetry.db",
      "sha256": "sha256:<digest>"
    }
  ],
  "location": "<tmp>/backup-2026-09-30",
  "manifest_digest": "sha256:<digest>",
  "streams": {
    "accounting": <version>,
    "analytics": <version>,
    "codex": <version>,
    "health": <version>,
    "ingest": <version>,
    "otlp": <version>,
    "policies": <version>,
    "quality": <version>
  },
  "watermark_unix_ms": <unix_ms>
}
```

`backup verify --from DIR` checks the manifest, its `backup_id` and every
file digest without restoring:

<!-- transcript: backup-verify -->
```text
$ herdr-projects telemetry demo backup verify --from <tmp>/backup-2026-09-30
{
  "backup_id": "sha256:<digest>",
  "files": [
    {
      "bytes": <bytes>,
      "name": "telemetry.db",
      "sha256": "sha256:<digest>"
    }
  ],
  "verified": true
}
```

## 6. Restore

`telemetry <slug> backup restore --from DIR [--force] [--identity <age identity file>]`

1. Stop the ticker (restore takes the maintenance barrier and is refused
   otherwise).
2. The manifest must name this project and match its `backup_id`; every file
   must match its digest (a changed byte is refused). An encrypted backup
   needs `--identity`; each decrypted file must match its plaintext digest.
3. The copy must pass `PRAGMA integrity_check`, and no stream may be newer
   than this binary.
4. **Newer sidecar.** When the live sidecar records anything later than the
   backup's watermark, restore refuses and names what is not derivable from
   sources and would be lost (attention samples, imported rate cards,
   charges, FX tables, valuation revisions newer than the backup). `--force`
   replaces it and the report lists that loss under
   `replaced_newer.not_recoverable`.
5. The backup's salts, tombstones and holds are merged into the live
   operations store, and **every tombstone is reapplied to the copy before it
   is exposed**: deleted sessions, attention samples, evaluations and
   revisions are never resurrected.
6. The checked copy is written into the live `telemetry.db` with the online
   backup API (readers and WAL handled by SQLite) and migrated to this
   binary's streams. A new incarnation id is recorded with the restore
   report in `restores`.
7. Restore is analytics-only: native identities (session, ordinal, payload
   digest, producer epoch) are preserved, so the next collect dedupes and
   old usage never becomes new usage; nothing is written to `state.db`, so
   no budget, usage or quality acceptance is re-fed from a restored sidecar.
   Then run `collect` (refreshes bindings from canonical state),
   `accounting sync` and `analytics refresh`.

<!-- transcript: restore -->
```text
$ herdr-projects telemetry demo backup restore --from <tmp>/backup-2026-09-30
{
  "backup_created_unix_ms": <unix_ms>,
  "backup_id": "sha256:<digest>",
  "budget": "restore is analytics-only: no canonical budget, usage or quality acceptance is written or re-fed from it",
  "canonical_written": false,
  "forced": false,
  "incarnation": "sha256:<digest>",
  "next": [
    "telemetry <slug> collect (refreshes bindings from canonical state; restored native identities dedupe)",
    "telemetry <slug> accounting sync",
    "telemetry <slug> analytics refresh"
  ],
  "orphans": 0,
  "replaced_newer": null,
  "restore_id": "sha256:<digest>",
  "restored_unix_ms": <unix_ms>,
  "rows": {
    "analytics_revisions": 0,
    "analytics_workspace_comparisons": 0,
    "analytics_workspace_metrics": 0,
    "attention_samples": 0,
    "claude_messages": 0,
    "claude_tool_results": 0,
    "codex_usage": 0,
    "fx_tables": 0,
    "gemini_file_cursors": 0,
    "health_evaluations": 0,
    "otlp_records": 0,
    "provider_charges": 0,
    "rate_cards": 0,
    "rollout_sources": 0,
    "source_observations": 0,
    "valuation_revisions": 0
  },
  "streams": {
    "accounting": <version>,
    "analytics": <version>,
    "codex": <version>,
    "health": <version>,
    "ingest": <version>,
    "otlp": <version>,
    "policies": <version>,
    "quality": <version>
  },
  "tombstones": {
    "merged_from_backup": 0,
    "reapplied": {
      "sidecar.normalized_sessions": 1
    },
    "total": 2
  },
  "verified_files": 1,
  "watermark_unix_ms": <unix_ms>
}
```

## 7. Canonical store

No canonical backup command exists in this build; a telemetry backup never
contains `state.db` and a restore never writes it. Safe procedure for the
canonical store (owner):

1. Stop the ticker and wait for running effects to finish (the project's
   `.state/lock` and the root's `.ticker.lock` must be free).
2. Copy `state.db` with SQLite's backup API, never by copying the file
   while a writer may hold WAL frames:
   `sqlite3 <project>/.state/state.db ".backup '<dest>/state.db'"`, then
   `sqlite3 <dest>/state.db "PRAGMA integrity_check"` must print `ok`.
3. Copy the rest of `<project>/.state` that the store's publication checks
   read (`format.json`, `migration/`, `objects/`, `canonical-artifacts/`,
   `factory-objects/` where present) in the same stopped window.
4. Restoring a canonical store follows the canonical recovery protocol
   (migration `recover`, the periodic integrity check's "preserve the store
   and restore it, never auto-repair"); after it, restore or rebuild the
   sidecar and collect again.

## 8. Recovery procedures

| Situation | Procedure |
| --- | --- |
| `apply` interrupted | Run `maintenance plan` and `apply` again. Tombstones were written before each deletion; the next apply (and every collect) reapplies them, and remaining artefacts are listed again. |
| `telemetry.db` lost or corrupt | Prefer `backup restore` of the latest backup (keeps rate cards, charges, valuation history and attention samples); otherwise delete it and `collect` (rebuilds ledger, graph, usage and quota views from rollouts still on disk; loses what R4 lists). Tombstones in `telemetry-ops.db` keep deleted sessions out either way. |
| `telemetry-ops.db` lost | Restore a backup: its tombstones, salts and holds are merged. Until then deleted sessions whose rollouts are still on disk can be collected again: apply retention again before exposing views. |
| Backup failed verification | Do not restore it; use an older verified backup (`backup list`, `backup verify`). |
| Restore refused as newer | Read the refusal's `not_recoverable` counts; export or re-import those first, or restore with `--force` knowingly. |
| Secret leakage | Stop the affected capture, restrict access, identify every copy (sidecar, WAL, backups, export directory), rotate the credential through its owner, then `maintenance apply --forget-session <id>` and expire affected backups. Never paste the leaked value into an incident note or a hold reason (hold reasons are excerpted, scopes that look like credentials are refused). |
| Spool retired prematurely | The worker's command reported `lost request`; rerun it once the spool exists again. The ticker records every refused request as `spool.request_denied`. |
| Spool or disk pressure | `maintenance plan` quotas; clear expired artefacts with `apply`; a spool over its cap is refused by the ticker until cleared. |
| Restore exercise | `backup create`, `backup verify`, then restore into a scratch copy of the project (as `backup_restore_reapplies_tombstones_and_never_writes_canonical_state` does) and compare `telemetry report`. |

## 9. Evidence

`tests/telemetry_operations.rs` (all through the CLI, hand-computed values):

| Test | Shows |
| --- | --- |
| `retention_classes_are_declared_with_doc09_defaults` | the declared table and doc 09 defaults; overrides; tombstones never shortened |
| `deleted_sessions_are_tombstoned_and_never_collected_again` | 91-day-old session blocked while its attempt is live, then eligible (2 records); refused without and with a stale digest; dry run; hold with excerpted reason; worker context refused; apply deletes every table of the scope with 2 tombstones; collect, a renamed copy of the session and a rebuilt sidecar never resurrect it; `state.db` byte-identical; no session id or hold reason in the operations store |
| `accounting_intents_stay_until_their_disposition` | `ledger_not_synced`, `accounting_disposition_pending`, eligible once synced; operator deletion |
| `attention_health_and_analytics_expire_without_losing_current_views` | attention per attempt with its cutoff, health evaluations, superseded analytics revisions only; M08 unchanged; the immutability triggers restored; a restore reapplies revision tombstones |
| `backup_restore_reapplies_tombstones_and_never_writes_canonical_state` | manifest, modes, verify, tampered byte refused, offline-only restore, tombstones reapplied, newer sidecar refused then forced with its loss named, `state.db` byte-identical; a restore keeps native identities: collect adds 0 records, no quarantine, usage 1500/180 and M08 1500 unchanged |
| `encrypted_backups_use_the_operators_age_and_leave_no_plaintext` | refused without `age`; only `*.age` and the manifest; identity required; decrypted restore |
| `backups_expire_from_the_inventory_unless_held` | backup expiry by inventory, a held backup kept |
| `quarantines_spools_and_replay_repositories_are_cleaned_only_after_their_verdict` | every precondition, byte counts, the spool quota, what stays |
| `a_spool_retired_before_ingestion_is_reported_not_lost` | premature spool retirement |
| `opt_in_export_evidence_expires` | default 7 days, a project override that only shortens, other files kept, disabled export untouched |
| `secret_canaries_never_reach_persisted_metadata` | rollout, operator and config canaries absent from every file under the project and both backups after collect, sync, refresh, evaluate, hold, backup, restore, rebuild |
| `runbook_transcripts_are_the_fixture_run` | this document's transcripts |

Limits: fixture evidence only (no live Herdr, provider or ticker); `age` is
exercised through a deterministic local stand-in because it is not installed
on the test host; the canonical store backup is a documented procedure, not
a command.
