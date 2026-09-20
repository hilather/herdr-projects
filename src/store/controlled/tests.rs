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
