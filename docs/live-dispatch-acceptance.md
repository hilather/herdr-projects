# Disposable authenticated dispatch acceptance

This test passed with explicit user authorization on 2026-09-22 (85.50 seconds;
controller child 48.00 seconds). It supplies the live controller evidence in the
[dispatch audit](dispatch-enablement.md), within the limits listed below.

The existing one-token diagnostic proved readiness and native prompt submission.
This next test connects the actual native profile proof to signed reservation and
the complete background ticker/controller. It requires separate authorization for
account use and a worker task, beyond the earlier diagnostic's no-tools prompt.

## Exact authorized activity

- Create disposable projects, an isolated native Herdr server, an ephemeral owner
  signing key and a temporary Codex home. Copy only the explicitly selected login
  file; never copy user configuration, hooks or plugins.
- Run one fixed no-tools profile-verification prompt and retain its real native
  capability report. Revalidate that report through normal draft/reservation APIs.
- Sign the exact disposable launch grant with the ephemeral key. Verify a tampered
  grant is refused, then import the valid signature and reserve through production
  admission. No user signing key or real project is involved.
- Run the ticker with prepared-launch dispatch enabled. Send one retained task brief
  instructing the agent to write exactly two files in its attempt output directory:
  `report.md` containing `CANONICAL_RETAINED_MEMORY_OK` and `library/result.txt`
  containing `CANONICAL_WORKER_RESULT_OK`, each followed by a newline.
- Trust only the exact disposable probe/work directories. Use Codex's workspace
  write sandbox, with only the disposable project added to writable roots, and
  disable network access for its tools. Account/model access still consumes usage.
- Change PROJECT.md after snapshot reservation; the task must use the retained
  instructions. The fixture never writes the two expected output files.
- Restart ticker memory after one confirmed brief; observe both agent-written
  outputs, request cancellation and prove supervised termination. Verify one
  creation, release, start and termination plus one brief attempt.
- Delete only the disposable output source and recover exact bytes through the
  recorded termination receipt. Stop any recorded worker on driver failure and
  remove the temporary home/login copy. The controller loop is bounded to 180
  seconds; the profile probe is separately bounded to 120 seconds.

## Run after authorization

```sh
CARGO_HOME=/tmp/herdr-projects-cargo scripts/test-live-canonical-dispatch \
  --herdr /tmp/herdr-capability-live \
  --agent /home/brewerm/.local/share/mise/installs/codex/0.154.0/bin/codex \
  --auth-file /home/brewerm/.codex/auth.json \
  --allow-authenticated-workflow
```

The script builds optimized library and controller test executables and selects
only `canonical_worker::tests::live_authenticated_controller_workflow`. Ordinary
unit test runs ignore this test; the child driver is inert without its explicit
project environment. There is no environment variable that bypasses production launch authority.

The passing run establishes live controller dispatch, retained initial instructions,
agent-authored outputs, controller restart and started-worker preservation. It
would not establish memory-update/checkpoint protocol adherence, mixed-agent
compatibility, repository work by a vendor agent, or a full workflow certificate.
Those requirements remain separate in the dispatch audit. Do not change the
production gate solely because this harness compiles or the earlier diagnostic
passed.
