# Canonical dispatch enablement audit

Canonical prepared-launch dispatch is enabled in source and the rebuilt local
`target/release/herdr-projects` state-store executable following the authorized
2026-09-22 live controller workflow. This is the user's canonical-worker-launch
scope, not the whole project's release or agent-protocol certification checklist.

## Acceptance basis

The original T03.3/T03.4 requirements govern one-use effects, crash recovery and
visible blocking of ambiguity; T04.1–T04.5 govern capacity, queues, profiles,
budgets and authority; T05.2 governs retained worker inputs. The original
`06-scheduling-workers-and-integration.md` explicitly records launch, readiness,
prompt, stop, checkpoint acknowledgment, usage and resume independently.
`FrozenProfile::validate_for_launch` requires the first four capabilities.

Earlier versions of this audit conflated full W08 protocol certification with
launch admission. That would keep all launches disabled despite the designed
launchable/protocol-capable/certified distinction. Enabling the existing launch
contract does not change that contract or promote any optional capability.
The broader certification items below remain incomplete; they are not reported as
passed or silently inherited from Codex's successful launch test.

| Launch requirement | Authoritative evidence | Remaining restriction |
| --- | --- | --- |
| Trusted profile | Actual Codex 0.154.0 executable and capability-enabled Herdr identities; authenticated native readiness, exact prompt acknowledgment and observed stop; retained project-bound report and current-installation revalidation | Every installation must supply its own matching evidence. Version detection or JSON flags alone do not authorize launch. |
| Approval and admission | Live workflow invokes production preparation, draft, ephemeral Ed25519 signing/import and reservation; tampered grant refusal; fixture changed-executable refusal after signing; transactional reservation/capacity tests | Signed exact inputs, policy, current revisions and capacity remain required. |
| Controller dispatch | Authorized live workflow uses the production background ticker/controller and real native agent; existing multi-project fixtures cover fairness, stale queued cancellation and lost naming acknowledgment | Production gate now selects prepared launches; it does not automatically reserve arbitrary tasks or approve them. Test-only gate overrides were removed. |
| One-use native effects | Seven live contracts cover creation/gate acknowledgment loss, caller SIGKILL, Herdr restart with recycled pane identity, unobserved exit and historical bootstrap; store tests cover intent/receipt commit boundaries | Uncertain submission is observed, never blindly replayed. |
| Brief and initial memory | Real worker receives immutable retained instructions after PROJECT.md changes, produces both expected files, and receives one brief; snapshot tests cover scope, body integrity and complete framing budgets | This proves retained initial input, not live checkpoint/update protocol adherence. Mandatory overflow or invalid evidence refuses launch. |
| Stop and preservation | Live workflow restarts controller memory, observes agent-written report/library bytes, cancels, proves termination and releases capacity, then recovers exact bytes after deleting the disposable output source; native repository tests verify file/pack preservation | Vendor repository editing is not certified by the non-repository vendor test. Retained canonical worktrees have no automatic deletion authority. |
| Recovery and cleanup | Exact supervisor/session identity, unrelated replacement worker survival, idempotent recovery; mixed-root cleanup/reopen conflict tests; real historical bootstrap remains alive while worker stop refuses capacity release and preserves bytes | Unknown initial identity and historical same-boot bootstrap quiescence stay visibly blocked as permitted by T03.4. Live host reboot is not claimed. |
| Unsupported inputs | Dependency-evidence, transport, model/effort/environment and capability validation paths refuse unsupported requests; worker authority and signed memory promotion tests remain enforced | Tasks requiring unavailable dependency evidence are blocked, never treated as independent. Unknown usage is allowed only by explicit policy and is not a hard monetary limit. |

## Live evidence

The authorized `live_authenticated_controller_workflow` passed in 85.50 seconds;
its controller child passed in 48.00 seconds. The parent verified deletion of its
temporary home/login copy. Neither the parent nor driver manufactured the expected
worker outputs. Log: `/tmp/herdr-authorized-dispatch-workflow.log`.

That run used the old test gate override to select the same controller path.
Enablement removes the override and switches ordinary production selection on;
the full binary suite passed (525 tests, 87.27 seconds), and both multi-project
controller/ticker fixtures passed through that default path (12.05 seconds).
See the repair ledger for validation details. No additional authenticated prompt is needed merely
to replace the identical true test-selector value with the production default.

The native live contracts previously passed together: 7 tests, 25.19 seconds.
The broad optimized state-store audit covers 435 library, 524 binary, 48 CLI,
12 memory-control and 2 contract tests with passing results across the broad run
and focused reruns; the original CLI wrapper timed out near the end and two stale
expectations were fixed. Five protected-API compile-fail tests also passed.
These historical counts are not a claim that every later edit reran every suite.

## Explicitly uncompleted broader acceptance

Live memory-update/checkpoint acknowledgments, mixed-agent compatibility, vendor
repository editing and full workflow certificates remain untested. Native profile
reports still say `protocol_capable: false`, `certified: false` and carry no workflow
certificate. Resume and usage remain unknown unless independently verified.
No memory, W08 or full release card is closed by enabling dispatch.

See [the executable live test scope](live-dispatch-acceptance.md),
[launch contracts](canonical-worker-launch.md) and the
[repair ledger](memory-repair-progress.md) for details and limits. No existing
user project was migrated, approved or launched by these disposable tests.
