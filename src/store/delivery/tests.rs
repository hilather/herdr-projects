use super::*;

#[test]
fn project_scope_uses_control_revision_without_inventing_a_task() {
    let temp=tempfile::tempdir().unwrap();let mut db=SqliteStore::create(&temp.path().join("state.db")).unwrap();
    let snapshot=db.read_snapshot(None).unwrap();
    db.set_project_state(snapshot.head,snapshot.control.unwrap().revision,ProjectState::Active,1000,None).unwrap();
    let snapshot=db.read_snapshot(None).unwrap();let revision=snapshot.control.as_ref().unwrap().revision;
    let id=OperationId::new("project-routine").unwrap();
    // Only a sealed routine service may produce this in production. Raw SQL is fixture setup.
    db.connection.execute("INSERT INTO operations VALUES(?1,NULL,'routine.run','routine:check',1,'{}',?2,?3,0,?1)",params![id.as_str(),format!("{:x}",Sha256::digest(b"{}")),integer(revision).unwrap()]).unwrap();
    let snapshot=db.read_snapshot(None).unwrap();assert!(snapshot.tasks.is_empty());
    assert!(snapshot.operations[0].task.is_none());
    assert!(db.commit(Commit{expected_head:snapshot.head,mutations:vec![Mutation::Enqueue(snapshot.operations[0].clone())]}).is_err());
    assert_eq!(db.read_snapshot(None).unwrap(),snapshot);
    let claim=db.claim_operation(&id,1,"routine-worker",1000,1000).unwrap();db.validate_claim(&claim,1001).unwrap();
    let snapshot=db.read_snapshot(None).unwrap();
    db.set_project_state(snapshot.head,revision,ProjectState::Paused,1001,None).unwrap();
    let before=db.read_snapshot(None).unwrap();assert!(db.validate_claim(&claim,1002).is_err());
    assert!(db.finish_operation(&claim,Outcome::Confirmed{observed_identity:"fixture receipt".into()},1002).is_err());
    assert_eq!(db.read_snapshot(None).unwrap(),before);
    db.expire_claims(2000).unwrap();
    assert_eq!(db.deliveries().unwrap()[0].state,DeliveryState::Ambiguous);
    assert!(db.read_snapshot(None).unwrap().tasks.is_empty());
}
fn fixture()->(tempfile::TempDir,SqliteStore,OperationId) {
    let temp=tempfile::tempdir().unwrap();let mut db=SqliteStore::create(&temp.path().join("state.db")).unwrap();
    let task=Task{id:TaskId::new("task").unwrap(),revision:1,state:TaskState::Ready,title:"fixture".into(),active_attempt:None};
    let id=OperationId::new("op").unwrap();
    let op=Operation{id:id.clone(),task:Some(task.id.clone()),kind:"fixture".into(),target:"disposable".into(),payload_version:1,payload:serde_json::json!({"key":"value"}),expected_revision:1,due_unix_ms:100,idempotency_key:"unique".into()};
    db.commit(Commit{expected_head:0,mutations:vec![Mutation::Task{expected:None,next:task},Mutation::Enqueue(op)]}).unwrap();
    (temp,db,id)
}
#[test]
fn claim_outcome_and_restart_preserve_intent_and_audit() {
    let (temp,mut db,id)=fixture();assert!(db.claim_operation(&id,1,"worker",99,1000).is_err());
    let claim=db.claim_operation(&id,1,"worker",100,1000).unwrap();
    assert!(db.claim_operation(&id,1,"second",100,1000).is_err());
    let done=db.finish_operation(&claim,Outcome::Confirmed{observed_identity:"resource-1".into()},101).unwrap();
    assert_eq!(done.state,DeliveryState::Confirmed);drop(db);
    let mut db=SqliteStore::open(&temp.path().join("state.db")).unwrap();assert_eq!(db.deliveries().unwrap(),vec![done]);
    assert_eq!(db.read_snapshot(None).unwrap().events.len(),4);
}
#[test]
fn expired_claim_is_ambiguous_and_old_owner_cannot_finish_over_new_epoch() {
    let (_temp,mut db,id)=fixture();let old=db.claim_operation(&id,1,"old",100,100).unwrap();
    assert!(db.finish_operation(&old,Outcome::Confirmed{observed_identity:"late".into()},200).is_err());
    assert_eq!(db.expire_claims(200).unwrap(),1);assert_eq!(db.expire_claims(201).unwrap(),0);
    let uncertain=db.deliveries().unwrap().remove(0);assert_eq!(uncertain.state,DeliveryState::Ambiguous);
    assert!(db.claim_operation(&id,uncertain.revision,"new",1000,100).is_err());
    let retry=db.observe_operation(&id,uncertain.revision,"reconciler",Outcome::Retryable{no_effect_evidence:"verified absent resource under exclusive ownership".into()},201).unwrap();
    assert!(db.claim_operation(&id,retry.revision,"new",202,100).is_err());
    let current=db.claim_operation(&id,retry.revision,"new",retry.next_due_ms,100).unwrap();assert!(current.epoch>old.epoch);
    assert!(db.finish_operation(&old,Outcome::Confirmed{observed_identity:"late".into()},retry.next_due_ms).is_err());
    db.finish_operation(&current,Outcome::Confirmed{observed_identity:"new-resource".into()},retry.next_due_ms+1).unwrap();
}
#[test]
fn only_no_effect_evidence_retries_and_stale_task_or_payload_blocks_claim() {
    let (_temp,mut db,id)=fixture();let claim=db.claim_operation(&id,1,"worker",100,1000).unwrap();
    assert!(db.finish_operation(&claim,Outcome::Retryable{no_effect_evidence:" ".into()},101).is_err());
    let retry=db.finish_operation(&claim,Outcome::Retryable{no_effect_evidence:"request rejected before dispatch".into()},101).unwrap();
    assert_eq!(retry.attempts,1);assert_eq!(retry.next_due_ms,1101);
    db.connection.execute("UPDATE tasks SET revision=2",[]).unwrap();assert!(db.claim_operation(&id,retry.revision,"worker",1101,1000).is_err());
    db.connection.execute("UPDATE tasks SET revision=1",[]).unwrap();db.connection.execute("UPDATE operations SET payload='{}'",[]).unwrap();
    assert!(matches!(db.claim_operation(&id,retry.revision,"worker",1101,1000),Err(StoreError::Corrupt(_))));
}
#[test]
fn concurrent_connections_have_one_claim_owner() {
    use std::sync::{Arc,Barrier};
    let (temp,mut db,id)=fixture();let barrier=Arc::new(Barrier::new(2));
    let workers:Vec<_>=(0..2).map(|n|{let path=temp.path().join("state.db");let id=id.clone();let barrier=barrier.clone();std::thread::spawn(move||{let mut db=SqliteStore::open(&path).unwrap();barrier.wait();db.claim_operation(&id,1,&format!("worker-{n}"),100,1000)})}).collect();
    let results:Vec<_>=workers.into_iter().map(|w|w.join().unwrap()).collect();assert_eq!(results.iter().filter(|r|r.is_ok()).count(),1);
    assert_eq!(db.deliveries().unwrap()[0].attempts,1);
}
#[test]
fn changed_entity_revision_fences_completion_without_losing_claim() {
    let (_temp,mut db,id)=fixture();let claim=db.claim_operation(&id,1,"worker",100,1000).unwrap();
    let snapshot=db.read_snapshot(None).unwrap();let mut task=snapshot.tasks[0].clone();task.revision+=1;task.title="changed after claim".into();
    db.commit(Commit{expected_head:snapshot.head,mutations:vec![Mutation::Task{expected:Some(1),next:task}]}).unwrap();
    assert!(matches!(db.finish_operation(&claim,Outcome::Confirmed{observed_identity:"stale-resource".into()},101),Err(StoreError::Conflict)));
    assert_eq!(db.deliveries().unwrap()[0].state,DeliveryState::Claimed);
    assert_eq!(db.expire_claims(1100).unwrap(),1);
    assert_eq!(db.deliveries().unwrap()[0].state,DeliveryState::Ambiguous);
}
#[test]
fn delivery_crash_child() {
    let Some(root)=std::env::var_os("HP_DELIVERY_CRASH_ROOT") else{return;};
    let root=std::path::PathBuf::from(root);let phase=std::env::var("HP_DELIVERY_CRASH_PHASE").unwrap();
    let mut db=SqliteStore::open(&root.join("state.db")).unwrap();let id=OperationId::new("op").unwrap();
    let claim=db.claim_operation(&id,1,"child",100,1000).unwrap();
    if phase!="before_effect" {
        use std::io::Write;
        let mut file=std::fs::OpenOptions::new().write(true).create_new(true).open(root.join("effect")).unwrap();file.write_all(b"one effect").unwrap();file.sync_all().unwrap();
    }
    if phase=="after_result" {db.finish_operation(&claim,Outcome::Confirmed{observed_identity:"fixture-effect".into()},101).unwrap();}
    std::fs::write(root.join("ready"),b"ready").unwrap();
    loop {std::thread::sleep(Duration::from_secs(1));}
}
#[test]
fn process_death_before_effect_after_effect_and_after_result_never_blindly_replays() {
    use std::{process::{Command,Stdio},time::Instant};
    for phase in ["before_effect","after_effect","after_result"] {
        let (temp,db,id)=fixture();drop(db);
        let mut child=Command::new(std::env::current_exe().unwrap()).args(["--exact","store::delivery::tests::delivery_crash_child","--nocapture"])
            .env("HP_DELIVERY_CRASH_ROOT",temp.path()).env("HP_DELIVERY_CRASH_PHASE",phase).stdout(Stdio::null()).stderr(Stdio::inherit()).spawn().unwrap();
        let deadline=Instant::now()+Duration::from_secs(10);
        while !temp.path().join("ready").exists() {
            if Instant::now()>deadline||child.try_wait().unwrap().is_some(){let _=child.kill();let _=child.wait();panic!("child failed to reach {phase}");}
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().unwrap();child.wait().unwrap();
        let mut db=SqliteStore::open(&temp.path().join("state.db")).unwrap();db.expire_claims(1100).unwrap();
        let record=db.deliveries().unwrap().remove(0);
        assert_eq!(record.state,if phase=="after_result"{DeliveryState::Confirmed}else{DeliveryState::Ambiguous});
        assert!(db.claim_operation(&id,record.revision,"replacement",1101,1000).is_err());
        assert_eq!(temp.path().join("effect").exists(),phase!="before_effect");
        if phase!="before_effect" {assert_eq!(std::fs::read(temp.path().join("effect")).unwrap(),b"one effect");}
        if phase!="after_result" {assert_eq!(db.read_snapshot(None).unwrap().events.last().unwrap().payload["epoch"].as_u64(),Some(record.epoch));}
    }
}
#[test]
fn retry_budget_and_input_bounds_preserve_delivery_invariants() {
    let (_temp,mut db,id)=fixture();let mut record=db.deliveries().unwrap().remove(0);
    assert!(db.claim_operation(&id,record.revision,"",100,1000).is_err());
    assert!(db.claim_operation(&id,record.revision,"owner",100,300_001).is_err());
    assert!(db.claim_operation(&id,record.revision,"owner",i64::MAX,1000).is_err());
    for n in 1..=32 {
        let now=record.next_due_ms.max(100);
        let claim=db.claim_operation(&id,record.revision,"owner",now,1000).unwrap();
        record=db.finish_operation(&claim,Outcome::Retryable{no_effect_evidence:"transport rejected before sending".into()},now+1).unwrap();
        assert_eq!(record.attempts,n);
    }
    assert_eq!(record.state,DeliveryState::PermanentFailure);
    assert!(db.claim_operation(&id,record.revision,"owner",record.next_due_ms,1000).is_err());
}

#[test]
fn retirement_refuses_an_owned_claim_until_expiry_without_erasing_intent() {
    let(_temp,mut db,id)=fixture();db.claim_operation(&id,1,"owner",100,1000).unwrap();let before=db.read_snapshot(None).unwrap();let revision=before.deliveries[0].revision;
    assert!(db.retire_operation(&id,revision,before.head,"stop attempts",101).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
    db.expire_claims(1100).unwrap();let expired=db.read_snapshot(None).unwrap();let retired=db.retire_operation(&id,expired.deliveries[0].revision,expired.head,"no longer needed",1101).unwrap();assert_eq!(retired.state,DeliveryState::PermanentFailure);assert_eq!(db.read_snapshot(None).unwrap().operations,before.operations);
}
