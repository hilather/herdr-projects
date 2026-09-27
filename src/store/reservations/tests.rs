use super::*;
use crate::reconcile::RuntimeObservation;
pub(in crate::store) fn fixture()->(tempfile::TempDir,SqliteStore,Vec<PreparedLaunch>) {
    fixture_at(15)
}
fn fixture_at(version:u32)->(tempfile::TempDir,SqliteStore,Vec<PreparedLaunch>) {
    let temp=tempfile::tempdir().unwrap();std::fs::create_dir(temp.path().join(".state")).unwrap();let path=temp.path().join(".state/state.db");
    let mut db=if version==14 {
        std::fs::write(&path,[]).unwrap();let mut connection=connect(&path).unwrap();
        let tx=connection.transaction().unwrap();
        for sql in [
            include_str!("../../../migrations/0001_project_store.sql"),include_str!("../../../migrations/0002_legacy_import.sql"),
            include_str!("../../../migrations/0003_operation_delivery.sql"),include_str!("../../../migrations/0004_canonical_inbox.sql"),
            include_str!("../../../migrations/0005_runtime_bindings.sql"),include_str!("../../../migrations/0006_runtime_observations.sql"),
            include_str!("../../../migrations/0007_project_control.sql"),include_str!("../../../migrations/0008_canonical_runtime.sql"),
            include_str!("../../../migrations/0009_runtime_ownership.sql"),include_str!("../../../migrations/0010_scheduler_queue.sql"),
            include_str!("../../../migrations/0011_attempt_inputs.sql"),include_str!("../../../migrations/0012_effective_profiles.sql"),
            include_str!("../../../migrations/0013_scoped_approvals.sql"),include_str!("../../../migrations/0014_admission_budgets.sql")
        ] {tx.execute_batch(sql).unwrap();}
        tx.commit().unwrap();drop(connection);SqliteStore::open(&path).unwrap()
    } else {SqliteStore::create(&path).unwrap()};
    db.commit(Commit{expected_head:0,mutations:["a","b"].into_iter().map(|id|Mutation::Task{expected:None,next:Task{id:TaskId::new(id).unwrap(),revision:1,state:TaskState::Draft,title:id.into(),active_attempt:None}}).collect()}).unwrap();
    for id in ["a","b"] {let id=TaskId::new(id).unwrap();let h=db.read_snapshot(None).unwrap().head;db.create_runtime(Some(&id),Some(1),h,&RuntimeRoute::default()).unwrap();let h=db.read_snapshot(None).unwrap().head;db.queue_task(&id,2,h,&QueueRequest{priority:0,dependencies:vec![]},0).unwrap();}
    let s=db.read_snapshot(None).unwrap();db.set_scheduler_policy(s.head,1,1,3).unwrap();let s=db.read_snapshot(None).unwrap();
    let observations=s.runtime_bindings.iter().map(|b|RuntimeObservation{binding:b.id.clone(),binding_revision:b.revision,task_revision:Some(3),observed_unix_ms:1000,collector:"herdr-git-v1".into(),..RuntimeObservation::default()}).collect::<Vec<_>>();
    db.record_observations(s.head,&observations).unwrap();let s=db.read_snapshot(None).unwrap();db.set_project_state(s.head,s.control.unwrap().revision,ProjectState::Active,1000,None).unwrap();
    let s=db.read_snapshot(None).unwrap();let mut prepared:Vec<PreparedLaunch>=s.runtime_bindings.iter().map(|b|PreparedLaunch{inputs:LaunchInputs{task_contract: None, version:2,project_store:std::fs::canonicalize(&path).unwrap().display().to_string(),task:b.task.clone().unwrap(),task_revision:3,scheduler_revision:s.scheduler.as_ref().unwrap().policy.revision,control_epoch:s.control.as_ref().unwrap().epoch,binding:b.id.clone(),binding_revision:b.revision,binding_digest:super::super::ownership::identity_digest(b).unwrap(),profile:crate::domain::profile::fixture(crate::migration::ConfigReference{path:temp.path().join("config.toml").display().to_string(),digest:None}).reference().unwrap(),effective_profile:Some(crate::domain::profile::fixture(crate::migration::ConfigReference{path:temp.path().join("config.toml").display().to_string(),digest:None})),approval:VersionedReference{id:"fixture-approval".into(),revision:1,digest:"b".repeat(64)},config:crate::migration::ConfigReference{path:temp.path().join("config.toml").display().to_string(),digest:None},repositories:vec![],dependencies:vec![],memory:None,budget:None}}).collect();
    for p in &mut prepared {
        let grant=ApprovalGrant{version:1,scope:ApprovalScope::for_launch(&p.inputs).unwrap(),policy:p.inputs.effective_profile.as_ref().unwrap().permission_policy.clone(),issued_unix_ms:0,expires_unix_ms:100_000};
        let head=db.read_snapshot(None).unwrap().head;p.inputs.approval=db.install_approval(&PreparedApproval{grant},head,1000).unwrap();
    }
    (temp,db,prepared)
}
fn reserve(db:&mut SqliteStore,p:&[PreparedLaunch])->Reservation {let h=db.read_snapshot(None).unwrap().head;db.reserve_prepared(p,h,1000).unwrap()}

#[test]
fn satisfaction_flag_does_not_reserve_a_dependent_task() {
    let(_temp,mut db,p)=fixture();
    db.testing_set_factory_admission(true).unwrap();
    db.connection.execute(
        "INSERT INTO task_dependencies(task_id,predecessor_id,requirement) VALUES('a','b','verified_result')",
        [],
    ).unwrap();
    let head=db.read_snapshot(None).unwrap().head;
    let err=db.reserve_prepared(&p,head,1000).unwrap_err();
    assert!(matches!(err, StoreError::Invalid(ref message) if message.contains("not ready") || message.contains("dependency")), "{err}");
    assert!(db.read_snapshot(None).unwrap().attempts.is_empty());
}

#[test]
fn project_outbox_upgrade_preserves_claims_inputs_and_consumed_approvals() {
    let(temp,mut db,p)=fixture_at(14);let r=reserve(&mut db,&p);
    let claim=db.claim_operation(&r.record.operation,1,"worker",1000,1000).unwrap();
    // A preserved v1 record coexists with the live v2 claim. The old JSON format
    // is literal historical data, not produced by the current serializer.
    let old_json=include_str!("../../../tests/fixtures/launch-inputs-v1.json").trim()
        .replace("\"task\":\"a\"","\"task\":\"b\"").replace("task:a","task:b");
    let inputs:LaunchInputs=serde_json::from_str(&old_json).unwrap();
    assert_eq!(serde_json::to_string(&inputs).unwrap(),old_json);
    let(attempt,operation)=record_ids(&inputs).unwrap();
    let old=AttemptInputRecord{attempt:attempt.clone(),operation:operation.clone(),inputs};
    let payload=serde_json::to_string(&old).unwrap();let digest=format!("{:x}",Sha256::digest(payload.as_bytes()));
    db.connection.execute_batch("DROP TRIGGER attempt_inputs_effective_profile;").unwrap();
    db.connection.execute("INSERT INTO attempts VALUES(?1,'b',1,'reserved',NULL,?2,0)",params![attempt.as_str(),format!("worker:{}",attempt.as_str())]).unwrap();
    db.connection.execute("UPDATE tasks SET revision=4,state='running',active_attempt=?1 WHERE id='b'",[attempt.as_str()]).unwrap();
    db.connection.execute("INSERT INTO operations VALUES(?1,'b','runtime.launch','task:b',1,?2,?3,4,1000,?1)",params![operation.as_str(),payload,digest]).unwrap();
    db.connection.execute("INSERT INTO attempt_inputs VALUES(?1,?2,?3,?4)",params![attempt.as_str(),operation.as_str(),payload,digest]).unwrap();
    db.connection.execute_batch("CREATE TRIGGER attempt_inputs_effective_profile BEFORE INSERT ON attempt_inputs WHEN COALESCE(json_extract(NEW.payload,'$.inputs.version'),0)<>2 OR COALESCE(json_type(NEW.payload,'$.inputs.effective_profile'),'missing')<>'object' BEGIN SELECT RAISE(ABORT,'effective profile evidence is required'); END;").unwrap();
    let mut before=db.read_snapshot(None).unwrap();
    let sql=include_str!("../../../migrations/0015_project_operations.sql");
    for checkpoint in ["DROP TABLE operation_delivery;","DROP TABLE operations;","UPDATE store_meta SET schema_version=15;"] {
        let failing=sql.replace(checkpoint,&format!("{checkpoint}\nSELECT nonexistent_migration_fixture();"));
        {let tx=db.connection.transaction().unwrap();assert!(tx.execute_batch(&failing).is_err());}
        assert_eq!(db.read_snapshot(None).unwrap(),before);db.integrity_check().unwrap();
        assert!(db.connection.execute("UPDATE operation_delivery SET attempts=0 WHERE attempts>0",[]).is_err());
        assert!(db.connection.execute("DELETE FROM approval_uses",[]).is_err());
        assert!(db.connection.execute("DELETE FROM attempt_inputs",[]).is_err());
    }
    db.upgrade_v1().unwrap();before.schema_version=crate::store::SCHEMA;
    assert_eq!(db.read_snapshot(None).unwrap(),before);db.integrity_check().unwrap();
    drop(db);let mut db=SqliteStore::open(&temp.path().join(".state/state.db")).unwrap();
    db.validate_claim(&claim,1001).unwrap();assert_eq!(db.read_snapshot(None).unwrap(),before);
    assert!(db.connection.execute("UPDATE operation_delivery SET attempts=0",[]).is_err());
    assert!(db.connection.execute("DELETE FROM approval_uses",[]).is_err());
    assert!(db.connection.execute("DELETE FROM attempt_inputs",[]).is_err());
}

fn budget(db:&mut SqliteStore,p:&mut [PreparedLaunch],limits:BudgetLimits) {
    let snapshot=db.read_snapshot(None).unwrap();
    let policy=BudgetPolicy{version:1,project_store:p[0].inputs.project_store.clone(),revision:snapshot.budget_policies.len() as u64+1,
        authority:p[0].inputs.effective_profile.as_ref().unwrap().permission_policy.clone(),limits};
    let reference=db.install_budget(&PreparedBudget{policy},snapshot.head).unwrap();
    for p in p {
        p.inputs.budget=Some(reference.clone());
        let grant=ApprovalGrant{version:1,scope:ApprovalScope::for_launch(&p.inputs).unwrap(),policy:p.inputs.effective_profile.as_ref().unwrap().permission_policy.clone(),issued_unix_ms:0,expires_unix_ms:100_000};
        let head=db.read_snapshot(None).unwrap().head;
        p.inputs.approval=db.install_approval(&PreparedApproval{grant},head,1000).unwrap();
    }
}
#[test]
fn budget_exhaustion_survives_reopen_and_cancel_does_not_refund_admissions() {
    let(temp,mut db,mut p)=fixture();
    budget(&mut db,&mut p,BudgetLimits{max_attempts:Some(1),max_provider_tokens:None,unknown_usage:UnknownUsagePolicy::Refuse});
    let r=reserve(&mut db,&p);
    let head=db.read_snapshot(None).unwrap().head;
    db.cancel_attempt(&r.record.attempt,1,head,"cancel without effect",1001).unwrap();
    drop(db);let mut db=SqliteStore::open(&temp.path().join(".state/state.db")).unwrap();
    let report=db.budget_report().unwrap();assert_eq!(report.admitted_attempts,1);
    assert_eq!(report.provider_tokens,UsageAvailability::Unknown);
    assert!(report.blockers.contains(&"attempt_budget_exhausted".into()));
    assert!(db.queue_report(1002).unwrap().entries.iter().all(|entry|entry.blockers.contains(&"attempt_budget_exhausted".into())));
    let before=db.read_snapshot(None).unwrap();
    let other=p.into_iter().filter(|p|p.inputs.task!=r.record.inputs.task).collect::<Vec<_>>();
    assert!(db.reserve_prepared(&other,before.head,1002).is_err());
    assert_eq!(db.read_snapshot(None).unwrap(),before);
}
#[test]
fn budget_revision_change_blocks_claim_and_pre_effect_without_releasing_capacity() {
    for claimed in [false,true] {
        let(_temp,mut db,p)=fixture();let r=reserve(&mut db,&p);
        let claim=claimed.then(||db.claim_operation(&r.record.operation,1,"worker",1000,1000).unwrap());
        let before=db.read_snapshot(None).unwrap();
        let policy=BudgetPolicy{version:1,project_store:p[0].inputs.project_store.clone(),revision:1,
            authority:p[0].inputs.effective_profile.as_ref().unwrap().permission_policy.clone(),
            limits:BudgetLimits{max_attempts:Some(0),max_provider_tokens:None,unknown_usage:UnknownUsagePolicy::Refuse}};
        db.install_budget(&PreparedBudget{policy},before.head).unwrap();
        let before=db.read_snapshot(None).unwrap();
        if let Some(claim)=claim {assert!(db.validate_claim(&claim,1001).is_err());}
        else {assert!(db.claim_operation(&r.record.operation,1,"worker",1001,1000).is_err());}
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        assert!(before.attempts[0].retains_capacity());
    }
}
#[test]
fn unknown_provider_usage_is_explicit_and_never_an_implicit_zero() {
    for (unknown_usage,tokens,admitted) in [(UnknownUsagePolicy::Refuse,10,false),(UnknownUsagePolicy::AllowIncomplete,10,true),(UnknownUsagePolicy::AllowIncomplete,0,false)] {
        let(_temp,mut db,mut p)=fixture();
        budget(&mut db,&mut p,BudgetLimits{max_attempts:Some(1),max_provider_tokens:Some(tokens),unknown_usage});
        let report=db.budget_report().unwrap();assert_eq!(report.provider_tokens,UsageAvailability::Unknown);
        let before=db.read_snapshot(None).unwrap();
        if !admitted {
            assert!(!report.blockers.is_empty());assert!(db.reserve_prepared(&p,before.head,1000).is_err());
            assert_eq!(db.read_snapshot(None).unwrap(),before);
        } else {
            assert!(report.incomplete);assert!(report.blockers.is_empty());
            let r=reserve(&mut db,&p);
            // Reaching the count limit after reserving must not reject its own launch.
            let claim=db.claim_operation(&r.record.operation,1,"worker",1000,1000).unwrap();
            db.validate_claim(&claim,1001).unwrap();
            assert!(db.budget_report().unwrap().blockers.contains(&"attempt_budget_exhausted".into()));
        }
    }
}
#[test]
fn budget_upgrade_preserves_pending_operations_without_inventing_policy() {
    let(_temp,mut db,p)=fixture();reserve(&mut db,&p);
    crate::store::test_schema::historical(&db.connection, 13).unwrap();
    let before=db.read_snapshot(None).unwrap();db.upgrade_v1().unwrap();
    let after=db.read_snapshot(None).unwrap();assert_eq!(after.schema_version,crate::store::SCHEMA);
    assert_eq!(after.events,before.events);assert_eq!(after.attempt_inputs,before.attempt_inputs);
    assert_eq!(after.deliveries,before.deliveries);assert!(after.budget_policies.is_empty());
}
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
        assert!(db.commit(Commit{expected_head:snapshot.head,mutations:vec![Mutation::Attempt{expected:Some(1),next:attempt.clone()}]}).is_err());
        assert_eq!(db.read_snapshot(None).unwrap(),snapshot);
        // Simulate a future trusted lifecycle transition to exercise the
        // independent claim fence; generic callers cannot make this transition.
        db.connection.execute("UPDATE attempts SET revision=?2,state=?3,termination_observed=?4 WHERE id=?1",params![attempt.id.as_str(),attempt.revision,attempt.state.as_str(),attempt.termination_observed]).unwrap();
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
        crate::store::test_schema::historical(&db.connection, 12).unwrap();
        let before=db.read_snapshot(None).unwrap();db.upgrade_v1().unwrap();let after=db.read_snapshot(None).unwrap();
        assert_eq!(after.head,before.head);assert_eq!(after.events,before.events);assert_eq!(after.attempt_inputs,before.attempt_inputs);assert_eq!(after.deliveries,before.deliveries);assert!(after.approvals.is_empty());
        drop(db);let mut db=SqliteStore::open(&temp.path().join(".state/state.db")).unwrap();
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
    let mut db=SqliteStore::open(&temp.path().join(".state/state.db")).unwrap();db.validate_claim(&claim,1002).unwrap();
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
    crate::store::test_schema::historical(&db.connection, 11).unwrap();
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
    assert_eq!(after.schema_version,crate::store::SCHEMA);assert_eq!(after.attempt_inputs,before.attempt_inputs);assert_eq!(after.events,before.events);
    assert_eq!(after.head,before.head);
    assert!(db.connection.execute("INSERT INTO attempt_inputs VALUES('old','old','{\"inputs\":{\"version\":1}}',?1)",params!["a".repeat(64)]).is_err());
    assert_eq!(after.attempt_inputs[0],record);
    assert_eq!(serde_json::to_string(&after.attempt_inputs[0]).unwrap(),payload);
    drop(db);let mut db=SqliteStore::open(&temp.path().join(".state/state.db")).unwrap();
    assert_eq!(db.read_snapshot(None).unwrap().attempt_inputs,after.attempt_inputs);
    assert!(db.cancel_attempt(&attempt,1,after.head,"cancel historical reservation",1001).unwrap().released);
}
#[test]
fn capacity_wait_distinguishes_never_claimed_cancellation_from_uncertain_stop() {
    use crate::domain::WaitTrigger;
    for claimed in [false,true] {
        let (_temp,mut db,p)=fixture();let r=reserve(&mut db,&p);
        if claimed {db.claim_operation(&r.record.operation,1,"worker",1000,1000).unwrap();}
        let trigger=WaitTrigger::AttemptCapacityReleased{attempt_id:r.record.attempt.clone(),after_revision:1};
        let wait=db.register_wait_with_trigger("a",None,"resource_availability",None,Some(&trigger)).unwrap();
        assert!(!db.replay_wait(&wait.wait_id).unwrap().wake_requested);
        let cancelled=db.cancel_attempt(&r.record.attempt,1,db.current_head().unwrap(),"capacity trigger",1001).unwrap();
        assert_eq!(cancelled.released,!claimed);
        assert_eq!(db.replay_wait(&wait.wait_id).unwrap().wake_requested,!claimed);
        assert_eq!(db.queue_report(1001).unwrap().available_slots,usize::from(!claimed));
        if !claimed {
            let next=db.rearm_wait(&wait.wait_id,None).unwrap();
            assert!(db.replay_wait(&next.wait_id).unwrap().wake_requested);
        }
    }
}

#[test]
fn reservation_and_never_claimed_cancellation_survive_restart() {
    let(temp,mut db,p)=fixture();let r=reserve(&mut db,&p);assert_eq!(r.record.inputs.task.as_str(),"a");let s=db.read_snapshot(None).unwrap();assert_eq!(s.attempt_inputs,vec![r.record.clone()]);assert_eq!(s.attempts.len(),1);assert!(s.attempts[0].retains_capacity());assert_eq!(db.queue_report(1000).unwrap().available_slots,0);
    assert!(db.connection.execute("UPDATE attempt_inputs SET payload='{}'",[]).is_err());assert!(db.connection.execute("DELETE FROM attempt_inputs",[]).is_err());
    drop(db);let mut db=SqliteStore::open(&temp.path().join(".state/state.db")).unwrap();let c=db.cancel_attempt(&r.record.attempt,1,r.head,"operator request",1001).unwrap();assert!(c.released);assert_eq!(db.queue_report(1001).unwrap().available_slots,1);assert_eq!(db.deliveries().unwrap()[0].state,DeliveryState::PermanentFailure);assert!(db.claim_operation(&r.record.operation,1,"worker",1002,1000).is_err());let s=db.read_snapshot(None).unwrap();assert_eq!(s.tasks[0].state,TaskState::Cancelled);assert_eq!(s.tasks[0].active_attempt,None);assert!(db.cancel_attempt(&r.record.attempt,c.attempt_revision,c.head,"operator request",1002).unwrap().released);assert_eq!(db.read_snapshot(None).unwrap(),s);
}
#[test]
fn cancellation_reads_only_its_launch_and_keeps_other_retained_attempts() {
    for other_retained in [false,true] {
        let(_temp,mut db,p)=fixture();let reserved=reserve(&mut db,&p);
        // An unrelated retired binding must not force cancellation to decode
        // historical payloads. Full administrative reads still diagnose it.
        db.connection.execute("UPDATE runtime_bindings SET payload='{}' WHERE task_id='b'",[]).unwrap();
        db.connection.execute("INSERT INTO attempts VALUES('other','a',1,'running',NULL,'other-slot',?1)",[!other_retained]).unwrap();
        let result=db.cancel_attempt(&reserved.record.attempt,1,reserved.head,"operator request",1001).unwrap();
        assert_eq!(result.released,!other_retained);
        assert_eq!(read_attempt(&db.connection,&reserved.record.attempt).unwrap().retains_capacity(),other_retained);
        assert!(db.read_snapshot(None).is_err());
    }
}

#[test]
fn cancellation_rejects_corrupt_selected_binding_without_releasing_capacity() {
    let(_temp,mut db,p)=fixture();let reserved=reserve(&mut db,&p);
    db.connection.execute("UPDATE runtime_bindings SET payload='{}' WHERE id=?1",[&reserved.record.inputs.binding]).unwrap();
    assert!(db.cancel_attempt(&reserved.record.attempt,1,reserved.head,"operator request",1001).is_err());
    let attempt=read_attempt(&db.connection,&reserved.record.attempt).unwrap();
    assert!(attempt.retains_capacity());assert_eq!(attempt.revision,1);
    assert!(read_cancellations(&db.connection).unwrap().is_empty());
    assert_eq!(head(&db.connection).unwrap(),reserved.head);
}

#[test]
fn stale_or_cross_store_preparations_and_rollback_never_leak_reservations() {
    let(_temp,mut db,p)=fixture();let before=db.read_snapshot(None).unwrap();
    for field in 0..7 {let mut bad=p[0].clone();match field {0=>bad.inputs.task_revision+=1,1=>bad.inputs.scheduler_revision+=1,2=>bad.inputs.control_epoch+=1,3=>bad.inputs.binding_revision+=1,4=>bad.inputs.binding_digest="f".repeat(64),5=>bad.inputs.project_store="/tmp/another-store".into(),_=>bad.inputs.config.digest=Some("c".repeat(64))};assert!(db.reserve_prepared(&[bad],before.head,1000).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);}
    db.connection.execute_batch("CREATE TRIGGER fail_input BEFORE INSERT ON attempt_inputs BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();assert!(db.reserve_prepared(&p,before.head,1000).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
}
#[test]
fn competing_reservations_cannot_exceed_capacity_even_with_a_fresh_head() {
    use std::sync::{Arc,Barrier};let(temp,mut db,p)=fixture();let h=db.read_snapshot(None).unwrap().head;let gate=Arc::new(Barrier::new(2));let workers:Vec<_>=p.into_iter().map(|p|{let gate=gate.clone();let path=temp.path().join(".state/state.db");std::thread::spawn(move||{let mut db=SqliteStore::open(&path).unwrap();gate.wait();let result=db.reserve_prepared(&[p.clone()],h,1000);(result,p)})}).collect();let results:Vec<_>=workers.into_iter().map(|w|w.join().unwrap()).collect();assert_eq!(results.iter().filter(|(r,_)|r.is_ok()).count(),1);let loser=&results.iter().find(|(r,_)|r.is_err()).unwrap().1;let h=db.read_snapshot(None).unwrap().head;assert!(matches!(db.reserve_prepared(&[loser.clone()],h,1000),Err(StoreError::Invalid(s)) if s.contains("capacity")));assert_eq!(db.read_snapshot(None).unwrap().attempts.len(),1);
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
    use std::sync::{Arc,Barrier};let(temp,mut db,p)=fixture();let r=reserve(&mut db,&p);let gate=Arc::new(Barrier::new(2));let path=temp.path().join(".state/state.db");let op=r.record.operation.clone();let g=gate.clone();let claimant=std::thread::spawn(move||{let mut db=SqliteStore::open(&path).unwrap();g.wait();db.claim_operation(&op,1,"worker",1001,1000)});gate.wait();let cancelled=db.cancel_attempt(&r.record.attempt,1,r.head,"cancel",1001);let claimed=claimant.join().unwrap();assert_ne!(cancelled.is_ok(),claimed.is_ok());if claimed.is_ok(){let h=db.read_snapshot(None).unwrap().head;assert!(!db.cancel_attempt(&r.record.attempt,1,h,"cancel",1002).unwrap().released);}let s=db.read_snapshot(None).unwrap();assert_eq!(s.attempts[0].retains_capacity(),claimed.is_ok());
}
#[test]
fn orphan_launches_refuse_reads_and_upgrade_rolls_back() {
    let(_temp,mut db,p)=fixture();let r=reserve(&mut db,&p);db.connection.execute_batch("DROP TRIGGER attempt_inputs_no_delete; DELETE FROM attempt_inputs;").unwrap();assert!(db.read_snapshot(None).is_err());crate::store::test_schema::historical(&db.connection, 10).unwrap();assert!(db.upgrade_v1().is_err());let version:u32=db.connection.query_row("PRAGMA user_version",[],|r|r.get(0)).unwrap();assert_eq!(version,10);assert_eq!(db.read_snapshot(None).unwrap().operations[0].id,r.record.operation);
}
#[test]
fn cancellation_without_launch_proof_retains_the_attempt() {
    let(_temp,mut db,_p)=fixture();let s=db.read_snapshot(None).unwrap();let id=AttemptId::new("adopted").unwrap();db.commit(Commit{expected_head:s.head,mutations:vec![Mutation::Attempt{expected:None,next:Attempt{id:id.clone(),task:TaskId::new("a").unwrap(),revision:1,state:AttemptState::Running,snapshot:None,reservation:"adopted-slot".into(),termination_observed:false}}]}).unwrap();let h=db.read_snapshot(None).unwrap().head;assert!(!db.cancel_attempt(&id,1,h,"request stop",1000).unwrap().released);assert_eq!(db.queue_report(1000).unwrap().available_slots,0);
}
#[test]
fn schema10_upgrade_preserves_nonzero_claim_history() {
    let(_temp,mut db,p)=fixture();let r=reserve(&mut db,&p);db.claim_operation(&r.record.operation,1,"worker",1000,1000).unwrap();crate::store::test_schema::historical(&db.connection, 10).and_then(|_| db.connection.execute_batch("UPDATE operations SET kind='fixture';")).unwrap();let before=db.deliveries().unwrap();db.upgrade_v1().unwrap();assert_eq!(db.deliveries().unwrap(),before);assert!(db.read_snapshot(None).unwrap().attempt_inputs.is_empty());assert!(db.connection.execute("UPDATE operation_delivery SET attempts=0",[]).is_err());
}
#[test]
fn missing_parent_is_not_hidden_from_an_open_store() {
    let(_temp,mut db,p)=fixture();let _r=reserve(&mut db,&p);db.connection.execute_batch("PRAGMA foreign_keys=OFF; DELETE FROM operation_delivery; DELETE FROM operations;").unwrap();assert!(read_inputs(&db.connection).is_err());
}
#[test]
fn reservation_crash_child() {
    let Some(root)=std::env::var_os("HP_RESERVATION_CRASH_ROOT") else{return;};let root=std::path::PathBuf::from(root);let phase=std::env::var("HP_RESERVATION_CRASH_PHASE").unwrap();let mut db=SqliteStore::open(&root.join(".state/state.db")).unwrap();let inputs:LaunchInputs=serde_json::from_slice(&std::fs::read(root.join("inputs.json")).unwrap()).unwrap();
    if phase=="committed" {reserve(&mut db,&[PreparedLaunch{inputs}]);}
    else {db.connection.execute_batch("BEGIN IMMEDIATE; INSERT INTO attempts VALUES('interrupted','a',1,'reserved',NULL,'interrupted-slot',0); UPDATE tasks SET revision=revision+1,state='running',active_attempt='interrupted' WHERE id='a';").unwrap();}
    std::fs::write(root.join("ready"),b"ready").unwrap();loop {std::thread::sleep(Duration::from_secs(1));}
}
#[test]
fn process_death_preserves_whole_reservation_or_original_queue() {
    use std::{process::{Command,Stdio},time::Instant};
    for phase in ["uncommitted","committed"] {let(temp,mut db,p)=fixture();let before=db.read_snapshot(None).unwrap();drop(db);std::fs::write(temp.path().join("inputs.json"),serde_json::to_vec(&p[0].inputs).unwrap()).unwrap();let mut child=Command::new(std::env::current_exe().unwrap()).args(["--exact","store::reservations::tests::reservation_crash_child","--nocapture"]).env("HP_RESERVATION_CRASH_ROOT",temp.path()).env("HP_RESERVATION_CRASH_PHASE",phase).stdout(Stdio::null()).stderr(Stdio::inherit()).spawn().unwrap();let deadline=Instant::now()+Duration::from_secs(10);while !temp.path().join("ready").exists() {if Instant::now()>deadline||child.try_wait().unwrap().is_some(){let _=child.kill();let _=child.wait();panic!("child failed to reach {phase}");}std::thread::sleep(Duration::from_millis(10));}child.kill().unwrap();child.wait().unwrap();let mut db=SqliteStore::open(&temp.path().join(".state/state.db")).unwrap();let after=db.read_snapshot(None).unwrap();if phase=="uncommitted" {assert_eq!(after,before);}else{assert_eq!(after.attempt_inputs.len(),1);assert_eq!(after.operations.len(),1);assert_eq!(after.tasks[0].active_attempt,Some(after.attempts[0].id.clone()));assert_eq!(db.queue_report(1000).unwrap().available_slots,0);assert_eq!(db.deliveries().unwrap()[0].state,DeliveryState::Pending);}}
}

#[test]
fn launch_knowledge_fence_does_not_scan_retained_memory_events() {
    use std::sync::{Arc,atomic::{AtomicU64,Ordering}};
    let mut work=Vec::new();
    for history in [0,10_000] {
        let (_temp,mut db,mut prepared)=fixture();
        let tx=db.connection.transaction().unwrap();
        for n in 0..history {
            // Model retained audit history before this snapshot's selection.
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.revision_inserted',?1,1,1,'{}')",[format!("historical-{n}")]).unwrap();
        }
        tx.commit().unwrap();
        let p=&mut prepared[0];
        let profile=p.inputs.effective_profile.as_ref().unwrap().clone();
        let snapshot=db.create_memory_snapshot(SnapshotPlan {
            coordinator:false,session_id:None,request:SnapshotRequest{schema_version:1,task_id:p.inputs.task.as_str().into(),profile:profile.name.clone(),domains:vec![],paths:vec![],pinned_keys:vec![],sensitivity:"default".into()},
            profile_name:profile.name.clone(),profile_digest:profile.definition_digest.clone(),config_digest:p.inputs.config.digest.clone(),budget_chars:32000,estimator:SELECTION_ESTIMATOR.into(),instructions:"Retained instructions".into(),now_unix_ms:1000,expected_heads_digest:None,
        }).unwrap();
        p.inputs.memory=Some(VersionedReference{id:snapshot.id.as_str().into(),revision:1,digest:snapshot.manifest_hash});
        let steps=Arc::new(AtomicU64::new(0));
        let observed=steps.clone();
        db.connection.progress_handler(1,Some(move||{observed.fetch_add(1,Ordering::Relaxed);false}));
        let budget=super::super::read_budget::ReadBudget::new(super::super::controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(10),Default::default()));
        super::super::worker_knowledge::validate_with_budget(&db.connection,&p.inputs,1000,Some(&budget)).unwrap();
        db.connection.progress_handler(0,None::<fn()->bool>);
        work.push(steps.load(Ordering::Relaxed));
        // The optimized lookup must still fence every relevant event kind.
        for kind in ["memory.revision_inserted","memory.head_changed","memory.revoked","memory.validity_changed","memory.record_policy_changed","memory.policy_changed","memory.policy_applied"] {
            db.connection.execute_batch("SAVEPOINT changed_memory").unwrap();
            db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,'later',1,1,'{}')",[kind]).unwrap();
            assert!(matches!(super::super::worker_knowledge::validate_with_budget(&db.connection,&p.inputs,1000,Some(&budget)),Err(StoreError::Invalid(message)) if message.contains("memory changed after knowledge selection")),"{kind}");
            db.connection.execute_batch("ROLLBACK TO changed_memory; RELEASE changed_memory").unwrap();
        }
        super::super::worker_knowledge::validate_with_budget(&db.connection,&p.inputs,1000,Some(&budget)).unwrap();
    }
    eprintln!("launch knowledge validation SQL steps with 0/10000 retained memory events: {work:?}");
    assert!(work[1]<=work[0]+500,"knowledge validation scanned retained memory history: {work:?}");
}

#[test]
fn sealed_reservation_binds_knowledge_and_pre_effect_checks_reject_later_changes() {
    let (_temp,mut db,mut prepared)=fixture();
    let p=&mut prepared[0];let profile=p.inputs.effective_profile.as_ref().unwrap().clone();
    let snapshot=db.create_memory_snapshot(SnapshotPlan {
        coordinator:false,session_id:None,request:SnapshotRequest {schema_version:1,task_id:p.inputs.task.as_str().into(),profile:profile.name.clone(),domains:vec![],paths:vec![],pinned_keys:vec![],sensitivity:"default".into()},
        profile_name:profile.name.clone(),profile_digest:profile.definition_digest.clone(),config_digest:p.inputs.config.digest.clone(),budget_chars:32000,estimator:SELECTION_ESTIMATOR.into(),instructions:"Retained worker instructions".into(),now_unix_ms:1000,expected_heads_digest:None,
    }).unwrap();
    p.inputs.memory=Some(VersionedReference {id:snapshot.id.as_str().into(),revision:1,digest:snapshot.manifest_hash.clone()});
    let grant=ApprovalGrant {version:1,scope:ApprovalScope::for_launch(&p.inputs).unwrap(),policy:profile.permission_policy,issued_unix_ms:0,expires_unix_ms:100000};
    let head=db.read_snapshot(None).unwrap().head;p.inputs.approval=db.install_approval(&PreparedApproval {grant},head,1000).unwrap();
    let head=db.read_snapshot(None).unwrap().head;
    let mut wrong=p.clone();wrong.inputs.memory.as_mut().unwrap().digest="0".repeat(64);
    assert!(db.reserve_prepared(&[wrong],head,1000).is_err());
    let mut wrong=p.clone();wrong.inputs.effective_profile.as_mut().unwrap().definition_digest="c".repeat(64);
    wrong.inputs.profile=wrong.inputs.effective_profile.as_ref().unwrap().reference().unwrap();
    assert!(db.reserve_prepared(&[wrong],head,1000).is_err());
    let reservation=db.reserve_prepared(&[p.clone()],head,1000).unwrap();
    let state=db.read_snapshot(None).unwrap();assert_eq!(state.attempts[0].snapshot.as_deref(),Some(snapshot.id.as_str()));
    assert_eq!(db.attempt_knowledge_snapshot(reservation.record.attempt.as_str(),1000).unwrap(),snapshot);
    let claim=db.claim_operation(&reservation.record.operation,1,"worker",1000,1000).unwrap();
    db.validate_claim(&claim,1001).unwrap();
    // Even an empty selected manifest becomes stale when new memory is published.
    db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.revision_inserted','new-rule',1,1,'{}')",[]).unwrap();
    assert!(db.validate_claim(&claim,1002).is_err());
    assert!(db.attempt_knowledge_snapshot(reservation.record.attempt.as_str(),1002).is_err());
}

#[test]
fn reserve_and_termination_refresh_bindings_without_generic_commit() {
    use crate::domain::{
        AttemptOutputReference, LaunchTarget, LaunchStoppedReceipt, PreparedLaunchStopped,
        ResourceIdentity, RuntimeRoute, SnapshotPlan, SnapshotRequest, VersionedReference,
    };
    use crate::domain::SELECTION_ESTIMATOR;
    use crate::worker_supervision::{ProcessIncarnation, SupervisorIdentity};
    let (_temp, mut db, mut prepared) = fixture();
    let launch = &mut prepared[0];
    let profile = launch.inputs.effective_profile.as_ref().unwrap().clone();
    let task_id = launch.inputs.task.as_str().to_string();
    let config_digest = launch.inputs.config.digest.clone();
    let snap = |db: &mut SqliteStore, instructions: &str| {
        db.create_memory_snapshot(SnapshotPlan {
            coordinator: false,
            session_id: None,
            request: SnapshotRequest {
                schema_version: 1,
                task_id: task_id.clone(),
                profile: profile.name.clone(),
                domains: vec![],
                paths: vec![],
                pinned_keys: vec![],
                sensitivity: "default".into(),
            },
            profile_name: profile.name.clone(),
            profile_digest: profile.definition_digest.clone(),
            config_digest: config_digest.clone(),
            budget_chars: 32_000,
            estimator: SELECTION_ESTIMATOR.into(),
            instructions: instructions.into(),
            now_unix_ms: 1000,
            expected_heads_digest: None,
        })
        .unwrap()
    };
    let reserved_snapshot = snap(&mut db, "reserved instructions");
    let earlier = snap(&mut db, "earlier sibling instructions");
    launch.inputs.memory = Some(VersionedReference {
        id: reserved_snapshot.id.as_str().into(),
        revision: 1,
        digest: reserved_snapshot.manifest_hash.clone(),
    });
    let grant = ApprovalGrant {
        version: 1,
        scope: ApprovalScope::for_launch(&launch.inputs).unwrap(),
        policy: profile.permission_policy.clone(),
        issued_unix_ms: 0,
        expires_unix_ms: 100_000,
    };
    let head = db.read_snapshot(None).unwrap().head;
    launch.inputs.approval = db
        .install_approval(&PreparedApproval { grant }, head, 1000)
        .unwrap();
    let head = db.read_snapshot(None).unwrap().head;
    let reservation = db
        .reserve_prepared(&[launch.clone()], head, 1000)
        .unwrap();
    let reserved_binding = db
        .consumer_binding_for_snapshot(reserved_snapshot.id.as_str())
        .unwrap()
        .unwrap();
    let earlier_binding = db
        .consumer_binding_for_snapshot(earlier.id.as_str())
        .unwrap()
        .unwrap();
    assert!(reserved_binding.active);
    assert_eq!(
        reserved_binding.attempt_id.as_deref(),
        Some(reservation.record.attempt.as_str())
    );
    assert!(!earlier_binding.active);
    let successor = snap(&mut db, "successor instructions");
    let successor_binding = db
        .consumer_binding_for_snapshot(successor.id.as_str())
        .unwrap()
        .unwrap();
    assert!(!successor_binding.active);
    let _claim = db
        .claim_operation(&reservation.record.operation, 1, "worker", 1000, 1000)
        .unwrap();
    let supervisor = SupervisorIdentity {
        version: 2,
        boot_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
        host_id: Some("a".repeat(64)),
        observer_namespace: (1, 2),
        worker_namespace: (1, 3),
        outer: ProcessIncarnation {
            pid: 2,
            device: 1,
            inode: 1,
        },
        init: ProcessIncarnation {
            pid: 3,
            device: 1,
            inode: 2,
        },
    };
    let target = LaunchTarget {
        version: 2,
        attempt: reservation.record.attempt.clone(),
        operation: reservation.record.operation.clone(),
        route: RuntimeRoute {
            machine: String::new(),
            socket: "/tmp/herdr.sock".into(),
            workspace_id: "ws".into(),
            tab_id: "tab".into(),
            pane_id: "pane".into(),
            cwd: "/tmp/task".into(),
        },
        terminal: "term".into(),
        session: ResourceIdentity {
            device: 1,
            inode: 2,
            born_secs: 3,
            born_nanos: 0,
        },
        supervisor: Some(supervisor),
        observed_unix_ms: 1500,
    };
    db.connection
        .execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.launch_target',?1,1,1,?2)",
            params![
                reservation.record.operation.as_str(),
                serde_json::to_string(&target).unwrap()
            ],
        )
        .unwrap();
    let head = db.read_snapshot(None).unwrap().head;
    db.record_launch_stopped(
        &PreparedLaunchStopped {
            receipt: LaunchStoppedReceipt {
                version: 1,
                target: target.clone(),
                host_reboot: None,
                repository_snapshots: vec![],
                output_snapshot: Some(AttemptOutputReference {
                    source: crate::domain::worker_output_path(
                        &reservation.record.inputs,
                        &reservation.record.attempt,
                    )
                    .unwrap(),
                    digest: None,
                }),
                observed_unix_ms: 1500,
            },
        },
        1,
        head,
        1500,
    )
    .unwrap();
    let successor_binding = db
        .consumer_binding_for_snapshot(successor.id.as_str())
        .unwrap()
        .unwrap();
    assert!(successor_binding.active);
    let sequence: i64 = db
        .connection
        .query_row("SELECT max(sequence) FROM events", [], |row| row.get(0))
        .unwrap();
    let hash = "ab".repeat(32);
    db.connection
        .execute(
            "INSERT INTO objects(hash,size,availability,collection,pin_count,fencing_token) VALUES(?1,1,'available','unclaimed',0,0)",
            [&hash],
        )
        .unwrap();
    db.connection
        .execute(
            "INSERT INTO memory_records(id,record_key,scope_id,kind,is_hard) VALUES('fact','fact','project','observation',0)",
            [],
        )
        .unwrap();
    db.connection
        .execute(
            "INSERT INTO memory_revisions(record_id,revision,body_hash,provenance_hash,promoted_seq,applicability) VALUES('fact',1,?1,?1,?2,'{\"domains\":[],\"paths\":[]}')",
            params![hash, sequence],
        )
        .unwrap();
    db.connection
        .execute(
            "INSERT INTO memory_delivery_intents(id,cause_id,subscriber,snapshot_id,task_id,record_id,revision,severity,triggering_seq,state) VALUES('delivery-keep','cause',?1,?2,?3,'fact',1,'informational',?4,'pending')",
            params![
                format!("task:{task_id}"),
                reserved_snapshot.id.as_str(),
                task_id,
                sequence
            ],
        )
        .unwrap();
    db.retire_consumer_binding(
        &reserved_binding.binding_id,
        Some(&successor_binding.binding_id),
    )
    .unwrap();
    assert!(db
        .binding_obligations(&successor_binding.binding_id)
        .unwrap()
        .contains(&"delivery-keep".to_string()));
    assert!(db
        .binding_undeliverable(&reserved_binding.binding_id)
        .unwrap()
        .is_empty());
}

#[test]
fn untyped_launch_confirmation_and_capacity_release_are_atomic_refusals() {
    let (_temp,mut db,p)=fixture();
    let reservation=reserve(&mut db,&p);
    let claim=db.claim_operation(&reservation.record.operation,1,"worker",1000,1000).unwrap();
    let before=db.read_snapshot(None).unwrap();
    assert!(db.finish_operation(&claim,Outcome::Confirmed{observed_identity:"caller says started".into()},1001).is_err());
    assert_eq!(db.read_snapshot(None).unwrap(),before);
    for field in 0..4 {
        let mut attempt=before.attempts[0].clone();attempt.revision+=1;
        match field {
            0=>attempt.termination_observed=true,
            1=>attempt.snapshot=Some("different-knowledge".into()),
            2=>attempt.state=AttemptState::Running,
            _=>attempt.reservation="different-capacity-slot".into(),
        }
        let mut task=before.tasks.iter().find(|t|t.id==attempt.task).unwrap().clone();
        task.revision+=1;task.title="must roll back too".into();
        assert!(db.commit(Commit{expected_head:before.head,mutations:vec![Mutation::Task{expected:Some(task.revision-1),next:task},Mutation::Attempt{expected:Some(1),next:attempt}]}).is_err());
        assert_eq!(db.read_snapshot(None).unwrap(),before);
    }
    db.expire_claims(2000).unwrap();
    let before=db.read_snapshot(None).unwrap();
    let delivery=before.deliveries.iter().find(|d|d.operation==claim.operation).unwrap();
    assert!(db.observe_operation(&claim.operation,delivery.revision,"operator",Outcome::Confirmed{observed_identity:"caller says recovered".into()},2001).is_err());
    assert_eq!(db.read_snapshot(None).unwrap(),before);
    assert_eq!(db.queue_report(2001).unwrap().available_slots,0);
}

// Invoked only in a separate test executable. A marker handshakes with the
// parent, which SIGKILLs the process; no destructor/SQLite close can run.
pub(in crate::store) fn crash_boundary(point:&str) {
    if std::env::var("HERDR_LAUNCH_CRASH_POINT").ok().as_deref()!=Some(point) {return;}
    let marker=std::env::var_os("HERDR_LAUNCH_CRASH_MARKER").expect("child marker");
    let mut file=std::fs::File::create(marker).unwrap();
    std::io::Write::write_all(&mut file,point.as_bytes()).unwrap();file.sync_all().unwrap();
    loop {std::thread::park();}
}

#[test]
fn atomic_creation_crash_child() {
    let Some(path)=std::env::var_os("HERDR_ATOMIC_CREATION_DB") else {return;};
    let mut db=SqliteStore::open(Path::new(&path)).unwrap();
    let state=db.read_snapshot(None).unwrap();let record=&state.attempt_inputs[0];
    let binding=state.runtime_bindings.iter().find(|b|b.id==record.inputs.binding).unwrap();
    let prepared=PreparedLaunchCreation{intent:LaunchCreationIntent {
        version:2,operation:record.operation.clone(),attempt:record.attempt.clone(),
        route:RuntimeRoute::from_identity(&binding.identity),
        session:ResourceIdentity{device:1,inode:2,born_secs:3,born_nanos:0},
        command_digest:"a".repeat(64),workspace_token:None,usage_warning:Some(LaunchUsageWarning::ProviderUsageUnavailable),
    }};
    db.claim_launch_creation(1,&prepared,1000,1000).unwrap();
    panic!("unknown atomic creation crash point");
}

#[test]
fn sigkill_atomic_creation_never_leaves_a_claim_without_recovery_identity() {
    use std::{process::{Command,Stdio},time::{Duration,Instant}};
    for point in ["creation_before_intent","creation_before_commit","creation_after_commit"] {
        let(temp,mut db,p)=fixture();let reserved=reserve(&mut db,&p);let before=db.read_snapshot(None).unwrap();
        let path=temp.path().join(".state/state.db");drop(db);
        let marker=temp.path().join("atomic-ready");
        let mut child=Command::new(std::env::current_exe().unwrap())
            .args(["--exact","store::reservations::tests::atomic_creation_crash_child","--nocapture"])
            .env("HERDR_ATOMIC_CREATION_DB",&path).env("HERDR_LAUNCH_CRASH_POINT",point)
            .env("HERDR_LAUNCH_CRASH_MARKER",&marker).stdout(Stdio::null()).stderr(Stdio::inherit()).spawn().unwrap();
        let deadline=Instant::now()+Duration::from_secs(10);
        while !marker.exists() && Instant::now()<deadline {
            if let Some(status)=child.try_wait().unwrap() {panic!("child exited before {point}: {status}");}
            std::thread::sleep(Duration::from_millis(10));
        }
        let ready=marker.exists();let _=child.kill();child.wait().unwrap();assert!(ready,"child did not reach {point}");
        let mut db=SqliteStore::open(&path).unwrap();db.integrity_check().unwrap();let after=db.read_snapshot(None).unwrap();
        if point!="creation_after_commit" {
            assert_eq!(after,before);
            assert!(db.cancel_attempt(&reserved.record.attempt,1,after.head,"no creation claim committed",1001).unwrap().released);
        } else {
            assert_eq!(after.deliveries[0].state,DeliveryState::Claimed);
            assert_eq!(after.approvals.iter().filter(|a|a.consumed.is_some()).count(),1);
            for kind in ["operation.claimed","runtime.launch_creation"] {
                assert_eq!(after.events.iter().filter(|e|e.kind==kind && e.entity==reserved.record.operation.as_str()).count(),1);
            }
            assert!(db.claim_operation(&reserved.record.operation,after.deliveries[0].revision,"replay",1001,1000).is_err());
            assert!(!db.cancel_attempt(&reserved.record.attempt,1,after.head,"uncertain creation",1001).unwrap().released);
        }
    }
}

#[test]
fn launch_crash_child() {
    let Some(path)=std::env::var_os("HERDR_LAUNCH_CRASH_DB") else {return;};
    let path=std::path::PathBuf::from(path);
    let mut db=SqliteStore::open(&path).unwrap();
    let inputs:Vec<LaunchInputs>=serde_json::from_slice(&std::fs::read(path.with_extension("inputs.json")).unwrap()).unwrap();
    let prepared:Vec<_>=inputs.into_iter().map(|inputs|PreparedLaunch{inputs}).collect();
    let reservation=reserve(&mut db,&prepared);
    crash_boundary("after_reservation_commit");
    let claim=db.claim_operation(&reservation.record.operation,1,"crash-worker",1000,1000).unwrap();
    crash_boundary("after_claim_commit");
    db.validate_claim(&claim,1001).unwrap();
    // This deliberately models an irreversible external start, not a Herdr
    // contract test. Lost acknowledgement must still never permit replay.
    let mut file=std::fs::OpenOptions::new().create_new(true).write(true).open(path.with_extension("effect")).unwrap();
    std::io::Write::write_all(&mut file,reservation.record.attempt.as_str().as_bytes()).unwrap();file.sync_all().unwrap();
    crash_boundary("after_external_effect");
    panic!("unknown child crash point");
}

#[test]
fn sigkill_launch_boundaries_preserve_capacity_authority_and_no_replay() {
    use std::{process::{Command,Stdio},time::{Duration,Instant}};
    for point in ["before_reservation_commit","after_reservation_commit","after_claim_commit","after_external_effect"] {
        let(temp,mut db,prepared)=fixture();
        let before=db.read_snapshot(None).unwrap();
        let path=temp.path().join(".state/state.db");
        std::fs::write(path.with_extension("inputs.json"),serde_json::to_vec(&prepared.iter().map(|p|&p.inputs).collect::<Vec<_>>()).unwrap()).unwrap();
        drop(db);
        let marker=temp.path().join("crash-ready");
        let mut child=Command::new(std::env::current_exe().unwrap())
            .args(["--exact","store::reservations::tests::launch_crash_child","--nocapture"])
            .env("HERDR_LAUNCH_CRASH_DB",&path).env("HERDR_LAUNCH_CRASH_POINT",point)
            .env("HERDR_LAUNCH_CRASH_MARKER",&marker).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let deadline=Instant::now()+Duration::from_secs(20);
        while !marker.exists() && Instant::now()<deadline {
            if let Some(status)=child.try_wait().unwrap() {panic!("crash child exited before {point}: {status}");}
            std::thread::sleep(Duration::from_millis(10));
        }
        let ready=marker.exists();let _=child.kill();child.wait().unwrap();assert!(ready,"child did not reach {point}");
        let mut db=SqliteStore::open(&path).unwrap();db.integrity_check().unwrap();
        if point=="before_reservation_commit" {
            assert_eq!(db.read_snapshot(None).unwrap(),before,"uncommitted reservation survived SIGKILL");
            assert!(!path.with_extension("effect").exists());continue;
        }
        let after=db.read_snapshot(None).unwrap();assert_eq!(after.attempt_inputs.len(),1);
        assert_eq!(db.queue_report(2001).unwrap().available_slots,0);
        let record=&after.attempt_inputs[0];
        assert!(db.reserve_prepared(&prepared,after.head,2001).is_err());
        if point=="after_reservation_commit" {
            assert_eq!(after.approvals.iter().filter(|a|a.consumed.is_some()).count(),0);
            assert!(db.cancel_attempt(&record.attempt,1,after.head,"never submitted",2001).unwrap().released);
            assert_eq!(db.queue_report(2001).unwrap().available_slots,1);
        } else {
            assert_eq!(after.approvals.iter().filter(|a|a.consumed.is_some()).count(),1);
            assert_eq!(db.expire_claims(2001).unwrap(),1);
            let delivery=db.deliveries().unwrap().into_iter().find(|d|d.operation==record.operation).unwrap();
            assert_eq!(delivery.state,crate::operations::DeliveryState::Ambiguous);
            assert!(db.claim_operation(&record.operation,delivery.revision,"replacement",2002,1000).is_err());
            let h=db.read_snapshot(None).unwrap().head;
            assert!(!db.cancel_attempt(&record.attempt,1,h,"uncertain worker",2002).unwrap().released);
            assert_eq!(db.queue_report(2002).unwrap().available_slots,0);
        }
        assert_eq!(path.with_extension("effect").exists(),point=="after_external_effect");
    }
}

fn started_fixture() -> (tempfile::TempDir,SqliteStore,Reservation,crate::operations::Claim,PreparedLaunchStarted) {
    let(temp,mut db,mut preparations)=fixture();
    let p=&mut preparations[0];
    let mut binding=db.read_snapshot(None).unwrap().runtime_bindings.into_iter().find(|b|b.id==p.inputs.binding).unwrap();
    binding.identity.socket="/fixture/herdr.sock".into();binding.identity.cwd="/fixture/task".into();
    let payload=serde_json::to_string(&binding).unwrap();
    db.connection.execute("UPDATE runtime_bindings SET payload=?2,payload_hash=?3 WHERE id=?1",params![binding.id,payload,format!("{:x}",Sha256::digest(payload.as_bytes()))]).unwrap();
    p.inputs.binding_digest=super::super::ownership::identity_digest(&binding).unwrap();
    let grant=ApprovalGrant{version:1,scope:ApprovalScope::for_launch(&p.inputs).unwrap(),policy:p.inputs.effective_profile.as_ref().unwrap().permission_policy.clone(),issued_unix_ms:0,expires_unix_ms:100_000};
    let head=db.read_snapshot(None).unwrap().head;p.inputs.approval=db.install_approval(&PreparedApproval{grant},head,1000).unwrap();
    let reservation=reserve(&mut db,&preparations[..1]);
    let claim=db.claim_operation(&reservation.record.operation,1,"launch-adapter",1000,1000).unwrap();
    let receipt=PreparedLaunchStarted{receipt:LaunchStartedReceipt {
        version:1,attempt:reservation.record.attempt.clone(),operation:reservation.record.operation.clone(),
        route:RuntimeRoute{socket:"/fixture/herdr.sock".into(),cwd:"/fixture/task".into(),workspace_id:"workspace".into(),tab_id:"tab".into(),pane_id:"pane".into(),machine:String::new()},
        terminal:"terminal".into(),session:ResourceIdentity{device:1,inode:2,born_secs:3,born_nanos:4},
        agent:AgentIdentity{kind:"claude".into(),name:worker_agent_name(&reservation.record.attempt)},supervisor:None,observed_unix_ms:1001,
    }};
    let target=PreparedLaunchTarget{target:LaunchTarget{version:1,attempt:receipt.receipt.attempt.clone(),operation:receipt.receipt.operation.clone(),route:receipt.receipt.route.clone(),terminal:receipt.receipt.terminal.clone(),session:receipt.receipt.session.clone(),supervisor:None,observed_unix_ms:1000}};
    db.record_launch_target(&claim,&target,1001).unwrap();
    (temp,db,reservation,claim,receipt)
}

fn brief_fixture() -> (tempfile::TempDir,SqliteStore,PreparedWorkerBrief,PreparedLaunchStarted) {
    let(temp,mut db,reservation,claim,started)=started_fixture();
    db.record_launch_started(&claim,&started,1002).unwrap();
    let state=db.read_snapshot(None).unwrap();
    let owned=state.ownership.iter().find(|o|o.attempt.as_ref()==Some(&reservation.record.attempt)).unwrap();
    let prepared=PreparedWorkerBrief{intent:WorkerBriefIntent{version:1,attempt:reservation.record.attempt.clone(),launch:reservation.record.operation.clone(),
        binding:owned.binding.clone(),binding_revision:owned.binding_revision,ownership_revision:owned.revision,
        knowledge:reservation.record.inputs.memory.clone(),prompt_digest:"a".repeat(64),prompt_chars:32}};
    (temp,db,prepared,started)
}

fn brief_receipt(operation:&Operation,prepared:&PreparedWorkerBrief,started:&PreparedLaunchStarted)->PreparedWorkerBriefReceipt {
    PreparedWorkerBriefReceipt{receipt:WorkerBriefReceipt{version:1,operation:operation.id.clone(),intent:prepared.intent.clone(),
        session:started.receipt.session.clone(),terminal:started.receipt.terminal.clone(),agent:started.receipt.agent.clone(),observed_unix_ms:1004}}
}

#[test]
fn worker_brief_confirmation_is_atomic_distinct_and_retains_capacity() {
    let(_temp,mut db,prepared,started)=brief_fixture();
    let head=db.read_snapshot(None).unwrap().head;
    let operation=db.enqueue_worker_brief(&prepared,head,1003).unwrap();
    let queued=db.read_snapshot(None).unwrap();
    assert_eq!(queued.attempts[0].state,AttemptState::Launching);
    assert_eq!(db.enqueue_worker_brief(&prepared,queued.head,1003).unwrap(),operation);
    assert_eq!(db.read_snapshot(None).unwrap(),queued);
    let claim=db.claim_operation(&operation.id,1,"brief-adapter",1004,1000).unwrap();
    let receipt=brief_receipt(&operation,&prepared,&started);
    let before=db.read_snapshot(None).unwrap();
    assert!(db.finish_operation(&claim,Outcome::Confirmed{observed_identity:"caller-supplied".into()},1005).is_err());
    db.connection.execute_batch("CREATE TRIGGER refuse_brief_outcome BEFORE INSERT ON events WHEN NEW.kind='operation.outcome' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(db.record_worker_brief_delivered(&claim,&receipt,1005).is_err());
    assert_eq!(db.read_snapshot(None).unwrap(),before);
    db.connection.execute_batch("DROP TRIGGER refuse_brief_outcome").unwrap();
    let delivery=db.record_worker_brief_delivered(&claim,&receipt,1005).unwrap();
    assert_eq!(delivery.state,DeliveryState::Confirmed);
    let after=db.read_snapshot(None).unwrap();
    assert_eq!(after.attempts[0].state,AttemptState::Running);
    assert!(after.attempts[0].retains_capacity());
    assert_eq!(after.tasks,before.tasks);
    assert_eq!(after.ownership,before.ownership);
    assert_eq!(db.record_worker_brief_delivered(&claim,&receipt,1006).unwrap(),delivery);
    assert_eq!(db.read_snapshot(None).unwrap(),after);
}

#[test]
fn brief_authority_reads_only_its_consumed_approval() {
    use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
    let (_temp, mut db, prepared, _) = brief_fixture();
    let operation = db.enqueue_worker_brief(&prepared, head(&db.connection).unwrap(), 1003).unwrap();
    let selected = read_input(&db.connection, &prepared.intent.launch, None).unwrap();
    let mut grant = super::super::approvals::read_all(&db.connection).unwrap()
        .into_iter().find(|a| a.reference == selected.inputs.approval).unwrap().grant;
    let mut work = Vec::new();
    for history in [0, 10_000] {
        // Valid unconsumed grants model retained history, not authority for this attempt.
        let tx = db.connection.transaction().unwrap();
        for index in 0..history {
            grant.expires_unix_ms = 200_000 + index;
            let reference = grant.reference().unwrap();
            tx.execute("INSERT INTO approval_grants VALUES(?1,?2,?3)",
                params![reference.id, serde_json::to_string(&grant).unwrap(), reference.digest]).unwrap();
        }
        tx.commit().unwrap();
        let steps = Arc::new(AtomicU64::new(0));
        let observed = steps.clone();
        db.connection.progress_handler(1, Some(move || { observed.fetch_add(1, Ordering::Relaxed); false }));
        let budget = super::super::read_budget::ReadBudget::new(super::super::controlled::ReadControl::new(
            std::time::Instant::now() + std::time::Duration::from_secs(30), crate::runner::Cancellation::default()));
        let result = super::super::worker_brief::check_with_budget(&db.connection, &operation.id, 1004, Some(&budget));
        db.connection.progress_handler(0, None::<fn() -> bool>);
        result.unwrap();
        work.push(steps.load(Ordering::Relaxed));
    }
    eprintln!("brief pre-effect SQL steps with 0/10000 unrelated grants: {work:?}");
    assert!(work[1] <= work[0] + 500, "brief validation scanned approval history: {work:?}");
    // Selected corruption must still fail closed; historical claim checks matter
    // even after a launch has a confirmed delivery.
    db.connection.execute_batch("DROP TRIGGER approval_uses_no_update").unwrap();
    db.connection.execute("UPDATE approval_uses SET claim_revision=claim_revision+100 WHERE approval_id=?1",
        [&selected.inputs.approval.id]).unwrap();
    assert!(matches!(super::super::worker_brief::check_with_budget(&db.connection, &operation.id, 1004, None), Err(StoreError::Corrupt(_))));
}

#[test]
fn brief_claim_and_pre_effect_refuse_revocation_pause_config_changes_and_cancellation() {
    for claimed in [false,true] { for change in 0..4 {
        let(_temp,mut db,prepared,_)=brief_fixture();
        let head=db.read_snapshot(None).unwrap().head;
        let operation=db.enqueue_worker_brief(&prepared,head,1003).unwrap();
        let claim=claimed.then(||db.claim_operation(&operation.id,1,"brief-adapter",1004,1000).unwrap());
        let state=db.read_snapshot(None).unwrap();
        let record=state.attempt_inputs.iter().find(|r|r.attempt==prepared.intent.attempt).unwrap();
        match change {
            0=>{db.revoke_approval(&record.inputs.approval.id,state.head,1005,"owner revoked").unwrap();},
            1=>{db.set_project_state(state.head,state.control.unwrap().revision,ProjectState::Paused,1005,None).unwrap();},
            2=>{std::fs::write(&record.inputs.config.path,"# changed").unwrap();},
            _=>{db.connection.execute("INSERT INTO attempt_cancellations VALUES(?1,1005,'cancel')",[prepared.intent.attempt.as_str()]).unwrap();},
        }
        let before=db.read_snapshot(None).unwrap();
        if let Some(claim)=claim {assert!(db.validate_claim(&claim,1006).is_err());}
        else {assert!(db.claim_operation(&operation.id,1,"brief-adapter",1006,1000).is_err());}
        assert_eq!(db.read_snapshot(None).unwrap(),before);
    }}
}

#[test]
fn uncertain_brief_never_replays_even_after_no_effect_report() {
    let(_temp,mut db,prepared,_)=brief_fixture();
    let head=db.read_snapshot(None).unwrap().head;
    let operation=db.enqueue_worker_brief(&prepared,head,1003).unwrap();
    db.claim_operation(&operation.id,1,"brief-adapter",1004,1).unwrap();
    db.expire_claims(1006).unwrap();
    let state=db.read_snapshot(None).unwrap();
    let delivery=state.deliveries.iter().find(|d|d.operation==operation.id).unwrap();
    assert!(db.observe_operation(&operation.id,delivery.revision,"caller",Outcome::Confirmed{observed_identity:"screen text".into()},1007).is_err());
    let pending=db.observe_operation(&operation.id,delivery.revision,"caller",Outcome::Retryable{no_effect_evidence:"claimed absent".into()},1007).unwrap();
    assert!(db.claim_operation(&operation.id,pending.revision,"retry",pending.next_due_ms,1000).is_err());
    let mut changed=prepared;changed.intent.prompt_digest="b".repeat(64);
    let before=db.read_snapshot(None).unwrap();
    assert!(db.enqueue_worker_brief(&changed,before.head,1008).is_err());
    assert_eq!(db.read_snapshot(None).unwrap(),before);
}

#[test]
fn brief_recovery_records_existing_delivery_without_renewing_authority() {
    let(_temp,mut db,prepared,started)=brief_fixture();
    let head=db.read_snapshot(None).unwrap().head;
    let operation=db.enqueue_worker_brief(&prepared,head,1003).unwrap();
    db.claim_operation(&operation.id,1,"brief-adapter",1004,1).unwrap();
    db.expire_claims(1006).unwrap();
    let state=db.read_snapshot(None).unwrap();
    let record=state.attempt_inputs.iter().find(|r|r.attempt==prepared.intent.attempt).unwrap();
    db.revoke_approval(&record.inputs.approval.id,state.head,1007,"owner revoked").unwrap();
    let state=db.read_snapshot(None).unwrap();
    db.set_project_state(state.head,state.control.unwrap().revision,ProjectState::Paused,1008,None).unwrap();
    let before=db.read_snapshot(None).unwrap();
    let delivery=before.deliveries.iter().find(|d|d.operation==operation.id).unwrap();
    let receipt=brief_receipt(&operation,&prepared,&started);
    db.observe_worker_brief_delivered(&receipt,delivery.revision,before.head,1009).unwrap();
    let after=db.read_snapshot(None).unwrap();
    assert_eq!(after.control,before.control);assert_eq!(after.approvals,before.approvals);
    assert_eq!(after.tasks,before.tasks);assert!(after.attempts[0].retains_capacity());
    assert_eq!(after.attempts[0].state,AttemptState::Running);
}

#[test]
fn brief_receipt_rejects_replaced_worker_or_prompt_and_stale_claim() {
    for change in 0..7 {
        let(_temp,mut db,prepared,started)=brief_fixture();
        let head=db.read_snapshot(None).unwrap().head;
        let operation=db.enqueue_worker_brief(&prepared,head,1003).unwrap();
        let claim=db.claim_operation(&operation.id,1,"brief-adapter",1004,1000).unwrap();
        let mut receipt=brief_receipt(&operation,&prepared,&started);
        let mut now=1005;
        match change {
            0=>receipt.receipt.session.inode+=1,
            1=>receipt.receipt.terminal="replacement".into(),
            2=>receipt.receipt.agent.name="foreign".into(),
            3=>receipt.receipt.intent.prompt_digest="b".repeat(64),
            4=>receipt.receipt.intent.ownership_revision+=1,
            5=>receipt.receipt.observed_unix_ms=999,
            _=>now=2005,
        }
        let before=db.read_snapshot(None).unwrap();
        assert!(db.record_worker_brief_delivered(&claim,&receipt,now).is_err(),"{change}");
        assert_eq!(db.read_snapshot(None).unwrap(),before);
    }
}

#[test]
fn typed_start_receipt_publishes_exact_ownership_atomically_without_releasing_capacity() {
    let(_temp,mut db,reservation,claim,receipt)=started_fixture();
    let before=db.read_snapshot(None).unwrap();
    db.connection.execute_batch("CREATE TRIGGER refuse_launch_outcome BEFORE INSERT ON events WHEN NEW.kind='operation.outcome' BEGIN SELECT RAISE(ABORT,'injected launch receipt failure'); END;").unwrap();
    assert!(db.record_launch_started(&claim,&receipt,1002).is_err());
    assert_eq!(db.read_snapshot(None).unwrap(),before);
    db.connection.execute_batch("DROP TRIGGER refuse_launch_outcome;").unwrap();
    let delivery=db.record_launch_started(&claim,&receipt,1002).unwrap();
    assert_eq!(delivery.state,crate::operations::DeliveryState::Confirmed);
    let after=db.read_snapshot(None).unwrap();
    let attempt=after.attempts.iter().find(|a|a.id==reservation.record.attempt).unwrap();
    assert_eq!(attempt.state,AttemptState::Launching);assert!(attempt.retains_capacity());
    assert_eq!(attempt.snapshot,before.attempts.iter().find(|a|a.id==attempt.id).unwrap().snapshot);
    let owned=after.ownership.iter().find(|o|o.attempt.as_ref()==Some(&attempt.id)).unwrap();
    assert_eq!(owned.origin,"launched");assert_eq!(owned.session.as_ref(),Some(&receipt.receipt.session));
    assert_eq!(after.tasks,before.tasks);
    assert_eq!(db.queue_report(1002).unwrap().available_slots,0);
    assert_eq!(db.record_launch_started(&claim,&receipt,1003).unwrap(),delivery);
    assert_eq!(db.read_snapshot(None).unwrap(),after);
    assert!(db.claim_operation(&claim.operation,delivery.revision,"duplicate",1003,1000).is_err());
    assert!(db.relinquish_runtime(&owned.binding,owned.revision,after.head,"pretend absent").is_err());
}

#[test]
fn typed_start_receipt_refuses_wrong_incarnations_routes_attempts_and_expired_claims() {
    for variant in 0..10 {
        let(_temp,mut db,_reservation,claim,mut receipt)=started_fixture();
        match variant {
            0=>receipt.receipt.attempt=AttemptId::new("another-attempt").unwrap(),
            1=>receipt.receipt.route.socket="/another/socket".into(),
            2=>receipt.receipt.route.cwd="/another/cwd".into(),
            3=>receipt.receipt.agent.kind="codex".into(),
            4=>receipt.receipt.agent.name="foreign-worker".into(),
            5=>receipt.receipt.route.machine="remote".into(),
            6=>receipt.receipt.session.born_nanos=1_000_000_000,
            8=>receipt.receipt.session.inode+=1,
            9=>receipt.receipt.terminal="replacement-terminal".into(),
            _=>{},
        }
        let before=db.read_snapshot(None).unwrap();
        assert!(db.record_launch_started(&claim,&receipt,if variant==7 {2000}else{1002}).is_err(),"variant {variant}");
        assert_eq!(db.read_snapshot(None).unwrap(),before);
    }
}

#[test]
fn lost_start_response_recovers_exact_worker_without_reauthorizing_or_restarting() {
    let(_temp,mut db,_reservation,claim,mut receipt)=started_fixture();
    db.expire_claims(2000).unwrap();
    let state=db.read_snapshot(None).unwrap();
    let used=state.approvals.iter().find(|a|a.consumed.is_some()).unwrap();
    db.revoke_approval(&used.reference.id,state.head,2001,"stop future effects").unwrap();
    let state=db.read_snapshot(None).unwrap();
    db.set_project_state(state.head,state.control.unwrap().revision,ProjectState::Paused,2002,None).unwrap();
    receipt.receipt.observed_unix_ms=2003;
    let before=db.read_snapshot(None).unwrap();
    let old=before.deliveries.iter().find(|d|d.operation==claim.operation).unwrap();
    assert!(db.observe_launch_started(&receipt,old.revision,before.head-1,2003).is_err());
    assert_eq!(db.read_snapshot(None).unwrap(),before);
    db.observe_launch_started(&receipt,old.revision,before.head,2003).unwrap();
    let after=db.read_snapshot(None).unwrap();
    assert_eq!(after.control,before.control);assert_eq!(after.tasks,before.tasks);
    assert!(after.attempts.iter().all(|a|a.retains_capacity()));
    assert_eq!(after.attempts[0].state,AttemptState::Launching);
    assert_eq!(after.deliveries[0].attempts,1);
    assert_eq!(after.ownership.len(),1);
    assert!(after.approvals.iter().any(|a|a.consumed.is_some()&&a.revoked.is_some()));
}

#[test]
fn typed_start_crash_child() {
    let Some(path)=std::env::var_os("HERDR_TYPED_START_DB") else {return;};
    let path=std::path::PathBuf::from(path);
    let mut db=SqliteStore::open(&path).unwrap();
    let(claim,receipt):(crate::operations::Claim,LaunchStartedReceipt)=serde_json::from_slice(&std::fs::read(path.with_extension("receipt.json")).unwrap()).unwrap();
    db.record_launch_started(&claim,&PreparedLaunchStarted{receipt},1002).unwrap();
    crash_boundary("after_start_receipt_commit");
    panic!("unknown start receipt crash point");
}

#[test]
fn sigkill_start_receipt_commit_never_splits_confirmation_ownership_and_attempt() {
    use std::{process::{Command,Stdio},time::{Duration,Instant}};
    for point in ["before_start_receipt_commit","after_start_receipt_commit"] {
        let(temp,mut db,_reservation,claim,receipt)=started_fixture();
        let before=db.read_snapshot(None).unwrap();let path=temp.path().join(".state/state.db");
        std::fs::write(path.with_extension("receipt.json"),serde_json::to_vec(&(&claim,&receipt.receipt)).unwrap()).unwrap();drop(db);
        let marker=temp.path().join("receipt-ready");
        let mut child=Command::new(std::env::current_exe().unwrap())
            .args(["--exact","store::reservations::tests::typed_start_crash_child","--nocapture"])
            .env("HERDR_TYPED_START_DB",&path).env("HERDR_LAUNCH_CRASH_POINT",point)
            .env("HERDR_LAUNCH_CRASH_MARKER",&marker).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let deadline=Instant::now()+Duration::from_secs(20);
        while !marker.exists()&&Instant::now()<deadline {
            if let Some(status)=child.try_wait().unwrap(){panic!("typed receipt child exited before {point}: {status}");}
            std::thread::sleep(Duration::from_millis(10));
        }
        let ready=marker.exists();let _=child.kill();child.wait().unwrap();assert!(ready,"receipt child did not reach {point}");
        let mut db=SqliteStore::open(&path).unwrap();db.integrity_check().unwrap();
        let after=db.read_snapshot(None).unwrap();
        if point=="before_start_receipt_commit" {
            assert_eq!(after,before);
            db.expire_claims(2000).unwrap();
            let state=db.read_snapshot(None).unwrap();
            let revision=state.deliveries.iter().find(|d|d.operation==claim.operation).unwrap().revision;
            db.observe_launch_started(&receipt,revision,state.head,2001).unwrap();
        } else {
            assert_eq!(after.ownership.len(),1);
            assert_eq!(after.attempts[0].state,AttemptState::Launching);
            assert_eq!(after.deliveries[0].state,crate::operations::DeliveryState::Confirmed);
            db.record_launch_started(&claim,&receipt,1003).unwrap();assert_eq!(db.read_snapshot(None).unwrap(),after);
        }
        assert_eq!(db.queue_report(2001).unwrap().available_slots,0);
        assert_eq!(db.read_snapshot(None).unwrap().ownership.len(),1);
    }
}

#[test]
fn selected_launch_target_is_immutable_and_replay_does_not_write() {
    let(_temp,mut db,_reservation,claim,_receipt)=started_fixture();
    let before=db.read_snapshot(None).unwrap();
    let event=before.events.iter().find(|e|e.kind=="runtime.launch_target").unwrap();
    let target:LaunchTarget=serde_json::from_value(event.payload.clone()).unwrap();
    assert_eq!(db.record_launch_target(&claim,&PreparedLaunchTarget{target:target.clone()},1002).unwrap(),before.head);
    assert_eq!(db.read_snapshot(None).unwrap(),before);
    for replacement in 0..3 {
        let mut target=target.clone();
        match replacement {
            0=>target.terminal="different-terminal".into(),
            1=>target.session.inode+=1,
            _=>target.route.pane_id="different-pane".into(),
        }
        assert!(db.record_launch_target(&claim,&PreparedLaunchTarget{target},1002).is_err());
        assert_eq!(db.read_snapshot(None).unwrap(),before);
    }
}


#[test]
fn reservation_requires_current_exact_unconsumed_approval_without_using_it() {
    for case in ["missing", "digest", "expired", "revoked", "config", "profile", "policy"] {
        let (_temp, mut db, mut prepared) = fixture();
        let mut now=1000;
        match case {
            "missing" => prepared[0].inputs.approval.id="missing-grant".into(),
            "digest" => prepared[0].inputs.approval.digest="f".repeat(64),
            "expired" => now=100_000,
            "revoked" => {
                let head=db.read_snapshot(None).unwrap().head;
                db.revoke_approval(&prepared[0].inputs.approval.id,head,now,"withdraw before admission").unwrap();
            },
            "config" => std::fs::write(&prepared[0].inputs.config.path,"# changed before admission").unwrap(),
            "policy" => {
                let mut policy=prepared[0].inputs.effective_profile.as_ref().unwrap().permission_policy.clone();
                policy.id="different-permission-policy".into();
                let grant=ApprovalGrant{version:1,scope:ApprovalScope::for_launch(&prepared[0].inputs).unwrap(),policy,issued_unix_ms:0,expires_unix_ms:100_000};
                let head=db.read_snapshot(None).unwrap().head;
                prepared[0].inputs.approval=db.install_approval(&PreparedApproval{grant},head,now).unwrap();
            },
            "profile" => {
                let profile=prepared[0].inputs.effective_profile.as_mut().unwrap();
                profile.arguments_digest="f".repeat(64);
                prepared[0].inputs.profile=profile.reference().unwrap();
            },
            _ => unreachable!(),
        }
        let before=db.read_snapshot(None).unwrap();
        assert!(db.reserve_prepared(&prepared,before.head,now).is_err(),"{case}");
        assert_eq!(db.read_snapshot(None).unwrap(),before,"{case}");
        assert!(before.attempts.is_empty());
        assert!(before.approvals.iter().all(|a|a.consumed.is_none()));
    }
    let (_temp,mut db,prepared)=fixture();
    let reservation=reserve(&mut db,&prepared);
    let state=db.read_snapshot(None).unwrap();
    assert!(state.approvals.iter().all(|a|a.consumed.is_none()));
    db.claim_operation(&reservation.record.operation,1,"worker",1001,1000).unwrap();
    let state=db.read_snapshot(None).unwrap();
    assert_eq!(state.approvals.iter().filter(|a|a.consumed.is_some()).count(),1);
    assert!(super::super::approvals::validate_preparation(&db.connection,&reservation.record.inputs,1002).is_err());
    assert_eq!(db.read_snapshot(None).unwrap(),state);
}

fn plant_verified(db:&SqliteStore,task:&str,attempt:&str,result_id:&str) {
    super::super::satisfaction::tests::store_verified_receipt(db,task,attempt,result_id,1,1);
}
fn reseal(db:&mut SqliteStore,task_name:&str,dependencies:Vec<DependencyInput>,repositories:Vec<RepositoryInput>,template:&LaunchInputs)->PreparedLaunch {
    let snapshot=db.read_snapshot(None).unwrap();
    let task=snapshot.tasks.iter().find(|task|task.id.as_str()==task_name).unwrap().clone();
    let binding=snapshot.runtime_bindings.iter().find(|binding|binding.task.as_ref()==Some(&task.id)).unwrap();
    let profile=template.effective_profile.clone().unwrap();
    let mut inputs=LaunchInputs{task_contract: None, version:2,project_store:template.project_store.clone(),task:task.id.clone(),task_revision:task.revision,scheduler_revision:snapshot.scheduler.as_ref().unwrap().policy.revision,control_epoch:snapshot.control.as_ref().unwrap().epoch,binding:binding.id.clone(),binding_revision:binding.revision,binding_digest:super::super::ownership::identity_digest(binding).unwrap(),profile:profile.reference().unwrap(),effective_profile:Some(profile.clone()),approval:VersionedReference{id:"unsigned-launch".into(),revision:1,digest:"0".repeat(64)},config:template.config.clone(),repositories,dependencies,memory:None,budget:template.budget.clone()};
    let grant=ApprovalGrant{version:1,scope:ApprovalScope::for_launch(&inputs).unwrap(),policy:profile.permission_policy,issued_unix_ms:0,expires_unix_ms:100_000};
    inputs.approval=db.install_approval(&PreparedApproval{grant},snapshot.head,1000).unwrap();
    PreparedLaunch{inputs}
}
fn queue_dependency(db:&mut SqliteStore,task_name:&str,predecessor:&str,requirement:DependencyRequirement) {
    let snapshot=db.read_snapshot(None).unwrap();
    let task=snapshot.tasks.iter().find(|task|task.id.as_str()==task_name).unwrap().clone();
    db.queue_task(&task.id,task.revision,snapshot.head,&QueueRequest{priority:0,dependencies:vec![Dependency{predecessor:TaskId::new(predecessor).unwrap(),requirement}]},1000).unwrap();
}
fn verified_dependent(db:&mut SqliteStore,template:&LaunchInputs)->PreparedLaunch {
    let head=db.read_snapshot(None).unwrap().head;
    db.commit(Commit{expected_head:head,mutations:vec![Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new("attempt-b").unwrap(),task:TaskId::new("b").unwrap(),revision:1,state:AttemptState::Completed,snapshot:None,reservation:"slot-b".into(),termination_observed:true}}]}).unwrap();
    plant_verified(db,"b","attempt-b",&"e".repeat(64));
    queue_dependency(db,"a","b",DependencyRequirement::VerifiedResult);
    let satisfaction_id:String=db.connection.query_row("SELECT satisfaction_id FROM dependency_satisfactions WHERE task_id='a' AND predecessor_task='b' AND state='valid'",[],|row|row.get(0)).unwrap();
    let predecessor_revision=db.read_snapshot(None).unwrap().tasks.iter().find(|task|task.id.as_str()=="b").unwrap().revision;
    reseal(db,"a",vec![DependencyInput{task:TaskId::new("b").unwrap(),task_revision:predecessor_revision,requirement:DependencyRequirement::VerifiedResult,evidence:VersionedReference{id:satisfaction_id.clone(),revision:1,digest:satisfaction_id}}],vec![],template)
}
#[test]
fn flag_off_dependency_evidence_does_not_reserve_and_flag_on_reserves_one() {
    let(_temp,mut db,prepared)=fixture();
    let launch=verified_dependent(&mut db,&prepared[0].inputs);
    let before=db.read_snapshot(None).unwrap();
    let err=db.reserve_prepared(&[launch.clone()],before.head,1000).unwrap_err();
    assert!(matches!(err,StoreError::Invalid(ref message) if message.contains("dependency evidence producers are not available")),"{err}");
    assert!(db.read_snapshot(None).unwrap().attempts.iter().all(|attempt|attempt.task.as_str()!="a"));
    db.testing_set_factory_admission(true).unwrap();
    let stale=db.reserve_prepared(&[launch.clone()],0,1000).unwrap_err();
    assert!(matches!(stale,StoreError::Conflict),"{stale}");
    assert!(db.read_snapshot(None).unwrap().attempts.iter().all(|attempt|attempt.task.as_str()!="a"));
    let head=db.read_snapshot(None).unwrap().head;
    let reserved=db.reserve_prepared(&[launch],head,1000).unwrap();
    assert_eq!(reserved.record.inputs.task.as_str(),"a");
    let attempts=db.read_snapshot(None).unwrap().attempts;
    assert_eq!(attempts.iter().filter(|attempt|attempt.task.as_str()=="a").count(),1);
}
#[test]
fn lost_attempt_still_fills_the_only_slot() {
    let(_temp,mut db,prepared)=fixture();
    let launch=verified_dependent(&mut db,&prepared[0].inputs);
    db.testing_set_factory_admission(true).unwrap();
    let snapshot=db.read_snapshot(None).unwrap();
    db.commit(Commit{expected_head:snapshot.head,mutations:vec![Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new("lost-b").unwrap(),task:TaskId::new("b").unwrap(),revision:1,state:AttemptState::Lost,snapshot:None,reservation:"slot-lost".into(),termination_observed:false}}]}).unwrap();
    let before=db.read_snapshot(None).unwrap();
    let err=db.reserve_prepared(&[launch],before.head,1000).unwrap_err();
    assert!(matches!(err,StoreError::Invalid(ref message) if message.contains("capacity")),"{err}");
    let after=db.read_snapshot(None).unwrap();
    assert!(after.attempts.iter().any(|attempt|attempt.id.as_str()=="lost-b"&&attempt.retains_capacity()));
    assert!(after.attempts.iter().all(|attempt|attempt.task.as_str()!="a"));
    assert_eq!(after.attempts.len(),before.attempts.len());
}
fn git_history()->(tempfile::TempDir,String,String) {
    let dir=tempfile::tempdir().unwrap();let repo=dir.path().join("repo");std::fs::create_dir(&repo).unwrap();
    let git=|args:&[&str]| {
        let output=std::process::Command::new("/usr/bin/git").arg("-C").arg(&repo).args(args).env_clear().env("PATH","/usr/bin:/bin").env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null").env("GIT_AUTHOR_NAME","t").env("GIT_AUTHOR_EMAIL","t@example.com").env("GIT_COMMITTER_NAME","t").env("GIT_COMMITTER_EMAIL","t@example.com").output().unwrap();
        assert!(output.status.success(),"git {args:?}: {}",String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    git(&["init","-b","main"]);std::fs::write(repo.join("a.txt"),"a").unwrap();git(&["add","a.txt"]);git(&["commit","-m","a"]);
    let first=git(&["rev-parse","HEAD"]);
    git(&["checkout","-b","side"]);std::fs::write(repo.join("b.txt"),"b").unwrap();git(&["add","b.txt"]);git(&["commit","-m","b"]);
    let second=git(&["rev-parse","HEAD"]);
    git(&["checkout","main"]);git(&["merge","--no-ff","-m","m","side"]);
    let merge=git(&["rev-parse","HEAD"]);
    assert_ne!(first,second);assert_ne!(merge,first);assert_ne!(merge,second);
    (dir,std::fs::canonicalize(repo).unwrap().display().to_string(),format!("{first} {second} {merge}"))
}
fn plant_integrated(db:&SqliteStore,integrated_id:&str,verified_result_id:&str,repository:&str,commit_oid:&str) {
    let digest="d".repeat(64);let old="c".repeat(40);let candidate=format!("{:x}",Sha256::digest(integrated_id.as_bytes()));
    db.connection.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
    db.connection.execute("INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,candidate_id,state,generation,object_format,checks_passed,reason,created_unix_ms) VALUES(?1,'/tmp/project',?1,?2,?5,'refs/heads/integration',?3,?4,NULL,'integrated',1,'sha1',1,NULL,1)",params![integrated_id,digest,old,verified_result_id,repository]).unwrap();
    db.connection.execute("INSERT INTO integrated_commits(integrated_id,candidate_id,operation_id,repository,ref_name,commit_oid,tree_oid,expected_old_oid,object_format,created_unix_ms) VALUES(?1,?2,?1,?5,'refs/heads/integration',?3,?3,?4,'sha1',1)",params![integrated_id,candidate,commit_oid,old,repository]).unwrap();
    db.connection.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
}
#[test]
fn two_integrated_parents_reserve_only_when_the_base_contains_both() {
    let(_git,repository,oids)=git_history();
    let mut oid=oids.split_whitespace();let first=oid.next().unwrap().to_string();let second=oid.next().unwrap().to_string();let merge=oid.next().unwrap().to_string();
    for (pin,reserves) in [(merge,true),(first.clone(),false)] {
        let(_temp,mut db,prepared)=fixture();
        let snapshot=db.read_snapshot(None).unwrap();
        db.commit(Commit{expected_head:snapshot.head,mutations:vec![Mutation::Task{expected:None,next:Task{id:TaskId::new("c").unwrap(),revision:1,state:TaskState::Draft,title:"c".into(),active_attempt:None}}]}).unwrap();
        let head=db.read_snapshot(None).unwrap().head;
        db.commit(Commit{expected_head:head,mutations:vec![Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new("attempt-b").unwrap(),task:TaskId::new("b").unwrap(),revision:1,state:AttemptState::Completed,snapshot:None,reservation:"slot-b".into(),termination_observed:true}},Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new("attempt-c").unwrap(),task:TaskId::new("c").unwrap(),revision:1,state:AttemptState::Completed,snapshot:None,reservation:"slot-c".into(),termination_observed:true}}]}).unwrap();
        plant_verified(&db,"b","attempt-b",&"e".repeat(64));
        plant_verified(&db,"c","attempt-c",&"f".repeat(64));
        plant_integrated(&db,&"1".repeat(64),&"e".repeat(64),&repository,&first);
        plant_integrated(&db,&"2".repeat(64),&"f".repeat(64),&repository,&second);
        let snapshot=db.read_snapshot(None).unwrap();
        let task=snapshot.tasks.iter().find(|task|task.id.as_str()=="a").unwrap().clone();
        db.queue_task(&task.id,task.revision,snapshot.head,&QueueRequest{priority:0,dependencies:vec![Dependency{predecessor:TaskId::new("b").unwrap(),requirement:DependencyRequirement::IntegratedCommit},Dependency{predecessor:TaskId::new("c").unwrap(),requirement:DependencyRequirement::IntegratedCommit}]},1000).unwrap();
        let mut satisfaction=|predecessor:&str|->(String,u64){
            let id:String=db.connection.query_row("SELECT satisfaction_id FROM dependency_satisfactions WHERE task_id='a' AND predecessor_task=?1 AND requirement='integrated_commit' AND state='valid'",[predecessor],|row|row.get(0)).unwrap();
            let revision=db.read_snapshot(None).unwrap().tasks.iter().find(|task|task.id.as_str()==predecessor).unwrap().revision;
            (id,revision)
        };
        let (left_id,left_revision)=satisfaction("b");let (right_id,right_revision)=satisfaction("c");
        let launch=reseal(&mut db,"a",vec![DependencyInput{task:TaskId::new("b").unwrap(),task_revision:left_revision,requirement:DependencyRequirement::IntegratedCommit,evidence:VersionedReference{id:left_id.clone(),revision:1,digest:left_id}},DependencyInput{task:TaskId::new("c").unwrap(),task_revision:right_revision,requirement:DependencyRequirement::IntegratedCommit,evidence:VersionedReference{id:right_id.clone(),revision:1,digest:right_id}}],vec![RepositoryInput{repository:repository.clone(),commit:pin.clone(),tree:"a".repeat(40)}],&prepared[0].inputs);
        db.testing_set_factory_admission(true).unwrap();
        let head=db.read_snapshot(None).unwrap().head;
        let result=db.reserve_prepared(&[launch],head,1000);
        if reserves {
            let reserved=result.unwrap();
            assert_eq!(reserved.record.inputs.task.as_str(),"a");
            assert_eq!(reserved.record.inputs.repositories[0].commit,pin);
        } else {
            let err=result.unwrap_err();
            assert!(matches!(err,StoreError::Invalid(ref message) if message.contains("integration_missing")),"{err}");
            assert!(db.read_snapshot(None).unwrap().attempts.iter().all(|attempt|attempt.task.as_str()!="a"));
        }
    }
}
#[cfg(target_os="linux")]
#[test]
fn missing_grant_records_authority_missing_and_does_not_reserve() {
    let(temp,mut db,_prepared)=fixture();
    db.testing_set_factory_admission(true).unwrap();
    crate::admission::admit_once(temp.path()).unwrap();
    assert!(db.read_snapshot(None).unwrap().attempts.is_empty());
    assert!(db.authority_denials().unwrap().iter().any(|denial|denial.reason_code=="authority_missing"));
}
