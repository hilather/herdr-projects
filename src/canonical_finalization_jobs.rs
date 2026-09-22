//! Canonical filesystem effects run under project ownership, outside the ticker.
use std::{path::{Path,PathBuf},os::unix::fs::MetadataExt,sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Serialize,Deserialize};
use crate::{paths::{Ctx,Env},runner::{Runner,Cmd,Output},source_tree::{Control,Directory},finalization_delivery as receipt};
use herdr_projects::{runtime,migration,execution_guard::ProjectGuard,domain::{Operation,OperationId,Snapshot},operations::{Claim,Outcome,DeliveryState,finalization::{Finalization,FinalizationReceipt,digest},dispatch::{self,DeliveryAdapter,PreparedDelivery,DispatchRequest,DispatchResult}}};
const JOB:&str="\0herdr-projects-canonical-finalization";
const BUDGET:Duration=Duration::from_secs(180);
#[derive(Clone,Copy,Serialize,Deserialize,PartialEq,Eq)]
#[serde(rename_all="snake_case")]
pub enum Mode {Deliver,Observe}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {project:PathBuf,identity:(u64,u64),home:PathBuf,bin:String,config:PathBuf,config_reference:migration::ConfigReference,operation:OperationId,operation_digest:String,revision:u64,mode:Mode}
impl Input {
    fn current(&self,ctx:&Ctx,guard:&ProjectGuard,control:&Control)->Result<()> {
        control.check()?;ensure!(self.project.is_absolute()&&self.home.is_absolute()&&self.config.is_absolute()&&!self.bin.is_empty()&&self.revision>0,"invalid canonical finalization input");guard.check_project(&self.project)?;
        let metadata=std::fs::metadata(&self.project)?;
        ensure!((metadata.dev(),metadata.ino())==self.identity&&self.project.canonicalize()?==self.project,"finalization project changed");
        ensure!(crate::notification_delivery::config(ctx,&self.project)?==self.config_reference,"finalization configuration changed");control.check()
    }
}
pub fn request(ctx:&Ctx,path:&Path,operation:&Operation,revision:u64,mode:Mode)->Result<crate::executor::Request> {
    let project=path.canonicalize()?;let metadata=std::fs::metadata(&project)?;
    let input=Input{identity:(metadata.dev(),metadata.ino()),config_reference:crate::notification_delivery::config(ctx,&project)?,project,home:std::path::absolute(&ctx.env.home)?,bin:ctx.env.herdr_bin(),config:std::path::absolute(&ctx.config_dir)?,operation:operation.id.clone(),operation_digest:digest(&serde_json::to_vec(operation)?),revision,mode};
    let text=serde_json::to_string(&input)?;ensure!(text.len()<=64*1024,"finalization input exceeds bounds");let deadline=Instant::now()+BUDGET;
    let mut command=Cmd::new(JOB,BUDGET).stdin(text);command.deadline=Some(deadline);command.capture_limit=1024*1024;
    Ok(crate::executor::Request{identity:crate::executor::Identity{operation:format!("canonical-finalization-{}:{}",if mode==Mode::Deliver{"deliver"}else{"observe"},operation.id.as_str()),revision,project:input.project.display().to_string(),machine:"local-artifacts".into(),terminal:None},lane:crate::executor::Lane::Transfer,deadline,command})
}
struct Adapter<'a,'b> {input:&'a Input,ctx:&'a Ctx<'b>,control:&'a Control,guard:&'a ProjectGuard}
struct Prepared<'a,'b> {adapter:Adapter<'a,'b>,payload:Finalization,source:Source}
enum Source {Receipt,Original((u64,u64)),#[cfg(target_os="linux")] Preserved}
impl Adapter<'_,'_> {
    fn validate(&self,operation:&Operation,payload:&Finalization)->Result<Snapshot> {
        self.input.current(self.ctx,self.guard,self.control)?;
        ensure!(operation.id==self.input.operation&&digest(&serde_json::to_vec(operation)?)==self.input.operation_digest,"finalization operation changed");
        let snapshot=runtime::snapshot(&self.input.project)?;payload.validate(operation,&snapshot,&self.input.config_reference)?;self.control.check()?;Ok(snapshot)
    }
    fn claim(&self,operation:&Operation,payload:&Finalization,claim:&Claim)->Result<()> {
        self.validate(operation,payload)?;
        migration::open_active(&self.input.project)?.validate_claim(claim,jiff::Timestamp::now().as_millisecond())?;self.control.check()
    }
}
impl<'a,'b> DeliveryAdapter for Adapter<'a,'b> {
    type Prepared=Prepared<'a,'b>;
    fn prepare(&mut self,operation:&Operation)->Result<Self::Prepared> {
        let payload=Finalization::decode(operation)?;self.validate(operation,&payload)?;
        let project=receipt::project(&self.input.project)?;
        // Freeze source identity only when a new capture is needed. A retained
        // receipt must recover even after the entire source has disappeared.
        let source=if receipt::load_receipt_controlled(&project,operation,&payload,self.control)?.is_some(){Source::Receipt}else{
            #[cfg(target_os="linux")]
            if receipt::preserved_outputs(&self.input.project,&payload.binding,self.control)?.is_some() {
                self.validate(operation,&payload)?;
                return Ok(Prepared{adapter:Adapter{input:self.input,ctx:self.ctx,control:self.control,guard:self.guard},payload,source:Source::Preserved});
            }
            ensure!(std::fs::canonicalize(&payload.source)?==Path::new(&payload.source),"artifact source path changed");
            let directory=Directory::open(Path::new(&payload.source))?;directory.matches_path(Path::new(&payload.source))?;let metadata=directory.metadata()?;Source::Original((metadata.dev(),metadata.ino()))
        };
        self.validate(operation,&payload)?;
        Ok(Prepared{adapter:Adapter{input:self.input,ctx:self.ctx,control:self.control,guard:self.guard},payload,source})
    }
}
impl PreparedDelivery for Prepared<'_,'_> {
    fn revalidate(&mut self,operation:&Operation)->Result<()>{self.adapter.validate(operation,&self.payload)?;Ok(())}
    fn deliver(&mut self,operation:&Operation,claim:&Claim)->Result<Outcome> {
        let a=&self.adapter;let project=receipt::project(&a.input.project)?;let authorize=||a.claim(operation,&self.payload,claim);authorize()?;
        let result=if let Some(retained)=receipt::load_receipt_controlled(&project,operation,&self.payload,a.control)? {retained}else{
            let snapshot=match self.source {
                Source::Receipt=>anyhow::bail!("retained receipt disappeared; refusing a second source copy"),
                Source::Original(source)=>crate::artifacts::capture_canonical_controlled(&project,&receipt::record(&self.payload),a.control,Some(source),authorize)?,
                #[cfg(target_os="linux")]
                Source::Preserved=>{
                    let outputs=receipt::preserved_outputs(&a.input.project,&self.payload.binding,a.control)?.context("preserved output evidence disappeared")?;
                    crate::artifacts::capture_preserved_outputs(&project,&receipt::record(&self.payload),&outputs,a.control,authorize)?
                }
            };
            ensure!(snapshot.manifest.matches_canonical_execution(&receipt::record(&self.payload))&&snapshot.manifest.report_hash()==Some(self.payload.report_hash.as_str()),"captured artifact does not match accepted finalization");
            let result=FinalizationReceipt{operation_hash:digest(&serde_json::to_vec(operation)?),artifact_key:self.payload.artifact_key(),snapshot:snapshot.id,report_hash:self.payload.report_hash.clone(),binding_revision:self.payload.binding_revision};
            receipt::save_receipt_authorized(&project,operation,&result,a.control,authorize)?;result
        };
        // A receipt describes verified retained bytes, never queue success.
        ensure!(receipt::load_receipt_controlled(&project,operation,&self.payload,a.control)?.as_ref()==Some(&result),"published finalization receipt changed");
        authorize()?;Ok(Outcome::Confirmed{observed_identity:serde_json::to_string(&result)?})
    }
}
fn execute(input:&Input,control:&Control)->Result<()> {
    control.check()?;let guard=ProjectGuard::acquire(&input.project)?;
    // No effect-capable subprocess exists. These handles exclude managed
    // routines and stay alive through capture, receipt and database persistence.
    let _locks=guard.inherit_transfer()?;
    let env=Env::for_observation(&input.home,&input.bin);let ctx=Ctx{env:&env,root:input.project.parent().context("finalization root missing")?.into(),config_dir:input.config.clone(),runner:&crate::runner::RealRunner,detached_ticker:false};
    input.current(&ctx,&guard,control)?;let mut db=migration::open_active(&input.project)?;let mut adapter=Adapter{input,ctx:&ctx,control,guard:&guard};
    if input.mode==Mode::Observe {
        let snapshot=db.read_snapshot(None)?;let operation=snapshot.operations.iter().find(|op|op.id==input.operation).context("finalization operation missing")?;
        let payload=Finalization::decode(operation)?;adapter.validate(operation,&payload)?;
        ensure!(snapshot.deliveries.iter().any(|d|d.operation==input.operation&&d.revision==input.revision&&d.state==DeliveryState::Ambiguous),"finalization observation is stale");
        let retained=receipt::load_receipt_controlled(&receipt::project(&input.project)?,operation,&payload,control)?.context("no verified finalization receipt; intent remains unresolved")?;
        ensure!(adapter.validate(operation,&payload)?.head==snapshot.head,"finalization observation head changed");control.check()?;
        db.observe_finalization(&input.operation,input.revision,snapshot.head,&retained,jiff::Timestamp::now().as_millisecond())?;return Ok(());
    }
    let result=dispatch::dispatch_one(&mut db,DispatchRequest{operation:&input.operation,expected_revision:input.revision,owner:"ticker.finalization",lease_ms:300_000},&mut adapter,||jiff::Timestamp::now().as_millisecond())?;
    match result {DispatchResult::Recorded(_)=>Ok(()),DispatchResult::Unrecorded{..}=>anyhow::bail!("canonical finalization outcome unrecorded; retain claim and observe after expiry")}
}
pub struct JobRunner {pub inner:Arc<dyn Runner+Send+Sync>}
impl Runner for JobRunner {
    fn run(&self,command:&Cmd)->Result<Output>{if command.program!=JOB{return self.inner.run(command);}let entered=Instant::now();ensure!(!command.timeout.is_zero()&&command.timeout<=BUDGET,"invalid canonical finalization budget");let text=command.stdin.as_deref().context("finalization input missing")?;ensure!(text.len()<=64*1024,"finalization input exceeds bounds");let control=Control{deadline:command.deadline.context("finalization deadline missing")?.min(entered+command.timeout),cancellation:command.cancellation.clone().context("finalization cancellation missing")?};execute(&serde_json::from_str(text)?,&control)?;Ok(Output{code:Some(0),elapsed:entered.elapsed(),..Output::default()})}
    fn socket_request(&self,path:&Path,line:&str,timeout:Duration)->Result<String>{self.inner.socket_request(path,line,timeout)}
}
#[cfg(test)]
#[path="canonical_finalization_jobs_tests.rs"]
mod tests;
