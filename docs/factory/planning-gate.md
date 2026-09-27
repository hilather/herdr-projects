# Planning gate

Deterministic simulator for ten logical workers. This is not a live pilot and not a spend approval. The record states that live F2.7 is not run.

The correction work adds a [production wait/replan entry path](waits.md) with
bounded controller replay and advisory notices. That document distinguishes
implemented deadlines, approval/capacity/owned-runtime triggers, explicit renewal,
and opt-in feedback-to-request servicing from the remaining planner execution,
automatic renewal and broader resource/user-decision work. It does not upgrade this simulator
record into live acceptance.

[Retained planner sessions](planner-sessions.md) add immutable intent/event input
snapshots and version-2 proposal bindings with restart and parent-revision checks.
This supplies a persistence boundary for proposal producers; automatic inference
and the complete executable-contract decomposition workflow remain incomplete.

The harness in `tests/factory_harness.rs` opens a disposable current-schema store and drives the controller APIs. It does not call a live provider. It does not poll pull requests.

Ten queued workers share one project. Five wait conditions are registered (`dependency_evidence`, `user_decision`, `resource_availability`, `adapter_recovery`, `validation_completion`). Replaying a wait does not prove the condition. Two workers write the same path; one is reserved and the other stays blocked. One worker fails verification. That same blocker is rejected three times so the replan budget can reach its cap: two automatic replans, then one escalation. The third does not insert a plan proposal. The other nine workers are not verified.

The harness records zero duplicate attempt ids and zero false satisfactions. A stored claim that checks passed is not evidence. A rejected run does not insert `verified_results` or `dependency_satisfactions`. An infrastructure retry of the same attempt does not mint another attempt id.

`factory_admission` defaults to `off`. The harness enables its disposable fixture
with SQL; that fixture shortcut is not production authority. Production activation
uses the separate authenticated `factory admission` policy ingress. A passing
simulator does not authorize activation or establish the live gate.
