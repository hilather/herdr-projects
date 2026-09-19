//! T00.1: executable counterexamples from the architecture review.
//!
//! These assert the repaired behavior and run in the normal suite. Run alone
//! with `cargo test --locked review_regressions -- --nocapture`.
//! Historical counterexamples are recorded in docs/review-baseline.md.

use super::*;
use crate::runner::{RealRunner, Runner};
use crate::thread::CopyOutcome;
use std::fs::{File, FileTimes};
use std::time::{Duration, Instant, UNIX_EPOCH};

#[test]
fn f01_artifact_copy_preserves_changed_bytes_with_identical_metadata() {
    let world = World::new();
    let project = world.project("demo", "a.sock");
    let t = world.thread(&project, world.home.path(), |_| {});
    let library = Path::new(&t.thread_dir).join("library");
    std::fs::create_dir_all(&library).unwrap();
    let source = library.join("artifact.bin");
    let destination = project.dir().join("library").join(&t.id).join("artifact.bin");
    let modified = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let write_version = |bytes: &[u8]| {
        std::fs::write(&source, bytes).unwrap();
        File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
    };

    write_version(b"version A\x00\xff");
    let first = thread::copy_home_local(&project, &t, true, &RealRunner);
    assert_eq!(first.outcome, CopyOutcome::Complete, "initial real rsync copy");
    assert_eq!(std::fs::read(&destination).unwrap(), b"version A\x00\xff");
    let old_metadata = std::fs::metadata(&source).unwrap();

    write_version(b"version B\x00\xfe");
    let new_metadata = std::fs::metadata(&source).unwrap();
    assert_eq!(old_metadata.len(), new_metadata.len());
    assert_eq!(old_metadata.modified().unwrap(), new_metadata.modified().unwrap());
    let second = thread::copy_home_local(&project, &t, true, &RealRunner);
    assert_eq!(second.outcome, CopyOutcome::Complete, "second real rsync copy");
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        std::fs::read(&source).unwrap(),
        "F01: a complete copy must contain the latest artifact bytes"
    );
}

#[test]
fn f02_shared_machine_serves_both_project_sessions() {
    let world = World { runner: FakeRunner::new(), ..World::new() };
    let projects = [world.project("alpha", "a.sock"), world.project("beta", "b.sock")];
    for project in &projects {
        world.thread(project, Path::new("/home/me/wt"), |t| {
            t.machine = "box".into();
            t.last_group = "working".into();
            t.last_state = "working".into();
            t.last_state_change = "2026-01-01T00:00:00Z".into();
        });
        let socket = project.coordinator().unwrap().socket;
        let remote_socket = socket.clone();
        let agent = agent_json("w2", "w2:t1", "w2:p1", "/home/me/wt", &format!("hp-{}-t-0001", project.slug), "blocked");
        world.runner.on_fn(
            move |cmd| is_machine_call(cmd) && cmd.display().contains("agent list") && socket_of(cmd) == remote_socket,
            move |_| Ok(ok(&format!(r#"{{"result":{{"agents":[{agent}]}}}}"#))),
        );
        let pane = world.coordinator_pane(project);
        world.runner.on_fn(
            move |cmd| !is_machine_call(cmd) && cmd.display().contains("pane list") && socket_of(cmd) == socket,
            move |_| Ok(ok(&format!(r#"{{"result":{{"panes":[{pane}]}}}}"#))),
        );
    }
    world.runner.on_fn(is_machine_call, |_| Ok(ok(r#"{"result":{"panes":[]}}"#)));
    world.runner.on("machine list --json", ok(r#"[{"id":"1","label":"box","target":"me@box"}]"#));
    world.runner.on("ssh", ok("t-0001 -\n"));
    world.runner.on("agent list", ok(r#"{"result":{"agents":[]}}"#));
    world.runner.on("report-metadata", ok(r#"{"result":{}}"#));
    world.runner.on("notification show", ok(r#"{"result":{"shown":true}}"#));

    let ctx = world.ctx();
    let mut memory = Memory::new(&ctx);
    let mut poll_counts = [0; 2];
    for tick in 1..=12 {
        assert!(ticker::tick_for_test(&ctx, &mut memory));
        if tick % crate::steps::REMOTE_EVERY_TICKS == 0 {
            for (index, project) in projects.iter().enumerate() {
                let socket = project.coordinator().unwrap().socket;
                let count = world.runner.calls.borrow().iter().filter(|cmd| {
                    is_machine_call(cmd) && cmd.display().contains("agent list") && socket_of(cmd) == socket
                }).count();
                assert!(count > poll_counts[index], "F02: {} starved through tick {tick}; polls={count}", project.slug);
                poll_counts[index] = count;
                assert_eq!(thread::load(project, "t-0001").unwrap().last_group, "waiting-on-you");
            }
        }
    }
}

#[test]
fn f03_merged_finalization_retries_after_restart() {
    let (world, project) = pr_world(include_str!("../../tests/fixtures/review/merged-pr.json"));
    let t = thread::load(&project, "t-0001").unwrap();
    std::fs::create_dir_all(Path::new(&t.thread_dir).join("library")).unwrap();
    world.runner.on("du -sk", ok("4\t/library\n"));
    let failing = Rc::new(RefCell::new(true));
    let flag = failing.clone();
    world.runner.on_fn(
        |cmd| cmd.program == "rsync",
        move |_| Ok(if *flag.borrow() { fail(12, "fixture: copy transport unavailable") } else { ok("") }),
    );
    let ctx = world.ctx();
    let now: jiff::Timestamp = "2026-09-19T12:00:00Z".parse().unwrap();
    let mut state = crate::steps::load_state(&project);
    let errors = crate::steps::pull_requests(&ctx, &project, &mut state, &mut Memory::new(&ctx), now);
    assert_eq!(errors.len(), 1, "expected only the injected final-copy failure: {errors:?}");
    assert!(format!("{:#}", errors[0]).contains("fixture: copy transport unavailable"));
    assert_eq!(thread::load(&project, &t.id).unwrap().status, Status::Open);
    assert_eq!(world.runner.count("rsync"), 1);
    crate::steps::save_state(&project, &state).unwrap();

    // Reload persisted state and replace process memory, as a ticker restart does.
    *failing.borrow_mut() = false;
    let mut state = crate::steps::load_state(&project);
    let mut memory = Memory::new(&ctx);
    let later = now + jiff::SignedDuration::from_secs(crate::steps::PR_INTERVAL_SECS + 1);
    let errors = crate::steps::pull_requests(&ctx, &project, &mut state, &mut memory, later);
    assert!(errors.is_empty(), "recovered transport: {errors:?}");
    assert_eq!(world.runner.count("gh pr view"), 1, "persisted merged intent retries without another GitHub request");
    assert_eq!(thread::load(&project, &t.id).unwrap().status, Status::Resolved, "F03: persisted merged observation must not suppress unfinished copying");
    assert_eq!(world.runner.count("rsync"), 2);
    assert_eq!(items_of(&project, "pr").len(), 1, "retry must not repeat the merged notification");

    let again = later + jiff::SignedDuration::from_secs(crate::steps::PR_INTERVAL_SECS + 1);
    assert!(crate::steps::pull_requests(&ctx, &project, &mut state, &mut memory, again).is_empty());
    assert_eq!(world.runner.count("rsync"), 2, "completed finalization is a no-op");
}

#[test]
fn f04_deadline_includes_descendant_held_pipes() {
    // The descendant exits by itself after two seconds even on the broken
    // runner, keeping this negative fixture finite and leaving no daemon behind.
    let start = Instant::now();
    let out = RealRunner.run(
        &Cmd::new("sh", Duration::from_millis(150)).args(["-c", "sleep 2 &"]).own_group(),
    ).unwrap();
    let elapsed = start.elapsed();
    assert!(out.timed_out && !out.success() && elapsed < Duration::from_secs(1),
        "F04: expected timeout within 1s including cleanup; elapsed={elapsed:?}, output={out:?}");
}

#[test]
fn f05_unicode_schedule_returns_error_without_panicking() {
    for text in ["every 1日", "every 1🦀", "every é"] {
        assert!(crate::routine::parse_schedule(text).is_err(), "{text}");
    }
}

#[test]
fn f05_overflow_schedule_returns_error_without_panicking() {
    for unit in ['m', 'h', 'd'] {
        let text = format!("every {}{unit}", i64::MAX);
        assert!(crate::routine::parse_schedule(&text).is_err(), "{text}");
    }
}
