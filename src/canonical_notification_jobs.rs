//! Concrete canonical notification effects; executor replies are not receipts.
use std::{path::{Path,PathBuf},os::unix::fs::{MetadataExt,FileTypeExt},sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Serialize,Deserialize};
use crate::{paths::{Ctx,Env},runner::{Runner,Cmd,Output,InheritedLock},source_tree::Control};
use herdr_projects::{runtime,migration,execution_guard::ProjectGuard,domain::{Operation,OperationId},operations::{Claim,Outcome,notification::Notification,dispatch::{self,DeliveryAdapter,PreparedDelivery,DispatchRequest,DispatchResult}}};
const JOB:&str="\0herdr-projects-canonical-notification";
const BUDGET:Duration=Duration::from_secs(45);
fn socket_identity(path:&Path)->Result<(u64,u64)>{let m=std::fs::symlink_metadata(path)?;ensure!(m.file_type().is_socket(),"notification endpoint must be a socket");Ok((m.dev(),m.ino()))}
fn fingerprint(operation:&Operation)->Result<String>{Ok(crate::thread::sha256_hex(&serde_json::to_vec(operation)?))}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {project:PathBuf,identity:(u64,u64),home:PathBuf,bin:String,config:PathBuf,config_reference:migration::ConfigReference,socket:PathBuf,socket_identity:(u64,u64),operation:OperationId,operation_digest:String,revision:u64}
impl Input {
    fn current(&self,ctx:&Ctx,guard:&ProjectGuard,control:&Control)->Result<()> {
        control.check()?;ensure!(self.project.is_absolute()&&self.home.is_absolute()&&self.config.is_absolute()&&self.socket.is_absolute()&&!self.bin.is_empty()&&self.revision>0,"invalid canonical notification input");guard.check_project(&self.project)?;
        let m=std::fs::metadata(&self.project)?;ensure!((m.dev(),m.ino())==self.identity&&self.project.canonicalize()?==self.project,"notification project changed");
        ensure!(socket_identity(&self.socket)?==self.socket_identity,"notification session changed");
        ensure!(crate::notification_delivery::config(ctx,&self.project)?==self.config_reference,"notification configuration changed");control.check()
    }
}
pub fn request(ctx:&Ctx,path:&Path,operation:&Operation,revision:u64,socket:&str)->Result<crate::executor::Request> {
    let project=path.canonicalize()?;let m=std::fs::metadata(&project)?;let socket=PathBuf::from(socket);
    let input=Input{identity:(m.dev(),m.ino()),config_reference:crate::notification_delivery::config(ctx,&project)?,project,home:std::path::absolute(&ctx.env.home)?,bin:ctx.env.herdr_bin(),config:std::path::absolute(&ctx.config_dir)?,socket_identity:socket_identity(&socket)?,socket,operation:operation.id.clone(),operation_digest:fingerprint(operation)?,revision};
    let text=serde_json::to_string(&input)?;ensure!(text.len()<=64*1024,"notification input exceeds bounds");let deadline=Instant::now()+BUDGET;
    let mut command=Cmd::new(JOB,BUDGET).stdin(text);command.deadline=Some(deadline);command.capture_limit=1024*1024;
    Ok(crate::executor::Request{identity:crate::executor::Identity{operation:format!("canonical-notification:{}",operation.id.as_str()),revision,project:input.project.display().to_string(),machine:format!("local-session:{}",crate::thread::sha256_hex(&serde_json::to_vec(&(&input.socket,input.socket_identity))?)),terminal:None},lane:crate::executor::Lane::Control,deadline,command})
}
struct Adapter<'a,'b> {input:&'a Input,ctx:&'a Ctx<'b>,control:&'a Control,guard:&'a ProjectGuard,locks:&'a [InheritedLock]}
struct Prepared<'a,'b> {adapter:Adapter<'a,'b>,notification:Notification}
impl Adapter<'_,'_> {
    fn validate(&self,operation:&Operation,notification:&Notification)->Result<()> {
        self.input.current(self.ctx,self.guard,self.control)?;
        ensure!(operation.id==self.input.operation&&fingerprint(operation)?==self.input.operation_digest,"notification operation changed");
        let snapshot=runtime::snapshot(&self.input.project)?;
        ensure!(Path::new(&notification.validate(operation,&snapshot,&self.input.config_reference)?.identity.socket)==self.input.socket,"notification route changed");self.control.check()
    }
    fn run(&self,command:Cmd)->Result<Output>{herdr_projects::supervision::run(command,self.control.deadline,self.control.cancellation.clone(),self.locks)}
}
impl<'a,'b> DeliveryAdapter for Adapter<'a,'b> {
    type Prepared=Prepared<'a,'b>;
    fn prepare(&mut self,operation:&Operation)->Result<Self::Prepared> {
        let notification=Notification::decode(operation)?;self.validate(operation,&notification)?;
        let h=crate::herdr::Herdr::new(&self.input.bin,&self.input.socket,&crate::runner::RealRunner);
        let output=self.run(h.cmd(crate::herdr::CALL_TIMEOUT).args(["remote-api-bridge","--check"]))?;self.control.check()?;
        ensure!(output.success()&&output.stdout.trim()=="herdr-api-bridge-v1","canonical notification bridge unavailable");self.validate(operation,&notification)?;
        Ok(Prepared{adapter:Adapter{input:self.input,ctx:self.ctx,control:self.control,guard:self.guard,locks:self.locks},notification})
    }
}
impl PreparedDelivery for Prepared<'_,'_> {
    fn revalidate(&mut self,operation:&Operation)->Result<()>{self.adapter.validate(operation,&self.notification)}
    fn deliver(&mut self,operation:&Operation,claim:&Claim)->Result<Outcome> {
        let a=&self.adapter;a.validate(operation,&self.notification)?;
        let now=jiff::Timestamp::now().as_millisecond();let reserve=crate::herdr::CALL_TIMEOUT+Duration::from_secs(2);
        if claim.lease_until_ms-now<reserve.as_millis() as i64||a.control.deadline.saturating_duration_since(Instant::now())<reserve {
            return Ok(Outcome::Retryable{no_effect_evidence:"notification was not called: insufficient remaining worker or claim budget".into()});
        }
        let id=format!("canonical-notification-{}-{}",operation.id.as_str(),claim.epoch);
        let frame=serde_json::to_string(&serde_json::json!({"id":id,"method":"notification.show","params":{"title":self.notification.title,"body":self.notification.body}}))?+"\n";ensure!(frame.len()<=64*1024,"notification frame exceeds bounds");
        let h=crate::herdr::Herdr::new(&a.input.bin,&a.input.socket,&crate::runner::RealRunner);
        let output=a.run(h.cmd(crate::herdr::CALL_TIMEOUT).arg("remote-api-bridge").stdin(frame))?;a.control.check()?;ensure!(output.success(),"canonical notification command failed");
        let reply:serde_json::Value=serde_json::from_str(&output.stdout)?;ensure!(reply["id"].as_str()==Some(&id)&&reply.get("error").is_none()&&reply["result"]["type"].as_str()==Some("notification_show"),"canonical notification acknowledgement mismatch");
        a.validate(operation,&self.notification)?;
        match (reply["result"]["shown"].as_bool(),reply["result"]["reason"].as_str()) {
            (Some(true),Some("shown"))=>Ok(Outcome::Confirmed{observed_identity:format!("herdr.notification.shown:{}:binding-{}:epoch-{}",operation.id.as_str(),self.notification.binding_revision,self.notification.control_epoch)}),
            (Some(false),Some(reason @ ("disabled"|"rate_limited"|"no_foreground_client"|"busy")))=>Ok(Outcome::Retryable{no_effect_evidence:format!("native notification was not shown: {reason}")}),
            _=>anyhow::bail!("canonical notification outcome is inconsistent"),
        }
    }
}
fn execute(input:&Input,control:&Control)->Result<()> {
    control.check()?;let guard=ProjectGuard::acquire(&input.project)?;let locks=guard.inherit_transfer()?;
    let env=Env::for_observation(&input.home,&input.bin);let ctx=Ctx{env:&env,root:input.project.parent().context("notification root missing")?.into(),config_dir:input.config.clone(),runner:&crate::runner::RealRunner,detached_ticker:false};
    input.current(&ctx,&guard,control)?;let mut db=migration::open_active(&input.project)?;
    let mut adapter=Adapter{input,ctx:&ctx,control,guard:&guard,locks:&locks};
    let result=dispatch::dispatch_one(&mut db,DispatchRequest{operation:&input.operation,expected_revision:input.revision,owner:"ticker.notification",lease_ms:60_000},&mut adapter,||jiff::Timestamp::now().as_millisecond())?;
    match result {DispatchResult::Recorded(_)=>Ok(()),DispatchResult::Unrecorded{..}=>anyhow::bail!("canonical notification outcome unrecorded; inspect operation after claim expiry")}
}
pub struct JobRunner {pub inner:Arc<dyn Runner+Send+Sync>}
impl Runner for JobRunner {
    fn run(&self,command:&Cmd)->Result<Output>{if command.program!=JOB{return self.inner.run(command);}let entered=Instant::now();ensure!(!command.timeout.is_zero()&&command.timeout<=BUDGET,"invalid canonical notification budget");let text=command.stdin.as_deref().context("notification input missing")?;ensure!(text.len()<=64*1024,"notification input exceeds bounds");let control=Control{deadline:command.deadline.context("notification deadline missing")?.min(entered+command.timeout),cancellation:command.cancellation.clone().context("notification cancellation missing")?};execute(&serde_json::from_str(text)?,&control)?;Ok(Output{code:Some(0),elapsed:entered.elapsed(),..Output::default()})}
    fn socket_request(&self,path:&Path,line:&str,timeout:Duration)->Result<String>{self.inner.socket_request(path,line,timeout)}
}

#[cfg(test)]
#[path="canonical_notification_jobs_tests.rs"]
mod tests;
