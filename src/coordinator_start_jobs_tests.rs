use super::*;
use std::{fs,os::unix::{fs::PermissionsExt,net::UnixListener}};
struct Fixture {root:tempfile::TempDir,project:Project,input:Input,_listener:UnixListener}
impl Fixture {
    fn new(mode:&str)->Self {
        let root=tempfile::tempdir().unwrap();let project=project::create(root.path(),"demo","",vec![]).unwrap();
        let socket=root.path().join("session.sock");let listener=UnixListener::bind(&socket).unwrap();
        project.update_coordinator(|c|{c.socket=socket.display().to_string();c.workspace_id="w".into();c.tab_id="tab".into();c.pane_id="p".into();c.cwd="/fixture".into();c.agent_name="coordinator".into();c.prime_pending=true;c.prime_request=1;}).unwrap();
        let helper=root.path().join("herdr");let env=paths::Env::for_test(root.path(),&[("HERDR_BIN_PATH",helper.to_str().unwrap())]);let runner=crate::runner::RealRunner;
        let ctx=Ctx{env:&env,root:root.path().into(),config_dir:root.path().join("cfg"),runner:&runner,detached_ticker:false};
        let input=serde_json::from_str(super::super::request_start(&ctx,&project,&project.coordinator().unwrap()).unwrap().command.stdin.as_ref().unwrap()).unwrap();
        let f=Self{root,project,input,_listener:listener};f.script(mode);f
    }
    fn script(&self,mode:&str) {
        let code=format!(r#"#!/usr/bin/python3
import json,sys,time
mode={mode:?}
a={{'workspace_id':'w','tab_id':'tab','pane_id':'p','terminal_id':'terminal','cwd':'/fixture','name':'coordinator','agent':'claude','agent_status':'blocked','launch_pending':True}}
args=sys.argv[1:]
if args==['remote-api-bridge','--check']:
 print('unsupported' if mode=='unsupported' else 'herdr-api-bridge-v1');sys.exit(0)
if args==['agent','list']:
 if mode=='foreign-busy':a['name']='foreign'
 print(json.dumps({{'result':{{'agents':[a] if mode in ['busy','foreign-busy'] else []}}}}));sys.exit(0)
if args==['pane','list']:
 if mode=='no-terminal':del a['terminal_id']
 print(json.dumps({{'result':{{'panes':[a,a] if mode=='duplicate' else [a]}}}}));sys.exit(0)
if args==['remote-api-bridge']:
 request=json.load(sys.stdin)
 assert request['method']=='agent.start'
 assert request['params']=={{'name':'coordinator','kind':'claude','pane_id':'p','args':[],'timeout_ms':20000}}
 with open({sent:?},'a') as f:f.write('start')
 if mode=='lost':sys.exit(1)
 if mode=='blocked':time.sleep(60)
 if mode=='foreign':a['workspace_id']='foreign'
 if mode=='terminal':a['terminal_id']='foreign'
 if mode=='kind':a['agent']='other'
 if mode=='native':del a['agent']
 if mode=='null':a['agent']=None
 print(json.dumps({{'id':'wrong' if mode=='wrongid' else request['id'],'result':{{'type':'wrong' if mode=='wrongtype' else 'agent_started','agent':a,'argv':['claude','wrong'] if mode=='args' else ['claude']}}}}));sys.exit(0)
sys.exit(2)
"#,sent=self.root.path().join("sent").display().to_string());
        fs::write(&self.input.herdr,code).unwrap();fs::set_permissions(&self.input.herdr,fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn record(&self)->Coordinator{self.project.try_coordinator().unwrap().unwrap()}
    fn recover(&self){let guard=ProjectGuard::acquire(&self.project.dir()).unwrap();recover(&self.project,&guard).unwrap();}
    fn sent(&self)->bool{self.root.path().join("sent").exists()}
}
#[test]
fn coordinator_start_confirms_submission_once_without_certifying_readiness() {
    for mode in ["confirmed","native","null"] {
        let f=Fixture::new(mode);execute(&f.input,&Control::default()).unwrap();let c=f.record();assert!(c.prime_pending);assert_eq!(c.launch_claim.unwrap().phase,LaunchPhase::Confirmed);assert_eq!(c.launch_attempts,1);
        assert!(super::super::ready(&f.record()).is_ok());assert!(execute(&f.input,&Control::default()).is_err());assert_eq!(fs::read(f.root.path().join("sent")).unwrap(),b"start");
    }
}
#[test]
fn coordinator_start_refuses_busy_foreign_ambiguous_and_unsupported_targets_before_claiming() {
    for mode in ["busy","foreign-busy","duplicate","unsupported","no-terminal"] {
        let f=Fixture::new(mode);assert!(execute(&f.input,&Control::default()).is_err(),"{mode}");assert!(!f.sent());assert!(f.record().launch_claim.is_none());assert_eq!(f.record().launch_attempts,0);
    }
}
#[test]
fn coordinator_start_lost_or_mismatched_acknowledgements_block_automatic_start_and_prime() {
    for mode in ["lost","foreign","terminal","kind","wrongid","wrongtype","args"] {
        let f=Fixture::new(mode);assert!(execute(&f.input,&Control::default()).is_err(),"{mode}");assert!(f.sent());f.recover();f.recover();let c=f.record();assert_eq!(c.launch_claim.unwrap().phase,LaunchPhase::Uncertain);
        assert!(super::super::ready(&f.record()).is_err());assert!(execute(&f.input,&Control::default()).is_err());assert_eq!(fs::read(f.root.path().join("sent")).unwrap(),b"start");
        assert_eq!(crate::inbox::unhandled(&f.project).iter().filter(|i|i.id.starts_with("coordinator-start-")).count(),1);
        f.project.update_coordinator(|c|c.cwd="/changed".into()).unwrap();assert!(ready(&f.record()).is_err());
        f.project.update_coordinator(|c|c.prime_request+=1).unwrap();assert!(ready(&f.record()).is_ok());
    }
}
#[test]
fn coordinator_start_after_claim_cancellation_and_changes_preserve_uncertainty() {
    for mode in ["cancel","config","request"] {
        let f=Fixture::new("confirmed");let control=Control::default();assert!(execute_with(&f.input,&control,||{
            match mode {"cancel"=>control.cancellation.cancel(),"config"=>{fs::create_dir_all(&f.input.config)?;fs::write(f.input.config.join("config.toml"),"# changed")?;},_=>{f.project.update_coordinator(|c|c.prime_request+=1)?;}}
            Ok(())
        }).is_err());assert!(!f.sent());f.recover();assert_eq!(f.record().launch_claim.unwrap().phase,LaunchPhase::Uncertain);assert_eq!(f.record().prime_request,if mode=="request"{2}else{1});
    }
}
#[test]
fn coordinator_start_argument_bytes_cannot_be_swapped_and_errors_withhold_contents() {
    let mut f=Fixture::new("confirmed");let a=format!("[safety.{:?}]\ncoordinator_agent_args_kind='claude'\ncoordinator_agent_args=['--A']\n",f.project.canonical_dir().display().to_string());let b=a.replace("--A","--B");
    f.input.config_digest=digest(Some(&a));assert_eq!(arguments(&f.input,&f.project,"claude",Some(&a)).unwrap(),["--A"]);assert!(arguments(&f.input,&f.project,"claude",Some(&b)).is_err());
    let secret="private-invalid-configuration";f.input.config_digest=digest(Some(secret));let error=arguments(&f.input,&f.project,"claude",Some(secret)).unwrap_err();assert!(!format!("{error:#}").contains(secret));assert!(!f.sent());
}
#[test]
fn blocked_coordinator_start_retains_ownership_and_cancels() {
    let f=Fixture::new("blocked");let input=f.input.clone();let control=Control::default();let worker_control=Control{deadline:control.deadline,cancellation:control.cancellation.clone()};
    let worker=std::thread::spawn(move||execute(&input,&worker_control));let end=Instant::now()+Duration::from_secs(10);while !f.sent(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}
    assert!(ProjectGuard::acquire(&f.project.dir()).is_err());control.cancellation.cancel();assert!(worker.join().unwrap().is_err());f.recover();assert!(f.record().launch_claim.unwrap().notified);
}
#[test]
fn open_retains_launch_history_and_queues_a_new_request_without_starting() {
    let world=crate::scenarios::World::new();let project=world.project("demo","a.sock");*world.panes.borrow_mut()=format!("[{}]",world.coordinator_pane(&project));
    project.update_coordinator(|c|{c.prime_request=1;c.prime_pending=true;c.launch_sequence=1;c.launch_claim=Some(LaunchClaim{sequence:1,generation:1,execution:"a".repeat(64),arguments_digest:"b".repeat(64),route_digest:"c".repeat(64),terminal:"terminal".into(),phase:LaunchPhase::Uncertain,error:"lost reply".into(),notified:true});}).unwrap();
    let before=project.coordinator().unwrap();let ctx=world.ctx();crate::coordinator::open(&ctx,"demo",&crate::coordinator::OpenOptions{session:paths::SessionFlags{socket:Some(before.socket.clone().into()),..Default::default()},reprime:true,rebind:false}).unwrap();
    let after=project.coordinator().unwrap();assert_eq!(after.prime_request,2);assert_eq!(after.launch_claim,before.launch_claim);assert_eq!(after.launch_attempts,0);assert!(after.prime_pending);assert_eq!(world.runner.count("agent start"),0);assert_eq!(world.runner.count("agent prompt"),0);
}
#[test]
fn coordinator_start_refuses_a_thread_sharing_its_pane() {
    let f=Fixture::new("confirmed");thread::allocate(&f.project,|t|{t.status=thread::Status::Resolved;t.pane_id="p".into();}).unwrap();assert!(execute(&f.input,&Control::default()).is_err());assert!(!f.sent());assert!(f.record().launch_claim.is_none());
}

#[test]
fn plain_open_cannot_turn_a_missing_agent_observation_into_another_launch_request() {
    for phase in [LaunchPhase::Confirmed,LaunchPhase::Uncertain,LaunchPhase::Pending] {
        let world=crate::scenarios::World::new();let project=world.project("demo","a.sock");*world.panes.borrow_mut()=format!("[{}]",world.coordinator_pane(&project));
        project.update_coordinator(|c|{c.prime_request=1;c.prime_pending=true;c.launch_sequence=1;c.launch_claim=Some(LaunchClaim{sequence:1,generation:1,execution:"a".repeat(64),arguments_digest:"b".repeat(64),route_digest:"c".repeat(64),terminal:"terminal".into(),error:if phase==LaunchPhase::Uncertain{"lost reply".into()}else{String::new()},notified:phase!=LaunchPhase::Pending,phase:phase.clone()});}).unwrap();
        let before=project.coordinator().unwrap();let ctx=world.ctx();let error=crate::coordinator::open(&ctx,"demo",&crate::coordinator::OpenOptions{session:paths::SessionFlags{socket:Some(before.socket.clone().into()),..Default::default()},reprime:false,rebind:false}).unwrap_err();
        assert!(error.to_string().contains("open --reprime"));assert_eq!(project.coordinator().unwrap(),before);assert!(ready(&before).is_err());assert_eq!(world.runner.count("agent start"),0);assert_eq!(world.runner.count("agent prompt"),0);
    }
}
