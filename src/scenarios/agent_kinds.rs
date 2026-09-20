use super::*;
fn config(world:&World,project:&Project,binding:Option<&str>) {
    let dir=world.ctx().config_dir;std::fs::create_dir_all(&dir).unwrap();let bound=binding.map(|k|format!("thread_agent_args_kind = {k:?}\n")).unwrap_or_default();std::fs::write(dir.join("config.toml"),format!("[safety.\"{}\"]\nthread_agent_args = ['--vendor-option']\n{bound}",project.canonical_dir().display())).unwrap();
}
#[test]
fn incompatible_arguments_refuse_before_creating_a_thread_or_resource() {
    let world=World::new();let project=world.project("demo","a.sock");let ctx=world.ctx();for kind in [None,Some("claude")] {config(&world,&project,kind);let before=world.runner.calls.borrow().len();let error=threads::start(&ctx,"demo",StartArgs{title:"mixed".into(),agent:Some("codex".into()),task:"task".into(),repo:None,machine:None,base:None}).unwrap_err();assert!(error.to_string().contains("thread_agent_args"));assert!(thread::list(&project).is_empty());assert_eq!(world.runner.calls.borrow().len(),before);}
}
#[test]
fn kind_mismatch_does_not_consume_launch_attempts_or_starve_matching_workers() {
    let world=World::new();let project=world.project("demo","a.sock");config(&world,&project,Some("claude"));let codex=world.thread(&project,Path::new("/codex"),|t|{t.agent="codex".into();t.prompt_pending=true;});let claude=world.thread(&project,Path::new("/claude"),|t|{t.agent="claude".into();t.prompt_pending=true;t.agent_name="hp-demo-t-0002".into();t.workspace_id="w3".into();t.tab_id="w3:t1".into();t.pane_id="w3:p1".into();});*world.panes.borrow_mut()=format!("[{},{},{}]",world.coordinator_pane(&project),pane_json("w2","w2:t1","w2:p1","/codex"),pane_json("w3","w3:t1","w3:p1","/claude"));world.runner.on("agent start",ok(r#"{"result":{"agent":{"workspace_id":"w3","tab_id":"w3:t1","pane_id":"w3:p1"}}}"#));let ctx=world.ctx();let _=ticker::tick_project(&ctx,&project);assert_eq!(thread::load(&project,&codex.id).unwrap().launch_attempts,0);assert_eq!(thread::load(&project,&claude.id).unwrap().launch_attempts,1);let calls=world.runner.calls.borrow();let starts=calls.iter().filter(|c|c.display().contains("agent start")).collect::<Vec<_>>();assert_eq!(starts.len(),1);assert!(starts[0].args.windows(2).any(|w|w==["--kind","claude"]));assert!(starts[0].args.windows(2).any(|w|w==["--","--vendor-option"]));
}
#[test]
fn unbound_coordinator_arguments_refuse_open_and_do_not_consume_retry_budget() {
    let world=World::new();let project=world.project("demo","a.sock");project.update_coordinator(|c|c.prime_pending=true).unwrap();*world.panes.borrow_mut()=format!("[{}]",world.coordinator_pane(&project));let cfg=world.ctx().config_dir;std::fs::create_dir_all(&cfg).unwrap();std::fs::write(cfg.join("config.toml"),format!("[safety.\"{}\"]\ncoordinator_agent_args=['--vendor-option']\n",project.canonical_dir().display())).unwrap();let before=project.coordinator().unwrap();let ctx=world.ctx();let error=coordinator::open(&ctx,"demo",&coordinator::OpenOptions{session:crate::paths::SessionFlags::default(),reprime:false,rebind:false}).unwrap_err();assert!(error.to_string().contains("coordinator_agent_args_kind"));assert!(world.runner.calls.borrow().is_empty());let _=ticker::tick_project(&ctx,&project);assert_eq!(project.coordinator().unwrap().launch_attempts,before.launch_attempts);assert_eq!(world.runner.count("agent start"),0);
}
#[test]
fn unreadable_safety_content_never_falls_back_to_an_unbound_launch() {
    let world=World::new();let project=world.project("demo","a.sock");let cfg=world.ctx().config_dir;std::fs::create_dir_all(&cfg).unwrap();let path=cfg.join("config.toml");let ctx=world.ctx();
    for mode in 0..3 {
        match mode {0=>std::fs::write(&path,[0xff]).unwrap(),1=>std::fs::create_dir(&path).unwrap(),_=>{let name=std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();assert_eq!(unsafe{libc::mkfifo(name.as_ptr(),0o600)},0);}}
        let start=std::time::Instant::now();assert!(threads::start(&ctx,"demo",StartArgs{title:"task".into(),task:"task".into(),agent:Some("codex".into()),repo:None,machine:None,base:None}).is_err());assert!(start.elapsed()<std::time::Duration::from_secs(1));assert!(thread::list(&project).is_empty());assert!(world.runner.calls.borrow().is_empty());if mode==1 {std::fs::remove_dir(&path).unwrap();}else{std::fs::remove_file(&path).unwrap();}
    }
}
