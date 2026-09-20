use super::*;
fn fixture()->(tempfile::TempDir,std::path::PathBuf) {
    let root=tempfile::tempdir().unwrap();let path=root.path().join("state.db");SqliteStore::create(&path).unwrap();(root,path)
}
fn control()->ReadControl {ReadControl::new(Instant::now()+Duration::from_secs(5),Cancellation::default())}
fn insert(db:&mut SqliteStore)->Result<u64> {
    let head=db.read_snapshot(None)?.head;
    db.commit(Commit{expected_head:head,mutations:vec![Mutation::Task{expected:None,next:Task{id:TaskId::new("task").unwrap(),revision:1,state:TaskState::Draft,title:"task".into(),active_attempt:None}}]})
}
#[test]
fn cancelled_and_expired_open_refuse_without_modifying_store() {
    let(_root,path)=fixture();let before=std::fs::read(&path).unwrap();let c=control();c.cancellation.cancel();
    assert!(matches!(ControlledStore::open(&path,c),Err(StoreError::Cancelled)));
    assert!(matches!(ControlledStore::open(&path,ReadControl::new(Instant::now(),Cancellation::default())),Err(StoreError::Deadline)));
    assert_eq!(std::fs::read(path).unwrap(),before);
}
#[test]
fn opening_installs_progress_control_before_schema_queries() {
    let(_root,path)=fixture();let raw=Connection::open(&path).unwrap();
    raw.execute_batch("ALTER TABLE store_meta RENAME TO original_meta; CREATE VIEW store_meta AS SELECT * FROM original_meta WHERE (WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n)>0;").unwrap();drop(raw);
    let start=Instant::now();let result=ControlledStore::open(&path,ReadControl::new(start+Duration::from_millis(100),Cancellation::default()));
    assert!(matches!(result,Err(StoreError::Deadline)));assert!(start.elapsed()<Duration::from_secs(2));
}
#[test]
fn snapshot_sql_retains_original_control_and_encoded_row_limit() {
    let(_root,path)=fixture();let mut db=ControlledStore::open(&path,control()).unwrap();
    let raw=Connection::open(&path).unwrap();raw.execute_batch("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('fixture','x',1,1,'{}'); ALTER TABLE events RENAME TO original_events; CREATE VIEW events AS SELECT * FROM original_events WHERE (WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n)>0;").unwrap();
    // Abort after worker entry, while SQLite is still inside the first query.
    let token=db.control.cancellation();let cancel=std::thread::spawn(move||{std::thread::sleep(Duration::from_millis(30));token.cancel();});
    let start=Instant::now();assert!(matches!(db.read_snapshot(None),Err(StoreError::Cancelled)));assert!(start.elapsed()<Duration::from_secs(2));cancel.join().unwrap();
    let(_root,path)=fixture();let raw=Connection::open(&path).unwrap();raw.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('large','x',1,1,?1)",[serde_json::to_string(&"x".repeat(2*1024*1024)).unwrap()]).unwrap();drop(raw);
    let opened=ControlledStore::open(&path,control().with_row_limit(1024*1024).unwrap());
    let result=match opened {Ok(mut db)=>db.read_snapshot(None).map(|_|()),Err(error)=>Err(error)};
    assert!(matches!(result,Err(StoreError::Limit(_))),"{result:?}");
}
#[test]
fn commit_hook_veto_rolls_back_and_success_after_commit_is_not_relabelled() {
    let(_root,path)=fixture();let before=SqliteStore::open(&path).unwrap().read_snapshot(None).unwrap();let mut db=ControlledStore::open(&path,control()).unwrap();let cancellation=db.control.cancellation();
    let result=db.mutation(|store|{
        let tx=store.connection.transaction()?;
        tx.execute("INSERT INTO tasks(id,revision,state,title) VALUES('task',1,'draft','vetoed')",[])?;
        cancellation.cancel();tx.commit()?;Ok(())
    });
    assert!(matches!(result,Err(StoreError::Cancelled)));drop(db);
    assert_eq!(SqliteStore::open(&path).unwrap().read_snapshot(None).unwrap(),before);
    let mut db=ControlledStore::open(&path,control()).unwrap();let cancellation=db.control.cancellation();
    let result=db.mutation(|store|{let head=insert(store)?;cancellation.cancel();Ok(head)});
    assert!(result.is_ok());drop(db);assert_eq!(SqliteStore::open(&path).unwrap().read_snapshot(None).unwrap().tasks.len(),1);
}
#[test]
fn unrelated_error_is_not_reclassified_when_cancel_arrives_during_unwind() {
    let(_root,path)=fixture();let mut db=ControlledStore::open(&path,control()).unwrap();let cancellation=db.control.cancellation();
    let result:Result<()>=db.mutation(|_|{cancellation.cancel();Err(StoreError::Conflict)});
    assert!(matches!(result,Err(StoreError::Conflict)));
}
#[test]
fn controlled_writes_do_not_wait_for_legacy_busy_timeout() {
    let(_root,path)=fixture();let mut db=ControlledStore::open(&path,control()).unwrap();let raw=Connection::open(&path).unwrap();raw.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start=Instant::now();assert!(matches!(db.mutation(insert),Err(StoreError::Busy)));assert!(start.elapsed()<Duration::from_millis(200));raw.execute_batch("ROLLBACK").unwrap();
    assert!(db.mutation(insert).is_ok());
}
#[test]
fn controlled_open_preserves_integrity_and_schema_refusal() {
    for sql in ["PRAGMA user_version=99;","PRAGMA foreign_keys=OFF; INSERT INTO task_dependencies VALUES('missing','also-missing','verified_result');"] {
        let(_root,path)=fixture();let raw=Connection::open(&path).unwrap();raw.execute_batch(sql).unwrap();drop(raw);
        let result=ControlledStore::open(&path,control());assert!(matches!(result,Err(StoreError::UnsupportedSchema(99))|Err(StoreError::Corrupt(_))));
    }
}

#[test]
fn core_snapshot_accounting_refuses_dense_json_before_decode() {
    let(_root,path)=fixture();
    let mut db=ControlledStore::open(&path,control()).unwrap();
    let mut legacy=SqliteStore::open(&path).unwrap();
    let raw=Connection::open(&path).unwrap();
    // Inject corruption after integrity-checked opening. Accounting must reject
    // before serde sees this invalid payload, below the encoded-row limit.
    raw.execute_batch("PRAGMA ignore_check_constraints=ON").unwrap();
    let payload="[".repeat(500_000);
    raw.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('dense','x',1,1,?1)",[payload]).unwrap();drop(raw);
    assert!(matches!(db.read_snapshot(None),Err(StoreError::Limit(_))));
    assert!(matches!(legacy.read_snapshot(None),Err(StoreError::Corrupt(_))));
}
#[test]
fn core_snapshot_accounting_is_shared_across_tables_and_resets_per_snapshot() {
    let(_root,path)=fixture();let mut legacy=SqliteStore::open(&path).unwrap();insert(&mut legacy).unwrap();
    let expected=legacy.read_snapshot(None).unwrap();let mut db=ControlledStore::open(&path,control()).unwrap();
    assert_eq!(db.read_snapshot(None).unwrap(),expected);assert_eq!(db.read_snapshot(None).unwrap(),expected);
    let raw=Connection::open(&path).unwrap();
    // Four 10 MiB task titles plus a 13 MiB JSON string event. Each table fits
    // separately; the same returned-row budget must cover both.
    raw.execute_batch("DELETE FROM tasks; DELETE FROM events;").unwrap();
    for id in 0..4 {raw.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,'draft',?2)",params![format!("t{id}"),"x".repeat(10*1024*1024)]).unwrap();}
    raw.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('large','x',1,1,?1)",[serde_json::to_string(&"x".repeat(13*1024*1024)).unwrap()]).unwrap();
    assert!(matches!(db.read_snapshot(None),Err(StoreError::Limit(_))));
}
#[test]
fn core_snapshot_accounting_rejects_valid_dense_payload_and_view_amplification() {
    let(_root,path)=fixture();let raw=Connection::open(&path).unwrap();
    let payload=format!("[{}0]","0,".repeat(210_000));
    raw.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('dense','x',1,1,?1)",[payload]).unwrap();
    let mut db=ControlledStore::open(&path,control()).unwrap();
    assert!(matches!(db.read_snapshot(None),Err(StoreError::Limit(_))));
    assert_eq!(SqliteStore::open(&path).unwrap().read_snapshot(None).unwrap().events.len(),1);
    raw.execute_batch("DELETE FROM events; INSERT INTO tasks(id,revision,state,title) VALUES('task',1,'draft',''); ALTER TABLE tasks RENAME TO original_tasks; CREATE VIEW tasks AS SELECT original_tasks.id,revision,state,CAST(zeroblob(10000000) AS TEXT) AS title,active_attempt FROM original_tasks CROSS JOIN (SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6);").unwrap();
    // The returned view rows, not the single small base-table row, are charged.
    assert!(matches!(db.read_snapshot(None),Err(StoreError::Limit(_))));
}

#[test]
fn projection_materializers_charge_actual_values_before_copy_or_validation() {
    let cases=[
        ("runtime_bindings", "SELECT CAST(zeroblob(16777217) AS TEXT) AS id,NULL AS task_id,1 AS revision,NULL AS source_path,'{}' AS payload,'' AS payload_hash"),
        ("legacy_sources", "SELECT '.state/coordinator.json' AS path,'runtime' AS kind,'' AS digest,zeroblob(16777217) AS bytes"),
        ("operation_delivery", "SELECT 'operation' AS operation_id,1 AS revision,'pending' AS state,0 AS epoch,0 AS attempts,CAST(zeroblob(16777217) AS TEXT) AS owner,NULL AS lease_until_ms,0 AS next_due_ms,NULL AS last_outcome"),
        ("inbox_items", "SELECT 1 AS revision,'{}' AS payload,'' AS payload_hash,0 AS seen,0 AS done,CAST(zeroblob(16777217) AS TEXT) AS id"),
        ("runtime_observations", "SELECT CAST(zeroblob(16777217) AS TEXT) AS binding_id,1 AS binding_revision,NULL AS task_revision,0 AS observed_unix_ms,'{}' AS payload,'' AS payload_hash"),
        ("runtime_ownership", "SELECT CAST(zeroblob(16777217) AS TEXT) AS binding_id,1 AS revision,1 AS binding_revision,NULL AS attempt_id,'{}' AS payload,'' AS payload_hash"),
        ("project_control", "SELECT 1 AS singleton,1 AS revision,1 AS epoch,CAST(zeroblob(16777217) AS TEXT) AS state,0 AS reconciliation_required,NULL AS config_digest"),
        ("task_queue", "SELECT CAST(zeroblob(16777217) AS TEXT) AS task_id,0 AS priority,0 AS enqueued_unix_ms,1 AS enqueue_sequence"),
        ("approval_grants", "SELECT 'grant' AS id,CAST(zeroblob(16777217) AS TEXT) AS payload,'' AS payload_hash"),
        ("budget_policies", "SELECT 1 AS revision,CAST(zeroblob(16777217) AS TEXT) AS payload,'' AS payload_hash"),
        ("routine_revisions", "SELECT 'r' AS name,1 AS revision,CAST(zeroblob(16777217) AS TEXT) AS payload,'' AS payload_hash"),
    ];
    for (table,query) in cases {
        let(_root,path)=fixture();let mut db=ControlledStore::open(&path,control()).unwrap();
        let raw=Connection::open(&path).unwrap();
        raw.execute_batch(&format!("ALTER TABLE {table} RENAME TO original_{table}; CREATE VIEW {table} AS {query};")).unwrap();
        let result=db.read_snapshot(None);
        assert!(matches!(result,Err(StoreError::Limit(_))),"{table}: {result:?}");
    }
}
#[test]
fn nested_delivery_json_uses_shared_structure_budget_before_decode() {
    let(_root,path)=fixture();let mut db=ControlledStore::open(&path,control()).unwrap();let raw=Connection::open(&path).unwrap();
    raw.execute_batch("ALTER TABLE operation_delivery RENAME TO original_delivery; CREATE TABLE operation_delivery(operation_id,revision,state,epoch,attempts,owner,lease_until_ms,next_due_ms,last_outcome);").unwrap();
    // Two passes exceed the allowance; one pass would reach typed decoding and
    // reject this shape as Corrupt instead. No matching operation is needed to
    // reach this projection's ID + singleton query sequence.
    let payload=format!("[{}0]","0,".repeat(110_000));
    raw.execute("INSERT INTO operation_delivery VALUES('operation',1,'pending',0,0,NULL,NULL,0,?1)",[payload]).unwrap();
    assert!(matches!(db.read_snapshot(None),Err(StoreError::Limit(_))));
}
#[test]
fn runtime_provenance_join_amplification_consumes_one_snapshot_budget() {
    let(_root,path)=fixture();let raw=Connection::open(&path).unwrap();
    let bytes=vec![0u8;10*1024*1024];let digest=format!("{:x}",Sha256::digest(&bytes));
    let binding=RuntimeBinding{id:"coordinator".into(),task:None,revision:1,source_path:Some(".state/coordinator.json".into()),source_digest:Some(digest.clone()),session_source_digest:None,verification:RuntimeVerification::Unverified,identity:RuntimeIdentity::default()};
    let payload=serde_json::to_string(&binding).unwrap();
    raw.execute("INSERT INTO legacy_sources VALUES('.state/coordinator.json','runtime',?1,?2)",params![digest,bytes]).unwrap();
    raw.execute("INSERT INTO runtime_bindings VALUES('coordinator',NULL,1,'.state/coordinator.json',?1,?2)",params![payload,format!("{:x}",Sha256::digest(payload.as_bytes()))]).unwrap();
    let mut db=ControlledStore::open(&path,ReadControl::new(Instant::now()+Duration::from_secs(30),Cancellation::default())).unwrap();
    assert_eq!(db.read_snapshot(None).unwrap(),SqliteStore::open(&path).unwrap().read_snapshot(None).unwrap());
    raw.execute_batch("ALTER TABLE runtime_bindings RENAME TO original_bindings; CREATE VIEW runtime_bindings AS SELECT original_bindings.* FROM original_bindings CROSS JOIN (SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5);").unwrap();
    // Singleton source plus five returned join rows would copy 60 MiB from one
    // 10 MiB source. Reject before the final inventory mismatch/partial return.
    let amplified=db.read_snapshot(None);
    assert!(matches!(amplified,Err(StoreError::Limit(_))),"{amplified:?}");
}

fn launch_neighbors(raw:&Connection) {
    let hash=format!("{:x}",Sha256::digest(b"{}"));
    raw.execute_batch("INSERT INTO tasks(id,revision,state,title) VALUES('a',1,'draft',''); INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES('attempt-a','a',1,'reserved',NULL,'slot',0);").unwrap();
    raw.execute("INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key) VALUES('launch-a','a','runtime.launch','task:a',1,'{}',?1,4,0,'launch-a')",[&hash]).unwrap();
}

#[test]
fn remaining_snapshot_readers_charge_join_and_nested_json_before_copy() {
    // Not a heap bound: this only accounts admitted input/JSON structure.
    let(_root,path)=fixture();let mut db=ControlledStore::open(&path,control()).unwrap();
    let raw=Connection::open(&path).unwrap();
    launch_neighbors(&raw);
    raw.execute_batch("ALTER TABLE attempt_inputs RENAME TO original_attempt_inputs; CREATE VIEW attempt_inputs AS SELECT 'attempt-a' AS attempt_id,'launch-a' AS operation_id,CAST(zeroblob(16777217) AS TEXT) AS payload,'' AS payload_hash;").unwrap();
    let result=db.read_snapshot(None);
    assert!(matches!(result,Err(StoreError::Limit(_))),"attempt_inputs: {result:?}");

    for (table,ddl) in [
        ("attempt_inputs", "ALTER TABLE attempt_inputs RENAME TO original_attempt_inputs; CREATE TABLE attempt_inputs(attempt_id,operation_id,payload,payload_hash);"),
        ("approval_grants", "ALTER TABLE approval_grants RENAME TO original_approval_grants; CREATE TABLE approval_grants(id,payload,payload_hash);"),
    ] {
        let(_root,path)=fixture();let mut db=ControlledStore::open(&path,control()).unwrap();
        let raw=Connection::open(&path).unwrap();
        raw.execute_batch(ddl).unwrap();
        let payload=format!("[{}0]","0,".repeat(110_000));
        if table=="attempt_inputs" {
            launch_neighbors(&raw);
            raw.execute("INSERT INTO attempt_inputs VALUES('attempt-a','launch-a',?1,'')",[&payload]).unwrap();
        } else {
            raw.execute("INSERT INTO approval_grants VALUES('grant',?1,'')",[&payload]).unwrap();
        }
        assert!(matches!(db.read_snapshot(None),Err(StoreError::Limit(_))),"{table} dense json");
        assert!(matches!(SqliteStore::open(&path).unwrap().read_snapshot(None),Err(StoreError::Corrupt(_))),"{table} unbounded decode");
    }
}

#[test]
fn approval_read_does_not_double_count_already_budgeted_inputs() {
    // Not a heap bound. One 7 MiB payload join must remain readable; re-scanning
    // those inputs from approvals would exceed 50 MiB.
    let(_root,path)=fixture();
    let mut inputs:LaunchInputs=serde_json::from_str(include_str!("../../../tests/fixtures/launch-inputs-v1.json")).unwrap();
    let profile=crate::domain::profile::fixture(inputs.config.clone());
    inputs.version=2;inputs.project_store=format!("/{}", "x".repeat(7*1024*1024));
    inputs.effective_profile=Some(profile.clone());inputs.profile=profile.reference().unwrap();
    let digest=format!("{:x}",Sha256::digest(serde_json::to_vec(&inputs).unwrap()));
    let attempt=format!("attempt-{digest}");let operation=format!("launch-{digest}");
    let record=AttemptInputRecord{attempt:AttemptId::new(attempt.clone()).unwrap(),operation:OperationId::new(operation.clone()).unwrap(),inputs:inputs.clone()};
    let payload=serde_json::to_string(&record).unwrap();let hash=format!("{:x}",Sha256::digest(payload.as_bytes()));
    let raw=Connection::open(&path).unwrap();
    raw.execute("INSERT INTO tasks(id,revision,state,title) VALUES('a',1,'draft','')",[]).unwrap();
    raw.execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,'a',1,'reserved',NULL,'slot',0)",[&attempt]).unwrap();
    raw.execute("INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key) VALUES(?1,'a','runtime.launch','task:a',1,?2,?3,4,0,?1)",params![&operation,&payload,&hash]).unwrap();
    raw.execute("INSERT INTO attempt_inputs VALUES(?1,?2,?3,?4)",params![&attempt,&operation,&payload,&hash]).unwrap();
    drop(raw);
    let expected=SqliteStore::open(&path).unwrap().read_snapshot(None).unwrap();
    let mut db=ControlledStore::open(&path,control()).unwrap();
    assert_eq!(db.read_snapshot(None).unwrap(),expected);
}
