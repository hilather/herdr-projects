//! Expiring pane metadata refreshes. Completion is never execution evidence.
use std::{collections::BTreeMap,path::{Path,PathBuf},os::unix::{fs::{MetadataExt,FileTypeExt}},sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Serialize,Deserialize};
use crate::{paths::{self,Ctx},project::{self,Project,Coordinator},thread::{self,Thread},runner::{Cmd,Output,Runner,InheritedLock},source_tree::Control};
use herdr_projects::execution_guard::ProjectGuard;
const JOB:&str="\0herdr-projects-tokens";
const BUDGET:Duration=Duration::from_secs(45);
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    project:PathBuf,identity:(u64,u64),target:Target,socket:PathBuf,socket_identity:(u64,u64),
    herdr:String,config:PathBuf,config_digest:Option<String>,settings_digest:String,remote:Option<Remote>,
}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
enum Target {Thread{id:String,execution:String},Coordinator{execution:String}}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Remote {route:crate::remote_api::Route,selector:String,binary:String}
fn socket_identity(p:&Path)->Result<(u64,u64)>{let m=std::fs::symlink_metadata(p)?;ensure!(m.file_type().is_socket(),"token endpoint is not a socket");Ok((m.dev(),m.ino()))}
fn digest(p:&Path)->Result<Option<String>> {Ok(paths::read_control_text(p,1024*1024)?.map(|s|thread::sha256_hex(s.as_bytes())))}
fn coordinator_execution(c:&Coordinator)->String {thread::sha256_hex(&serde_json::to_vec(&(&c.socket,&c.session,&c.workspace_id,&c.tab_id,&c.pane_id,&c.cwd,&c.agent_name,c.prime_request)).unwrap())}
fn ready(t:&Thread)->Result<()> {ensure!(matches!(t.status,thread::Status::Open|thread::Status::Failed)&&!t.pane_id.is_empty()&&t.removal.is_none()&&t.pending_live_copy.is_none()&&t.pending_final_copy.is_none(),"thread token target is unavailable");Ok(())}
enum Current {Thread(Thread),Coordinator(Coordinator,String)}
impl Current {
    fn pane(&self)->&str {match self {Self::Thread(t)=>&t.pane_id,Self::Coordinator(c,_)=>&c.pane_id}}
    fn matches_pane(&self,p:&crate::herdr::Pane)->bool {match self{Self::Thread(t)=>thread::pane_matches(t,p),Self::Coordinator(c,_)=>crate::coordinator::pane_matches(c,p)}}
    fn matches_agent(&self,a:&crate::herdr::Agent)->bool {match self {Self::Thread(t)=>thread::agent_matches(t,a)&&a.name==t.agent_name&&a.agent==t.agent,Self::Coordinator(c,kind)=>crate::coordinator::agent_matches(c,a)&&&a.agent==kind}}
}
impl Input {
    fn current(&self,p:&Project,g:&ProjectGuard,control:&Control)->Result<Current> {
        control.check()?;ensure!(self.project.is_absolute()&&self.socket.is_absolute()&&self.config.is_absolute()&&!self.herdr.is_empty(),"invalid token worker route");
        g.check_project(&self.project)?;project::ensure_legacy(&self.project)?;let m=std::fs::metadata(&self.project)?;
        ensure!((m.dev(),m.ino())==self.identity&&p.try_status()?==project::Status::Active,"token project authority changed");
        let c=p.try_coordinator()?.context("token session missing")?;
        ensure!(Path::new(&c.socket)==self.socket&&socket_identity(&self.socket)?==self.socket_identity,"token session changed");
        ensure!(digest(&self.config.join("config.toml"))?==self.config_digest,"token configuration changed");
        let md=paths::read_control_text(&p.project_md(),1024*1024)?.context("token project settings missing")?;
        ensure!(thread::sha256_hex(md.as_bytes())==self.settings_digest,"token project settings changed");
        let current=match &self.target {
            Target::Thread{id,execution}=>{thread::validate_id(id)?;let t=thread::load(p,id)?;ready(&t)?;ensure!(thread::execution_fingerprint(&t)==*execution&&self.remote.as_ref().map_or(t.machine.is_empty(),|r|r.selector==t.machine),"token thread execution changed");Current::Thread(t)},
            Target::Coordinator{execution}=>{crate::coordinator_jobs::validate(&c)?;ensure!(self.remote.is_none()&&!c.pane_id.is_empty()&&coordinator_execution(&c)==*execution,"token coordinator execution changed");let(settings,_)=project::parse_project_md(&md).map_err(|_|anyhow::anyhow!("invalid token settings (contents withheld)"))?;Current::Coordinator(c,settings.coordinator_agent)},
        };control.check()?;Ok(current)
    }
    fn route(&self,control:&Control,locks:&[InheritedLock])->Result<()> {
        if let Some(r)=&self.remote {
            let mut cmd=Cmd::new(&self.herdr,crate::remote::SSH_TIMEOUT).args(["machine","list","--json"]);cmd.capture_limit=1024*1024;
            let out=herdr_projects::supervision::run(cmd,control.deadline,control.cancellation.clone(),locks)?;control.check()?;
            let current=crate::remote_api::resolve(&out,&r.selector)?;ensure!(r.route.same_destination(&current),"token remote route changed");
        }Ok(())
    }
    fn command(&self)->Cmd {crate::herdr::Herdr::new(&self.herdr,&self.socket,&crate::runner::RealRunner).cmd(crate::herdr::CALL_TIMEOUT)}
    fn probe(&self,control:&Control,locks:&[InheritedLock])->Result<()> {
        if let Some(r)=&self.remote{return crate::remote_api::probe(&r.route,&r.binary,control,locks);}
        let mut cmd=self.command().args(["remote-api-bridge","--check"]);cmd.capture_limit=1024*1024;
        let out=herdr_projects::supervision::run(cmd,control.deadline,control.cancellation.clone(),locks)?;control.check()?;
        ensure!(out.success()&&out.stdout.trim()=="herdr-api-bridge-v1","token JSON API bridge unavailable");Ok(())
    }
    fn call(&self,method:&str,params:serde_json::Value,control:&Control,locks:&[InheritedLock])->Result<serde_json::Value> {
        control.check()?;let id=format!("tokens-{method}");
        if let Some(r)=&self.remote{return crate::remote_api::request(&r.route,&r.binary,&id,method,params,control,locks);}
        let payload=serde_json::to_string(&serde_json::json!({"id":id,"method":method,"params":params}))?+"\n";ensure!(payload.len()<=64*1024,"token frame exceeds bounds");
        let mut cmd=self.command().arg("remote-api-bridge").stdin(payload);cmd.capture_limit=1024*1024;
        let out=herdr_projects::supervision::run(cmd,control.deadline,control.cancellation.clone(),locks)?;control.check()?;ensure!(out.success(),"token API bridge failed");
        let reply:serde_json::Value=serde_json::from_str(&out.stdout)?;ensure!(reply["id"].as_str()==Some(&id)&&reply.get("error").is_none(),"token API acknowledgement mismatch or rejection");reply.get("result").cloned().context("token API result missing")
    }
}
fn observe(input:&Input,target:&Current,control:&Control,locks:&[InheritedLock])->Result<(String,Vec<crate::herdr::Agent>,Vec<crate::herdr::Pane>)> {
    let agent_result=input.call("agent.list",serde_json::json!({}),control,locks)?;
    let agents:Vec<crate::herdr::Agent>=serde_json::from_value(agent_result["agents"].clone())?;
    let result=input.call("pane.list",serde_json::json!({}),control,locks)?;let values=result["panes"].as_array().context("token pane inventory missing")?;
    ensure!(agents.len()<=4096&&values.len()<=4096,"token observation exceeds resource bounds");
    let matched=values.iter().filter(|p|p["pane_id"].as_str()==Some(target.pane())).collect::<Vec<_>>();ensure!(matched.len()==1,"token pane absent or ambiguous");
    let pane:crate::herdr::Pane=serde_json::from_value(matched[0].clone())?;ensure!(target.matches_pane(&pane),"token pane identity changed");
    let terminal=matched[0]["terminal_id"].as_str().filter(|s|!s.is_empty()&&s.len()<=256).context("token terminal identity missing")?.to_owned();
    for a in agent_result["agents"].as_array().context("token agent inventory missing")?.iter().filter(|a|a["pane_id"].as_str()==Some(target.pane())) {ensure!(a["terminal_id"].as_str()==Some(&terminal),"token agent terminal differs from pane");}
    let matched_agents=agents.iter().filter(|a|a.pane_id==target.pane()).collect::<Vec<_>>();
    ensure!(matched_agents.len()<=1&&matched_agents.iter().all(|a|target.matches_agent(a)),"token agent identity changed or ambiguous");
    ensure!(!matches!(target,Current::Coordinator(..))||matched_agents.len()==1,"coordinator token agent missing");
    let panes=serde_json::from_value(result["panes"].clone())?;Ok((terminal,agents,panes))
}
fn execute(input:&Input,control:&Control)->Result<()> {execute_with(input,control,||Ok(()))}
fn execute_with(input:&Input,control:&Control,before_send:impl FnOnce()->Result<()>)->Result<()> {
    control.check()?;let guard=ProjectGuard::acquire(&input.project)?;let locks=guard.inherit_transfer()?;
    let p=Project::load(input.project.parent().context("token root missing")?,input.project.file_name().and_then(|s|s.to_str()).context("token project invalid")?)?;
    let target=input.current(&p,&guard,control)?;
    match &target {Current::Thread(t)=>crate::brief_jobs::ownership::check(&p,t,&input.socket,control)?,Current::Coordinator(c,_)=>crate::brief_jobs::ownership::check_coordinator(&p,&c.pane_id,&input.socket,control)?,}
    input.route(control,&locks)?;input.probe(control,&locks)?;
    let(terminal,agents,panes)=observe(input,&target,control,&locks)?;
    before_send()?;input.route(control,&locks)?;let target=input.current(&p,&guard,control)?;
    let tokens:BTreeMap<String,String>=match &target {
        Current::Thread(t)=>{let now=jiff::Timestamp::now();let mut live=thread::live_state(t,&agents,&panes,now);if t.is_remote()&&live.agent_state.as_deref()==Some("blocked"){live.state_secs=live.state_secs.max(thread::BLOCKED_DEBOUNCE_SECS);}crate::threads::thread_tokens(t,&p.slug,thread::group(t,&live,now)).into_iter().collect()},
        Current::Coordinator(..)=>[("project".into(),p.slug.clone()),("thread".into(),"coordinator".into()),("rank".into(),"0".into())].into_iter().collect(),
    };
    let result=input.call("pane.report_metadata",serde_json::json!({"pane_id":target.pane(),"source":crate::herdr::SOURCE,"ttl_ms":crate::coordinator::TOKEN_TTL.as_millis() as u64,"tokens":tokens}),control,&locks)?;
    // Native `ok` also covers an ignored update. Never publish durable success
    // or infer execution/readiness from it. Expiration bounds stale decoration.
    ensure!(result["type"].as_str()==Some("ok"),"token response has wrong type");
    input.route(control,&locks)?;let target=input.current(&p,&guard,control)?;
    ensure!(observe(input,&target,control,&locks)?.0==terminal,"token terminal changed during refresh");Ok(())
}
pub struct JobRunner {pub inner:Arc<dyn Runner+Send+Sync>}
impl Runner for JobRunner {
    fn run(&self,cmd:&Cmd)->Result<Output>{if cmd.program!=JOB{return self.inner.run(cmd);}let entered=Instant::now();ensure!(!cmd.timeout.is_zero()&&cmd.timeout<=BUDGET,"invalid token budget");let control=Control{deadline:cmd.deadline.context("token deadline missing")?.min(entered+cmd.timeout),cancellation:cmd.cancellation.clone().context("token cancellation missing")?};let text=cmd.stdin.as_deref().context("token input missing")?;ensure!(text.len()<=64*1024,"token input exceeds bounds");execute(&serde_json::from_str(text)?,&control)?;Ok(Output{code:Some(0),elapsed:entered.elapsed(),..Default::default()})}
    fn socket_request(&self,p:&Path,s:&str,t:Duration)->Result<String>{self.inner.socket_request(p,s,t)}
}
pub fn request(ctx:&Ctx,p:&Project,t:Option<&Thread>,route:Option<&crate::remote_api::Route>)->Result<crate::executor::Request> {
    let c=p.try_coordinator()?.context("token coordinator missing")?;let(target,pane,remote)=if let Some(t)=t {ready(t)?;ensure!(t.is_remote()==route.is_some(),"token route kind mismatch");(Target::Thread{id:t.id.clone(),execution:thread::execution_fingerprint(t)},t.pane_id.clone(),route.map(|route|Remote{route:route.clone(),selector:t.machine.clone(),binary:ctx.env.var("HERDR_PROJECTS_REMOTE_HERDR_BIN").unwrap_or("herdr").into()}))}else{ensure!(route.is_none()&&!c.pane_id.is_empty(),"invalid coordinator token route");(Target::Coordinator{execution:coordinator_execution(&c)},c.pane_id.clone(),None)};
    let project=p.dir().canonicalize()?;let m=std::fs::metadata(&project)?;let socket=PathBuf::from(&c.socket);ensure!(socket.is_absolute(),"token session must be absolute");let config=std::path::absolute(&ctx.config_dir)?;
    let input=Input{project:project.clone(),identity:(m.dev(),m.ino()),target,socket_identity:socket_identity(&socket)?,socket,herdr:ctx.env.herdr_bin(),config_digest:digest(&config.join("config.toml"))?,config,settings_digest:digest(&p.project_md())?.context("token project settings missing")?,remote};
    let text=serde_json::to_string(&input)?;ensure!(text.len()<=64*1024,"token input exceeds bounds");let deadline=Instant::now()+BUDGET;let mut command=Cmd::new(JOB,BUDGET).stdin(text);command.deadline=Some(deadline);
    Ok(crate::executor::Request{identity:crate::executor::Identity{operation:format!("tokens:{}",t.map_or("coordinator",|t|&t.id)),revision:1,project:project.display().to_string(),machine:format!("brief-root:{}",project.parent().unwrap().display()),terminal:Some(pane)},lane:crate::executor::Lane::Control,deadline,command})
}
#[cfg(all(test,target_os="linux"))]
#[path="token_jobs_tests.rs"]
mod tests;
