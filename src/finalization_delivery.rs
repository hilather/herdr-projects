//! Bounded immutable artifact capture with a durable operation-bound receipt.
use std::{fs::{self,File,OpenOptions},io::Write,os::unix::fs::{OpenOptionsExt,DirBuilderExt},path::{Path,PathBuf}};
use anyhow::{Context,Result,ensure};
use crate::{artifacts,cleanup,paths::Ctx,project::Project,thread::Thread};
use herdr_projects::{domain::{Operation,OperationId},migration,operations::{Claim,Outcome,Delivery,dispatch::{self,DeliveryAdapter,PreparedDelivery,DispatchRequest,DispatchResult},finalization::{Finalization,FinalizationReceipt,digest}},runtime};

fn record(payload:&Finalization)->Thread {Thread{id:payload.artifact_key(),thread_dir:payload.source.clone(),lifecycle_generation:payload.binding_revision,..Default::default()}}
fn project(path:&Path)->Result<Project> {Ok(Project{root:path.parent().context("project has no root")?.into(),slug:path.file_name().and_then(|s|s.to_str()).context("invalid project slug")?.into()})}
fn receipt_path(project:&Project,op:&Operation)->PathBuf {project.state_dir().join("finalization-receipts").join(format!("{}.json",op.id.as_str()))}
fn load_receipt(project:&Project,op:&Operation,payload:&Finalization)->Result<Option<FinalizationReceipt>> {
    let path=receipt_path(project,op);
    match fs::symlink_metadata(path.parent().unwrap()) {Ok(m)=>ensure!(m.is_dir(),"receipt parent must be a real directory"),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(None),Err(e)=>return Err(e.into())}
    match fs::symlink_metadata(&path) {Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(None),Err(e)=>return Err(e.into()),Ok(_)=>{}}
    let receipt:FinalizationReceipt=serde_json::from_slice(&migration::read_plan_file(&path)?)?;
    receipt.validate(op,payload)?;
    let manifest=artifacts::load_canonical(project,&record(payload),&receipt.snapshot)?;
    ensure!(manifest.matches_canonical_execution(&record(payload)),"retained manifest execution identity mismatch");
    ensure!(manifest.report_hash()==Some(payload.report_hash.as_str()),"retained report does not match finalization intent");
    Ok(Some(receipt))
}
fn save_receipt(project:&Project,op:&Operation,receipt:&FinalizationReceipt)->Result<()> {
    let path=receipt_path(project,op);let parent=path.parent().unwrap();
    match fs::DirBuilder::new().mode(0o700).create(parent) {Ok(())=>{},Err(e) if e.kind()==std::io::ErrorKind::AlreadyExists=>{},Err(e)=>return Err(e.into())}
    ensure!(fs::symlink_metadata(parent)?.is_dir(),"receipt directory must be real");
    // The root execution lease excludes other supported publishers. Never replace
    // any existing receipt; malformed evidence is preserved for operator repair.
    ensure!(matches!(fs::symlink_metadata(&path),Err(e) if e.kind()==std::io::ErrorKind::NotFound),"finalization receipt already exists or is unreadable");
    let temp=parent.join(format!(".{}-{}.next",op.id.as_str(),std::process::id()));
    let mut file=OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&temp)?;
    file.write_all(&serde_json::to_vec(receipt)?)?;file.sync_all()?;
    fs::hard_link(&temp,&path)?;fs::remove_file(&temp)?;File::open(parent)?.sync_all()?;File::open(project.state_dir())?.sync_all()?;Ok(())
}

pub fn enqueue(ctx:&Ctx,path:&Path,binding:&str,head:u64,reason:String)->Result<Operation> {
    let path=path.canonicalize()?;let snapshot=runtime::snapshot(&path)?;ensure!(snapshot.head==head,"project head changed");
    let binding=snapshot.runtime_bindings.iter().find(|b|b.id==binding).context("runtime binding not found")?;
    ensure!(binding.identity.machine.is_empty()&&Path::new(&binding.identity.thread_dir).is_absolute(),"finalization requires a recorded local source");
    ensure!(fs::canonicalize(&binding.identity.thread_dir)?==Path::new(&binding.identity.thread_dir),"artifact source must be its canonical recorded path");
    let bytes=migration::read_plan_file(&Path::new(&binding.identity.thread_dir).join("report.md"))?;
    let payload=Finalization{authority:"operator.artifact_finalization".into(),binding:binding.id.clone(),binding_revision:binding.revision,control_epoch:snapshot.control.as_ref().context("upgrade-store required")?.epoch,config:crate::notification_delivery::config(ctx,&path)?,source:binding.identity.thread_dir.clone(),report_hash:digest(&bytes),reason};
    let op=payload.operation(&snapshot,jiff::Timestamp::now().as_millisecond())?;
    runtime::enqueue_finalization(&path,head,op)
}
struct Adapter<'a,'b> {ctx:&'a Ctx<'b>,path:PathBuf}
struct Prepared<'a,'b> {ctx:&'a Ctx<'b>,path:PathBuf,project:Project,payload:Finalization,_lease:cleanup::Lease}
impl<'a,'b> DeliveryAdapter for Adapter<'a,'b> {
    type Prepared=Prepared<'a,'b>;
    fn prepare(&mut self,op:&Operation)->Result<Self::Prepared> {
        let lease=cleanup::lease(self.path.parent().context("project has no root")?)?;
        let payload=Finalization::decode(op)?;
        payload.validate(op,&runtime::snapshot(&self.path)?,&crate::notification_delivery::config(self.ctx,&self.path)?)?;
        Ok(Prepared{ctx:self.ctx,path:self.path.clone(),project:project(&self.path)?,payload,_lease:lease})
    }
}
impl PreparedDelivery for Prepared<'_,'_> {
    fn revalidate(&mut self,op:&Operation)->Result<()> {self.payload.validate(op,&runtime::snapshot(&self.path)?,&crate::notification_delivery::config(self.ctx,&self.path)?)?;Ok(())}
    fn deliver(&mut self,op:&Operation,_claim:&Claim)->Result<Outcome> {
        let receipt=if let Some(receipt)=load_receipt(&self.project,op,&self.payload)? {receipt}else{
            ensure!(fs::canonicalize(&self.payload.source)?==Path::new(&self.payload.source),"artifact source identity changed before capture");
            let snapshot=artifacts::capture_canonical(&self.project,&record(&self.payload))?;
            ensure!(snapshot.manifest.matches_canonical_execution(&record(&self.payload)),"captured manifest execution identity mismatch");
            ensure!(snapshot.manifest.report_hash()==Some(self.payload.report_hash.as_str()),"report changed after finalization was requested; snapshot retained but no completion receipt written");
            let receipt=FinalizationReceipt{operation_hash:digest(&serde_json::to_vec(op)?),artifact_key:self.payload.artifact_key(),snapshot:snapshot.id,report_hash:self.payload.report_hash.clone(),binding_revision:self.payload.binding_revision};
            save_receipt(&self.project,op,&receipt)?;receipt
        };
        Ok(Outcome::Confirmed{observed_identity:serde_json::to_string(&receipt)?})
    }
}
pub fn deliver(ctx:&Ctx,path:&Path,id:&OperationId,revision:u64)->Result<DispatchResult> {
    let path=path.canonicalize()?;let mut db=migration::open_active(&path)?;let mut adapter=Adapter{ctx,path};
    dispatch::dispatch_one(&mut db,DispatchRequest{operation:id,expected_revision:revision,owner:"operator.finalization",lease_ms:300_000},&mut adapter,||jiff::Timestamp::now().as_millisecond())
}
pub fn observe(ctx:&Ctx,path:&Path,id:&OperationId,revision:u64,head:u64)->Result<Delivery> {
    let path=path.canonicalize()?;let _lease=cleanup::lease(path.parent().context("project has no root")?)?;
    let mut db=migration::open_active(&path)?;let snapshot=db.read_snapshot(Some(head))?;
    let op=snapshot.operations.iter().find(|o|&o.id==id).context("operation not found")?;
    let payload=Finalization::decode(op)?;payload.validate(op,&snapshot,&crate::notification_delivery::config(ctx,&path)?)?;
    let receipt=load_receipt(&project(&path)?,op,&payload)?.context("no verified finalization receipt; intent remains unresolved")?;
    Ok(db.observe_finalization(id,revision,head,&receipt,jiff::Timestamp::now().as_millisecond())?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::scenarios::World;
    use herdr_projects::{domain::{TaskState,Attempt,AttemptId,AttemptState,Commit,Mutation},operations::DeliveryState};
    pub(crate) fn fixture()->(World,PathBuf,Operation) {
        let world=World::new();let project=crate::project::create(&world.root,"finalize","",vec![]).unwrap();project.set_status(crate::project::Status::Paused).unwrap();
        let source=world.home.path().join("artifact-root/source");fs::create_dir_all(source.join("library")).unwrap();fs::write(source.join("report.md"),"report for review\n").unwrap();fs::write(source.join("library/artifact"),b"preserved bytes").unwrap();
        let thread=Thread{id:"t-0001".into(),status:crate::thread::Status::Resolved,thread_dir:source.to_str().unwrap().into(),..Default::default()};fs::write(project.dir().join("threads/t-0001.toml"),toml::to_string(&thread).unwrap()).unwrap();
        let path=project.dir().canonicalize().unwrap();let plan=migration::inspect(&path).unwrap();migration::apply(&path,&plan,true).unwrap();let head=runtime::snapshot(&path).unwrap().head;let op=enqueue(&world.ctx(),&path,"thread:t-0001",head,"operator requests artifact review".into()).unwrap();(world,path,op)
    }
    #[test]
    fn finalization_commits_verified_receipt_and_review_disposition_without_legacy_mutation() {
        let(world,path,op)=fixture();let original=fs::read(path.join("threads/t-0001.toml")).unwrap();let before=runtime::snapshot(&path).unwrap();
        let result=deliver(&world.ctx(),&path,&op.id,1).unwrap();assert!(matches!(result,DispatchResult::Recorded(d) if d.state==DeliveryState::Confirmed));let after=runtime::snapshot(&path).unwrap();let task=after.tasks.iter().find(|t|t.id==op.task).unwrap();assert_eq!(task.state,TaskState::AwaitingReview);assert_eq!(task.revision,op.expected_revision+1);assert_eq!(after.attempts,before.attempts);assert_eq!(after.runtime_bindings,before.runtime_bindings);
        let payload=Finalization::decode(&op).unwrap();let receipt=load_receipt(&project(&path).unwrap(),&op,&payload).unwrap().unwrap();assert_eq!(fs::read(path.join(".state/canonical-artifacts").join(receipt.artifact_key).join(receipt.snapshot).join("library/artifact")).unwrap(),b"preserved bytes");assert_eq!(fs::read(path.join("threads/t-0001.toml")).unwrap(),original);assert!(!path.join(".state/artifacts").exists());assert!(deliver(&world.ctx(),&path,&op.id,1).is_err());
    }
    #[test]
    fn finalization_stale_task_config_and_retained_attempts_never_copy() {
        for change in ["task","config","attempt"] {
            let(world,path,op)=fixture();let snapshot=runtime::snapshot(&path).unwrap();
            match change {
                "task"=>{runtime::rename_task(&path,&op.task,"changed".into(),op.expected_revision,snapshot.head).unwrap();},
                "config"=>{fs::create_dir_all(&world.ctx().config_dir).unwrap();fs::write(world.ctx().config_dir.join("config.toml"),"# changed").unwrap();},
                _=>{let attempt=Attempt{id:AttemptId::new("lost").unwrap(),task:op.task.clone(),revision:1,state:AttemptState::Lost,snapshot:None,reservation:"retained".into(),termination_observed:false};migration::open_active(&path).unwrap().commit(Commit{expected_head:snapshot.head,mutations:vec![Mutation::Attempt{expected:None,next:attempt}]}).unwrap();},
            }
            let before=runtime::snapshot(&path).unwrap();assert!(deliver(&world.ctx(),&path,&op.id,1).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);assert!(!path.join(".state/canonical-artifacts").exists());
        }
    }
    #[test]
    fn finalization_changed_report_and_corrupted_retained_bytes_cannot_complete() {
        let(world,path,op)=fixture();let payload=Finalization::decode(&op).unwrap();fs::write(Path::new(&payload.source).join("report.md"),b"changed report").unwrap();
        let result=deliver(&world.ctx(),&path,&op.id,1).unwrap();assert!(matches!(result,DispatchResult::Recorded(d) if d.state==DeliveryState::Ambiguous));assert!(load_receipt(&project(&path).unwrap(),&op,&payload).unwrap().is_none());let before=runtime::snapshot(&path).unwrap();assert!(observe(&world.ctx(),&path,&op.id,3,before.head).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);
        let(world,path,op)=fixture();let payload=Finalization::decode(&op).unwrap();let mut adapter=Adapter{ctx:&world.ctx(),path:path.clone()};let mut prepared=adapter.prepare(&op).unwrap();let mut db=migration::open_active(&path).unwrap();let now=jiff::Timestamp::now().as_millisecond();let claim=db.claim_operation(&op.id,1,"fixture",now,300_000).unwrap();prepared.deliver(&op,&claim).unwrap();drop(prepared);db.expire_claims(claim.lease_until_ms+1).unwrap();let receipt=load_receipt(&project(&path).unwrap(),&op,&payload).unwrap().unwrap();fs::write(path.join(".state/canonical-artifacts").join(receipt.artifact_key).join(receipt.snapshot).join("library/artifact"),b"corrupted").unwrap();let before=runtime::snapshot(&path).unwrap();assert!(observe(&world.ctx(),&path,&op.id,3,before.head).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);
    }
    #[test]
    fn finalization_task_and_delivery_receipt_roll_back_together() {
        let(world,path,op)=fixture();let ctx=world.ctx();let mut adapter=Adapter{ctx:&ctx,path:path.clone()};let mut prepared=adapter.prepare(&op).unwrap();let mut db=migration::open_active(&path).unwrap();let claim=db.claim_operation(&op.id,1,"rollback-fixture",jiff::Timestamp::now().as_millisecond(),300_000).unwrap();let outcome=prepared.deliver(&op,&claim).unwrap();let before=db.read_snapshot(None).unwrap();
        let raw=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();raw.execute_batch("CREATE TRIGGER reject_outcome BEFORE UPDATE ON operation_delivery WHEN NEW.state='confirmed' BEGIN SELECT RAISE(ABORT,'fixture outcome write failure'); END;").unwrap();assert!(db.finish_operation(&claim,outcome.clone(),jiff::Timestamp::now().as_millisecond()).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);raw.execute_batch("DROP TRIGGER reject_outcome;").unwrap();
        let mut wrong:FinalizationReceipt=match &outcome {Outcome::Confirmed{observed_identity}=>serde_json::from_str(observed_identity).unwrap(),_=>unreachable!()};wrong.report_hash="0".repeat(64);assert!(db.finish_operation(&claim,Outcome::Confirmed{observed_identity:serde_json::to_string(&wrong).unwrap()},jiff::Timestamp::now().as_millisecond()).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
        assert_eq!(db.finish_operation(&claim,outcome,jiff::Timestamp::now().as_millisecond()).unwrap().state,DeliveryState::Confirmed);assert_eq!(db.read_snapshot(None).unwrap().tasks.iter().find(|t|t.id==op.task).unwrap().revision,op.expected_revision+1);
    }
    #[test]
    fn finalization_refuses_replaced_source_ancestor_and_preserves_existing_receipts() {
        let(world,path,op)=fixture();let original=world.home.path().join("artifact-root");let alternate=world.home.path().join("alternate");fs::rename(&original,&alternate).unwrap();std::os::unix::fs::symlink(&alternate,&original).unwrap();
        let before=runtime::snapshot(&path).unwrap();let result=deliver(&world.ctx(),&path,&op.id,1).unwrap();assert!(matches!(result,DispatchResult::Recorded(d) if d.state==DeliveryState::Ambiguous));assert_eq!(runtime::snapshot(&path).unwrap().tasks,before.tasks);assert!(!path.join(".state/canonical-artifacts").exists());
        let payload=Finalization::decode(&op).unwrap();let p=project(&path).unwrap();let receipt=FinalizationReceipt{operation_hash:digest(&serde_json::to_vec(&op).unwrap()),artifact_key:payload.artifact_key(),snapshot:"a".repeat(64),report_hash:payload.report_hash.clone(),binding_revision:payload.binding_revision};
        save_receipt(&p,&op,&receipt).unwrap();let bytes=fs::read(receipt_path(&p,&op)).unwrap();let mut changed=receipt;changed.snapshot="b".repeat(64);assert!(save_receipt(&p,&op,&changed).is_err());assert_eq!(fs::read(receipt_path(&p,&op)).unwrap(),bytes);
    }
    fn pause(home:&Path)->! {fs::write(home.join("finalization-ready"),b"ready").unwrap();loop {std::thread::sleep(std::time::Duration::from_secs(1));}}
    #[test]
    fn finalization_crash_child() {
        let Some(home)=std::env::var_os("HP_FINALIZE_CRASH_HOME") else{return;};let home=PathBuf::from(home);let path=home.join("root/finalize");let env=crate::paths::Env::for_test(&home,&[]);let runner=crate::runner::fake::FakeRunner::new();let ctx=Ctx{env:&env,root:home.join("root"),config_dir:home.join("cfg"),runner:&runner,detached_ticker:false};let mut db=migration::open_active(&path).unwrap();let op=db.read_snapshot(None).unwrap().operations.into_iter().find(|o|o.kind=="runtime.finalization").unwrap();
        let mut adapter=Adapter{ctx:&ctx,path};let mut prepared=adapter.prepare(&op).unwrap();let claim=db.claim_operation(&op.id,1,"crash-fixture",jiff::Timestamp::now().as_millisecond(),300_000).unwrap();let phase=std::env::var("HP_FINALIZE_CRASH_PHASE").unwrap();prepared.revalidate(&op).unwrap();if phase=="before" {pause(&home);}
        let outcome=prepared.deliver(&op,&claim).unwrap();if phase=="after"{pause(&home);}db.finish_operation(&claim,outcome,jiff::Timestamp::now().as_millisecond()).unwrap();pause(&home);
    }
    #[test]
    fn finalization_process_death_preserves_intent_and_recovers_published_receipt() {
        use std::{process::{Command,Stdio},time::{Duration,Instant}};
        for phase in ["before","after","committed"] {
            let(world,path,op)=fixture();let mut child=Command::new(std::env::current_exe().unwrap()).args(["--exact","finalization_delivery::tests::finalization_crash_child","--nocapture"]).env("HP_FINALIZE_CRASH_HOME",world.home.path()).env("HP_FINALIZE_CRASH_PHASE",phase).stdout(Stdio::null()).stderr(Stdio::inherit()).spawn().unwrap();let deadline=Instant::now()+Duration::from_secs(10);
            while !world.home.path().join("finalization-ready").exists(){if child.try_wait().unwrap().is_some(){panic!("crash fixture exited before ready");}if Instant::now()>deadline{let _=child.kill();let _=child.wait();panic!("crash fixture readiness timeout");}std::thread::sleep(Duration::from_millis(10));}
            child.kill().unwrap();child.wait().unwrap();migration::recover(&path,true).unwrap();let mut db=migration::open_active(&path).unwrap();let delivery=db.deliveries().unwrap().into_iter().find(|d|d.operation==op.id).unwrap();
            if phase=="committed" {assert_eq!(delivery.state,DeliveryState::Confirmed);assert_eq!(db.read_snapshot(None).unwrap().tasks.iter().find(|t|t.id==op.task).unwrap().revision,op.expected_revision+1);continue;}
            assert_eq!(delivery.state,DeliveryState::Claimed);db.expire_claims(delivery.lease_until_ms.unwrap()+1).unwrap();let before=db.read_snapshot(None).unwrap();let pending=db.deliveries().unwrap().into_iter().find(|d|d.operation==op.id).unwrap();assert_eq!(pending.state,DeliveryState::Ambiguous);assert_eq!(before.tasks.iter().find(|t|t.id==op.task).unwrap().revision,op.expected_revision);
            if phase=="before" {assert!(observe(&world.ctx(),&path,&op.id,pending.revision,before.head).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);}else{
                // Receipt recovery uses retained bytes, not a second source copy.
                fs::remove_dir_all(world.home.path().join("artifact-root/source")).unwrap();assert_eq!(observe(&world.ctx(),&path,&op.id,pending.revision,before.head).unwrap().state,DeliveryState::Confirmed);assert_eq!(runtime::snapshot(&path).unwrap().tasks.iter().find(|t|t.id==op.task).unwrap().revision,op.expected_revision+1);
            }
        }
    }
}
