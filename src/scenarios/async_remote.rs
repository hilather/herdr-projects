use super::*;
use crate::{steps,executor::{Executor,Limits},remote_polling::{Reads,ProbeRunner},runner::{Runner,Output}};
use std::{sync::{Arc,Mutex,mpsc},time::{Duration,Instant}};
struct Delayed {started:mpsc::Sender<()>,release:Mutex<mpsc::Receiver<()>>}
impl Runner for Delayed {
    fn run(&self,cmd:&Cmd)->anyhow::Result<Output> {
        if cmd.args.first().is_some_and(|s|s=="--machine")&&cmd.display().contains("agent list") {
            self.started.send(()).unwrap();loop {if cmd.cancellation.as_ref().unwrap().is_cancelled(){return Ok(Output{cancelled:true,..Output::default()});}match self.release.lock().unwrap().recv_timeout(Duration::from_millis(5)){Ok(())=>break,Err(mpsc::RecvTimeoutError::Timeout)=>{},Err(_)=>break}}
            return Ok(ok(r#"{"result":{"agents":[{"workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1","cwd":"/home/me/wt","name":"hp-demo-t-0001","agent_status":"blocked"}]}}"#));
        }
        if cmd.display().contains("pane list"){return Ok(ok(r#"{"result":{"panes":[]}}"#));}
        if cmd.display().contains("machine list"){return Ok(ok(r#"[{"label":"box","target":"me@box"}]"#));}
        if cmd.program=="ssh"{return Ok(ok("t-0001 -\n"));}
        anyhow::bail!("unexpected observation effect: {}",cmd.display())
    }
    fn socket_request(&self,_:&Path,_:&str,_:Duration)->anyhow::Result<String>{unreachable!()}
}
fn pool()->(Arc<Executor>,mpsc::Receiver<()>,mpsc::Sender<()>) {let(started,rx)=mpsc::channel();let(tx,release)=mpsc::channel();let pool=Arc::new(Executor::new(Limits::default(),Arc::new(ProbeRunner{inner:Arc::new(Delayed{started,release:Mutex::new(release)})})).unwrap());(pool,rx,tx)}
#[test]
fn pending_remote_batch_allows_local_status_then_applies_the_complete_observation() {
    let(world,project)=remote_world();world.runner.on("report-metadata",ok(r#"{"result":{}}"#));let healthy=world.project("healthy","healthy.sock");world.thread(&healthy,Path::new("/healthy-fixture"),|t|{t.workspace_id="w3".into();t.tab_id="w3:t1".into();t.pane_id="w3:p1".into();t.last_state="working".into();t.last_group="working".into();});*world.agents.borrow_mut()=format!("[{}]",agent_json("w3","w3:t1","w3:p1","/healthy-fixture","hp-healthy-t-0001","blocked"));
    let ctx=world.ctx();let mut memory=Memory::new(&ctx);let(executor,started,release)=pool();memory.remote_reads=Some(Reads::new(executor.clone()));memory.pr_reads=Some(crate::pr_polling::Reads::with_executor(executor.clone()));let start=Instant::now();assert!(ticker::tick_for_test(&ctx,&mut memory));assert!(start.elapsed()<Duration::from_secs(1));started.recv_timeout(Duration::from_secs(1)).unwrap();assert_eq!(thread::load(&healthy,"t-0001").unwrap().last_state,"blocked");assert_eq!(thread::load(&project,"t-0001").unwrap().last_state,"working");assert!(items_of(&project,"outage").is_empty());
    assert!(ticker::tick_for_test(&ctx,&mut memory));assert!(started.try_recv().is_err());release.send(()).unwrap();let deadline=Instant::now()+Duration::from_secs(2);loop {ticker::tick_for_test(&ctx,&mut memory);if thread::load(&project,"t-0001").unwrap().last_state=="blocked"{break;}assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}assert_eq!(thread::load(&project,"t-0001").unwrap().last_group,"waiting-on-you");assert!(items_of(&project,"outage").is_empty());assert_eq!(world.runner.count("agent start"),0);assert_eq!(world.runner.count("agent prompt"),0);assert!(executor.stop(Duration::from_secs(2)));
}
#[test]
fn changing_remote_thread_or_config_discards_the_pending_batch() {
    let(world,project)=remote_world();let ctx=world.ctx();let mut memory=Memory::new(&ctx);let(executor,started,_release)=pool();memory.remote_reads=Some(Reads::new(executor.clone()));ticker::tick_for_test(&ctx,&mut memory);started.recv_timeout(Duration::from_secs(1)).unwrap();thread::update(&project,"t-0001",|t|t.lifecycle_generation+=1).unwrap();ticker::tick_for_test(&ctx,&mut memory);started.recv_timeout(Duration::from_secs(1)).unwrap();std::fs::create_dir_all(&ctx.config_dir).unwrap();std::fs::write(ctx.config_dir.join("config.toml"),"[machines.box]\nssh='new@host'\n").unwrap();ticker::tick_for_test(&ctx,&mut memory);started.recv_timeout(Duration::from_secs(1)).unwrap();assert_eq!(thread::load(&project,"t-0001").unwrap().last_state,"working");assert!(steps::load_state(&project).machine_outages.is_empty());assert!(executor.stop(Duration::from_secs(2)));
}
#[test]
fn unreachable_remote_observation_never_turns_missing_evidence_into_closed_panes() {
    struct Offline;
    impl Runner for Offline {
        fn run(&self,_:&Cmd)->anyhow::Result<Output>{Ok(fail(255,"fixture transport unavailable"))}
        fn socket_request(&self,_:&Path,_:&str,_:Duration)->anyhow::Result<String>{unreachable!()}
    }
    let(world,project)=remote_world();let ctx=world.ctx();let mut memory=Memory::new(&ctx);memory.outage_secs=0;let executor=Arc::new(Executor::new(Limits::default(),Arc::new(ProbeRunner{inner:Arc::new(Offline)})).unwrap());memory.remote_reads=Some(Reads::new(executor.clone()));let deadline=Instant::now()+Duration::from_secs(2);loop {assert!(ticker::tick_for_test(&ctx,&mut memory));if !items_of(&project,"outage").is_empty(){break;}assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}let t=thread::load(&project,"t-0001").unwrap();assert_eq!(t.last_state,"working");assert_eq!(t.last_group,"working");assert_eq!(t.status,Status::Open);assert_eq!(world.runner.count("agent start"),0);assert!(executor.stop(Duration::from_secs(2)));
}
