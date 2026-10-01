# Export contracts: portable exports and read-interface cursors (TM4.3)

Plan card TM4.3 (doc 12), doc 08 §4–§5. Common rules are
[contracts.md](contracts.md) §0 and §7. Code: `src/telemetry/export/`
(`mod.rs`, `cursor.rs`, `redact.rs`, `csv.rs`, `external.rs`); tests:
`tests/telemetry_export.rs`. An export reads **only** through the analytics
query service (`analytics::query::run_with`,
[contracts-analytics.md](contracts-analytics.md)); it adds no store, stream,
migration or canonical schema, and never writes `state.db` or the sidecar.

## 1. Command

```
telemetry <slug> export --metric M..[,M..] [--cohort C] [--from MS] [--to MS]
    [--as-of MS | --as-of-seq N] [--by DIM] [--horizon-ms MS]
    [--drill BUCKET [--page-size N] [--cursor C]]
    [--format json|csv] [--out FILE | --external] [--max-bytes N]
```

The query flags mean exactly what they mean for `telemetry query`
(normalized, validated and rejected by the query service: `ambiguous_cohort`,
`page_size_out_of_range`, ... as `query rejected: {...}`). Export-level
rejections print `export rejected: {code, ...}` on stderr with exit status 1
and write nothing: `cursor_needs_drill` (only records are paged),
`max_bytes_out_of_range` (1..8 MiB), `export_too_large`
(`bytes`, `max_bytes`), `external_export_disabled`.

Output: without `--out`/`--external`, the page on stdout. With `--out FILE`, a
**new** file (an existing one is refused, never replaced), owner-only (0600),
written to a hidden partial file, synced, then hard-linked into place, so a
reader never sees an incomplete export under the final name; a CSV export also
writes the companion manifest `FILE.manifest.json` (the JSON manifest plus
`data {format, bytes, digest}` of the CSV bytes). Stdout then carries a receipt
`{contract, export_id, written: [{file, bytes, digest}], next_cursor}`.

## 2. JSON (`export.v1`)

`{manifest, metrics, records}`.

**Manifest** (every page): `contract` `export.v1`, `schema_version` 1,
`export_id` (sha256 of the normalized request and the snapshot identity: equal
on every page of one pinned snapshot), `format`, `created_unix_ms`,
`created_at` (RFC 3339, UTC), `timezone` `UTC`, `complete` (true: pages are
only emitted whole), `query {contract, schema_version, registry, request,
query_unix_ms}` (the query service's normalized request, redacted),
`snapshot` (per metric `{metric_id, definition, revision, content_digest}`,
or `{drill: {revision, content_digest}}` when paging records), `page {first,
last, offset, rows, page_size, total, next_cursor,
next_cursor_expires_unix_ms}`, `bounds {max_records_per_page 500, max_bytes,
max_cells_per_metric 64}`, `redaction`, `summary`, `external`
(`{destination}` or null) and, for CSV, `csv {columns, dialect,
formula_guard, missing}`.

**Metrics** (first page only; continuation pages carry `metrics: []` and the
records of the same snapshot, so a later page can never mix a newer summary
with pinned records). One entry per query result:

| field | meaning |
|---|---|
| `metric_id`, `name`, `definition`, `registry`, `family`, `proxy`, `unit`, `certification` | registry identity and definition version |
| `cohort`, `time_basis`, `window {from_unix_ms, to_unix_ms, semantics}`, `horizon_ms`, `by` | the evaluated cohort |
| `status`, `value`, `value_type`, `value_status`, `reason` | `value_type` `ratio` (unreduced `"n/d"`), `integer`, `decimal`, `string`, `object`, `missing`; `value_status` is `available` or `<status>:<reason>` |
| `numerator`, `denominator`, `missing` | counters as the query answers them; every null among value/numerator/denominator has a typed token in `missing`: `unavailable:<reason>`, `empty:<reason>` or `not_applicable:no_<field>` — unknown is never 0 |
| `exclusions`, `coverage`, `breakdown`, `censored`, `provisional`, `diagnostic`, `detail` | as the query (lane `detail` redacted) |
| `as_of {requested, event_cutoff_unix_ms, observation_cutoff_unix_ms}` | requested knowledge time or sequence (null live), latest occurrence in the cohort, knowledge time |
| `cost_basis` | `{status: not_applicable, reason: not_a_cost_metric}`, or for priced metrics `{status, reason, rate_card_revision, basis, currency}` |
| `projection`, `source_watermarks`, `lag_ms`, `lag_reason` | projection revision (live or stored; `kind`, `supersedes`, `superseded_by`, `restated` for as-of reads), source watermarks |
| `cells[]` | per dimension cell `{dimension, value, value_type, value_status, numerator, denominator, missing, status, reason}` |

**Corrections.** A late correction appends a restatement (contracts-analytics
§4); an export `--as-of-seq` of the earlier revision reproduces its totals and
carries `projection.restated: true`, `superseded_by` and `current_revision`.
Analytics itself deletes nothing, only supersedes; under `retention.v1`
(TM5.3, [operations-runbook.md](operations-runbook.md)) the owner's
`maintenance apply` may expire superseded revisions older than 365 days,
with a tombstone each, after which an `--as-of-seq` of such a revision is
unavailable. The current revision of every cell is kept.

**Records** (with `--drill`): `{metric_id, definition, bucket, buckets,
snapshot {revision, content_digest}, offset, page_size, total, rows}`, rows as
the query's drill rows (redacted), or `{status: unavailable, reason}` for a
lane definition. Numbers keep the query's JSON types so totals reconcile
field for field with `telemetry query --json`.

## 3. CSV

RFC 4180: UTF-8, CRLF records, one header row, fields quoted when they hold
`,` `"` CR LF or edge spaces. Columns (fixed order):
`row_kind`, `metric_id`, `definition`, `cohort`, `window_from`, `window_to`, `dimension`, `dimension_value`, `record_bucket`, `record_entity`, `record_id`, `value_status`, `value`, `value_type`, `unit`, `numerator`, `denominator`, `exclusions`, `coverage`, `coverage_known`, `coverage_expected`, `cost_basis`, `projection_mode`, `projection_revision`, `projection_restated`, `content_digest`, `as_of`, `event_cutoff`, `observation_cutoff`, `attrs`, `page_offset`, `page_total`, `next_cursor`.

Rows: `metric` (one per result), `cell` (one per dimension cell), `record`
(one per drill row; `attrs` is the row's other fields as compact JSON) and a
closing `page` row (`page_offset`, `page_total`, `next_cursor` or
`none:last_page`); no preamble. Timestamps are RFC 3339 UTC; `window_*` is
`unbounded` when open; `as_of` is `live`, `seq:<n>` or a timestamp.

**Missing values** are typed tokens `<status>:<reason>` (`unavailable`,
`empty`, `not_applicable`, `none`, `unknown`) — never an empty cell and never
0. A cell is empty only where its column does not apply to the row kind
(e.g. `record_id` on a `metric` row).

**Formula guard** (OWASP CSV injection): a cell starting with `=`, `+`, `-`,
`@`, TAB or CR is prefixed with `'` unless it is a plain decimal numeral
(`-12`, `3.5`) or a ratio (`2/3`). Consumers strip one leading `'` before one
of those characters to recover the text; the JSON export is the exact-text
source.

## 4. Cursors (shared with `telemetry query --drill`)

`c2.<hex payload>.<hex mac>`; payload compact JSON `{v: 2, kind:
"analytics-drill", kid, project, iat, exp, request, snapshot, revision,
bucket, next}`; `mac` = HMAC-SHA256(key, payload bytes). The key is 32 random
bytes as 64 hex digits in `<config_dir>/telemetry-cursor.key`
(`~/.config/herdr-projects`), created on first issue with mode 0600 (its
directory 0700 when created), checked on every use (regular file, this user,
one link, mode 0600) and never exported. `project` is
`sha256("herdr-projects/project:" + canonical project path)`: the path itself
never appears. Lifetime 30 minutes.

Every page is reauthorized. Refusals, in check order: `invalid_cursor`
(malformed or MAC mismatch: any tampering), `cursor_revoked` (the key id
differs or the key is gone: deleting or rotating the key file revokes every
outstanding cursor), `cursor_key_unusable` (the key fails its checks),
`cursor_expired`, `cursor_foreign_project` (issued for another project),
`cursor_mismatch` (another normalized request or bucket), `restart_required`
(the unpinned live snapshot changed, or the pinned revision is gone). A first
page whose live content matches a stored analytics revision is pinned to it;
continuation pages read that revision's lineage, so ingestion and
restatements between pages neither duplicate nor skip records.

Authorization scope: the local CLI user may read the named project's stores
(slug validated; stores opened read-only); a cursor is bound to that project
and to the user's key. There are no other accounts on this interface.

## 5. Redaction

Exports contain metadata and evidence references only — never prompts, tool
arguments or output, command arguments, secrets or home paths. One rule for
every string leaf and object key of `metrics`, `records` and the echoed
request: a `sha256:` digest and a `c2.` cursor are kept; a value under an
identity key (`id`, `task_id`, `attempt_id`, `session_id`, `metric_id`,
`definition`, `export_id`) is kept when it has identifier grammar
`[A-Za-z0-9][A-Za-z0-9._:-]{0,191}` and no secret prefix; every other string
gets the contracts §7 excerpt rules (first line, home prefixes to `~`, URL
query/fragment stripped, token-like runs `[redacted]`, 160 scalars). Object
keys are kept with identifier grammar, otherwise excerpted (two keys that
redact alike stay distinct with a `#` suffix). The same rule applies to JSON
and CSV; hashes are not an anonymization guarantee. The `--out` path, the
cursor and the key never appear in an export.

## 6. External export (deployment setting, disabled by default)

`--external` sends the page to the destination in
`<config_dir>/telemetry-export.toml` and is refused (`external_export_disabled`)
unless that file exists and enables it. The file must be a regular file of
this user, not group/world writable, at most 16 KiB; unknown keys are errors.

```toml
schema = "telemetry-export-config.v1"

[external]
enabled = false               # true to allow `export --external`
destination = "directory"     # "directory" or "stdout"; nothing else
directory = "/srv/telemetry-outbox"   # absolute, a real directory of this user, not group/world writable
```

`directory`: the page is written as a new file
`<slug>-<export_id[..16]>-<offset>.<json|csv>` (plus the CSV companion
manifest) with the same atomic, never-replace rule as `--out`; a separate
shipper may pick it up. `stdout`: the page on stdout for a pipe. There is no
network client; `manifest.external.destination` records which was used.

## 7. Contract samples

Generated from the test fixtures (the contracts §6 worked example) by
`HERDR_EXPORT_SAMPLES=<dir> cargo test --features state-store --test
telemetry_export`; long cursors are shortened with `…`.

`export --metric M02,M49` manifest:

```json
{
  "bounds": {
    "max_bytes": 8388608,
    "max_cells_per_metric": 64,
    "max_records_per_page": 500
  },
  "complete": true,
  "contract": "export.v1",
  "created_at": "2026-09-30T11:12:10.45Z",
  "created_unix_ms": 1790766730450,
  "export_id": "sha256:e74be1f5bf8b6b0f1821f32c4ac351f6f96aa49f2e20b1132e9b0d791789f1a1",
  "external": null,
  "format": "json",
  "page": {
    "first": true,
    "last": true,
    "next_cursor": null,
    "next_cursor_expires_unix_ms": null,
    "offset": 0,
    "page_size": null,
    "rows": 0,
    "total": null
  },
  "query": {
    "contract": "analytics-query.v1",
    "query_unix_ms": 1790766730450,
    "registry": "analytics-registry.v3",
    "request": {
      "as_of": {
        "seq": null,
        "unix_ms": null
      },
      "by": null,
      "cohort": null,
      "contract": "analytics-query.v1",
      "drill": null,
      "horizon_ms": null,
      "metrics": [
        "M02.cohort-v1",
        "M49.v1"
      ],
      "page_size": 100,
      "registry": "analytics-registry.v3",
      "schema_version": 1,
      "window": {
        "from_unix_ms": null,
        "to_unix_ms": null
      }
    },
    "schema_version": 1
  },
  "redaction": {
    "content": "metadata and evidence references only; no prompts, tool arguments, output, secrets or home paths",
    "policy": "contracts.md \u00a77",
    "text": "excerpt rules 1-5 on every string except sha256 digests, page cursors and identifiers under identity keys"
  },
  "schema_version": 1,
  "snapshot": [
    {
      "content_digest": "sha256:df5ccb22afc89b2da429b4fd2752ebd50cae1158e42cd908c98deb1f3e0ac804",
      "definition": "M02.cohort-v1",
      "metric_id": "M02",
      "revision": null
    },
    {
      "content_digest": "sha256:0392b968194aadadc3046e7d5b134a90e21d86666424040e3f2506a0ea9d4db4",
      "definition": "M49.v1",
      "metric_id": "M49",
      "revision": null
    }
  ],
  "summary": "metrics on this page",
  "timezone": "UTC"
}
```

Its `M49` entry (an absent producer: typed missing values, never 0):

```json
{
  "as_of": {
    "event_cutoff_unix_ms": null,
    "observation_cutoff_unix_ms": 1790766730450,
    "requested": null
  },
  "by": null,
  "cells": [],
  "certification": {
    "evidence": "no producer (TM4.6)",
    "restriction": null,
    "status": "absent"
  },
  "cohort": "activity_window",
  "cost_basis": {
    "reason": "not_a_cost_metric",
    "status": "not_applicable"
  },
  "coverage": {
    "reasons": {
      "no_replay_suite": null
    },
    "state": "unavailable"
  },
  "definition": "M49.v1",
  "denominator": null,
  "diagnostic": {
    "family": "replay"
  },
  "exclusions": {},
  "family": "replay",
  "horizon_ms": null,
  "lag_ms": null,
  "lag_reason": "collection_not_run",
  "metric_id": "M49",
  "missing": {
    "denominator": "unavailable:no_replay_suite",
    "numerator": "unavailable:no_replay_suite",
    "value": "unavailable:no_replay_suite"
  },
  "name": "replay_suite_pass_rate",
  "numerator": null,
  "projection": {
    "content_digest": "sha256:0392b968194aadadc3046e7d5b134a90e21d86666424040e3f2506a0ea9d4db4",
    "matches_revision": null,
    "mode": "live",
    "revision": null
  },
  "proxy": false,
  "reason": "no_replay_suite",
  "registry": "analytics-registry.v3",
  "source_watermarks": {
    "canonical": {
      "events_head": 0,
      "last_event_unix_ms": 3100,
      "lifecycle_digest": "sha256:d7a507ecaaf4168c6e223c8fff5230e5df1d0c92f36a7057c61325e67ed2b625"
    },
    "sidecar": null
  },
  "status": "unavailable",
  "time_basis": "none",
  "unit": "ratio",
  "value": null,
  "value_status": "unavailable:no_replay_suite",
  "value_type": "missing",
  "window": {
    "from_unix_ms": null,
    "semantics": "half_open",
    "to_unix_ms": null
  }
}
```

`export --metric M01,M02,M49 --by route --format csv`:

```csv
row_kind,metric_id,definition,cohort,window_from,window_to,dimension,dimension_value,record_bucket,record_entity,record_id,value_status,value,value_type,unit,numerator,denominator,exclusions,coverage,coverage_known,coverage_expected,cost_basis,projection_mode,projection_revision,projection_restated,content_digest,as_of,event_cutoff,observation_cutoff,attrs,page_offset,page_total,next_cursor
metric,M01,M01.cohort-v1,terminal_cohort,unbounded,unbounded,,,,,,available,2,integer,tasks,2,not_applicable:no_denominator,"{""open"":2}",complete,3,3,not_applicable:not_a_cost_metric,live,none:live,none:live,sha256:94bde337f41630ea29e81a342a80f8eebd85820d97ccc17ab762dd2a20bd97cb,live,1970-01-01T00:00:03.1Z,2026-09-30T11:12:11.11Z,,,,
cell,M01,M01.cohort-v1,terminal_cohort,unbounded,unbounded,route,none,,,,available,0,integer,tasks,0,not_applicable:no_denominator,,,,,,live,none:live,none:live,sha256:94bde337f41630ea29e81a342a80f8eebd85820d97ccc17ab762dd2a20bd97cb,live,,,,,,
cell,M01,M01.cohort-v1,terminal_cohort,unbounded,unbounded,route,verify_only,,,,available,1,integer,tasks,1,not_applicable:no_denominator,,,,,,live,none:live,none:live,sha256:94bde337f41630ea29e81a342a80f8eebd85820d97ccc17ab762dd2a20bd97cb,live,,,,,,
cell,M01,M01.cohort-v1,terminal_cohort,unbounded,unbounded,route,verify_then_integrate,,,,available,1,integer,tasks,1,not_applicable:no_denominator,,,,,,live,none:live,none:live,sha256:94bde337f41630ea29e81a342a80f8eebd85820d97ccc17ab762dd2a20bd97cb,live,,,,,,
metric,M02,M02.cohort-v1,terminal_cohort,unbounded,unbounded,,,,,,available,2/3,ratio,ratio,2,3,"{""open"":2}",complete,3,3,not_applicable:not_a_cost_metric,live,none:live,none:live,sha256:27e09cb91d16e790752ac949b106d0b8c0842ff4a998858710db64445b7d7c1e,live,1970-01-01T00:00:03.1Z,2026-09-30T11:12:11.11Z,,,,
cell,M02,M02.cohort-v1,terminal_cohort,unbounded,unbounded,route,none,,,,available,0/1,ratio,ratio,0,1,,,,,,live,none:live,none:live,sha256:27e09cb91d16e790752ac949b106d0b8c0842ff4a998858710db64445b7d7c1e,live,,,,,,
cell,M02,M02.cohort-v1,terminal_cohort,unbounded,unbounded,route,verify_only,,,,available,1/1,ratio,ratio,1,1,,,,,,live,none:live,none:live,sha256:27e09cb91d16e790752ac949b106d0b8c0842ff4a998858710db64445b7d7c1e,live,,,,,,
cell,M02,M02.cohort-v1,terminal_cohort,unbounded,unbounded,route,verify_then_integrate,,,,available,1/1,ratio,ratio,1,1,,,,,,live,none:live,none:live,sha256:27e09cb91d16e790752ac949b106d0b8c0842ff4a998858710db64445b7d7c1e,live,,,,,,
metric,M49,M49.v1,activity_window,unbounded,unbounded,,,,,,unavailable:no_replay_suite,unavailable:no_replay_suite,missing,ratio,unavailable:no_replay_suite,unavailable:no_replay_suite,{},unavailable,unknown:coverage_unavailable,unknown:coverage_unavailable,not_applicable:not_a_cost_metric,live,none:live,none:live,sha256:0392b968194aadadc3046e7d5b134a90e21d86666424040e3f2506a0ea9d4db4,live,unavailable:no_event_in_cohort,2026-09-30T11:12:11.11Z,,,,
page,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,0,not_applicable:no_records,none:last_page
```

`export --metric M07 --drill numerator --page-size 2` (first page):

```json
{
  "manifest.page": {
    "first": true,
    "last": false,
    "next_cursor": "c2.7b226275636b6574223a2\u202617b80ef9b059",
    "next_cursor_expires_unix_ms": 1790768530196,
    "offset": 0,
    "page_size": 2,
    "rows": 2,
    "total": 5
  },
  "records": {
    "bucket": "numerator",
    "buckets": {
      "denominator": 2,
      "excluded.open": 2,
      "numerator": 5,
      "outcome.accepted": 2,
      "outcome.failed": 1
    },
    "definition": "M07.cohort-v1",
    "metric_id": "M07",
    "offset": 0,
    "page_size": 2,
    "rows": [
      {
        "decided_unix_ms": null,
        "entity": "attempt",
        "id": "t1-a1",
        "state": "completed",
        "task_id": "t1"
      },
      {
        "decided_unix_ms": null,
        "entity": "attempt",
        "id": "t2-a1",
        "state": "failed",
        "task_id": "t2"
      }
    ],
    "snapshot": {
      "content_digest": "sha256:456925caa359cea7a813ce349cd3d70c6c9250cbd8a4adda847dfbe86bb1779e",
      "revision": null
    },
    "total": 5
  }
}
```

The same page as CSV:

```csv
row_kind,metric_id,definition,cohort,window_from,window_to,dimension,dimension_value,record_bucket,record_entity,record_id,value_status,value,value_type,unit,numerator,denominator,exclusions,coverage,coverage_known,coverage_expected,cost_basis,projection_mode,projection_revision,projection_restated,content_digest,as_of,event_cutoff,observation_cutoff,attrs,page_offset,page_total,next_cursor
metric,M07,M07.cohort-v1,terminal_cohort,unbounded,unbounded,,,,,,available,5/2,ratio,attempts_per_accepted_task,5,2,"{""open"":2}",complete,3,3,not_applicable:not_a_cost_metric,live,none:live,none:live,sha256:456925caa359cea7a813ce349cd3d70c6c9250cbd8a4adda847dfbe86bb1779e,live,1970-01-01T00:00:03.1Z,2026-09-30T11:12:10.344Z,,,,
record,M07,M07.cohort-v1,,,,,,numerator,attempt,t1-a1,,,,,,,,,,,,,none:live,,sha256:456925caa359cea7a813ce349cd3d70c6c9250cbd8a4adda847dfbe86bb1779e,,,,"{""decided_unix_ms"":null,""state"":""completed"",""task_id"":""t1""}",,,
record,M07,M07.cohort-v1,,,,,,numerator,attempt,t2-a1,,,,,,,,,,,,,none:live,,sha256:456925caa359cea7a813ce349cd3d70c6c9250cbd8a4adda847dfbe86bb1779e,,,,"{""decided_unix_ms"":null,""state"":""failed"",""task_id"":""t2""}",,,
page,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,0,5,c2.7b226275636b6574223a2…14c87c8d6e9a
```

## 8. Restrictions

- Records exist only for native definitions (lane definitions answer
  `drill_unsupported`, as the query).
- Counters are JSON numbers as the query answers them, not integer strings;
  all current counters are far below 2^53.
- Continuation pages carry no summary; reconcile a multi-page export against
  its first page (`records.total` equals the drilled bucket's count).
- Cursor expiry and revocation are per user and key; there is no remote read
  interface in this card.

## 9. Weekly report companion (TM4.8)

The existing signed weekly report template ([workspace.md](workspace.md#10-owner-signed-telemetry-routines))
reads the JSON export service without an external destination. Its companion
keeps the `export.v1` manifest and adds `report {file, bytes, digest}` for the
Markdown bytes. The digest is SHA-256, prefixed `sha256:`. ISO-week filenames
are versioned on every rerun; reports and manifests never replace an existing
file. The report is all-time evidence generated weekly, with each metric’s
cohort/time basis, coverage and denominator, and typed unknown reasons.
