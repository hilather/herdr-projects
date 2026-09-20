use super::*;
use std::{fs,os::unix::{fs::PermissionsExt,net::UnixListener}};
struct Fixture {root:tempfile::TempDir,project:Project,input:Input,_listener:UnixListener}
impl Fixture {
    fn new()->Self {
        let root=tempfile::tempdir().unwrap();let project=project::create(root.path(),"demo","",vec![]).unwrap();
        let socket=root.path().join("session.sock");let listener=UnixListener::bind(&socket).unwrap();
        project.update_coordinator(|c|{c.socket=socket.display().to_string();c.workspace_id="w".into();c.tab_id="tab".into();c.pane_id="p".into();c.cwd="/fixture".into();c.agent_name="coordinator".into();c.prime_pending=true;c.prime_request=1;}).unwrap();
        let helper=root.path().join("herdr");let env=paths::Env::for_test(root.path(),&[("HERDR_BIN_PATH",helper.to_str().unwrap())]);let runner=crate::runner::RealRunner;
        let ctx=Ctx{env:&env,root:root.path().into(),config_dir:root.path().join("cfg"),runner:&runner,detached_ticker:false};
        let input=serde_json::from_str(request(&ctx,&project,&project.coordinator().unwrap()).unwrap().command.stdin.as_ref().unwrap()).unwrap();
        let f=Self{root,project,input,_listener:listener};f.script("confirmed");f
    }
    fn script(&self,mode:&str) {
        let code=format!(r#"#!/usr/bin/python3
import json,sys,time
mode={mode:?}
a={{'workspace_id':'w','tab_id':'tab','pane_id':'p','terminal_id':'terminal','cwd':'/fixture','name':'coordinator','agent':'claude','agent_status':'working' if mode=='busy' else 'idle'}}
args=sys.argv[1:]
if args==['remote-api-bridge','--check']:
 print('unsupported' if mode=='unsupported' else 'herdr-api-bridge-v1');sys.exit(0)
if args==['agent','list']:
 print(json.dumps({{'result':{{'agents':[a,a] if mode=='duplicate' else [a]}}}}));sys.exit(0)
if args==['pane','list']:
 print(json.dumps({{'result':{{'panes':[a]}}}}));sys.exit(0)
if args==['remote-api-bridge']:
 request=json.load(sys.stdin)
 assert request['method']=='agent.prompt'
 with open({sent:?},'a') as f:f.write('send')
 if mode=='lost':sys.exit(1)
 if mode=='blocked':time.sleep(60)
 if mode=='foreign':a['workspace_id']='foreign'
 if mode=='terminal':a['terminal_id']='foreign'
 if mode=='kind':a['agent']='other'
 print(json.dumps({{'id':'wrong' if mode=='wrongid' else request['id'],'result':{{'type':'agent_prompted','agent':a}}}}));sys.exit(0)
# Token refreshes are outside the priming worker.
if args[:2]==['pane','report-tokens']:sys.exit(0)
sys.exit(2)
"#,sent=self.root.path().join("sent").display().to_string());
        fs::write(&self.input.herdr,code).unwrap();fs::set_permissions(&self.input.herdr,fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn record(&self)->Coordinator {self.project.try_coordinator().unwrap().unwrap()}
    fn recover(&self) {let guard=ProjectGuard::acquire(&self.project.dir()).unwrap();recover(&self.project,&guard).unwrap();}
    fn sent(&self)->bool{self.root.path().join("sent").exists()}
    fn refresh(&mut self) {let c=self.record();self.input.execution=execution(&c);self.input.request=c.prime_request;self.input.sequence=c.prime_sequence;}
}
#[test]
fn concrete_prime_confirms_once_and_requires_an_explicit_new_request() {
    let mut f=Fixture::new();execute(&f.input,&Control::default()).unwrap();assert!(!f.record().prime_pending);assert_eq!(f.record().prime_claim.unwrap().delivery.phase,Phase::Confirmed);
    assert!(execute(&f.input,&Control::default()).is_err());
    f.project.update_coordinator(|c|c.prime_pending=true).unwrap();assert!(execute(&f.input,&Control::default()).is_err());
    f.project.update_coordinator(|c|c.prime_request+=1).unwrap();f.refresh();execute(&f.input,&Control::default()).unwrap();
    assert_eq!(fs::read(f.root.path().join("sent")).unwrap(),b"sendsend");assert_eq!(f.record().prime_sequence,2);
}
#[test]
fn lost_and_mismatched_prime_replies_recover_once_without_replay() {
    for mode in ["lost","foreign","terminal","kind","wrongid"] {
        let mut f=Fixture::new();f.script(mode);assert!(execute(&f.input,&Control::default()).is_err(),"{mode}");assert!(f.sent());
        f.recover();f.recover();let c=f.record();assert!(c.prime_pending);assert_eq!(c.prime_claim.unwrap().delivery.phase,Phase::Uncertain);
        assert_eq!(crate::inbox::unhandled(&f.project).iter().filter(|i|i.id.starts_with("coordinator-prime-")).count(),1);
        assert!(execute(&f.input,&Control::default()).is_err());
        // A mutable route or settings edit never authorizes replay.
        f.project.update_coordinator(|c|c.cwd="/changed".into()).unwrap();f.refresh();assert!(ready(&f.record()).is_err());
        assert_eq!(fs::read(f.root.path().join("sent")).unwrap(),b"send");
    }
}
#[test]
fn coordinator_prime_preflight_refuses_busy_duplicate_unsupported_and_changed_authority() {
    for mode in ["busy","duplicate","unsupported","socket","settings","config","stale"] {
        let f=Fixture::new();match mode {
            "socket"=>{fs::remove_file(&f.input.socket).unwrap();fs::write(&f.input.socket,"replaced").unwrap();},
            "settings"=>{fs::write(f.project.project_md(),"changed").unwrap();},
            "config"=>{fs::create_dir_all(&f.input.config).unwrap();fs::write(f.input.config.join("config.toml"),"changed").unwrap();},
            "stale"=>{f.project.update_coordinator(|c|c.prime_request+=1).unwrap();},
            _=>f.script(mode),
        }
        assert!(execute(&f.input,&Control::default()).is_err(),"{mode}");assert!(!f.sent());assert!(f.record().prime_claim.is_none());
    }
}
#[test]
fn coordinator_prime_after_claim_changes_leave_recoverable_uncertainty() {
    for mode in ["cancel","settings","request"] {
        let mut f=Fixture::new();let control=Control::default();assert!(execute_with(&f.input,&control,||{
            match mode {"cancel"=>control.cancellation.cancel(),"settings"=>fs::write(f.project.project_md(),"changed")?,_=>{f.project.update_coordinator(|c|c.prime_request+=1)?;}}
            Ok(())
        }).is_err());assert!(!f.sent());f.recover();assert_eq!(f.record().prime_claim.unwrap().delivery.phase,Phase::Uncertain);
        if mode=="request" {assert_eq!(f.record().prime_request,2);f.refresh();execute(&f.input,&Control::default()).unwrap();assert!(!f.record().prime_pending);}
    }
}
#[test]
fn coordinator_prime_checks_same_project_threads_and_neighbor_aliases() {
    for mode in ["own-thread","neighbor","alias","corrupt","canonical"] {
        let f=Fixture::new();
        if mode=="own-thread" {thread::allocate(&f.project,|t|{t.status=thread::Status::Resolved;t.pane_id="p".into();}).unwrap();}
        else {
            let other=project::create(f.root.path(),"other","",vec![]).unwrap();let mut socket=f.input.socket.clone();
            if mode=="alias" {socket=f.root.path().join("alias.sock");std::os::unix::fs::symlink(&f.input.socket,&socket).unwrap();}
            other.update_coordinator(|c|{c.socket=socket.display().to_string();c.pane_id="p".into();}).unwrap();
            if mode=="corrupt" {fs::write(other.state_dir().join("coordinator.json"),"corrupt").unwrap();}
            if mode=="canonical" {
                #[cfg(feature="state-store")]
                {other.set_status(project::Status::Paused).unwrap();project::write_json(&other.state_dir().join("coordinator.json"),&Coordinator::default()).unwrap();let plan=herdr_projects::migration::inspect(&other.dir()).unwrap();herdr_projects::migration::apply(&other.dir(),&plan,true).unwrap();let snapshot=herdr_projects::runtime::snapshot(&other.dir()).unwrap();herdr_projects::runtime::rebind(&other.dir(),"coordinator",1,snapshot.head,&herdr_projects::domain::RuntimeRoute{socket:socket.display().to_string(),pane_id:"p".into(),workspace_id:"w".into(),tab_id:"tab".into(),cwd:"/fixture".into(),..Default::default()}).unwrap();}
                #[cfg(not(feature="state-store"))]
                fs::write(other.state_dir().join("format.json"),"{}").unwrap();
            }
        }
        assert!(execute(&f.input,&Control::default()).is_err(),"{mode}");assert!(!f.sent());assert!(f.record().prime_claim.is_none());
    }
}
#[test]
fn blocked_coordinator_prime_allows_neighbor_progress_and_cancels() {
    let f=Fixture::new();f.script("blocked");let other=project::create(f.root.path(),"other","",vec![]).unwrap();
    let input=f.input.clone();let control=Control::default();let worker_control=Control{deadline:control.deadline,cancellation:control.cancellation.clone()};
    let worker=std::thread::spawn(move||execute(&input,&worker_control));let end=Instant::now()+Duration::from_secs(10);
    while !f.sent(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}
    let guard=ProjectGuard::acquire(&other.dir()).unwrap();assert_eq!(other.try_status().unwrap(),project::Status::Active);drop(guard);
    assert!(ProjectGuard::acquire(&f.project.dir()).is_err());control.cancellation.cancel();assert!(worker.join().unwrap().is_err());f.recover();assert!(f.record().prime_claim.unwrap().delivery.notified);
}
#[test]
fn malformed_coordinator_cannot_be_silently_replaced() {
    let f=Fixture::new();let path=f.project.state_dir().join("coordinator.json");fs::write(&path,"corrupt {").unwrap();assert!(f.project.update_coordinator(|c|c.prime_pending=false).is_err());assert_eq!(fs::read_to_string(path).unwrap(),"corrupt {");
}
#[test]
fn ticker_queues_prime_without_a_synchronous_send_or_forged_receipt() {
    let f=Fixture::new();let env=paths::Env::for_test(f.root.path(),&[("HERDR_BIN_PATH",&f.input.herdr)]);let runner=crate::runner::RealRunner;
    let ctx=Ctx{env:&env,root:f.root.path().into(),config_dir:f.root.path().join("cfg"),runner:&runner,detached_ticker:false};
    struct Forged;impl Runner for Forged {fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{anyhow::bail!("unexpected socket request")} fn run(&self,_:&Cmd)->Result<Output>{Ok(Output{code:Some(0),..Default::default()})}}
    let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(Forged)).unwrap());
    let mut memory=crate::steps::Memory::new(&ctx);memory.copy_jobs=Some(crate::copy_jobs::Queue::new(pool.clone()));
    crate::ticker::tick_project_with(&ctx,&f.project,&mut memory).unwrap();assert!(!f.sent());assert!(memory.copy_jobs.as_ref().unwrap().offered());
    memory.copy_jobs.as_mut().unwrap().admit();let end=Instant::now()+Duration::from_secs(5);
    while memory.copy_jobs.as_ref().unwrap().pending(){assert!(Instant::now()<end);memory.copy_jobs.as_mut().unwrap().drain();std::thread::sleep(Duration::from_millis(5));}
    assert!(f.record().prime_pending);assert!(f.record().prime_claim.is_none());assert!(!f.sent());assert!(pool.stop(Duration::from_secs(2)));
}
#[test]
fn coordinator_update_preserves_previous_bytes_when_claim_exceeds_record_limit() {
    let f=Fixture::new();let mut c=f.record();c.session="s".repeat(16*1024*1024-serde_json::to_string_pretty(&c).unwrap().len()-16);
    let path=f.project.state_dir().join("coordinator.json");project::write_json(&path,&c).unwrap();let before=fs::read(&path).unwrap();assert!(f.project.try_coordinator().is_ok());
    assert!(f.project.update_coordinator(|c|{c.prime_sequence=1;c.prime_claim=Some(Claim{request:1,delivery:herdr_projects::prompt_claim::Claim{sequence:1,execution:"a".repeat(64),prompt:"prime".into(),phase:Phase::Pending,error:String::new(),notified:false}});}).is_err());
    assert_eq!(fs::read(path).unwrap(),before);assert!(f.project.try_coordinator().is_ok());
}
#[test]
#[cfg(target_os="linux")]
fn coordinator_open_reprime_queues_an_explicit_new_request_preserving_uncertainty() {
    let mut f=Fixture::new();f.script("lost");assert!(execute(&f.input,&Control::default()).is_err());f.recover();let old=f.record().prime_claim;
    let env=paths::Env::for_test(f.root.path(),&[("HERDR_BIN_PATH",&f.input.herdr)]);let runner=crate::runner::RealRunner;
    let ctx=Ctx{env:&env,root:f.root.path().into(),config_dir:f.root.path().join("cfg"),runner:&runner,detached_ticker:false};
    crate::coordinator::open(&ctx,"demo",&crate::coordinator::OpenOptions{session:paths::SessionFlags{socket:Some(f.input.socket.clone()),..Default::default()},reprime:true,rebind:false}).unwrap();
    assert_eq!(f.record().prime_request,2);assert_eq!(f.record().prime_claim,old);assert!(f.record().prime_pending);assert_eq!(fs::read(f.root.path().join("sent")).unwrap(),b"send");
    f.script("confirmed");f.refresh();execute(&f.input,&Control::default()).unwrap();assert_eq!(fs::read(f.root.path().join("sent")).unwrap(),b"sendsend");assert!(!f.record().prime_pending);
}
