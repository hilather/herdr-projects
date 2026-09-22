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

#[cfg(target_os="linux")]
#[test]
fn stop_snapshot_finalizes_without_source_in_foreground_and_queued_paths() {
    use herdr_projects::domain::*;
    // Seed historical canonical evidence directly. Native creation/stop provenance
    // is covered by canonical_worker tests; this fixture tests both consumers.
    for mode in ["foreground","queued","corrupt","absent"] {
        let(world,path,old)=fixture();let mut state=runtime::snapshot(&path).unwrap();
        let binding=state.runtime_bindings.iter_mut().find(|b|b.id==old.target).unwrap();
        let mut inputs:LaunchInputs=serde_json::from_str(include_str!("../tests/fixtures/launch-inputs-v1.json")).unwrap();
        inputs.project_store=path.join(".state/state.db").to_str().unwrap().into();inputs.task=old.task.clone().unwrap();inputs.binding=binding.id.clone();inputs.binding_revision=binding.revision;
        let id=digest(&serde_json::to_vec(&inputs).unwrap());let attempt=AttemptId::new(format!("attempt-{id}")).unwrap();let launch=OperationId::new(format!("launch-{id}")).unwrap();
        let source=worker_output_path(&inputs,&attempt).unwrap();binding.identity.thread_dir=source.clone();
        let record=AttemptInputRecord{attempt:attempt.clone(),operation:launch.clone(),inputs};let body=serde_json::to_string(&record).unwrap();
        let raw=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();
        // The v2 ingress trigger excludes new v1 reservations; retain that rule
        // while installing this read-compatibility fixture as historical data.
        let trigger:String=raw.query_row("SELECT sql FROM sqlite_master WHERE name='attempt_inputs_effective_profile'",[],|r|r.get(0)).unwrap();
        raw.execute_batch("DROP TRIGGER attempt_inputs_effective_profile;").unwrap();
        raw.execute("INSERT INTO attempts VALUES(?1,?2,1,'cancelled',NULL,?1,1)",rusqlite::params![attempt.as_str(),old.task.as_ref().unwrap().as_str()]).unwrap();
        raw.execute("INSERT INTO operations VALUES(?1,?2,'runtime.launch',?3,1,?4,?5,?6,0,?1)",rusqlite::params![launch.as_str(),old.task.as_ref().unwrap().as_str(),old.target,body,digest(body.as_bytes()),record.inputs.task_revision+1]).unwrap();
        raw.execute("INSERT INTO attempt_inputs VALUES(?1,?2,?3,?4)",rusqlite::params![attempt.as_str(),launch.as_str(),body,digest(body.as_bytes())]).unwrap();raw.execute_batch(&trigger).unwrap();
        let payload=serde_json::to_string(binding).unwrap();raw.execute("UPDATE runtime_bindings SET payload=?2,payload_hash=?3 WHERE id=?1",rusqlite::params![binding.id,payload,digest(payload.as_bytes())]).unwrap();
        let report=digest(b"preserved report");let artifact=digest(&[0,255,3]);
        let manifest=herdr_projects::worktree_preservation::OutputManifest{version:1,attempt:attempt.clone(),source:source.clone(),entries:vec![
            herdr_projects::worktree_preservation::Entry{symlink:false,path:"library".into(),directory:true,executable:false,bytes:0,sha256:String::new()},
            herdr_projects::worktree_preservation::Entry{symlink:false,path:"library/binary".into(),directory:false,executable:true,bytes:3,sha256:artifact.clone()},
            herdr_projects::worktree_preservation::Entry{symlink:false,path:"report.md".into(),directory:false,executable:false,bytes:16,sha256:report.clone()},
        ]};
        let bytes=serde_json::to_vec(&manifest).unwrap();let manifest_hash=digest(&bytes);let directory=path.join(".state/worker-output-snapshots").join(attempt.as_str()).join(&manifest_hash);
        fs::create_dir_all(&directory).unwrap();fs::write(directory.join("manifest.json"),bytes).unwrap();fs::write(directory.join(&report),b"preserved report").unwrap();fs::write(directory.join(&artifact),[0,255,3]).unwrap();
        let process=herdr_projects::worker_supervision::ProcessIncarnation{pid:1,device:1,inode:1};
        let stop=WorkerTerminationReceipt{version:1,attempt:attempt.clone(),launch,binding:binding.id.clone(),binding_revision:binding.revision,ownership_revision:1,
            supervisor:herdr_projects::worker_supervision::SupervisorIdentity{version:1,boot_id:"00000000-0000-0000-0000-000000000001".into(),host_id:None,observer_namespace:(1,1),worker_namespace:(1,2),outer:process.clone(),init:process},
            host_reboot:None,repository_snapshots:vec![],output_snapshot:Some(AttemptOutputReference{source:source.clone(),digest:if mode=="absent"{None}else{Some(manifest_hash)}}),retained_resources:binding.identity.clone(),cause:WorkerTerminationCause::Cancellation,observed_unix_ms:0};
        raw.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worker_terminated',?1,1,1,?2)",rusqlite::params![attempt.as_str(),serde_json::to_string(&stop).unwrap()]).unwrap();
        if mode=="queued" {raw.execute("UPDATE tasks SET state='cancelled' WHERE id=?1",[old.task.as_ref().unwrap().as_str()]).unwrap();}
        let state=runtime::snapshot(&path).unwrap();assert!(!Path::new(&source).exists());
        if mode=="absent" {assert!(receipt::enqueue(&world.ctx(),&path,&old.target,state.head,"recover".into()).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),state);continue;}
        let op=receipt::enqueue(&world.ctx(),&path,&old.target,state.head,"recover preserved outputs".into()).unwrap();
        if mode=="corrupt" {fs::create_dir_all(&source).unwrap();fs::write(Path::new(&source).join("report.md"),b"preserved report").unwrap();fs::write(directory.join(&artifact),b"bad").unwrap();let before=runtime::snapshot(&path).unwrap();let request=input(&world.ctx(),&path,&op,1,Mode::Deliver);assert!(execute(&request,&control()).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);continue;}
        if mode=="foreground" {receipt::deliver(&world.ctx(),&path,&op.id,1).unwrap();}else{let request=input(&world.ctx(),&path,&op,1,Mode::Deliver);execute(&request,&control()).unwrap();}
        let after=runtime::snapshot(&path).unwrap();assert!(after.deliveries.iter().any(|d|d.operation==op.id&&d.state==DeliveryState::Confirmed));
        if mode=="queued" {assert_eq!(after.tasks,state.tasks);}
        let payload=Finalization::decode(&op).unwrap();let published=receipt::load_receipt_controlled(&receipt::project(&path).unwrap(),&op,&payload,&control()).unwrap().unwrap();
        let captured=path.join(".state/canonical-artifacts").join(published.artifact_key).join(published.snapshot);
        assert_eq!(fs::read(captured.join("library/binary")).unwrap(),[0,255,3]);assert_eq!(fs::read(captured.join("report.md")).unwrap(),b"preserved report");assert!(!Path::new(&source).exists());
    }
}
