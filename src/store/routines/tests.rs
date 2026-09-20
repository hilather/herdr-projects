use super::*;
fn fixture(missed:MissedRunPolicy)->(tempfile::TempDir,SqliteStore,RoutineDefinition) {
    let temp=tempfile::tempdir().unwrap();let path=temp.path().join("state.db");let mut db=SqliteStore::create(&path).unwrap();
    let before=db.read_snapshot(None).unwrap();db.set_project_state(before.head,before.control.unwrap().revision,ProjectState::Active,0,Some(&"a".repeat(64))).unwrap();
    let d=RoutineDefinition{version:1,name:"check".into(),revision:1,project_store:path.canonicalize().unwrap().display().to_string(),
        authority:VersionedReference{id:"owner".into(),revision:1,digest:"b".repeat(64)},config:crate::migration::ConfigReference{path:temp.path().join("owner.toml").display().to_string(),digest:Some("a".repeat(64))},
        enabled:true,schedule:"every 1m".into(),timezone:"UTC".into(),start_unix_ms:1000,missed,overlap:OverlapPolicy::Skip,
        script:temp.path().join("check.sh").display().to_string(),script_sha256:"c".repeat(64),cwd:temp.path().display().to_string(),deadline_ms:1000,output_cap_bytes:4000};
    let head=db.read_snapshot(None).unwrap().head;db.install_routine(&PreparedRoutine{definition:d.clone()},head).unwrap();(temp,db,d)
}
fn tick(db:&mut SqliteStore,d:&RoutineDefinition,now:i64)->Result<Option<RoutineOccurrence>> {
    let head=db.read_snapshot(None)?.head;db.schedule_routine(&PreparedRoutineTick{definition:d.clone(),now},head)
}
#[test]
fn occurrence_cursor_and_outbox_commit_together_and_reopen_deduplicates() {
    let(temp,mut db,d)=fixture(MissedRunPolicy::CoalesceLatest);let before=db.read_snapshot(None).unwrap();
    db.connection.execute_batch("CREATE TRIGGER fail_occurrence BEFORE INSERT ON events WHEN NEW.kind='routine.occurrence_recorded' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(tick(&mut db,&d,181_000).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
    db.connection.execute_batch("DROP TRIGGER fail_occurrence;").unwrap();
    let o=tick(&mut db,&d,181_000).unwrap().unwrap();assert_eq!(o.slots,4);assert_eq!(o.disposition,RoutineDisposition::Enqueued);
    assert_eq!(o.scheduled_unix_ms,181_000);let before=db.read_snapshot(None).unwrap();
    assert!(before.tasks.is_empty());assert_eq!(before.operations.len(),1);assert_eq!(before.deliveries.len(),1);
    drop(db);let mut db=SqliteStore::open(&temp.path().join("state.db")).unwrap();
    for now in [181_000,180_000,181_001] {assert!(tick(&mut db,&d,now).unwrap().is_none());assert_eq!(db.read_snapshot(None).unwrap(),before);}
    let overlap=tick(&mut db,&d,241_000).unwrap().unwrap();assert_eq!(overlap.disposition,RoutineDisposition::SkippedOverlap);
    assert!(overlap.operation.is_none());assert_eq!(db.read_snapshot(None).unwrap().operations.len(),1);
    db.connection.execute("UPDATE routine_cursors SET after_unix_ms=999999",[]).unwrap();
    assert!(db.read_snapshot(None).is_err());
}
#[test]
fn missed_policy_skips_a_bounded_window_and_revisions_do_not_reuse_occurrences() {
    let(_temp,mut db,mut d)=fixture(MissedRunPolicy::Skip);
    let o=tick(&mut db,&d,121_000).unwrap().unwrap();assert_eq!(o.slots,3);assert_eq!(o.disposition,RoutineDisposition::SkippedMissed);
    assert!(db.read_snapshot(None).unwrap().operations.is_empty());
    let o=tick(&mut db,&d,181_000).unwrap().unwrap();assert!(o.operation.is_some());
    d.revision+=1;d.missed=MissedRunPolicy::CoalesceLatest;
    let head=db.read_snapshot(None).unwrap().head;db.install_routine(&PreparedRoutine{definition:d.clone()},head).unwrap();
    assert_eq!(db.deliveries().unwrap()[0].state,DeliveryState::PermanentFailure);
    let next=tick(&mut db,&d,181_000).unwrap().unwrap();assert_ne!(next.id,o.id);assert_eq!(next.disposition,RoutineDisposition::Enqueued);
    let before=db.read_snapshot(None).unwrap();assert!(db.install_routine(&PreparedRoutine{definition:d},before.head).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
}
#[test]
fn uncertain_claim_history_blocks_overlap_across_revisions_and_retirement() {
    let(_temp,mut db,mut d)=fixture(MissedRunPolicy::CoalesceLatest);let o=tick(&mut db,&d,1000).unwrap().unwrap();
    // Simulate a prior claimed adapter whose cleanup has not been certified.
    db.connection.execute("UPDATE operation_delivery SET attempts=1,epoch=1,state='ambiguous' WHERE operation_id=?1",[o.operation.unwrap().as_str()]).unwrap();
    d.revision+=1;let head=db.read_snapshot(None).unwrap().head;db.install_routine(&PreparedRoutine{definition:d.clone()},head).unwrap();
    for state in ["ambiguous","permanent_failure","confirmed"] {
        db.connection.execute("UPDATE operation_delivery SET state=?1",[state]).unwrap();
        let now=match state {"ambiguous"=>61_000,"permanent_failure"=>121_000,_=>181_000};
        assert_eq!(tick(&mut db,&d,now).unwrap().unwrap().disposition,RoutineDisposition::SkippedOverlap);
    }
}
#[test]
fn competing_ticks_have_one_winner_and_disabled_or_paused_routines_do_not_enqueue() {
    use std::sync::{Arc,Barrier};
    let(temp,mut db,d)=fixture(MissedRunPolicy::CoalesceLatest);let head=db.read_snapshot(None).unwrap().head;
    let barrier=Arc::new(Barrier::new(2));let jobs:Vec<_>=(0..2).map(|_|{let path=temp.path().join("state.db");let barrier=barrier.clone();let d=d.clone();std::thread::spawn(move||{let mut db=SqliteStore::open(&path).unwrap();barrier.wait();db.schedule_routine(&PreparedRoutineTick{definition:d,now:1000},head)})}).collect();
    let results:Vec<_>=jobs.into_iter().map(|j|j.join().unwrap()).collect();assert_eq!(results.iter().filter(|r|r.is_ok()).count(),1);
    let before=db.read_snapshot(None).unwrap();assert_eq!(before.routine_occurrences.len(),1);assert_eq!(before.operations.len(),1);
    let mut disabled=d.clone();disabled.revision+=1;disabled.enabled=false;
    db.install_routine(&PreparedRoutine{definition:disabled.clone()},before.head).unwrap();let before=db.read_snapshot(None).unwrap();
    assert!(tick(&mut db,&disabled,181_000).unwrap().is_none());assert_eq!(db.read_snapshot(None).unwrap(),before);
    db.set_project_state(before.head,before.control.unwrap().revision,ProjectState::Paused,2000,None).unwrap();
    assert!(tick(&mut db,&d,241_000).is_err());
}

#[test]
fn schema15_upgrade_preserves_existing_outbox_without_inventing_routine_authority() {
    let temp=tempfile::tempdir().unwrap();let mut db=SqliteStore::create(&temp.path().join("state.db")).unwrap();
    let task=Task{id:TaskId::new("task").unwrap(),revision:1,state:TaskState::Draft,title:"fixture".into(),active_attempt:None};
    let operation=Operation{id:OperationId::new("old-operation").unwrap(),task:Some(task.id.clone()),kind:"fixture".into(),target:"fixture".into(),payload_version:1,payload:serde_json::json!({}),expected_revision:1,due_unix_ms:0,idempotency_key:"old-operation".into()};
    db.commit(Commit{expected_head:0,mutations:vec![Mutation::Task{expected:None,next:task},Mutation::Enqueue(operation.clone())]}).unwrap();
    db.claim_operation(&operation.id,1,"old-worker",1000,1000).unwrap();
    db.connection.execute_batch("DROP TABLE memory_invalidations; DROP TABLE memory_promotions; DROP TABLE review_decisions; DROP TABLE proposal_validations; DROP TABLE memory_proposals; DROP TABLE coordinator_checkpoints; DROP TABLE coordinator_sessions; DROP TABLE memory_subscriptions; DROP TABLE snapshot_entries; DROP TABLE memory_snapshots; DROP TABLE memory_validity; DROP TABLE memory_dependencies; DROP TABLE memory_heads; DROP TABLE memory_revisions; DROP TABLE memory_records; DROP TABLE objects; DROP TABLE authority_denials; DROP TABLE memory_policies; DROP TABLE routine_occurrences; DROP TABLE routine_cursors; DROP TABLE routine_revisions; UPDATE store_meta SET schema_version=15; PRAGMA user_version=15;").unwrap();
    let mut before=db.read_snapshot(None).unwrap();db.upgrade_v1().unwrap();before.schema_version=22;
    assert_eq!(db.read_snapshot(None).unwrap(),before);assert!(before.routine_revisions.is_empty());assert!(before.routine_occurrences.is_empty());
    db.integrity_check().unwrap();
}

fn claimed_receipt(db:&mut SqliteStore,d:&RoutineDefinition,cleanup:bool,success:bool)->RoutineReceipt {
    let o=tick(db,d,1000).unwrap().unwrap();let id=o.operation.unwrap();
    db.connection.execute("UPDATE operation_delivery SET revision=2,attempts=1,epoch=1,state='claimed',owner='routine-linux-namespace-v1',lease_until_ms=31000 WHERE operation_id=?1",[id.as_str()]).unwrap();
    RoutineReceipt{operation:id.clone(),routine:o.routine,claim:crate::operations::Claim{operation:id,revision:2,epoch:1,owner:"routine-linux-namespace-v1".into(),lease_until_ms:31000},finished_unix_ms:1001,cleanup_verified:cleanup,succeeded:success,
        stdout:b"result".to_vec(),stderr:vec![],stdout_truncated:false,stderr_truncated:false,stdout_total_bytes:6,stderr_total_bytes:0,elapsed_ms:1}
}
#[test]
fn completion_inbox_outcome_and_overlap_release_are_atomic_and_survive_reopen() {
    for success in [true,false] {
        let(temp,mut db,d)=fixture(MissedRunPolicy::CoalesceLatest);let r=claimed_receipt(&mut db,&d,true,success);
        let before=db.read_snapshot(None).unwrap();
        db.connection.execute_batch("CREATE TRIGGER fail_completion BEFORE INSERT ON events WHEN NEW.kind='routine.completed' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(db.complete_routine(&crate::routines::CompletedRoutine{receipt:r.clone()}).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
        db.connection.execute_batch("DROP TRIGGER fail_completion;").unwrap();
        db.complete_routine(&crate::routines::CompletedRoutine{receipt:r.clone()}).unwrap();
        let after=db.read_snapshot(None).unwrap();assert_eq!(after.routine_receipts,vec![r.clone()]);assert_eq!(after.inbox.len(),1);
        assert_eq!(after.deliveries[0].state,if success {DeliveryState::Confirmed}else{DeliveryState::PermanentFailure});
        assert!(db.complete_routine(&crate::routines::CompletedRoutine{receipt:r.clone()}).is_err());assert_eq!(db.read_snapshot(None).unwrap(),after);
        drop(db);let mut db=SqliteStore::open(&temp.path().join("state.db")).unwrap();assert_eq!(db.read_snapshot(None).unwrap(),after);
        assert_eq!(tick(&mut db,&d,61_000).unwrap().unwrap().disposition,RoutineDisposition::Enqueued);
        db.connection.execute("UPDATE events SET payload='[]' WHERE kind='routine.completed'",[]).unwrap();
        assert!(db.read_snapshot(None).is_err());assert!(tick(&mut db,&d,121_000).is_err());
    }
}
#[test]
fn unknown_cleanup_generic_confirmation_and_no_effect_retry_never_release_overlap() {
    use crate::operations::Outcome;
    let(_temp,mut db,d)=fixture(MissedRunPolicy::CoalesceLatest);let r=claimed_receipt(&mut db,&d,false,false);
    db.complete_routine(&crate::routines::CompletedRoutine{receipt:r.clone()}).unwrap();
    assert_eq!(tick(&mut db,&d,61_000).unwrap().unwrap().disposition,RoutineDisposition::SkippedOverlap);
    let delivery=db.deliveries().unwrap().remove(0);
    db.observe_operation(&r.operation,delivery.revision,"fixture",Outcome::Retryable{no_effect_evidence:"generic evidence cannot replay scripts".into()},61_001).unwrap();
    let delivery=db.deliveries().unwrap().remove(0);
    assert!(db.claim_operation(&r.operation,delivery.revision,"fixture",62_002,1000).is_err());
    assert_eq!(tick(&mut db,&d,121_000).unwrap().unwrap().disposition,RoutineDisposition::SkippedOverlap);
    db.connection.execute("UPDATE operation_delivery SET state='confirmed'",[]).unwrap();
    assert_eq!(tick(&mut db,&d,181_000).unwrap().unwrap().disposition,RoutineDisposition::SkippedOverlap);
}
#[test]
fn stale_or_misbound_completion_cannot_release_a_claim() {
    let(_temp,mut db,d)=fixture(MissedRunPolicy::CoalesceLatest);let r=claimed_receipt(&mut db,&d,true,true);let before=db.read_snapshot(None).unwrap();
    for case in 0..5 {let mut bad=r.clone();match case {0=>bad.claim.epoch+=1,1=>bad.routine.revision+=1,2=>bad.finished_unix_ms=bad.claim.lease_until_ms,3=>bad.stdout_total_bytes=0,_=>bad.cleanup_verified=false};
        assert!(db.complete_routine(&crate::routines::CompletedRoutine{receipt:bad}).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
    }
    db.expire_claims(31_001).unwrap();let expired=db.read_snapshot(None).unwrap();
    assert!(db.complete_routine(&crate::routines::CompletedRoutine{receipt:r}).is_err());assert_eq!(db.read_snapshot(None).unwrap(),expired);
    assert_eq!(tick(&mut db,&d,61_000).unwrap().unwrap().disposition,RoutineDisposition::SkippedOverlap);
}
