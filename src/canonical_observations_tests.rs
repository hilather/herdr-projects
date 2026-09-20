use super::*;
use std::{fs,os::unix::{fs::PermissionsExt,net::UnixListener}};
struct Fixture {world:crate::scenarios::World,path:PathBuf,env:Env,_socket:UnixListener}
impl Fixture {
    fn new(mode:&str)->Self {
        let world=crate::scenarios::World::new();let project=crate::project::create(&world.root,"canonical","",vec![]).unwrap();project.set_status(crate::project::Status::Paused).unwrap();let path=project.dir().canonicalize().unwrap();
        let plan=migration::inspect(&path).unwrap();migration::apply(&path,&plan,true).unwrap();
        let socket=world.home.path().join("canonical.sock");let listener=UnixListener::bind(&socket).unwrap();
        runtime::create_binding(&path,None,None,runtime::snapshot(&path).unwrap().head,&herdr_projects::domain::RuntimeRoute{socket:socket.display().to_string(),workspace_id:"w".into(),tab_id:"t".into(),pane_id:"p".into(),cwd:"/fixture".into(),..Default::default()}).unwrap();
        let helper=world.home.path().join("herdr");let env=Env::for_test(world.home.path(),&[("HERDR_BIN_PATH",helper.to_str().unwrap())]);
        fs::write(world.home.path().join("mode"),mode).unwrap();
        fs::write(&helper,format!(r#"#!/usr/bin/python3
import pathlib,sys,json,time
root=pathlib.Path({root:?})
if sys.argv[1:]==['--version']:print('herdr 0.9.1');sys.exit(0)
(root/'entered').write_text('yes')
mode=(root/'mode').read_text()
if mode=='blocked':time.sleep(60)
if mode=='failed':sys.exit(1)
if mode=='config-change':
 (root/'cfg').mkdir(exist_ok=True)
 (root/'cfg'/'config.toml').write_text('# changed during collection')
p={{'pane_id':'p','workspace_id':'w','tab_id':'t','cwd':'/fixture','agent':'claude','name':'fixture','agent_status':'idle'}}
if sys.argv[1:]==['pane','list']:r={{'panes':[p]}}
elif sys.argv[1:]==['agent','list']:r={{'agents':[p]}}
else:sys.exit(4)
print(json.dumps({{'result':r}}))
"#,root=world.home.path().display().to_string())).unwrap();fs::set_permissions(&helper,fs::Permissions::from_mode(0o700)).unwrap();
        Self{world,path,env,_socket:listener}
    }
    fn ctx(&self)->Ctx<'_>{Ctx{env:&self.env,root:self.world.root.clone(),config_dir:self.world.ctx().config_dir,runner:&crate::runner::RealRunner,detached_ticker:false}}
    fn input(&self)->Input{Input::new(&self.ctx(),&self.path).unwrap()}
    fn pool(&self)->Arc<Executor>{Arc::new(Executor::new(crate::executor::Limits::default(),Arc::new(ProbeRunner{inner:Arc::new(crate::runner::RealRunner)})).unwrap())}
}
fn finish(reads:&mut Reads){let end=Instant::now()+Duration::from_secs(5);while !reads.pending.is_empty(){reads.begin_pass();if reads.pending.is_empty(){break;}assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}}

#[test]
fn canonical_worker_commits_fresh_evidence_without_trusting_queue_consumption() {
    let f=Fixture::new("ok");let before=runtime::snapshot(&f.path).unwrap();let pool=f.pool();let mut reads=Reads::new(pool.clone());
    assert!(matches!(reads.poll(&f.ctx(),&f.path).unwrap(),Poll::Pending));assert!(reads.admit().is_empty());finish(&mut reads);
    // Drop the completion without consuming it. The DB, not this reply, is truth.
    drop(reads);let after=runtime::snapshot(&f.path).unwrap();assert!(after.head>before.head);assert!(after.observations.iter().any(|o|o.pane==ResourceState::Present));assert_eq!(after.tasks,before.tasks);assert_eq!(after.attempts,before.attempts);
    let mut restarted=Reads::new(pool.clone());assert!(matches!(restarted.poll(&f.ctx(),&f.path).unwrap(),Poll::Pending));assert!(restarted.admit().is_empty());finish(&mut restarted);assert!(matches!(restarted.poll(&f.ctx(),&f.path).unwrap(),Poll::Ready(true)));assert!(pool.stop(Duration::from_secs(3)));
}

#[test]
fn canonical_worker_cancellation_excludes_mutations_and_preserves_snapshot() {
    let f=Fixture::new("blocked");let before=runtime::snapshot(&f.path).unwrap();let pool=f.pool();let mut reads=Reads::new(pool.clone());reads.poll(&f.ctx(),&f.path).unwrap();reads.admit();
    let end=Instant::now()+Duration::from_secs(5);while !f.world.home.path().join("entered").exists(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}
    assert!(ProjectGuard::acquire(&f.path).is_err());
    assert!(runtime::add_task(&f.path,herdr_projects::domain::TaskId::new("refused").unwrap(),"refused".into(),before.head).is_err());
    let other=crate::project::create(&f.world.root,"other","",vec![]).unwrap();assert!(ProjectGuard::acquire(&other.dir()).is_ok());
    let started=Instant::now();assert!(pool.stop(Duration::from_secs(3)));assert!(started.elapsed()<Duration::from_secs(3));assert_eq!(runtime::snapshot(&f.path).unwrap(),before);assert!(ProjectGuard::acquire(&f.path).is_ok());
}

#[test]
fn canonical_worker_refuses_expiry_and_changed_configuration_without_committing() {
    for mode in ["ok","config-change"] {
        let f=Fixture::new(mode);let before=runtime::snapshot(&f.path).unwrap();let mut control=Control{deadline:Instant::now()+BUDGET,cancellation:Default::default()};
        if mode=="ok"{control.deadline=Instant::now();}
        assert!(collect(&f.input(),&control).is_err());assert_eq!(runtime::snapshot(&f.path).unwrap(),before);
        if mode=="ok"{assert!(!f.world.home.path().join("entered").exists());}
    }
}

#[test]
fn canonical_worker_queue_rotates_beyond_its_inventory_limit() {
    use std::sync::Mutex;
    struct Count {seen:Arc<Mutex<std::collections::BTreeSet<PathBuf>>>}
    impl Runner for Count {
        fn run(&self,command:&Cmd)->Result<Output>{let input:Input=serde_json::from_str(command.stdin.as_deref().unwrap())?;self.seen.lock().unwrap().insert(input.project.clone());Ok(Output{code:Some(0),stdout:serde_json::to_string(&Sample{reachable:false,head:observation_head(&input.project)?})?,..Output::default()})}
        fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}
    }
    let root=tempfile::tempdir().unwrap();let env=Env::for_test(root.path(),&[]);let ctx=Ctx{env:&env,root:root.path().into(),config_dir:root.path().join("cfg"),runner:&crate::runner::RealRunner,detached_ticker:false};
    let projects=(0..129).map(|n|{let p=crate::project::create(root.path(),&format!("p{n:03}"),"",vec![]).unwrap();p.set_status(crate::project::Status::Paused).unwrap();let path=p.dir();let plan=migration::inspect(&path).unwrap();migration::apply(&path,&plan,true).unwrap();path}).collect::<Vec<_>>();
    let seen=Arc::new(Mutex::new(std::collections::BTreeSet::new()));let pool=Arc::new(Executor::new(crate::executor::Limits::default(),Arc::new(Count{seen:seen.clone()})).unwrap());let mut reads=Reads::new(pool.clone());
    for _ in 0..24 {
        for path in &projects{reads.poll(&ctx,path).unwrap();}
        assert!(reads.offers.len()<=OFFER_LIMIT);assert!(reads.classified.len()<=OFFER_LIMIT);assert!(reads.unknown());
        assert!(reads.admit().is_empty());assert!(reads.pending.len()<=PENDING_LIMIT);finish(&mut reads);
    }
    assert_eq!(seen.lock().unwrap().len(),129);assert!(pool.stop(Duration::from_secs(3)));
}

#[test]
fn canonical_worker_negative_liveness_is_invalidated_by_rebinding() {
    let f=Fixture::new("failed");let pool=f.pool();let mut reads=Reads::new(pool.clone());reads.poll(&f.ctx(),&f.path).unwrap();reads.admit();finish(&mut reads);
    assert!(matches!(reads.poll(&f.ctx(),&f.path).unwrap(),Poll::Ready(false)));
    reads.begin_pass();assert!(matches!(reads.poll(&f.ctx(),&f.path).unwrap(),Poll::Pending));assert!(!reads.unknown());
    let before=runtime::snapshot(&f.path).unwrap();let binding=before.runtime_bindings.iter().find(|b|!b.identity.pane_id.is_empty()).unwrap();
    let mut route=herdr_projects::domain::RuntimeRoute::from_identity(&binding.identity);route.pane_id="new-pane".into();
    runtime::rebind(&f.path,&binding.id,binding.revision,before.head,&route).unwrap();
    fs::write(f.world.home.path().join("mode"),"blocked").unwrap();reads.admit();
    assert!(matches!(reads.poll(&f.ctx(),&f.path).unwrap(),Poll::Pending));assert!(reads.unknown(),"old absence must not allow idle exit while the changed inventory is pending");
    assert!(pool.stop(Duration::from_secs(3)));
}

#[test]
fn canonical_worker_repeated_negative_completions_clear_exit_veto() {
    let f=Fixture::new("failed");let pool=f.pool();let mut reads=Reads::new(pool.clone());reads.poll(&f.ctx(),&f.path).unwrap();
    for _ in 0..4 {
        assert!(reads.admit().is_empty());finish(&mut reads);
        assert!(matches!(reads.poll(&f.ctx(),&f.path).unwrap(),Poll::Ready(false)));
        assert!(!reads.unknown(),"a fresh negative completion supersedes the previous head");
    }
    assert!(pool.stop(Duration::from_secs(3)));
}

#[test]
fn canonical_worker_slow_probe_does_not_block_legacy_status() {
    struct NoCalls;
    impl Runner for NoCalls {fn run(&self,command:&Cmd)->Result<Output>{panic!("synchronous ticker command: {}",command.display())}fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{panic!("synchronous socket")}}
    let f=Fixture::new("blocked");let helper=f.env.herdr_bin();let script=fs::read_to_string(&helper).unwrap().replace("import pathlib,sys,json,time","import pathlib,sys,json,time,os").replace("if mode=='blocked':time.sleep(60)","if mode=='blocked' and pathlib.Path(os.environ['HERDR_SOCKET_PATH']).name=='canonical.sock':time.sleep(60)");fs::write(&helper,script).unwrap();
    let p=crate::project::create(&f.world.root,"healthy","",vec![]).unwrap();let socket=f.world.home.path().join("healthy.sock");let _listener=UnixListener::bind(&socket).unwrap();p.update_coordinator(|c|c.socket=socket.display().to_string()).unwrap();
    let t=crate::thread::allocate(&p,|t|{t.status=crate::thread::Status::Open;t.pane_id="p".into();t.workspace_id="w".into();t.tab_id="t".into();t.cwd="/fixture".into();t.agent="claude".into();t.agent_name="fixture".into();t.last_state="working".into();t.last_group="working".into();}).unwrap();
    let runner=Arc::new(crate::token_jobs::JobRunner{inner:Arc::new(ProbeRunner{inner:Arc::new(crate::local_observations::ProbeRunner{inner:Arc::new(crate::runner::RealRunner)})})});
    let pool=Arc::new(Executor::new(crate::executor::Limits::default(),runner).unwrap());let ctx=Ctx{runner:&NoCalls,..f.ctx()};let mut memory=crate::steps::Memory::new(&ctx);
    memory.canonical_observations=Some(Reads::new(pool.clone()));memory.local_observations=Some(crate::local_observations::Reads::new(pool.clone()));memory.copy_jobs=Some(crate::copy_jobs::Queue::new(pool.clone()));
    let before=runtime::snapshot(&f.path).unwrap();let started=Instant::now();crate::ticker::tick_for_test(&ctx,&mut memory);assert!(started.elapsed()<Duration::from_secs(1));assert!(memory.observations_unknown());
    let end=Instant::now()+Duration::from_secs(5);while crate::thread::load(&p,&t.id).unwrap().last_state!="idle"{assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));crate::ticker::tick_for_test(&ctx,&mut memory);}
    assert!(pool.stop(Duration::from_secs(3)));assert_eq!(runtime::snapshot(&f.path).unwrap(),before);
}

#[test]
fn canonical_worker_leaves_a_scheduling_turn_between_probes() {
    let(world,path)=crate::canonical_controller::tests::routine_fixture(&[("a-first",b"touch MUST_NOT_EXECUTE\n",1000),("b-second",b"touch MUST_NOT_EXECUTE\n",1000)]);
    let pool=Arc::new(Executor::new(crate::executor::Limits::default(),Arc::new(ProbeRunner{inner:Arc::new(crate::runner::RealRunner)})).unwrap());let mut reads=Reads::new(pool.clone());
    for turn in 0..2 {
        let result=crate::canonical_controller::poll_queued(&world.ctx(),&path,turn,&mut reads).unwrap();assert!(result.scheduled_work,"{:?}",result.operation_error);
        assert!(reads.admit().is_empty());finish(&mut reads);
    }
    assert_eq!(runtime::snapshot(&path).unwrap().routine_occurrences.len(),2);assert!(!path.join("MUST_NOT_EXECUTE").exists());assert!(pool.stop(Duration::from_secs(3)));
}

#[test]
fn canonical_worker_batches_leave_root_exclusive_notifications_a_turn() {
    use crate::notification_delivery;
    use crate::runner::fake::ok;
    let f=Fixture::new("blocked");let(world,path,task)=notification_delivery::tests::fixture_with_items(3);
    world.runner.on("--version",ok("herdr 0.9.1")).on("notification show",ok(r#"{"result":{"shown":true}}"#));
    let slow=crate::project::create(&world.root,"slow","",vec![]).unwrap();slow.set_status(crate::project::Status::Paused).unwrap();let slow=slow.dir();let plan=migration::inspect(&slow).unwrap();migration::apply(&slow,&plan,true).unwrap();
    let original=runtime::snapshot(&f.path).unwrap();let route=herdr_projects::domain::RuntimeRoute::from_identity(&original.runtime_bindings.iter().find(|b|!b.identity.pane_id.is_empty()).unwrap().identity);
    runtime::create_binding(&slow,None,None,runtime::snapshot(&slow).unwrap().head,&route).unwrap();
    let ctx=Ctx{env:&f.env,root:world.root.clone(),config_dir:world.ctx().config_dir,runner:&world.runner,detached_ticker:false};
    let pool=f.pool();let mut reads=Reads::new(pool.clone());
    for round in 0..3 {
        reads.poll(&ctx,&slow).unwrap();reads.poll(&ctx,&path).unwrap();assert!(reads.admit().is_empty());
        let end=Instant::now()+Duration::from_secs(5);
        while reads.pending.len()!=1||!f.world.home.path().join("entered").exists(){reads.begin_pass();assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}
        let op=notification_delivery::enqueue(&ctx,&path,&task,runtime::snapshot(&path).unwrap().head).unwrap();
        let head=runtime::snapshot(&path).unwrap().head;
        for _ in 0..3 {
            crate::canonical_controller::poll_queued(&ctx,&path,0,&mut reads).unwrap();reads.admit();
            assert_eq!(reads.pending.len(),1,"the completed neighbor must not replenish a still-held root batch");
            assert_eq!(runtime::snapshot(&path).unwrap().head,head);
            assert_eq!(world.runner.count("notification show"),round);
        }
        for pending in reads.pending.values(){pending.ticket.cancel();}finish(&mut reads);
        // This is the normal ticker order: effects first, then next admission.
        crate::canonical_controller::poll_queued(&ctx,&path,0,&mut reads).unwrap();
        assert_eq!(world.runner.count("notification show"),round+1);
        assert_eq!(runtime::snapshot(&path).unwrap().deliveries.iter().find(|d|d.operation==op.id).unwrap().state,herdr_projects::operations::DeliveryState::Confirmed);
        let snapshot=runtime::snapshot(&path).unwrap();let item=snapshot.inbox.iter().find(|i|!i.seen).unwrap();
        runtime::update_inbox(&path,snapshot.head,&[item.content.id.clone()],false).unwrap();
        fs::remove_file(f.world.home.path().join("entered")).unwrap();
    }
    assert!(pool.stop(Duration::from_secs(3)));
}

#[test]
fn canonical_worker_and_routine_admission_take_separate_project_turns() {
    use std::sync::atomic::{AtomicBool,Ordering};
    struct Gate {held:Arc<AtomicBool>,inner:Arc<dyn Runner+Send+Sync>}
    impl Runner for Gate {
        fn run(&self,command:&Cmd)->Result<Output>{if command.program==JOB{while self.held.load(Ordering::SeqCst){ensure!(!command.cancellation.as_ref().unwrap().is_cancelled()&&Instant::now()<command.deadline.unwrap(),"fixture cancelled");std::thread::sleep(Duration::from_millis(5));}}self.inner.run(command)}
        fn socket_request(&self,p:&Path,s:&str,t:Duration)->Result<String>{self.inner.socket_request(p,s,t)}
    }
    let(world,path)=crate::canonical_controller::tests::routine_fixture(&[("turns",b"touch STARTED\nsleep 1\ntouch COMPLETED\n",5000)]);
    let held=Arc::new(AtomicBool::new(true));let runner=Arc::new(Gate{held:held.clone(),inner:Arc::new(crate::routine_jobs::JobRunner{inner:Arc::new(ProbeRunner{inner:Arc::new(crate::runner::RealRunner)})})});let pool=Arc::new(Executor::new(crate::executor::Limits::default(),runner).unwrap());
    let mut reads=Reads::new(pool.clone());reads.poll(&world.ctx(),&path).unwrap();reads.admit();let mut memory=crate::steps::Memory::new(&world.ctx());memory.canonical_observations=Some(reads);memory.routine_jobs=Some(crate::routine_jobs::Queue::new(pool.clone()));
    crate::ticker::tick_for_test(&world.ctx(),&mut memory);assert!(!memory.routine_jobs.as_ref().unwrap().pending());assert!(!path.join("STARTED").exists());
    let snapshot=runtime::snapshot(&path).unwrap();assert_eq!(snapshot.routine_occurrences.len(),1);assert_eq!(snapshot.deliveries[0].attempts,0);
    held.store(false,Ordering::SeqCst);let end=Instant::now()+Duration::from_secs(5);while pool.metrics().running[0]+pool.metrics().queued[0]>0{assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}
    crate::ticker::tick_for_test(&world.ctx(),&mut memory);assert!(memory.routine_jobs.as_ref().unwrap().pending_project(path.to_str().unwrap()));assert!(!memory.canonical_observations.as_ref().unwrap().pending_project(path.to_str().unwrap()));
    while !path.join("STARTED").exists(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}
    crate::ticker::tick_for_test(&world.ctx(),&mut memory);assert!(!memory.canonical_observations.as_ref().unwrap().pending_project(path.to_str().unwrap()));
    while runtime::snapshot(&path).unwrap().deliveries[0].state!=herdr_projects::operations::DeliveryState::Confirmed{assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}
    assert!(path.join("COMPLETED").exists());assert!(pool.stop(Duration::from_secs(3)));
}
