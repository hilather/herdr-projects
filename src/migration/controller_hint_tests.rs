use super::*;
use std::time::{Duration,Instant};
use crate::store::controller_hint::EffectMode;
fn budget()->Budget {Budget::new(2*1024*1024,1024,Instant::now()+Duration::from_secs(2),Default::default()).unwrap()}
fn fixture(count:usize)->(tempfile::TempDir,std::path::PathBuf) {
    let(root,path)=super::tests::fixture();let db=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();
    db.execute("INSERT INTO tasks(id,revision,state,title) VALUES('task',1,'draft','task')",[]).unwrap();
    db.execute_batch("BEGIN").unwrap();
    for n in 0..count {let id=format!("effect-{n:04}");db.execute("INSERT INTO operations VALUES(?1,'task','runtime.finalization','binding',1,'{}',?2,1,0,?1)",rusqlite::params![id,hash(b"{}")]).unwrap();}
    db.execute_batch("COMMIT").unwrap();(root,path)
}
#[test]
fn effect_hints_skip_unrelated_history_and_rotate_the_complete_eligible_set() {
    let(_root,path)=fixture(4);let db=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();
    db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('unrelated','x',1,1,?1)",[serde_json::to_string(&"x".repeat(20*1024*1024)).unwrap()]).unwrap();
    // Oversized unrelated intent/outcome data also stays outside the hint reader.
    db.execute("INSERT INTO operations VALUES('unrelated','task','legacy.import','binding',1,?1,?2,1,0,'unrelated')",rusqlite::params![serde_json::to_string(&"x".repeat(3*1024*1024)).unwrap(),"0".repeat(64)]).unwrap();
    db.execute("UPDATE operation_delivery SET last_outcome=?1 WHERE operation_id='effect-0000'",[serde_json::to_string(&"x".repeat(3*1024*1024)).unwrap()]).unwrap();
    let expected_head:u64=db.query_row("SELECT max(sequence) FROM events",[],|r|r.get(0)).unwrap();
    for turn in 0..8 {let mut budget=budget();let hint=read_controller_effect_hint(&path,&mut budget,turn,100).unwrap().unwrap();assert_eq!(hint.operation.id.as_str(),format!("effect-{:04}",turn%4));assert_eq!(hint.delivery_revision,1);assert_eq!(hint.mode,EffectMode::Deliver);assert_eq!(hint.head,expected_head);assert!(hint.notification_socket.is_none());assert!(budget.used()<100_000);}
    db.execute("UPDATE operation_delivery SET next_due_ms=1000 WHERE operation_id='effect-0000'",[]).unwrap();
    db.execute("UPDATE operation_delivery SET state='ambiguous' WHERE operation_id='effect-0001'",[]).unwrap();
    db.execute("UPDATE operation_delivery SET state='confirmed' WHERE operation_id='effect-0002'",[]).unwrap();
    let hint=read_controller_effect_hint(&path,&mut budget(),0,100).unwrap().unwrap();assert_eq!(hint.operation.id.as_str(),"effect-0001");assert_eq!(hint.mode,EffectMode::Observe);
    assert_eq!(read_controller_effect_hint(&path,&mut budget(),1,100).unwrap().unwrap().operation.id.as_str(),"effect-0003");
}
#[test]
fn effect_hints_refuse_overflow_without_starving_late_candidates() {
    let(_root,path)=fixture(1024);let hint=read_controller_effect_hint(&path,&mut budget(),1023,100).unwrap().unwrap();assert_eq!(hint.operation.id.as_str(),"effect-1023");
    let mut small=Budget::new(2*1024*1024,3,Instant::now()+Duration::from_secs(2),Default::default()).unwrap();assert!(read_controller_effect_hint(&path,&mut small,0,100).unwrap_err().to_string().contains("candidate budget"));
    let db=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();db.execute("INSERT INTO operations VALUES('overflow','task','runtime.finalization','binding',1,'{}',?1,1,0,'overflow')",[hash(b"{}")]).unwrap();
    assert!(read_controller_effect_hint(&path,&mut budget(),1024,100).unwrap_err().to_string().contains("candidate budget"));
}
#[test]
fn effect_hints_measure_selected_fields_and_refuse_corruption() {
    for change in ["payload","key","candidate","hash","dangling"] {
        let(_root,path)=fixture(1);let db=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();
        match change {
            "payload"=>{let value=serde_json::to_string(&"x".repeat(1024*1024)).unwrap();db.execute("UPDATE operations SET payload=?1,payload_hash=?2",rusqlite::params![value,hash(value.as_bytes())]).unwrap();},
            "key"=>{db.execute("UPDATE operations SET idempotency_key=?1",["x".repeat(1024*1024+1)]).unwrap();},
            "candidate"=>{db.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();let id="x".repeat(513);db.execute("UPDATE operations SET id=?1",[&id]).unwrap();db.execute("UPDATE operation_delivery SET operation_id=?1",[&id]).unwrap();},
            "hash"=>{db.execute("UPDATE operations SET payload_hash=?1",["0".repeat(64)]).unwrap();},
            _=>{db.execute_batch("PRAGMA foreign_keys=OFF; DELETE FROM operations;").unwrap();},
        }
        let mut budget=budget();let error=read_controller_effect_hint(&path,&mut budget,0,100).unwrap_err().to_string();
        assert!(error.contains(match change {"hash"=>"hash mismatch","dangling"=>"dangling",_=>"field exceeds bounds"}),"{change}: {error}");
        assert!(budget.used()<100_000,"oversized fields must not be materialized or charged as accepted bytes");
    }
}
#[test]
fn effect_hints_preserve_publication_cancellation_and_sql_deadline_checks() {
    let(_root,path)=fixture(1);let mut cancelled=budget();cancelled.cancellation.cancel();assert!(read_controller_effect_hint(&path,&mut cancelled,0,100).is_err());
    let mut expired=budget();expired.deadline=Instant::now();assert!(read_controller_effect_hint(&path,&mut expired,0,100).is_err());
    let marker=path.join(".state/format.json");let bytes=fs::read(&marker).unwrap();let mut value:serde_json::Value=serde_json::from_slice(&bytes).unwrap();value["migration"]="0".repeat(64).into();fs::write(&marker,serde_json::to_vec(&value).unwrap()).unwrap();assert!(read_controller_effect_hint(&path,&mut budget(),0,100).is_err());fs::write(marker,bytes).unwrap();
    let db=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();
    db.execute_batch("ALTER TABLE operations RENAME TO original_operations; CREATE VIEW operations AS SELECT * FROM original_operations WHERE (WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n)>0;").unwrap();
    let mut limited=Budget::new(2*1024*1024,1024,Instant::now()+Duration::from_millis(100),Default::default()).unwrap();let start=Instant::now();let error=read_controller_effect_hint(&path,&mut limited,0,100).unwrap_err();assert!(start.elapsed()<Duration::from_secs(2),"{error:#}");
}
#[test]
fn effect_notification_route_is_only_a_bounded_hash_checked_hint() {
    let(_root,path)=fixture(1);let db=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();
    let notice=crate::operations::notification::Notification{authority:"operator.session_notification".into(),binding_revision:1,control_epoch:1,config:ConfigReference{path:"/tmp/config.toml".into(),digest:None},inbox_ids:vec!["a".into()],title:"herdr-projects: fixture".into(),body:"1 new inbox item(s). The coordinator reads them at its next turn.".into()};let payload=serde_json::to_string(&notice).unwrap();
    db.execute("UPDATE operations SET kind='runtime.notification',target='coordinator',payload=?1,payload_hash=?2",rusqlite::params![payload,hash(payload.as_bytes())]).unwrap();
    let raw:String=db.query_row("SELECT payload FROM runtime_bindings WHERE id='coordinator'",[],|r|r.get(0)).unwrap();let mut binding:RuntimeBinding=serde_json::from_str(&raw).unwrap();binding.identity.socket="/tmp/fixture.sock".into();
    let payload=serde_json::to_string(&binding).unwrap();db.execute("UPDATE runtime_bindings SET payload=?1,payload_hash=?2 WHERE id='coordinator'",rusqlite::params![payload,hash(payload.as_bytes())]).unwrap();
    // Lifecycle remains paused, and this deliberately non-authoritative operation
    // has not passed Notification::validate. A hint must never be used to claim.
    let hint=read_controller_effect_hint(&path,&mut budget(),0,100).unwrap().unwrap();assert_eq!(hint.notification_socket.as_deref(),Some("/tmp/fixture.sock"));
    db.execute("UPDATE runtime_bindings SET revision=2 WHERE id='coordinator'",[]).unwrap();assert!(read_controller_effect_hint(&path,&mut budget(),0,100).unwrap_err().to_string().contains("row mismatch"));
    db.execute("UPDATE runtime_bindings SET revision=1,payload_hash=?1 WHERE id='coordinator'",["0".repeat(64)]).unwrap();assert!(read_controller_effect_hint(&path,&mut budget(),0,100).unwrap_err().to_string().contains("hash mismatch"));
    db.execute("UPDATE runtime_bindings SET payload=?1 WHERE id='coordinator'",[serde_json::to_string(&"x".repeat(64*1024)).unwrap()]).unwrap();assert!(read_controller_effect_hint(&path,&mut budget(),0,100).unwrap_err().to_string().contains("field exceeds bounds"));
}

#[test]
fn effect_hint_busy_database_respects_short_read_budget() {
    let(_root,path)=fixture(1);let db=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;").unwrap();
    let mut limited=Budget::new(2*1024*1024,1024,Instant::now()+Duration::from_millis(100),Default::default()).unwrap();let start=Instant::now();
    assert!(read_controller_effect_hint(&path,&mut limited,0,100).is_err());assert!(start.elapsed()<Duration::from_secs(1));
    db.execute_batch("ROLLBACK").unwrap();assert!(read_controller_effect_hint(&path,&mut budget(),0,100).unwrap().is_some());
}

#[test]
fn effect_hint_duplicate_id_view_is_refused_without_collecting_payload_copies() {
    let(_root,path)=fixture(1);let db=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();
    let payload=serde_json::to_string(&"x".repeat(256*1024)).unwrap();db.execute("UPDATE operations SET payload=?1,payload_hash=?2",rusqlite::params![payload,hash(payload.as_bytes())]).unwrap();
    db.execute_batch("ALTER TABLE operations RENAME TO original_operations; CREATE VIEW operations AS WITH RECURSIVE copies(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM copies WHERE n<128) SELECT o.* FROM original_operations o CROSS JOIN copies;").unwrap();
    let mut budget=budget();let error=read_controller_effect_hint(&path,&mut budget,0,100).unwrap_err().to_string();assert!(error.contains("duplicate selected"),"{error}");assert!(budget.used()<512*1024);
}
