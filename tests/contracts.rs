#[path = "../contracts/phase_b.rs"]
mod domain;
use domain::*;
#[test]
fn ambiguity_never_authorizes_blind_retry_and_lost_attempt_keeps_capacity() {
    assert!(!ExternalOutcome::Ambiguous { observation_required: "inspect pane identity".into() }.permits_blind_retry());
    let attempt = Attempt { id: AttemptId("a".into()), task: TaskId("t".into()), revision: Revision(2), state: AttemptState::Lost, snapshot: SnapshotId("s".into()), reservation: "slot".into(), termination_observed: false };
    assert!(attempt.retains_capacity());
    let restored: Attempt = serde_json::from_str(&serde_json::to_string(&attempt).unwrap()).unwrap();
    assert_eq!(attempt, restored);
}
#[test]
fn fixture_and_stale_results_cannot_pass_as_verified() {
    let binding = EvidenceBinding { repository: "r".into(), commit: "c".into(), tree: "t".into(), integration_base: "b".into(), snapshot: SnapshotId("s".into()), criteria_hash: "h".into() };
    let result = ResultManifest { id: ResultId("r".into()), attempt: AttemptId("a".into()), binding: binding.clone(), artifact_hashes: vec![], commands: vec![], disposition: Disposition::FixtureOnly };
    assert_eq!(completion_gate(&result, &binding), Gate::FixtureOnly);
    let mut changed = binding;
    changed.integration_base = "new base".into();
    assert_eq!(completion_gate(&result, &changed), Gate::Stale);
}
