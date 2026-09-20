use super::*;
use std::{fs,os::unix::{fs::PermissionsExt,net::UnixListener}};
struct Fixture {root:tempfile::TempDir,p:Project,t:thread::Thread,env:paths::Env,_socket:UnixListener}
impl Fixture {
    fn new(mode:&str)->Self {
        let root=tempfile::tempdir().unwrap();let p=project::create(root.path(),"demo","",vec![]).unwrap();let socket=root.path().join("socket");let listener=UnixListener::bind(&socket).unwrap();p.update_coordinator(|c|c.socket=socket.display().to_string()).unwrap();
        let t=thread::allocate(&p,|t|{t.status=thread::Status::Open;t.pane_id="p".into();t.workspace_id="w".into();t.tab_id="tab".into();t.cwd="/fixture".into();t.agent="claude".into();t.agent_name="worker".into();t.last_state="working".into();t.last_group="working".into();}).unwrap();
        let helper=root.path().join("herdr");let env=paths::Env::for_test(root.path(),&[("HERDR_BIN_PATH",helper.to_str().unwrap())]);
        fs::write(root.path().join("mode"),mode).unwrap();let script=format!(r#"#!/usr/bin/python3
import json,sys,pathlib,time
root=pathlib.Path({root:?});mode=(root/'mode').read_text()
with open(root/'calls','a') as f:f.write(':'.join(sys.argv[1:])+'\n')
if mode=='blocked':time.sleep(60)
if mode=='slow':time.sleep(8)
if mode=='unavailable':sys.exit(1)
if mode=='malformed':print('not json');sys.exit(0)
if mode=='oversized':print('x'*1100000);sys.exit(0)
a={{'pane_id':'p','workspace_id':'w','tab_id':'tab','cwd':'/fixture','agent':'claude','name':'worker','agent_status':'idle'}}
if sys.argv[1:]==['agent','list']:result={{'agents':[a,a] if mode=='duplicate-agent' else [a]}}
elif sys.argv[1:]==['pane','list']:
 if mode=='partial':sys.exit(1)
 if mode=='inconsistent':a['cwd']='different'
 if mode=='empty-id':a['tab_id']=''
 result={{'panes':[] if mode=='missing-pane' else [a,a] if mode=='duplicate-pane' else [a]}}
else:sys.exit(3)
print(json.dumps({{'result':result}}))
"#,root=root.path().display().to_string());fs::write(&helper,script).unwrap();fs::set_permissions(&helper,fs::Permissions::from_mode(0o700)).unwrap();Self{root,p,t,env,_socket:listener}
    }
    fn ctx(&self)->Ctx<'_>{Ctx{env:&self.env,root:self.root.path().into(),config_dir:self.root.path().join("cfg"),runner:&crate::runner::RealRunner,detached_ticker:false}}
    fn input(&self)->Input{Input::new(&self.ctx(),&self.p,&Control::default()).unwrap()}
    fn pool(&self)->Arc<Executor>{Arc::new(Executor::new(crate::executor::Limits::default(),Arc::new(ProbeRunner{inner:Arc::new(crate::runner::RealRunner)})).unwrap())}
    fn change(&self,what:&str) {
        match what {
            "generation"=>{thread::update(&self.p,&self.t.id,|t|t.lifecycle_generation+=1).unwrap();},
            "new-thread"=>{thread::allocate(&self.p,|t|{t.pane_id="new".into();t.status=thread::Status::Open;}).unwrap();},
            "socket"=>{let socket=self.p.try_coordinator().unwrap().unwrap().socket;fs::remove_file(&socket).unwrap();let _other=UnixListener::bind(&socket).unwrap();},
            "config"=>{fs::create_dir_all(self.ctx().config_dir.clone()).unwrap();fs::write(self.ctx().config_dir.join("config.toml"),"# changed").unwrap();},
            "coordinator"=>{self.p.update_coordinator(|c|c.pane_id="changed".into()).unwrap();},
            "paused"=>self.p.set_status(project::Status::Paused).unwrap(),
            _=>panic!("unknown fixture change"),
        }
    }
}
fn finish(reads:&mut Reads) {let end=Instant::now()+Duration::from_secs(5);while !reads.pending.is_empty(){reads.begin_pass();if reads.pending.is_empty(){break;}assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}}
#[test]
fn collection_is_complete_bounded_and_distinguishes_negative_from_invalid() {
    let f=Fixture::new("ok");let ProbeOutcome::Ready(sample)=collect(&f.input(),&Control::default()).unwrap()else{panic!()};assert_eq!(sample.agents.len(),1);assert_eq!(sample.panes.len(),1);
    for mode in ["malformed","oversized","partial","duplicate-agent","duplicate-pane","inconsistent","empty-id","missing-pane"] {let f=Fixture::new(mode);assert!(collect(&f.input(),&Control::default()).is_err(),"{mode}");}
    let f=Fixture::new("unavailable");assert!(matches!(collect(&f.input(),&Control::default()).unwrap(),ProbeOutcome::Unavailable));
    let f=Fixture::new("ok");let input=f.input();fs::remove_file(&input.socket).unwrap();let input=f.input();assert!(matches!(collect(&input,&Control::default()).unwrap(),ProbeOutcome::Unavailable));assert!(!f.root.path().join("calls").exists());
    let f=Fixture::new("ok");let control=Control::default();control.cancellation.cancel();assert!(collect(&f.input(),&control).is_err());assert!(!f.root.path().join("calls").exists());
}
#[test]
fn collection_and_application_reject_changed_authority_and_expired_samples() {
    for what in ["generation","new-thread","socket","config","coordinator","paused"] {
        let f=Fixture::new("ok");let input=f.input();f.change(what);assert!(collect(&input,&Control::default()).is_err(),"{what}");assert!(!f.root.path().join("calls").exists());
        let f=Fixture::new("ok");let input=f.input();let ProbeOutcome::Ready(observation)=collect(&input,&Control::default()).unwrap()else{panic!()};let sample=Sample{input,deadline:Instant::now()+BUDGET,agents:observation.agents,panes:observation.panes};f.change(what);assert!(sample.current(&f.ctx(),&f.p).is_err(),"{what}");
    }
    let f=Fixture::new("ok");let sample=Sample{input:f.input(),deadline:Instant::now(),agents:vec![],panes:vec![]};assert!(sample.current(&f.ctx(),&f.p).is_err());
    let input=f.input();thread::update(&f.p,&f.t.id,|t|{t.last_state="idle".into();t.report_hash="fresh".into();}).unwrap();assert!(input.current(&Control::default()).is_ok(),"observation and receipt updates do not change execution bindings");
}
#[test]
fn failed_retries_keep_classification_but_route_changes_and_partial_samples_do_not() {
    let f=Fixture::new("unavailable");let pool=f.pool();let mut reads=Reads::new(pool.clone());
    assert!(matches!(reads.poll(&f.ctx(),&f.p).unwrap(),Poll::Pending));assert!(reads.unknown());assert!(reads.admit().is_empty());finish(&mut reads);
    assert!(matches!(reads.poll(&f.ctx(),&f.p).unwrap(),Poll::Unavailable));assert!(!reads.unknown());
    for _ in 0..2 {reads.begin_pass();assert!(matches!(reads.poll(&f.ctx(),&f.p).unwrap(),Poll::Pending));assert!(!reads.unknown(),"retry must not reset a known failure");reads.admit();finish(&mut reads);assert!(matches!(reads.poll(&f.ctx(),&f.p).unwrap(),Poll::Unavailable));assert!(!reads.unknown());}
    fs::write(f.root.path().join("mode"),"partial").unwrap();reads.begin_pass();reads.poll(&f.ctx(),&f.p).unwrap();reads.admit();finish(&mut reads);assert!(matches!(reads.poll(&f.ctx(),&f.p).unwrap(),Poll::Failed(_)));assert!(reads.unknown());
    f.change("config");reads.begin_pass();reads.poll(&f.ctx(),&f.p).unwrap();assert!(reads.unknown());assert!(pool.stop(Duration::from_secs(2)));
}
#[test]
fn queued_binding_changes_discard_completion_and_recollect_current_records() {
    let f=Fixture::new("ok");let pool=f.pool();let mut reads=Reads::new(pool.clone());reads.poll(&f.ctx(),&f.p).unwrap();reads.admit();finish(&mut reads);f.change("generation");
    assert!(matches!(reads.poll(&f.ctx(),&f.p).unwrap(),Poll::Pending));assert!(reads.unknown());reads.admit();finish(&mut reads);let Poll::Ready(sample)=reads.poll(&f.ctx(),&f.p).unwrap()else{panic!()};assert!(sample.current(&f.ctx(),&f.p).is_ok());assert!(reads.unknown(),"unapplied reachability must veto exit");assert!(pool.stop(Duration::from_secs(2)));
}
#[test]
fn blocked_observation_does_not_hold_effect_locks_and_stop_cancels_processes() {
    let f=Fixture::new("blocked");let pool=f.pool();let mut reads=Reads::new(pool.clone());reads.poll(&f.ctx(),&f.p).unwrap();reads.admit();let end=Instant::now()+Duration::from_secs(5);while !f.root.path().join("calls").exists(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}
    assert!(herdr_projects::execution_guard::ProjectGuard::acquire(&f.p.dir()).is_ok());assert!(crate::cleanup::lease(f.root.path()).is_ok());
    let start=Instant::now();assert!(pool.stop(Duration::from_secs(3)));assert!(start.elapsed()<Duration::from_secs(3));finish(&mut reads);assert!(!matches!(reads.poll(&f.ctx(),&f.p).unwrap(),Poll::Ready(_)));assert!(reads.unknown());
}
struct Negative;
impl Runner for Negative {
    fn run(&self,_:&Cmd)->Result<Output>{Ok(Output{code:Some(0),stdout:serde_json::to_string(&ProbeOutcome::Unavailable)?,..Output::default()})}
    fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}
}
#[test]
fn saturated_project_inventory_rotates_and_cannot_certify_untracked_sessions_failed() {
    let root=tempfile::tempdir().unwrap();let env=paths::Env::for_test(root.path(),&[]);let ctx=Ctx{env:&env,root:root.path().into(),config_dir:root.path().join("cfg"),runner:&crate::runner::RealRunner,detached_ticker:false};
    let projects=(0..OFFER_LIMIT+1).map(|n|{let p=project::create(root.path(),&format!("p{n:04}"),"",vec![]).unwrap();p.update_coordinator(|c|c.socket=root.path().join(format!("missing{n}.sock")).display().to_string()).unwrap();p}).collect::<Vec<_>>();
    let pool=Arc::new(Executor::new(crate::executor::Limits::default(),Arc::new(Negative)).unwrap());let mut reads=Reads::new(pool.clone());let mut seen=BTreeSet::new();
    for _ in 0..24 {
        reads.begin_pass();for p in &projects {if matches!(reads.poll(&ctx,p).unwrap(),Poll::Unavailable){seen.insert(p.slug.clone());}}
        assert!(reads.offers.len()<=OFFER_LIMIT&&reads.classified.len()<=OFFER_LIMIT);assert!(reads.unknown(),"untracked eligible sessions must veto exit");assert!(reads.admit().is_empty());assert!(reads.pending.len()<=PENDING_LIMIT);
        let end=Instant::now()+Duration::from_secs(3);while pool.metrics().queued[0]+pool.metrics().running[0]>0 {assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(2));}
    }
    assert_eq!(seen.len(),projects.len());reads.begin_pass();reads.poll(&ctx,&projects[0]).unwrap();reads.admit();assert!(reads.classified.len()<=1,"inactive identities must not permanently exhaust classification capacity");assert!(pool.stop(Duration::from_secs(2)));
}
#[test]
fn expiry_without_entering_runner_never_establishes_a_failed_session() {
    use std::sync::{Mutex,Condvar};
    struct Gate {state:Arc<(Mutex<bool>,Condvar)>}
    impl Runner for Gate {
        fn run(&self,cmd:&Cmd)->Result<Output>{if cmd.program=="hold" {let(mut open,wake)=(self.state.0.lock().unwrap(),&self.state.1);while !*open{open=wake.wait(open).unwrap();}Ok(Output{code:Some(0),..Output::default()})}else{panic!("expired observation entered runner")}}
        fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}
    }
    let f=Fixture::new("ok");let state=Arc::new((Mutex::new(false),Condvar::new()));let mut limits=crate::executor::Limits::default();limits.workers[0]=1;let pool=Arc::new(Executor::new(limits,Arc::new(Gate{state:state.clone()})).unwrap());
    let held=pool.submit(Request{identity:Identity{operation:"hold".into(),revision:1,project:"other".into(),machine:"other".into(),terminal:None},lane:Lane::Control,deadline:Instant::now()+BUDGET,command:Cmd::new("hold",BUDGET)}).unwrap();let end=Instant::now()+Duration::from_secs(3);while pool.metrics().running[0]!=1{assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(2));}
    let mut reads=Reads::new(pool.clone());reads.poll(&f.ctx(),&f.p).unwrap();for c in reads.offers.values_mut(){c.request.deadline=Instant::now()+Duration::from_millis(20);c.request.command.deadline=Some(c.request.deadline);}reads.admit();std::thread::sleep(Duration::from_millis(30));*state.0.lock().unwrap()=true;state.1.notify_all();held.recv_timeout(Duration::from_secs(2)).unwrap();finish(&mut reads);
    assert!(reads.classified.is_empty());assert!(matches!(reads.poll(&f.ctx(),&f.p).unwrap(),Poll::Pending|Poll::Failed(_)));assert!(reads.unknown());assert!(pool.stop(Duration::from_secs(2)));
}
#[test]
fn slow_session_cannot_block_healthy_status_or_fall_back_to_synchronous_observation() {
    struct NoCalls;
    impl Runner for NoCalls {fn run(&self,cmd:&Cmd)->Result<Output>{panic!("synchronous ticker command: {}",cmd.display())}fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{panic!("synchronous socket")}}
    let f=Fixture::new("blocked");let helper=f.env.herdr_bin();let script=fs::read_to_string(&helper).unwrap().replace("import json,sys,pathlib,time","import json,sys,pathlib,time,os").replace("if mode=='blocked':time.sleep(60)","if pathlib.Path(os.environ['HERDR_SOCKET_PATH']).name=='healthy.sock':mode='ok'\nif mode=='blocked':time.sleep(60)");fs::write(&helper,script).unwrap();
    let p=project::create(f.root.path(),"healthy","",vec![]).unwrap();let socket=f.root.path().join("healthy.sock");let _listener=UnixListener::bind(&socket).unwrap();p.update_coordinator(|c|c.socket=socket.display().to_string()).unwrap();let t=thread::allocate(&p,|t|{*t=thread::Thread{id:t.id.clone(),created:t.created.clone(),..f.t.clone()};}).unwrap();
    let runner=Arc::new(crate::token_jobs::JobRunner{inner:Arc::new(ProbeRunner{inner:Arc::new(crate::runner::RealRunner)})});let pool=Arc::new(Executor::new(crate::executor::Limits::default(),runner).unwrap());let ctx=Ctx{runner:&NoCalls,..f.ctx()};let mut memory=crate::steps::Memory::new(&ctx);memory.local_observations=Some(Reads::new(pool.clone()));memory.copy_jobs=Some(crate::copy_jobs::Queue::new(pool.clone()));
    let now=Instant::now();assert!(!crate::ticker::tick_for_test(&ctx,&mut memory));assert!(now.elapsed()<Duration::from_secs(1));assert!(memory.local_observations.as_ref().unwrap().unknown());
    assert_eq!(thread::load(&f.p,&f.t.id).unwrap().last_state,"working");let end=Instant::now()+Duration::from_secs(5);
    while thread::load(&p,&t.id).unwrap().last_state!="idle" {assert!(Instant::now()<end,"{}",fs::read_to_string(f.root.path().join("ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));crate::ticker::tick_for_test(&ctx,&mut memory);}
    assert_eq!(thread::load(&f.p,&f.t.id).unwrap().last_state,"working");assert!(pool.stop(Duration::from_secs(3)));
}
#[test]
fn late_success_survives_the_actual_fifteen_second_ticker_cadence() {
    let f=Fixture::new("slow");let pool=f.pool();let mut reads=Reads::new(pool.clone());reads.poll(&f.ctx(),&f.p).unwrap();reads.admit();let admitted=Instant::now();
    std::thread::sleep(crate::ticker::TICK);reads.begin_pass();assert!(matches!(reads.poll(&f.ctx(),&f.p).unwrap(),Poll::Pending));assert!(reads.unknown());
    std::thread::sleep(crate::ticker::TICK);reads.begin_pass();assert!(admitted.elapsed()>=BUDGET);
    let Poll::Ready(sample)=reads.poll(&f.ctx(),&f.p).unwrap()else{panic!("successful second-half collection must survive the next ticker pass")};assert!(sample.current(&f.ctx(),&f.p).is_ok());assert!(sample.deadline<=admitted+SAMPLE_AGE);
    let expired=Sample{deadline:Instant::now(),..sample};assert!(expired.current(&f.ctx(),&f.p).is_err());assert!(pool.stop(Duration::from_secs(2)));
}
