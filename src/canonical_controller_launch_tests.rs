//! Cross-binary fixture: the library test owns native processes and canonical
//! reservations with internally prepared approvals; this binary drives the real controller queue and executor.
use super::*;
use herdr_projects::operations::DeliveryState;
use std::{fs,path::PathBuf,sync::Arc,time::{Duration,Instant}};

#[test]
fn enabled_dispatch_fixture_driver() {
    let Some(encoded)=std::env::var_os("HP_CONTROLLER_FIXTURE_PROJECTS") else{return;};
    let projects:Vec<PathBuf>=serde_json::from_str(encoded.to_str().unwrap()).unwrap();assert_eq!(projects.len(),3);
    let home=tempfile::tempdir().unwrap();let env=crate::paths::Env::for_test(home.path(),&[]);
    let runner=Arc::new(crate::canonical_brief_jobs::JobRunner{inner:Arc::new(crate::runner::RealRunner)});
    let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),runner).unwrap());
    let mut queue=crate::copy_jobs::Queue::new(pool.clone());let mut errors=Vec::new();
    let deadline=Instant::now()+Duration::from_secs(25);
    // Explicitly excluded launch selection has no effects. Production polling
    // below must select launches without a test-only override.
    for path in &projects {
        let before=runtime::snapshot(path).unwrap();
        let ctx=Ctx{env:&env,root:path.parent().unwrap().into(),config_dir:home.path().join("config"),runner:&crate::runner::RealRunner,detached_ticker:false};
        assert!(!process_next_with_launches(&ctx,path,0,Some(&mut queue),false).unwrap());
        assert_eq!(runtime::snapshot(path).unwrap(),before);
    }
    assert!(!queue.offered());
    // Offer all launches, then revoke the third by cancellation before admission.
    for path in &projects {
        let ctx=Ctx{env:&env,root:path.parent().unwrap().into(),config_dir:home.path().join("config"),runner:&crate::runner::RealRunner,detached_ticker:false};
        process_next(&ctx,path,0,Some(&mut queue)).unwrap();
    }
    assert!(queue.offered());
    let cancel=runtime::snapshot(&projects[2]).unwrap();
    migration::open_active(&projects[2]).unwrap().cancel_attempt(&cancel.attempts[0].id,cancel.attempts[0].revision,cancel.head,"cancel offered launch",jiff::Timestamp::now().as_millisecond()).unwrap();
    let mut first_ready=None;let mut cancellation_sent=false;let mut observed_other_before_retry=false;
    for turn in 0..5000 {
        assert!(Instant::now()<deadline,"enabled dispatch timed out: {errors:?}");
        errors.extend(queue.drain());
        let states=projects.iter().map(|p|runtime::snapshot(p).unwrap()).collect::<Vec<_>>();
        let started=|s:&herdr_projects::domain::Snapshot|s.events.iter().any(|e|e.kind=="runtime.launch_started");
        if started(&states[1])&&!started(&states[0]) {observed_other_before_retry=true;}
        let ready=states[..2].iter().all(|s|s.deliveries.iter().any(|d|d.state==DeliveryState::Confirmed&&s.operations.iter().any(|o|o.id==d.operation&&o.kind=="runtime.worker_brief")));
        if ready&&!cancellation_sent&&!queue.pending() {
            first_ready=Some(states.clone());
            // Discard volatile offers as a controller restart would, then recover
            // cancellation solely from durable records through a fresh queue.
            queue=crate::copy_jobs::Queue::new(pool.clone());
            for (path,state) in projects[..2].iter().zip(&states) {
                let record=&state.attempt_inputs[0];let output=herdr_projects::domain::worker_output_path(&record.inputs,&record.attempt).unwrap();
                fs::create_dir_all(Path::new(&output).join("library")).unwrap();fs::write(Path::new(&output).join("report.md"),b"controller fixture report").unwrap();fs::write(Path::new(&output).join("library/result"),[0,255,7]).unwrap();
                migration::open_active(path).unwrap().cancel_attempt(&state.attempts[0].id,state.attempts[0].revision,state.head,"stop controller fixture",jiff::Timestamp::now().as_millisecond()).unwrap();
            }
            cancellation_sent=true;
        }
        if cancellation_sent&&states.iter().all(|s|s.attempts[0].termination_observed)&&!queue.pending() {break;}
        for path in &projects {
            let ctx=Ctx{env:&env,root:path.parent().unwrap().into(),config_dir:home.path().join("config"),runner:&crate::runner::RealRunner,detached_ticker:false};
            process_next(&ctx,path,turn,Some(&mut queue)).unwrap();
        }
        errors.extend(queue.admit());std::thread::sleep(Duration::from_millis(5));
    }
    assert!(first_ready.is_some(),"brief delivery never completed: {errors:?}");
    assert!(observed_other_before_retry,"lost acknowledgment monopolized controller admission: {errors:?}");
    for path in &projects[..2] {
        let state=runtime::snapshot(path).unwrap();assert!(state.attempts[0].termination_observed&&!state.attempts[0].retains_capacity());
        assert_eq!(state.tasks[0].state,herdr_projects::domain::TaskState::Cancelled);
        let stop:herdr_projects::domain::WorkerTerminationReceipt=serde_json::from_value(state.events.iter().find(|e|e.kind=="runtime.worker_terminated").unwrap().payload.clone()).unwrap();
        assert!(stop.output_snapshot.as_ref().unwrap().digest.is_some());
        assert_eq!(stop.repository_snapshots.len(),state.attempt_inputs[0].inputs.repositories.len());
        let binding=state.runtime_bindings.iter().find(|b|b.id==stop.binding).unwrap();
        let saved=herdr_projects::worktree_preservation::load_binding_outputs(path,&state,binding,&herdr_projects::source_tree::Control::default()).unwrap().unwrap();
        for (name,bytes) in [("report.md",b"controller fixture report".as_slice()),("library/result",&[0,255,7])] {
            let entry=saved.manifest().entries.iter().find(|e|e.path==name).unwrap();assert_eq!(saved.bytes(entry).unwrap(),bytes);
        }
        for kind in ["runtime.launch_started","runtime.worker_terminated"] {assert_eq!(state.events.iter().filter(|e|e.kind==kind).count(),1);}
    }
    let cancelled=runtime::snapshot(&projects[2]).unwrap();assert!(cancelled.attempts[0].termination_observed);
    assert!(cancelled.approvals.iter().all(|a|a.consumed.is_none()));
    assert!(!cancelled.events.iter().any(|e|e.kind.starts_with("runtime.launch_")));
    assert!(pool.stop(Duration::from_secs(2)));assert!(pool.metrics().high_water[0]<=2);
    assert!(!errors.is_empty(),"fixture must exercise a lost acknowledgment and stale queued launch");
}

#[test]
fn enabled_ticker_fixture_driver() {
    use std::os::unix::fs::PermissionsExt;
    let Some(encoded)=std::env::var_os("HP_CONTROLLER_FIXTURE_PROJECTS") else{return;};
    let projects:Vec<PathBuf>=serde_json::from_str(encoded.to_str().unwrap()).unwrap();assert_eq!(projects.len(),3);
    let root=projects[0].parent().unwrap();assert!(projects.iter().all(|p|p.parent()==Some(root)));
    let config=root.join(".fixture-config");fs::create_dir(&config).unwrap();
    let mut mapping=serde_json::Map::new();
    for path in &projects {
        let state=runtime::snapshot(path).unwrap();let inputs=&state.attempt_inputs[0].inputs;
        let bytes=fs::read(&inputs.config.path).unwrap();
        if config.join("config.toml").exists(){assert_eq!(fs::read(config.join("config.toml")).unwrap(),bytes);}else{fs::write(config.join("config.toml"),bytes).unwrap();}
        mapping.insert(state.runtime_bindings[0].identity.socket.clone(),serde_json::json!(Path::new(&inputs.effective_profile.as_ref().unwrap().herdr.path).parent().unwrap()));
    }
    let router=root.join(".fixture-herdr");
    fs::write(&router,format!(r#"#!/usr/bin/python3
import sys,json,pathlib,os
if sys.argv[1:]==['--version']:print('herdr 0.9.1');sys.exit(0)
mapping=json.loads({mapping:?})
socket=os.environ['HERDR_SOCKET_PATH']
root=pathlib.Path(mapping[socket])
a=json.loads((root/'agent.json').read_text())
a['name']=a.get('name') or ''
rows=[a] if a.get('pane_id') else []
with open({log:?},'a') as f:f.write(socket+'\n')
if sys.argv[-2:]==['pane','list']:result={{'panes':rows}}
elif sys.argv[-2:]==['agent','list']:result={{'agents':rows}}
else:sys.exit(2)
print(json.dumps({{'result':result}}))
"#,mapping=serde_json::to_string(&mapping).unwrap(),log=root.join(".maintenance-probes").display().to_string())).unwrap();
    fs::set_permissions(&router,fs::Permissions::from_mode(0o700)).unwrap();
    let env=crate::paths::Env::for_test(root,&[("HERDR_BIN_PATH",router.to_str().unwrap())]);
    let ctx=Ctx{env:&env,root:root.into(),config_dir:config,runner:&crate::runner::RealRunner,detached_ticker:false};
    let mut memory=crate::ticker::background_memory(&ctx).unwrap();
    // Reproduce cancellation after volatile offer, before the ticker admits it.
    for path in &projects {process_next(&ctx,path,0,memory.copy_jobs.as_mut()).unwrap();}
    let cancelled=runtime::snapshot(&projects[2]).unwrap();migration::open_active(&projects[2]).unwrap().cancel_attempt(&cancelled.attempts[0].id,cancelled.attempts[0].revision,cancelled.head,"cancel ticker offer",jiff::Timestamp::now().as_millisecond()).unwrap();
    let deadline=Instant::now()+Duration::from_secs(25);let mut stopped=false;let mut live_tick=false;
    {
        loop {
            let logfile=std::env::temp_dir().join(format!("hp-test-log-{}",std::process::id()));
            assert!(Instant::now()<deadline,"ticker acceptance timed out: {}",fs::read_to_string(logfile).unwrap_or_default());
            live_tick|=crate::ticker::tick_for_test(&ctx,&mut memory);
            let states=projects.iter().map(|p|runtime::snapshot(p).unwrap()).collect::<Vec<_>>();
            let pending=memory.copy_jobs.as_ref().unwrap().pending();
            let briefs=states[..2].iter().all(|s|s.deliveries.iter().any(|d|d.state==DeliveryState::Confirmed&&s.operations.iter().any(|o|o.id==d.operation&&o.kind=="runtime.worker_brief")));
            let observed=states.iter().enumerate().all(|(n,s)|s.observations.iter().any(|o|o.collector=="herdr-git-v2"&&(n==2||o.pane==herdr_projects::reconcile::ResourceState::Present)));
            if briefs&&observed&&!stopped&&!pending {
                // Drain the entire production executor and restart ticker memory.
                memory.pr_reads.as_mut().unwrap().stop().unwrap();
                memory=crate::ticker::background_memory(&ctx).unwrap();
                for path in &projects[..2] {
                    let state=runtime::snapshot(path).unwrap();let record=&state.attempt_inputs[0];
                    let source=herdr_projects::domain::worker_output_path(&record.inputs,&record.attempt).unwrap();fs::create_dir_all(&source).unwrap();fs::write(Path::new(&source).join("report.md"),b"ticker result").unwrap();
                    migration::open_active(path).unwrap().cancel_attempt(&record.attempt,state.attempts[0].revision,state.head,"ticker restart cancellation",jiff::Timestamp::now().as_millisecond()).unwrap();
                }
                stopped=true;
                continue; // A restarted daemon ticks immediately before its first sleep.
            }
            if stopped&&states.iter().all(|s|s.attempts[0].termination_observed)&&!pending {break;}
            std::thread::sleep(crate::ticker::next_tick_delay(&memory));
        }
    }
    assert!(live_tick&&stopped);memory.pr_reads.as_mut().unwrap().stop().unwrap();
    assert!(root.join(".maintenance-probes").is_file());
    for path in &projects[..2] {
        let state=runtime::snapshot(path).unwrap();assert!(!state.attempts[0].retains_capacity());
        assert_eq!(state.tasks[0].state,herdr_projects::domain::TaskState::Cancelled);
        let outputs=herdr_projects::worktree_preservation::load_binding_outputs(path,&state,&state.runtime_bindings[0],&herdr_projects::source_tree::Control::default()).unwrap().unwrap();
        let report=outputs.manifest().entries.iter().find(|e|e.path=="report.md").unwrap();assert_eq!(outputs.bytes(report).unwrap(),b"ticker result");
    }
    let state=runtime::snapshot(&projects[2]).unwrap();assert!(state.approvals.iter().all(|a|a.consumed.is_none()));
    assert!(!state.events.iter().any(|e|e.kind.starts_with("runtime.launch_")));
}

/// Invoked only by the explicitly authorized library live fixture. This driver
/// never writes worker outputs; positive output evidence must come from the agent.
#[test]
fn live_dispatch_workflow_driver() {
    let Some(project)=std::env::var_os("HP_LIVE_DISPATCH_PROJECT") else{return;};
    let project=PathBuf::from(project).canonicalize().unwrap();
    let initial=runtime::snapshot(&project).unwrap();assert_eq!(initial.attempts.len(),1);
    let input=&initial.attempt_inputs[0];let profile=input.inputs.effective_profile.as_ref().unwrap();
    assert_eq!(profile.kind,"codex");
    let output=PathBuf::from(herdr_projects::domain::worker_output_path(&input.inputs,&input.attempt).unwrap());
    assert!(!output.exists(),"outputs must not be supplied by the fixture");
    let home=tempfile::tempdir().unwrap();
    let env=crate::paths::Env::for_test(home.path(),&[("HERDR_BIN_PATH",&profile.herdr.path)]);
    let ctx=Ctx{env:&env,root:project.parent().unwrap().into(),config_dir:Path::new(&input.inputs.config.path).parent().unwrap().into(),runner:&crate::runner::RealRunner,detached_ticker:false};
    let mut memory=crate::ticker::background_memory(&ctx).unwrap();
    let deadline=Instant::now()+Duration::from_secs(180);
    let mut restarted=false;let mut cancelled=false;
    {
        loop {
            assert!(Instant::now()<deadline,"live workflow exceeded its 180-second bound (terminal output withheld)");
            crate::ticker::tick_for_test(&ctx,&mut memory);
            let state=runtime::snapshot(&project).unwrap();
            let pending=memory.copy_jobs.as_ref().unwrap().pending();
            let brief_confirmed=state.deliveries.iter().any(|d|d.state==DeliveryState::Confirmed&&state.operations.iter().any(|o|o.id==d.operation&&o.kind=="runtime.worker_brief"));
            if brief_confirmed&&!restarted&&!pending {
                memory.pr_reads.as_mut().unwrap().stop().unwrap();
                memory=crate::ticker::background_memory(&ctx).unwrap();
                restarted=true;
                continue;
            }
            if restarted&&!cancelled&&!pending
                &&fs::read(output.join("report.md")).is_ok_and(|b|b==b"CANONICAL_RETAINED_MEMORY_OK\n")
                &&fs::read(output.join("library/result.txt")).is_ok_and(|b|b==b"CANONICAL_WORKER_RESULT_OK\n") {
                migration::open_active(&project).unwrap().cancel_attempt(&state.attempts[0].id,state.attempts[0].revision,state.head,"live acceptance outputs observed",jiff::Timestamp::now().as_millisecond()).unwrap();
                cancelled=true;
            }
            if cancelled&&state.attempts[0].termination_observed&&!pending {break;}
            std::thread::sleep(crate::ticker::next_tick_delay(&memory));
        }
    }
    memory.pr_reads.as_mut().unwrap().stop().unwrap();
    assert!(restarted&&cancelled);
    let state=runtime::snapshot(&project).unwrap();
    assert!(!state.attempts[0].retains_capacity());
    assert_eq!(state.tasks[0].state,herdr_projects::domain::TaskState::Cancelled);
    for kind in ["runtime.launch_creation","runtime.launch_release","runtime.launch_started","runtime.worker_terminated"] {
        assert_eq!(state.events.iter().filter(|e|e.kind==kind).count(),1,"{kind}");
    }
    let brief=state.operations.iter().find(|o|o.kind=="runtime.worker_brief").unwrap();
    assert_eq!(state.deliveries.iter().find(|d|d.operation==brief.id).unwrap().attempts,1);
    let stop:herdr_projects::domain::WorkerTerminationReceipt=serde_json::from_value(state.events.iter().find(|e|e.kind=="runtime.worker_terminated").unwrap().payload.clone()).unwrap();
    assert!(stop.output_snapshot.as_ref().unwrap().digest.is_some());
    assert!(herdr_projects::worker_supervision::SupervisorObservation::recover_exited(&stop.supervisor).unwrap());
    // Delete only the disposable output source, then prove receipt-bound recovery.
    assert!(output.starts_with(project.join(".state/worker-output")));fs::remove_dir_all(&output).unwrap();
    let binding=state.runtime_bindings.iter().find(|b|b.id==stop.binding).unwrap();
    let saved=herdr_projects::worktree_preservation::load_binding_outputs(&project,&state,binding,&herdr_projects::source_tree::Control::default()).unwrap().unwrap();
    for (name,bytes) in [("report.md",b"CANONICAL_RETAINED_MEMORY_OK\n".as_slice()),("library/result.txt",b"CANONICAL_WORKER_RESULT_OK\n".as_slice())] {
        let entry=saved.manifest().entries.iter().find(|e|e.path==name).unwrap();assert_eq!(saved.bytes(entry).unwrap(),bytes);
    }
    eprintln!("Live controller launch, retained instructions, agent-written outputs, controller restart and receipt-bound preservation passed; memory-update and mixed-agent certification remain separate");
}
