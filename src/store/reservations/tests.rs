use super::*;
use crate::reconcile::RuntimeObservation;
fn fixture()->(tempfile::TempDir,SqliteStore,Vec<PreparedLaunch>) {
    let temp=tempfile::tempdir().unwrap();let path=temp.path().join("state.db");let mut db=SqliteStore::create(&path).unwrap();
    db.commit(Commit{expected_head:0,mutations:["a","b"].into_iter().map(|id|Mutation::Task{expected:None,next:Task{id:TaskId::new(id).unwrap(),revision:1,state:TaskState::Draft,title:id.into(),active_attempt:None}}).collect()}).unwrap();
    for id in ["a","b"] {let id=TaskId::new(id).unwrap();let h=db.read_snapshot(None).unwrap().head;db.create_runtime(Some(&id),Some(1),h,&RuntimeRoute::default()).unwrap();let h=db.read_snapshot(None).unwrap().head;db.queue_task(&id,2,h,&QueueRequest{priority:0,dependencies:vec![]},0).unwrap();}
    let s=db.read_snapshot(None).unwrap();db.set_scheduler_policy(s.head,1,1,3).unwrap();let s=db.read_snapshot(None).unwrap();
    let observations=s.runtime_bindings.iter().map(|b|RuntimeObservation{binding:b.id.clone(),binding_revision:b.revision,task_revision:Some(3),observed_unix_ms:1000,collector:"herdr-git-v1".into(),..RuntimeObservation::default()}).collect::<Vec<_>>();
    db.record_observations(s.head,&observations).unwrap();let s=db.read_snapshot(None).unwrap();db.set_project_state(s.head,s.control.unwrap().revision,ProjectState::Active,1000,None).unwrap();
    let s=db.read_snapshot(None).unwrap();let mut prepared:Vec<PreparedLaunch>=s.runtime_bindings.iter().map(|b|PreparedLaunch{inputs:LaunchInputs{version:2,project_store:std::fs::canonicalize(&path).unwrap().display().to_string(),task:b.task.clone().unwrap(),task_revision:3,scheduler_revision:s.scheduler.as_ref().unwrap().policy.revision,control_epoch:s.control.as_ref().unwrap().epoch,binding:b.id.clone(),binding_revision:b.revision,binding_digest:super::super::ownership::identity_digest(b).unwrap(),profile:crate::domain::profile::fixture(crate::migration::ConfigReference{path:temp.path().join("config.toml").display().to_string(),digest:None}).reference().unwrap(),effective_profile:Some(crate::domain::profile::fixture(crate::migration::ConfigReference{path:temp.path().join("config.toml").display().to_string(),digest:None})),approval:VersionedReference{id:"fixture-approval".into(),revision:1,digest:"b".repeat(64)},config:crate::migration::ConfigReference{path:temp.path().join("config.toml").display().to_string(),digest:None},repositories:vec![],dependencies:vec![],memory:None,budget:None}}).collect();
    for p in &mut prepared {
        let grant=ApprovalGrant{version:1,scope:ApprovalScope::for_launch(&p.inputs).unwrap(),policy:p.inputs.effective_profile.as_ref().unwrap().permission_policy.clone(),issued_unix_ms:0,expires_unix_ms:100_000};
        let head=db.read_snapshot(None).unwrap().head;p.inputs.approval=db.install_approval(&PreparedApproval{grant},head,1000).unwrap();
    }
    (temp,db,prepared)
}
fn reserve(db:&mut SqliteStore,p:&[PreparedLaunch])->Reservation {let h=db.read_snapshot(None).unwrap().head;db.reserve_prepared(p,h,1000).unwrap()}
#[test]
fn disk_config_change_withdraws_launch_authority_before_claim_or_effect() {
    for claimed in [false,true] {
        let(_temp,mut db,p)=fixture();let r=reserve(&mut db,&p);
        let claim=claimed.then(||db.claim_operation(&r.record.operation,1,"worker",1000,1000).unwrap());
        std::fs::write(&p[0].inputs.config.path,"# owner edited config\n").unwrap();
        let before=db.read_snapshot(None).unwrap();
        if let Some(claim)=claim {assert!(db.validate_claim(&claim,1001).is_err());}
        else {assert!(db.claim_operation(&r.record.operation,1,"worker",1001,1000).is_err());}
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        assert!(before.attempts[0].retains_capacity());
    }
}
#[test]
fn changed_attempt_state_refuses_claim_and_pre_effect_without_consuming_new_authority() {
    for claimed in [false,true] { for terminated in [false,true] {
        let(_temp,mut db,p)=fixture();let r=reserve(&mut db,&p);
        let claim=claimed.then(||db.claim_operation(&r.record.operation,1,"worker",1000,1000).unwrap());
        let snapshot=db.read_snapshot(None).unwrap();let mut attempt=snapshot.attempts[0].clone();
        attempt.revision+=1;attempt.state=if terminated {AttemptState::Cancelled}else{AttemptState::Lost};attempt.termination_observed=terminated;
        db.commit(Commit{expected_head:snapshot.head,mutations:vec![Mutation::Attempt{expected:Some(1),next:attempt}]}).unwrap();
        let before=db.read_snapshot(None).unwrap();
        if let Some(claim)=claim {assert!(db.validate_claim(&claim,1001).is_err());}
        else {assert!(db.claim_operation(&r.record.operation,1,"worker",1001,1000).is_err());}
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        let uses:u64=db.connection.query_row("SELECT count(*) FROM approval_uses",[],|r|r.get(0)).unwrap();assert_eq!(uses,u64::from(claimed));
    }}
}

#[test]
fn schema12_launches_upgrade_without_fabricating_grants_or_releasing_capacity() {
    for claimed in [false,true] {
        let(temp,mut db,p)=fixture();let r=reserve(&mut db,&p);
        let claim=claimed.then(||db.claim_operation(&r.record.operation,1,"worker",1000,1000).unwrap());
        db.connection.execute_batch("DROP TABLE approval_uses; DROP TABLE approval_revocations; DROP TABLE approval_grants; UPDATE store_meta SET schema_version=12; PRAGMA user_version=12;").unwrap();
        let before=db.read_snapshot(None).unwrap();db.upgrade_v1().unwrap();let after=db.read_snapshot(None).unwrap();
        assert_eq!(after.head,before.head);assert_eq!(after.events,before.events);assert_eq!(after.attempt_inputs,before.attempt_inputs);assert_eq!(after.deliveries,before.deliveries);assert!(after.approvals.is_empty());
        drop(db);let mut db=SqliteStore::open(&temp.path().join("state.db")).unwrap();
        if let Some(claim)=claim {assert!(db.validate_claim(&claim,1001).is_err());}
        else {assert!(db.claim_operation(&r.record.operation,1,"worker",1001,1000).is_err());}
        assert!(db.read_snapshot(None).unwrap().attempts[0].retains_capacity());
    }
}
#[test]
fn launch_approval_use_survives_restart_and_cannot_be_reused_after_no_effect_retry() {
    let(temp,mut db,p)=fixture();let r=reserve(&mut db,&p);
    let claim=db.claim_operation(&r.record.operation,1,"worker",1000,10_000).unwrap();
    db.validate_claim(&claim,1001).unwrap();drop(db);
    let mut db=SqliteStore::open(&temp.path().join("state.db")).unwrap();db.validate_claim(&claim,1002).unwrap();
    db.finish_operation(&claim,Outcome::Retryable{no_effect_evidence:"fixture proves no effect".into()},1003).unwrap();
    let before=db.read_snapshot(None).unwrap();let revision=db.deliveries().unwrap()[0].revision;
    assert!(db.claim_operation(&r.record.operation,revision,"worker",3000,1000).is_err());
    assert_eq!(db.read_snapshot(None).unwrap(),before);
    assert_eq!(db.connection.query_row("SELECT count(*) FROM approval_uses",[],|r|r.get::<_,u64>(0)).unwrap(),1);
    assert!(db.connection.execute("DELETE FROM approval_uses",[]).is_err());
}

#[test]
fn approval_consumption_rolls_back_if_claim_write_fails() {
    let(_temp,mut db,p)=fixture();let r=reserve(&mut db,&p);let before=db.read_snapshot(None).unwrap();
    db.connection.execute_batch("CREATE TRIGGER fail_claim BEFORE UPDATE ON operation_delivery BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(db.claim_operation(&r.record.operation,1,"worker",1000,1000).is_err());
    assert_eq!(db.read_snapshot(None).unwrap(),before);
    assert_eq!(db.connection.query_row("SELECT count(*) FROM approval_uses",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    db.connection.execute_batch("DROP TRIGGER fail_claim;").unwrap();
    db.claim_operation(&r.record.operation,1,"worker",1000,1000).unwrap();
}

#[test]
fn revoked_or_expired_approval_never_authorizes_launch_or_releases_capacity() {
    for already_claimed in [false,true] {
        let(_temp,mut db,p)=fixture();let r=reserve(&mut db,&p);
        let claim=already_claimed.then(||db.claim_operation(&r.record.operation,1,"worker",1000,1000).unwrap());
        let h=db.read_snapshot(None).unwrap().head;db.revoke_approval(&p[0].inputs.approval.id,h,1001,"operator withdrew permission").unwrap();
        if let Some(claim)=claim {assert!(db.validate_claim(&claim,1002).is_err());}
        else {assert!(db.claim_operation(&r.record.operation,1,"worker",1002,1000).is_err());}
        assert!(db.read_snapshot(None).unwrap().attempts[0].retains_capacity());
        assert!(db.connection.execute("DELETE FROM approval_revocations",[]).is_err());
    }
    let(_temp,mut db,p)=fixture();let r=reserve(&mut db,&p);let before=db.read_snapshot(None).unwrap();
    assert!(db.claim_operation(&r.record.operation,1,"worker",100_000,1000).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
}

#[test]
fn missing_or_corrupt_approval_blocks_claim_without_consuming_it() {
    for corrupt in [false,true] {
        let(_temp,mut db,p)=fixture();let r=reserve(&mut db,&p);
        db.connection.execute_batch("DROP TRIGGER approval_grants_no_delete; DROP TRIGGER approval_grants_no_update;").unwrap();
        if corrupt {db.connection.execute("UPDATE approval_grants SET payload='{}' WHERE id=?1",[&p[0].inputs.approval.id]).unwrap();}
        else {db.connection.execute("DELETE FROM approval_grants WHERE id=?1",[&p[0].inputs.approval.id]).unwrap();}
        assert!(db.claim_operation(&r.record.operation,1,"worker",1000,1000).is_err());
        assert_eq!(db.deliveries().unwrap()[0].state,DeliveryState::Pending);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM approval_uses",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    }
}
#[test]
fn incomplete_or_changed_profile_evidence_cannot_reserve_capacity() {
    let(_temp,mut db,p)=fixture();let before=db.read_snapshot(None).unwrap();
    for field in 0..5 {
        let mut bad=p[0].clone();
        match field {
            0=>{bad.inputs.version=1;bad.inputs.effective_profile=None;},
            1=>bad.inputs.effective_profile=None,
            2=>bad.inputs.effective_profile.as_mut().unwrap().agent.version="9.9.9".into(),
            3=>bad.inputs.effective_profile.as_mut().unwrap().config.digest=Some("f".repeat(64)),
            _=>{let profile=bad.inputs.effective_profile.as_mut().unwrap();profile.capabilities.stop=CapabilityEvidence::Unknown;bad.inputs.profile=profile.reference().unwrap();},
        }
        assert!(db.reserve_prepared(&[bad],before.head,1000).is_err());
        assert_eq!(db.read_snapshot(None).unwrap(),before);
    }
}

#[test]
fn version_one_input_serialization_preserves_historical_identity() {
    let(_temp,_db,p)=fixture();let mut old=p[0].inputs.clone();old.version=1;old.effective_profile=None;
    let bytes=serde_json::to_vec(&old).unwrap();assert!(!String::from_utf8_lossy(&bytes).contains("effective_profile"));
    let record:LaunchInputs=serde_json::from_slice(&bytes).unwrap();validate_inputs(&record).unwrap();
    assert_eq!(record_ids(&old).unwrap(),record_ids(&record).unwrap());
    assert_eq!(serde_json::to_vec(&record).unwrap(),bytes);
}

#[test]
fn schema11_upgrade_retains_records_and_blocks_old_format_insertions() {
    let(temp,mut db,p)=fixture();
    db.connection.execute_batch("DROP TABLE approval_uses; DROP TABLE approval_revocations; DROP TABLE approval_grants; DROP TRIGGER attempt_inputs_effective_profile; UPDATE store_meta SET schema_version=11; PRAGMA user_version=11;").unwrap();
    // Reproduce a schema-11/v1 historical reservation, before v2's producer existed.
    let old_json=include_str!("../../../tests/fixtures/launch-inputs-v1.json").trim().replace(&"a".repeat(64),&p[0].inputs.binding_digest);
    let inputs:LaunchInputs=serde_json::from_str(&old_json).unwrap();
    assert_eq!(serde_json::to_string(&inputs).unwrap(),old_json);
    let (attempt,operation)=record_ids(&inputs).unwrap();
    let record=AttemptInputRecord{attempt:attempt.clone(),operation:operation.clone(),inputs};
    let payload=serde_json::to_string(&record).unwrap();let digest=format!("{:x}",Sha256::digest(payload.as_bytes()));
    let tx=db.connection.transaction().unwrap();
    tx.execute("INSERT INTO attempts VALUES(?1,'a',1,'reserved',NULL,?2,0)",params![attempt.as_str(),format!("worker:{}",attempt.as_str())]).unwrap();
    tx.execute("UPDATE tasks SET revision=4,state='running',active_attempt=?1 WHERE id='a'",params![attempt.as_str()]).unwrap();
    tx.execute("INSERT INTO operations VALUES(?1,'a','runtime.launch',?2,1,?3,?4,4,1000,?1)",params![operation.as_str(),record.inputs.binding,payload,digest]).unwrap();
    tx.execute("INSERT INTO attempt_inputs VALUES(?1,?2,?3,?4)",params![attempt.as_str(),operation.as_str(),payload,digest]).unwrap();
    event(&tx,"attempt.reserved",attempt.as_str(),1,&record).unwrap();
    let task=read_tasks(&tx).unwrap().into_iter().find(|t|t.id.as_str()=="a").unwrap();
    event(&tx,"task.changed","a",4,&task).unwrap();event(&tx,"operation.enqueued",operation.as_str(),4,&record).unwrap();
    tx.commit().unwrap();
    let before=db.read_snapshot(None).unwrap();
    assert!(matches!(db.reserve_prepared(&p,before.head,1000),Err(StoreError::UnsupportedSchema(11))));
    db.upgrade_v1().unwrap();let after=db.read_snapshot(None).unwrap();
    assert_eq!(after.schema_version,13);assert_eq!(after.attempt_inputs,before.attempt_inputs);assert_eq!(after.events,before.events);
    assert_eq!(after.head,before.head);
    assert!(db.connection.execute("INSERT INTO attempt_inputs VALUES('old','old','{\"inputs\":{\"version\":1}}',?1)",params!["a".repeat(64)]).is_err());
    assert_eq!(after.attempt_inputs[0],record);
    assert_eq!(serde_json::to_string(&after.attempt_inputs[0]).unwrap(),payload);
    drop(db);let mut db=SqliteStore::open(&temp.path().join("state.db")).unwrap();
    assert_eq!(db.read_snapshot(None).unwrap().attempt_inputs,after.attempt_inputs);
    assert!(db.cancel_attempt(&attempt,1,after.head,"cancel historical reservation",1001).unwrap().released);
}
#[test]
fn reservation_and_never_claimed_cancellation_survive_restart() {
    let(temp,mut db,p)=fixture();let r=reserve(&mut db,&p);assert_eq!(r.record.inputs.task.as_str(),"a");let s=db.read_snapshot(None).unwrap();assert_eq!(s.attempt_inputs,vec![r.record.clone()]);assert_eq!(s.attempts.len(),1);assert!(s.attempts[0].retains_capacity());assert_eq!(db.queue_report(1000).unwrap().available_slots,0);
    assert!(db.connection.execute("UPDATE attempt_inputs SET payload='{}'",[]).is_err());assert!(db.connection.execute("DELETE FROM attempt_inputs",[]).is_err());
    drop(db);let mut db=SqliteStore::open(&temp.path().join("state.db")).unwrap();let c=db.cancel_attempt(&r.record.attempt,1,r.head,"operator request",1001).unwrap();assert!(c.released);assert_eq!(db.queue_report(1001).unwrap().available_slots,1);assert_eq!(db.deliveries().unwrap()[0].state,DeliveryState::PermanentFailure);assert!(db.claim_operation(&r.record.operation,1,"worker",1002,1000).is_err());let s=db.read_snapshot(None).unwrap();assert_eq!(s.tasks[0].state,TaskState::Cancelled);assert_eq!(s.tasks[0].active_attempt,None);assert!(db.cancel_attempt(&r.record.attempt,c.attempt_revision,c.head,"operator request",1002).unwrap().released);assert_eq!(db.read_snapshot(None).unwrap(),s);
}
#[test]
fn stale_or_cross_store_preparations_and_rollback_never_leak_reservations() {
    let(_temp,mut db,p)=fixture();let before=db.read_snapshot(None).unwrap();
    for field in 0..7 {let mut bad=p[0].clone();match field {0=>bad.inputs.task_revision+=1,1=>bad.inputs.scheduler_revision+=1,2=>bad.inputs.control_epoch+=1,3=>bad.inputs.binding_revision+=1,4=>bad.inputs.binding_digest="f".repeat(64),5=>bad.inputs.project_store="/tmp/another-store".into(),_=>bad.inputs.config.digest=Some("c".repeat(64))};assert!(db.reserve_prepared(&[bad],before.head,1000).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);}
    db.connection.execute_batch("CREATE TRIGGER fail_input BEFORE INSERT ON attempt_inputs BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();assert!(db.reserve_prepared(&p,before.head,1000).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
}
#[test]
fn competing_reservations_cannot_exceed_capacity_even_with_a_fresh_head() {
    use std::sync::{Arc,Barrier};let(temp,mut db,p)=fixture();let h=db.read_snapshot(None).unwrap().head;let gate=Arc::new(Barrier::new(2));let workers:Vec<_>=p.into_iter().map(|p|{let gate=gate.clone();let path=temp.path().join("state.db");std::thread::spawn(move||{let mut db=SqliteStore::open(&path).unwrap();gate.wait();let result=db.reserve_prepared(&[p.clone()],h,1000);(result,p)})}).collect();let results:Vec<_>=workers.into_iter().map(|w|w.join().unwrap()).collect();assert_eq!(results.iter().filter(|(r,_)|r.is_ok()).count(),1);let loser=&results.iter().find(|(r,_)|r.is_err()).unwrap().1;let h=db.read_snapshot(None).unwrap().head;assert!(matches!(db.reserve_prepared(&[loser.clone()],h,1000),Err(StoreError::Invalid(s)) if s.contains("capacity")));assert_eq!(db.read_snapshot(None).unwrap().attempts.len(),1);
}
#[test]
fn retry_history_and_lost_attempts_retain_capacity_on_cancel() {
    for lost in [false,true] {let(_temp,mut db,p)=fixture();let r=reserve(&mut db,&p);let claim=db.claim_operation(&r.record.operation,1,"worker",1000,1000).unwrap();db.finish_operation(&claim,Outcome::Retryable{no_effect_evidence:"fixture rejected before dispatch".into()},1001).unwrap();assert!(db.connection.execute("UPDATE operation_delivery SET attempts=0,epoch=0",[]).is_err());if lost {db.connection.execute("UPDATE attempts SET state='lost'",[]).unwrap();}let h=db.read_snapshot(None).unwrap().head;let c=db.cancel_attempt(&r.record.attempt,1,h,"cancel uncertain",1002).unwrap();assert!(!c.released);assert_eq!(db.queue_report(1002).unwrap().available_slots,0);let s=db.read_snapshot(None).unwrap();assert!(s.attempts[0].retains_capacity());assert_eq!(s.cancellations.len(),1);let revision=db.deliveries().unwrap()[0].revision;assert!(db.claim_operation(&r.record.operation,revision,"worker",3000,1000).is_err());}
}
#[test]
fn cancellation_failure_rolls_back_retirement_and_capacity_release() {
    let(_temp,mut db,p)=fixture();let r=reserve(&mut db,&p);let before=db.read_snapshot(None).unwrap();db.connection.execute_batch("CREATE TRIGGER fail_cancel BEFORE INSERT ON attempt_cancellations BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();assert!(db.cancel_attempt(&r.record.attempt,1,r.head,"cancel",1001).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
}
#[test]
fn claim_and_cancellation_race_never_releases_a_claimed_worker() {
    use std::sync::{Arc,Barrier};let(temp,mut db,p)=fixture();let r=reserve(&mut db,&p);let gate=Arc::new(Barrier::new(2));let path=temp.path().join("state.db");let op=r.record.operation.clone();let g=gate.clone();let claimant=std::thread::spawn(move||{let mut db=SqliteStore::open(&path).unwrap();g.wait();db.claim_operation(&op,1,"worker",1001,1000)});gate.wait();let cancelled=db.cancel_attempt(&r.record.attempt,1,r.head,"cancel",1001);let claimed=claimant.join().unwrap();assert_ne!(cancelled.is_ok(),claimed.is_ok());if claimed.is_ok(){let h=db.read_snapshot(None).unwrap().head;assert!(!db.cancel_attempt(&r.record.attempt,1,h,"cancel",1002).unwrap().released);}let s=db.read_snapshot(None).unwrap();assert_eq!(s.attempts[0].retains_capacity(),claimed.is_ok());
}
#[test]
fn orphan_launches_refuse_reads_and_upgrade_rolls_back() {
    let(_temp,mut db,p)=fixture();let r=reserve(&mut db,&p);db.connection.execute_batch("DROP TRIGGER attempt_inputs_no_delete; DELETE FROM attempt_inputs;").unwrap();assert!(db.read_snapshot(None).is_err());db.connection.execute_batch("DROP TABLE approval_uses; DROP TABLE approval_revocations; DROP TABLE approval_grants; DROP TRIGGER operation_delivery_monotonic; DROP TABLE attempt_inputs; DROP TABLE attempt_cancellations; UPDATE store_meta SET schema_version=10; PRAGMA user_version=10;").unwrap();assert!(db.upgrade_v1().is_err());let version:u32=db.connection.query_row("PRAGMA user_version",[],|r|r.get(0)).unwrap();assert_eq!(version,10);assert_eq!(db.read_snapshot(None).unwrap().operations[0].id,r.record.operation);
}
#[test]
fn cancellation_without_launch_proof_retains_the_attempt() {
    let(_temp,mut db,_p)=fixture();let s=db.read_snapshot(None).unwrap();let id=AttemptId::new("adopted").unwrap();db.commit(Commit{expected_head:s.head,mutations:vec![Mutation::Attempt{expected:None,next:Attempt{id:id.clone(),task:TaskId::new("a").unwrap(),revision:1,state:AttemptState::Running,snapshot:None,reservation:"adopted-slot".into(),termination_observed:false}}]}).unwrap();let h=db.read_snapshot(None).unwrap().head;assert!(!db.cancel_attempt(&id,1,h,"request stop",1000).unwrap().released);assert_eq!(db.queue_report(1000).unwrap().available_slots,0);
}
#[test]
fn schema10_upgrade_preserves_nonzero_claim_history() {
    let(_temp,mut db,p)=fixture();let r=reserve(&mut db,&p);db.claim_operation(&r.record.operation,1,"worker",1000,1000).unwrap();db.connection.execute_batch("DROP TABLE approval_uses; DROP TABLE approval_revocations; DROP TABLE approval_grants; DROP TRIGGER operation_delivery_monotonic; DROP TABLE attempt_inputs; DROP TABLE attempt_cancellations; UPDATE operations SET kind='fixture'; UPDATE store_meta SET schema_version=10; PRAGMA user_version=10;").unwrap();let before=db.deliveries().unwrap();db.upgrade_v1().unwrap();assert_eq!(db.deliveries().unwrap(),before);assert!(db.read_snapshot(None).unwrap().attempt_inputs.is_empty());assert!(db.connection.execute("UPDATE operation_delivery SET attempts=0",[]).is_err());
}
#[test]
fn missing_parent_is_not_hidden_from_an_open_store() {
    let(_temp,mut db,p)=fixture();let _r=reserve(&mut db,&p);db.connection.execute_batch("PRAGMA foreign_keys=OFF; DELETE FROM operation_delivery; DELETE FROM operations;").unwrap();assert!(read_inputs(&db.connection).is_err());
}
#[test]
fn reservation_crash_child() {
    let Some(root)=std::env::var_os("HP_RESERVATION_CRASH_ROOT") else{return;};let root=std::path::PathBuf::from(root);let phase=std::env::var("HP_RESERVATION_CRASH_PHASE").unwrap();let mut db=SqliteStore::open(&root.join("state.db")).unwrap();let inputs:LaunchInputs=serde_json::from_slice(&std::fs::read(root.join("inputs.json")).unwrap()).unwrap();
    if phase=="committed" {reserve(&mut db,&[PreparedLaunch{inputs}]);}
    else {db.connection.execute_batch("BEGIN IMMEDIATE; INSERT INTO attempts VALUES('interrupted','a',1,'reserved',NULL,'interrupted-slot',0); UPDATE tasks SET revision=revision+1,state='running',active_attempt='interrupted' WHERE id='a';").unwrap();}
    std::fs::write(root.join("ready"),b"ready").unwrap();loop {std::thread::sleep(Duration::from_secs(1));}
}
#[test]
fn process_death_preserves_whole_reservation_or_original_queue() {
    use std::{process::{Command,Stdio},time::Instant};
    for phase in ["uncommitted","committed"] {let(temp,mut db,p)=fixture();let before=db.read_snapshot(None).unwrap();drop(db);std::fs::write(temp.path().join("inputs.json"),serde_json::to_vec(&p[0].inputs).unwrap()).unwrap();let mut child=Command::new(std::env::current_exe().unwrap()).args(["--exact","store::reservations::tests::reservation_crash_child","--nocapture"]).env("HP_RESERVATION_CRASH_ROOT",temp.path()).env("HP_RESERVATION_CRASH_PHASE",phase).stdout(Stdio::null()).stderr(Stdio::inherit()).spawn().unwrap();let deadline=Instant::now()+Duration::from_secs(10);while !temp.path().join("ready").exists() {if Instant::now()>deadline||child.try_wait().unwrap().is_some(){let _=child.kill();let _=child.wait();panic!("child failed to reach {phase}");}std::thread::sleep(Duration::from_millis(10));}child.kill().unwrap();child.wait().unwrap();let mut db=SqliteStore::open(&temp.path().join("state.db")).unwrap();let after=db.read_snapshot(None).unwrap();if phase=="uncommitted" {assert_eq!(after,before);}else{assert_eq!(after.attempt_inputs.len(),1);assert_eq!(after.operations.len(),1);assert_eq!(after.tasks[0].active_attempt,Some(after.attempts[0].id.clone()));assert_eq!(db.queue_report(1000).unwrap().available_slots,0);assert_eq!(db.deliveries().unwrap()[0].state,DeliveryState::Pending);}}
}
