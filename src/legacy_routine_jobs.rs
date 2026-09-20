//! Legacy command ingress. Durable claims precede effects; only this concrete
//! supervisor may attach output. Lost claims report uncertainty, never replay.
use std::{path::{Path,PathBuf},os::unix::fs::MetadataExt,sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Serialize,Deserialize};
use crate::{executor::{Identity,Lane,Request},paths::Ctx,project::{self,Project},routine::{self,Routine,Ran},runner::{Cmd,Output,Runner},source_tree::Control,steps,thread::sha256_hex};
use herdr_projects::execution_guard::ProjectGuard;
const JOB:&str="\0herdr-projects-legacy-routine";
const BUDGET:Duration=Duration::from_secs(95);
const INPUT_LIMIT:usize=128*1024;

#[derive(Debug,Clone,Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Dispatch {id:String,prompt:String,result:Option<Ran>}
impl Dispatch {
    fn validate(&self)->Result<()> {
        ensure!(self.id.strip_prefix("routine-").is_some_and(|id|id.len()==64&&id.bytes().all(|b|b.is_ascii_hexdigit()))&&self.prompt.len()<=INPUT_LIMIT,"invalid routine dispatch");
        if let Some(result)=&self.result {
            ensure!(result.output_hash.len()==64&&result.output_hash.bytes().all(|b|b.is_ascii_hexdigit())&&result.block.len()<=50_000&&result.exit.len()<=128,"invalid routine result");
        }
        Ok(())
    }
}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    project:PathBuf,identity:(u64,u64),config:PathBuf,name:String,
    routine:String,previous:String,occurrence:String,
}
fn fingerprint(r:&Routine)->String {
    sha256_hex(serde_json::json!([r.name,r.schedule_text,r.command,r.enabled,r.prompt]).to_string().as_bytes())
}
fn save(project:&Project,state:&steps::State)->Result<()> {
    steps::save_state(project,state)?;
    std::fs::File::open(project.state_dir())?.sync_all()?;Ok(())
}
impl Input {
    fn validate(&self)->Result<()> {
        ensure!(self.project.is_absolute()&&self.config.is_absolute(),"routine paths must be absolute");
        project::validate_slug(&self.name)?;
        let previous=self.previous.parse::<jiff::Timestamp>()?;
        ensure!(self.occurrence.parse::<jiff::Timestamp>()?>previous,"routine occurrence must advance its cursor");
        ensure!(self.routine.len()==64&&self.routine.bytes().all(|b|b.is_ascii_hexdigit()),"invalid routine fingerprint");Ok(())
    }
    fn id(&self)->Result<String> {Ok(format!("routine-{}",sha256_hex(&serde_json::to_vec(self)?)))}
    fn current(&self,project:&Project,guard:&ProjectGuard,control:&Control)->Result<Routine> {
        control.check()?;guard.check_project(&self.project)?;project::ensure_legacy(&self.project)?;
        let m=std::fs::metadata(&self.project)?;ensure!((m.dev(),m.ino())==self.identity,"routine project identity changed");
        ensure!(project.try_status()?==project::Status::Active,"routine project is not active");
        ensure!(project.safety(&self.config)?.routine_commands,"routine commands are disabled");
        // Descriptor-relative read refuses aliases, special files and oversized definitions.
        let root=crate::source_tree::Directory::open(&project.dir())?;
        let mut file=root.file(Path::new(&format!("routines/{}.md",self.name)))?;
        use std::io::Read;
        let before=file.metadata()?;ensure!(before.len()<=INPUT_LIMIT as u64,"routine definition exceeds bounds");
        let mut bytes=Vec::new();(&mut file).take(INPUT_LIMIT as u64+1).read_to_end(&mut bytes)?;
        ensure!(bytes.len()<=INPUT_LIMIT,"routine definition exceeds bounds");crate::source_tree::unchanged(&file,&before)?;
        let routine=routine::parse(&self.name,std::str::from_utf8(&bytes)?)?;
        ensure!(routine.enabled&&!routine.command.is_empty()&&fingerprint(&routine)==self.routine,"queued routine changed");
        ensure!(routine::is_approved(&self.config,project,&routine),"routine command approval is absent or changed");
        control.check()?;Ok(routine)
    }
}
fn execute(input:&Input,control:&Control)->Result<()> {
    execute_with(input,control,||Ok(()))
}
fn execute_with(input:&Input,control:&Control,before_run:impl FnOnce()->Result<()>)->Result<()> {
    input.validate()?;control.check()?;
    let guard=ProjectGuard::acquire(&input.project)?;let locks=guard.inherit_transfer()?;
    let project=Project::load(input.project.parent().context("routine project root missing")?,input.project.file_name().and_then(|s|s.to_str()).context("invalid routine project name")?)?;
    let routine=input.current(&project,&guard,control)?;
    let occurrence=input.occurrence.parse::<jiff::Timestamp>()?;
    ensure!(occurrence<=jiff::Timestamp::now()&&routine::is_due(&routine.schedule,input.previous.parse()?,&occurrence.to_zoned(jiff::Zoned::now().time_zone().clone())),"routine occurrence is not due");
    let mut state=steps::try_load_state(&project)?;
    let entry=state.routines.get_mut(&input.name).context("routine schedule cursor missing")?;
    ensure!(entry.last_run==input.previous&&entry.dispatch.is_none(),"routine occurrence changed or awaits delivery");
    // Claim and cursor share one atomic file. Restart must not infer a command
    // never ran merely because its completion was lost.
    let dispatch=Dispatch{id:input.id()?,prompt:routine.prompt.clone(),result:None};
    entry.last_run=input.occurrence.clone();entry.dispatch=Some(dispatch.clone());
    save(&project,&state)?;
    before_run()?;
    input.current(&project,&guard,control)?;
    let command=Cmd::new("/bin/sh",routine::COMMAND_TIMEOUT).args(["-c",&routine.command]).cwd(project.dir());
    let output=herdr_projects::supervision::run(command,control.deadline,control.cancellation.clone(),&locks)?;
    // Cancellation may end execution, but recording its observation is still
    // required. No output from the caller's injectable Runner reaches here.
    let mut result=routine::render_output(&output);
    if output.code==Some(200)&&!output.timed_out&&!output.cancelled {
        result.exit="nonzero exit (supervisor status 200)".into();
        result.block=result.block.replacen("exit code 200",&result.exit,1);
    }
    guard.check_project(&project.dir())?;
    let mut state=steps::try_load_state(&project)?;let entry=state.routines.get_mut(&input.name).context("routine claim missing")?;
    ensure!(entry.last_run==input.occurrence&&entry.dispatch.as_ref()==Some(&dispatch),"routine claim changed during execution");
    entry.dispatch.as_mut().unwrap().result=Some(result);save(&project,&state)
}

/// Call only under the ticker's exclusive root lease. An inherited worker guard
/// excludes this recovery until all of its supervised descendants have exited.
pub fn deliver(project:&Project,state:&mut steps::State)->Result<()> {
    for (name,entry) in &mut state.routines {
        let Some(dispatch)=&entry.dispatch else {continue;};
        project::validate_slug(name)?;dispatch.validate()?;
        let (summary,body)=match &dispatch.result {
            Some(result) if result.output_hash==entry.output_hash=>{entry.dispatch=None;continue;},
            Some(result)=>(format!("routine `{name}` ran ({}) and its output changed",result.exit),format!("{}\n\n{}",dispatch.prompt,result.block).trim().to_string()),
            None=>(format!("routine `{name}` was interrupted; execution outcome is unknown and this occurrence will not be rerun"),String::new()),
        };
        crate::inbox::write_once(project,&dispatch.id,"routine",name,&summary,&body)?;
        // Make inbox rename durable before acknowledging in ticker state.
        std::fs::File::open(project.dir().join("inbox"))?.sync_all()?;
        if let Some(result)=&dispatch.result {entry.output_hash=result.output_hash.clone();}
        entry.dispatch=None;
    }
    Ok(())
}
pub struct JobRunner {pub inner:Arc<dyn Runner+Send+Sync>}
impl Runner for JobRunner {
    fn run(&self,cmd:&Cmd)->Result<Output> {
        if cmd.program!=JOB {return self.inner.run(cmd);}
        let entered=Instant::now();ensure!(!cmd.timeout.is_zero()&&cmd.timeout<=BUDGET,"invalid routine queue budget");
        let control=Control{deadline:cmd.deadline.context("routine absolute deadline missing")?.min(entered+cmd.timeout),cancellation:cmd.cancellation.clone().context("routine cancellation missing")?};
        let text=cmd.stdin.as_deref().context("routine input missing")?;ensure!(text.len()<=INPUT_LIMIT,"routine input exceeds bounds");
        execute(&serde_json::from_str(text)?,&control)?;
        Ok(Output{code:Some(0),elapsed:entered.elapsed(),..Output::default()})
    }
    fn socket_request(&self,socket:&Path,line:&str,timeout:Duration)->Result<String>{self.inner.socket_request(socket,line,timeout)}
}
pub fn request(ctx:&Ctx,project:&Project,routine:&Routine,previous:&str,occurrence:&str)->Result<Request> {
    let path=project.dir().canonicalize()?;let m=std::fs::metadata(&path)?;
    let input=Input{project:path.clone(),identity:(m.dev(),m.ino()),config:std::path::absolute(&ctx.config_dir)?,name:routine.name.clone(),routine:fingerprint(routine),previous:previous.into(),occurrence:occurrence.into()};
    input.validate()?;let text=serde_json::to_string(&input)?;ensure!(text.len()<=INPUT_LIMIT,"routine input exceeds bounds");
    let identity=Identity{operation:format!("legacy-routine:{}",routine.name),revision:1,project:path.to_str().context("routine project is not UTF-8")?.into(),machine:format!("routine-root:{}",path.parent().unwrap().display()),terminal:None};
    let deadline=Instant::now()+BUDGET;let mut command=Cmd::new(JOB,BUDGET).stdin(text);command.deadline=Some(deadline);
    Ok(Request{identity,lane:Lane::Transfer,deadline,command})
}

#[cfg(all(test,target_os="linux"))]
mod tests {
    use super::*;
    use crate::{executor::{Executor,Limits},scenarios::World};
    fn fixture(command:&str)->(World,Project,Routine,Input) {
        let world=World::new();let project=world.project("demo","session.sock");
        let text=format!("+++\nschedule = \"every 1m\"\ncommand = {}\n+++\nInspect output.\n",serde_json::to_string(command).unwrap());
        std::fs::write(project.dir().join("routines/check.md"),&text).unwrap();let routine=routine::parse("check",&text).unwrap();
        let ctx=world.ctx();std::fs::create_dir_all(&ctx.config_dir).unwrap();
        std::fs::write(ctx.config_dir.join("config.toml"),format!("[safety.\"{}\"]\nroutine_commands = true\n",project.canonical_dir().display())).unwrap();
        project::write_json(&ctx.config_dir.join("approved-routines.json"),&vec![routine::Approval{project:project.canonical_dir().display().to_string(),routine:routine.name.clone(),command_sha256:routine.command_hash(),approved:"fixture".into()}]).unwrap();
        let previous="2026-01-01T00:00:00Z";let mut state=steps::State::default();state.routines.entry(routine.name.clone()).or_default().last_run=previous.into();save(&project,&state).unwrap();
        let request=request(&ctx,&project,&routine,previous,&jiff::Timestamp::now().to_string()).unwrap();
        let input=serde_json::from_str(request.command.stdin.as_ref().unwrap()).unwrap();(world,project,routine,input)
    }
    fn deliver_saved(project:&Project) {
        let _lease=crate::cleanup::lease(&project.root).unwrap();let mut state=steps::try_load_state(project).unwrap();deliver(project,&mut state).unwrap();save(project,&state).unwrap();
    }
    fn wait(predicate:impl Fn()->bool) {let end=Instant::now()+Duration::from_secs(5);while !predicate(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}}
    #[test]
    fn concrete_execution_claims_once_and_output_delivery_replays_after_restart() {
        let (_world,project,_,input)=fixture("printf run >> count; printf 'hello ``` data\\n'");
        execute(&input,&Control::default()).unwrap();let state=steps::try_load_state(&project).unwrap();let entry=&state.routines["check"];
        assert_eq!(entry.last_run,input.occurrence);assert!(entry.dispatch.as_ref().unwrap().result.is_some());assert!(crate::inbox::unhandled(&project).is_empty());
        assert!(execute(&input,&Control::default()).is_err());assert_eq!(std::fs::read(project.dir().join("count")).unwrap(),b"run");
        // Crash after inbox write, before state acknowledgement; handled item is reused.
        {let _lease=crate::cleanup::lease(&project.root).unwrap();let mut state=state.clone();deliver(&project,&mut state).unwrap();}
        let item=crate::inbox::unhandled(&project).pop().unwrap();assert!(item.body.contains("hello ``` data"));
        std::fs::create_dir_all(project.dir().join("inbox/done")).unwrap();std::fs::rename(project.dir().join("inbox").join(format!("{}.md",item.id)),project.dir().join("inbox/done").join(format!("{}.md",item.id))).unwrap();
        deliver_saved(&project);assert!(crate::inbox::unhandled(&project).is_empty());assert!(steps::try_load_state(&project).unwrap().routines["check"].dispatch.is_none());
    }
    #[test]
    fn changed_approval_definition_cursor_and_expired_requests_have_no_effects() {
        for change in ["approval","definition","cursor","cancel","expired","inode","future"] {
            let (world,project,_,mut input)=fixture("touch executed");let mut control=Control::default();
            match change {
                "approval"=>std::fs::write(world.ctx().config_dir.join("approved-routines.json"),"[]").unwrap(),
                "definition"=>{let path=project.dir().join("routines/check.md");let text=std::fs::read_to_string(&path).unwrap();std::fs::write(path,text.replace("Inspect output.","changed prompt")).unwrap();},
                "cursor"=>{let mut state=steps::try_load_state(&project).unwrap();state.routines.get_mut("check").unwrap().last_run=input.occurrence.clone();save(&project,&state).unwrap();},
                "cancel"=>control.cancellation.cancel(),"expired"=>control.deadline=Instant::now(),
                "inode"=>input.identity.1+=1,"future"=>input.occurrence="2099-01-01T00:00:00Z".into(),_=>unreachable!(),
            }
            assert!(execute(&input,&control).is_err(),"{change}");assert!(!project.dir().join("executed").exists());assert!(steps::try_load_state(&project).unwrap().routines["check"].dispatch.is_none());
        }
    }
    #[test]
    fn interrupted_claim_is_reported_without_reexecuting_or_duplicate_delivery() {
        let (_world,project,_,input)=fixture("touch executed");
        assert!(execute_with(&input,&Control::default(),||anyhow::bail!("fixture crash after durable claim")).is_err());
        assert!(execute(&input,&Control::default()).is_err());assert!(!project.dir().join("executed").exists());
        deliver_saved(&project);deliver_saved(&project);let items=crate::inbox::unhandled(&project);assert_eq!(items.len(),1);assert!(items[0].summary.contains("outcome is unknown"));
    }
    #[test]
    fn special_and_oversized_control_files_refuse_without_claim_or_retained_locks() {
        use std::os::unix::ffi::OsStrExt;
        for target in ["approvals","state","definition"] {for kind in ["fifo","oversized"] {
            let (world,project,_,input)=fixture("touch executed");
            let (path,limit)=match target {
                "approvals"=>(world.ctx().config_dir.join("approved-routines.json"),1024*1024),
                "state"=>(project.state_dir().join("ticker.json"),16*1024*1024),
                _=>(project.dir().join("routines/check.md"),128*1024),
            };
            let original=std::fs::read(&path).unwrap();std::fs::remove_file(&path).unwrap();
            if kind=="fifo" {let c=std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();assert_eq!(unsafe{libc::mkfifo(c.as_ptr(),0o600)},0);}
            else {std::fs::File::create(&path).unwrap().set_len(limit+1).unwrap();}
            let started=Instant::now();assert!(execute(&input,&Control::default()).is_err(),"{target} {kind}");assert!(started.elapsed()<Duration::from_secs(2));
            assert!(crate::cleanup::lease(&world.root).is_ok());assert!(!project.dir().join("executed").exists());
            if target=="approvals" {assert!(!routine::is_approved(&world.ctx().config_dir,&project,&routine::parse("check","+++\nschedule = \"every 1m\"\ncommand = \"touch executed\"\n+++\n").unwrap()));}
            if target=="definition" {assert!(routine::load_all(&project).0.is_empty());}
            std::fs::remove_file(&path).unwrap();std::fs::write(&path,original).unwrap();assert!(steps::try_load_state(&project).unwrap().routines["check"].dispatch.is_none());
        }}
    }
    struct Untrusted;
    impl Runner for Untrusted {
        fn run(&self,_:&Cmd)->Result<Output>{anyhow::bail!("injectable runner must never execute a legacy routine")}
        fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}
    }
    #[test]
    fn queued_native_routine_keeps_other_status_responsive_and_cancel_is_durable() {
        let (world,project,routine,input)=fixture("touch entered; while :; do sleep 1; done");
        let other=world.project("other","other.sock");let thread=world.thread(&other,world.home.path(),|_|{});
        *world.panes.borrow_mut()=format!("[{},{}]",world.coordinator_pane(&other),crate::scenarios::pane_json("w2","w2:t1","w2:p1",&thread.cwd));
        *world.agents.borrow_mut()=format!("[{}]",crate::scenarios::agent_json("w2","w2:t1","w2:p1",&thread.cwd,&thread.agent_name,"working"));
        let pool=Arc::new(Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Untrusted)})).unwrap());
        let ticket=pool.submit(request(&world.ctx(),&project,&routine,&input.previous,&input.occurrence).unwrap()).unwrap();wait(||project.dir().join("entered").exists());
        assert!(crate::cleanup::lease(&world.root).is_err());assert!(ProjectGuard::acquire(&project.dir()).is_err());
        let ctx=world.ctx();let mut memory=steps::Memory::new(&ctx);assert!(crate::ticker::tick_for_test(&ctx,&mut memory));
        assert_eq!(crate::thread::load(&other,&thread.id).unwrap().last_state,"working");
        ticket.cancel();wait(||pool.metrics().completed[1]==1);let completion=ticket.try_recv().unwrap().unwrap();assert!(completion.result.unwrap().success());assert!(pool.stop(Duration::from_secs(2)));
        let state=steps::try_load_state(&project).unwrap();assert_eq!(state.routines["check"].dispatch.as_ref().unwrap().result.as_ref().unwrap().exit,"cancelled");deliver_saved(&project);assert!(crate::cleanup::lease(&world.root).is_ok());
    }
    #[test]
    fn ticker_offers_without_advancing_until_worker_claim_and_then_delivers() {
        let (world,project,_,input)=fixture("printf once >> count; printf result");
        *world.panes.borrow_mut()=format!("[{}]",world.coordinator_pane(&project));
        let pool=Arc::new(Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Untrusted)})).unwrap());
        let ctx=world.ctx();let mut memory=steps::Memory::new(&ctx);memory.copy_jobs=Some(crate::copy_jobs::Queue::new(pool.clone()));
        assert!(crate::ticker::tick_project_with(&ctx,&project,&mut memory).unwrap());
        assert_eq!(steps::try_load_state(&project).unwrap().routines["check"].last_run,input.previous);assert!(!project.dir().join("count").exists());assert!(memory.copy_jobs.as_ref().unwrap().offered());
        assert!(memory.copy_jobs.as_mut().unwrap().admit().is_empty());wait(||pool.metrics().completed[1]==1);
        assert!(memory.copy_jobs.as_mut().unwrap().drain().is_empty());assert!(crate::ticker::tick_project_with(&ctx,&project,&mut memory).unwrap());
        assert_eq!(std::fs::read(project.dir().join("count")).unwrap(),b"once");assert_eq!(crate::inbox::unhandled(&project).iter().filter(|i|i.kind=="routine").count(),1);assert!(pool.stop(Duration::from_secs(1)));
    }
}
