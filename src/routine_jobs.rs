//! Routine jobs share the bounded command pool. The marker is never an OS
//! executable, and pool output is never accepted as durable cleanup authority.
use std::{collections::BTreeMap,path::{Path,PathBuf},sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Deserialize,Serialize};
use crate::{executor::{Identity,Lane,Request},runner::{Cmd,Output,Runner}};
use herdr_projects::domain::OperationId;

const JOB:&str="\0herdr-projects-routine-execution";
const BUDGET:Duration=Duration::from_secs(95);
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {project:String,operation:OperationId,revision:u64}

pub struct JobRunner {pub inner:Arc<dyn Runner+Send+Sync>}
impl Runner for JobRunner {
    fn run(&self,cmd:&Cmd)->Result<Output> {
        if cmd.program!=JOB {return self.inner.run(cmd);}
        let entered=Instant::now();
        ensure!(!cmd.timeout.is_zero()&&cmd.timeout<=BUDGET,"invalid routine queue budget");
        let deadline=cmd.deadline.context("routine absolute queue deadline missing")?.min(entered+cmd.timeout);
        let input=cmd.stdin.as_deref().context("routine job input missing")?;
        ensure!(input.len()<=8192,"routine job input exceeds bound");
        let input:Input=serde_json::from_str(input)?;
        ensure!(input.project.len()<=4096&&Path::new(&input.project).is_absolute()&&input.revision>0,"invalid routine job identity");
        let cancellation=cmd.cancellation.clone().context("routine job cancellation missing")?;
        // This trusted service uses concrete RealRunner and commits the claim,
        // output and sealed cleanup receipt itself. An injected observation
        // Runner cannot mint a successful routine receipt.
        let receipt=herdr_projects::routines::execute_queued(Path::new(&input.project),&input.operation,input.revision,cancellation,deadline)?;
        let stdout=serde_json::to_string(&receipt)?;
        ensure!(stdout.len()<=crate::runner::CAPTURE_LIMIT,"routine result exceeds executor capture bound");
        Ok(Output{code:Some(0),stdout_bytes:stdout.as_bytes().to_vec(),stdout_total_bytes:stdout.len() as u64,stdout,elapsed:entered.elapsed(),..Output::default()})
    }
    fn socket_request(&self,socket:&Path,line:&str,timeout:Duration)->Result<String> {self.inner.socket_request(socket,line,timeout)}
}

pub fn request(project:&Path,operation:&OperationId,revision:u64)->Result<Request> {
    let project=project.canonicalize()?;
    let root=project.parent().context("routine project has no root")?;
    let identity=Identity{operation:format!("routine-execute:{}",operation.as_str()),revision,
        project:project.to_str().context("routine project is not UTF-8")?.into(),
        // Conservatively admit one running routine per root while terminal
        // effects still use the exclusive root compatibility barrier.
        machine:format!("routine-root:{}",root.display()),terminal:None};
    let input=Input{project:identity.project.clone(),operation:operation.clone(),revision};
    let deadline=Instant::now()+BUDGET;
    let mut command=Cmd::new(JOB,BUDGET).stdin(serde_json::to_string(&input)?);command.deadline=Some(deadline);
    Ok(Request{identity,lane:Lane::Transfer,deadline,command})
}

enum Entry {Pending{identity:Identity,operation:OperationId,ticket:crate::executor::Ticket},Cooldown{until:Instant,last:OperationId}}
/// Volatile tickets only: durable eligibility and all claim/effect decisions
/// remain in the store service. Restart never infers success from a lost ticket.
pub struct Queue {executor:Arc<crate::executor::Executor>,entries:BTreeMap<PathBuf,Entry>,last_project:Option<PathBuf>,unknown:bool}
impl Queue {
    pub fn new(executor:Arc<crate::executor::Executor>)->Self {Self{executor,entries:BTreeMap::new(),last_project:None,unknown:false}}
    pub fn drain(&mut self)->Vec<String> {
        let mut errors=Vec::new();let now=Instant::now();
        self.entries.retain(|path,entry| {
            let result=match entry {
                Entry::Cooldown{until,..}=>return now<*until+Duration::from_secs(120),
                Entry::Pending{identity,ticket,..}=>match ticket.try_recv() {
                    Ok(None)=>return true,
                    Ok(Some(completion))=>{
                        if completion.identity!=*identity {Err(anyhow::anyhow!("routine completion identity mismatch"))}
                        else {completion.result.and_then(|output| {ensure!(output.success(),"routine adapter failed");Ok(())})}
                    },
                    Err(error)=>Err(error),
                },
            };
            // The trusted ingress committed its own receipt. This completion
            // only controls local admission and diagnostics, never durable state.
            let last=match entry {Entry::Pending{operation,..}=>operation.clone(),_=>unreachable!()};
            let retry=result.is_err();
            if let Err(error)=result {
                errors.push(format!("{}: routine queue: {error:#}",path.display()));
            }
            *entry=Entry::Cooldown{until:now+if retry {Duration::from_secs(30)} else {Duration::ZERO},last};true
        });errors
    }
    pub fn unknown(&self)->bool {self.unknown}
    pub fn pending(&self)->bool {self.entries.values().any(|e|matches!(e,Entry::Pending{..}))}
    pub fn pending_project(&self,project:&str)->bool {self.entries.get(Path::new(project)).is_some_and(|e|matches!(e,Entry::Pending{..}))}
    #[cfg(test)]
    pub fn admit_projects(&mut self,projects:impl IntoIterator<Item=PathBuf>)->Vec<String> {
        self.admit_projects_where(projects,|_|true)
    }
    pub fn admit_projects_where(&mut self,projects:impl IntoIterator<Item=PathBuf>,allowed:impl Fn(&Path)->bool)->Vec<String> {
        self.unknown=false;let mut errors=Vec::new();if self.pending(){return errors;}
        let mut paths=Vec::new();
        for project in projects {match project.canonicalize(){Ok(path)=>paths.push(path),Err(error)=>errors.push(format!("{}: routine admission: {error}",project.display()))}}
        paths.sort();paths.dedup();
        if let Some(last)=&self.last_project {
            let first=paths.iter().position(|path|path>last).unwrap_or(0);paths.rotate_left(first);
        }
        for path in paths {
            if !allowed(&path){continue;}
            if let Err(error)=self.admit(&path){errors.push(format!("{}: routine admission: {error:#}",path.display()));}
            if self.pending(){break;}
        }
        self.unknown=!errors.is_empty();errors
    }
    fn admit(&mut self,project:&Path)->Result<()> {
        if !cfg!(target_os="linux") {return Ok(());}
        // A ticker owns one root. Never prequeue its next routine: the full
        // project pass must offer exclusive effects a turn between routines.
        if self.pending() {return Ok(());}
        let path=project.canonicalize()?;
        let last=match self.entries.get(&path) {
            Some(Entry::Pending{..})=>return Ok(()),
            Some(Entry::Cooldown{until,..}) if Instant::now()<*until=>return Ok(()),
            Some(Entry::Cooldown{last,..})=>Some(last.clone()),None=>None,
        };
        ensure!(self.entries.contains_key(&path)||self.entries.len()<128,"routine admission inventory is full");
        let mut budget=herdr_projects::store::identity_inventory::Budget::new(2*1024*1024,1024,Instant::now()+Duration::from_millis(100),Default::default())?;
        let Some(hint)=herdr_projects::migration::read_routine_execution_hint(&path,&mut budget,last.as_ref(),jiff::Timestamp::now().as_millisecond())? else{return Ok(());};
        let work=request(&path,&hint.operation,hint.delivery_revision)?;let identity=work.identity.clone();
        let ticket=self.executor.submit(work)?;
        self.last_project=Some(path.clone());
        self.entries.insert(path,Entry::Pending{identity,operation:hint.operation,ticket});Ok(())
    }
}
impl Drop for Queue {
    fn drop(&mut self) {for entry in self.entries.values(){if let Entry::Pending{ticket,..}=entry {ticket.cancel();}}}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{Executor,Limits};
    use herdr_projects::{runtime,routines,domain::TaskId,operations::DeliveryState};
    fn fixture(script:&[u8],deadline:u64)->(crate::scenarios::World,std::path::PathBuf,OperationId) {
        let(world,path)=crate::canonical_controller::tests::routine_fixture(&[("check",script,deadline)]);
        let occurrence=routines::schedule(&path,"check",runtime::snapshot(&path).unwrap().head).unwrap().unwrap();
        (world,path,occurrence.operation.unwrap())
    }
    #[cfg(target_os="linux")]
    #[test]
    fn owner_death_helper() {
        let Some(path)=std::env::var_os("HP_ROUTINE_OWNER_DEATH_PROJECT") else {return;};
        let path=std::path::PathBuf::from(path);let operation=OperationId::new(std::env::var("HP_ROUTINE_OWNER_DEATH_OPERATION").unwrap()).unwrap();
        routines::execute(&path,&operation,runtime::snapshot(&path).unwrap().head).unwrap();
    }
    #[cfg(target_os="linux")]
    #[test]
    fn supervisor_retains_execution_locks_after_owner_sigkill_until_namespace_cleanup() {
        let(world,path)=crate::canonical_controller::tests::routine_fixture(&[("first",b"touch started; setsid /bin/sh -c 'sleep 2; touch escaped' >/dev/null 2>&1 & sleep 10",1000),("second",b"touch second",1000)]);
        let first=routines::schedule(&path,"first",runtime::snapshot(&path).unwrap().head).unwrap().unwrap().operation.unwrap();
        let second=routines::schedule(&path,"second",runtime::snapshot(&path).unwrap().head).unwrap().unwrap().operation.unwrap();
        let mut owner=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","routine_jobs::tests::owner_death_helper","--nocapture"])
            .env("HP_ROUTINE_OWNER_DEATH_PROJECT",&path).env("HP_ROUTINE_OWNER_DEATH_OPERATION",first.as_str())
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        let deadline=Instant::now()+Duration::from_secs(5);
        while !path.join("started").exists() {
            if owner.try_wait().unwrap().is_some(){panic!("routine owner exited before script started");}
            if Instant::now()>=deadline{let _=owner.kill();let _=owner.wait();panic!("routine start deadline");}
            std::thread::sleep(Duration::from_millis(5));
        }
        owner.kill().unwrap();owner.wait().unwrap();
        assert!(crate::cleanup::lease(&world.root).is_err(),"root effects must remain excluded after owner death");
        let root_routines=std::fs::File::options().read(true).write(true).open(world.root.join(".routine-execution.lock")).unwrap();
        assert!(root_routines.try_lock().is_err(),"a restarted pool must not run a different project's routine");
        assert!(routines::execute(&path,&second,runtime::snapshot(&path).unwrap().head).is_err(),"another routine must remain excluded");
        assert!(!path.join("second").exists());
        let deadline=Instant::now()+Duration::from_secs(4);
        loop {if let Ok(guard)=crate::cleanup::lease(&world.root){drop(guard);break;}assert!(Instant::now()<deadline,"supervisor did not release ownership after cleanup");std::thread::sleep(Duration::from_millis(10));}
        root_routines.try_lock().unwrap();root_routines.unlock().unwrap();
        std::thread::sleep(Duration::from_millis(1300));assert!(!path.join("escaped").exists());
        let snapshot=runtime::snapshot(&path).unwrap();let first_delivery=snapshot.deliveries.iter().find(|d|d.operation==first).unwrap();
        assert_eq!(first_delivery.state,DeliveryState::Claimed);assert_eq!(first_delivery.attempts,1);assert!(snapshot.routine_receipts.is_empty());
        assert!(routines::execute(&path,&first,snapshot.head).is_err(),"owner death must never authorize replay");
        assert!(routines::execute(&path,&second,runtime::snapshot(&path).unwrap().head).unwrap().cleanup_verified);
    }
    #[cfg(target_os="linux")]
    #[test]
    fn ticker_admits_signed_work_once_and_restart_does_not_replay() {
        let(world,path)=crate::canonical_controller::tests::routine_fixture(&[("check",b"printf once >> marker",1000)]);
        let pool=Arc::new(Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Forbidden)})).unwrap());
        let mut memory=crate::steps::Memory::new(&world.ctx());memory.routine_jobs=Some(Queue::new(pool.clone()));
        let deadline=Instant::now()+Duration::from_secs(5);
        loop {
            assert!(crate::ticker::tick_for_test(&world.ctx(),&mut memory));
            let s=runtime::snapshot(&path).unwrap();
            if !s.routine_receipts.is_empty() {assert!(s.routine_receipts[0].cleanup_verified);assert_eq!(s.deliveries[0].state,DeliveryState::Confirmed);break;}
            assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(10));
        }
        // Lose all in-memory admission history, as on a ticker restart.
        assert!(pool.stop(Duration::from_secs(2)));drop(memory);drop(pool);
        let pool=Arc::new(Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Forbidden)})).unwrap());
        let mut memory=crate::steps::Memory::new(&world.ctx());memory.routine_jobs=Some(Queue::new(pool.clone()));
        for _ in 0..3 {assert!(crate::ticker::tick_for_test(&world.ctx(),&mut memory));}
        assert!(!memory.routine_jobs.as_ref().unwrap().pending());
        let s=runtime::snapshot(&path).unwrap();assert_eq!(s.routine_occurrences.len(),1);assert_eq!(s.routine_receipts.len(),1);assert_eq!(s.deliveries[0].attempts,1);
        assert_eq!(std::fs::read(path.join("marker")).unwrap(),b"once");assert!(pool.stop(Duration::from_secs(2)));
    }
    #[cfg(target_os="linux")]
    #[test]
    fn completed_ticket_must_be_drained_before_any_other_project_can_queue() {
        let(_first,path)=crate::canonical_controller::tests::routine_fixture(&[("check",b"touch marker",1000)]);
        let(_second,other)=crate::canonical_controller::tests::routine_fixture(&[("check",b"touch marker",1000)]);
        for path in [&path,&other] {routines::schedule(path,"check",runtime::snapshot(path).unwrap().head).unwrap();}
        let pool=Arc::new(Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Forbidden)})).unwrap());let mut queue=Queue::new(pool.clone());
        queue.admit(&path).unwrap();queue.admit(&other).unwrap();assert_eq!(queue.entries.len(),1);
        let deadline=Instant::now()+Duration::from_secs(3);while runtime::snapshot(&path).unwrap().routine_receipts.is_empty(){assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
        // Even after the worker completes, no admission may race the next pass.
        queue.admit(&other).unwrap();assert_eq!(queue.entries.len(),1);assert!(!other.join("marker").exists());
        assert!(queue.drain().is_empty());queue.admit(&other).unwrap();assert!(queue.entries.contains_key(&other));
        assert!(pool.stop(Duration::from_secs(2)));
    }
    #[cfg(target_os="linux")]
    #[test]
    fn project_fairness_advances_on_admission_not_ticker_cadence() {
        let(_first,path)=crate::canonical_controller::tests::routine_fixture(&[("a",b"true",1000),("b",b"true",1000)]);
        let(_second,other)=crate::canonical_controller::tests::routine_fixture(&[("a",b"true",1000),("b",b"true",1000)]);
        for path in [&path,&other] {for name in ["a","b"] {routines::schedule(path,name,runtime::snapshot(path).unwrap().head).unwrap();}}
        let pool=Arc::new(Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Forbidden)})).unwrap());let mut queue=Queue::new(pool.clone());
        let mut admitted=Vec::new();
        for _ in 0..2 {
            // Same scan order at every opportunity; both still have backlog.
            assert!(queue.admit_projects([path.clone(),other.clone()]).is_empty());
            admitted.push(queue.last_project.clone().unwrap());
            let deadline=Instant::now()+Duration::from_secs(3);while queue.pending(){assert!(queue.drain().is_empty());assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
        }
        assert_ne!(admitted[0],admitted[1]);
        for path in [&path,&other] {assert_eq!(runtime::snapshot(path).unwrap().routine_receipts.len(),1);}
        assert!(pool.stop(Duration::from_secs(2)));
    }
    #[cfg(target_os="linux")]
    #[test]
    fn all_projects_get_exclusive_effect_opportunity_before_routine_admission() {
        use herdr_projects::domain::{ProjectState,RuntimeRoute};
        use crate::runner::fake::ok;
        let(world,path)=crate::canonical_controller::tests::routine_fixture(&[("check",b"touch marker",1000)]);
        let config=world.ctx().config_dir.join("config.toml");
        let other=crate::project::create(&world.root,"z-notify","",vec![]).unwrap();other.set_status(crate::project::Status::Paused).unwrap();
        crate::inbox::write(&other,"test","fixture","pending notice","").unwrap();let other=other.dir();
        let plan=herdr_projects::migration::inspect_with_config(&other,&config).unwrap();herdr_projects::migration::apply(&other,&plan,true).unwrap();
        let task=TaskId::new("notify").unwrap();let head=runtime::add_task(&other,task.clone(),"notify".into(),runtime::snapshot(&other).unwrap().head).unwrap();
        runtime::create_binding(&other,None,None,head,&RuntimeRoute{socket:"/explicit/fairness.sock".into(),..Default::default()}).unwrap();
        crate::reconcile_live::run(&world.ctx(),&other,true).unwrap();let s=runtime::snapshot(&other).unwrap();runtime::set_state(&other,s.head,s.control.unwrap().revision,ProjectState::Active,&config).unwrap();
        world.runner.on("--version",ok("herdr 0.9.1")).on("notification show",ok(r#"{"result":{"shown":true}}"#));
        let operation=crate::notification_delivery::enqueue(&world.ctx(),&other,&task,runtime::snapshot(&other).unwrap().head).unwrap();
        let pool=Arc::new(Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Forbidden)})).unwrap());
        struct Check<'a>{inner:&'a dyn Runner,pool:Arc<Executor>}
        impl Runner for Check<'_> {
            fn run(&self,cmd:&Cmd)->Result<Output>{
                if cmd.args.windows(2).any(|a|a==["notification","show"]){assert_eq!(self.pool.metrics().high_water[1],0,"routine admitted before another project's effect opportunity");}
                self.inner.run(cmd)
            }
            fn socket_request(&self,path:&Path,line:&str,timeout:Duration)->Result<String>{self.inner.socket_request(path,line,timeout)}
        }
        let check=Check{inner:&world.runner,pool:pool.clone()};let original=world.ctx();
        let ctx=crate::paths::Ctx{runner:&check,..original};let mut memory=crate::steps::Memory::new(&ctx);memory.routine_jobs=Some(Queue::new(pool.clone()));
        assert!(crate::ticker::tick_for_test(&ctx,&mut memory));assert_eq!(world.runner.count("notification show"),1);
        assert_eq!(runtime::snapshot(&other).unwrap().deliveries.iter().find(|d|d.operation==operation.id).unwrap().state,DeliveryState::Confirmed);
        assert!(memory.routine_jobs.as_ref().unwrap().pending());assert_eq!(runtime::snapshot(&path).unwrap().routine_occurrences.len(),1);
        assert!(pool.stop(Duration::from_secs(2)));
    }
    #[cfg(target_os="linux")]
    #[test]
    fn failed_admission_backs_off_rotates_and_pause_prevents_new_tickets() {
        let(world,path)=crate::canonical_controller::tests::routine_fixture(&[("a",b"touch a-marker",1000),("b",b"touch b-marker",1000)]);
        for name in ["a","b"] {routines::schedule(&path,name,runtime::snapshot(&path).unwrap().head).unwrap();}
        // Both queued operations remain pending when exact script authority is withdrawn.
        for name in ["a","b"] {std::fs::write(path.join(format!("{name}.sh")),"changed").unwrap();}
        let pool=Arc::new(Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Forbidden)})).unwrap());let mut queue=Queue::new(pool.clone());
        queue.admit(&path).unwrap();let first=match &queue.entries[&path] {Entry::Pending{operation,..}=>operation.clone(),_=>panic!()};
        let deadline=Instant::now()+Duration::from_secs(3);let errors=loop {let errors=queue.drain();if !queue.pending(){break errors;}assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));};assert_eq!(errors.len(),1);
        queue.admit(&path).unwrap();assert!(!queue.pending());
        if let Entry::Cooldown{until,..}=queue.entries.get_mut(&path).unwrap(){*until=Instant::now();}
        queue.admit(&path).unwrap();let second=match &queue.entries[&path] {Entry::Pending{operation,..}=>operation.clone(),_=>panic!()};assert_ne!(first,second);
        let deadline=Instant::now()+Duration::from_secs(3);while queue.pending(){queue.drain();assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
        let s=runtime::snapshot(&path).unwrap();assert!(s.deliveries.iter().all(|d|d.attempts==0&&d.state==DeliveryState::Pending));
        runtime::set_state(&path,s.head,s.control.unwrap().revision,herdr_projects::domain::ProjectState::Paused,&world.ctx().config_dir.join("config.toml")).unwrap();
        queue.entries.clear();queue.admit(&path).unwrap();assert!(!queue.pending());assert!(!path.join("a-marker").exists()&&!path.join("b-marker").exists());assert!(pool.stop(Duration::from_secs(2)));
    }
    #[cfg(target_os="linux")]
    #[test]
    fn queued_ingress_rechecks_revision_budget_and_authority_without_pinning_event_head() {
        let(_world,path,id)=fixture(b"printf once >> marker",1000);let before=runtime::snapshot(&path).unwrap();
        for (revision,budget) in [(2,95),(1,1)] {
            let runner=JobRunner{inner:Arc::new(Forbidden)};let mut cmd=request(&path,&id,revision).unwrap().command;
            cmd.timeout=Duration::from_secs(budget);cmd.cancellation=Some(crate::runner::Cancellation::default());
            assert!(runner.run(&cmd).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);
        }
        let runner=JobRunner{inner:Arc::new(Forbidden)};let mut expired=request(&path,&id,1).unwrap().command;
        expired.deadline=Some(Instant::now());expired.cancellation=Some(crate::runner::Cancellation::default());
        assert!(runner.run(&expired).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);
        std::fs::write(path.join("check.sh"),b"touch changed").unwrap();
        let runner=JobRunner{inner:Arc::new(Forbidden)};let mut cmd=request(&path,&id,1).unwrap().command;cmd.cancellation=Some(crate::runner::Cancellation::default());
        assert!(runner.run(&cmd).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);
        std::fs::write(path.join("check.sh"),b"printf once >> marker").unwrap();
        let work=request(&path,&id,1).unwrap();
        runtime::add_task(&path,TaskId::new("unrelated").unwrap(),"unrelated event".into(),before.head).unwrap();
        let pool=Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Forbidden)})).unwrap();
        let completion=pool.submit(work).unwrap().recv_timeout(Duration::from_secs(3)).unwrap();assert!(completion.result.unwrap().success());
        let after=runtime::snapshot(&path).unwrap();assert!(after.routine_receipts[0].cleanup_verified);
        assert!(pool.submit(request(&path,&id,1).unwrap()).unwrap().recv_timeout(Duration::from_secs(3)).unwrap().result.is_err());
        assert_eq!(runtime::snapshot(&path).unwrap(),after);assert_eq!(std::fs::read(path.join("marker")).unwrap(),b"once");
        assert!(pool.stop(Duration::from_secs(2)));
    }
    #[cfg(target_os="linux")]
    #[test]
    fn running_job_preserves_control_lane_and_cancellation_drains_without_certifying_cleanup() {
        let(world,path,id)=fixture(b"touch started; setsid /bin/sh -c 'sleep 0.8; touch escaped' >/dev/null 2>&1 & sleep 10",10_000);
        let other=crate::project::create(&world.root,"other","",vec![]).unwrap();other.set_status(crate::project::Status::Paused).unwrap();
        let other=other.dir().canonicalize().unwrap();let config=world.ctx().config_dir.join("config.toml");
        let plan=herdr_projects::migration::inspect_with_config(&other,&config).unwrap();herdr_projects::migration::apply(&other,&plan,true).unwrap();
        let s=runtime::snapshot(&other).unwrap();runtime::create_binding(&other,None,None,s.head,&herdr_projects::domain::RuntimeRoute{socket:"/explicit/other.sock".into(),..Default::default()}).unwrap();
        crate::reconcile_live::run(&world.ctx(),&other,true).unwrap();let s=runtime::snapshot(&other).unwrap();
        runtime::set_state(&other,s.head,s.control.unwrap().revision,herdr_projects::domain::ProjectState::Active,&config).unwrap();
        let pool=Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(crate::runner::RealRunner)})).unwrap();
        let ticket=pool.submit(request(&path,&id,1).unwrap()).unwrap();let deadline=Instant::now()+Duration::from_secs(3);
        while !path.join("started").exists() {assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
        let before=runtime::snapshot(&path).unwrap();assert_eq!(before.deliveries[0].state,DeliveryState::Claimed);
        assert!(runtime::add_task(&path,TaskId::new("blocked").unwrap(),"mutation remains fenced".into(),before.head).is_err());
        let before_other=runtime::snapshot(&other).unwrap();
        runtime::add_task(&other,TaskId::new("independent").unwrap(),"other project progresses".into(),before_other.head).unwrap();
        assert!(crate::canonical_controller::poll(&world.ctx(),&other,0).is_ok());
        let refreshed=runtime::snapshot(&other).unwrap();assert!(refreshed.head>before_other.head);assert_eq!(refreshed.observations.len(),1);
        assert!(crate::cleanup::lease(&world.root).is_err());assert!(herdr_projects::migration::upgrade_active(&other).is_err());
        let control=Request{identity:Identity{operation:"independent-read".into(),revision:1,project:"other-project".into(),machine:"local".into(),terminal:None},lane:Lane::Control,deadline:Instant::now()+Duration::from_secs(1),command:Cmd::new("/bin/true",Duration::from_secs(1))};
        assert!(pool.submit(control).unwrap().recv_timeout(Duration::from_secs(1)).unwrap().result.unwrap().success());
        assert!(pool.stop(Duration::from_secs(2)));assert!(ticket.recv_timeout(Duration::from_secs(1)).unwrap().result.is_ok());
        let after=runtime::snapshot(&path).unwrap();assert_eq!(after.deliveries[0].state,DeliveryState::Ambiguous);assert!(!after.routine_receipts[0].cleanup_verified);
        std::thread::sleep(Duration::from_millis(900));assert!(!path.join("escaped").exists());
    }
    #[cfg(target_os="linux")]
    #[test]
    fn receipt_commit_failure_survives_pool_restart_without_replaying_effect() {
        let(_world,path,id)=fixture(b"printf once >> marker",1000);
        let raw=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();raw.execute_batch("CREATE TRIGGER fail_routine_receipt BEFORE INSERT ON events WHEN NEW.kind='routine.completed' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        let pool=Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Forbidden)})).unwrap();
        assert!(pool.submit(request(&path,&id,1).unwrap()).unwrap().recv_timeout(Duration::from_secs(3)).unwrap().result.is_err());
        assert!(pool.stop(Duration::from_secs(2)));drop(pool);raw.execute_batch("DROP TRIGGER fail_routine_receipt;").unwrap();
        let before=runtime::snapshot(&path).unwrap();assert_eq!(before.deliveries[0].state,DeliveryState::Claimed);assert!(before.routine_receipts.is_empty());
        let pool=Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Forbidden)})).unwrap();
        assert!(pool.submit(request(&path,&id,1).unwrap()).unwrap().recv_timeout(Duration::from_secs(3)).unwrap().result.is_err());
        assert_eq!(runtime::snapshot(&path).unwrap(),before);assert_eq!(std::fs::read(path.join("marker")).unwrap(),b"once");assert!(pool.stop(Duration::from_secs(2)));
    }
    struct Forbidden;
    impl Runner for Forbidden {
        fn run(&self,_:&Cmd)->Result<Output> {panic!("routine must not use the injectable observation runner")}
        fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}
    }
    #[cfg(target_os="linux")]
    #[test]
    fn script_withdrawn_while_waiting_in_pool_is_never_claimed() {
        use std::sync::{mpsc,Mutex};
        struct Gate {started:mpsc::Sender<()>,release:Mutex<mpsc::Receiver<()>>}
        impl Runner for Gate {
            fn run(&self,cmd:&Cmd)->Result<Output> {assert_eq!(cmd.program,"fixture-gate");self.started.send(()).unwrap();self.release.lock().unwrap().recv_timeout(Duration::from_secs(3)).unwrap();Ok(Output{code:Some(0),..Output::default()})}
            fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}
        }
        let(_world,path,id)=fixture(b"touch marker",1000);let before=runtime::snapshot(&path).unwrap();
        let(started,ready)=mpsc::channel();let(release,gate)=mpsc::channel();let mut limits=Limits::default();limits.workers=[1,1];
        let pool=Executor::new(limits,Arc::new(JobRunner{inner:Arc::new(Gate{started,release:Mutex::new(gate)})})).unwrap();
        let blocker=Request{identity:Identity{operation:"gate".into(),revision:1,project:"other-project".into(),machine:"fixture".into(),terminal:None},lane:Lane::Transfer,deadline:Instant::now()+Duration::from_secs(3),command:Cmd::new("fixture-gate",Duration::from_secs(3))};
        let blocker=pool.submit(blocker).unwrap();ready.recv_timeout(Duration::from_secs(1)).unwrap();
        let queued=pool.submit(request(&path,&id,1).unwrap()).unwrap();assert_eq!(pool.metrics().queued[1],1);
        std::fs::write(path.join("check.sh"),b"touch unauthorized").unwrap();release.send(()).unwrap();
        assert!(blocker.recv_timeout(Duration::from_secs(1)).unwrap().result.is_ok());
        assert!(queued.recv_timeout(Duration::from_secs(2)).unwrap().result.is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);
        assert!(!path.join("marker").exists()&&!path.join("unauthorized").exists());assert!(pool.stop(Duration::from_secs(1)));
    }
    #[test]
    fn invalid_or_cancelled_job_cannot_enter_an_external_runner() {
        let dir=tempfile::tempdir().unwrap();
        let pool=Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Forbidden)})).unwrap();
        let id=OperationId::new("missing-operation").unwrap();
        let mut work=request(dir.path(),&id,1).unwrap();let cancel=crate::runner::Cancellation::default();cancel.cancel();work.command.cancellation=Some(cancel);
        let completion=pool.submit(work).unwrap().recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!completion.runner_entered&&completion.result.unwrap().cancelled);
        let completion=pool.submit(request(dir.path(),&id,1).unwrap()).unwrap().recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(completion.runner_entered&&completion.result.is_err());
        assert!(!dir.path().join(".state/state.db").exists());assert!(pool.stop(Duration::from_secs(2)));
    }
    #[test]
    fn routine_hint_admission_does_not_bypass_worker_payload_validation() {
        let(_world,path,operation)=fixture(b"printf should-not-run",2000);let db=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();
        db.execute("UPDATE operations SET payload_hash=?1 WHERE id=?2",rusqlite::params!["0".repeat(64),operation.as_str()]).unwrap();assert!(runtime::snapshot(&path).is_err());
        let pool=Arc::new(Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(crate::runner::RealRunner)})).unwrap());let mut queue=Queue::new(pool.clone());
        assert!(queue.admit_projects([path.clone()]).is_empty());assert!(queue.pending());let deadline=Instant::now()+Duration::from_secs(5);let mut errors=Vec::new();
        while queue.pending(){errors.extend(queue.drain());assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
        assert_eq!(errors.len(),1);let state:(String,u64)=db.query_row("SELECT state,attempts FROM operation_delivery WHERE operation_id=?1",[operation.as_str()],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();assert_eq!(state,("pending".into(),0));assert!(pool.stop(Duration::from_secs(2)));
    }
    #[test]
    fn routine_hint_errors_veto_idle_until_next_known_admission_pass() {
        let(world,path,_operation)=fixture(b"printf unused",2000);let db=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();let original:Option<String>=db.query_row("SELECT config_digest FROM project_control",[],|r|r.get(0)).unwrap();
        db.execute_batch("PRAGMA ignore_check_constraints=ON;").unwrap();db.execute("UPDATE project_control SET config_digest=?1",["x".repeat(65)]).unwrap();
        let pool=Arc::new(Executor::new(Limits::default(),Arc::new(crate::runner::RealRunner)).unwrap());let mut queue=Queue::new(pool.clone());assert_eq!(queue.admit_projects([path.clone()]).len(),1);assert!(queue.unknown());assert!(!queue.pending());
        let mut memory=crate::steps::Memory::new(&world.ctx());memory.routine_jobs=Some(queue);assert!(memory.observations_unknown());
        db.execute("UPDATE project_control SET state='paused',config_digest=?1",[original]).unwrap();assert!(memory.routine_jobs.as_mut().unwrap().admit_projects([path]).is_empty());assert!(!memory.observations_unknown());assert!(pool.stop(Duration::from_secs(2)));
    }

}
