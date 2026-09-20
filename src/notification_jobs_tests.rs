use super::*;
use std::{fs,os::unix::{fs::PermissionsExt,net::UnixListener}};
struct Fixture {root:tempfile::TempDir,p:Project,input:Input,_listener:UnixListener}
impl Fixture {
    fn new(nudge:bool,outcome:&str)->Self {
        let root=tempfile::tempdir().unwrap();let p=project::create(root.path(),"demo","",vec![]).unwrap();let socket=root.path().join("session.sock");let listener=UnixListener::bind(&socket).unwrap();
        p.update_coordinator(|c|{c.socket=socket.display().to_string();c.workspace_id="w".into();c.tab_id="tab".into();c.pane_id="p".into();c.cwd="/fixture".into();c.agent_name="coordinator".into();}).unwrap();
        if nudge{fs::write(p.project_md(),fs::read_to_string(p.project_md()).unwrap().replace("nudge = false","nudge = true")).unwrap();}
        crate::inbox::write_once(&p,"item-a","test","fixture","A","").unwrap();
        let helper=root.path().join("herdr");let env=paths::Env::for_test(root.path(),&[("HERDR_BIN_PATH",helper.to_str().unwrap())]);let runner=crate::runner::RealRunner;
        let ctx=Ctx{env:&env,root:root.path().into(),config_dir:root.path().join("cfg"),runner:&runner,detached_ticker:false};
        let input=serde_json::from_str(super::super::request_notification(&ctx,&p,&p.coordinator().unwrap()).unwrap().command.stdin.as_ref().unwrap()).unwrap();
        let f=Self{root,p,input,_listener:listener};f.script(outcome);f
    }
    fn script(&self,outcome:&str) {
        let script=format!(r#"#!/usr/bin/python3
import sys,json,time,pathlib
root=pathlib.Path({root:?});outcome={outcome:?}
a={{'pane_id':'p','tab_id':'tab','workspace_id':'w','terminal_id':'terminal','name':'coordinator','cwd':'/fixture','agent':'claude','agent_status':'working' if outcome=='busy' else 'idle'}}
args=sys.argv[1:]
if args==['remote-api-bridge','--check']:print('unsupported' if outcome=='unsupported' else 'herdr-api-bridge-v1');sys.exit(0)
if args==['agent','list']:print(json.dumps({{'result':{{'agents':[a,a] if outcome=='duplicate' else [a]}}}}));sys.exit(0)
if args==['pane','list']:print(json.dumps({{'result':{{'panes':[a]}}}}));sys.exit(0)
if args==['remote-api-bridge']:
 r=json.load(sys.stdin)
 with open(root/'sent','a') as f:f.write('send')
 if outcome=='lost':sys.exit(1)
 if outcome=='blocked':time.sleep(60)
 if outcome=='foreign':a['terminal_id']='foreign'
 if outcome=='kind':a['agent']='other'
 if r['method']=='agent.prompt':
  assert r['params']['target']=='p'
  assert r['params']['text'].startswith('[herdr-projects ticker: automated, not the user, approves nothing]')
  result={{'type':'agent_prompted','agent':a}}
 else:
  assert r['method']=='notification.show'
  result={{'type':'notification_show','shown':outcome!='not-shown','reason':'busy' if outcome in ['not-shown','contradictory'] else 'shown'}}
 record=root/'demo/.state/ticker.json'
 if outcome=='unrelated':
  state=json.loads(record.read_text());state['last_pr_check']='preserve-me';record.write_text(json.dumps(state))
 if outcome=='receipt-failure':record.rename(record.with_suffix('.backup'));record.mkdir()
 print(json.dumps({{'id':'wrong' if outcome=='wrongid' else r['id'],'result':result}}));sys.exit(0)
sys.exit(2)
"#,root=self.root.path().display().to_string());
        fs::write(&self.input.herdr,script).unwrap();fs::set_permissions(&self.input.herdr,fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn state(&self)->State{steps::try_load_state(&self.p).unwrap()}
    fn recover(&self){let g=ProjectGuard::acquire(&self.p.dir()).unwrap();recover(&self.p,&g).unwrap();}
    fn sends(&self)->usize{fs::read(self.root.path().join("sent")).map_or(0,|b|b.len()/4)}
    fn add(&self,id:&str){crate::inbox::write_once(&self.p,id,"test","fixture","New","").unwrap();}
    fn reconcile(&self,retry:bool)->Result<()> {
        let env=paths::Env::for_test(self.root.path(),&[("HERDR_BIN_PATH",&self.input.herdr)]);let ctx=Ctx{env:&env,root:self.root.path().into(),config_dir:self.input.config.clone(),runner:&crate::runner::RealRunner,detached_ticker:false};
        reconcile(&ctx,&self.p,self.state().notification_sequence,retry)
    }
    fn refresh(&mut self) {
        let env=paths::Env::for_test(self.root.path(),&[("HERDR_BIN_PATH",&self.input.herdr)]);let ctx=Ctx{env:&env,root:self.root.path().into(),config_dir:self.input.config.clone(),runner:&crate::runner::RealRunner,detached_ticker:false};
        self.input=serde_json::from_str(super::super::request_notification(&ctx,&self.p,&self.p.coordinator().unwrap()).unwrap().command.stdin.as_ref().unwrap()).unwrap();
    }
}
#[test]
fn notification_confirmed_delivery_is_not_replayed_and_preserves_unrelated_state() {
    for nudge in [false,true] {
        let f=Fixture::new(nudge,"unrelated");execute(&f.input,&Control::default()).unwrap();assert_eq!(f.state().notification_claim.unwrap().phase,NoticePhase::Confirmed);assert_eq!(f.state().last_pr_check,"preserve-me");
        f.recover();execute(&f.input,&Control::default()).unwrap();assert_eq!(f.sends(),1);f.add("item-b");execute(&f.input,&Control::default()).unwrap();assert_eq!(f.sends(),2);
    }
}
#[test]
fn notification_uncertainty_blocks_new_items_and_mode_route_changes() {
    for (nudge,outcome) in [(false,"lost"),(false,"wrongid"),(false,"contradictory"),(true,"lost"),(true,"foreign"),(true,"kind")] {
        let mut f=Fixture::new(nudge,outcome);assert!(execute(&f.input,&Control::default()).is_err());f.recover();assert_eq!(f.state().notification_claim.unwrap().phase,NoticePhase::Uncertain);
        f.add("item-b");assert!(execute(&f.input,&Control::default()).is_err());assert_eq!(f.sends(),1);
        let md=fs::read_to_string(f.p.project_md()).unwrap().replace(if nudge{"nudge = true"}else{"nudge = false"},if nudge{"nudge = false"}else{"nudge = true"});fs::write(f.p.project_md(),md).unwrap();f.p.update_coordinator(|c|c.cwd="/changed".into()).unwrap();f.refresh();assert!(execute(&f.input,&Control::default()).is_err());assert_eq!(f.sends(),1);
        assert_eq!(crate::inbox::unhandled(&f.p).len(),2,"uncertainty must not manufacture another inbox item");
    }
}
#[test]
fn notification_explicit_ack_suppresses_old_ids_but_new_items_can_progress() {
    let f=Fixture::new(false,"lost");assert!(execute(&f.input,&Control::default()).is_err());f.recover();f.add("item-b");f.reconcile(false).unwrap();assert!(f.state().notification_suppressed.contains("item-a"));assert_eq!(f.sends(),1);
    f.script("confirmed");execute(&f.input,&Control::default()).unwrap();let state=f.state();assert_eq!(state.notification_claim.unwrap().batch.unwrap().ids,["item-b"]);assert_eq!(f.sends(),2);execute(&f.input,&Control::default()).unwrap();assert_eq!(f.sends(),2);
}
#[test]
fn notification_explicit_retry_is_linked_and_frozen() {
    for change in [false,true] {
        let f=Fixture::new(false,"lost");assert!(execute(&f.input,&Control::default()).is_err());f.recover();f.reconcile(true).unwrap();assert_eq!(f.state().notification_claim.as_ref().unwrap().retry_of,Some(1));assert_eq!(f.state().notification_claim.unwrap().phase,NoticePhase::Ready);
        f.script("confirmed");if change{f.add("item-b");assert!(execute(&f.input,&Control::default()).is_err());assert_eq!(f.sends(),1);}
        else{execute(&f.input,&Control::default()).unwrap();assert_eq!(f.sends(),2);assert_eq!(f.state().notification_sequence,2);assert_eq!(f.state().notification_claim.unwrap().phase,NoticePhase::Confirmed);}
    }
}
#[test]
fn notification_consumption_requires_seen_or_handled_evidence_not_disappearance() {
    for how in ["seen","done","missing"] {
        let f=Fixture::new(false,"lost");assert!(execute(&f.input,&Control::default()).is_err());f.recover();f.add("item-b");
        match how {"seen"=>crate::inbox::mark_seen(&f.p,&["item-a".into()]).unwrap(),"done"=>{crate::inbox::done(&f.p,&["item-a".into()],false).unwrap();},_=>fs::remove_file(f.p.dir().join("inbox/item-a.md")).unwrap()}
        f.script("confirmed");assert_eq!(execute(&f.input,&Control::default()).is_ok(),how!="missing");assert_eq!(f.sends(),if how=="missing"{1}else{2});
    }
}
#[test]
fn native_not_shown_receipt_allows_bounded_retry_without_uncertainty() {
    let f=Fixture::new(false,"not-shown");execute(&f.input,&Control::default()).unwrap();assert_eq!(f.state().notification_claim.unwrap().phase,NoticePhase::NotShown);execute(&f.input,&Control::default()).unwrap();assert_eq!(f.sends(),1);
    let mut state=f.state();state.notification_retry.retry.next_attempt=String::new();steps::save_state(&f.p,&state).unwrap();f.script("confirmed");execute(&f.input,&Control::default()).unwrap();assert_eq!(f.sends(),2);assert_eq!(f.state().notification_claim.unwrap().phase,NoticePhase::Confirmed);
}
#[test]
fn notification_preflight_and_post_claim_changes_do_not_send() {
    for how in ["busy","duplicate","unsupported","unprimed","cancel","settings","consumed"] {
        let f=Fixture::new(true,how);if how=="unprimed"{f.p.update_coordinator(|c|c.prime_pending=true).unwrap();}
        let control=Control::default();let result=execute_with(&f.input,&control,||{match how{"cancel"=>control.cancellation.cancel(),"settings"=>fs::write(f.p.project_md(),"changed")?,"consumed"=>crate::inbox::mark_seen(&f.p,&["item-a".into()])?,_=>{}}Ok(())});assert!(result.is_err(),"{how}");assert_eq!(f.sends(),0);
        if matches!(how,"cancel"|"settings"|"consumed"){f.recover();assert_eq!(f.state().notification_claim.unwrap().phase,NoticePhase::Uncertain);}else{assert!(f.state().notification_claim.is_none());}
    }
}
#[test]
fn notification_receipt_failure_retains_claim_for_reconciliation() {
    let f=Fixture::new(false,"receipt-failure");assert!(execute(&f.input,&Control::default()).is_err());assert_eq!(f.sends(),1);
    let path=f.p.state_dir().join("ticker.json");fs::remove_dir(&path).unwrap();fs::rename(path.with_extension("backup"),&path).unwrap();
    f.recover();assert_eq!(f.state().notification_claim.unwrap().phase,NoticePhase::Uncertain);assert!(execute(&f.input,&Control::default()).is_err());assert_eq!(f.sends(),1);
}
#[test]
fn legacy_reserved_notifications_become_uncertain_without_replaying() {
    let f=Fixture::new(false,"confirmed");let mut state=f.state();state.notification_retry.hash="a".repeat(64);state.notification_retry.retry.attempts=1;steps::save_state(&f.p,&state).unwrap();
    assert!(execute(&f.input,&Control::default()).is_err());assert_eq!(f.sends(),0);assert_eq!(f.state().notification_claim.unwrap().mode,Mode::Legacy);f.reconcile(false).unwrap();execute(&f.input,&Control::default()).unwrap();assert_eq!(f.sends(),0);
}
#[test]
fn blocked_notification_keeps_project_ownership_and_cancels() {
    let f=Fixture::new(false,"blocked");let input=f.input.clone();let control=Control::default();let child_control=Control{deadline:control.deadline,cancellation:control.cancellation.clone()};let child=std::thread::spawn(move||execute(&input,&child_control));let end=Instant::now()+Duration::from_secs(10);
    while f.sends()==0{assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}assert!(ProjectGuard::acquire(&f.p.dir()).is_err());let other=project::create(f.root.path(),"other","",vec![]).unwrap();assert!(ProjectGuard::acquire(&other.dir()).is_ok());control.cancellation.cancel();assert!(child.join().unwrap().is_err());f.recover();assert_eq!(f.state().notification_claim.unwrap().phase,NoticePhase::Uncertain);
}
#[test]
fn explicit_notification_retry_does_not_authorize_a_replacement_socket() {
    let mut f=Fixture::new(false,"lost");assert!(execute(&f.input,&Control::default()).is_err());f.recover();f.reconcile(true).unwrap();let ready=f.state().notification_claim;
    fs::remove_file(&f.input.socket).unwrap();let _replacement=UnixListener::bind(&f.input.socket).unwrap();f.refresh();f.script("confirmed");assert!(execute(&f.input,&Control::default()).is_err());assert_eq!(f.sends(),1);assert_eq!(f.state().notification_claim,ready);
}
