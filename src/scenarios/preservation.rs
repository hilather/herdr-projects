use super::*;

fn fixture() -> (World, Project, tempfile::TempDir, Thread) {
    let world = World::new();
    let project = world.project("demo", "a.sock");
    let work = tempfile::tempdir().unwrap();
    let repo = world.home.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let record = world.thread(&project, work.path(), |t| t.repo = repo.to_str().unwrap().into());
    std::fs::create_dir_all(&record.thread_dir).unwrap();
    std::fs::write(Path::new(&record.thread_dir).join("report.md"), "preserve this report").unwrap();
    (world, project, work, record)
}

#[test]
fn inactive_projects_cannot_restart_open_or_adopt_execution() {
    for status in [project::Status::Paused, project::Status::Archived] {
        let (world, project, _work, record) = fixture();
        project.set_status(status).unwrap();
        assert!(threads::restart(&world.ctx(), "demo", &record.id).is_err());
        assert!(threads::prompt(&world.ctx(), "demo", &record.id, "start more work").is_err());
        assert!(coordinator::open(&world.ctx(), "demo", &coordinator::OpenOptions { session: Default::default(), reprime: false, rebind: false }).is_err());
        assert!(crate::adopt::adopt(&world.ctx(), "demo", "w2:p1", "adopt", None).is_err());
        assert!(world.runner.calls.borrow().is_empty(), "refusal precedes ticker start or Herdr side effects");
        assert_eq!(thread::load(&project, &record.id).unwrap(), record);
    }
}

#[test]
fn restart_refuses_a_pane_claimed_by_another_project_in_the_same_session() {
    let (world, project, work, record) = fixture();
    let other = world.project("other", "a.sock");
    world.thread(&other, work.path(), |_| {});
    *world.panes.borrow_mut() = format!("[{}]", pane_json("w2", "w2:t1", "w2:p1", work.path().to_str().unwrap()));
    let error = threads::restart(&world.ctx(), "demo", &record.id).unwrap_err();
    assert!(error.to_string().contains("also claimed"), "{error:#}");
    assert_eq!(thread::load(&project, &record.id).unwrap(), record);
    assert_eq!(world.runner.count("agent start") + world.runner.count("agent prompt") + world.runner.count("worktree open"), 0);
}

#[test]
fn resolve_records_a_verified_snapshot_but_cleanup_waits_for_writer_exclusion() {
    let (world, project, work, record) = fixture();
    let error = threads::resolve(&world.ctx(), "demo", &record.id, &ResolveArgs { remove_worktree: true, ..Default::default() }).unwrap_err();
    assert!(error.to_string().contains("writer quiescence"), "{error:#}");
    let current = thread::load(&project, &record.id).unwrap();
    assert!(!current.artifact_snapshot.is_empty());
    crate::artifacts::load(&project, &current, &current.artifact_snapshot).unwrap();
    assert!(work.path().exists());
    assert_eq!(current.status, Status::Open);
    assert_eq!(world.runner.count("worktree remove"), 0);
    threads::resolve(&world.ctx(), "demo", &record.id, &ResolveArgs::default()).unwrap();
    assert_eq!(thread::load(&project, &record.id).unwrap().status, Status::Resolved);
    assert!(work.path().exists());
}

#[test]
fn idle_managed_panes_and_adopted_references_block_cleanup() {
    for shared in [false, true] {
        let (world, project, work, record) = fixture();
        if shared {
            let other = world.project("other", "b.sock");
            world.thread(&other, work.path(), |t| t.kind = Kind::Adopted);
        } else {
            let pane = pane_json("w2", "w2:t1", "w2:p1", work.path().to_str().unwrap());
            *world.panes.borrow_mut() = format!("[{pane}]");
        }
        let error = threads::resolve(&world.ctx(), "demo", &record.id, &ResolveArgs { remove_worktree: true, ..Default::default() }).unwrap_err();
        assert!(error.to_string().contains(if shared { "shared/adopted" } else { "managed pane" }), "{error:#}");
        assert_eq!(world.runner.count("worktree remove"), 0);
        assert!(work.path().exists());
        assert_eq!(thread::load(&project, &record.id).unwrap().status, Status::Open);
    }
}

#[test]
fn linked_live_library_destinations_never_receive_a_transfer() {
    for replace_parent in [false, true] {
        let (world, project, _work, record) = fixture();
        std::fs::create_dir(Path::new(&record.thread_dir).join("library")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let parent = project.dir().join("library");
        let target = if replace_parent { std::fs::remove_dir(&parent).unwrap(); parent } else { parent.join(&record.id) };
        std::os::unix::fs::symlink(outside.path(), &target).unwrap();
        world.runner.on("du -sk", ok("0\tlibrary\n"));
        let copied = thread::copy_home_local(&project, &record, true, &world.runner);
        assert!(matches!(copied.outcome, thread::CopyOutcome::Failed(ref error) if error.contains("not a real directory")));
        assert_eq!(world.runner.count("rsync"), 0);
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}

#[test]
fn failed_receipt_commit_is_a_copy_failure_and_keeps_the_source() {
    let (world, project, work, record) = fixture();
    std::fs::create_dir(Path::new(&record.thread_dir).join("library")).unwrap();
    world.runner.on("du -sk", ok("0\tlibrary\n"));
    let path = thread::record_path(&project, &record.id);
    world.runner.on_fn(|cmd| cmd.program == "rsync", move |_| {
        std::fs::rename(&path, path.with_extension("backup"))?;
        std::fs::create_dir(&path)?;
        Ok(ok(""))
    });
    let copied = threads::final_copy(&world.ctx(), &project, &record);
    assert!(matches!(copied.outcome, thread::CopyOutcome::Failed(ref e) if e.contains("commit final-copy receipt")));
    assert!(work.path().exists());
    assert_eq!(world.runner.count("worktree remove"), 0);
}

#[test]
fn lost_source_cannot_replace_a_previous_preservation_receipt() {
    let (world, project, _work, record) = fixture();
    let first = threads::final_copy(&world.ctx(), &project, &record);
    assert!(matches!(first.outcome, thread::CopyOutcome::Complete));
    let saved = thread::load(&project, &record.id).unwrap();
    std::fs::remove_dir_all(&record.thread_dir).unwrap();
    let failed = threads::final_copy(&world.ctx(), &project, &saved);
    assert!(matches!(failed.outcome, thread::CopyOutcome::Failed(_)));
    assert_eq!(thread::load(&project, &record.id).unwrap().artifact_snapshot, saved.artifact_snapshot);
    crate::artifacts::load(&project, &saved, &saved.artifact_snapshot).unwrap();
}

#[test]
fn corrupt_retry_state_is_preserved_and_healthy_projects_continue() {
    let world = World::new();
    let broken = world.project("broken", "a.sock");
    let healthy = world.project("healthy", "b.sock");
    let path = broken.state_dir().join("ticker.json");
    std::fs::write(&path, b"{broken retry obligations").unwrap();
    assert!(ticker::tick_project(&world.ctx(), &broken).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"{broken retry obligations");
    write_routine(&healthy, "continue", "+++\nschedule = \"every 1h\"\n+++\nContinue.\n");
    make_due(&healthy, "continue");
    ticker::tick_project(&world.ctx(), &healthy).unwrap();
    assert_eq!(items_of(&healthy, "routine").len(), 1);
}

#[test]
fn context_includes_current_instructions_and_broken_record_diagnostics() {
    let world = World::new();
    let project = world.project("demo", "a.sock");
    let record = world.thread(&project, world.home.path(), |_| {});
    std::fs::write(project.dir().join("threads/t-0002.toml"), "broken = [").unwrap();
    std::fs::write(project.dir().join("inbox/broken.md"), "broken inbox").unwrap();
    inbox::write(&project, "routine", "healthy", "still visible", "").unwrap();
    let (settings, _) = project.read_project_md().unwrap();
    let mut revisions = Vec::new();
    for instruction in ["Always use the purple test fixture.", "Now use the orange fixture."] {
        std::fs::write(project.project_md(), format!("+++\n{}+++\n\n{instruction}\n", toml::to_string(&settings).unwrap())).unwrap();
        let digest = coordinator::digest(&world.ctx(), &project, "hp").unwrap().0;
        assert!(digest.contains(instruction));
        assert!(thread::brief_for(&project, &record, "test task", false).unwrap().contains(instruction));
        assert!(digest.contains("t-0002.toml") && digest.contains("preserve and repair"));
        assert!(digest.contains("inbox/broken.md") && digest.contains("still visible"));
        assert!(digest.contains(&record.id));
        assert!(digest.contains("max_parallel_threads is advisory"));
        revisions.push(digest.lines().find(|line| line.starts_with("## Project instructions")).unwrap().to_string());
    }
    assert_ne!(revisions[0], revisions[1]);
    assert_eq!(thread::list_with_diagnostics(&project).0.len(), 1);
    assert_eq!(thread::list_with_diagnostics(&project).1.len(), 1);
    assert_eq!(inbox::unhandled_with_diagnostics(&project).0.len(), 1);
    assert_eq!(inbox::unhandled_with_diagnostics(&project).1.len(), 1);
    assert_eq!(std::fs::read_to_string(project.dir().join("inbox/broken.md")).unwrap(), "broken inbox");
    assert_eq!(std::fs::read_to_string(project.dir().join("threads/t-0002.toml")).unwrap(), "broken = [");
}

#[test]
#[cfg(target_os = "linux")]
fn resolve_remove_reopen_restart_uses_the_retained_git_branch() {
    use crate::runner::{RealRunner, Runner};
    let world = World::new();
    let project = world.project("demo", "a.sock");
    let repo = world.home.path().join("repo");
    let work = world.home.path().join("work");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = RealRunner.run(&Cmd::new("git", std::time::Duration::from_secs(5)).args(["-C", repo.to_str().unwrap()]).args(args.iter().copied())).unwrap();
        assert!(output.success(), "{}", output.error_text());
        output.stdout
    };
    git(&["init", "--quiet"]);
    git(&["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--quiet", "--allow-empty", "-m", "base"]);
    git(&["worktree", "add", "-b", "retained", work.to_str().unwrap()]);
    std::fs::write(repo.join(".git/info/exclude"), ".herdr-project/\n").unwrap();
    let record = world.thread(&project, &work, |t| { t.repo = repo.to_str().unwrap().into(); t.branch = "retained".into(); });
    std::fs::create_dir_all(&record.thread_dir).unwrap();
    std::fs::write(Path::new(&record.thread_dir).join("report.md"), b"final report").unwrap();
    world.runner.on_fn(|cmd| ["git", "du", "rsync"].contains(&cmd.program.as_str()), |cmd| RealRunner.run(cmd));
    world.runner.on("worktree open", ok(&format!(r#"{{"result":{{"root_pane":{{"workspace_id":"w3","tab_id":"w3:t1","pane_id":"w3:p1","cwd":"{}"}},"worktree":{{"path":"{}"}}}}}}"#, work.display(), work.display())));
    let args = ResolveArgs { remove_worktree: true, writers_stopped: true, ..Default::default() };
    threads::resolve(&world.ctx(), "demo", &record.id, &args).unwrap();
    let removed = thread::load(&project, &record.id).unwrap();
    assert_eq!(removed.status, Status::Resolved);
    assert!(removed.worktree_path.is_empty());
    assert!(!work.exists());
    let snapshot = project.state_dir().join("artifacts").join(&record.id).join(&removed.artifact_snapshot);
    assert_eq!(std::fs::read(snapshot.join("report.md")).unwrap(), b"final report");
    threads::resolve(&world.ctx(), "demo", &record.id, &ResolveArgs { reopen: true, ..Default::default() }).unwrap();
    assert!(!work.exists(), "logical reopen starts nothing");
    let restarted = threads::restart(&world.ctx(), "demo", &record.id).unwrap();
    assert!(work.exists());
    assert_eq!(restarted.branch, "retained");
    assert!(restarted.removal.is_none());
    assert!(restarted.prompt_pending);
    assert_eq!(world.runner.count("worktree create"), 0);
    assert_eq!(world.runner.count("agent start"), 0);
}

#[test]
fn missing_legacy_source_cannot_be_resolved_as_preserved() {
    let (world, project, _work, record) = fixture();
    thread::update(&project, &record.id, |t| {
        t.report_hash = thread::sha256_hex(b"old report");
        t.last_report_change = project::now();
    }).unwrap();
    std::fs::write(thread::home_report_path(&project, &record.id), b"old report").unwrap();
    std::fs::remove_dir_all(&record.thread_dir).unwrap();
    let error = threads::resolve(&world.ctx(), "demo", &record.id, &ResolveArgs::default()).unwrap_err();
    assert!(error.to_string().contains("source is missing"), "{error:#}");
    let saved = thread::load(&project, &record.id).unwrap();
    assert_eq!(saved.status, Status::Open);
    assert!(saved.artifact_snapshot.is_empty());
    assert_eq!(std::fs::read(thread::home_report_path(&project, &record.id)).unwrap(), b"old report");
    threads::resolve(&world.ctx(), "demo", &record.id, &ResolveArgs { skip_copy: true, ..Default::default() }).unwrap();
}
