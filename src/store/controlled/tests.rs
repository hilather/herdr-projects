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
