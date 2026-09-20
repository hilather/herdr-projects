use super::*;
use std::fs;
use crate::finalization_delivery::tests::fixture;
use herdr_projects::domain::TaskState;
fn input(ctx:&Ctx,path:&Path,op:&Operation,revision:u64,mode:Mode)->Input {serde_json::from_str(request(ctx,path,op,revision,mode).unwrap().command.stdin.as_ref().unwrap()).unwrap()}
fn control()->Control {Control::default()}

#[test]
fn canonical_finalization_controller_queues_capture_and_worker_commits_once() {
    let(world,path,op)=fixture();let before=runtime::snapshot(&path).unwrap();let ctx=world.ctx();let request=input(&ctx,&path,&op,1,Mode::Deliver);
    let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(JobRunner{inner:Arc::new(crate::runner::RealRunner)})).unwrap());
    let mut queue=crate::copy_jobs::Queue::new(pool.clone());let mut reads=crate::canonical_controller::observations::Reads::new(pool.clone());
    let result=crate::canonical_controller::poll_queued_effects(&ctx,&path,0,&mut reads,Some(&mut queue)).unwrap();assert!(result.operation_error.is_none(),"{:?}",result.operation_error);
    assert!(queue.offered());assert_eq!(runtime::snapshot(&path).unwrap().deliveries,before.deliveries);assert!(!path.join(".state/canonical-artifacts").exists());
    assert!(queue.admit().is_empty());let deadline=Instant::now()+Duration::from_secs(5);
    while queue.pending(){assert!(queue.drain().is_empty());assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
    let after=runtime::snapshot(&path).unwrap();assert_eq!(after.deliveries[0].state,DeliveryState::Confirmed);assert_eq!(after.deliveries[0].attempts,1);
    assert_eq!(after.tasks.iter().find(|t|Some(&t.id)==op.task.as_ref()).unwrap().state,TaskState::AwaitingReview);assert_eq!(after.runtime_bindings,before.runtime_bindings);assert_eq!(after.attempts,before.attempts);
    assert!(execute(&request,&control()).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),after);assert!(pool.stop(Duration::from_secs(2)));
}

#[test]
fn canonical_finalization_stale_or_cancelled_ingress_never_claims_or_captures() {
    for kind in ["configuration","operation","revision","identity","cancelled","expired"] {
        let(world,path,op)=fixture();let mut input=input(&world.ctx(),&path,&op,1,Mode::Deliver);let mut control=control();
        match kind {
            "configuration"=>{fs::create_dir_all(&world.ctx().config_dir).unwrap();fs::write(world.ctx().config_dir.join("config.toml"),"# changed").unwrap();},
            "operation"=>input.operation_digest="0".repeat(64),"revision"=>input.revision=99,"identity"=>input.identity.1+=1,
            "cancelled"=>control.cancellation.cancel(),_=>control.deadline=Instant::now(),
        }
        let before=runtime::snapshot(&path).unwrap();assert!(execute(&input,&control).is_err(),"{kind}");assert_eq!(runtime::snapshot(&path).unwrap(),before);assert!(!path.join(".state/canonical-artifacts").exists());
    }
}

#[test]
fn canonical_finalization_source_replacement_after_prepare_cannot_publish() {
    let(world,path,op)=fixture();let input=input(&world.ctx(),&path,&op,1,Mode::Deliver);let ctx=world.ctx();let control=control();let guard=ProjectGuard::acquire(&path).unwrap();let _locks=guard.inherit_transfer().unwrap();
    let mut adapter=Adapter{input:&input,ctx:&ctx,control:&control,guard:&guard};let mut prepared=adapter.prepare(&op).unwrap();let mut db=migration::open_active(&path).unwrap();let claim=db.claim_operation(&op.id,1,"fixture",jiff::Timestamp::now().as_millisecond(),300_000).unwrap();
    let source=Path::new(&prepared.payload.source);let old=world.home.path().join("old-source");fs::rename(source,&old).unwrap();fs::create_dir_all(source.join("library")).unwrap();
    for name in ["report.md","library/artifact"] {fs::copy(old.join(name),source.join(name)).unwrap();}
    assert!(prepared.deliver(&op,&claim).is_err());assert!(!path.join(".state/canonical-artifacts").exists());assert_eq!(db.read_snapshot(None).unwrap().deliveries[0].state,DeliveryState::Claimed);
}

#[test]
fn canonical_finalization_queue_success_cannot_certify_preservation() {
    struct Forged;impl Runner for Forged {fn run(&self,_:&Cmd)->Result<Output>{Ok(Output{code:Some(0),..Output::default()})}fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}}
    let(world,path,op)=fixture();let before=runtime::snapshot(&path).unwrap();let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(Forged)).unwrap());let mut queue=crate::copy_jobs::Queue::new(pool.clone());
    queue.offer_canonical_finalization(&world.ctx(),&path,&op,1,Mode::Deliver).unwrap();assert!(queue.admit().is_empty());let deadline=Instant::now()+Duration::from_secs(3);
    while queue.pending(){assert!(queue.drain().is_empty());assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
    assert_eq!(runtime::snapshot(&path).unwrap(),before);assert!(!path.join(".state/canonical-artifacts").exists());assert!(pool.stop(Duration::from_secs(2)));
}

#[test]
fn canonical_finalization_crash_child() {
    let Some(home)=std::env::var_os("HP_CANONICAL_FINALIZATION_CRASH")else{return;};let home=PathBuf::from(home);let input:Input=serde_json::from_slice(&fs::read(home.join("input.json")).unwrap()).unwrap();
    let control=control();let guard=ProjectGuard::acquire(&input.project).unwrap();let _locks=guard.inherit_transfer().unwrap();let env=Env::for_observation(&input.home,&input.bin);let ctx=Ctx{env:&env,root:input.project.parent().unwrap().into(),config_dir:input.config.clone(),runner:&crate::runner::RealRunner,detached_ticker:false};
    let mut adapter=Adapter{input:&input,ctx:&ctx,control:&control,guard:&guard};let mut db=migration::open_active(&input.project).unwrap();let op=db.read_snapshot(None).unwrap().operations.into_iter().find(|o|o.id==input.operation).unwrap();let mut prepared=adapter.prepare(&op).unwrap();let claim=db.claim_operation(&op.id,1,"ticker.finalization",jiff::Timestamp::now().as_millisecond(),300_000).unwrap();
    let phase=std::env::var("HP_CANONICAL_FINALIZATION_PHASE").unwrap();
    if phase!="before" {let outcome=prepared.deliver(&op,&claim).unwrap();if phase=="committed"{db.finish_operation(&claim,outcome,jiff::Timestamp::now().as_millisecond()).unwrap();}}
    fs::write(home.join("ready"),b"ready").unwrap();loop{std::thread::sleep(Duration::from_secs(1));}
}

#[test]
fn canonical_finalization_owner_death_recovers_only_verified_receipts_without_source() {
    use std::process::{Command,Stdio};
    for phase in ["before","after","committed"] {
        let(world,path,op)=fixture();fs::write(world.home.path().join("input.json"),serde_json::to_vec(&input(&world.ctx(),&path,&op,1,Mode::Deliver)).unwrap()).unwrap();
        let mut child=Command::new(std::env::current_exe().unwrap()).args(["--exact","canonical_finalization_jobs::tests::canonical_finalization_crash_child","--nocapture"]).env("HP_CANONICAL_FINALIZATION_CRASH",world.home.path()).env("HP_CANONICAL_FINALIZATION_PHASE",phase).stdout(Stdio::null()).stderr(Stdio::inherit()).spawn().unwrap();let deadline=Instant::now()+Duration::from_secs(10);
        while !world.home.path().join("ready").exists(){if child.try_wait().unwrap().is_some(){panic!("crash child exited");}if Instant::now()>deadline{let _=child.kill();let _=child.wait();panic!("crash child timed out");}std::thread::sleep(Duration::from_millis(10));}
        assert!(ProjectGuard::acquire(&path).is_err());assert!(herdr_projects::execution_guard::RootGuard::exclusive(&world.root).is_err());
        child.kill().unwrap();child.wait().unwrap();assert!(ProjectGuard::acquire(&path).is_ok());
        let mut db=migration::open_active(&path).unwrap();let delivery=db.read_snapshot(None).unwrap().deliveries[0].clone();
        if phase=="committed"{assert_eq!(delivery.state,DeliveryState::Confirmed);assert!(execute(&input(&world.ctx(),&path,&op,1,Mode::Deliver),&control()).is_err());continue;}
        assert_eq!(delivery.state,DeliveryState::Claimed);db.expire_claims(delivery.lease_until_ms.unwrap()+1).unwrap();let before=db.read_snapshot(None).unwrap();
        let source=Finalization::decode(&op).unwrap().source;fs::remove_dir_all(source).unwrap();let result=execute(&input(&world.ctx(),&path,&op,3,Mode::Observe),&control());
        if phase=="before"{assert!(result.is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);}else{result.unwrap();let after=runtime::snapshot(&path).unwrap();assert_eq!(after.deliveries[0].state,DeliveryState::Confirmed);assert_eq!(after.deliveries[0].attempts,1);assert_eq!(after.tasks.iter().find(|t|Some(&t.id)==op.task.as_ref()).unwrap().state,TaskState::AwaitingReview);}
    }
}
