# Steward live certification procedure

`tests/telemetry_live.rs` provides ignored, paid workflows for Claude Code,
Grok Build CLI, Muse, Devin and a newer Codex. Only the steward runs these outside
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
| Grok | none: owner decision 2026-10-01, an xAI API key in a private 0600 file via `HERDR_LIVE_TOKEN_FILE` + `HERDR_LIVE_TOKEN_ENV=XAI_API_KEY` (never copy `~/.grok/auth.json`: its OAuth refresh tokens rotate) | TODO confirm headless prompt, usage summary output and resume-last flags |
| Muse | none: owner decision 2026-10-01, `HERDR_LIVE_ENV=XDG_CONFIG_HOME=<owner's ~/.config>` uses the login in place (no copy, so refreshes stay in one file; the release binary ignores the launcher's `MUSE_AUTH_PATH`), with sessions and data still in the throwaway home; run the installed release binary directly, not the self-updating launcher | TODO confirm headless prompt, numeric usage JSON and whether resume exists |
| Devin | none by default: `devin auth login` is interactive; the steward supplies a private login in the throwaway home (never copy owner `~/.local/share/devin` or `~/.config/devin` in) and an API-key variable via `HERDR_LIVE_TOKEN_*` if Devin documents one; run the installed `3000.11.3` binary directly | TODO confirm `-p`/`--print` prompt (or `-- <prompt>`), `--permission-mode` that disables tools, `--respect-workspace-trust false`, and `-c`/`--continue` for turn two; stdout carries no usage JSON |
| Codex | `.codex/auth.json` | TODO confirm `exec --json`, selected model, tool restrictions and `exec resume --last` for installed version |

The steward must resolve TODOs using known deployment documentation before
running. The fixed prompt includes `HERDR_LIVE_PRIVACY_LC0`; it requests one
word and no tools. CLI flags, rather than the prompt, must enforce tool policy.

Optional `HERDR_LIVE_ENV` passes non-secret `NAME=value` lines to the harness process. Optional `HERDR_LIVE_TOKEN_FILE` (a private 0600 file) and `HERDR_LIVE_TOKEN_ENV` (the variable name) pass a login token to the harness process only; it is never logged or reported.

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

## Direct Grok and Muse receiver (DG4h)

`grok_live` and `muse_live` start a fresh product loopback receiver and mint a
600-second token scoped to the fixture project and canonical attempt. The
harness receives only this attempt token through
`OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer <attempt token>`, with
`OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` and the receiver's HTTP endpoint.
No external bridge or bridge environment variables are required. Grok's ignored
`OTEL_RESOURCE_ATTRIBUTES` no longer prevents binding. The resource attribute
is also supplied for exporters that support it; a conflicting attempt is
quarantined rather than rebound. The project credential remains private to the
product receiver.

The test sets GROK_EXTERNAL_OTEL=1, OTEL_METRICS_EXPORTER=otlp,
OTEL_LOGS_EXPORTER=otlp and disabled content export switches. Confirm exporter
shutdown flush before stopping the receiver. Compression, gRPC and traces are
unsupported; `/v1/traces` returns 404. Muse's installed evidence establishes
only its endpoint switch, so the steward must verify that its build honors
bearer headers and exports to `/v1/logs` and `/v1/metrics`. DG4h does not bypass
service/version gates: Muse must report `service.name=tbh` and the accepted
`service.version=1.4.0-R4161.1`; `app.version` alone remains uncertified. These
are live evidence gaps until the disposable owner-approved run confirms them.

For other launchers, mint with
`telemetry <slug> otlp mint-token --attempt <id> --seconds 600`. Capture the
JSON output privately: `token` is returned once; `token_hash` is the nonsecret
revocation handle. Set the headers above only in that attempt's environment.
Revoke with `telemetry <slug> otlp revoke-token --token-hash <hash>` when the
attempt ends. Never pass the project token to an agent. No product launch-env
helper exists yet; the steward or launcher sets these variables explicitly.

Optional `HERDR_LIVE_USAGE_FILES` lists exact relative fresh-session
`.grok/sessions/.../signals.json` or `summary.json` paths, one per line.
Only numeric `usage` fields and model are parsed; an unrecognized schema is
`not_reported`, not proof of zero usage. Do not include both overlapping
summary and signals totals. Codex reads only `.codex/sessions/**/*.jsonl`,
using the last `thread_token_usage` per rollout. Claude parses JSON stdout
usage, normalizing input as uncached + cache read + cache creation.

## Reviewing a report

The report selects accepted `basis=delta` accounting entries for numeric
ledger records/totals. Cumulative thread entries are reconciliation evidence
and must never be summed with deltas (LC1 corrected this double counting).
It includes own reported totals,
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

Stop the receiver, remove the disposable execution home and private
login copy, delete any scratch remaining after interrupted runs, and retain
only the reviewed counts-only report. Never clean up an owner home, running
server or ticker. DG4h adds OTLP stream migration 0003 and retained hashed-token/revocation
metadata in full sidecar backups; plaintext attempt credentials are never stored.

DG4i: `muse_live` reads its execution home's
`.local/share/muse/sessions/**/session.jsonl`, including `subagent/<child>`
files, for `harness_usage`. It counts only `runtime.session` model_completed
outer event ids once per path session. Input includes explicit cache read/write
counts, output includes reasoning; combined cached_tokens and repeated
attribution events are excluded. Native fixture certification precedes the
steward's live ledger reconciliation; public-build OTLP export may be disabled.

DG4k: `devin_live` (e.g. `HERDR_LIVE_ARGS='-p {prompt} --permission-mode auto'`)
sets the DG4h receiver, bearer header and `OTEL_RESOURCE_ATTRIBUTES`, and writes
`otel.enabled=true` (prompt/tool logging disabled) to the home's
`.config/devin/config.json` only if absent. Version is `devin --version`'s semver
(`3000.11.3`; the parenthesized build `9c803229faa4` is diagnostics). Devin's
stdout has no usage JSON, so set `HERDR_LIVE_USAGE_FILES` to the fresh
`sessions.db`/`cli_sessions.db` path(s) relative to the home (find them yourself
after a run: the harness never walks the home). The harness reads only numeric
leaves of `sessions.metadata` (`input_tokens`/`output_tokens`/
`cache_read_tokens`/`cache_creation_tokens`/cache/total aliases; other numeric
keys are reported as `unmapped_keys`, never `message_nodes`). Devin is
fixture-only and outside the ledger, so `ledger_totals` are the exact-bound
`api_request` rows and `metric_reconciliation` sums DELTA `devin.token.usage`.
TODOs: confirm export is enabled by the config key (else add the real key),
exporter event names/prefix, whether input is cache-inclusive, and whether
`sessions.metadata` carries a stable usage schema; a stable one justifies a
native `devin` adapter (bound by `sessions.working_directory`, sessions read
only from recorded `devin`-kind execution homes). Recording a live report and
adding it to the certification registry remains a separate reviewed change.
