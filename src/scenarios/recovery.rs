//! W01 file-backed recovery and fault boundaries. External services are mocked.

use super::*;
use crate::steps::{self, Memory, State};

const MERGED: &str = include_str!("../../tests/fixtures/review/merged-pr.json");

fn now() -> jiff::Timestamp { "2026-09-19T12:00:00Z".parse().unwrap() }
fn later(now: jiff::Timestamp, seconds: i64) -> jiff::Timestamp { now + jiff::SignedDuration::from_secs(seconds) }

fn poll(world: &World, project: &Project, at: jiff::Timestamp) -> Vec<anyhow::Error> {
    let ctx = world.ctx();
    let mut state = steps::load_state(project);
    let mut memory = Memory::new(&ctx);
    memory.outage_secs = 0;
    let errors = steps::pull_requests(&ctx, project, &mut state, &mut memory, at);
    steps::save_state(project, &state).unwrap();
    errors
}

fn copy_fixture() -> (World, Project) {
    let (world, project) = pr_world(MERGED);
    let t = thread::load(&project, "t-0001").unwrap();
    std::fs::create_dir_all(Path::new(&t.thread_dir).join("library")).unwrap();
    world.runner.on("du -sk", ok("4\t/library\n"));
    (world, project)
}

#[test]
fn finalization_intent_precedes_copy_and_retry_precedes_next_pr_poll() {
    let (world, project) = copy_fixture();
    let project_copy = project.clone();
    let copies = Rc::new(RefCell::new(0));
    let count = copies.clone();
    world.runner.on_fn(|cmd| cmd.program == "rsync", move |_| {
        let durable = steps::load_state(&project_copy);
        let pending = &durable.finalizations["t-0001"];
        *count.borrow_mut() += 1;
        assert_eq!(pending.retry.attempts, *count.borrow());
        assert!(!pending.operation_id.is_empty());
        Ok(if *count.borrow() == 1 { fail(12, "copy unavailable") } else { ok("") })
    });
    assert_eq!(poll(&world, &project, now()).len(), 1);
    let retry = steps::load_state(&project).finalizations["t-0001"].retry.clone();
    assert!(retry.last_error.contains("copy unavailable"));
    assert!(poll(&world, &project, later(now(), 1)).is_empty());
    assert_eq!(*copies.borrow(), 1);
    let due = retry.next_attempt.parse().unwrap();
    assert!(due < later(now(), steps::PR_INTERVAL_SECS));
    assert!(poll(&world, &project, due).is_empty());
    assert_eq!(*copies.borrow(), 2);
    assert_eq!(world.runner.count("gh pr view"), 1);
    let t = thread::load(&project, "t-0001").unwrap();
    assert_eq!(t.status, Status::Resolved);
    assert!(!t.last_finalization.is_empty());
    assert!(steps::load_state(&project).finalizations.is_empty());
    assert_eq!(items_of(&project, "pr").len(), 1);
}

#[test]
fn crash_after_thread_commit_does_not_repeat_the_copy() {
    let (world, project) = copy_fixture();
    world.runner.on("rsync", fail(12, "offline"));
    assert_eq!(poll(&world, &project, now()).len(), 1);
    let state = steps::load_state(&project);
    let operation = state.finalizations["t-0001"].operation_id.clone();
    // State immediately after a successful thread commit but before clearing
    // the operation record. Restart must reconcile without another transfer.
    thread::update(&project, "t-0001", |t| {
        t.status = Status::Resolved;
        t.resolved_reason = "merged".into();
        t.last_finalization = operation;
    }).unwrap();
    assert!(poll(&world, &project, later(now(), 121)).is_empty());
    assert_eq!(world.runner.count("rsync"), 1);
    assert!(steps::load_state(&project).finalizations.is_empty());
}

#[test]
fn a_thread_rebound_during_copy_is_not_resolved() {
    let (world, project) = copy_fixture();
    let changed = project.clone();
    world.runner.on_fn(|cmd| cmd.program == "rsync", move |_| {
        thread::update(&changed, "t-0001", |t| t.pane_id = "replacement:pane".into()).unwrap();
        Ok(ok(""))
    });
    let errors = poll(&world, &project, now());
    assert!(errors.iter().any(|e| e.to_string().contains("identity changed")), "{errors:?}");
    assert_eq!(thread::load(&project, "t-0001").unwrap().status, Status::Open);
    assert!(poll(&world, &project, later(now(), 121)).is_empty());
    assert_eq!(world.runner.count("rsync"), 1);
    assert!(steps::load_state(&project).finalizations.is_empty());
}

#[test]
fn pause_during_copy_preserves_pending_until_resume() {
    let (world, project) = copy_fixture();
    let changed = project.clone();
    let first = Rc::new(RefCell::new(true));
    let flag = first.clone();
    world.runner.on_fn(|cmd| cmd.program == "rsync", move |_| {
        if *flag.borrow() { changed.set_status(project::Status::Paused).unwrap(); *flag.borrow_mut() = false; }
        Ok(ok(""))
    });
    assert_eq!(poll(&world, &project, now()).len(), 1);
    assert!(poll(&world, &project, later(now(), 121)).is_empty());
    assert_eq!(world.runner.count("rsync"), 1);
    assert_eq!(thread::load(&project, "t-0001").unwrap().status, Status::Open);
    project.set_status(project::Status::Active).unwrap();
    assert!(poll(&world, &project, later(now(), 242)).is_empty());
    assert_eq!(thread::load(&project, "t-0001").unwrap().status, Status::Resolved);
}

#[test]
fn failed_intent_write_prevents_copy() {
    let (world, project) = copy_fixture();
    world.runner.on("rsync", ok(""));
    std::fs::create_dir(project.state_dir().join("ticker.json")).unwrap();
    let mut state = State::default();
    let ctx = world.ctx();
    let errors = steps::pull_requests(&ctx, &project, &mut state, &mut Memory::new(&ctx), now());
    assert!(!errors.is_empty());
    assert_eq!(world.runner.count("rsync"), 0);
    assert_eq!(thread::load(&project, "t-0001").unwrap().status, Status::Open);
}

#[test]
fn notification_failure_does_not_stop_the_project_slow_pass() {
    let world = World { runner: FakeRunner::new(), ..World::new() };
    let project = world.project("demo", "a.sock");
    world.runner.on("agent list", ok(r#"{"result":{"agents":[]}}"#));
    world.runner.on("pane list", ok(&format!(r#"{{"result":{{"panes":[{}]}}}}"#, world.coordinator_pane(&project))));
    world.runner.on("notification show", fail(1, "offline"));
    inbox::write(&project, "routine", "old", "due", "").unwrap();
    write_routine(&project, "healthy", "+++\nschedule = \"every 1h\"\n+++\nContinue.\n");
    make_due(&project, "healthy");
    let ctx = world.ctx();
    assert!(ticker::tick_for_test(&ctx, &mut Memory::new(&ctx)));
    assert_eq!(items_of(&project, "routine").len(), 2);
    assert!(steps::load_state(&project).notification_retry.retry.last_error.contains("offline"));
}

#[test]
fn idle_auto_resolve_does_not_bypass_a_pending_merge_retry() {
    let (world, project) = copy_fixture();
    world.runner.on("rsync", fail(12, "offline"));
    assert_eq!(poll(&world, &project, now()).len(), 1);
    thread::update(&project, "t-0001", |t| {
        t.last_state_change = "2026-01-01T00:00:00Z".into();
        t.last_report_change = "2026-01-01T00:00:00Z".into();
    }).unwrap();
    let ctx = world.ctx();
    let mut memory = Memory::new(&ctx);
    memory.started = "2026-01-01T00:00:00Z".parse().unwrap();
    let (mut settings, _) = project.read_project_md().unwrap();
    settings.auto_resolve_days = 1;
    assert!(steps::auto_resolve(&ctx, &project, &settings, &memory, &steps::load_state(&project), now()).is_empty());
    assert_eq!(world.runner.count("rsync"), 1);
    assert_eq!(thread::load(&project, "t-0001").unwrap().status, Status::Open);
}

#[test]
fn merged_finalization_queue_keeps_durable_retry_until_worker_commits() {
    use std::sync::Arc;
    let(world,project)=copy_fixture();let ctx=world.ctx();
    let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(crate::runner::RealRunner)).unwrap());
    let mut memory=Memory::new(&ctx);memory.copy_jobs=Some(crate::copy_jobs::Queue::new(pool.clone()));
    let mut state=State::default();
    assert!(steps::pull_requests(&ctx,&project,&mut state,&mut memory,now()).is_empty());
    assert_eq!(world.runner.count("rsync"),0);assert_eq!(thread::load(&project,"t-0001").unwrap().status,Status::Open);
    let pending=steps::load_state(&project).finalizations["t-0001"].clone();assert_eq!(pending.retry.attempts,1);
    assert!(memory.copy_jobs.as_ref().unwrap().outstanding(&project,"t-0001"));assert_eq!(pool.metrics().high_water[1],0);
    // Simulate the worker's durable commit before a ticker restart.
    thread::update(&project,"t-0001",|t|{t.status=Status::Resolved;t.last_finalization=pending.operation_id;}).unwrap();
    let mut restarted=steps::load_state(&project);
    assert!(steps::pull_requests(&ctx,&project,&mut restarted,&mut memory,later(now(),1)).is_empty());
    assert!(steps::load_state(&project).finalizations.is_empty());assert_eq!(world.runner.count("rsync"),0);
    assert!(pool.stop(std::time::Duration::from_secs(1)));
}
