# Planning gate

Deterministic simulator for ten logical workers. This is not a live pilot and not a spend approval. The record states that live F2.7 is not run.

The harness in `tests/factory_harness.rs` opens a disposable schema-36 store and drives the controller APIs. It does not call a live provider. It does not poll pull requests.

Ten queued workers share one project. Five wait conditions are registered (`dependency_evidence`, `user_decision`, `resource_availability`, `adapter_recovery`, `validation_completion`). Replaying a wait does not prove the condition. Two workers write the same path; one is reserved and the other stays blocked. One worker fails verification. That same blocker is rejected three times so the replan budget can reach its cap: two automatic replans, then one escalation. The third does not insert a plan proposal. The other nine workers are not verified.

The harness records zero duplicate attempt ids and zero false satisfactions. A stored claim that checks passed is not evidence. A rejected run does not insert `verified_results` or `dependency_satisfactions`. An infrastructure retry of the same attempt does not mint another attempt id.

`factory_admission` stays `off` in production code. The harness turns that column on with SQL on the fixture database. That statement is not a library function and is not linked into `herdr-projects`.
