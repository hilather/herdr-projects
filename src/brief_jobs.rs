//! Concrete supervised brief ingress. Queue output never certifies delivery.
use std::{path::{Path,PathBuf},os::unix::fs::MetadataExt,sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Serialize,Deserialize};
use crate::{executor::{Identity,Lane,Request},paths::{self,Ctx},project::{self,Project},runner::{Cmd,Output,Runner,InheritedLock},source_tree::Control,thread::{self,Thread}};
use herdr_projects::execution_guard::ProjectGuard;
#[path="brief_jobs_ownership.rs"]
mod ownership;
const JOB:&str="\0herdr-projects-brief";
const BUDGET:Duration=Duration::from_secs(45);
const INPUT_LIMIT:usize=64*1024;
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    project:PathBuf,identity:(u64,u64),id:String,execution:String,sequence:u64,
    socket:PathBuf,socket_identity:(u64,u64),herdr:String,config:PathBuf,config_digest:Option<String>,
}
fn digest(config:&Path)->Result<Option<String>> {Ok(paths::read_control_text(&config.join("config.toml"),1024*1024)?.map(|s|thread::sha256_hex(s.as_bytes())))}
fn socket(project:&Project)->Result<PathBuf> {
    let text=paths::read_control_text(&project.state_dir().join("coordinator.json"),1024*1024)?.context("brief session missing")?;
    let record:project::Coordinator=serde_json::from_str(&text)?;
    let path=PathBuf::from(record.socket);ensure!(path.is_absolute(),"brief session must be absolute");Ok(path)
}
fn socket_identity(path:&Path)->Result<(u64,u64)> {
    use std::os::unix::fs::FileTypeExt;
    let m=std::fs::symlink_metadata(path)?;ensure!(m.file_type().is_socket(),"brief endpoint is not a socket");Ok((m.dev(),m.ino()))
}
impl Input {
    fn validate(&self)->Result<()> {
        ensure!(self.project.is_absolute()&&self.config.is_absolute()&&self.socket.is_absolute()&&!self.herdr.is_empty(),"invalid brief route");thread::validate_id(&self.id)?;
        ensure!(self.execution.len()==64&&self.execution.bytes().all(|b|b.is_ascii_hexdigit())&&self.sequence<i64::MAX as u64,"invalid brief execution");Ok(())
    }
    fn current(&self,project:&Project,guard:&ProjectGuard,control:&Control)->Result<Thread> {
        control.check()?;guard.check_project(&self.project)?;project::ensure_legacy(&self.project)?;
        let m=std::fs::metadata(&self.project)?;ensure!((m.dev(),m.ino())==self.identity,"brief project identity changed");
        ensure!(project.try_status()?==project::Status::Active,"brief project is not active");
        ensure!(socket(project)?==self.socket&&socket_identity(&self.socket)?==self.socket_identity,"brief session changed");
        ensure!(digest(&self.config)?==self.config_digest,"brief configuration changed");
        let t=thread::load(project,&self.id)?;
        ensure!(t.machine.is_empty()&&thread::execution_fingerprint(&t)==self.execution&&t.prompt_sequence==self.sequence,"brief execution changed");thread::prompt_delivery::ready(&t)?;control.check()?;Ok(t)
    }
}
fn run(mut cmd:Cmd,control:&Control,locks:&[InheritedLock])->Result<serde_json::Value> {
    control.check()?;cmd.capture_limit=1024*1024;
    let out=herdr_projects::supervision::run(cmd,control.deadline,control.cancellation.clone(),locks)?;
    control.check()?;ensure!(out.success(),"supervised brief command failed");
    let reply:serde_json::Value=serde_json::from_str(&out.stdout)?;
    ensure!(reply.get("error").is_none()&&reply.get("result").is_some(),"brief command did not return success");Ok(reply["result"].clone())
}
fn execute(input:&Input,control:&Control)->Result<()> {execute_with(input,control,||Ok(()))}
fn execute_with(input:&Input,control:&Control,after_claim:impl FnOnce()->Result<()>)->Result<()> {
    input.validate()?;control.check()?;let guard=ProjectGuard::acquire(&input.project)?;let locks=guard.inherit_transfer()?;
    let project=Project::load(input.project.parent().context("brief project root missing")?,input.project.file_name().and_then(|s|s.to_str()).context("invalid brief project name")?)?;
    let t=input.current(&project,&guard,control)?;
    ownership::check(&project,&t,&input.socket,control)?;
    let herdr=crate::herdr::Herdr::new(&input.herdr,&input.socket,&crate::runner::RealRunner);
    let agents:Vec<crate::herdr::Agent>=serde_json::from_value(run(herdr.cmd(crate::herdr::CALL_TIMEOUT).args(["agent","list"]),control,&locks)?["agents"].clone())?;
    let panes:Vec<crate::herdr::Pane>=serde_json::from_value(run(herdr.cmd(crate::herdr::CALL_TIMEOUT).args(["pane","list"]),control,&locks)?["panes"].clone())?;
    let agents:Vec<_>=agents.iter().filter(|a|a.pane_id==t.pane_id).collect();let panes:Vec<_>=panes.iter().filter(|p|p.pane_id==t.pane_id).collect();
    ensure!(!t.pane_id.is_empty()&&agents.len()==1&&panes.len()==1&&thread::agent_matches(&t,agents[0])&&agents[0].name==t.agent_name&&agents[0].agent==t.agent&&agents[0].ready()&&thread::pane_matches(&t,panes[0]),"brief target is absent, changed, ambiguous or busy");
    let t=input.current(&project,&guard,control)?;
    let claim=thread::prompt_delivery::claim(&project,&guard,&t,&thread::launch_prompt(&project.slug,&t.id),control)?;
    after_claim()?;
    // A failure from this point remains uncertain, including failure before spawn.
    control.check()?;guard.check_project(&input.project)?;
    ensure!(socket(&project)?==input.socket&&socket_identity(&input.socket)?==input.socket_identity&&digest(&input.config)?==input.config_digest&&project.try_status()?==project::Status::Active,"brief authority changed after claim");
    let current=thread::load(&project,&t.id)?;ensure!(thread::execution_fingerprint(&current)==input.execution&&current.prompt_claim.as_ref()==Some(&claim)&&current.status==thread::Status::Open&&current.prompt_pending&&current.removal.is_none()&&current.pending_live_copy.is_none()&&current.pending_final_copy.is_none(),"brief claim changed before send");
    let result=run(herdr.cmd(crate::herdr::CALL_TIMEOUT).args(["agent","prompt",&t.pane_id,&claim.prompt]),control,&locks)?;
    ensure!(result["type"].as_str()==Some("agent_prompted"),"brief acknowledgement has wrong type");
    let agent:crate::herdr::Agent=serde_json::from_value(result["agent"].clone())?;
    ensure!(thread::agent_matches(&t,&agent)&&agent.name==t.agent_name&&agent.agent==t.agent,"brief acknowledgement names another execution");
    ensure!(socket(&project)?==input.socket&&socket_identity(&input.socket)?==input.socket_identity,"brief session changed during send");
    thread::prompt_delivery::confirm(&project,&guard,&t.id,&claim,control)
}
pub struct JobRunner {pub inner:Arc<dyn Runner+Send+Sync>}
impl Runner for JobRunner {
    fn run(&self,cmd:&Cmd)->Result<Output> {
        if cmd.program!=JOB {return self.inner.run(cmd);}
        let entered=Instant::now();ensure!(!cmd.timeout.is_zero()&&cmd.timeout<=BUDGET,"invalid brief queue budget");
        let control=Control{deadline:cmd.deadline.context("brief deadline missing")?.min(entered+cmd.timeout),cancellation:cmd.cancellation.clone().context("brief cancellation missing")?};
        let text=cmd.stdin.as_deref().context("brief input missing")?;ensure!(text.len()<=INPUT_LIMIT,"brief input exceeds bounds");execute(&serde_json::from_str(text)?,&control)?;
        Ok(Output{code:Some(0),elapsed:entered.elapsed(),..Output::default()})
    }
    fn socket_request(&self,socket:&Path,line:&str,timeout:Duration)->Result<String>{self.inner.socket_request(socket,line,timeout)}
}
pub fn request(ctx:&Ctx,project:&Project,t:&Thread)->Result<Request> {
    thread::prompt_delivery::ready(t)?;ensure!(!t.is_remote(),"supervised remote briefs are not supported yet");
    let path=project.dir().canonicalize()?;let m=std::fs::metadata(&path)?;let socket=socket(project)?;let config=std::path::absolute(&ctx.config_dir)?;
    let input=Input{project:path.clone(),identity:(m.dev(),m.ino()),id:t.id.clone(),execution:thread::execution_fingerprint(t),sequence:t.prompt_sequence,socket_identity:socket_identity(&socket)?,socket,herdr:ctx.env.herdr_bin(),config_digest:digest(&config)?,config};input.validate()?;
    let text=serde_json::to_string(&input)?;ensure!(text.len()<=INPUT_LIMIT,"brief input exceeds bounds");
    let identity=Identity{operation:format!("brief:{}",t.id),revision:1,project:path.to_str().context("brief project is not UTF-8")?.into(),machine:format!("brief-root:{}",path.parent().unwrap().display()),terminal:Some(t.pane_id.clone())};
    let deadline=Instant::now()+BUDGET;let mut command=Cmd::new(JOB,BUDGET).stdin(text);command.deadline=Some(deadline);Ok(Request{identity,lane:Lane::Control,deadline,command})
}

#[cfg(all(test,target_os="linux"))]
mod tests {
    use super::*;
    use std::{fs,os::unix::{fs::PermissionsExt,net::UnixListener}};
    struct Fixture {root:tempfile::TempDir,project:Project,t:Thread,input:Input,_listener:UnixListener}
    impl Fixture {
        fn new()->Self {
            let root=tempfile::tempdir().unwrap();let project=project::create(root.path(),"demo","",vec![]).unwrap();
            let socket=root.path().join("session.sock");let listener=UnixListener::bind(&socket).unwrap();
            let record=project::Coordinator{socket:socket.display().to_string(),..Default::default()};project::write_json(&project.state_dir().join("coordinator.json"),&record).unwrap();
            let t=thread::allocate(&project,|t|{t.status=thread::Status::Open;t.prompt_pending=true;t.workspace_id="w".into();t.tab_id="tab".into();t.pane_id="p".into();t.cwd="/fixture".into();t.agent_name="worker".into();t.agent="claude".into();}).unwrap();
            let helper=root.path().join("herdr");let env=paths::Env::for_test(root.path(),&[("HERDR_BIN_PATH",helper.to_str().unwrap())]);let runner=crate::runner::RealRunner;
            let ctx=Ctx{env:&env,root:root.path().into(),config_dir:root.path().join("cfg"),runner:&runner,detached_ticker:false};let request=request(&ctx,&project,&t).unwrap();
            let input=serde_json::from_str(request.command.stdin.as_ref().unwrap()).unwrap();let fixture=Self{root,project,t,input,_listener:listener};fixture.script("idle",false,"confirmed");fixture
        }
        fn script(&self,status:&str,duplicate:bool,outcome:&str) {
            let agent=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":"/fixture","name":"worker","agent":"claude","agent_status":status});
            let agents=if duplicate {vec![agent.clone(),agent.clone()]}else{vec![agent.clone()]};
            let list=serde_json::json!({"result":{"agents":agents}}).to_string();let panes=serde_json::json!({"result":{"panes":[{"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":"/fixture"}]}}).to_string();
            let mut acknowledgement=serde_json::json!({"result":{"type":"agent_prompted","agent":agent}});
            if outcome=="foreign" {acknowledgement["result"]["agent"]["workspace_id"]="foreign".into();}
            let response=match outcome {"lost"=>"exit 1".into(),"blocked"=>"/bin/sleep 60".into(),"invalid"=>"printf '{}\\n'".into(),_=>format!("printf '%s\\n' {}",crate::remote::quote(&acknowledgement.to_string()))};
            let body=format!("#!/bin/sh\ncase \"$1:$2\" in\nagent:list) printf '%s\\n' {};;\npane:list) printf '%s\\n' {};;\nagent:prompt) printf send >> {}; {response};;\n*) exit 2;;\nesac\n",crate::remote::quote(&list),crate::remote::quote(&panes),crate::remote::quote(self.root.path().join("sent").to_str().unwrap()));
            fs::write(&self.input.herdr,body).unwrap();fs::set_permissions(&self.input.herdr,fs::Permissions::from_mode(0o700)).unwrap();
        }
        fn recover(&self) {let guard=ProjectGuard::acquire(&self.project.dir()).unwrap();thread::prompt_delivery::recover(&self.project,&guard).unwrap();}
        fn sent(&self)->bool {self.root.path().join("sent").exists()}
    }
    #[test]
    fn concrete_sender_confirms_matching_agent_and_never_replays() {
        let f=Fixture::new();execute(&f.input,&Control::default()).unwrap();assert!(f.sent());
        let t=thread::load(&f.project,&f.t.id).unwrap();assert!(!t.prompt_pending);assert_eq!(t.prompt_claim.unwrap().phase,thread::prompt_delivery::Phase::Confirmed);
        assert!(execute(&f.input,&Control::default()).is_err());assert_eq!(fs::read(f.root.path().join("sent")).unwrap(),b"send");
    }
    #[test]
    fn lost_malformed_and_foreign_acknowledgements_remain_uncertain() {
        for outcome in ["lost","invalid","foreign"] {
            let f=Fixture::new();f.script("idle",false,outcome);assert!(execute(&f.input,&Control::default()).is_err());assert!(f.sent());
            f.recover();let t=thread::load(&f.project,&f.t.id).unwrap();assert_eq!(t.status,thread::Status::Failed);assert!(t.prompt_pending);assert!(execute(&f.input,&Control::default()).is_err());assert_eq!(fs::read(f.root.path().join("sent")).unwrap(),b"send");
        }
    }
    #[test]
    fn busy_ambiguous_stale_and_replaced_socket_targets_do_not_claim() {
        for variant in ["busy","duplicate","stale","socket"] {
            let f=Fixture::new();match variant {
                "busy"=>f.script("working",false,"confirmed"),"duplicate"=>f.script("idle",true,"confirmed"),
                "stale"=>{thread::update(&f.project,&f.t.id,|t|t.lifecycle_generation+=1).unwrap();},
                _=>{fs::remove_file(&f.input.socket).unwrap();fs::write(&f.input.socket,b"replacement").unwrap();},
            }
            assert!(execute(&f.input,&Control::default()).is_err());assert!(!f.sent());assert!(thread::load(&f.project,&f.t.id).unwrap().prompt_claim.is_none());
        }
    }
    #[test]
    fn cancellation_after_claim_leaves_recovery_and_no_effect() {
        let f=Fixture::new();let control=Control::default();assert!(execute_with(&f.input,&control,||{control.cancellation.cancel();Ok(())}).is_err());
        assert!(!f.sent());f.recover();assert_eq!(thread::load(&f.project,&f.t.id).unwrap().status,thread::Status::Failed);
    }
    #[test]
    fn conflicting_aliases_corrupt_neighbors_and_canonical_references_refuse() {
        for variant in ["thread","coordinator","alias","corrupt","canonical"] {
            let f=Fixture::new();let other=project::create(f.root.path(),"other","",vec![]).unwrap();
            let mut socket=f.input.socket.clone();if variant=="alias" {socket=f.root.path().join("alias.sock");std::os::unix::fs::symlink(&f.input.socket,&socket).unwrap();}
            other.update_coordinator(|c|{c.socket=socket.display().to_string();if variant!="thread"&&variant!="corrupt" {c.pane_id="p".into();}}).unwrap();
            if variant=="thread" {thread::allocate(&other,|t|{t.status=thread::Status::Resolved;t.pane_id="p".into();}).unwrap();}
            if variant=="corrupt" {fs::write(other.dir().join("threads/t-0001.toml"),"corrupt [").unwrap();}
            if variant=="canonical" {
                #[cfg(feature="state-store")]
                {other.set_status(project::Status::Paused).unwrap();project::write_json(&other.state_dir().join("coordinator.json"),&project::Coordinator::default()).unwrap();let plan=herdr_projects::migration::inspect(&other.dir()).unwrap();herdr_projects::migration::apply(&other.dir(),&plan,true).unwrap();let snapshot=herdr_projects::runtime::snapshot(&other.dir()).unwrap();herdr_projects::runtime::rebind(&other.dir(),"coordinator",1,snapshot.head,&herdr_projects::domain::RuntimeRoute{socket:socket.display().to_string(),pane_id:"p".into(),workspace_id:"w".into(),tab_id:"tab".into(),cwd:"/fixture".into(),..Default::default()}).unwrap();}
                #[cfg(not(feature="state-store"))]
                fs::write(other.state_dir().join("format.json"),"{}").unwrap();
            }
            assert!(execute(&f.input,&Control::default()).is_err(),"{variant}");assert!(!f.sent());assert!(thread::load(&f.project,&f.t.id).unwrap().prompt_claim.is_none());
        }
    }
    #[test]
    fn ticker_defers_send_and_restart_confirms_or_recovers_without_replay() {
        for outcome in ["confirmed","lost"] {
            let f=Fixture::new();f.script("idle",false,outcome);
            let env=paths::Env::for_test(f.root.path(),&[("HERDR_BIN_PATH",&f.input.herdr)]);let runner=crate::runner::RealRunner;
            let ctx=Ctx{env:&env,root:f.root.path().into(),config_dir:f.root.path().join("cfg"),runner:&runner,detached_ticker:false};
            let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(JobRunner{inner:Arc::new(crate::runner::RealRunner)})).unwrap());
            let mut memory=crate::steps::Memory::new(&ctx);memory.copy_jobs=Some(crate::copy_jobs::Queue::new(pool.clone()));
            crate::ticker::tick_project_with(&ctx,&f.project,&mut memory).unwrap();assert!(!f.sent());assert!(memory.copy_jobs.as_ref().unwrap().offered());
            assert!(memory.copy_jobs.as_mut().unwrap().admit().is_empty());
            let end=Instant::now()+Duration::from_secs(10);
            while memory.copy_jobs.as_ref().unwrap().pending() {assert!(Instant::now()<end);memory.copy_jobs.as_mut().unwrap().drain();std::thread::sleep(Duration::from_millis(5));}
            assert!(f.sent());assert!(pool.stop(Duration::from_secs(2)));drop(memory);
            let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(JobRunner{inner:Arc::new(crate::runner::RealRunner)})).unwrap());
            let mut memory=crate::steps::Memory::new(&ctx);memory.copy_jobs=Some(crate::copy_jobs::Queue::new(pool.clone()));
            crate::ticker::tick_for_test(&ctx,&mut memory);let t=thread::load(&f.project,&f.t.id).unwrap();
            if outcome=="confirmed" {assert!(!t.prompt_pending);assert_eq!(t.prompt_claim.unwrap().phase,thread::prompt_delivery::Phase::Confirmed);}
            else {assert_eq!(t.status,thread::Status::Failed);assert!(t.prompt_claim.unwrap().notified);assert_eq!(crate::inbox::unhandled(&f.project).iter().filter(|i|i.id.starts_with("brief-")).count(),1);}
            assert!(pool.stop(Duration::from_secs(2)));assert_eq!(fs::read(f.root.path().join("sent")).unwrap(),b"send");
        }
    }

    #[test]
    fn blocked_sender_preserves_exclusion_while_neighbor_observation_progresses() {
        let f=Fixture::new();f.script("idle",false,"blocked");
        let other=project::create(f.root.path(),"neighbor","",vec![]).unwrap();other.update_coordinator(|c|c.socket=f.input.socket.display().to_string()).unwrap();
        let neighbor=thread::allocate(&other,|t|{t.status=thread::Status::Open;t.pane_id="neighbor-pane".into();t.last_state="working".into();t.last_group="working".into();}).unwrap();
        let env=paths::Env::for_test(f.root.path(),&[("HERDR_BIN_PATH",&f.input.herdr)]);let runner=crate::runner::RealRunner;let ctx=Ctx{env:&env,root:f.root.path().into(),config_dir:f.root.path().join("cfg"),runner:&runner,detached_ticker:false};
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(JobRunner{inner:Arc::new(crate::runner::RealRunner)})).unwrap());let ticket=pool.submit(request(&ctx,&f.project,&f.t).unwrap()).unwrap();
        let end=Instant::now()+Duration::from_secs(5);while !f.sent(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}
        assert!(ProjectGuard::acquire(&f.project.dir()).is_err());assert!(crate::cleanup::lease(f.root.path()).is_err());
        let mut memory=crate::steps::Memory::new(&ctx);let _=crate::ticker::tick_project_with(&ctx,&other,&mut memory);
        assert_eq!(thread::load(&other,&neighbor.id).unwrap().last_state,"");
        ticket.cancel();assert!(pool.stop(Duration::from_secs(5)));f.recover();assert_eq!(thread::load(&f.project,&f.t.id).unwrap().status,thread::Status::Failed);assert_eq!(fs::read(f.root.path().join("sent")).unwrap(),b"send");
    }

}
