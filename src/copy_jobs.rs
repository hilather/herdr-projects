//! Trusted live-copy ingress. Queue completions never certify publication.
use std::{path::{Path,PathBuf},os::unix::fs::MetadataExt,sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Serialize,Deserialize};
use crate::{artifacts::live,executor::{Identity,Lane,Request},paths::{self,Ctx},project::{self,Project},remote,runner::{Cmd,Output,Runner,InheritedLock},source_tree::Control,thread::{self,Thread}};
use herdr_projects::{copy_receipt::CopyReceipt,execution_guard::ProjectGuard,live_copy_intent::LiveCopyIntent,final_copy_intent::{FinalCopyIntent,Purpose}};
const JOB:&str="\0herdr-projects-live-copy";
const BUDGET:Duration=Duration::from_secs(180);
const INPUT_LIMIT:usize=64*1024;
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Finalization {
    sequence:u64,pending:Option<FinalCopyIntent>,purpose:Purpose,operation:String,socket:Option<String>,
}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    project:PathBuf,project_identity:(u64,u64),id:String,execution:String,
    previous_hash:String,previous_receipt:Option<CopyReceipt>,pending:Option<LiveCopyIntent>,sequence:u64,
    finalization:Option<Finalization>,
    config:PathBuf,config_digest:Option<String>,herdr:String,helper:String,machine:String,target:Option<String>,
}
fn config(path:&Path)->Result<(Option<String>,Option<String>)> {
    let text=paths::read_root_config(path)?;
    ensure!(text.as_ref().is_none_or(|s|s.len()<=1024*1024),"copy configuration exceeds bounds");
    let digest=text.as_ref().map(|s|thread::sha256_hex(s.as_bytes()));Ok((text,digest))
}
impl Input {
    fn validate(&self)->Result<()> {
        ensure!(self.project.is_absolute()&&self.config.is_absolute(),"copy paths must be absolute");
        thread::validate_id(&self.id)?;
        ensure!(!self.herdr.is_empty()&&!self.helper.is_empty()&&(if self.machine.is_empty(){self.target.is_none()}else{self.target.is_some()||self.pending.is_some()||self.finalization.as_ref().is_some_and(|f|f.pending.is_some())}),"invalid live-copy route");
        ensure!(self.execution.len()==64&&self.execution.bytes().all(|b|b.is_ascii_hexdigit()),"invalid live-copy execution");
        if let Some(receipt)=&self.previous_receipt {receipt.validate()?;}
        if let Some(intent)=&self.pending {intent.validate()?;}
        if let Some(finalization)=&self.finalization {
            ensure!(self.pending.is_none(),"live and final copy cannot share an intent");
            finalization.purpose.validate()?;
            ensure!(!matches!(finalization.purpose,Purpose::Idle{..})||finalization.socket.as_ref().is_some_and(|s|!s.is_empty()&&s.len()<=4096),"idle finalization requires a recorded session");
            ensure!((finalization.sequence<i64::MAX as u64||(finalization.pending.is_some()&&finalization.sequence==i64::MAX as u64))&&!finalization.operation.is_empty()&&finalization.operation.len()<=256&&!finalization.operation.chars().any(char::is_control),"invalid final-copy request");
            if let Some(intent)=&finalization.pending {
                intent.validate()?;
                ensure!(intent.sequence==finalization.sequence&&intent.purpose==finalization.purpose&&intent.operation==finalization.operation&&intent.execution==self.execution,"final-copy request disagrees with retained intent");
            }
        }
        Ok(())
    }
    fn authority(&self)->Result<String> {
        let route=thread::sha256_hex(&serde_json::to_vec(&(&self.project,self.project_identity,&self.config,&self.config_digest,&self.herdr,&self.helper,&self.machine,&self.target))?);
        match &self.finalization {Some(f)=>Ok(thread::sha256_hex(&serde_json::to_vec(&(route,&f.socket))?)),None=>Ok(route)}
    }
    fn current(&self,project:&Project,guard:&ProjectGuard,control:&Control)->Result<Thread> {
        control.check()?;guard.check_project(&self.project)?;
        let metadata=std::fs::metadata(&self.project)?;
        ensure!((metadata.dev(),metadata.ino())==self.project_identity,"copy project identity changed");
        project::ensure_legacy(&self.project)?;
        ensure!(project.try_status()?==project::Status::Active,"copy project is not active");
        let current=thread::load(project,&self.id)?;thread::copy_delivery::validate(&current)?;
        ensure!(current.status==thread::Status::Open&&current.removal.is_none()
            &&thread::execution_fingerprint(&current)==self.execution&&current.machine==self.machine
            &&current.report_hash==self.previous_hash&&current.copy_receipt==self.previous_receipt
            &&current.pending_live_copy==self.pending&&current.live_copy_sequence==self.sequence,"queued live-copy execution changed");
        ensure!(current.pending_copy_notice.is_none()&&current.pending_review_notice.is_none(),"pending notice blocks live copy");
        ensure!(current.pending_final_notice.is_none(),"pending final-copy notice blocks copy");
        match &self.finalization {
            Some(finalization)=>ensure!(current.pending_final_copy==finalization.pending&&current.final_copy_sequence==finalization.sequence,"queued final-copy intent changed"),
            None=>ensure!(current.pending_final_copy.is_none(),"recover pending final copy first"),
        }
        Ok(current)
    }
    fn configuration(&self)->Result<Option<String>> {
        let (bytes,digest)=config(&self.config)?;ensure!(digest==self.config_digest,"live-copy configuration changed");Ok(bytes)
    }
    fn resolve_target(&self,control:&Control,locks:&[InheritedLock])->Result<String> {
        control.check()?;let bytes=self.configuration()?;
        let listed=run(Cmd::new(&self.herdr,remote::SSH_TIMEOUT).args(["machine","list","--json"]),control,locks).ok();
        // Cancellation/deadline never become permission to use fallback routing.
        control.check()?;
        let target=remote::target_from_listing(listed,||bytes.as_ref().and_then(|s|remote::configured_target_bytes(s.as_bytes(),&self.machine)),&self.machine)?;
        self.configuration()?;control.check()?;Ok(target)
    }
    fn authorize(&self,control:&Control,locks:&[InheritedLock])->Result<()> {
        control.check()?;self.configuration()?;
        if !self.machine.is_empty() {
            ensure!(self.target.as_ref()==Some(&self.resolve_target(control,locks)?),"live-copy machine route changed");
        }
        control.check()
    }

}
fn idle_observation(current:&Thread,agents:&[crate::herdr::Agent],panes:&[crate::herdr::Pane])->bool {
    let occupants:Vec<_>=agents.iter().filter(|agent|agent.pane_id==current.pane_id).collect();
    let locations:Vec<_>=panes.iter().filter(|pane|pane.pane_id==current.pane_id).collect();
    if occupants.len()>1||locations.len()>1||occupants.iter().any(|agent|!thread::agent_matches(current,agent)||!agent.ready())||locations.iter().any(|pane|!thread::pane_matches(current,pane)) {return false;}
    let now=jiff::Timestamp::now();let live=thread::live_state(current,agents,panes,now);
    live.agent_state.as_deref().unwrap_or("")==current.last_state&&thread::group(current,&live,now)==thread::Group::Idle
}
fn session(project:&Project)->Result<String> {
    let text=paths::read_control_text(&project.state_dir().join("coordinator.json"),1024*1024)?.context("final-copy session record missing")?;
    let record:project::Coordinator=serde_json::from_str(&text)?;ensure!(!record.socket.is_empty(),"final-copy session missing");Ok(record.socket)
}
impl Input {
    fn final_authorize(&self,project:&Project,control:&Control,locks:&[InheritedLock])->Result<bool> {
        self.authorize(control,locks)?;
        let finalization=self.finalization.as_ref().context("final-copy purpose missing")?;
        if !matches!(finalization.purpose,Purpose::Idle{..}) {return Ok(true);}
        let socket=finalization.socket.as_ref().context("idle session missing")?;
        ensure!(session(project)?==*socket,"idle final-copy session changed");
        // Project ownership fences record mutations, not a live agent resuming
        // independently. Observe the recorded session through concrete supervision.
        let observation=(||->Result<bool>{
            let base=crate::herdr::Herdr::new(&self.herdr,socket,&crate::runner::RealRunner);
            let herdr=base.on_machine(&self.machine);
            let agents=run(herdr.cmd(crate::herdr::CALL_TIMEOUT).args(["agent","list"]),control,locks)?;
            let panes=run(herdr.cmd(crate::herdr::CALL_TIMEOUT).args(["pane","list"]),control,locks)?;
            let agents:serde_json::Value=serde_json::from_str(&agents.stdout)?;
            let panes:serde_json::Value=serde_json::from_str(&panes.stdout)?;
            ensure!(agents.get("error").is_none()&&panes.get("error").is_none(),"idle observation failed");
            let agents:Vec<crate::herdr::Agent>=serde_json::from_value(agents["result"]["agents"].clone())?;
            let panes:Vec<crate::herdr::Pane>=serde_json::from_value(panes["result"]["panes"].clone())?;
            let current=thread::load(project,&self.id)?;
            ensure!(thread::execution_fingerprint(&current)==self.execution,"idle execution changed");
            Ok(idle_observation(&current,&agents,&panes))
        })();
        // A failed observation denies resolution but permits retained copy recovery.
        // Cancellation and changed route/session still withdraw effect authority.
        control.check()?;self.authorize(control,locks)?;ensure!(session(project)?==*socket,"idle final-copy session changed");
        Ok(observation.unwrap_or(false))
    }
}
fn run(command:Cmd,control:&Control,locks:&[InheritedLock])->Result<Output> {
    control.check()?;
    let output=herdr_projects::supervision::run(command,control.deadline,control.cancellation.clone(),locks)?;
    control.check()?;ensure!(output.success(),"supervised live-copy command failed");Ok(output)
}
fn supports_live(output:&Output)->Result<()> {
    let value:serde_json::Value=serde_json::from_str(&output.stdout).context("invalid live-copy capability response")?;
    ensure!(value.get("live_versions").and_then(|v|v.as_array()).is_some_and(|v|v.iter().any(|n|n.as_u64()==Some(1))),"remote helper does not support live-copy protocol 1");Ok(())
}
struct Helpers {local:String,ssh:String}
impl Helpers {
    fn production()->Result<Self> {Ok(Self{local:std::env::current_exe()?.to_str().context("copy executable path is not UTF-8")?.into(),ssh:"ssh".into()})}
    fn ssh(&self,target:&str,script:&str,timeout:Duration)->Result<Cmd> {
        let mut cmd=remote::ssh_command(target,script,timeout)?;cmd.program=self.ssh.clone();Ok(cmd)
    }
}
fn execute(input:&Input,control:&Control,helpers:&Helpers)->Result<()> {
    input.validate()?;control.check()?;
    ensure!(input.project.canonicalize()?==input.project,"copy project path changed");
    let guard=ProjectGuard::acquire(&input.project)?;let locks=guard.inherit_transfer()?;
    let project=Project::load(input.project.parent().context("copy project root missing")?,input.project.file_name().and_then(|s|s.to_str()).context("invalid copy project name")?)?;
    let expected=input.current(&project,&guard,control)?;
    let resolved;
    let input=if !input.machine.is_empty()&&input.target.is_none() {
        ensure!(input.pending.is_some()||input.finalization.as_ref().is_some_and(|f|f.pending.is_some()),"unobserved route requires retained recovery intent");
        resolved={let mut value=input.clone();value.target=Some(input.resolve_target(control,&locks)?);value};&resolved
    }else{input};
    let authority=input.authority()?;input.authorize(control,&locks)?;
    if let Some(intent)=&input.pending {
        ensure!(intent.authority==authority,"retained live-copy authority changed");
        // Recovery uses only the retained stage. No capability probe or fetch.
        return live::projection::resume_controlled(&project,&guard,&input.id,&authority,control,||input.authorize(control,&locks));
    }
    if let Some(finalization)=&input.finalization {
        if let Some(intent)=&finalization.pending {
            ensure!(intent.authority==authority,"retained final-copy authority changed");
            return live::finalization::resume_controlled(&project,&guard,&input.id,&authority,control,||input.final_authorize(&project,control,&locks));
        }
        ensure!(input.final_authorize(&project,control,&locks)?&&thread::final_copy::resolution_eligible(&project,&expected,&finalization.purpose)?,"automatic finalization is no longer eligible");
    }
    thread::copy_delivery::ready(&expected)?;
    ensure!(!expected.thread_dir.is_empty()&&Path::new(&expected.thread_dir).is_absolute(),"copy source path must be absolute");
    let spool=live::Spool::reserve(&project,control)?;
    let mut command=if let Some(target)=&input.target {
        let probe=helpers.ssh(target,&format!("{} artifact-stream --probe",remote::quote(&input.helper)),remote::SSH_TIMEOUT)?;
        supports_live(&run(probe,control,&locks)?)?;
        input.authorize(control,&locks)?;
        helpers.ssh(target,&format!("{} artifact-stream --live --path {}",remote::quote(&input.helper),remote::quote(&expected.thread_dir)),remote::COPY_TIMEOUT)?
    }else {
        let mut command=Cmd::new(&helpers.local,remote::COPY_TIMEOUT).args(["artifact-stream","--live","--path",&expected.thread_dir]);
        command.env_clear=true;command
    };
    input.current(&project,&guard,control)?;
    command.stdout_file=Some((spool.path(),live::STREAM_LIMIT));
    run(command,control,&locks)?; // Sender success is mandatory, even for valid bytes.
    input.current(&project,&guard,control)?;input.authorize(control,&locks)?;
    let staged=live::receive_controlled(&project,&spool.path(),control)?;
    if let Some(finalization)=&input.finalization {
        staged.begin_final_controlled(&project,&guard,&expected,&authority,&finalization.operation,finalization.purpose.clone(),control,||input.final_authorize(&project,control,&locks))?;
        live::finalization::resume_controlled(&project,&guard,&input.id,&authority,control,||input.final_authorize(&project,control,&locks))
    }else {staged.publish_controlled(&project,&guard,&expected,&authority,control,||input.authorize(control,&locks))}
}

pub struct JobRunner {pub inner:Arc<dyn Runner+Send+Sync>}
impl Runner for JobRunner {
    fn run(&self,cmd:&Cmd)->Result<Output> {
        if cmd.program!=JOB {return self.inner.run(cmd);}
        let entered=Instant::now();ensure!(!cmd.timeout.is_zero()&&cmd.timeout<=BUDGET,"invalid live-copy queue budget");
        let control=Control{deadline:cmd.deadline.context("copy absolute deadline missing")?.min(entered+cmd.timeout),cancellation:cmd.cancellation.clone().context("copy cancellation missing")?};
        let text=cmd.stdin.as_deref().context("copy input missing")?;ensure!(text.len()<=INPUT_LIMIT,"copy input exceeds bounds");
        let input:Input=serde_json::from_str(text)?;
        execute(&input,&control,&Helpers::production()?)?;
        Ok(Output{code:Some(0),elapsed:entered.elapsed(),..Output::default()})
    }
    fn socket_request(&self,socket:&Path,line:&str,timeout:Duration)->Result<String>{self.inner.socket_request(socket,line,timeout)}
}
pub fn request(ctx:&Ctx<'_>,project:&Project,expected:&Thread,target:Option<&str>)->Result<Request> {
    request_inner(ctx,project,expected,target,None)
}
#[allow(dead_code)] // Ticker admission follows worker review.
pub fn request_final(ctx:&Ctx<'_>,project:&Project,expected:&Thread,target:Option<&str>,purpose:Purpose,operation:String)->Result<Request> {
    let socket=if matches!(purpose,Purpose::Idle{..}) {Some(session(project)?)}else{None};
    request_inner(ctx,project,expected,target,Some(Finalization{sequence:expected.final_copy_sequence,pending:expected.pending_final_copy.clone(),purpose,operation,socket}))
}
fn request_inner(ctx:&Ctx<'_>,project:&Project,expected:&Thread,target:Option<&str>,finalization:Option<Finalization>)->Result<Request> {
    let path=project.dir().canonicalize()?;let metadata=std::fs::metadata(&path)?;
    let config_path=std::path::absolute(ctx.config_dir.join("config.toml"))?;
    let (kind,sequence)=if finalization.is_some(){("final-copy",expected.final_copy_sequence)}else{("live-copy",expected.live_copy_sequence)};
    let input=Input{finalization,project:path.clone(),project_identity:(metadata.dev(),metadata.ino()),id:expected.id.clone(),execution:thread::execution_fingerprint(expected),
        previous_hash:expected.report_hash.clone(),previous_receipt:expected.copy_receipt.clone(),pending:expected.pending_live_copy.clone(),sequence:expected.live_copy_sequence,
        config_digest:config(&config_path)?.1,config:config_path,herdr:ctx.env.herdr_bin().into(),helper:ctx.env.var("HERDR_PROJECTS_REMOTE_BIN").unwrap_or("herdr-projects").into(),machine:expected.machine.clone(),target:target.map(str::to_owned)};
    input.validate()?;let text=serde_json::to_string(&input)?;ensure!(text.len()<=INPUT_LIMIT,"copy input exceeds bounds");
    let identity=Identity{operation:format!("{kind}:{}",expected.id),revision:sequence.checked_add(1).context("copy sequence exhausted")?,project:path.to_str().context("copy project is not UTF-8")?.into(),machine:format!("routine-root:{}",path.parent().unwrap().display()),terminal:None};
    let deadline=Instant::now()+BUDGET;let mut command=Cmd::new(JOB,BUDGET).stdin(text);command.deadline=Some(deadline);
    Ok(Request{identity,lane:Lane::Transfer,deadline,command})
}

mod queue;
pub use queue::Queue;

#[cfg(all(test,target_os="linux"))]
mod tests {
    use super::*;
    use std::{fs,os::unix::fs::PermissionsExt};
    fn script(path:&Path,body:&str) {fs::write(path,format!("#!/bin/sh\n{body}\n")).unwrap();fs::set_permissions(path,fs::Permissions::from_mode(0o700)).unwrap();}
    fn fixture()->(tempfile::TempDir,Project,Thread,Input,Helpers) {
        let root=tempfile::tempdir().unwrap();let project=project::create(root.path(),"demo","",vec![]).unwrap();
        let source=root.path().join("source '$λ\n");fs::create_dir_all(source.join("library/empty")).unwrap();
        fs::write(source.join("report.md"),b"new\0\xff").unwrap();fs::write(source.join("library/item"),b"artifact").unwrap();
        let t=thread::allocate(&project,|t|{t.status=thread::Status::Open;t.thread_dir=source.to_str().unwrap().into();t.report_hash=thread::sha256_hex(b"old");}).unwrap();
        fs::write(thread::home_report_path(&project,&t.id),b"old").unwrap();
        let mut archive=Vec::new();live::export(&source,&mut archive).unwrap();fs::write(root.path().join("archive"),archive).unwrap();
        let helper=root.path().join("helper");let archive=remote::quote(root.path().join("archive").to_str().unwrap());
        script(&helper,&format!("/bin/cat {archive}"));
        let env=paths::Env::for_test(root.path(),&[]);let runner=crate::runner::RealRunner;
        let ctx=Ctx{env:&env,root:root.path().into(),config_dir:root.path().join("cfg"),runner:&runner,detached_ticker:false};
        let request=request(&ctx,&project,&t,None).unwrap();let input=serde_json::from_str(request.command.stdin.as_ref().unwrap()).unwrap();
        let helpers=Helpers{local:helper.to_str().unwrap().into(),ssh:helper.to_str().unwrap().into()};
        (root,project,t,input,helpers)
    }
    fn final_fixture()->(tempfile::TempDir,Project,Thread,Input,Helpers) {
        let(root,project,t,mut input,helpers)=fixture();
        let bytes=fs::read(Path::new(&t.thread_dir).join("report.md")).unwrap();
        fs::write(thread::home_report_path(&project,&t.id),&bytes).unwrap();
        let t=thread::update(&project,&t.id,|t|{t.report_hash=thread::sha256_hex(&bytes);t.last_group=thread::Group::Idle.token().into();t.last_state_change="2020-01-01T00:00:00Z".into();t.last_report_change=t.last_state_change.clone();t.last_state="idle".into();t.acked_report_hash=t.report_hash.clone();}).unwrap();
        project.update_coordinator(|c|c.socket="fixture.sock".into()).unwrap();
        let observer=root.path().join("observer");
        let agent=serde_json::json!({"result":{"agents":[{"pane_id":t.pane_id,"tab_id":t.tab_id,"workspace_id":t.workspace_id,"cwd":t.cwd,"name":t.agent_name,"agent_status":"idle"}]}}).to_string();
        script(&observer,&format!("case \"$*\" in *\"agent list\"*) printf '%s' {};; *) printf '%s' '{{\"result\":{{\"panes\":[]}}}}';; esac",remote::quote(&agent)));
        input.herdr=observer.to_str().unwrap().into();
        input.previous_hash=t.report_hash.clone();
        input.finalization=Some(Finalization{sequence:0,pending:None,operation:"idle-fixture".into(),socket:Some("fixture.sock".into()),purpose:Purpose::Idle{days:project.read_project_md().unwrap().0.auto_resolve_days,started:"2020-01-01T00:00:00Z".into(),last_state_change:t.last_state_change.clone(),last_report_change:t.last_report_change.clone()}});
        (root,project,t,input,helpers)
    }
    #[test]
    fn idle_observation_refuses_foreign_and_ambiguous_pane_occupants() {
        let mut t=Thread{status:thread::Status::Open,pane_id:"pane".into(),workspace_id:"workspace".into(),tab_id:"tab".into(),cwd:"/work".into(),agent_name:"ours".into(),report_hash:"acknowledged".into(),acked_report_hash:"acknowledged".into(),..Thread::default()};
        let agent=crate::herdr::Agent{pane_id:t.pane_id.clone(),workspace_id:t.workspace_id.clone(),tab_id:t.tab_id.clone(),cwd:t.cwd.clone(),name:t.agent_name.clone(),agent_status:"idle".into(),..Default::default()};
        let pane=crate::herdr::Pane{pane_id:t.pane_id.clone(),workspace_id:t.workspace_id.clone(),tab_id:t.tab_id.clone(),cwd:t.cwd.clone()};
        assert!(idle_observation(&t,&[],&[]),"an absent agent with an acknowledged report retains legacy idle policy");
        let mut foreign=agent.clone();foreign.name="foreign".into();foreign.agent_status="working".into();
        assert!(!idle_observation(&t,&[foreign],&[]));
        for status in ["","unknown"] {let mut unknown=agent.clone();unknown.agent_status=status.into();t.last_state=status.into();assert!(!idle_observation(&t,&[unknown],&[]));}
        t.last_state="idle".into();assert!(idle_observation(&t,&[agent.clone()],&[pane.clone()]));
        let mut working=agent.clone();working.agent_status="working".into();
        assert!(!idle_observation(&t,&[agent.clone(),working],&[pane.clone()]));
        assert!(!idle_observation(&t,&[agent.clone()],&[pane.clone(),pane.clone()]));
        let mut rebound=pane;rebound.cwd="/elsewhere".into();
        assert!(!idle_observation(&t,&[agent],&[rebound]));
    }
    #[test]
    fn final_worker_preserves_complete_missing_and_partial_native_copies() {
        for variant in ["complete","missing","partial","changed"] {
            let(root,project,t,input,helpers)=final_fixture();
            match variant {
                "missing"=>fs::remove_file(Path::new(&t.thread_dir).join("report.md")).unwrap(),
                "partial"=>std::os::unix::fs::symlink("item",Path::new(&t.thread_dir).join("library/link")).unwrap(),
                "changed"=>fs::write(Path::new(&t.thread_dir).join("report.md"),b"changed").unwrap(),
                _=>{},
            }
            let mut archive=Vec::new();live::export(Path::new(&t.thread_dir),&mut archive).unwrap();fs::write(root.path().join("archive"),archive).unwrap();
            execute(&input,&Control::default(),&helpers).unwrap();let current=thread::load(&project,&t.id).unwrap();
            assert_eq!(current.status,if variant=="changed"{thread::Status::Open}else{thread::Status::Resolved},"{variant}");
            assert!(current.pending_final_copy.is_none());assert!(current.pending_final_notice.is_some());
            assert_eq!(current.artifact_snapshot.is_empty(),variant=="partial"||variant=="changed");
            assert_eq!(current.copy_receipt.is_none(),variant=="missing");
            assert_eq!(fs::read(project.dir().join("library").join(&t.id).join("item")).unwrap(),b"artifact");
            assert_eq!(fs::read_dir(project.state_dir().join("live-copies")).unwrap().count(),0);
        }
    }
    #[test]
    fn final_worker_refuses_failed_sender_stale_eligibility_and_cancellation() {
        for variant in ["failed","working","cancelled","config","sequence"] {
            let(root,project,t,input,helpers)=final_fixture();let control=Control::default();
            let marker=root.path().join("spawned");
            script(Path::new(&helpers.local),&format!("touch {}\n/bin/cat {}\nexit 7",remote::quote(marker.to_str().unwrap()),remote::quote(root.path().join("archive").to_str().unwrap())));
            match variant {
                "working"=>{thread::update(&project,&t.id,|t|t.last_group=thread::Group::Working.token().into()).unwrap();},
                "cancelled"=>control.cancellation.cancel(),
                "config"=>{fs::create_dir(root.path().join("cfg")).unwrap();fs::write(&input.config,b"").unwrap();},
                "sequence"=>{thread::update(&project,&t.id,|t|t.final_copy_sequence+=1).unwrap();},
                _=>{},
            }
            let before=thread::load(&project,&t.id).unwrap();
            assert!(execute(&input,&control,&helpers).is_err(),"{variant}");assert_eq!(marker.exists(),variant=="failed");
            assert_eq!(thread::load(&project,&t.id).unwrap(),before);
            assert_eq!(fs::read_dir(project.state_dir().join("live-copies")).map(|d|d.count()).unwrap_or(0),0);
        }
    }
    #[test]
    fn merged_final_worker_requires_matching_published_pr_over_supervised_remote_transport() {
        for changed in [false,true] {
            let(root,project,t,mut input,helpers)=fixture();let url="https://github.com/example/repo/pull/1";
            let before=format!("PR: {url}\nold\n");fs::write(thread::home_report_path(&project,&t.id),&before).unwrap();
            let t=thread::update(&project,&t.id,|t|{t.pr=url.into();t.pr_state="MERGED".into();t.report_hash=thread::sha256_hex(before.as_bytes());}).unwrap();
            input.previous_hash=t.report_hash.clone();input.finalization=Some(Finalization{sequence:0,pending:None,purpose:Purpose::Merged{pr:url.into()},operation:"merged-fixture".into(),socket:None});
            fs::write(Path::new(&t.thread_dir).join("report.md"),format!("PR: {}\nnew\n",if changed{"https://github.com/example/repo/pull/2"}else{url})).unwrap();
            let mut archive=Vec::new();live::export(Path::new(&t.thread_dir),&mut archive).unwrap();fs::write(root.path().join("archive"),archive).unwrap();
            remote(root.path(),&project,&t,&mut input,&helpers,"{\"live_versions\":[1]}");
            execute(&input,&Control::default(),&helpers).unwrap();
            let current=thread::load(&project,&t.id).unwrap();assert!(current.copy_receipt.is_some());assert!(current.pending_final_copy.is_none());
            assert_eq!(current.status,if changed{thread::Status::Open}else{thread::Status::Resolved});
        }
    }
    #[test]
    fn live_agent_resuming_during_stream_prevents_idle_intent_without_record_changes() {
        let(root,project,t,input,helpers)=final_fixture();
        let replacement=root.path().join("working-observer");
        fs::write(&replacement,fs::read_to_string(&input.herdr).unwrap().replace("idle","working")).unwrap();
        script(Path::new(&helpers.local),&format!("/bin/cp {} {}\n/bin/cat {}",remote::quote(replacement.to_str().unwrap()),remote::quote(&input.herdr),remote::quote(root.path().join("archive").to_str().unwrap())));
        assert!(execute(&input,&Control::default(),&helpers).is_err());
        assert_eq!(thread::load(&project,&t.id).unwrap(),t);
        assert_eq!(fs::read_dir(project.state_dir().join("live-copies")).unwrap().count(),0);
    }
    #[test]
    fn final_worker_recovers_without_sender_and_finishes_copy_after_eligibility_loss() {
        for variant in ["idle","record-working","live-working","offline","maximum"] {
            let(root,project,mut t,mut input,helpers)=final_fixture();
            if variant=="maximum" {t=thread::update(&project,&t.id,|t|t.final_copy_sequence=i64::MAX as u64-1).unwrap();input.finalization.as_mut().unwrap().sequence=t.final_copy_sequence;}
            let authority=input.authority().unwrap();
            let guard=ProjectGuard::acquire(&project.dir()).unwrap();let staged=live::receive(&project,&root.path().join("archive")).unwrap();
            let f=input.finalization.as_ref().unwrap();
            staged.begin_final_controlled(&project,&guard,&t,&authority,&f.operation,f.purpose.clone(),&Control::default(),||Ok(true)).unwrap();drop(guard);
            if variant=="record-working" {thread::update(&project,&t.id,|t|t.last_group=thread::Group::Working.token().into()).unwrap();}
            if variant=="live-working" {let script=fs::read_to_string(&input.herdr).unwrap().replace("idle","working");fs::write(&input.herdr,script).unwrap();}
            if variant=="offline" {fs::remove_file(&input.herdr).unwrap();}
            let pending=thread::load(&project,&t.id).unwrap();let f=input.finalization.as_mut().unwrap();f.pending=pending.pending_final_copy.clone();f.sequence=pending.final_copy_sequence;
            fs::remove_dir_all(&t.thread_dir).unwrap();fs::remove_file(&helpers.local).unwrap();
            fs::create_dir(root.path().join("cfg")).unwrap();fs::write(&input.config,b"").unwrap();
            assert!(execute(&input,&Control::default(),&helpers).is_err());assert_eq!(thread::load(&project,&t.id).unwrap(),pending);
            fs::remove_file(&input.config).unwrap();execute(&input,&Control::default(),&helpers).unwrap();
            let current=thread::load(&project,&t.id).unwrap();assert!(current.pending_final_copy.is_none());
            assert_eq!(current.status,if matches!(variant,"idle"|"maximum"){thread::Status::Resolved}else{thread::Status::Open},"{variant}");
        }
    }
    #[test]
    fn supervised_local_success_publishes_and_preserves_literal_source_argument() {
        let (root,project,t,input,helpers)=fixture();
        script(Path::new(&helpers.local),&format!("printf '%s\\0' \"$@\" > {}\n/bin/cat {}",remote::quote(root.path().join("args").to_str().unwrap()),remote::quote(root.path().join("archive").to_str().unwrap())));
        execute(&input,&Control::default(),&helpers).unwrap();
        assert_eq!(fs::read(root.path().join("args")).unwrap(),format!("artifact-stream\0--live\0--path\0{}\0",t.thread_dir).as_bytes());
        assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"new\0\xff");
        assert_eq!(fs::read(project.dir().join("library").join(&t.id).join("item")).unwrap(),b"artifact");
        let current=thread::load(&project,&t.id).unwrap();assert!(current.pending_live_copy.is_none());assert!(current.copy_receipt.is_some());assert!(current.artifact_snapshot.is_empty());
        assert_eq!(fs::read_dir(project.state_dir().join("live-copies")).unwrap().count(),0);
    }
    #[test]
    fn valid_stream_from_failed_sender_never_creates_intent_or_receipt() {
        let (root,project,t,input,helpers)=fixture();
        script(Path::new(&helpers.local),&format!("/bin/cat {}\nexit 7",remote::quote(root.path().join("archive").to_str().unwrap())));
        assert!(execute(&input,&Control::default(),&helpers).is_err());
        assert_eq!(thread::load(&project,&t.id).unwrap(),t);assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"old");
        assert_eq!(fs::read_dir(project.state_dir().join("live-copies")).unwrap().count(),0);
    }
    #[test]
    fn stale_execution_config_and_expired_or_cancelled_requests_cannot_start_sender() {
        for variant in ["execution","receipt","config","expired","cancelled","project"] {
            let (root,project,t,input,helpers)=fixture();let mut control=Control::default();
            script(Path::new(&helpers.local),&format!("touch {}",remote::quote(root.path().join("spawned").to_str().unwrap())));
            match variant {
                "execution"=>{thread::update(&project,&t.id,|t|t.lifecycle_generation+=1).unwrap();},
                "receipt"=>{thread::update(&project,&t.id,|t|t.copy_receipt=Some(CopyReceipt{sequence:1,execution:thread::execution_fingerprint(t),report_hash:t.report_hash.clone(),notes:Vec::new()})).unwrap();},
                "config"=>{fs::create_dir(root.path().join("cfg")).unwrap();fs::write(&input.config,b"").unwrap();},
                "expired"=>control.deadline=Instant::now(),
                "cancelled"=>control.cancellation.cancel(),
                "project"=>{fs::rename(project.dir(),root.path().join("previous")).unwrap();project::create(root.path(),"demo","",vec![]).unwrap();},
                _=>unreachable!(),
            }
            assert!(execute(&input,&control,&helpers).is_err(),"{variant}");assert!(!root.path().join("spawned").exists(),"{variant}");
        }
    }
    #[test]
    fn retained_recovery_never_refetches_and_config_withdrawal_preserves_intent() {
        let (root,project,t,mut input,helpers)=fixture();let authority=input.authority().unwrap();
        let guard=ProjectGuard::acquire(&project.dir()).unwrap();let staged=live::receive(&project,&root.path().join("archive")).unwrap();let mut calls=0;
        assert!(staged.publish(&project,&guard,&t,&authority,||{calls+=1;if calls==3 {anyhow::bail!("interrupt before publication");}Ok(())}).is_err());drop(guard);
        let pending=thread::load(&project,&t.id).unwrap();input.pending=pending.pending_live_copy.clone();input.sequence=pending.live_copy_sequence;
        assert!(input.pending.is_some());fs::remove_dir_all(&t.thread_dir).unwrap();fs::remove_file(&helpers.local).unwrap();
        fs::create_dir(root.path().join("cfg")).unwrap();fs::write(&input.config,b"").unwrap();
        assert!(execute(&input,&Control::default(),&helpers).is_err());assert_eq!(thread::load(&project,&t.id).unwrap(),pending);
        fs::remove_file(&input.config).unwrap();execute(&input,&Control::default(),&helpers).unwrap();
        assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"new\0\xff");assert!(thread::load(&project,&t.id).unwrap().pending_live_copy.is_none());
    }
    #[test]
    fn running_sender_cancellation_discards_spool_and_releases_ownership() {
        let (root,project,t,input,helpers)=fixture();let marker=root.path().join("started");
        script(Path::new(&helpers.local),&format!("/bin/cat {}\ntouch {}\nsleep 60",remote::quote(root.path().join("archive").to_str().unwrap()),remote::quote(marker.to_str().unwrap())));
        let control=Control::default();let cancellation=control.cancellation.clone();
        let cancel=std::thread::spawn(move|| {
            let deadline=Instant::now()+Duration::from_secs(5);
            while !marker.exists()&&Instant::now()<deadline {std::thread::sleep(Duration::from_millis(2));}
            let started=marker.exists();cancellation.cancel();started
        });
        assert!(execute(&input,&control,&helpers).is_err());assert!(cancel.join().unwrap());
        assert_eq!(thread::load(&project,&t.id).unwrap(),t);assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"old");
        assert_eq!(fs::read_dir(project.state_dir().join("live-copies")).unwrap().count(),0);
        assert!(herdr_projects::execution_guard::RootGuard::exclusive(root.path()).is_ok());
    }
    #[test]
    fn observation_runner_cannot_forge_sender_success_or_receipt() {
        struct Forged(std::sync::atomic::AtomicUsize);
        impl Runner for Forged {
            fn run(&self,_:&Cmd)->Result<Output> {self.0.fetch_add(1,std::sync::atomic::Ordering::Relaxed);Ok(Output{code:Some(0),..Output::default()})}
            fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{panic!("unexpected socket effect")}
        }
        let (_root,project,t,input,_helpers)=fixture();let inner=Arc::new(Forged(std::sync::atomic::AtomicUsize::new(0)));let runner=JobRunner{inner:inner.clone()};
        let mut command=Cmd::new(JOB,BUDGET).stdin(serde_json::to_string(&input).unwrap());command.deadline=Some(Instant::now()+BUDGET);command.cancellation=Some(crate::runner::Cancellation::default());
        // current_exe is this test harness, which rejects artifact-stream args.
        // A permissive observation Runner must not replace that concrete failure.
        assert!(runner.run(&command).is_err());assert_eq!(inner.0.load(std::sync::atomic::Ordering::Relaxed),0);
        assert_eq!(thread::load(&project,&t.id).unwrap(),t);
        command.deadline=Some(Instant::now());assert!(runner.run(&command).is_err());assert_eq!(inner.0.load(std::sync::atomic::Ordering::Relaxed),0);
    }
    #[test]
    fn ticker_recovers_retained_remote_copy_without_reachable_session_or_source_host() {
        let (root,project,t,mut input,helpers)=fixture();remote(root.path(),&project,&t,&mut input,&helpers,"{\"live_versions\":[1]}");
        let current=thread::load(&project,&t.id).unwrap();let authority=input.authority().unwrap();
        let guard=ProjectGuard::acquire(&project.dir()).unwrap();let staged=live::receive(&project,&root.path().join("archive")).unwrap();let mut calls=0;
        assert!(staged.publish(&project,&guard,&current,&authority,||{calls+=1;if calls==3 {anyhow::bail!("interrupted");}Ok(())}).is_err());drop(guard);
        fs::remove_dir_all(&current.thread_dir).unwrap();fs::remove_file(&helpers.ssh).unwrap();
        let env=paths::Env::for_test(root.path(),&[("HERDR_BIN_PATH",&input.herdr)]);let runner=crate::runner::RealRunner;
        let ctx=Ctx{env:&env,root:root.path().into(),config_dir:root.path().join("cfg"),runner:&runner,detached_ticker:false};
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(JobRunner{inner:Arc::new(crate::runner::RealRunner)})).unwrap());
        let mut memory=crate::steps::Memory::new(&ctx);memory.copy_jobs=Some(Queue::new(pool.clone()));
        assert!(crate::ticker::tick_for_test(&ctx,&mut memory));assert!(memory.copy_jobs.as_ref().unwrap().pending());
        let deadline=Instant::now()+Duration::from_secs(3);
        while thread::load(&project,&t.id).unwrap().pending_live_copy.is_some(){assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
        assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"new\0\xff");
        assert!(pool.stop(Duration::from_secs(1)));assert!(memory.copy_jobs.as_mut().unwrap().drain().is_empty());
    }
    fn remote(root:&Path,project:&Project,t:&Thread,input:&mut Input,helpers:&Helpers,probe:&str) {
        thread::update(project,&t.id,|t|t.machine="box".into()).unwrap();let current=thread::load(project,&t.id).unwrap();input.execution=thread::execution_fingerprint(&current);input.machine="box".into();input.target=Some("user@box".into());
        let listing=root.join("herdr");script(&listing,"printf '%s' '[{\"label\":\"box\",\"target\":\"user@box\"}]'");input.herdr=listing.to_str().unwrap().into();
        script(Path::new(&helpers.ssh),&format!("case \"$*\" in *'artifact-stream --probe'*) printf '%s' {};; *) /bin/cat {};; esac",remote::quote(probe),remote::quote(root.join("archive").to_str().unwrap())));
    }
    #[test]
    fn remote_requires_capability_and_matching_supervised_route_without_fallback_copy() {
        for variant in ["supported","unsupported","rerouted"] {
            let (root,project,t,mut input,helpers)=fixture();
            remote(root.path(),&project,&t,&mut input,&helpers,if variant=="unsupported" {"{\"schema\":1}"}else{"{\"live_versions\":[1]}"});
            if variant=="rerouted" {script(Path::new(&input.herdr),"printf '%s' '[{\"label\":\"box\",\"target\":\"other\"}]'");}
            let before=thread::load(&project,&t.id).unwrap();let result=execute(&input,&Control::default(),&helpers);
            if variant=="supported" {result.unwrap();assert!(thread::load(&project,&t.id).unwrap().copy_receipt.is_some());}
            else {assert!(result.is_err(),"{variant}");assert_eq!(thread::load(&project,&t.id).unwrap(),before);assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"old");}
        }
    }
    #[test]
    fn route_changed_during_stream_cannot_authorize_publication() {
        let (root,project,t,mut input,helpers)=fixture();remote(root.path(),&project,&t,&mut input,&helpers,"{\"live_versions\":[1]}");
        let replacement=root.path().join("reroute");script(&replacement,"printf '%s' '[{\"label\":\"box\",\"target\":\"other\"}]'");
        script(Path::new(&helpers.ssh),&format!("case \"$*\" in *'artifact-stream --probe'*) printf '%s' '{{\"live_versions\":[1]}}';; *) /bin/cp {} {}\n/bin/cat {};; esac",remote::quote(replacement.to_str().unwrap()),remote::quote(&input.herdr),remote::quote(root.path().join("archive").to_str().unwrap())));
        let before=thread::load(&project,&t.id).unwrap();assert!(execute(&input,&Control::default(),&helpers).is_err());assert_eq!(thread::load(&project,&t.id).unwrap(),before);
        assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"old");
    }
}
