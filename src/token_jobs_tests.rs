use super::*;
use std::{fs,os::unix::{fs::PermissionsExt,net::UnixListener}};
struct Fixture {root:tempfile::TempDir,p:Project,t:Thread,input:Input,_socket:UnixListener}
impl Fixture {
    fn new(mode:&str,coordinator:bool)->Self {
        let root=tempfile::tempdir().unwrap();let p=project::create(root.path(),"demo","",vec![]).unwrap();let socket=root.path().join("socket");let listener=UnixListener::bind(&socket).unwrap();
        p.update_coordinator(|c|{c.socket=socket.display().to_string();if coordinator{c.workspace_id="w".into();c.tab_id="tab".into();c.pane_id="p".into();c.cwd="/fixture".into();c.agent_name="worker".into();}}).unwrap();
        let t=if coordinator{Thread::default()}else{thread::allocate(&p,|t|{t.status=thread::Status::Open;t.workspace_id="w".into();t.tab_id="tab".into();t.pane_id="p".into();t.cwd="/fixture".into();t.agent_name="worker".into();t.agent="claude".into();}).unwrap()};
        let binary=root.path().join("herdr");let env=paths::Env::for_test(root.path(),&[("HERDR_BIN_PATH",binary.to_str().unwrap())]);let ctx=Ctx{env:&env,root:root.path().into(),config_dir:root.path().join("cfg"),runner:&crate::runner::RealRunner,detached_ticker:false};
        let request=request(&ctx,&p,if coordinator{None}else{Some(&t)},None).unwrap();let input=serde_json::from_str(request.command.stdin.as_ref().unwrap()).unwrap();let f=Self{root,p,t,input,_socket:listener};f.script(mode);f
    }
    fn script(&self,mode:&str) {
        fs::write(self.root.path().join("mode"),mode).unwrap();
        let script=format!(r#"#!/usr/bin/python3
import sys,json,pathlib,time
root=pathlib.Path({root:?});mode=(root/'mode').read_text()
args=sys.argv[1:]
if args[:2]==['--session','named-session']:args=args[2:]
if args==['remote-api-bridge','--check']:
 print('unsupported' if mode=='unsupported' else 'herdr-api-bridge-v1');sys.exit(0)
assert args==['remote-api-bridge']
r=json.loads(sys.stdin.readline())
a={{'workspace_id':'w','tab_id':'tab','pane_id':'p','terminal_id':'terminal','cwd':'/fixture','name':'worker','agent':'claude','agent_status':'idle'}}
if mode=='foreign':a['name']='other'
if mode=='foreign-kind':a['agent']='codex'
if mode=='foreign-pane':a['cwd']='other'
if mode=='busy':a['agent_status']='working'
if mode=='post-terminal' and (root/'sent').exists():a['terminal_id']='replacement'
if r['method']=='agent.list':
 if mode=='agent-terminal':a['terminal_id']='other'
 result={{'agents':[] if mode=='no-agent' else [a,a] if mode=='duplicate-agent' else [a]}}
elif r['method']=='pane.list':
 if mode=='no-terminal':del a['terminal_id']
 result={{'panes':[] if mode=='no-pane' else [a,a] if mode=='duplicate-pane' else [a]}}
elif r['method']=='pane.report_metadata':
 assert set(r['params'])=={{'pane_id','source','ttl_ms','tokens'}}
 assert r['params']['pane_id']=='p' and r['params']['source']=='herdr-projects' and r['params']['ttl_ms']==300000
 with open(root/'sent','a') as f:f.write(json.dumps(r['params'])+'\n')
 if mode=='lost':sys.exit(1)
 if mode=='blocked':time.sleep(60)
 if mode=='malformed':print('not-json');sys.exit(0)
 result={{'type':'wrong' if mode=='wrong-type' else 'ok'}}
else:sys.exit(2)
print(json.dumps({{'id':'wrong' if mode=='wrong-id' and r['method']=='pane.report_metadata' else r['id'],'result':result}}))
"#,root=self.root.path().display().to_string());
        fs::write(&self.input.herdr,script).unwrap();fs::set_permissions(&self.input.herdr,fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn sent(&self)->Vec<serde_json::Value>{fs::read_to_string(self.root.path().join("sent")).unwrap_or_default().lines().map(|l|serde_json::from_str(l).unwrap()).collect()}
}
#[test]
fn native_ok_refresh_is_repeatable_and_does_not_write_execution_state() {
    for coordinator in [false,true] {let f=Fixture::new("ok",coordinator);let before=fs::read(f.p.state_dir().join("coordinator.json")).unwrap();execute(&f.input,&Control::default()).unwrap();execute(&f.input,&Control::default()).unwrap();let sent=f.sent();assert_eq!(sent.len(),2);assert_eq!(sent[0],sent[1]);assert_eq!(sent[0]["tokens"]["project"],"demo");assert_eq!(sent[0]["tokens"]["thread"],if coordinator{"coordinator"}else{&f.t.id});assert_eq!(fs::read(f.p.state_dir().join("coordinator.json")).unwrap(),before);if !coordinator{assert_eq!(thread::load(&f.p,&f.t.id).unwrap(),f.t);}}
}
#[test]
fn refresh_uses_current_group_and_allows_busy_or_empty_owned_thread_panes() {
    for mode in ["ok","busy","no-agent"] {let f=Fixture::new(mode,false);execute_with(&f.input,&Control::default(),||{thread::update(&f.p,&f.t.id,|t|t.report_hash="report".into())?;Ok(())}).unwrap();assert_eq!(f.sent()[0]["tokens"]["review"],if mode=="busy"{"working"}else{"ready-for-review"});}
    let f=Fixture::new("no-agent",true);assert!(execute(&f.input,&Control::default()).is_err());assert!(f.sent().is_empty());
}
#[test]
fn stale_ambiguous_and_foreign_targets_refuse_before_refresh() {
    for mode in ["foreign","foreign-kind","foreign-pane","agent-terminal","duplicate-agent","duplicate-pane","no-pane","no-terminal","unsupported"] {let f=Fixture::new(mode,false);assert!(execute(&f.input,&Control::default()).is_err(),"{mode}");assert!(f.sent().is_empty());}
    for change in ["generation","resolved","removal","socket","config","settings","paused","cancel"] {
        let f=Fixture::new("ok",false);let control=Control::default();
        let result=execute_with(&f.input,&control,||{match change {
            "generation"=>{thread::update(&f.p,&f.t.id,|t|t.lifecycle_generation+=1)?;},
            "resolved"=>{thread::update(&f.p,&f.t.id,|t|t.status=thread::Status::Resolved)?;},
            "removal"=>{thread::update(&f.p,&f.t.id,|t|t.removal=Some(crate::cleanup::Removal{operation:"fixture".into(),repo:String::new(),path:String::new(),branch:String::new(),head:String::new(),snapshot:String::new(),generation:0,removed:false}))?;},
            "socket"=>{fs::remove_file(&f.input.socket)?;let _new=UnixListener::bind(&f.input.socket)?;},
            "config"=>{fs::create_dir_all(&f.input.config)?;fs::write(f.input.config.join("config.toml"),"# changed")?;},
            "settings"=>{fs::write(f.p.project_md(),"changed")?;},
            "paused"=>f.p.set_status(project::Status::Paused)?,
            _=>control.cancellation.cancel(),
        }Ok(())});
        assert!(result.is_err(),"{change}");assert!(f.sent().is_empty(),"{change}");
    }
    let f=Fixture::new("ok",false);let other=project::create(f.root.path(),"other","",vec![]).unwrap();other.update_coordinator(|c|{c.socket=f.input.socket.display().to_string();c.pane_id="p".into();}).unwrap();assert!(execute(&f.input,&Control::default()).is_err());assert!(f.sent().is_empty());
}
#[test]
fn lost_wrong_or_changed_receipts_never_become_execution_evidence() {
    for mode in ["lost","wrong-id","wrong-type","malformed","post-terminal"] {let f=Fixture::new(mode,false);assert!(execute(&f.input,&Control::default()).is_err(),"{mode}");assert_eq!(f.sent().len(),1);assert_eq!(thread::load(&f.p,&f.t.id).unwrap(),f.t);f.script("ok");execute(&f.input,&Control::default()).unwrap();assert_eq!(f.sent().len(),2);}
}
#[test]
fn blocked_refresh_retains_ownership_and_cancels_while_neighbor_can_observe() {
    let f=Fixture::new("blocked",false);let input=f.input.clone();let control=Control::default();let c=Control{deadline:control.deadline,cancellation:control.cancellation.clone()};let worker=std::thread::spawn(move||execute(&input,&c));let end=Instant::now()+Duration::from_secs(10);while f.sent().is_empty(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}
    assert!(ProjectGuard::acquire(&f.p.dir()).is_err());assert!(crate::cleanup::lease(f.root.path()).is_err());let neighbor=project::create(f.root.path(),"neighbor","",vec![]).unwrap();assert!(ProjectGuard::acquire(&neighbor.dir()).is_ok());
    neighbor.update_coordinator(|c|c.socket=f.input.socket.display().to_string()).unwrap();
    let t=thread::allocate(&neighbor,|t|{t.status=thread::Status::Open;t.pane_id="neighbor".into();t.last_state="working".into();t.last_group="working".into();}).unwrap();
    let helper=f.root.path().join("observer");fs::write(&helper,"#!/bin/sh\ncase \"$1:$2\" in\nagent:list) printf '%s\\n' '{\"result\":{\"agents\":[]}}';;\npane:list) printf '%s\\n' '{\"result\":{\"panes\":[]}}';;\n*) exit 2;;\nesac\n").unwrap();fs::set_permissions(&helper,fs::Permissions::from_mode(0o700)).unwrap();
    let env=paths::Env::for_test(f.root.path(),&[("HERDR_BIN_PATH",helper.to_str().unwrap())]);let ctx=Ctx{env:&env,root:f.root.path().into(),config_dir:f.root.path().join("cfg"),runner:&crate::runner::RealRunner,detached_ticker:false};let mut memory=crate::steps::Memory::new(&ctx);
    let _=crate::ticker::tick_project_with(&ctx,&neighbor,&mut memory);assert_eq!(thread::load(&neighbor,&t.id).unwrap().last_state,"");
    control.cancellation.cancel();assert!(worker.join().unwrap().is_err());assert!(ProjectGuard::acquire(&f.p.dir()).is_ok());assert_eq!(f.sent().len(),1);
}
#[test]
fn remote_refresh_uses_frozen_route_and_refuses_changed_destination() {
    const CHILD:&str="HP_REMOTE_TOKEN_FIXTURE";
    if std::env::var_os(CHILD).is_none(){let bin=tempfile::tempdir().unwrap();let ssh=bin.path().join("ssh");fs::write(&ssh,"#!/usr/bin/python3\nimport subprocess,sys\nassert sys.argv[1:4]==['-T','-o','StrictHostKeyChecking=yes']\nassert sys.argv[-2]=='fixture.invalid'\nsys.exit(subprocess.call(sys.argv[-1],shell=True))\n").unwrap();fs::set_permissions(&ssh,fs::Permissions::from_mode(0o700)).unwrap();let out=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","token_jobs::tests::remote_refresh_uses_frozen_route_and_refuses_changed_destination","--nocapture"]).env(CHILD,"1").env("PATH",format!("{}:/usr/bin:/bin",bin.path().display())).output().unwrap();assert!(out.status.success(),"{}\n{}",String::from_utf8_lossy(&out.stdout),String::from_utf8_lossy(&out.stderr));return;}
    for change in ["none","session","disabled","selector","cancel"] {
        let mut f=Fixture::new("ok",false);thread::update(&f.p,&f.t.id,|t|t.machine="saved".into()).unwrap();f.t=thread::load(&f.p,&f.t.id).unwrap();f.input.target=Target::Thread{id:f.t.id.clone(),execution:thread::execution_fingerprint(&f.t)};
        let binary=f.input.herdr.clone();f.input.herdr=f.root.path().join("local").display().to_string();let route=crate::remote_api::Route{id:"a".repeat(32),label:"saved".into(),target:"fixture.invalid".into(),session:"named-session".into(),enabled:true,selected:false};
        let listing=f.root.path().join("listing");fs::write(&listing,serde_json::to_string(&vec![route.clone()]).unwrap()).unwrap();fs::write(&f.input.herdr,format!("#!/bin/sh\n[ \"$1:$2:$3\" = machine:list:--json ] || exit 2\n/bin/cat {}\n",crate::remote::quote(listing.to_str().unwrap()))).unwrap();fs::set_permissions(&f.input.herdr,fs::Permissions::from_mode(0o700)).unwrap();f.input.remote=Some(Remote{route:route.clone(),selector:"saved".into(),binary});
        let control=Control::default();let result=execute_with(&f.input,&control,||{let mut route=route.clone();match change{"session"=>route.session="changed".into(),"disabled"=>route.enabled=false,"selector"=>{thread::update(&f.p,&f.t.id,|t|t.machine="other".into())?;},"cancel"=>control.cancellation.cancel(),_=>{}}fs::write(&listing,serde_json::to_string(&vec![route])?)?;Ok(())});assert_eq!(result.is_ok(),change=="none","{change}: {result:?}");assert_eq!(f.sent().len(),usize::from(change=="none"));
    }
}
