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

#[test]
fn idle_finalization_is_offered_without_synchronous_copy_or_forged_resolution() {
    let world=crate::scenarios::World::new();let project=world.project("demo","session.sock");
    let t=world.thread(&project,world.home.path(),|t|{t.last_group=thread::Group::Idle.token().into();t.last_state_change="2020-01-01T00:00:00Z".into();t.last_report_change=t.last_state_change.clone();});
    let pool=Arc::new(Executor::new(Limits::default(),Arc::new(Immediate)).unwrap());let ctx=world.ctx();let mut memory=Memory::new(&ctx);memory.copy_jobs=Some(crate::copy_jobs::Queue::new(pool.clone()));
    let settings=project.read_project_md().unwrap().0;let started="2020-01-01T00:00:00Z".parse().unwrap();
    assert!(steps::auto_resolve_queued(&ctx,&project,&settings,started,&steps::State::default(),jiff::Timestamp::now(),memory.copy_jobs.as_mut()).is_empty());
    assert!(memory.copy_jobs.as_ref().unwrap().outstanding(&project,&t.id));assert_eq!(pool.metrics().high_water[1],0);
    assert_eq!(thread::load(&project,&t.id).unwrap(),t);
    memory.copy_jobs.as_mut().unwrap().admit();drain(&mut memory);
    assert_eq!(thread::load(&project,&t.id).unwrap(),t,"queue success cannot resolve or certify copy");
    assert_eq!(world.runner.count("rsync"),0);assert!(pool.stop(Duration::from_secs(1)));
}

#[test]
fn retained_merged_projection_blocks_brief_prompts_and_agent_starts() {
    let world=crate::scenarios::World::new();let project=world.project("demo","session.sock");let url="https://github.com/example/repo/pull/1";
    let t=world.thread(&project,world.home.path(),|t|{t.prompt_pending=true;t.pr=url.into();t.pr_state="MERGED".into();});
    std::fs::create_dir_all(&t.thread_dir).unwrap();let report=format!("PR: {url}\ncomplete\n");
    std::fs::write(Path::new(&t.thread_dir).join("report.md"),&report).unwrap();std::fs::write(thread::home_report_path(&project,&t.id),&report).unwrap();
    let archive=world.home.path().join("final-stream");let mut bytes=Vec::new();crate::artifacts::live::export(Path::new(&t.thread_dir),&mut bytes).unwrap();std::fs::write(&archive,bytes).unwrap();
    let guard=herdr_projects::execution_guard::ProjectGuard::acquire(&project.dir()).unwrap();
    crate::artifacts::live::receive(&project,&archive).unwrap().begin_final_controlled(&project,&guard,&t,&"a".repeat(64),"merged-fixture",herdr_projects::final_copy_intent::Purpose::Merged{pr:url.into()},&crate::source_tree::Control::default(),||Ok(true)).unwrap();drop(guard);
    let ctx=world.ctx();let herdr=Herdr::new(ctx.env.herdr_bin(),"session.sock",ctx.runner);let current=thread::load(&project,&t.id).unwrap();
    let agent=Agent{pane_id:t.pane_id.clone(),workspace_id:t.workspace_id.clone(),tab_id:t.tab_id.clone(),cwd:t.cwd.clone(),name:t.agent_name.clone(),agent_status:"idle".into(),..Default::default()};
    let pane=Pane{pane_id:t.pane_id.clone(),workspace_id:t.workspace_id.clone(),tab_id:t.tab_id.clone(),cwd:t.cwd.clone()};
    thread_pass(&project,&herdr,&[current],&[agent],&[pane.clone()],Some(&std::collections::BTreeMap::new()),false).unwrap();
    let current=thread::load(&project,&t.id).unwrap();let mut errors=Vec::new();let mut may_start=true;
    launch_pass(&ctx,&project,&herdr,&[current],&[],&[pane],&mut may_start,&mut errors);
    assert!(errors.is_empty());assert_eq!(world.runner.count("agent prompt"),0);assert_eq!(world.runner.count("agent start"),0);
    let current=thread::load(&project,&t.id).unwrap();assert!(current.prompt_pending);assert!(current.pending_final_copy.is_some());assert_eq!(current.launch_attempts,t.launch_attempts);
}

#[cfg(target_os="linux")]
#[test]
fn remote_briefs_queue_without_synchronous_effects_and_require_saved_session_contract() {
    use std::os::unix::net::UnixListener;
    for supported in [true,false] {
        let world=crate::scenarios::World::new();let project=world.project("remote","session.sock");
        let socket=world.home.path().join("session.sock");std::fs::remove_file(&socket).unwrap();let _listener=UnixListener::bind(&socket).unwrap();
        let t=world.thread(&project,world.home.path(),|t|{t.machine="saved".into();t.prompt_pending=true;});
        let agents=vec![serde_json::from_str(&crate::scenarios::agent_json("w2","w2:t1","w2:p1",&t.cwd,&t.agent_name,"idle")).unwrap()];
        let panes=vec![serde_json::from_str(&crate::scenarios::pane_json("w2","w2:t1","w2:p1",&t.cwd)).unwrap()];
        let route=supported.then(||crate::remote_api::Route{id:"a".repeat(32),label:"saved".into(),target:"fixture.invalid".into(),session:"named".into(),enabled:true,selected:false});
        let observation=crate::remote_polling::Observation{agents,panes,route,target:"fixture.invalid".into(),hashes:Default::default()};
        let ctx=world.ctx();let herdr=Herdr::new(ctx.env.herdr_bin(),&socket,ctx.runner);let pool=Arc::new(Executor::new(Limits::default(),Arc::new(Immediate)).unwrap());let mut queue=crate::copy_jobs::Queue::new(pool.clone());let mut errors=Vec::new();
        apply_remote(&ctx,&project,&herdr,"saved",&[t.clone()],observation,true,&mut false,&mut errors,Some(&mut queue),&mut Default::default(),&mut Default::default()).unwrap();
        assert_eq!(queue.offered(),supported);assert_eq!(errors.is_empty(),supported,"{errors:?}");
        assert!(!world.runner.calls.borrow().iter().any(|c|["agent list","pane list","agent prompt","agent start","machine list"].iter().any(|text|c.display().contains(text))),"admission performed a synchronous terminal preflight/effect");
        assert!(thread::load(&project,&t.id).unwrap().prompt_pending);assert!(thread::load(&project,&t.id).unwrap().prompt_claim.is_none());
        if supported {assert!(queue.admit().is_empty());let end=Instant::now()+Duration::from_secs(3);while queue.pending(){assert!(Instant::now()<end);assert!(queue.drain().is_empty());std::thread::sleep(Duration::from_millis(2));}assert!(thread::load(&project,&t.id).unwrap().prompt_pending);}
        assert!(pool.stop(Duration::from_secs(1)));
    }
}

#[test]
fn delayed_remote_shell_still_refreshes_before_remaining_synchronous_starts() {
    for changed_route in [false,true] {
        let world=crate::scenarios::World::new();let project=world.project("remote","session.sock");let t=world.thread(&project,world.home.path(),|t|{t.machine="saved".into();t.prompt_pending=true;});
        *world.agents.borrow_mut()=format!("[{}]",crate::scenarios::agent_json("w2","w2:t1","w2:p1",&t.cwd,&t.agent_name,"working"));
        let pane=crate::scenarios::pane_json("w2","w2:t1","w2:p1",&t.cwd);*world.panes.borrow_mut()=format!("[{pane}]");
        world.runner.on("machine list",crate::runner::fake::ok(&serde_json::json!([{"label":"saved","target":if changed_route{"changed.invalid"}else{"fixture.invalid"}}]).to_string()));
        let observation=crate::remote_polling::Observation{agents:vec![],panes:vec![serde_json::from_str(&pane).unwrap()],route:None,target:"fixture.invalid".into(),hashes:Default::default()};
        let ctx=world.ctx();let herdr=Herdr::new(ctx.env.herdr_bin(),world.home.path().join("session.sock"),ctx.runner);let pool=Arc::new(Executor::new(Limits::default(),Arc::new(Immediate)).unwrap());let mut queue=crate::copy_jobs::Queue::new(pool.clone());
        let result=apply_remote(&ctx,&project,&herdr,"saved",&[t.clone()],observation,true,&mut true,&mut Vec::new(),Some(&mut queue),&mut Default::default(),&mut Default::default());
        assert_eq!(result.is_err(),changed_route);assert_eq!(thread::load(&project,&t.id).unwrap().launch_attempts,0);assert_eq!(world.runner.count("agent start"),0);
        assert_eq!(world.runner.count("machine list"),1);assert_eq!(world.runner.count("agent list"),usize::from(!changed_route));assert!(pool.stop(Duration::from_secs(1)));
    }
}
