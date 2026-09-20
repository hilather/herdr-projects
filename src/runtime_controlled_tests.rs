use super::*;
use std::{fs,time::{Duration,Instant}};
use crate::{store::{controlled::ReadControl,SqliteStore,StoreError},execution_guard::ProjectGuard,reconcile::{ObservationBatch,RuntimeObservation,ResourceState}};
fn fixture()->(tempfile::TempDir,std::path::PathBuf,ObservationBatch) {
    let root=tempfile::tempdir().unwrap();let p=root.path().join("project");
    for dir in [".state","threads"] {fs::create_dir_all(p.join(dir)).unwrap();}
    fs::write(p.join("PROJECT.md"),"+++\nname='Fixture'\n+++\n").unwrap();fs::write(p.join(".state/project.json"),"{\"status\":\"paused\"}").unwrap();fs::write(p.join(".state/coordinator.json"),"{}").unwrap();
    let plan=migration::inspect(&p).unwrap();migration::apply(&p,&plan,true).unwrap();let before=snapshot(&p).unwrap();
    let observations=before.runtime_bindings.iter().map(|binding|RuntimeObservation{binding:binding.id.clone(),binding_revision:binding.revision,task_revision:None,observed_unix_ms:jiff::Timestamp::now().as_millisecond(),pane:ResourceState::Unrecorded,worktree:ResourceState::Unrecorded,agent_present:false,collector:"herdr-git-v2".into(),config_digest:None,diagnostic:"test observation".into(),session_identity:None,worktree_identity:None,agent_identity:None}).collect();
    (root,p,ObservationBatch{expected_head:before.head,observations,dispatch_allowed:false,recorded_head:None})
}
fn control()->ReadControl {ReadControl::new(Instant::now()+Duration::from_secs(5),Default::default())}
#[test]
fn controlled_observation_commit_retains_guard_and_cancellation_refuses_entry() {
    let(_root,p,batch)=fixture();let before=snapshot(&p).unwrap();let guard=ProjectGuard::acquire(&p).unwrap();let c=control();c.cancellation().cancel();
    assert!(record_controller_observations_controlled(&p,&batch,&guard,&c).is_err());assert_eq!(snapshot(&p).unwrap(),before);
    record_controller_observations_controlled(&p,&batch,&guard,&control()).unwrap();assert_eq!(snapshot(&p).unwrap().observations,batch.observations);assert!(ProjectGuard::acquire(&p).is_err());
}
#[test]
fn cancellation_after_observation_commit_preserves_db_and_recovery_repairs_marker() {
    let(_root,p,batch)=fixture();let guard=ProjectGuard::acquire(&p).unwrap();let c=control();
    let error=record_controller_controlled(&p,&batch,&guard,&c,||{
        // Inject a control transition in the retained ownership interval to
        // exercise DB-first publication recovery without requiring live agents.
        let mut db=SqliteStore::open(&p.join(".state/state.db")).unwrap();let s=db.read_snapshot(None).unwrap();
        db.set_project_state(s.head,s.control.unwrap().revision,crate::domain::ProjectState::Active,jiff::Timestamp::now().as_millisecond(),None).unwrap();c.cancellation().cancel();
    }).unwrap_err();
    assert!(error.to_string().contains("observations committed"));assert!(matches!(error.downcast_ref::<StoreError>(),Some(StoreError::Cancelled)));
    assert!(snapshot(&p).is_err());let stored=SqliteStore::open(&p.join(".state/state.db")).unwrap().read_snapshot(None).unwrap();assert_eq!(stored.observations,batch.observations);
    drop(guard);migration::recover(&p,true).unwrap();assert_eq!(snapshot(&p).unwrap(),stored);
}
#[test]
fn expiry_sql_is_interrupted_without_undoing_published_observations() {
    let(_root,p,batch)=fixture();let guard=ProjectGuard::acquire(&p).unwrap();let c=ReadControl::new(Instant::now()+Duration::from_millis(300),Default::default());
    let raw=rusqlite::Connection::open(p.join(".state/state.db")).unwrap();
    raw.execute_batch("ALTER TABLE operation_delivery RENAME TO original_delivery; CREATE VIEW operation_delivery AS SELECT 'fake' operation_id,'claimed' state,0 lease_until_ms WHERE (WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n)>0;").unwrap();
    let start=Instant::now();let error=record_controller_observations_controlled(&p,&batch,&guard,&c).unwrap_err();assert!(start.elapsed()<Duration::from_secs(2));assert!(error.to_string().contains("claim expiry incomplete"),"{error:#}");assert!(matches!(error.downcast_ref::<StoreError>(),Some(StoreError::Deadline)));
    assert_eq!(raw.query_row("SELECT count(*) FROM runtime_observations",[],|r|r.get::<_,usize>(0)).unwrap(),batch.observations.len());
}
