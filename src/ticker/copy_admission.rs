//! Scheduling tests use a recording runner; durable copy behavior is covered by
//! the concrete-supervision fixtures in copy_jobs.
use super::*;
use std::sync::Arc;
use crate::{executor::{Executor,Limits},runner::{Runner,Cmd,Output}};
struct Immediate;
impl Runner for Immediate {
    fn run(&self,_:&Cmd)->Result<Output>{Ok(Output{code:Some(0),..Output::default()})}
    fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}
}
fn drain(memory:&mut Memory) {
    let deadline=Instant::now()+Duration::from_secs(3);
    loop {
        let mut pending=false;
        if let Some(queue)=memory.copy_jobs.as_mut(){assert!(queue.drain().is_empty());pending|=queue.pending();}
        #[cfg(feature="state-store")]
        if let Some(queue)=memory.routine_jobs.as_mut(){assert!(queue.drain().is_empty());pending|=queue.pending();}
        if !pending {break;}
        assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn newly_ready_thread_does_not_announce_old_unannounced_receipt_while_copy_is_offered() {
    let world=crate::scenarios::World::new();let project=world.project("demo","session.sock");
    let t=world.thread(&project,world.home.path(),|t|t.last_group=thread::Group::Idle.token().into());
    std::fs::create_dir_all(&t.thread_dir).unwrap();std::fs::write(Path::new(&t.thread_dir).join("report.md"),b"new").unwrap();
    std::fs::write(thread::home_report_path(&project,&t.id),b"old").unwrap();
    thread::copy_delivery::record(&project,&t,&thread::Copied{artifact_snapshot:None,outcome:thread::CopyOutcome::Complete,report_hash:Some(thread::sha256_hex(b"old"))}).unwrap();
    *world.panes.borrow_mut()=format!("[{},{}]",world.coordinator_pane(&project),crate::scenarios::pane_json("w2","w2:t1","w2:p1",&t.cwd));
    *world.agents.borrow_mut()=format!("[{}]",crate::scenarios::agent_json("w2","w2:t1","w2:p1",&t.cwd,&t.agent_name,"idle"));
    let pool=Arc::new(Executor::new(Limits::default(),Arc::new(Immediate)).unwrap());let ctx=world.ctx();let mut memory=Memory::new(&ctx);memory.copy_jobs=Some(crate::copy_jobs::Queue::new(pool.clone()));
    assert!(tick_project_with(&ctx,&project,&mut memory).unwrap());
    let current=thread::load(&project,&t.id).unwrap();assert_eq!(current.last_group,thread::Group::ReadyForReview.token());
    assert!(memory.copy_jobs.as_ref().unwrap().outstanding(&project,&t.id));assert!(current.pending_review_notice.is_none());assert!(current.last_review_item_hash.is_empty());
    assert_eq!(current.report_hash,thread::sha256_hex(b"old"));assert_eq!(pool.metrics().high_water[1],0,"single project pass may only offer copies");
    // Even a forged successful completion cannot change the copy receipt. The
    // next fresh source observation must offer it again before review preparation.
    memory.copy_jobs.as_mut().unwrap().admit();drain(&mut memory);
    assert!(tick_project_with(&ctx,&project,&mut memory).unwrap());assert!(thread::load(&project,&t.id).unwrap().last_review_item_hash.is_empty());
    assert!(pool.stop(Duration::from_secs(1)));
}
#[cfg(feature="state-store")]
#[test]
fn copy_and_routine_backlogs_alternate_only_after_ticket_drain() {
    let(world,path)=crate::canonical_controller::tests::routine_fixture(&[("check",b"true",1000)]);
    herdr_projects::routines::schedule(&path,"check",herdr_projects::runtime::snapshot(&path).unwrap().head).unwrap();
    let project=world.project("legacy","session.sock");let t=world.thread(&project,world.home.path(),|_|{});
    let pool=Arc::new(Executor::new(Limits::default(),Arc::new(Immediate)).unwrap());let ctx=world.ctx();let mut memory=Memory::new(&ctx);
    memory.copy_jobs=Some(crate::copy_jobs::Queue::new(pool.clone()));memory.routine_jobs=Some(crate::routine_jobs::Queue::new(pool.clone()));
    let log=Log{path:world.home.path().join("log")};
    for copy in [true,false,true,false] {
        memory.copy_jobs.as_mut().unwrap().offer(&ctx,&project,&t,None).unwrap();
        admit_background(&ctx,&log,&mut memory,vec![path.clone()]);
        assert_eq!(memory.copy_jobs.as_ref().unwrap().pending(),copy);assert_eq!(memory.routine_jobs.as_ref().unwrap().pending(),!copy);
        for _ in 0..20 {admit_background(&ctx,&log,&mut memory,vec![path.clone()]);}
        assert_eq!(memory.copy_jobs.as_ref().unwrap().pending(),copy);assert_eq!(memory.routine_jobs.as_ref().unwrap().pending(),!copy);
        drain(&mut memory);
    }
    assert!(thread::load(&project,&t.id).unwrap().copy_receipt.is_none());assert!(herdr_projects::runtime::snapshot(&path).unwrap().routine_receipts.is_empty());
    assert!(pool.stop(Duration::from_secs(1)));
}

#[cfg(feature="state-store")]
#[test]
fn canonical_effects_get_their_turn_before_copy_admission() {
    use herdr_projects::{runtime,operations::DeliveryState};
    let(world,path,task)=crate::notification_delivery::tests::fixture();
    world.runner.on("--version",crate::runner::fake::ok("herdr 0.9.1"));
    let operation=crate::notification_delivery::enqueue(&world.ctx(),&path,&task,runtime::snapshot(&path).unwrap().head).unwrap();
    let project=world.project("alpha","session.sock");let t=world.thread(&project,world.home.path(),|_|{});
    std::fs::create_dir_all(&t.thread_dir).unwrap();std::fs::write(Path::new(&t.thread_dir).join("report.md"),b"new").unwrap();
    let pool=Arc::new(Executor::new(Limits::default(),Arc::new(Immediate)).unwrap());let observed=pool.clone();
    world.runner.on_fn(|c|c.display().contains("notification show"),move |_| {
        assert_eq!(observed.metrics().high_water[1],0,"copy admission preempted a canonical effect");
        Ok(crate::runner::fake::ok(r#"{"result":{"shown":true}}"#))
    });
    let ctx=world.ctx();let mut memory=Memory::new(&ctx);memory.copy_jobs=Some(crate::copy_jobs::Queue::new(pool.clone()));
    assert!(tick_for_test(&ctx,&mut memory));assert_eq!(world.runner.count("notification show"),1);
    assert_eq!(runtime::snapshot(&path).unwrap().deliveries.iter().find(|d|d.operation==operation.id).unwrap().state,DeliveryState::Confirmed);
    assert!(memory.copy_jobs.as_ref().unwrap().pending());assert!(pool.stop(Duration::from_secs(1)));
}
