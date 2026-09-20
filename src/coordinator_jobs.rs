//! Durable, supervised coordinator priming. Queue completion is not a receipt.
use std::{path::{Path,PathBuf},os::unix::fs::MetadataExt,sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Serialize,Deserialize};
use crate::{project::{self,Project,Coordinator},paths::{self,Ctx},runner::{Cmd,Output,Runner,InheritedLock},source_tree::Control,thread};
use herdr_projects::{execution_guard::ProjectGuard,coordinator_prime::Claim,prompt_claim::Phase};
const JOB:&str="\0herdr-projects-coordinator-prime";
#[path="coordinator_start_jobs.rs"]
mod start;
const BUDGET:Duration=Duration::from_secs(45);
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {#[serde(default)] start:bool,project:PathBuf,identity:(u64,u64),execution:String,request:u64,sequence:u64,socket:PathBuf,socket_identity:(u64,u64),herdr:String,config:PathBuf,config_digest:Option<String>,settings_digest:String,prompt:String}
fn digest(text:Option<&str>)->Option<String>{text.map(|s|thread::sha256_hex(s.as_bytes()))}
fn socket_identity(path:&Path)->Result<(u64,u64)>{use std::os::unix::fs::FileTypeExt;let m=std::fs::symlink_metadata(path)?;ensure!(m.file_type().is_socket(),"coordinator endpoint is not a socket");Ok((m.dev(),m.ino()))}
fn execution(c:&Coordinator)->String{thread::sha256_hex(&serde_json::to_vec(&(&c.socket,&c.session,&c.workspace_id,&c.tab_id,&c.pane_id,&c.agent_name,&c.cwd,c.prime_request)).unwrap())}
pub fn validate(c:&Coordinator)->Result<()> {ensure!(c.prime_request<=i64::MAX as u64&&c.prime_sequence<=i64::MAX as u64,"coordinator prime counters exhausted");if let Some(claim)=&c.prime_claim{claim.validate(c.prime_sequence,c.prime_request)?;}
    ensure!(c.launch_sequence<=i64::MAX as u64,"coordinator launch sequence exhausted");
    if let Some(claim)=&c.launch_claim {
        claim.validate(c.launch_sequence)?;ensure!(claim.generation<=c.prime_request,"coordinator launch request is in the future");
        ensure!(claim.phase!=herdr_projects::launch_claim::Phase::Pending||c.prime_claim.as_ref().is_none_or(|c|c.delivery.phase!=Phase::Pending),"coordinator has conflicting pending effects");
    }Ok(())}
pub fn ready(c:&Coordinator)->Result<()> {
    validate(c)?;
    if let Some(claim)=&c.launch_claim {ensure!(claim.phase!=herdr_projects::launch_claim::Phase::Pending&&claim.notified&&(claim.phase!=herdr_projects::launch_claim::Phase::Uncertain||claim.generation!=c.prime_request),"coordinator launch requires reconciliation before priming");}
    ensure!(c.prime_pending&&!c.pane_id.is_empty(),"coordinator has no pending prime");
    if let Some(claim)=&c.prime_claim{ensure!(claim.delivery.phase!=Phase::Pending&&claim.delivery.notified&&claim.request!=c.prime_request,"coordinator priming already claimed; inspect the pane before open --reprime");}Ok(())
}
fn current(input:&Input,p:&Project,guard:&ProjectGuard,control:&Control)->Result<(Coordinator,String)> {
    control.check()?;guard.check_project(&input.project)?;project::ensure_legacy(&input.project)?;let m=std::fs::metadata(&input.project)?;ensure!((m.dev(),m.ino())==input.identity&&p.try_status()?==project::Status::Active,"coordinator project authority changed");
    let c=p.try_coordinator()?.context("coordinator record missing")?;ensure!(execution(&c)==input.execution&&c.prime_request==input.request&&c.socket==input.socket.to_str().context("invalid coordinator socket")?&&socket_identity(&input.socket)?==input.socket_identity,"coordinator execution changed");
    let cfg=paths::read_control_text(&input.config.join("config.toml"),1024*1024)?;ensure!(digest(cfg.as_deref())==input.config_digest,"coordinator configuration changed");project::parse_safety(cfg.as_deref().unwrap_or(""),&input.project).map_err(|_|anyhow::anyhow!("invalid coordinator safety configuration (contents withheld)"))?;
    let md=paths::read_control_text(&p.project_md(),1024*1024)?.context("project settings missing")?;ensure!(thread::sha256_hex(md.as_bytes())==input.settings_digest,"coordinator project settings changed");let(settings,_)=project::parse_project_md(&md).map_err(|_|anyhow::anyhow!("invalid coordinator settings (contents withheld)"))?;
    ensure!(input.prompt==crate::coordinator::priming_prompt(&crate::coordinator::current_prefix(&p.root)?,&p.slug),"coordinator priming payload changed");control.check()?;Ok((c,settings.coordinator_agent))
}
fn run(cmd:Cmd,control:&Control,locks:&[InheritedLock])->Result<serde_json::Value>{let out=herdr_projects::supervision::run(cmd,control.deadline,control.cancellation.clone(),locks)?;control.check()?;ensure!(out.success(),"coordinator observation failed");let reply:serde_json::Value=serde_json::from_str(&out.stdout)?;ensure!(reply.get("error").is_none(),"coordinator observation rejected");reply.get("result").cloned().context("coordinator observation has no result")}
fn execute(input:&Input,control:&Control)->Result<()> {execute_with(input,control,||Ok(()))}
fn execute_with(input:&Input,control:&Control,after_claim:impl FnOnce()->Result<()>)->Result<()> {
    ensure!(input.project.is_absolute()&&input.config.is_absolute()&&input.socket.is_absolute()&&!input.herdr.is_empty()&&input.prompt.len()<=32768,"invalid coordinator worker input");control.check()?;let guard=ProjectGuard::acquire(&input.project)?;let locks=guard.inherit_transfer()?;
    let p=Project::load(input.project.parent().context("coordinator root missing")?,input.project.file_name().and_then(|s|s.to_str()).context("invalid coordinator project")?)?;
    let(c,kind)=current(input,&p,&guard,control)?;ready(&c)?;ensure!(c.prime_sequence==input.sequence,"coordinator prime sequence changed");
    crate::brief_jobs::ownership::check_coordinator(&p,&c.pane_id,&input.socket,control)?;
    let h=crate::herdr::Herdr::new(&input.herdr,&input.socket,&crate::runner::RealRunner);
    let probe=herdr_projects::supervision::run(h.cmd(crate::herdr::CALL_TIMEOUT).args(["remote-api-bridge","--check"]),control.deadline,control.cancellation.clone(),&locks)?;control.check()?;ensure!(probe.success()&&probe.stdout.trim()=="herdr-api-bridge-v1","coordinator JSON API bridge unavailable");
    let agents:Vec<crate::herdr::Agent>=serde_json::from_value(run(h.cmd(crate::herdr::CALL_TIMEOUT).args(["agent","list"]),control,&locks)?["agents"].clone())?;
    let agents=agents.iter().filter(|a|a.pane_id==c.pane_id).collect::<Vec<_>>();ensure!(agents.len()==1&&crate::coordinator::agent_matches(&c,agents[0])&&agents[0].agent==kind&&agents[0].ready(),"coordinator agent absent, ambiguous, changed or busy");
    let panes=run(h.cmd(crate::herdr::CALL_TIMEOUT).args(["pane","list"]),control,&locks)?;let panes=panes["panes"].as_array().context("coordinator pane inventory missing")?;let panes=panes.iter().filter(|v|v["pane_id"].as_str()==Some(&c.pane_id)).collect::<Vec<_>>();ensure!(panes.len()==1,"coordinator pane absent or ambiguous");let pane:crate::herdr::Pane=serde_json::from_value(panes[0].clone())?;ensure!(crate::coordinator::pane_matches(&c,&pane),"coordinator pane changed");let terminal=panes[0]["terminal_id"].as_str().filter(|s|!s.is_empty()&&s.len()<=256).context("coordinator terminal identity unavailable")?;
    let(c,_)=current(input,&p,&guard,control)?;ready(&c)?;
    let claim=Claim{request:c.prime_request,delivery:herdr_projects::prompt_claim::Claim{sequence:c.prime_sequence.checked_add(1).context("prime sequence exhausted")?,execution:input.execution.clone(),prompt:input.prompt.clone(),phase:Phase::Pending,error:String::new(),notified:false}};claim.validate(claim.delivery.sequence,c.prime_request)?;
    p.update_coordinator_checked(|stored|{control.check()?;ensure!(stored==&c,"coordinator changed before claim");stored.prime_sequence=claim.delivery.sequence;stored.prime_claim=Some(claim.clone());Ok(())})?;after_claim()?;
    let(stored,_)=current(input,&p,&guard,control)?;ensure!(stored.prime_pending&&stored.prime_claim.as_ref()==Some(&claim),"coordinator claim changed before send");
    let id=format!("coordinator-prime-{}-{}",claim.request,claim.delivery.sequence);let payload=serde_json::to_string(&serde_json::json!({"id":id,"method":"agent.prompt","params":{"target":c.pane_id,"text":claim.delivery.prompt}}))?+"\n";ensure!(payload.len()<=64*1024,"coordinator prompt frame exceeds bounds");
    let out=herdr_projects::supervision::run(h.cmd(crate::herdr::CALL_TIMEOUT).arg("remote-api-bridge").stdin(payload),control.deadline,control.cancellation.clone(),&locks)?;control.check()?;ensure!(out.success(),"coordinator priming command failed");let reply:serde_json::Value=serde_json::from_str(&out.stdout)?;ensure!(reply["id"].as_str()==Some(&id)&&reply.get("error").is_none()&&reply["result"]["type"].as_str()==Some("agent_prompted")&&reply["result"]["agent"]["terminal_id"].as_str()==Some(terminal),"coordinator priming acknowledgement mismatch");let agent:crate::herdr::Agent=serde_json::from_value(reply["result"]["agent"].clone())?;ensure!(crate::coordinator::agent_matches(&c,&agent)&&agent.agent==kind,"coordinator priming acknowledgement names another execution");
    let(stored,_)=current(input,&p,&guard,control)?;ensure!(stored.prime_claim.as_ref()==Some(&claim)&&stored.prime_pending,"coordinator claim changed after send");
    p.update_coordinator_checked(|stored|{control.check()?;ensure!(stored.prime_claim.as_ref()==Some(&claim)&&execution(stored)==input.execution&&stored.prime_pending,"coordinator confirmation authority changed");let mut confirmed=claim.clone();confirmed.delivery.phase=Phase::Confirmed;confirmed.delivery.notified=true;stored.prime_claim=Some(confirmed);stored.prime_pending=false;Ok(())})?;Ok(())
}
pub fn ready_start(c:&Coordinator)->Result<()> {start::ready(c)}
pub fn recover(p:&Project,guard:&ProjectGuard)->Result<()> {
    start::recover(p,guard)?;
    guard.check_project(&p.dir())?;let Some(c)=p.try_coordinator()? else{return Ok(());};validate(&c)?;let Some(mut claim)=c.prime_claim else{return Ok(());};
    if claim.delivery.phase==Phase::Pending{let pending=claim.clone();claim.delivery.phase=Phase::Uncertain;claim.delivery.error="Coordinator priming was interrupted and may already have been submitted. Inspect the pane before explicitly running open --reprime.".into();p.update_coordinator_checked(|c|{ensure!(c.prime_claim.as_ref()==Some(&pending),"coordinator claim changed during recovery");c.prime_claim=Some(claim.clone());Ok(())})?;}
    if claim.delivery.phase==Phase::Uncertain&&!claim.delivery.notified{let id=format!("coordinator-prime-{}-{}",claim.request,claim.delivery.sequence);crate::inbox::write_once(p,&id,"coordinator-state","coordinator","coordinator priming needs reconciliation",&claim.delivery.error)?;std::fs::File::open(p.dir().join("inbox"))?.sync_all()?;p.update_coordinator_checked(|c|{ensure!(c.prime_claim.as_ref()==Some(&claim),"coordinator claim changed before notice acknowledgement");c.prime_claim.as_mut().unwrap().delivery.notified=true;Ok(())})?;}Ok(())
}
pub struct JobRunner{pub inner:Arc<dyn Runner+Send+Sync>}
impl Runner for JobRunner {
    fn run(&self,cmd:&Cmd)->Result<Output>{if cmd.program!=JOB{return self.inner.run(cmd);}let entered=Instant::now();ensure!(!cmd.timeout.is_zero()&&cmd.timeout<=BUDGET,"invalid coordinator worker budget");let control=Control{deadline:cmd.deadline.context("coordinator deadline missing")?.min(entered+cmd.timeout),cancellation:cmd.cancellation.clone().context("coordinator cancellation missing")?};let text=cmd.stdin.as_deref().context("coordinator input missing")?;ensure!(text.len()<=64*1024,"coordinator input exceeds bounds");let input:Input=serde_json::from_str(text)?;if input.start {start::execute(&input,&control)?;}else{execute(&input,&control)?;}Ok(Output{code:Some(0),elapsed:entered.elapsed(),..Default::default()})}
    fn socket_request(&self,path:&Path,line:&str,timeout:Duration)->Result<String>{self.inner.socket_request(path,line,timeout)}
}
pub fn request(ctx:&Ctx,p:&Project,c:&Coordinator)->Result<crate::executor::Request>{request_mode(ctx,p,c,false)}
pub fn request_start(ctx:&Ctx,p:&Project,c:&Coordinator)->Result<crate::executor::Request>{request_mode(ctx,p,c,true)}
fn request_mode(ctx:&Ctx,p:&Project,c:&Coordinator,start:bool)->Result<crate::executor::Request>{
    if start {ready_start(c)?;}else{ready(c)?;}let project=p.dir().canonicalize()?;let m=std::fs::metadata(&project)?;let config=std::path::absolute(&ctx.config_dir)?;let socket=PathBuf::from(&c.socket);ensure!(socket.is_absolute(),"coordinator session must be absolute");
    let cfg=paths::read_control_text(&config.join("config.toml"),1024*1024)?;let md=paths::read_control_text(&p.project_md(),1024*1024)?.context("project settings missing")?;
    let input=Input{start,project:project.clone(),identity:(m.dev(),m.ino()),execution:execution(c),request:c.prime_request,sequence:if start{c.launch_sequence}else{c.prime_sequence},socket_identity:socket_identity(&socket)?,socket,herdr:ctx.env.herdr_bin(),config,config_digest:digest(cfg.as_deref()),settings_digest:thread::sha256_hex(md.as_bytes()),prompt:crate::coordinator::priming_prompt(&crate::coordinator::current_prefix(&p.root)?,&p.slug)};
    let text=serde_json::to_string(&input)?;ensure!(text.len()<=64*1024,"coordinator input exceeds bounds");let deadline=Instant::now()+BUDGET;let mut command=Cmd::new(JOB,BUDGET).stdin(text);command.deadline=Some(deadline);
    Ok(crate::executor::Request{identity:crate::executor::Identity{operation:if start{"coordinator-start"}else{"coordinator-prime"}.into(),revision:1,project:project.display().to_string(),machine:format!("brief-root:{}",project.parent().unwrap().display()),terminal:Some(c.pane_id.clone())},lane:crate::executor::Lane::Control,deadline,command})
}
#[cfg(test)]
#[path="coordinator_jobs_tests.rs"]
mod tests;
