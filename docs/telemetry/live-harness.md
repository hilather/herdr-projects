# Steward live certification procedure

`tests/telemetry_live.rs` provides ignored, paid workflows for Claude Code,
Grok Build CLI, Muse and a newer Codex. Only the steward runs these outside
the worker sandbox. No certification status changes automatically.

Prepare a fresh private directory (0700) for each run. Place a private 0600
login copy there without displaying, parsing, hashing or committing it. The
worker never prepares the copy or accesses owner agent directories. Do not
reuse a home containing earlier sessions: their usage would contaminate totals.
The test clears inherited environment and sets HOME, all XDG directories,
CODEX_HOME and GROK_HOME inside that prepared home. Prepare `runtime` as 0700.

| Harness | Login destination inside prepared home | Flags to confirm before spending |
| --- | --- | --- |
| Claude Code | none: owner decision 2026-10-01, a long-lived `claude setup-token` token in a private 0600 file outside the home, passed via `HERDR_LIVE_TOKEN_FILE` + `HERDR_LIVE_TOKEN_ENV=CLAUDE_CODE_OAUTH_TOKEN` (the owner's OAuth credentials are never copied, so refresh-token rotation cannot sign them out) | TODO confirm noninteractive prompt, `--output-format json`, tools disabled and resume-last flags |
| Grok | TODO steward confirm login filename under `.grok`; do not guess or enumerate owner data | TODO confirm headless prompt, usage summary output and resume-last flags |
| Muse | TODO steward confirm login filenames under `.config/muse` | TODO confirm headless prompt, numeric usage JSON and whether resume exists |
| Codex | `.codex/auth.json` | TODO confirm `exec --json`, selected model, tool restrictions and `exec resume --last` for installed version |

The steward must resolve TODOs using known deployment documentation before
running. The fixed prompt includes `HERDR_LIVE_PRIVACY_LC0`; it requests one
word and no tools. CLI flags, rather than the prompt, must enforce tool policy.

Optional `HERDR_LIVE_TOKEN_FILE` (a private 0600 file) and `HERDR_LIVE_TOKEN_ENV` (the variable name) pass a login token to the harness process only; it is never logged or reported.

Set absolute `HERDR_LIVE_HOME`, `HERDR_LIVE_BIN`, and `HERDR_LIVE_OUT` (a new
report filename outside the prepared home), plus `HERDR_LIVE=1`.
Optional `HERDR_LIVE_PATH` supplies executable dependency directories (for
example the installed Node binary directory); its default is
`/usr/local/bin:/usr/bin:/bin`. No other owner environment is inherited.
`HERDR_LIVE_ARGS` is a shell-words string with a `{prompt}` placeholder. Quotes
and backslash escapes are supported; variable expansion, shell commands and
command substitution are not. Set `HERDR_LIVE_RESUME_ARGS` similarly for a
second turn in the same session wherever supported. Resume-last must refer to
this fresh home; do not supply an owner session identifier. Run only one test:

```sh
TMPDIR="$PWD/target/tmp" cargo test --locked --offline -j 3 --features state-store \
  --test telemetry_live claude_live -- --ignored --exact --test-threads=1
```

Substitute `grok_live`, `muse_live` or `codex_live`. Do not run the entire ignored
suite with a shared home. Each harness command has a 180-second deadline, null
stdin and private scratch stdout/stderr. Raw captures are deleted with scratch
and never copied into the report. Reports are created exclusively with 0600
permissions; reruns require a new output filename. Never attach raw captures,
login copies, prompts, environment dumps or tokens to review artifacts.

## Grok transport prerequisite

DG4a accepts authenticated OTLP JSON only. DG4e's installed Grok sends
HTTP/protobuf and ignores `OTEL_RESOURCE_ATTRIBUTES`. A direct endpoint does
not work. The test requires `HERDR_LIVE_OTLP_BRIDGE_BIN` (absolute) and
`HERDR_LIVE_OTLP_BRIDGE_ARGS`. This steward-supplied converter must bind
loopback on an ephemeral port, accept protobuf from this single attempt,
inject its canonical attempt ID, convert to JSON, and authenticate to the
product receiver. Arguments substitute `{receiver}`, `{token_file}`, `{attempt}` and
`{attempt_token_file}` (required). The latter is a fresh 0600 random credential
created in scratch for this attempt; the bridge must require its bearer value
on ingress. The harness receives only that attempt credential in OTLP headers. Its first stdout line must be JSON with an `endpoint` URL; it
must remain running and flush delivery before the agent exits. The test starts
a fresh product DG4a receiver; only the bridge receives its token-file path.
The agent receives no project bearer token. Confirm the bridge rejects requests without the attempt credential before
live execution. A bridge implementation
is not included in LC0. Do not claim direct Grok exporter compatibility.

The harness sets GROK_EXTERNAL_OTEL=1, OTEL_METRICS_EXPORTER=otlp,
OTEL_LOGS_EXPORTER=otlp, OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf and the
bridge endpoint, plus the attempt resource attribute and disabled content
export switches. Confirm exporter shutdown flush and bridge delivery.
Optional `HERDR_LIVE_USAGE_FILES` lists exact relative fresh-session
`.grok/sessions/.../signals.json` or `summary.json` paths, one per line.
Only numeric `usage` fields and model are parsed; an unrecognized schema is
`not_reported`, not proof of zero usage. Do not include both overlapping
summary and signals totals. Codex reads only `.codex/sessions/**/*.jsonl`,
using the last `thread_token_usage` per rollout. Claude parses JSON stdout
usage, normalizing input as uncached + cache read + cache creation.

## Reviewing a report

The report includes numeric ledger records/totals, own reported totals,
per-counter differences, unmapped OTLP attribute/resource keys, binding,
version and a marker scan over telemetry.db including WAL/SHM. It runs collect,
accounting sync, usage, attempts, collector bindings and OTLP records through
the CLI. Nonzero reported-counter differences or privacy hits fail after
writing the report. `not_reported` is an evidence gap, never equality.

Current product limitations matter: Muse has no established usage adapter;
Grok's OTLP evidence does not feed the accounting ledger; newer Codex versions
are deliberately excluded by the certified-version gate. These runs can expose
those gaps but cannot certify them by inventing counters or bypassing gates.
The ledger record list is not a complete inventory of excluded native evidence.
An unbound result, missing own totals, an unsupported schema or absent records
requires further adapter work before certification. Confirm at least two turns
where resume exists and compare every relevant counter, model and binding.

After review, retain the counts-only report and a certificate documenting
version, source revision, normalization, limitations and exact reconciliation.
A separate reviewed change may update the adapter's `certified_versions` and
individual `live` fields. `tests/telemetry_certification.rs` now enforces that
only adapters/versions with a recorded live report in its evidence registry
may claim live. Its existing Codex 0.154.0 evidence stays unchanged. Extend
that registry only with a reviewed certificate, and extend E2E adapter coverage
for newly observed formats. No file-content assertions are used for this rule.

Stop the bridge/receiver, remove the disposable execution home and private
login copy, delete any scratch remaining after interrupted runs, and retain
only the reviewed counts-only report. Never clean up an owner home, running
server or ticker. No new schema, retention class or backup category is added.
