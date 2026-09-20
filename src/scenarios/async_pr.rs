use super::*;
use crate::steps;
use std::{sync::{Arc,Mutex,mpsc},time::{Duration,Instant}};
use crate::runner::{Runner,Output};
const JSON:&str=r#"{"state":"OPEN","headRefName":"hp/demo/t-0001-task","headRepository":{"name":"app"},"headRepositoryOwner":{"login":"owner"},"comments":[]}"#;
struct Delayed {started:mpsc::Sender<()>,release:Mutex<mpsc::Receiver<()>>}
impl Runner for Delayed {
    fn run(&self,cmd:&Cmd)->anyhow::Result<Output> {
        assert_eq!(cmd.program,"gh");assert_eq!(&cmd.args[..2],["pr","view"]);self.started.send(()).unwrap();
        loop {if cmd.cancellation.as_ref().unwrap().is_cancelled(){return Ok(Output{cancelled:true,..Output::default()});}match self.release.lock().unwrap().recv_timeout(Duration::from_millis(5)){Ok(())=>return Ok(ok(JSON)),Err(mpsc::RecvTimeoutError::Timeout)=>{},Err(_)=>return Ok(ok(JSON))}}
    }
    fn socket_request(&self,_:&Path,_:&str,_:Duration)->anyhow::Result<String>{unreachable!()}
}
fn reads()->(crate::pr_polling::Reads,mpsc::Receiver<()>,mpsc::Sender<()>) {let(started,rx)=mpsc::channel();let(tx,release)=mpsc::channel();(crate::pr_polling::Reads::new(Arc::new(Delayed{started,release:Mutex::new(release)})).unwrap(),rx,tx)}
#[test]
fn pending_pr_does_not_block_other_project_status_or_become_an_outage() {
    let(world,project)=pr_world(JSON);let healthy=world.project("healthy","healthy.sock");let ctx=world.ctx();let mut memory=Memory::new(&ctx);let(pool,started,release)=reads();memory.pr_reads=Some(pool);
    let start=Instant::now();assert!(ticker::tick_for_test(&ctx,&mut memory));assert!(start.elapsed()<Duration::from_secs(1));started.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(items_of(&project,"outage").is_empty());assert!(steps::load_state(&project).last_pr_check.is_empty());assert!(thread::load(&project,"t-0001").unwrap().pr_state.is_empty());assert_eq!(world.runner.count("gh pr view"),0);
    world.thread(&healthy,Path::new("/healthy-fixture"),|t|{t.workspace_id="w3".into();t.tab_id="w3:t1".into();t.pane_id="w3:p1".into();t.last_state="working".into();t.last_group="working".into();});
    let mut agents:Vec<serde_json::Value>=serde_json::from_str(&world.agents.borrow()).unwrap();agents.push(serde_json::from_str(&agent_json("w3","w3:t1","w3:p1","/healthy-fixture","hp-healthy-t-0001","blocked")).unwrap());*world.agents.borrow_mut()=serde_json::to_string(&agents).unwrap();
    // A second full pass still probes the unrelated session while gh is waiting.
    let before=world.runner.calls.borrow().len();assert!(ticker::tick_for_test(&ctx,&mut memory));assert!(world.runner.calls.borrow()[before..].iter().any(|cmd|cmd.env.iter().any(|(k,v)|k=="HERDR_SOCKET_PATH"&&v==&healthy.coordinator().unwrap().socket)));assert!(started.try_recv().is_err());assert_eq!(thread::load(&healthy,"t-0001").unwrap().last_state,"blocked");
    release.send(()).unwrap();let deadline=Instant::now()+Duration::from_secs(2);loop {ticker::tick_for_test(&ctx,&mut memory);if thread::load(&project,"t-0001").unwrap().pr_state=="OPEN"{break;}assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
    assert!(items_of(&project,"outage").is_empty());assert!(!steps::load_state(&project).last_pr_check.is_empty());memory.pr_reads.as_mut().unwrap().stop().unwrap();
}
#[test]
fn changed_report_discards_pending_observation_and_shutdown_drains_it() {
    let(world,project)=pr_world(JSON);let ctx=world.ctx();let mut memory=Memory::new(&ctx);let(pool,started,_release)=reads();memory.pr_reads=Some(pool);ticker::tick_for_test(&ctx,&mut memory);started.recv_timeout(Duration::from_secs(1)).unwrap();
    std::fs::write(thread::home_report_path(&project,"t-0001"),format!("PR: {PR_URL}\nChanged report while query pending\n")).unwrap();ticker::tick_for_test(&ctx,&mut memory);started.recv_timeout(Duration::from_secs(1)).unwrap();assert!(thread::load(&project,"t-0001").unwrap().pr_state.is_empty());
    match ticker::metrics_state(&ctx.root) {
        ticker::MetricsFile::Present(metrics) => {
            assert!(!metrics.uncertain);
            assert!(metrics.control.queued+metrics.control.running>=1||metrics.control.completed>=1);
        }
        other => panic!("expected persisted executor metrics, got {other:?}"),
    }
    let start=Instant::now();memory.pr_reads.as_mut().unwrap().stop().unwrap();assert!(start.elapsed()<Duration::from_secs(2));assert!(items_of(&project,"outage").is_empty());
}
