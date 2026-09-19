//! W01 polling scenarios. All external operations use FakeRunner.

use super::*;
use crate::steps::Memory;

#[test]
fn shared_remote_projects_launch_prompt_copy_and_recover_independently() {
    let world = World { runner: FakeRunner::new(), ..World::new() };
    let projects = [
        world.project("alpha", "a.sock"),
        world.project("beta", "b.sock"),
        world.project("sleeping", "c.sock"),
    ];
    let phase = Rc::new(RefCell::new(0));
    let down = Rc::new(RefCell::new(true));
    for project in &projects {
        let cwd = format!("/home/me/{}", project.slug);
        world.thread(project, Path::new(&cwd), |t| {
            t.machine = "box".into();
            t.prompt_pending = true;
        });
        let socket = project.coordinator().unwrap().socket;
        let remote_socket = socket.clone();
        let agent = agent_json("w2", "w2:t1", "w2:p1", &cwd, &format!("hp-{}-t-0001", project.slug), "idle");
        let stage = phase.clone();
        let unavailable = down.clone();
        let sleeping = project.slug == "sleeping";
        world.runner.on_fn(
            move |cmd| is_machine_call(cmd) && cmd.display().contains("agent list") && socket_of(cmd) == remote_socket,
            move |_| {
                if sleeping && *unavailable.borrow() { return Ok(fail(255, "fixture: session offline")); }
                let agents = if *stage.borrow() == 0 { "" } else { &agent };
                Ok(ok(&format!(r#"{{"result":{{"agents":[{agents}]}}}}"#)))
            },
        );
        let local_pane = world.coordinator_pane(project);
        let remote_pane = pane_json("w2", "w2:t1", "w2:p1", &cwd);
        world.runner.on_fn(
            move |cmd| cmd.display().contains("pane list") && socket_of(cmd) == socket,
            move |cmd| {
                let pane = if is_machine_call(cmd) { &remote_pane } else { &local_pane };
                Ok(ok(&format!(r#"{{"result":{{"panes":[{pane}]}}}}"#)))
            },
        );
        let dir = thread::load(project, "t-0001").unwrap().thread_dir;
        let report = format!("## Report\n{} result\n", project.slug);
        let hash = thread::sha256_hex(report.as_bytes());
        let stage = phase.clone();
        let source = dir.clone();
        world.runner.on_fn(
            move |cmd| cmd.program == "ssh" && cmd.args.last().is_some_and(|s| s.contains(&source)),
            move |cmd| {
                if cmd.args.last().unwrap().contains("echo dir_ok") {
                    Ok(ok(if *stage.borrow() == 3 { "absent\n" } else { "dir_ok\nreport_ok\n" }))
                } else if *stage.borrow() < 2 {
                    Ok(ok("t-0001 -\n"))
                } else if *stage.borrow() >= 3 {
                    // Observation differs from the bytes ultimately fetched.
                    Ok(ok(&format!("t-0001 {}\n", "a".repeat(64))))
                } else {
                    Ok(ok(&format!("t-0001 {hash}\n")))
                }
            },
        );
        world.runner.on_fn(
            move |cmd| cmd.program == "scp" && cmd.display().contains(&dir),
            move |cmd| {
                std::fs::write(cmd.args.last().unwrap(), &report)?;
                Ok(ok(""))
            },
        );
    }
    world.runner.on("agent start", ok(r#"{"result":{"agent":{"workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1"}}}"#));
    world.runner.on("agent prompt", ok(r#"{"result":{}}"#));
    world.runner.on("agent list", ok(r#"{"result":{"agents":[]}}"#));
    world.runner.on("machine list --json", ok(r#"[{"label":"box","target":"me@box"}]"#));
    world.runner.on("report-metadata", ok(r#"{"result":{}}"#));
    world.runner.on("notification show", ok(r#"{"result":{"shown":true}}"#));

    let local = world.project("local", "local.sock");
    let pane = world.coordinator_pane(&local);
    world.runner.on("pane list", ok(&format!(r#"{{"result":{{"panes":[{pane}]}}}}"#)));
    write_routine(&local, "healthy", "+++\nschedule = \"every 1h\"\n+++\nLocal work continues.\n");
    make_due(&local, "healthy");
    let ctx = world.ctx();
    let mut memory = Memory::new(&ctx);
    memory.outage_secs = 0;
    for stage in 0..3 {
        *phase.borrow_mut() = stage;
        for _ in 0..4 { assert!(ticker::tick_for_test(&ctx, &mut memory)); }
        for project in &projects[..2] {
            let t = thread::load(project, "t-0001").unwrap();
            assert_eq!(t.launch_attempts, 1, "{} must launch exactly once", project.slug);
            assert_eq!(t.prompt_pending, stage == 0, "{} prompt stage {stage}", project.slug);
            if stage == 2 {
                let report = std::fs::read_to_string(thread::home_report_path(project, &t.id)).unwrap();
                assert_eq!(report, format!("## Report\n{} result\n", project.slug));
                assert_eq!(t.report_hash, thread::sha256_hex(report.as_bytes()));
            }
        }
    }
    assert_eq!(items_of(&local, "routine").len(), 1);
    assert_eq!(items_of(&projects[2], "outage").len(), 1);
    assert!(items_of(&projects[0], "outage").is_empty());
    assert!(items_of(&projects[1], "outage").is_empty());
    *down.borrow_mut() = false;
    for _ in 0..12 { assert!(ticker::tick_for_test(&ctx, &mut memory)); }
    assert_eq!(items_of(&projects[2], "outage").len(), 2);
    assert!(items_of(&projects[2], "outage")[1].summary.contains("reachable again"));
    assert!(thread::home_report_path(&projects[2], "t-0001").exists());

    // Disappearance after observation must not acknowledge uncopied bytes.
    // A later successful transfer must acknowledge the actual destination.
    for stage in [3, 4] {
        *phase.borrow_mut() = stage;
        for _ in 0..4 { assert!(ticker::tick_for_test(&ctx, &mut memory)); }
        for project in &projects {
            let report = std::fs::read(thread::home_report_path(project, "t-0001")).unwrap();
            assert_eq!(thread::load(project, "t-0001").unwrap().report_hash, thread::sha256_hex(&report),
                "{}: receipt must describe the copied bytes at stage {stage}", project.slug);
        }
    }
}
