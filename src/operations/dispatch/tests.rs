use super::*;
use crate::{domain::{Commit,Mutation,Task,TaskId,TaskState},operations::DeliveryState};
use std::{cell::Cell,rc::Rc,path::PathBuf};
fn fixture()->(tempfile::TempDir,SqliteStore,OperationId) {
    let temp=tempfile::tempdir().unwrap();let mut db=SqliteStore::create(&temp.path().join("state.db")).unwrap();
    let task=Task{id:TaskId::new("task").unwrap(),revision:1,state:TaskState::Ready,title:"fixture".into(),active_attempt:None};
    let id=OperationId::new("dispatch").unwrap();
    let op=Operation{id:id.clone(),task:task.id.clone(),kind:"fixture".into(),target:"local".into(),payload_version:1,payload:serde_json::json!({}),expected_revision:1,due_unix_ms:0,idempotency_key:"dispatch-fixture".into()};
    db.commit(Commit{expected_head:0,mutations:vec![Mutation::Task{expected:None,next:task},Mutation::Enqueue(op)]}).unwrap();
    (temp,db,id)
}
struct Adapter { path:PathBuf,calls:Rc<Cell<u32>>,mode:&'static str }
impl DeliveryAdapter for Adapter {
    type Prepared=Self;
    fn prepare(&mut self,_:&Operation)->Result<Self> {
        anyhow::ensure!(self.mode!="denied","policy denied");
        Ok(Self{path:self.path.clone(),calls:self.calls.clone(),mode:self.mode})
    }
}
impl PreparedDelivery for Adapter {
    fn revalidate(&mut self,_:&Operation)->Result<()> {
        if self.mode=="crash_before" {pause_child(&self.path);}
        if self.mode=="stale" {
            let mut db=SqliteStore::open(&self.path)?;let snap=db.read_snapshot(None)?;
            let mut task=snap.tasks[0].clone();task.revision+=1;
            db.commit(Commit{expected_head:snap.head,mutations:vec![Mutation::Task{expected:Some(1),next:task}]})?;
        }
        anyhow::ensure!(self.mode!="revoked","revoked policy");Ok(())
    }
    fn deliver(&mut self,_:&Operation,claim:&Claim)->Result<Outcome> {
        self.calls.set(self.calls.get()+1);
        // Independent write proves no store transaction surrounds external work.
        let mut db=SqliteStore::open(&self.path)?;
        assert_eq!(db.deliveries()?[0].state,DeliveryState::Claimed);
        db.validate_claim(claim,100)?;
        db.expire_claims(0)?;
        if self.mode.starts_with("crash_") {
            use std::io::Write;
            let mut file=std::fs::OpenOptions::new().write(true).create_new(true).open(self.path.with_file_name("effect"))?;
            file.write_all(b"single effect")?;file.sync_all()?;
            if self.mode=="crash_after" {pause_child(&self.path);}
        }
        if self.mode=="timeout" {anyhow::bail!("secret adapter error");}
        Ok(Outcome::Confirmed{observed_identity:"fixture-receipt".into()})
    }
}
fn run(db:&mut SqliteStore,id:&OperationId,adapter:&mut Adapter,late:bool)->Result<DispatchResult> {
    let mut tick=0;
    dispatch_one(db,DispatchRequest{operation:id,expected_revision:1,owner:"worker",lease_ms:1000},adapter,||{tick+=1;if late&&tick==3{1100}else{100}})
}
#[test]
fn service_claims_before_effect_records_receipt_and_refuses_replay() {
    let(temp,mut db,id)=fixture();let calls=Rc::new(Cell::new(0));let mut a=Adapter{path:temp.path().join("state.db"),calls:calls.clone(),mode:"ok"};
    assert!(matches!(run(&mut db,&id,&mut a,false).unwrap(),DispatchResult::Recorded(d) if d.state==DeliveryState::Confirmed));
    assert!(run(&mut db,&id,&mut a,false).is_err());assert_eq!(calls.get(),1);
}
#[test]
fn service_policy_denial_and_stale_binding_never_call_effect() {
    for mode in ["denied","revoked","stale"] {
        let(temp,mut db,id)=fixture();let calls=Rc::new(Cell::new(0));let mut a=Adapter{path:temp.path().join("state.db"),calls:calls.clone(),mode};
        let result=run(&mut db,&id,&mut a,false);
        if mode=="denied" {assert!(result.is_err());assert_eq!(db.deliveries().unwrap()[0].attempts,0);}
        else if mode=="revoked" {assert!(matches!(result.unwrap(),DispatchResult::Recorded(d) if d.state==DeliveryState::Pending));}
        else {assert!(matches!(result.unwrap(),DispatchResult::Unrecorded{..}));}
        assert_eq!(calls.get(),0);
    }
}
#[test]
fn adapter_errors_and_lost_receipt_remain_uncertain_across_restart() {
    for mode in ["timeout","late"] {
        let(temp,mut db,id)=fixture();let calls=Rc::new(Cell::new(0));let mut a=Adapter{path:temp.path().join("state.db"),calls:calls.clone(),mode};
        let result=run(&mut db,&id,&mut a,mode=="late").unwrap();
        if mode=="timeout" {assert!(matches!(result,DispatchResult::Recorded(ref d) if d.state==DeliveryState::Ambiguous));assert!(!format!("{result:?}").contains("secret adapter error"));}
        else {assert!(matches!(result,DispatchResult::Unrecorded{..}));}
        drop(db);let mut db=SqliteStore::open(&a.path).unwrap();db.expire_claims(1100).unwrap();
        assert_eq!(db.deliveries().unwrap()[0].state,DeliveryState::Ambiguous);
        assert!(run(&mut db,&id,&mut a,false).is_err());assert_eq!(calls.get(),1);
    }
}

#[test]
fn lease_expiring_during_prepare_recheck_prevents_effect() {
    let(temp,mut db,id)=fixture();let calls=Rc::new(Cell::new(0));let mut a=Adapter{path:temp.path().join("state.db"),calls:calls.clone(),mode:"ok"};
    let mut tick=0;
    let result=dispatch_one(&mut db,DispatchRequest{operation:&id,expected_revision:1,owner:"worker",lease_ms:10},&mut a,||{tick+=1;if tick==1{100}else{110}}).unwrap();
    assert!(matches!(result,DispatchResult::Unrecorded{..}));assert_eq!(calls.get(),0);
    db.expire_claims(110).unwrap();assert_eq!(db.deliveries().unwrap()[0].state,DeliveryState::Ambiguous);
}

fn pause_child(path:&std::path::Path)->! {
    std::fs::write(path.with_file_name("ready"),b"ready").unwrap();
    loop {std::thread::sleep(std::time::Duration::from_secs(1));}
}
#[test]
fn dispatch_crash_child() {
    let Some(root)=std::env::var_os("HP_DISPATCH_CRASH_ROOT") else{return;};
    let path=PathBuf::from(root).join("state.db");
    let mode=match std::env::var("HP_DISPATCH_CRASH_PHASE").unwrap().as_str(){"before"=>"crash_before","after"=>"crash_after",_=>"crash_committed"};
    let mut db=SqliteStore::open(&path).unwrap();let mut adapter=Adapter{path:path.clone(),mode,calls:Rc::new(Cell::new(0))};
    assert!(matches!(run(&mut db,&OperationId::new("dispatch").unwrap(),&mut adapter,false).unwrap(),DispatchResult::Recorded(_)));
    pause_child(&path);
}
#[test]
fn dispatch_process_death_retains_claim_or_receipt_without_replay() {
    use std::{process::{Command,Stdio},time::{Duration,Instant}};
    for phase in ["before","after","committed"] {
        let(temp,db,id)=fixture();drop(db);
        let mut child=Command::new(std::env::current_exe().unwrap()).args(["--exact","operations::dispatch::tests::dispatch_crash_child","--nocapture"])
            .env("HP_DISPATCH_CRASH_ROOT",temp.path()).env("HP_DISPATCH_CRASH_PHASE",phase).stdout(Stdio::null()).stderr(Stdio::inherit()).spawn().unwrap();
        let deadline=Instant::now()+Duration::from_secs(10);
        while !temp.path().join("ready").exists() {
            if Instant::now()>deadline||child.try_wait().unwrap().is_some(){let _=child.kill();let _=child.wait();panic!("dispatch child failed at {phase}");}
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().unwrap();child.wait().unwrap();
        let mut db=SqliteStore::open(&temp.path().join("state.db")).unwrap();db.expire_claims(1100).unwrap();
        assert_eq!(db.deliveries().unwrap()[0].state,if phase=="committed"{DeliveryState::Confirmed}else{DeliveryState::Ambiguous});
        let calls=Rc::new(Cell::new(0));let mut a=Adapter{path:temp.path().join("state.db"),mode:"ok",calls:calls.clone()};
        assert!(run(&mut db,&id,&mut a,false).is_err());assert_eq!(calls.get(),0);
        assert_eq!(temp.path().join("effect").exists(),phase!="before");
        if phase!="before" {assert_eq!(std::fs::read(temp.path().join("effect")).unwrap(),b"single effect");}
    }
}
