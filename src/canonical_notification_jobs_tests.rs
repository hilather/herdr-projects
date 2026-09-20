use super::*;
use std::{fs,os::unix::{fs::PermissionsExt,net::UnixListener}};
use herdr_projects::{domain::{RuntimeRoute,ProjectState},operations::DeliveryState};
struct Fixture {world:crate::scenarios::World,path:PathBuf,env:Env,operation:Operation,_socket:UnixListener}
impl Fixture {
    fn new(mode:&str)->Self {
        let(world,path,task)=crate::notification_delivery::tests::fixture();
        let socket=world.home.path().join("notification.sock");let listener=UnixListener::bind(&socket).unwrap();let snapshot=runtime::snapshot(&path).unwrap();let binding=snapshot.runtime_bindings.iter().find(|b|b.id=="coordinator").unwrap();
        runtime::rebind(&path,&binding.id,binding.revision,snapshot.head,&RuntimeRoute{socket:socket.display().to_string(),..Default::default()}).unwrap();
        crate::reconcile_live::run(&world.ctx(),&path,true).unwrap();let snapshot=runtime::snapshot(&path).unwrap();runtime::set_state(&path,snapshot.head,snapshot.control.unwrap().revision,ProjectState::Active,&world.ctx().config_dir.join("config.toml")).unwrap();
        let helper=world.home.path().join("herdr");let env=Env::for_test(world.home.path(),&[("HERDR_BIN_PATH",helper.to_str().unwrap())]);fs::write(world.home.path().join("mode"),mode).unwrap();
        fs::write(&helper,format!(r#"#!/usr/bin/python3
import sys,json,pathlib,fcntl,sqlite3,time
root=pathlib.Path({home:?});project=pathlib.Path({project:?})
if sys.argv[1:]==['remote-api-bridge','--check']:print('herdr-api-bridge-v1');sys.exit(0)
if sys.argv[1:] in [['agent','list'],['pane','list']]:
 p={{'pane_id':'p','workspace_id':'w','tab_id':'t','cwd':'/fixture','agent':'claude','name':'fixture','agent_status':'idle'}}
 print(json.dumps({{'result':{{'agents' if sys.argv[1]=='agent' else 'panes':[p]}}}}));sys.exit(0)
if sys.argv[1:]!=['remote-api-bridge']:sys.exit(4)
request=json.loads(sys.stdin.readline())
assert request['method']=='notification.show'
for path in [project.parent/'.execution.lock',project/'.state'/'effect.lock',project.parent/'.routine-execution.lock']:
 with open(path,'r+') as f:
  try:fcntl.flock(f,fcntl.LOCK_EX|fcntl.LOCK_NB)
  except BlockingIOError:pass
  else:(root/'WRONG_LOCK').write_text(str(path));sys.exit(5)
db=sqlite3.connect(project/'.state'/'state.db')
assert db.execute("select count(*) from operation_delivery where state='claimed'").fetchone()[0]==1
with open(root/'sent','a') as f:f.write('send\n')
mode=(root/'mode').read_text()
if mode=='blocked':time.sleep(60)
if mode=='lost':sys.exit(0)
if mode=='malformed':print('bad json');sys.exit(0)
if mode=='oversized':print('x'*1100000);sys.exit(0)
if mode=='config-change':
 (root/'cfg').mkdir(exist_ok=True)
 (root/'cfg'/'config.toml').write_text('# changed after effect')
result={{'type':'notification_show','shown':True,'reason':'shown'}}
if mode in ['disabled','rate_limited','no_foreground_client','busy']:result.update(shown=False,reason=mode)
if mode=='unknown-negative':result.update(shown=False,reason='unknown')
if mode=='contradictory':result.update(reason='busy')
print(json.dumps({{'id':'wrong' if mode=='foreign' else request['id'],'result':result}}))
"#,home=world.home.path().display().to_string(),project=path.display().to_string())).unwrap();fs::set_permissions(&helper,fs::Permissions::from_mode(0o700)).unwrap();
        let operation=crate::notification_delivery::enqueue(&world.ctx(),&path,&task,runtime::snapshot(&path).unwrap().head).unwrap();Self{world,path,env,operation,_socket:listener}
    }
    fn ctx(&self)->Ctx<'_>{Ctx{env:&self.env,root:self.world.root.clone(),config_dir:self.world.ctx().config_dir,runner:&crate::runner::RealRunner,detached_ticker:false}}
    fn input(&self)->Input{let snapshot=runtime::snapshot(&self.path).unwrap();let binding=snapshot.runtime_bindings.iter().find(|b|b.id=="coordinator").unwrap();serde_json::from_str(request(&self.ctx(),&self.path,&self.operation,1,&binding.identity.socket).unwrap().command.stdin.as_deref().unwrap()).unwrap()}
    fn delivery(&self)->herdr_projects::operations::Delivery{runtime::snapshot(&self.path).unwrap().deliveries.into_iter().find(|d|d.operation==self.operation.id).unwrap()}
}
fn control()->Control{Control{deadline:Instant::now()+BUDGET,cancellation:Default::default()}}

#[test]
fn canonical_notification_confirms_once_with_claim_and_inherited_ownership() {
    let f=Fixture::new("ok");let input=f.input();execute(&input,&control()).unwrap();assert_eq!(f.delivery().state,DeliveryState::Confirmed);assert_eq!(f.delivery().attempts,1);
    assert!(execute(&input,&control()).is_err());assert_eq!(fs::read_to_string(f.world.home.path().join("sent")).unwrap(),"send\n");assert!(!f.world.home.path().join("WRONG_LOCK").exists());
    assert!(ProjectGuard::acquire(&f.path).is_ok());
}
#[test]
fn canonical_notification_only_verified_native_negatives_allow_retry() {
    for mode in ["disabled","rate_limited","no_foreground_client","busy"] {
        let f=Fixture::new(mode);execute(&f.input(),&control()).unwrap();let delivery=f.delivery();assert_eq!(delivery.state,DeliveryState::Pending,"{mode}");assert_eq!(delivery.attempts,1);assert!(matches!(delivery.last_outcome,Some(Outcome::Retryable{..})));
    }
    for mode in ["lost","malformed","foreign","unknown-negative","contradictory","oversized","config-change"] {
        let f=Fixture::new(mode);let input=f.input();execute(&input,&control()).unwrap();assert_eq!(f.delivery().state,DeliveryState::Ambiguous,"{mode}");assert!(execute(&input,&control()).is_err());assert_eq!(fs::read_to_string(f.world.home.path().join("sent")).unwrap(),"send\n","{mode}");
    }
}
#[test]
fn canonical_notification_frozen_selection_and_expired_work_never_send() {
    for mode in ["config","socket","operation","revision","cancelled","expired"] {
        let f=Fixture::new("ok");let mut input=f.input();let mut control=control();
        match mode {
            "config"=>{fs::create_dir_all(&input.config).unwrap();fs::write(input.config.join("config.toml"),"# changed").unwrap();},
            "socket"=>{fs::remove_file(&input.socket).unwrap();let _new=UnixListener::bind(&input.socket).unwrap();},
            "operation"=>input.operation_digest="0".repeat(64),
            "revision"=>input.revision+=1,
            "cancelled"=>control.cancellation.cancel(),
            "expired"=>control.deadline=Instant::now(),
            _=>unreachable!(),
        }
        assert!(execute(&input,&control).is_err(),"{mode}");assert_eq!(f.delivery().attempts,0);assert!(!f.world.home.path().join("sent").exists());
    }
}

// Model arrival from another canonical producer without introducing an effect
// adapter into this notification fixture. Preserve the inbox payload hash.
fn add_inbox(f:&Fixture)->String {
    use sha2::{Digest,Sha256};
    let mut content=runtime::snapshot(&f.path).unwrap().inbox[0].content.clone();content.id="new-inbox-item".into();
    let payload=serde_json::to_string(&content).unwrap();let db=rusqlite::Connection::open(f.path.join(".state/state.db")).unwrap();
    db.execute("INSERT INTO inbox_items VALUES(?1,1,?2,?3,0,0)",rusqlite::params![content.id,payload,format!("{:x}",Sha256::digest(payload.as_bytes()))]).unwrap();content.id
}
#[test]
fn canonical_notification_uncertainty_blocks_overlapping_batches_across_changed_authority() {
    for change in ["restart","route","config","task"] {
        let f=Fixture::new("lost");execute(&f.input(),&control()).unwrap();assert_eq!(f.delivery().state,DeliveryState::Ambiguous);add_inbox(&f);
        let snapshot=runtime::snapshot(&f.path).unwrap();
        match change {
            "route"=>{let b=snapshot.runtime_bindings.iter().find(|b|b.id=="coordinator").unwrap();runtime::rebind(&f.path,&b.id,b.revision,snapshot.head,&RuntimeRoute{socket:"/different/session.sock".into(),..Default::default()}).unwrap();},
            "config"=>{fs::create_dir_all(&f.ctx().config_dir).unwrap();fs::write(f.ctx().config_dir.join("config.toml"),"# changed authority").unwrap();},
            "task"=>{runtime::rename_task(&f.path,f.operation.task.as_ref().unwrap(),"renamed".into(),f.operation.expected_revision,snapshot.head).unwrap();},
            _=>{},
        }
        if matches!(change,"route"|"config") {
            crate::reconcile_live::run(&f.world.ctx(),&f.path,true).unwrap();let s=runtime::snapshot(&f.path).unwrap();assert!(runtime::set_state(&f.path,s.head,s.control.unwrap().revision,ProjectState::Active,&f.ctx().config_dir.join("config.toml")).is_err(),"unresolved intents also prevent reactivation");
        }
        migration::recover(&f.path,true).unwrap();let head=runtime::snapshot(&f.path).unwrap().head;
        let error=crate::notification_delivery::enqueue(&f.ctx(),&f.path,f.operation.task.as_ref().unwrap(),head).unwrap_err();assert!(format!("{error:#}").contains("overlaps"),"{change}: {error:#}");
        assert_eq!(fs::read_to_string(f.world.home.path().join("sent")).unwrap(),"send\n");
    }
}
#[test]
fn canonical_notification_consumption_or_explicit_retirement_resolves_overlap() {
    for consume in [true,false] {
        let f=Fixture::new("lost");execute(&f.input(),&control()).unwrap();add_inbox(&f);let snapshot=runtime::snapshot(&f.path).unwrap();
        if consume {let notice=Notification::decode(&f.operation).unwrap();runtime::update_inbox(&f.path,snapshot.head,&notice.inbox_ids,false).unwrap();}
        else {runtime::retire_operation(&f.path,&f.operation.id,f.delivery().revision,snapshot.head,"operator inspected and accepts the possible earlier effect").unwrap();}
        let op=crate::notification_delivery::enqueue(&f.ctx(),&f.path,f.operation.task.as_ref().unwrap(),runtime::snapshot(&f.path).unwrap().head).unwrap();
        let mut input=f.input();input.operation=op.id.clone();input.operation_digest=fingerprint(&op).unwrap();fs::write(f.world.home.path().join("mode"),"ok").unwrap();execute(&input,&control()).unwrap();
        let snapshot=runtime::snapshot(&f.path).unwrap();assert_eq!(snapshot.deliveries.iter().find(|d|d.operation==op.id).unwrap().state,DeliveryState::Confirmed);
        assert_eq!(f.delivery().state,if consume{DeliveryState::Ambiguous}else{DeliveryState::PermanentFailure});
    }
}
#[test]
fn canonical_notification_malformed_unresolved_history_is_not_assumed_disjoint() {
    let f=Fixture::new("lost");execute(&f.input(),&control()).unwrap();let new=add_inbox(&f);let mut snapshot=runtime::snapshot(&f.path).unwrap();
    for item in &mut snapshot.inbox{item.seen=item.content.id!=new;}
    for change in ["decode","identity","revision","config","digest"] {
        let mut bad=snapshot.clone();let operation=bad.operations.iter_mut().find(|op|op.id==f.operation.id).unwrap();
        match change {
            "decode"=>operation.payload=serde_json::json!({"invalid":"old notification"}),
            "identity"=>operation.payload["inbox_ids"]=serde_json::json!(["../bad"]),
            "revision"=>operation.payload["binding_revision"]=serde_json::json!(0),
            "config"=>operation.payload["config"]["path"]=serde_json::json!("relative"),
            "digest"=>operation.payload["config"]["digest"]=serde_json::json!("not-a-hash"),
            _=>unreachable!(),
        }
        assert!(herdr_projects::operations::notification::build(&bad,f.operation.task.as_ref().unwrap(),"notify",crate::notification_delivery::config(&f.ctx(),&f.path).unwrap(),jiff::Timestamp::now().as_millisecond()).is_err(),"{change}");
    }
}

#[test]
fn canonical_notification_ticker_defers_effect_without_blocking_legacy_status() {
    struct NoCalls;impl Runner for NoCalls{fn run(&self,command:&Cmd)->Result<Output>{panic!("synchronous ticker effect: {}",command.display())}fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{panic!("synchronous socket")}}
    let f=Fixture::new("blocked");let p=crate::project::create(&f.world.root,"healthy","",vec![]).unwrap();let socket=f.world.home.path().join("healthy.sock");let _listener=UnixListener::bind(&socket).unwrap();p.update_coordinator(|c|c.socket=socket.display().to_string()).unwrap();
    let t=crate::thread::allocate(&p,|t|{t.status=crate::thread::Status::Open;t.pane_id="p".into();t.workspace_id="w".into();t.tab_id="t".into();t.cwd="/fixture".into();t.agent="claude".into();t.agent_name="fixture".into();t.last_state="working".into();t.last_group="working".into();}).unwrap();
    let runner=Arc::new(JobRunner{inner:Arc::new(crate::canonical_controller::observations::ProbeRunner{inner:Arc::new(crate::local_observations::ProbeRunner{inner:Arc::new(crate::runner::RealRunner)})})});
    let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),runner).unwrap());let ctx=Ctx{runner:&NoCalls,..f.ctx()};let mut memory=crate::steps::Memory::new(&ctx);memory.copy_jobs=Some(crate::copy_jobs::Queue::new(pool.clone()));memory.local_observations=Some(crate::local_observations::Reads::new(pool.clone()));memory.canonical_observations=Some(crate::canonical_controller::observations::Reads::new(pool.clone()));
    let started=Instant::now();crate::ticker::tick_for_test(&ctx,&mut memory);assert!(started.elapsed()<Duration::from_secs(1));
    let end=Instant::now()+Duration::from_secs(5);while !f.world.home.path().join("sent").exists(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}
    assert!(!memory.canonical_observations.as_ref().unwrap().pending_project(f.path.to_str().unwrap()));
    while crate::thread::load(&p,&t.id).unwrap().last_state!="idle"{assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));crate::ticker::tick_for_test(&ctx,&mut memory);}
    assert_eq!(f.delivery().state,DeliveryState::Claimed);assert!(ProjectGuard::acquire(&p.dir()).is_ok());
    assert!(pool.stop(Duration::from_secs(3)));assert_eq!(f.delivery().state,DeliveryState::Ambiguous);assert!(ProjectGuard::acquire(&f.path).is_ok());
}

#[test]
fn canonical_notification_overlapping_pending_workers_cannot_both_send() {
    let f=Fixture::new("ok");let older=f.input();add_inbox(&f);
    let op=crate::notification_delivery::enqueue(&f.ctx(),&f.path,f.operation.task.as_ref().unwrap(),runtime::snapshot(&f.path).unwrap().head).unwrap();
    let mut newer=f.input();newer.operation=op.id.clone();newer.operation_digest=fingerprint(&op).unwrap();
    let (first,second)=std::thread::scope(|scope|{let first=scope.spawn(||execute(&older,&control()));let second=scope.spawn(||execute(&newer,&control()));(first.join().unwrap(),second.join().unwrap())});
    assert!(first.is_err());if second.is_err(){execute(&newer,&control()).unwrap();}
    assert_eq!(f.delivery().attempts,0);let snapshot=runtime::snapshot(&f.path).unwrap();let delivery=snapshot.deliveries.iter().find(|d|d.operation==op.id).unwrap();assert_eq!(delivery.attempts,1);assert_eq!(delivery.state,DeliveryState::Confirmed);
    assert_eq!(fs::read_to_string(f.world.home.path().join("sent")).unwrap(),"send\n");
}

#[test]
fn canonical_notification_queue_completion_cannot_certify_delivery() {
    struct Forged;impl Runner for Forged{fn run(&self,_:&Cmd)->Result<Output>{Ok(Output{code:Some(0),..Output::default()})}fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}}
    let f=Fixture::new("ok");let before=runtime::snapshot(&f.path).unwrap();let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(Forged)).unwrap());let mut queue=crate::copy_jobs::Queue::new(pool.clone());let input=f.input();
    queue.offer_canonical_notification(&f.ctx(),&f.path,&f.operation,1,input.socket.to_str().unwrap()).unwrap();assert!(queue.admit().is_empty());let end=Instant::now()+Duration::from_secs(3);
    while queue.pending(){assert!(queue.drain().is_empty());assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}
    assert_eq!(runtime::snapshot(&f.path).unwrap(),before);assert!(!f.world.home.path().join("sent").exists());assert!(pool.stop(Duration::from_secs(3)));
}

#[test]
fn canonical_notification_hint_cannot_authorize_a_paused_delivery() {
    let f=Fixture::new("ok");let before=runtime::snapshot(&f.path).unwrap();runtime::set_state(&f.path,before.head,before.control.unwrap().revision,ProjectState::Paused,&f.ctx().config_dir.join("config.toml")).unwrap();let before=runtime::snapshot(&f.path).unwrap();
    let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(JobRunner{inner:Arc::new(crate::runner::RealRunner)})).unwrap());let mut queue=crate::copy_jobs::Queue::new(pool.clone());let mut reads=crate::canonical_controller::observations::Reads::new(pool.clone());
    let result=crate::canonical_controller::poll_queued_effects(&f.ctx(),&f.path,0,&mut reads,Some(&mut queue)).unwrap();assert!(result.operation_error.is_none(),"{:?}",result.operation_error);assert!(queue.offered());
    assert!(queue.admit().is_empty());let end=Instant::now()+Duration::from_secs(3);let mut errors=Vec::new();while queue.pending(){errors.extend(queue.drain());assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}
    assert_eq!(errors.len(),1);assert_eq!(runtime::snapshot(&f.path).unwrap(),before);assert!(!f.world.home.path().join("sent").exists());assert!(pool.stop(Duration::from_secs(2)));
}
