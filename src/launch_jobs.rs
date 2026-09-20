//! Concrete agent-start worker sharing brief routing and execution ownership.
use super::*;
fn arguments_from(input:&Input,project:&Project,t:&Thread,text:Option<&str>)->Result<Vec<String>> {
    ensure!(text.map(|s|thread::sha256_hex(s.as_bytes()))==input.config_digest,"launch argument configuration changed");
    let safety=project::parse_safety(text.unwrap_or(""),&project.canonical_dir()).map_err(|_|anyhow::anyhow!("invalid launch safety configuration (contents withheld)"))?;
    let args=safety.worker_arguments(&t.agent)?.to_vec();ensure!(args.len()<=128&&args.iter().map(String::len).sum::<usize>()<=32768,"launch arguments exceed bounds");Ok(args)
}
pub(super) fn execute(input:&Input,control:&Control)->Result<()> {execute_with(input,control,||Ok(()))}
fn execute_with(input:&Input,control:&Control,after_claim:impl FnOnce()->Result<()>)->Result<()> {
    input.validate()?;ensure!(input.launch,"launch mode missing");control.check()?;let guard=ProjectGuard::acquire(&input.project)?;let locks=guard.inherit_transfer()?;
    let project=Project::load(input.project.parent().context("launch root missing")?,input.project.file_name().and_then(|s|s.to_str()).context("invalid project name")?)?;
    let t=input.current(&project,&guard,control)?;ownership::check(&project,&t,&input.socket,control)?;
    check_route(input,control,&locks)?;if let Some(remote)=&input.remote {crate::remote_api::probe(&remote.route,&remote.binary,control,&locks)?;}
    let herdr=crate::herdr::Herdr::new(&input.herdr,&input.socket,&crate::runner::RealRunner);
    if input.remote.is_none() {
        let out=herdr_projects::supervision::run(herdr.cmd(crate::herdr::CALL_TIMEOUT).args(["remote-api-bridge","--check"]),control.deadline,control.cancellation.clone(),&locks)?;
        control.check()?;ensure!(out.success()&&out.stdout.trim()=="herdr-api-bridge-v1","local Herdr JSON API bridge unavailable");
    }
    let agents:Vec<crate::herdr::Agent>=serde_json::from_value(call(input,&herdr,"agent.list",None,control,&locks)?["agents"].clone())?;
    // Any agent in the pane (including a foreign name/kind) prevents starting.
    ensure!(!agents.iter().any(|a|a.pane_id==t.pane_id),"launch pane already has an agent");
    let result=call(input,&herdr,"pane.list",None,control,&locks)?;
    let panes=result["panes"].as_array().context("launch pane inventory missing")?;let matches=panes.iter().filter(|p|p["pane_id"].as_str()==Some(&t.pane_id)).collect::<Vec<_>>();
    ensure!(!t.pane_id.is_empty()&&matches.len()==1,"launch pane missing or ambiguous");let pane:crate::herdr::Pane=serde_json::from_value(matches[0].clone())?;ensure!(thread::pane_matches(&t,&pane),"launch pane identity changed");
    let terminal=matches[0]["terminal_id"].as_str().context("launch terminal identity unavailable")?;
    check_route(input,control,&locks)?;let t=input.current(&project,&guard,control)?;
    let text=paths::read_control_text(&input.config.join("config.toml"),1024*1024)?;
    let arguments=arguments_from(input,&project,&t,text.as_deref())?;
    let route_digest=thread::sha256_hex(&serde_json::to_vec(&(&input.socket,input.socket_identity,input.remote.as_ref().map(|r|(&r.route.id,&r.route.target,&r.route.session))))?);
    let claim=thread::launch_delivery::claim(&project,&guard,&t,&arguments,&route_digest,terminal,control)?;after_claim()?;
    control.check()?;check_route(input,control,&locks)?;guard.check_project(&input.project)?;
    ensure!(socket(&project)?==input.socket&&socket_identity(&input.socket)?==input.socket_identity&&digest(&input.config)?==input.config_digest&&project.try_status()?==project::Status::Active,"launch authority changed after claim");
    let current=thread::load(&project,&t.id)?;ensure!(thread::execution_fingerprint(&current)==input.execution&&current.launch_claim.as_ref()==Some(&claim)&&current.status==thread::Status::Open&&current.prompt_pending&&current.removal.is_none()&&current.pending_live_copy.is_none()&&current.pending_final_copy.is_none(),"launch claim changed before dispatch");
    let id=format!("launch-{}-{}-{}",t.id,claim.execution,claim.sequence);
    let params=serde_json::json!({"name":t.agent_name,"kind":t.agent,"pane_id":t.pane_id,"args":arguments,"timeout_ms":20000});
    let result=if let Some(remote)=&input.remote {
        crate::remote_api::request(&remote.route,&remote.binary,&id,"agent.start",params,control,&locks)?
    }else{
        // Raw JSON bridge acknowledges submission. Unlike the CLI's start/wait
        // wrapper, it does not reinterpret a later trust dialog as a lost start.
        let payload=serde_json::to_string(&serde_json::json!({"id":id,"method":"agent.start","params":params}))?+"\n";
        let out=herdr_projects::supervision::run(herdr.cmd(crate::herdr::CALL_TIMEOUT).arg("remote-api-bridge").stdin(payload),control.deadline,control.cancellation.clone(),&locks)?;control.check()?;
        ensure!(out.success(),"local launch API bridge failed");let reply:serde_json::Value=serde_json::from_str(&out.stdout)?;
        ensure!(reply["id"].as_str()==Some(&id)&&reply.get("error").is_none(),"local launch acknowledgement mismatch or rejection");reply.get("result").cloned().context("local launch reply lacks result")?
    };
    ensure!(result["type"].as_str()==Some("agent_started")&&result["agent"]["terminal_id"].as_str()==Some(&claim.terminal),"launch acknowledgement has wrong type or terminal");
    let agent=&result["agent"];
    for (field,expected) in [("pane_id",&t.pane_id),("tab_id",&t.tab_id),("workspace_id",&t.workspace_id),("cwd",&t.cwd),("name",&t.agent_name)] {ensure!(agent[field].as_str()==Some(expected.as_str()),"launch acknowledgement names another execution");}
    // Native startup replies can omit detected kind until screen recognition.
    // The correlated typed reply acknowledges the requested kind's submission;
    // explicit detected kind must match, and briefs still require exact kind.
    ensure!(agent["agent"].as_str()==Some(&t.agent)||(agent["agent"].is_null()&&agent["launch_pending"].as_bool()==Some(true)),"launch acknowledgement has a foreign or unavailable agent kind");
    let argv:Vec<String>=serde_json::from_value(result["argv"].clone()).map_err(|_|anyhow::anyhow!("launch acknowledgement lacks a valid command vector (contents withheld)"))?;
    ensure!(argv.len()==arguments.len()+1&&!argv[0].is_empty()&&argv[0].len()<=4096&&!argv[0].chars().any(char::is_control)&&argv[1..]==arguments,"launch acknowledgement command arguments changed");
    check_route(input,control,&locks)?;ensure!(socket(&project)?==input.socket&&socket_identity(&input.socket)?==input.socket_identity,"launch session changed during dispatch");
    thread::launch_delivery::confirm(&project,&guard,&t.id,&claim,control)
}

#[cfg(all(test,target_os="linux"))]
mod tests {
    use super::*;
    use std::{fs,os::unix::{fs::PermissionsExt,net::UnixListener}};
    struct Fixture{root:tempfile::TempDir,project:Project,t:Thread,input:Input,_socket:UnixListener}
    impl Fixture {
        fn new(mode:&str)->Self {
            let root=tempfile::tempdir().unwrap();let project=project::create(root.path(),"demo","",vec![]).unwrap();let socket=root.path().join("socket");let listener=UnixListener::bind(&socket).unwrap();
            project.update_coordinator(|c|c.socket=socket.display().to_string()).unwrap();let t=thread::allocate(&project,|t|{t.status=thread::Status::Open;t.prompt_pending=true;t.agent="claude".into();t.agent_name="worker".into();t.pane_id="p".into();t.workspace_id="w".into();t.tab_id="tab".into();t.cwd="/fixture".into();}).unwrap();
            let binary=root.path().join("herdr");let script=format!(r#"#!/usr/bin/python3
import sys,json,pathlib,time
root=pathlib.Path({root:?});mode={mode:?}
a={{'workspace_id':'w','tab_id':'tab','pane_id':'p','terminal_id':'terminal','cwd':'/fixture','name':'worker','agent':'claude','agent_status':'blocked','launch_pending':True}}
if sys.argv[1:]==['remote-api-bridge','--check']:print('unsupported' if mode=='unsupported' else 'herdr-api-bridge-v1')
elif sys.argv[1:]==['agent','list']:
 if mode=='foreign-busy':a['name']='foreign'
 print(json.dumps({{'result':{{'agents':[a] if mode in ['busy','foreign-busy'] else []}}}}))
elif sys.argv[1:]==['pane','list']:
 if mode=='no-terminal':del a['terminal_id']
 print(json.dumps({{'result':{{'panes':[a,a] if mode=='ambiguous' else [a]}}}}))
elif sys.argv[1:]==['remote-api-bridge']:
 r=json.loads(sys.stdin.readline());assert r['method']=='agent.start';assert r['params']=={{'name':'worker','kind':'claude','pane_id':'p','args':[],'timeout_ms':20000}}
 with open(root/'started','a') as f:f.write('start')
 if mode=='lost':sys.exit(1)
 if mode=='blocked':time.sleep(60)
 if mode=='foreign-terminal':a['terminal_id']='other'
 if mode=='foreign-kind':a['agent']='codex'
 if mode=='native-pending':del a['agent'];a['agent_status']='unknown'
 print(json.dumps({{'id':'other' if mode=='wrong-id' else r['id'],'result':{{'type':'wrong' if mode=='wrong-type' else 'agent_started','agent':a,'argv':['claude','wrong'] if mode=='wrong-args' else ['claude']}}}}))
else:sys.exit(3)
"#,root=root.path().display().to_string());
            fs::write(&binary,script).unwrap();fs::set_permissions(&binary,fs::Permissions::from_mode(0o700)).unwrap();
            let env=paths::Env::for_test(root.path(),&[("HERDR_BIN_PATH",binary.to_str().unwrap())]);let ctx=Ctx{env:&env,root:root.path().into(),config_dir:root.path().join("cfg"),runner:&crate::runner::RealRunner,detached_ticker:false};let request=request_launch(&ctx,&project,&t,None).unwrap();let input=serde_json::from_str(request.command.stdin.as_ref().unwrap()).unwrap();Self{root,project,t,input,_socket:listener}
        }
        fn started(&self)->bool{self.root.path().join("started").exists()}
        fn recover(&self){let guard=ProjectGuard::acquire(&self.project.dir()).unwrap();thread::launch_delivery::recover(&self.project,&guard).unwrap();}
    }
    #[test]
    fn concrete_launch_acknowledges_submission_without_waiting_for_interactive_readiness(){
        for mode in ["confirmed","native-pending"] {let f=Fixture::new(mode);execute(&f.input,&Control::default()).unwrap();let t=thread::load(&f.project,&f.t.id).unwrap();assert!(t.prompt_pending);assert_eq!(t.launch_claim.unwrap().phase,thread::launch_delivery::Phase::Confirmed);assert!(execute(&f.input,&Control::default()).is_err());assert_eq!(fs::read(f.root.path().join("started")).unwrap(),b"start");}
    }
    #[test]
    fn ambiguous_busy_missing_capability_and_missing_terminal_do_not_claim(){
        for mode in ["busy","foreign-busy","ambiguous","no-terminal","unsupported"]{let f=Fixture::new(mode);assert!(execute(&f.input,&Control::default()).is_err(),"{mode}");assert!(!f.started());assert!(thread::load(&f.project,&f.t.id).unwrap().launch_claim.is_none());}
    }
    #[test]
    fn lost_foreign_and_malformed_acknowledgements_never_repeat_launch(){
        for mode in ["lost","foreign-terminal","foreign-kind","wrong-id","wrong-type","wrong-args"]{let f=Fixture::new(mode);assert!(execute(&f.input,&Control::default()).is_err());assert!(f.started());f.recover();assert_eq!(thread::load(&f.project,&f.t.id).unwrap().status,thread::Status::Failed);assert!(execute(&f.input,&Control::default()).is_err());assert_eq!(fs::read(f.root.path().join("started")).unwrap(),b"start");}
    }
    #[test]
    fn changed_configuration_and_cancellation_after_claim_leave_uncertainty_without_start(){
        for cancel in [true,false]{let f=Fixture::new("confirmed");let control=Control::default();assert!(execute_with(&f.input,&control,||{if cancel{control.cancellation.cancel();}else{fs::create_dir(&f.input.config)?;fs::write(f.input.config.join("config.toml"),"# changed")?;}Ok(())}).is_err());assert!(!f.started());f.recover();assert_eq!(thread::load(&f.project,&f.t.id).unwrap().status,thread::Status::Failed);}
    }
    #[test]
    fn blocked_launch_retains_exclusion_and_cancels_its_subprocess(){
        let f=Fixture::new("blocked");let input=f.input.clone();let control=Control::default();let worker_control=Control{deadline:control.deadline,cancellation:control.cancellation.clone()};let worker=std::thread::spawn(move||execute(&input,&worker_control));let end=Instant::now()+Duration::from_secs(5);
        while !f.started(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}assert!(ProjectGuard::acquire(&f.project.dir()).is_err());control.cancellation.cancel();assert!(worker.join().unwrap().is_err());f.recover();assert_eq!(thread::load(&f.project,&f.t.id).unwrap().status,thread::Status::Failed);
    }
    #[test]
    fn restored_config_path_cannot_authorize_arguments_parsed_from_other_bytes(){
        let mut f=Fixture::new("confirmed");let a=format!("[safety.{:?}]\nthread_agent_args_kind='claude'\nthread_agent_args=['--A']\n",f.project.canonical_dir().display().to_string());let b=a.replace("--A","--B");
        fs::create_dir(&f.input.config).unwrap();let path=f.input.config.join("config.toml");fs::write(&path,&a).unwrap();f.input.config_digest=digest(&f.input.config).unwrap();
        fs::write(&path,&b).unwrap();let captured=fs::read_to_string(&path).unwrap();fs::write(&path,&a).unwrap();
        assert_eq!(digest(&f.input.config).unwrap(),f.input.config_digest);assert!(arguments_from(&f.input,&f.project,&f.t,Some(&captured)).is_err());assert_eq!(arguments_from(&f.input,&f.project,&f.t,Some(&a)).unwrap(),["--A"]);let malformed="secret-value-without-toml-syntax";f.input.config_digest=Some(thread::sha256_hex(malformed.as_bytes()));let error=arguments_from(&f.input,&f.project,&f.t,Some(malformed)).unwrap_err();assert!(!format!("{error:#}").contains(malformed));assert!(thread::load(&f.project,&f.t.id).unwrap().launch_claim.is_none());assert!(!f.started());
    }

}
