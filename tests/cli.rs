//! End-to-end checks of the built binary with a scrubbed environment.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

#[cfg(all(feature="state-store",target_os="linux"))]
#[test]
fn launch_worker_snapshot_cli_retains_instructions_and_refuses_missing_source() {
    use herdr_projects::{domain::TaskId,migration,runtime};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
    for action in ["new","pause"] {assert!(hp(home.path(),&["--root",r,action,"demo"]).status.success());}
    let config=home.path().join(".config/herdr-projects");std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.toml"),"[profiles.worker]\nkind='codex'\npermission_policy='interactive'\n").unwrap();
    let project=root.join("demo");let plan=migration::inspect(&project).unwrap();migration::apply(&project,&plan,true).unwrap();
    runtime::add_task(&project,TaskId::new("task").unwrap(),"Assigned work".into(),runtime::snapshot(&project).unwrap().head).unwrap();
    let scope=home.path().join("scope.json");std::fs::write(&scope,r#"{"schema_version":1,"task_id":"task","profile":"worker","domains":[],"paths":[],"pinned_keys":[],"sensitivity":"default"}"#).unwrap();
    std::fs::write(project.join("PROJECT.md"),"Retained instructions 🔥").unwrap();
    let args=["--root",r,"memory","demo","snapshot","--task","task","--profile","worker","--input-file",scope.to_str().unwrap(),"--worker"];
    let out=hp(home.path(),&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let snapshot:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(snapshot["estimator"],"char-count-worker-brief-v2");
    std::fs::remove_file(project.join("PROJECT.md")).unwrap();
    let retained=herdr_projects::memory::render_knowledge_snapshot(&project,snapshot["id"].as_str().unwrap()).unwrap();
    assert!(retained["text"].as_str().unwrap().contains("Retained instructions 🔥"));
    let before=runtime::snapshot(&project).unwrap();let out=hp(home.path(),&args);assert!(!out.status.success());assert_eq!(runtime::snapshot(&project).unwrap(),before);
    std::fs::write(project.join("PROJECT.md"),[0xff]).unwrap();let out=hp(home.path(),&args);assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("project instructions are not UTF-8"));assert_eq!(runtime::snapshot(&project).unwrap(),before);
    for command in ["draft","reserve"] {let out=hp(home.path(),&["launch","demo",command,"--help"]);assert!(out.status.success());assert!(String::from_utf8_lossy(&out.stdout).contains("--selection"));}
}

#[cfg(all(feature="state-store",target_os="linux"))]
#[test]
fn launch_exec_consumes_one_private_digest_bound_spec_and_execs_the_supervisor() {
    use sha2::{Digest,Sha256};use std::{fs,os::unix::fs::{DirBuilderExt,PermissionsExt}};
    let home=tempfile::tempdir().unwrap();let base=home.path().canonicalize().unwrap();
    let dir=base.join("launch-specs");fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let marker=base.join("started");
    let argv=herdr_projects::worker_supervision::command(Path::new("/usr/bin/touch"),&[marker.display().to_string()],5).unwrap();
    let digest=format!("{:x}",Sha256::digest(serde_json::to_vec(&argv).unwrap()));
    let body=serde_json::to_vec(&serde_json::json!({"version":1,"operation":"cli","cwd":base,"command_digest":digest,"argv":argv})).unwrap();
    let spec=dir.join(format!("{digest}.spec"));let other=dir.join(format!("{}.spec","0".repeat(64)));
    let run=|path:&Path|Command::new(BIN).env_clear().env("PATH","/usr/bin:/bin").arg("launch-exec").arg(path).output().unwrap();
    // A spec bound to another digest, or readable by others, is refused and kept.
    for (path,mode) in [(&other,0o600),(&spec,0o644)] {
        fs::write(path,&body).unwrap();fs::set_permissions(path,fs::Permissions::from_mode(mode)).unwrap();
        let out=run(path);assert!(!out.status.success());assert!(path.exists()&&!marker.exists());fs::remove_file(path).unwrap();
    }
    fs::write(&spec,&body).unwrap();fs::set_permissions(&spec,fs::Permissions::from_mode(0o600)).unwrap();
    let out=run(&spec);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    assert!(marker.exists());assert!(!spec.exists());
    assert!(!run(&spec).status.success(),"a consumed spec must not launch again");
}

#[cfg(all(feature="state-store",target_os="linux"))]
#[test]
fn ticker_canonical_observations_commit_cancel_and_restart_in_the_shared_pool() {
    use std::{fs,os::unix::{fs::PermissionsExt,net::UnixListener},process::Stdio,time::{Duration,Instant}};
    use herdr_projects::{migration,runtime,domain::RuntimeRoute,reconcile::ResourceState};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
    for action in ["new","pause"]{assert!(hp(home.path(),&["--root",r,action,"demo"]).status.success());}
    let project=root.join("demo");let plan=migration::inspect(&project).unwrap();migration::apply(&project,&plan,true).unwrap();
    let socket=home.path().join("canonical.sock");let _listener=UnixListener::bind(&socket).unwrap();
    runtime::create_binding(&project,None,None,runtime::snapshot(&project).unwrap().head,&RuntimeRoute{socket:socket.display().to_string(),workspace_id:"w".into(),tab_id:"t".into(),pane_id:"p".into(),cwd:"/fixture".into(),..Default::default()}).unwrap();
    let helper=home.path().join("herdr");fs::write(home.path().join("mode"),"ok").unwrap();
    fs::write(&helper,format!(r#"#!/usr/bin/python3
import json,pathlib,sys,time
root=pathlib.Path({home:?})
if sys.argv[1:]==['--version']:print('herdr 0.9.1');sys.exit(0)
(root/'entered').write_text('yes')
if (root/'mode').read_text()=='blocked':time.sleep(60)
p={{'pane_id':'p','workspace_id':'w','tab_id':'t','cwd':'/fixture','agent':'claude','name':'fixture','agent_status':'idle'}}
if sys.argv[1:]==['pane','list']:r={{'panes':[p]}}
elif sys.argv[1:]==['agent','list']:r={{'agents':[p]}}
else:sys.exit(3)
print(json.dumps({{'result':r}}))
"#,home=home.path().display().to_string())).unwrap();fs::set_permissions(&helper,fs::Permissions::from_mode(0o700)).unwrap();
    struct Child(std::process::Child);impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
    let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&helper).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let wait=|child:&mut Child,predicate:&dyn Fn()->bool|{let end=Instant::now()+Duration::from_secs(8);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
    let stop=|child:&mut Child|{fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
    let mut child=spawn();wait(&mut child,&||runtime::snapshot(&project).unwrap().observations.iter().any(|o|o.pane==ResourceState::Present));stop(&mut child);
    let before=runtime::snapshot(&project).unwrap();fs::remove_file(home.path().join("entered")).unwrap();fs::write(home.path().join("mode"),"blocked").unwrap();
    let mut child=spawn();wait(&mut child,&||home.path().join("entered").exists());
    assert!(herdr_projects::execution_guard::ProjectGuard::acquire(&project).is_err());stop(&mut child);assert_eq!(runtime::snapshot(&project).unwrap(),before);
    fs::write(home.path().join("mode"),"ok").unwrap();let mut child=spawn();wait(&mut child,&||runtime::snapshot(&project).unwrap().head>before.head);stop(&mut child);
    assert!(herdr_projects::execution_guard::ProjectGuard::acquire(&project).is_ok());
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
    let observed=||raw.query_row("SELECT coalesce(max(observed_unix_ms),0) FROM runtime_observations",[],|row|row.get::<_,i64>(0)).unwrap();
    let previous=observed();
    raw.execute("INSERT INTO tasks VALUES('retired/invalid',1,'succeeded','unrelated historical task',NULL)",[]).unwrap();
    assert!(runtime::snapshot(&project).is_err());
    let mut child=spawn();wait(&mut child,&||observed()>previous);stop(&mut child);
    assert!(runtime::snapshot(&project).is_err(),"background progress must not erase corrupt history");
    raw.execute("DELETE FROM tasks WHERE id='retired/invalid'",[]).unwrap();
    let after=runtime::snapshot(&project).unwrap();assert_eq!(after.tasks,before.tasks);assert_eq!(after.attempts,before.attempts);

}

#[cfg(feature="state-store")]
#[test]
fn signed_routine_cli_records_then_explicitly_executes_once_with_durable_cleanup() {
    use herdr_projects::{domain::*,authority,migration,runtime};
    use sha2::{Digest,Sha256};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for action in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,action,"demo"]).status.success());}
    let key=home.path().join("owner");let output=Command::new("/usr/bin/ssh-keygen").args(["-q","-t","ed25519","-N","","-f"]).arg(&key).output().unwrap();assert!(output.status.success());
    let public=std::fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    let project=root.join("demo");let config=home.path().join("owner.toml");
    std::fs::write(&config,format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n[safety.{:?}]\nroutine_commands=true\n",project.display().to_string())).unwrap();
    let plan=migration::inspect_with_config(&project,&config).unwrap();migration::apply(&project,&plan,true).unwrap();
    let s=runtime::snapshot(&project).unwrap();runtime::set_state(&project,s.head,s.control.unwrap().revision,ProjectState::Active,&config).unwrap();
    let script=project.join("check.sh");let bytes=b"printf once >> ROUTINE_MARKER\nprintf 'result\\033[31m'\n";std::fs::write(&script,bytes).unwrap();
    let definition=RoutineDefinition{version:1,name:"check".into(),revision:1,project_store:project.join(".state/state.db").canonicalize().unwrap().display().to_string(),
        authority:authority::policy_reference(&project).unwrap(),config:migration::config_reference(&config).unwrap(),enabled:true,schedule:"every 1m".into(),timezone:"UTC".into(),start_unix_ms:0,
        missed:MissedRunPolicy::CoalesceLatest,overlap:OverlapPolicy::Skip,script:script.display().to_string(),script_sha256:format!("{:x}",Sha256::digest(bytes)),cwd:project.display().to_string(),deadline_ms:1000,output_cap_bytes:4000};
    let document=home.path().join("routine.json");std::fs::write(&document,serde_json::to_vec(&definition).unwrap()).unwrap();
    let output=Command::new("/usr/bin/ssh-keygen").args(["-Y","sign","-f"]).arg(&key).args(["-n",authority::ROUTINE_SIGNATURE_NAMESPACE]).arg(&document).output().unwrap();assert!(output.status.success());
    let signature=home.path().join("routine.json.sig");let head=runtime::snapshot(&project).unwrap().head.to_string();
    let output=hp(home.path(),&["--root",root_arg,"routine-store","demo","import",document.to_str().unwrap(),signature.to_str().unwrap(),"--expected-head",&head]);
    assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let head=runtime::snapshot(&project).unwrap().head.to_string();let args=["--root",root_arg,"routine-store","demo","schedule","check","--expected-head",&head];
    let output=hp(home.path(),&args);assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let after=runtime::snapshot(&project).unwrap();assert_eq!(after.routine_occurrences.len(),1);assert_eq!(after.operations.len(),1);assert!(after.operations[0].task.is_none());
    assert!(!hp(home.path(),&args).status.success());assert_eq!(runtime::snapshot(&project).unwrap(),after);
    assert!(!project.join("ROUTINE_MARKER").exists());
    #[cfg(target_os="linux")]
    {
        let operation=after.operations[0].id.as_str();let head=after.head.to_string();
        let args=["--root",root_arg,"routine-store","demo","execute",operation,"--expected-head",&head];
        std::fs::write(&script,b"touch WRONG_SCRIPT").unwrap();
        assert!(!hp(home.path(),&args).status.success());assert_eq!(runtime::snapshot(&project).unwrap(),after);
        assert!(!project.join("WRONG_SCRIPT").exists());std::fs::write(&script,bytes).unwrap();
        let output=hp(home.path(),&args);assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
        let receipt:RoutineReceipt=serde_json::from_slice(&output.stdout).unwrap();assert!(receipt.cleanup_verified && receipt.succeeded,"{receipt:?}");
        assert_eq!(receipt.stdout,b"result\x1b[31m");
        let completed=runtime::snapshot(&project).unwrap();assert_eq!(completed.routine_receipts,vec![receipt]);
        assert_eq!(completed.deliveries[0].state,herdr_projects::operations::DeliveryState::Confirmed);
        let result=completed.inbox.iter().find(|i|i.content.kind=="routine-result").unwrap();assert!(!result.content.body.contains('\x1b'));
        let current=completed.head.to_string();
        assert!(!hp(home.path(),&["--root",root_arg,"routine-store","demo","execute",operation,"--expected-head",&current]).status.success());
        assert_eq!(runtime::snapshot(&project).unwrap(),completed);
        assert_eq!(std::fs::read(project.join("ROUTINE_MARKER")).unwrap(),b"once");
    }
}

/// Retire 10,000 unrelated tasks/operations and add one undecodable cold row of
/// each kind. The event head is unchanged, and any whole-history read now fails.
#[cfg(feature="state-store")]
fn retire_history(project:&Path) {
    use sha2::{Digest,Sha256};
    let mut raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
    let tx=raw.transaction().unwrap();let hash=format!("{:x}",Sha256::digest(b"{}"));
    for n in 0..10_000 {
        tx.execute("INSERT INTO tasks(id,revision,state,title,active_attempt) VALUES(?1,1,'succeeded','retired',NULL)",[format!("retired-{n}")]).unwrap();
        tx.execute("INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key) VALUES(?1,?1,'runtime.retired','retired',1,'{}',?2,1,0,?1)",[format!("retired-{n}"),hash.clone()]).unwrap();
    }
    tx.execute_batch("UPDATE operation_delivery SET state='confirmed' WHERE operation_id LIKE 'retired-%';
        INSERT INTO tasks(id,revision,state,title,active_attempt) VALUES('cold-history',1,'succeeded',CAST(x'ff' AS TEXT),NULL);
        INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key) VALUES('cold-op','cold-history','runtime.retired','retired',1,'{}',printf('%064d',0),1,0,'cold-op');
        UPDATE operation_delivery SET state='confirmed' WHERE operation_id='cold-op';").unwrap();
    tx.commit().unwrap();
    assert!(herdr_projects::runtime::snapshot(project).is_err(),"cold history must make whole-history reads fail");
}

#[cfg(all(feature="state-store",target_os="linux"))]
#[test]
fn effect_commands_read_only_their_rows_with_ten_thousand_retired_neighbors() {
    use herdr_projects::{domain::*,authority,migration,runtime};
    use sha2::{Digest,Sha256};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
    for project in ["demo","fin"] {for action in ["new","pause"] {assert!(hp(home.path(),&["--root",r,action,project]).status.success());}}
    let head_of=|project:&Path|rusqlite::Connection::open(project.join(".state/state.db")).unwrap().query_row("SELECT MAX(sequence) FROM events",[],|row|row.get::<_,u64>(0)).unwrap();
    let delivery_of=|project:&Path,id:&str|rusqlite::Connection::open(project.join(".state/state.db")).unwrap().query_row("SELECT state,revision FROM operation_delivery WHERE operation_id=?1",[id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,u64>(1)?))).unwrap();

    // Finalization: an ambiguous capture whose receipt is still unverified.
    let fin=root.join("fin");let source=home.path().join("source");std::fs::create_dir_all(source.join("library")).unwrap();std::fs::write(source.join("report.md"),"report\n").unwrap();std::fs::write(source.join("library/result"),"bytes").unwrap();
    std::fs::write(fin.join("threads/t-0001.toml"),format!("id='t-0001'\nstatus='resolved'\nthread_dir={}\n",serde_json::to_string(source.to_str().unwrap()).unwrap())).unwrap();
    let plan=migration::inspect(&fin).unwrap();migration::apply(&fin,&plan,true).unwrap();
    let out=hp(home.path(),&["--root",r,"operations","fin","finalize","thread:t-0001","--reason","review","--expected-head",&head_of(&fin).to_string()]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let op:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();let fin_op=op["id"].as_str().unwrap().to_owned();
    std::fs::write(source.join("report.md"),"changed\n").unwrap();
    let out=hp(home.path(),&["--root",r,"operations","fin","deliver-finalization",&fin_op,"--expected-revision","1"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let (state,revision)=delivery_of(&fin,&fin_op);assert_eq!(state,"ambiguous");
    let observe=|head:u64|{let started=std::time::Instant::now();let out=hp(home.path(),&["--root",r,"operations","fin","observe-finalization",&fin_op,"--expected-revision",&revision.to_string(),"--expected-head",&head.to_string()]);(out,started.elapsed())};
    let (empty,empty_time)=observe(head_of(&fin));assert!(!empty.status.success());
    let unresolved=String::from_utf8_lossy(&empty.stderr).into_owned();assert!(unresolved.contains("no verified finalization receipt"),"{unresolved}");

    // Routine: one scheduled, enabled occurrence awaiting explicit execution.
    let key=home.path().join("owner");assert!(Command::new("/usr/bin/ssh-keygen").args(["-q","-t","ed25519","-N","","-f"]).arg(&key).output().unwrap().status.success());
    let public=std::fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    let project=root.join("demo");let config=home.path().join("owner.toml");
    std::fs::write(&config,format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n[safety.{:?}]\nroutine_commands=true\n",project.display().to_string())).unwrap();
    let plan=migration::inspect_with_config(&project,&config).unwrap();migration::apply(&project,&plan,true).unwrap();
    let s=runtime::snapshot(&project).unwrap();runtime::set_state(&project,s.head,s.control.unwrap().revision,ProjectState::Active,&config).unwrap();
    runtime::add_task(&project,TaskId::new("live").unwrap(),"live".into(),head_of(&project)).unwrap();
    let script=project.join("check.sh");let bytes=b"printf once >> ROUTINE_MARKER\n";std::fs::write(&script,bytes).unwrap();
    let definition=RoutineDefinition{version:1,name:"check".into(),revision:1,project_store:project.join(".state/state.db").canonicalize().unwrap().display().to_string(),
        authority:authority::policy_reference(&project).unwrap(),config:migration::config_reference(&config).unwrap(),enabled:true,schedule:"every 1m".into(),timezone:"UTC".into(),start_unix_ms:0,
        missed:MissedRunPolicy::CoalesceLatest,overlap:OverlapPolicy::Skip,script:script.display().to_string(),script_sha256:format!("{:x}",Sha256::digest(bytes)),cwd:project.display().to_string(),deadline_ms:1000,output_cap_bytes:4000};
    let document=home.path().join("routine.json");std::fs::write(&document,serde_json::to_vec(&definition).unwrap()).unwrap();
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y","sign","-f"]).arg(&key).args(["-n",authority::ROUTINE_SIGNATURE_NAMESPACE]).arg(&document).output().unwrap().status.success());
    let signature=home.path().join("routine.json.sig");
    let out=hp(home.path(),&["--root",r,"routine-store","demo","import",document.to_str().unwrap(),signature.to_str().unwrap(),"--expected-head",&head_of(&project).to_string()]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let out=hp(home.path(),&["--root",r,"routine-store","demo","schedule","check","--expected-head",&head_of(&project).to_string()]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let routine_op=runtime::snapshot(&project).unwrap().operations[0].id.as_str().to_owned();

    // Retain 10,000 unrelated rows plus undecodable cold rows in both stores.
    retire_history(&fin);retire_history(&project);
    let (fin_head,head)=(head_of(&fin),head_of(&project));

    let (retained,retained_time)=observe(fin_head);assert!(!retained.status.success());
    let stderr=String::from_utf8_lossy(&retained.stderr);assert!(stderr.contains("no verified finalization receipt"),"observation scanned retained history: {stderr}");
    assert_eq!(delivery_of(&fin,&fin_op),(state.clone(),revision));assert_eq!(head_of(&fin),fin_head);
    let stale=observe(fin_head-1).0;assert!(!stale.status.success()&&!String::from_utf8_lossy(&stale.stderr).contains("no verified finalization receipt"),"a stale head must be refused before the receipt check");
    println!("finalization observe wall time: empty history={empty_time:?}, 10,000 retained={retained_time:?}");

    // A stale head is refused before any claim; the current head executes once.
    let stale=hp(home.path(),&["--root",r,"routine-store","demo","execute",&routine_op,"--expected-head",&(head-1).to_string()]);assert!(!stale.status.success());
    assert_eq!(delivery_of(&project,&routine_op),("pending".into(),1));assert!(!project.join("ROUTINE_MARKER").exists());
    let out=hp(home.path(),&["--root",r,"routine-store","demo","execute",&routine_op,"--expected-head",&head.to_string()]);
    assert!(out.status.success(),"routine execution scanned retained history: {}",String::from_utf8_lossy(&out.stderr));
    let receipt:RoutineReceipt=serde_json::from_slice(&out.stdout).unwrap();assert!(receipt.succeeded&&receipt.cleanup_verified,"{receipt:?}");
    assert_eq!(delivery_of(&project,&routine_op).0,"confirmed");assert_eq!(std::fs::read(project.join("ROUTINE_MARKER")).unwrap(),b"once");
    let replay=hp(home.path(),&["--root",r,"routine-store","demo","execute",&routine_op,"--expected-head",&head_of(&project).to_string()]);assert!(!replay.status.success());
    assert_eq!(std::fs::read(project.join("ROUTINE_MARKER")).unwrap(),b"once");

    // Operator task rename reads one task and the head only; fences still hold.
    let head=head_of(&project);let out=hp(home.path(),&["--root",r,"task","demo","rename","live","--title","renamed","--expected-revision","1","--expected-head",&head.to_string()]);
    assert!(out.status.success(),"task rename scanned retained history: {}",String::from_utf8_lossy(&out.stderr));assert_eq!(head_of(&project),head+1);
    assert!(!hp(home.path(),&["--root",r,"task","demo","rename","live","--title","again","--expected-revision","1","--expected-head",&head_of(&project).to_string()]).status.success(),"a stale task revision must still be refused");
    assert!(!hp(home.path(),&["--root",r,"task","demo","rename","live","--title","again","--expected-revision","2","--expected-head",&head.to_string()]).status.success(),"a stale head must still be refused");
}

#[cfg(feature="state-store")]
#[test]
fn approval_cli_uses_pinned_policy_and_refuses_unsigned_import() {
    use herdr_projects::{authority, migration, runtime};
    let home=tempfile::tempdir().unwrap();
    let caller=tempfile::tempdir().unwrap();
    let root=home.path().join("root");
    let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {
        assert!(hp(home.path(), &["--root",root_arg,command,"demo"]).status.success());
    }
    let config=home.path().join("owner.toml");
    std::fs::write(&config, "[authority]\nversion=1\nrevision=7\napproval_public_key='ssh-ed25519 AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'\n").unwrap();
    let project=root.join("demo");
    let plan=migration::inspect_with_config(&project,&config).unwrap();
    migration::apply(&project,&plan,true).unwrap();
    let before=runtime::snapshot(&project).unwrap();
    let output=hp(caller.path(), &["--root",root_arg,"approval","demo","policy"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::to_value(authority::policy_reference(&project).unwrap()).unwrap());
    let output=hp(caller.path(), &["--root",root_arg,"approval","demo","inspect"]);
    assert!(output.status.success());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),serde_json::json!([]));
    let output=hp(caller.path(), &["--root",root_arg,"budget","demo","inspect"]);
    assert!(output.status.success());
    let report:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["provider_tokens"],"unknown");assert!(report["policy"].is_null());
    let output=hp(caller.path(), &["--root",root_arg,"routine-store","demo","inspect"]);
    assert!(output.status.success());
    let report:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["execution_enabled"],cfg!(target_os="linux"));assert_eq!(report["automatic_dispatch"],cfg!(target_os="linux"));assert_eq!(report["occurrences"],serde_json::json!([]));
    let output=hp(caller.path(), &["--root",root_arg,"routine-store","demo","schedule","unknown","--expected-head",&before.head.to_string()]);
    assert!(!output.status.success());assert_eq!(runtime::snapshot(&project).unwrap(),before);
    let document=home.path().join("grant.json");
    let signature=home.path().join("grant.sig");
    std::fs::write(&document,"{}").unwrap();
    std::fs::write(&signature,"unsigned").unwrap();
    let output=hp(caller.path(), &["--root",root_arg,"approval","demo","import",document.to_str().unwrap(),signature.to_str().unwrap(),"--expected-head",&before.head.to_string()]);
    assert!(!output.status.success());
    assert_eq!(runtime::snapshot(&project).unwrap(),before);
    let output=hp(caller.path(), &["--root",root_arg,"budget","demo","import",document.to_str().unwrap(),signature.to_str().unwrap(),"--expected-head",&before.head.to_string()]);
    assert!(!output.status.success());assert_eq!(runtime::snapshot(&project).unwrap(),before);
    let output=hp(caller.path(), &["--root",root_arg,"routine-store","demo","import",document.to_str().unwrap(),signature.to_str().unwrap(),"--expected-head",&before.head.to_string()]);
    assert!(!output.status.success());assert_eq!(runtime::snapshot(&project).unwrap(),before);
    assert!(!caller.path().join(".config").exists());
}

#[test]
fn profile_probe_binds_explicit_binaries_without_launching_profile_arguments() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".config/herdr-projects");
    std::fs::create_dir_all(&config).unwrap();
    let herdr = home.path().join("herdr-bin");
    let agent = home.path().join("agent-bin");
    let missing=home.path().join("missing-agent");
    for (kind, version, expected) in [("codex","codex-cli 0.99.0-preview.3","0.99.0-preview.3"),("claude","2.1.0 (Claude Code)","2.1.0")] {
        std::fs::write(config.join("config.toml"),format!("[profiles.worker]\nkind='{kind}'\npermission_policy='interactive'\nextra_args=['SECRET']\nmodel='PRIVATE_MODEL'\nreasoning_effort='PRIVATE_EFFORT'\nenvironment=['PRIVATE_ENV']\n")).unwrap();
        for (path, version) in [(&herdr, "herdr 0.9.1"), (&agent, version)] {
            std::fs::write(path, format!("#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = --version ] || exit 2\nprintf '%s\\n' '{version}'\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let original_config = std::fs::read(config.join("config.toml")).unwrap();
        let args=["profile", "probe", "worker", "--herdr-executable", herdr.to_str().unwrap(), "--agent-executable", agent.to_str().unwrap()];
        let output = hp(home.path(), &args);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["scope"], "local_installation_only");assert_eq!(report["agent"]["status"],"version_observed");
        assert_eq!(report["agent"]["version"], expected);assert_eq!(report["herdr"]["version"], "0.9.1");
        assert_eq!(report["profile"]["agent_version"], report["agent"]["version"]);
        assert_eq!(report["profile"]["herdr_version"], report["herdr"]["version"]);
        for field in ["launchable","protocol_capable","certified"] {assert_eq!(report["profile"][field],false,"{kind}: {field}");}
        assert_eq!(report["profile"]["capabilities"]["checkpoint_acknowledgment"], "unknown");
        for secret in ["SECRET","PRIVATE_MODEL","PRIVATE_EFFORT","PRIVATE_ENV"] {assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));}
        let mut absent=args;absent[6]=missing.to_str().unwrap();assert!(!hp(home.path(),&absent).status.success());
        // A process that prints a plausible version and then fails supplies no evidence.
        std::fs::write(&agent,format!("#!/bin/sh\nprintf '%s\\n' '{version}'\nexit 7\n")).unwrap();
        let failed=hp(home.path(),&args);assert!(failed.status.success(),"{}",String::from_utf8_lossy(&failed.stderr));
        let failed:serde_json::Value=serde_json::from_slice(&failed.stdout).unwrap();assert_eq!(failed["agent"]["status"],"probe_failed");assert!(failed["agent"]["version"].is_null());assert_eq!(failed["profile"]["launchable"],false);
        assert_eq!(std::fs::read(config.join("config.toml")).unwrap(), original_config, "probing must not rewrite model, effort, arguments or environment");
        assert!(!home.path().join(".herdr-projects").exists());
    }
}

#[test]
fn profile_inspection_is_redacted_read_only_and_refuses_malformed_config() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".config/herdr-projects");
    std::fs::create_dir_all(&config).unwrap();
    let path = config.join("config.toml");
    let text = "[profiles.worker]\nkind='codex'\npermission_policy='interactive'\nextra_args=['SECRET_VALUE']\nenvironment=['SECRET_VARIABLE']\n";
    std::fs::write(&path, text).unwrap();
    let output = hp(home.path(), &["profile", "inspect", "worker"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["kind"], "codex");
    assert_eq!(value["launchable"], false);
    assert_eq!(value["capabilities"]["resume"], "unknown");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SECRET"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    assert!(!home.path().join(".herdr-projects").exists());
    for bad in ["SECRET = [", "[profiles.worker]\nkind='codex'\npermission_policy='interactive'\ncredential='SECRET'"] {
        std::fs::write(&path, bad).unwrap();
        let output = hp(home.path(), &["profile", "inspect", "worker"]);
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SECRET"));
    }
}

#[test]
fn profile_resolve_prints_envelope_without_argv_and_selects_unique_kind() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".config/herdr-projects");
    std::fs::create_dir_all(&config).unwrap();
    let path = config.join("config.toml");
    std::fs::write(&path, "[profiles.implementation]\nkind='codex'\npermission_policy='interactive'\nextra_args=['SECRET_ARG']\n[profiles.implementation.budget]\nsoft_input_tokens=80\nunknown_usage='allow_with_warning'\n[profiles.planner]\nkind='claude'\npermission_policy='interactive'\n").unwrap();
    let output = hp(home.path(), &["profile", "resolve", "implementation"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["kind"], "codex");
    assert_eq!(value["budget"]["soft_input_chars"], 320);
    assert_eq!(value["budget"]["estimator"], "char-count-v1");
    assert!(value["frozen"].is_null());
    assert_eq!(value["inspection"]["launchable"], false);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SECRET"));
    let by_kind = hp(home.path(), &["profile", "resolve", "--agent", "claude"]);
    assert!(by_kind.status.success(), "{}", String::from_utf8_lossy(&by_kind.stderr));
    let value: serde_json::Value = serde_json::from_slice(&by_kind.stdout).unwrap();
    assert_eq!(value["name"], "planner");
    assert_eq!(value["budget"]["soft_input_chars"], 32000);
    std::fs::write(&path, "[profiles.a]\nkind='codex'\npermission_policy='interactive'\n[profiles.b]\nkind='codex'\npermission_policy='interactive'\n").unwrap();
    let output = hp(home.path(), &["profile", "resolve", "--agent", "codex"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("multiple named profiles"));
    assert!(!home.path().join(".herdr-projects").exists());
}

fn hp(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .env_clear()
        .env("HOME", home)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn build_info_is_independent_of_config_projects_and_sessions() {
    let home=tempfile::tempdir().unwrap();
    let config=home.path().join(".config/herdr-projects");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.toml"),"this is deliberately invalid TOML [").unwrap();
    let root=home.path().join("must-not-be-created");
    let output=hp(home.path(),&["--root",root.to_str().unwrap(),"build-info"]);
    assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let value:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["state_store"],cfg!(feature="state-store"));
    assert_eq!(value["os"],std::env::consts::OS);
    assert_eq!(value["live_capacity_certified"],false);
    #[cfg(feature="state-store")]
    {
        assert_eq!(value["schema"],herdr_projects::store::SCHEMA);
        assert_eq!(value["sqlite"]["version"],rusqlite::version());
        assert_eq!(value["sqlite"]["minimum"],herdr_projects::store::MIN_SQLITE_VERSION);
    }
    #[cfg(not(feature="state-store"))]
    {
        assert!(value["schema"].is_null());
        assert!(value["sqlite"].is_null());
        assert_eq!(value["factory_runtime_compatible"],false);
    }
    let checked=hp(home.path(),&["--root",root.to_str().unwrap(),"build-info","--require-factory"]);
    assert_eq!(checked.status.success(),value["factory_runtime_compatible"]==true);
    assert!(!root.exists());
    assert_eq!(std::fs::read_to_string(config.join("config.toml")).unwrap(),"this is deliberately invalid TOML [");
}


#[test]
fn doctor_reports_compiled_features_without_migrating_a_legacy_project() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(
        hp(home.path(), &["--root", root_arg, "new", "demo"])
            .status
            .success()
    );
    let project = root.join("demo");
    let files = ["PROJECT.md", "MEMORY.md", ".state/project.json"];
    let before: Vec<_> = files
        .iter()
        .map(|rel| std::fs::read(project.join(rel)).unwrap())
        .collect();
    let out = hp(home.path(), &["--root", root_arg, "doctor"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("[ok  ] project demo: active; never opened"),
        "{text}"
    );
    assert!(
        text.contains("cargo build --release --locked --features state-store"),
        "{text}"
    );
    assert!(
        text.contains(
            "upgrade: existing projects upgrade only through `migration PROJECT upgrade-store`"
        ),
        "{text}"
    );
    #[cfg(feature = "state-store")]
    {
        assert!(text.contains("state-store: compiled\n"), "{text}");
        assert!(
            text.contains(&format!("schema: {}\n", herdr_projects::store::SCHEMA)),
            "{text}"
        );
        assert!(
            text.contains(&format!("sqlite: {}\n", rusqlite::version())),
            "{text}"
        );
        assert!(
            text.contains("prepared_dispatch: true\n"),
            "quotes PREPARED_LAUNCH_DISPATCH_ENABLED; {text}"
        );
        assert!(
            !text.contains("canonical factory commands are absent"),
            "{text}"
        );
    }
    #[cfg(not(feature = "state-store"))]
    {
        assert!(text.contains("state-store: not compiled\n"), "{text}");
        assert!(text.contains("schema: absent\n"), "{text}");
        assert!(text.contains("sqlite: absent\n"), "{text}");
        assert!(text.contains("prepared_dispatch: absent\n"), "{text}");
        assert!(
            text.contains("factory: canonical factory commands are absent"),
            "{text}"
        );
    }
    for (rel, bytes) in files.iter().zip(before) {
        assert_eq!(std::fs::read(project.join(rel)).unwrap(), bytes, "{rel}");
    }
    assert!(!project.join(".state/state.db").exists());
    assert!(!project.join(".state/format.json").exists());
    assert!(!project.join(".state/migration").exists());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("upgrade-store"), "{stderr}");
    assert!(!stderr.contains("Store schema upgraded"), "{stderr}");
}

#[test]
fn context_prints_a_usable_prefix_in_a_scrubbed_environment() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("my root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());

    let out = hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let prefix = text.lines().next().unwrap().strip_prefix("Commands: ").unwrap();
    // Fixed shape `<binary> --root <root>`, with the spaced root shell-quoted.
    assert_eq!(prefix, format!("{BIN} --root '{root_arg}'"));

    // The printed prefix works as typed, from a bare shell.
    let listed = Command::new("/bin/sh")
        .env_clear()
        .env("HOME", home.path())
        .args(["-c", &format!("{prefix} list")])
        .output()
        .unwrap();
    assert!(listed.status.success());
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "demo\tactive\tno threads\n");
}

#[test]
fn peek_records_nothing_and_context_records_seen_items() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "demo"]).status.success());
    let item = "+++\nid = \"20260917T000000Z-routine-r-1\"\nkind = \"routine\"\nsubject = \"r\"\ncreated = \"x\"\nsummary = \"s\"\n+++\n";
    std::fs::write(root.join("demo/inbox/20260917T000000Z-routine-r-1.md"), item).unwrap();
    let seen = root.join("demo/.state/inbox-seen.json");

    assert!(hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]).status.success());
    assert!(!seen.exists());
    assert!(hp(home.path(), &["--root", root_arg, "context", "demo"]).status.success());
    assert!(std::fs::read_to_string(&seen).unwrap().contains("routine-r-1"));
}

#[test]
fn path_like_names_and_slugs_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(!hp(home.path(), &["--root", root_arg, "new", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "open", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "context", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "thread", "list", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "delete", "../x", "--force"]).status.success());
    assert!(!root.exists());
    assert!(!home.path().join("x").exists());
}

#[test]
fn ticker_start_without_projects_creates_nothing() {
    let home = tempfile::tempdir().unwrap();
    assert!(hp(home.path(), &["ticker", "start"]).status.success());
    assert!(!home.path().join(".herdr-projects").exists());
    assert!(!home.path().join(".config").exists());
}

#[test]
fn repair_is_explicit_hash_checked_and_preserves_original_bytes() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let r = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", r, "new", "demo"]).status.success());
    let target = root.join("demo/threads/t-0001.toml");
    let bad = b"invalid = [";
    std::fs::write(&target, bad).unwrap();
    let output = hp(home.path(), &["--root", r, "repair", "demo", "inspect"]);
    assert!(output.status.success());
    let diagnostics: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let hash = diagnostics[0]["sha256"].as_str().unwrap();
    assert_eq!(diagnostics[0]["path"], "threads/t-0001.toml");
    assert_eq!(std::fs::read(&target).unwrap(), bad);
    let replacement = home.path().join("replacement");
    std::fs::write(&replacement, "id = 't-0002'\n").unwrap();
    let args = ["--root", r, "repair", "demo", "restore", "threads/t-0001.toml", "--from", replacement.to_str().unwrap(), "--expected-hash", hash];
    assert!(!hp(home.path(), &args).status.success());
    std::fs::write(&replacement, "id = 't-0001'\n").unwrap();
    let lock = std::fs::File::options().write(true).create(true).truncate(false).open(root.join(".ticker.lock")).unwrap();
    lock.lock().unwrap();
    assert!(!hp(home.path(), &args).status.success());
    drop(lock);
    let output = hp(home.path(), &args);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(std::fs::read(root.join(format!("demo/.state/repair-backups/{hash}.original"))).unwrap(), bad);
    assert!(!hp(home.path(), &args).status.success()); // stale inspection must not overwrite
    assert!(serde_json::from_slice::<Vec<serde_json::Value>>(&hp(home.path(), &["--root", r, "repair", "demo", "inspect"]).stdout).unwrap().is_empty());
}

#[test]
fn native_artifact_helper_needs_no_configuration_and_preserves_large_binary_payload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("space 'λ$; source");
    std::fs::create_dir(&path).unwrap();
    let bytes = vec![255u8; 2 * 1024 * 1024];
    std::fs::write(path.join("report.md"), &bytes).unwrap();
    let output = Command::new(BIN).env_clear().args(["artifact-stream", "--probe"]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["schema"], 1);
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["live_versions"], serde_json::json!([1]));
    let output = Command::new(BIN).env_clear().args(["artifact-stream", "--path", path.to_str().unwrap()]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(&output.stdout[..8], b"HPAR\x01\0\0\0");
    let size = u32::from_be_bytes(output.stdout[8..12].try_into().unwrap()) as usize;
    assert_eq!(&output.stdout[12 + size..], bytes);
    std::os::unix::fs::symlink("report.md", path.join("library")).unwrap();
    assert!(!Command::new(BIN).env_clear().args(["artifact-stream", "--path", path.to_str().unwrap()]).output().unwrap().status.success());
    let live=Command::new(BIN).env_clear().args(["artifact-stream","--live","--path",path.to_str().unwrap()]).output().unwrap();
    assert!(live.status.success(),"{}",String::from_utf8_lossy(&live.stderr));
    assert_eq!(&live.stdout[..8],b"HPLV\x01\0\0\0");
    let size=u32::from_be_bytes(live.stdout[8..12].try_into().unwrap()) as usize;
    let manifest:serde_json::Value=serde_json::from_slice(&live.stdout[12..12+size]).unwrap();
    assert_eq!(manifest["omissions"][0]["reason"],"symbolic-link");
    assert_eq!(&live.stdout[12+size..],bytes);
}

#[test]
fn corrupted_project_lifecycle_is_visible_and_cannot_authorize_execution() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let r = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", r, "new", "demo"]).status.success());
    let target = root.join("demo/.state/project.json");
    for broken in ["{corrupt", "{}", r#"{"status":"future-state"}"#] {
        std::fs::write(&target, broken).unwrap();
        let listed = hp(home.path(), &["--root", r, "list"]);
        assert!(String::from_utf8_lossy(&listed.stdout).contains("invalid"));
        assert!(!hp(home.path(), &["--root", r, "open", "demo"]).status.success());
        assert!(!hp(home.path(), &["--root", r, "resume", "demo"]).status.success());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), broken);
        let diagnostics: serde_json::Value = serde_json::from_slice(&hp(home.path(), &["--root", r, "repair", "demo", "inspect"]).stdout).unwrap();
        assert_eq!(diagnostics[0]["path"], ".state/project.json");
        let replacement = home.path().join("replacement.json");
        std::fs::write(&replacement, r#"{"status":"paused"}"#).unwrap();
        assert!(hp(home.path(), &["--root", r, "repair", "demo", "restore", ".state/project.json", "--from", replacement.to_str().unwrap(), "--expected-hash", diagnostics[0]["sha256"].as_str().unwrap()]).status.success());
        assert!(String::from_utf8_lossy(&hp(home.path(), &["--root", r, "list"]).stdout).contains("paused"));
    }
}

#[test]
fn legacy_commands_refuse_store_ownership_even_without_feature() {
    let home=tempfile::tempdir().unwrap(); let root=home.path().join("root"); let root_arg=root.to_str().unwrap();
    assert!(hp(home.path(),&["--root",root_arg,"new","demo"]).status.success());
    let state=root.join("demo/.state/project.json"); let before=std::fs::read(&state).unwrap();
    std::fs::write(root.join("demo/.state/format.json"),b"unknown or interrupted format").unwrap();
    for command in ["resume","pause","open","context"] {
        let out=hp(home.path(),&["--root",root_arg,command,"demo"]);
        assert!(!out.status.success()); assert!(String::from_utf8_lossy(&out.stderr).contains("legacy runtime is disabled"));
    }
    assert_eq!(std::fs::read(state).unwrap(),before);
    let listed=hp(home.path(),&["--root",root_arg,"list"]); assert!(listed.status.success());
    assert!(String::from_utf8_lossy(&listed.stdout).contains("store/maintenance"));
}

#[test]
#[cfg(feature="state-store")]
fn migration_cli_round_trip_keeps_memory_and_blocks_legacy_mutation() {
    let home=tempfile::tempdir().unwrap(); let root=home.path().join("root"); let root_arg=root.to_str().unwrap();
    for args in [vec!["--root",root_arg,"new","demo"],vec!["--root",root_arg,"pause","demo"]] {
        let out=hp(home.path(),&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    }
    let memory=std::fs::read(root.join("demo/MEMORY.md")).unwrap();
    let plan=home.path().join("plan.json"); let plan_arg=plan.to_str().unwrap();
    for args in [vec!["plan","--output",plan_arg],vec!["apply","--plan",plan_arg,"--writers-stopped"],vec!["status"],vec!["export"],vec!["recover","--writers-stopped"]] {
        let mut full=vec!["--root",root_arg,"migration","demo"];full.extend(args);
        let out=hp(home.path(),&full);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    }
    assert_eq!(std::fs::read(root.join("demo/MEMORY.md")).unwrap(),memory);
    assert!(!hp(home.path(),&["--root",root_arg,"resume","demo"]).status.success());
    let restore=home.path().join("restored");
    let out=hp(home.path(),&["--root",root_arg,"migration","demo","restore","--destination",restore.to_str().unwrap()]);
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert!(restore.join("PROJECT.md").is_file());
}
#[cfg(feature="state-store")]
fn configure_checkpoint_profile(home: &std::path::Path) {
    let config = home.join(".config/herdr-projects");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.toml"), "[profiles.planner]\nkind='claude'\npermission_policy='interactive'\n").unwrap();
}

#[test]
#[cfg(feature="state-store")]
fn migrated_task_commands_use_revisions_and_do_not_touch_legacy_task_file() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for args in [vec!["--root",root_arg,"new","demo"],vec!["--root",root_arg,"pause","demo"]] {assert!(hp(home.path(),&args).status.success());}
    let plan=home.path().join("plan.json");
    for args in [vec!["plan","--output",plan.to_str().unwrap()],vec!["apply","--plan",plan.to_str().unwrap(),"--writers-stopped"],vec!["upgrade-store"]] {
        let mut full=vec!["--root",root_arg,"migration","demo"];full.extend(args);let out=hp(home.path(),&full);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    }
    let original=std::fs::read(root.join("demo/TASKS.md")).unwrap();
    let out=hp(home.path(),&["--root",root_arg,"task","demo","list"]);assert!(out.status.success());
    let snapshot:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();let head=snapshot["head"].as_u64().unwrap().to_string();
    let args=["--root",root_arg,"task","demo","add","operator-task","--title","Keep original","--expected-head",&head];
    let out=hp(home.path(),&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert!(!hp(home.path(),&args).status.success());
    let missing = hp(home.path(), &["--root",root_arg,"context","demo","--peek"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("context requires profiles.planner or --profile NAME"));
    configure_checkpoint_profile(home.path());
    let out=hp(home.path(),&["--root",root_arg,"context","demo","--peek"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert!(String::from_utf8_lossy(&out.stdout).contains("Runtime owner: SQLite"));
    let named=hp(home.path(),&["--root",root_arg,"context","demo","--peek","--session","test-context"]);
    assert!(named.status.success(),"{}",String::from_utf8_lossy(&named.stderr));
    let rendered=String::from_utf8(named.stdout).unwrap();assert!(rendered.contains("kind=full"));
    let checkpoint=rendered.split_whitespace().nth(1).unwrap();
    assert!(!hp(home.path(),&["--root",root_arg,"context","demo","--ack",checkpoint]).status.success());
    assert!(!hp(home.path(),&["--root",root_arg,"context","demo","--session","other-context","--ack",checkpoint]).status.success());
    assert!(hp(home.path(),&["--root",root_arg,"context","demo","--session","test-context","--ack",checkpoint]).status.success());
    let continued=hp(home.path(),&["--root",root_arg,"context","demo","--peek","--session","test-context"]);
    assert!(continued.status.success(),"{}",String::from_utf8_lossy(&continued.stderr));
    assert!(String::from_utf8_lossy(&continued.stdout).contains("kind=delta"));
    let restarted=hp(home.path(),&["--root",root_arg,"context","demo","--peek"]);
    assert!(restarted.status.success());assert!(String::from_utf8_lossy(&restarted.stdout).contains("kind=full"));
    let out=hp(home.path(),&["--root",root_arg,"migration","demo","recover","--writers-stopped"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read(root.join("demo/TASKS.md")).unwrap(),original);
    let out=hp(home.path(),&["--root",root_arg,"operations","demo","inspect"]);assert!(out.status.success());assert_eq!(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),serde_json::json!([]));
    assert!(!hp(home.path(),&["--root",root_arg,"resume","demo"]).status.success());
}
#[test]
#[cfg(feature="state-store")]
fn migrated_inbox_cli_drains_once_marks_seen_and_keeps_legacy_files_untouched() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for args in [vec!["--root",root_arg,"new","demo"],vec!["--root",root_arg,"pause","demo"]] {assert!(hp(home.path(),&args).status.success());}
    let ticker=root.join("demo/.state/ticker.json");let bytes=br#"{"pending_events":{"notice":{"id":"notice","kind":"notice","subject":"s","summary":"summary","body":"  body"}}}"#;std::fs::write(&ticker,bytes).unwrap();
    let plan=home.path().join("plan.json");for args in [vec!["plan","--output",plan.to_str().unwrap()],vec!["apply","--plan",plan.to_str().unwrap(),"--writers-stopped"]] {let mut full=vec!["--root",root_arg,"migration","demo"];full.extend(args);let out=hp(home.path(),&full);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));}
    let out=hp(home.path(),&["--root",root_arg,"task","demo","list"]);let snapshot:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();let head=snapshot["head"].as_u64().unwrap().to_string();
    let out=hp(home.path(),&["--root",root_arg,"operations","demo","drain-inbox","--expected-head",&head]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    configure_checkpoint_profile(home.path());
    let out=hp(home.path(),&["--root",root_arg,"context","demo","--peek"]);assert!(out.status.success());assert!(String::from_utf8_lossy(&out.stdout).contains("  body"));
    let out=hp(home.path(),&["--root",root_arg,"inbox","list","demo"]);let items:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(items[0]["seen"],false);
    assert!(hp(home.path(),&["--root",root_arg,"context","demo"]).status.success());
    assert!(hp(home.path(),&["--root",root_arg,"inbox","done","demo","notice"]).status.success());
    let out=hp(home.path(),&["--root",root_arg,"inbox","list","demo"]);let items:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(items[0]["seen"],true);assert_eq!(items[0]["done"],true);
    assert_eq!(std::fs::read(ticker).unwrap(),bytes);assert!(!root.join("demo/inbox/notice.md").exists());
}
#[test]
#[cfg(feature="state-store")]
fn preflight_refuses_fifo_and_oversized_external_config_without_hanging() {
    use std::{ffi::CString,os::unix::ffi::OsStrExt,process::Stdio,time::{Duration,Instant}};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    assert!(hp(home.path(),&["--root",root_arg,"new","demo"]).status.success());assert!(hp(home.path(),&["--root",root_arg,"pause","demo"]).status.success());
    let config=home.path().join(".config/herdr-projects/config.toml");std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    for fifo in [true,false] {
        if fifo {let path=CString::new(config.as_os_str().as_bytes()).unwrap();
            // SAFETY: valid NUL-terminated disposable path, permissions only.
            assert_eq!(unsafe{libc::mkfifo(path.as_ptr(),0o600)},0);
        } else {std::fs::File::create(&config).unwrap().set_len(16*1024*1024+1).unwrap();}
        let output=std::fs::File::create(home.path().join("preflight.json")).unwrap();
        let mut child=Command::new(BIN).env_clear().env("HOME",home.path()).args(["--root",root_arg,"migration","demo","preflight"]).stdout(output).stderr(Stdio::inherit()).spawn().unwrap();
        let deadline=Instant::now()+Duration::from_secs(5);
        loop {if let Some(status)=child.try_wait().unwrap(){assert!(status.success());break;}if Instant::now()>deadline{let _=child.kill();let _=child.wait();panic!("preflight blocked on config");}std::thread::sleep(Duration::from_millis(10));}
        let report:serde_json::Value=serde_json::from_slice(&std::fs::read(home.path().join("preflight.json")).unwrap()).unwrap();assert!(report["blockers"].as_array().unwrap().iter().any(|v|v.as_str().unwrap().contains("config.toml")));
        std::fs::remove_file(&config).unwrap();
    }
}

#[test]
#[cfg(feature="state-store")]
fn migration_plan_binds_config_and_rejects_legacy_unbound_plans() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let config=home.path().join(".config/herdr-projects/config.toml");std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let bytes=b"private='do-not-print-this-value'\n";std::fs::write(&config,bytes).unwrap();
    let plan=home.path().join("plan.json");
    let out=hp(home.path(),&["--root",root_arg,"migration","demo","plan","--output",plan.to_str().unwrap()]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let encoded=std::fs::read(&plan).unwrap();assert!(!String::from_utf8_lossy(&encoded).contains("do-not-print-this-value"));
    std::fs::write(&config,b"private='changed'\n").unwrap();
    let args=["--root",root_arg,"migration","demo","apply","--plan",plan.to_str().unwrap(),"--writers-stopped"];
    assert!(!hp(home.path(),&args).status.success());assert!(!root.join("demo/.state/migration").exists());
    std::fs::write(&config,bytes).unwrap();
    let mut unbound:serde_json::Value=serde_json::from_slice(&encoded).unwrap();unbound["version"]=serde_json::json!(1);unbound.as_object_mut().unwrap().remove("config");
    std::fs::write(&plan,serde_json::to_vec(&unbound).unwrap()).unwrap();
    let out=hp(home.path(),&args);assert!(!out.status.success());assert!(String::from_utf8_lossy(&out.stderr).contains("regenerate"));
    std::fs::write(&plan,encoded).unwrap();assert!(hp(home.path(),&args).status.success());
    assert_eq!(std::fs::read(config).unwrap(),bytes);
}

#[test]
fn root_config_special_files_fail_promptly_without_an_explicit_root() {
    use std::{ffi::CString,os::unix::ffi::OsStrExt,process::Stdio,time::{Duration,Instant}};
    let home=tempfile::tempdir().unwrap();let config=home.path().join(".config/herdr-projects/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    for fifo in [true,false] {
        if fifo {let path=CString::new(config.as_os_str().as_bytes()).unwrap();
            // SAFETY: valid NUL-terminated path in a disposable directory.
            assert_eq!(unsafe{libc::mkfifo(path.as_ptr(),0o600)},0);
        } else {std::fs::File::create(&config).unwrap().set_len(16*1024*1024+1).unwrap();}
        let mut child=Command::new(BIN).env_clear().env("HOME",home.path()).arg("list").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let deadline=Instant::now()+Duration::from_secs(5);
        loop {if let Some(status)=child.try_wait().unwrap(){assert!(!status.success());break;}if Instant::now()>deadline{let _=child.kill();let _=child.wait();panic!("root resolution blocked on config");}std::thread::sleep(Duration::from_millis(10));}
        std::fs::remove_file(&config).unwrap();
    }
}

#[test]
#[cfg(feature="state-store")]
fn plan_wait_cli_registers_replays_and_keeps_capacity_untouched() {
    use herdr_projects::{domain::*,migration};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");
    let plan=migration::inspect_with_config(&project,&home.path().join(".config/herdr-projects/config.toml")).unwrap();
    migration::apply(&project,&plan,true).unwrap();
    let mut db=migration::open_active(&project).unwrap();
    db.commit(Commit{expected_head:db.current_head().unwrap(),mutations:vec![Mutation::Task{expected:None,next:Task{id:TaskId::new("parent").unwrap(),revision:1,state:TaskState::Blocked,title:"parent".into(),active_attempt:None}}]}).unwrap();
    drop(db);
    let registered=hp(home.path(),&["--root",root_arg,"plan","wait","demo","register","--task","parent","--condition","dependency_evidence"]);
    assert!(registered.status.success(),"{}",String::from_utf8_lossy(&registered.stderr));
    let value:serde_json::Value=serde_json::from_slice(&registered.stdout).unwrap();
    let id=value["wait_id"].as_str().unwrap();
    let replay=hp(home.path(),&["--root",root_arg,"plan","wait","demo","replay",id]);
    assert!(replay.status.success(),"{}",String::from_utf8_lossy(&replay.stderr));
    let value:serde_json::Value=serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(value["wake_requested"],false);assert_eq!(value["proved"],false);
    let approval=format!("approval-{}","a".repeat(64));
    let typed=hp(home.path(),&["--root",root_arg,"plan","wait","demo","register","--task","parent","--condition","user_decision","--approval-id",&approval,"--approval-task-revision","2"]);
    assert!(typed.status.success(),"{}",String::from_utf8_lossy(&typed.stderr));
    let typed:serde_json::Value=serde_json::from_slice(&typed.stdout).unwrap();
    let replay=hp(home.path(),&["--root",root_arg,"plan","wait","demo","replay",typed["wait_id"].as_str().unwrap()]);
    assert!(replay.status.success(),"{}",String::from_utf8_lossy(&replay.stderr));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&replay.stdout).unwrap()["wake_requested"],false);
    assert!(!hp(home.path(),&["--root",root_arg,"plan","wait","demo","register","--task","parent","--condition","user_decision","--approval-id",&approval]).status.success());
    assert!(!hp(home.path(),&["--root",root_arg,"plan","wait","demo","register","--task","parent","--condition","dependency_evidence","--approval-id",&approval,"--approval-task-revision","2"]).status.success());
    assert!(!hp(home.path(),&["--root",root_arg,"feedback","demo","replan","missing-feedback"]).status.success());
    let expired=hp(home.path(),&["--root",root_arg,"plan","wait","demo","register","--task","parent","--condition","user_decision","--deadline","2000-01-01T00:00:00Z"]);
    assert!(expired.status.success(),"{}",String::from_utf8_lossy(&expired.stderr));
    let expired:serde_json::Value=serde_json::from_slice(&expired.stdout).unwrap();
    let replay=hp(home.path(),&["--root",root_arg,"plan","wait","demo","replay",expired["wait_id"].as_str().unwrap()]);
    assert!(replay.status.success(),"{}",String::from_utf8_lossy(&replay.stderr));
    let replay:serde_json::Value=serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(replay["wake_requested"],true);assert_eq!(replay["proved"],false);
    let previous=expired["wait_id"].as_str().unwrap();
    let rearmed=hp(home.path(),&["--root",root_arg,"plan","wait","demo","rearm",previous]);
    assert!(rearmed.status.success(),"{}",String::from_utf8_lossy(&rearmed.stderr));
    let rearmed:serde_json::Value=serde_json::from_slice(&rearmed.stdout).unwrap();
    assert_ne!(rearmed["wait_id"],expired["wait_id"]);
    let replay=hp(home.path(),&["--root",root_arg,"plan","wait","demo","replay",rearmed["wait_id"].as_str().unwrap()]);
    assert!(replay.status.success(),"{}",String::from_utf8_lossy(&replay.stderr));
    let replay:serde_json::Value=serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(replay["wake_requested"],false);
    let again=hp(home.path(),&["--root",root_arg,"plan","wait","demo","rearm",previous]);
    assert!(again.status.success(),"{}",String::from_utf8_lossy(&again.stderr));
    let again:serde_json::Value=serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(again["wait_id"],rearmed["wait_id"]);
    assert_eq!(again["already_registered"],true);
    let mut db=migration::open_active(&project).unwrap();
    db.commit(Commit{expected_head:db.current_head().unwrap(),mutations:vec![Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new("capacity-holder").unwrap(),task:TaskId::new("parent").unwrap(),revision:1,state:AttemptState::Running,snapshot:None,reservation:"retained".into(),termination_observed:false}}]}).unwrap();drop(db);
    let capacity=hp(home.path(),&["--root",root_arg,"plan","wait","demo","register","--task","parent","--condition","resource_availability","--capacity-attempt","capacity-holder","--capacity-after-revision","1"]);
    assert!(capacity.status.success(),"{}",String::from_utf8_lossy(&capacity.stderr));
    let capacity:serde_json::Value=serde_json::from_slice(&capacity.stdout).unwrap();
    let replay=hp(home.path(),&["--root",root_arg,"plan","wait","demo","replay",capacity["wait_id"].as_str().unwrap()]);
    assert!(replay.status.success(),"{}",String::from_utf8_lossy(&replay.stderr));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&replay.stdout).unwrap()["wake_requested"],false);
    assert!(!hp(home.path(),&["--root",root_arg,"plan","wait","demo","register","--task","parent","--condition","resource_availability","--capacity-attempt","capacity-holder"]).status.success());
    assert!(!hp(home.path(),&["--root",root_arg,"plan","wait","demo","register","--task","parent","--condition","resource_availability","--capacity-attempt","capacity-holder","--capacity-after-revision","1","--approval-id",&approval,"--approval-task-revision","2"]).status.success());
    let snapshot=migration::open_active(&project).unwrap().read_snapshot(None).unwrap();
    assert_eq!(snapshot.attempts.len(),1);assert!(snapshot.attempts[0].retains_capacity());
}

#[test]
#[cfg(feature="state-store")]
fn planner_session_cli_retains_inputs_and_replays_bound_proposals() {
    use herdr_projects::migration;
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let plan=migration::inspect_with_config(&project,&home.path().join(".config/herdr-projects/config.toml")).unwrap();migration::apply(&project,&plan,true).unwrap();
    let before=migration::open_active(&project).unwrap().read_snapshot(None).unwrap();
    let intent=home.path().join("intent.txt");std::fs::write(&intent,"Add an independently verified feature\n").unwrap();
    let args=["--root",root_arg,"plan","session","demo","create","planner-1","--intent-file",intent.to_str().unwrap(),"--expected-head",&before.head.to_string(),"--expected-plan-revision","0"];
    let created=hp(home.path(),&args);assert!(created.status.success(),"{}",String::from_utf8_lossy(&created.stderr));
    let session:serde_json::Value=serde_json::from_slice(&created.stdout).unwrap();
    let retry=hp(home.path(),&args);assert!(retry.status.success());assert_eq!(serde_json::from_slice::<serde_json::Value>(&retry.stdout).unwrap(),session);
    let db=rusqlite::Connection::open(session["input"]["project_store"].as_str().unwrap()).unwrap();
    let empty=hp(home.path(),&["--root",root_arg,"plan","inspect","demo"]);assert!(empty.status.success());
    let empty:serde_json::Value=serde_json::from_slice(&empty.stdout).unwrap();assert_eq!(empty["plan_revision"],0);assert_eq!(empty["entries"],serde_json::json!([]));assert!(empty["next_after"].is_null());
    let count=|table:&str|db.query_row(&format!("SELECT count(*) FROM {table}"),[],|r|r.get::<_,u64>(0)).unwrap();
    let authority_tables=["task_contracts","contract_scope_paths","contract_named_resources","attempts","task_queue"];
    let authority_before:Vec<_>=authority_tables.iter().map(|t|count(t)).collect();
    let raw=serde_json::json!({"version":2,"planner":{"session_id":"planner-1","input_digest":session["input_digest"],"rationale":"Follow the retained intent"},"contracts":[{"task_id":"planned","text":"Implement and verify the feature","dependencies":[]}]});
    let document=home.path().join("proposal.json");std::fs::write(&document,serde_json::to_vec(&raw).unwrap()).unwrap();
    let propose=["--root",root_arg,"plan","propose","demo","--input-file",document.to_str().unwrap(),"--expected-plan-revision","0","--idempotency-key","planner-response"];
    let accepted=hp(home.path(),&propose);assert!(accepted.status.success(),"{}",String::from_utf8_lossy(&accepted.stderr));
    let first:serde_json::Value=serde_json::from_slice(&accepted.stdout).unwrap();assert_eq!(first["replayed"],false);assert_eq!(first["plan_revision"],1);
    let shown=hp(home.path(),&["--root",root_arg,"plan","session","demo","show","planner-1"]);assert!(shown.status.success());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&shown.stdout).unwrap(),session);
    let replay=hp(home.path(),&propose);assert!(replay.status.success());assert_eq!(serde_json::from_slice::<serde_json::Value>(&replay.stdout).unwrap()["replayed"],true);
    let head=migration::open_active(&project).unwrap().current_head().unwrap();
    let mut changed=raw.clone();changed["contracts"][0]["text"]=serde_json::json!("Changed under the same key");
    std::fs::write(&document,changed.to_string()).unwrap();
    assert!(!hp(home.path(),&propose).status.success());
    assert_eq!(migration::open_active(&project).unwrap().current_head().unwrap(),head);
    std::fs::write(&document,serde_json::to_vec(&raw).unwrap()).unwrap();
    let stale=hp(home.path(),&["--root",root_arg,"plan","propose","demo","--input-file",document.to_str().unwrap(),"--expected-plan-revision","0","--idempotency-key","stale-key"]);
    assert!(!stale.status.success());assert_eq!(migration::open_active(&project).unwrap().current_head().unwrap(),head);
    for table in ["plan_proposals","plan_revisions"] {assert_eq!(count(table),1);}
    let (stored,parent,digest):(Vec<u8>,u64,String)=db.query_row("SELECT payload,parent_revision,payload_digest FROM plan_proposals",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(stored,serde_json::to_vec(&raw).unwrap());assert_eq!(parent,0);assert_eq!(digest,first["digest"].as_str().unwrap());
    assert_eq!(db.query_row("SELECT count(*) FROM events WHERE kind='plan.proposed'",[],|r|r.get::<_,u64>(0)).unwrap(),1);
    let inspected=hp(home.path(),&["--root",root_arg,"plan","inspect","demo","--limit","1"]);
    assert!(inspected.status.success(),"{}",String::from_utf8_lossy(&inspected.stderr));
    let page:serde_json::Value=serde_json::from_slice(&inspected.stdout).unwrap();
    assert_eq!(page["plan_revision"],1);assert_eq!(page["entries"][0]["task_id"],"planned");assert!(page["next_after"].is_null());
    assert_eq!(migration::open_active(&project).unwrap().current_head().unwrap(),head);
    assert!(!hp(home.path(),&["--root",root_arg,"plan","inspect","demo","--after","planned"]).status.success());
    let next=home.path().join("next.json");std::fs::write(&next,serde_json::json!({"version":1,"contracts":[{"task_id":"zz-next","text":"Follow the first planned task","dependencies":[{"predecessor":"planned","requirement":"verified_result"}]}]}).to_string()).unwrap();
    let accepted=hp(home.path(),&["--root",root_arg,"plan","propose","demo","--input-file",next.to_str().unwrap(),"--expected-plan-revision","1","--idempotency-key","next"]);
    assert!(accepted.status.success(),"{}",String::from_utf8_lossy(&accepted.stderr));
    assert!(!hp(home.path(),&["--root",root_arg,"plan","inspect","demo","--after","planned","--expected-plan-revision","1"]).status.success());
    let last=hp(home.path(),&["--root",root_arg,"plan","inspect","demo","--after","planned","--expected-plan-revision","2","--limit","1"]);assert!(last.status.success());
    let page:serde_json::Value=serde_json::from_slice(&last.stdout).unwrap();assert_eq!(page["entries"][0]["task_id"],"zz-next");assert_eq!(page["entries"][0]["dependencies"][0]["predecessor"],"planned");assert!(page["next_after"].is_null());
    // Each invocation starts a new process: retained references and retries must
    // survive reopening the store throughout the complete typed-change flow.
    use sha2::{Digest,Sha256};
    let hash=|bytes:&[u8]|format!("{:x}",Sha256::digest(bytes));
    let mut previous=serde_json::Value::Null;
    for (step,kind) in ["create_contract","supersede_unstarted","add_dependencies","request_cancellation"].into_iter().enumerate() {
        let parent=2+step as u64;let name=format!("typed-{step}");let parent_arg=parent.to_string();
        let head=migration::open_active(&project).unwrap().current_head().unwrap().to_string();
        let created=hp(home.path(),&["--root",root_arg,"plan","session","demo","create",&name,"--intent-file",intent.to_str().unwrap(),"--expected-head",&head,"--expected-plan-revision",&parent_arg]);
        assert!(created.status.success(),"{}",String::from_utf8_lossy(&created.stderr));
        let session:serde_json::Value=serde_json::from_slice(&created.stdout).unwrap();
        let edge=serde_json::json!({"predecessor":"planned","requirement":"verified_result"});
        let mut contract=serde_json::json!({"task_id":"typed-task","text":if step==0 {"Initial intent"}else{"Revised intent"},"dependencies":if step<2 {serde_json::json!([])}else{serde_json::json!([edge.clone()])}});
        let mut change=serde_json::json!({"kind":kind,"task_id":"typed-task"});
        if step>0 {change["expected"]=serde_json::json!({"proposal_id":previous["proposal_id"],"plan_revision":parent,"payload_digest":previous["digest"]});}
        if step==2 {change["dependencies"]=serde_json::json!([edge]);}
        if step==3 {change["reason"]=serde_json::json!("Requirements withdrawn");contract["cancellation_reason"]=change["reason"].clone();}
        let planner=serde_json::json!({"session_id":name,"input_digest":session["input_digest"],"rationale":"Apply reviewed typed change"});
        let mut payload=serde_json::json!({"contracts":[contract],"planner":planner,"changes":[change]});payload.sort_all_objects();
        let mut raw=payload.clone();raw["version"]=serde_json::json!(3);
        raw["envelope"]=serde_json::json!({"schema_version":1,"project_id":hash(format!("project-store\0{}",session["input"]["project_store"].as_str().unwrap()).as_bytes()),"store_incarnation":session["input"]["store_incarnation"],"request_id":name,"idempotency_key":name,"actor_id":name,"delegation":null,"expected_plan_revision":parent,"payload_digest":hash(&serde_json::to_vec(&payload).unwrap())});
        let propose=["--root",root_arg,"plan","propose","demo","--input-file",document.to_str().unwrap(),"--expected-plan-revision",&parent_arg,"--idempotency-key",&name];
        let head=migration::open_active(&project).unwrap().current_head().unwrap();
        for field in ["project_id","store_incarnation","payload_digest","actor_id","idempotency_key"] {
            let mut bad=raw.clone();bad["envelope"][field]=serde_json::json!("0".repeat(64));
            std::fs::write(&document,bad.to_string()).unwrap();assert!(!hp(home.path(),&propose).status.success(),"accepted wrong {field}");
            assert_eq!(migration::open_active(&project).unwrap().current_head().unwrap(),head);
        }
        if step>0 {
            // Correctly hashed but stale source identity must still be refused.
            let mut bad=raw.clone();bad["changes"][0]["expected"]["payload_digest"]=serde_json::json!("0".repeat(64));
            let mut payload=serde_json::json!({"contracts":bad["contracts"],"planner":bad["planner"],"changes":bad["changes"]});payload.sort_all_objects();
            bad["envelope"]["payload_digest"]=serde_json::json!(hash(&serde_json::to_vec(&payload).unwrap()));
            std::fs::write(&document,bad.to_string()).unwrap();assert!(!hp(home.path(),&propose).status.success());
            let mut reused=raw.clone();reused["envelope"]["request_id"]=serde_json::json!("typed-0");
            std::fs::write(&document,reused.to_string()).unwrap();assert!(!hp(home.path(),&propose).status.success());
            assert_eq!(migration::open_active(&project).unwrap().current_head().unwrap(),head);
        }
        std::fs::write(&document,raw.to_string()).unwrap();
        let accepted=hp(home.path(),&propose);assert!(accepted.status.success(),"{}",String::from_utf8_lossy(&accepted.stderr));
        previous=serde_json::from_slice(&accepted.stdout).unwrap();assert_eq!(previous["plan_revision"],parent+1);
        let head=migration::open_active(&project).unwrap().current_head().unwrap();
        let retry=hp(home.path(),&propose);assert!(retry.status.success(),"{}",String::from_utf8_lossy(&retry.stderr));
        let replay:serde_json::Value=serde_json::from_slice(&retry.stdout).unwrap();assert_eq!(replay["proposal_id"],previous["proposal_id"]);assert_eq!(replay["replayed"],true);
        assert_eq!(migration::open_active(&project).unwrap().current_head().unwrap(),head);
    }
    let inspected=hp(home.path(),&["--root",root_arg,"plan","inspect","demo"]);assert!(inspected.status.success());
    let page:serde_json::Value=serde_json::from_slice(&inspected.stdout).unwrap();
    let cancelled=page["entries"].as_array().unwrap().iter().find(|v|v["task_id"]=="typed-task").unwrap();
    assert_eq!(cancelled["text"],"Revised intent");assert_eq!(cancelled["cancellation_reason"],"Requirements withdrawn");assert_eq!(cancelled["dependencies"][0]["predecessor"],"planned");
    assert_eq!(cancelled["source_plan_revision"],6);assert_eq!(cancelled["proposal_id"],previous["proposal_id"]);assert_eq!(cancelled["payload_digest"],previous["digest"]);
    let page1=hp(home.path(),&["--root",root_arg,"plan","inspect","demo","--limit","2"]);assert!(page1.status.success());
    let page1:serde_json::Value=serde_json::from_slice(&page1.stdout).unwrap();assert_eq!(page1["next_after"],"typed-task");assert_eq!(page1["entries"][0]["task_id"],"planned");assert_eq!(page1["entries"][0]["source_plan_revision"],1);assert_eq!(page1["entries"][1],*cancelled);
    let page2=hp(home.path(),&["--root",root_arg,"plan","inspect","demo","--limit","2","--after","typed-task","--expected-plan-revision","6"]);assert!(page2.status.success());
    let page2:serde_json::Value=serde_json::from_slice(&page2.stdout).unwrap();assert_eq!(page2["entries"][0]["task_id"],"zz-next");assert!(page2["next_after"].is_null());
    assert_eq!(authority_tables.iter().map(|t|count(t)).collect::<Vec<_>>(),authority_before);
    assert_eq!(count("plan_proposals"),6);assert_eq!(count("plan_revisions"),6);assert_eq!(count("plan_proposal_requests"),4);
    std::fs::write(&intent,"different input").unwrap();assert!(!hp(home.path(),&args).status.success());
    let after=migration::open_active(&project).unwrap().read_snapshot(None).unwrap();
    assert_eq!(before.tasks,after.tasks);assert_eq!(before.attempts,after.attempts);assert_eq!(before.control,after.control);
}

#[test]
#[cfg(feature="state-store")]
fn barrier_revoke_cli_records_refusal_without_releasing_capacity() {
    use herdr_projects::migration;
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let plan=migration::inspect_with_config(&project,&home.path().join(".config/herdr-projects/config.toml")).unwrap();migration::apply(&project,&plan,true).unwrap();
    let before=migration::open_active(&project).unwrap().read_snapshot(None).unwrap();
    let missing=hp(home.path(),&["--root",root_arg,"memory","demo","barrier","--id",&"a".repeat(64)]);
    assert!(missing.status.success(),"{}",String::from_utf8_lossy(&missing.stderr));
    assert!(serde_json::from_slice::<serde_json::Value>(&missing.stdout).unwrap().is_null());
    let input=home.path().join("members.json");
    for (raw,message) in [("[]".to_owned(),"membership is empty"),(format!("[{}null]","{},".repeat(200_000)),"input/structure accounting")] {
        std::fs::write(&input,raw).unwrap();
        let refused=hp(home.path(),&["--root",root_arg,"memory","demo","barrier-freeze","--input",input.to_str().unwrap(),"--expected-head",&before.head.to_string()]);
        assert!(!refused.status.success());assert!(String::from_utf8_lossy(&refused.stderr).contains(message),"{}",String::from_utf8_lossy(&refused.stderr));
    }
    let result=hp(home.path(),&["--root",root_arg,"memory","demo","barrier-revoke","--id",&"a".repeat(64),"--expected-head",&before.head.to_string(),"--reason","operator withdrawal"]);
    assert!(!result.status.success());assert!(String::from_utf8_lossy(&result.stderr).contains("barrier is not stored"));
    let mut db=migration::open_active(&project).unwrap();let denials=db.authority_denials().unwrap();
    assert_eq!(denials.len(),1);assert_eq!(denials[0].command,"barrier-revoke");assert_eq!(denials[0].actual_head,Some(before.head));
    let after=db.read_snapshot(None).unwrap();assert_eq!(after.head,before.head);assert_eq!(after.attempts,before.attempts);assert_eq!(after.control,before.control);
}

#[test]
#[cfg(feature="state-store")]
fn auto_replan_cli_requires_current_head_and_preserves_execution_state() {
    use herdr_projects::migration;
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let plan=migration::inspect_with_config(&project,&home.path().join(".config/herdr-projects/config.toml")).unwrap();migration::apply(&project,&plan,true).unwrap();
    let before=migration::open_active(&project).unwrap().read_snapshot(None).unwrap();
    let on=hp(home.path(),&["--root",root_arg,"plan","auto-replan","demo","on","--expected-head",&before.head.to_string()]);
    assert!(on.status.success(),"{}",String::from_utf8_lossy(&on.stderr));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&on.stdout).unwrap()["enabled"],true);
    let stale=hp(home.path(),&["--root",root_arg,"plan","auto-replan","demo","off","--expected-head",&before.head.to_string()]);assert!(!stale.status.success());
    let head=migration::open_active(&project).unwrap().current_head().unwrap();
    let off=hp(home.path(),&["--root",root_arg,"plan","auto-replan","demo","off","--expected-head",&head.to_string()]);
    assert!(off.status.success(),"{}",String::from_utf8_lossy(&off.stderr));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&off.stdout).unwrap()["enabled"],false);
    let after=migration::open_active(&project).unwrap().read_snapshot(None).unwrap();
    assert_eq!(after.attempts,before.attempts);assert_eq!(after.tasks,before.tasks);assert_eq!(after.control,before.control);
}

#[test]
#[cfg(feature="state-store")]
fn recovery_wait_cli_preserves_owned_resources() {
    use herdr_projects::{domain::*,migration,reconcile::{RuntimeObservation,ResourceState}};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let plan=migration::inspect_with_config(&project,&home.path().join(".config/herdr-projects/config.toml")).unwrap();migration::apply(&project,&plan,true).unwrap();
    let mut db=migration::open_active(&project).unwrap();
    db.commit(Commit{expected_head:db.current_head().unwrap(),mutations:vec![Mutation::Task{expected:None,next:Task{id:TaskId::new("waiting-adapter").unwrap(),revision:1,state:TaskState::Blocked,title:"waiting".into(),active_attempt:None}}]}).unwrap();
    let route=RuntimeRoute{socket:"/tmp/fixture-recovery.sock".into(),workspace_id:"w".into(),tab_id:"t".into(),pane_id:"p".into(),cwd:"/tmp".into(),..Default::default()};
    let snapshot=db.read_snapshot(None).unwrap();
    let binding=if let Some(binding)=snapshot.runtime_bindings.iter().find(|binding|binding.id=="coordinator") {
        db.rebind_runtime(&binding.id,binding.revision,snapshot.head,&route).unwrap().binding
    }else {db.create_runtime(None,None,snapshot.head,&route).unwrap().binding};
    let snapshot=db.read_snapshot(None).unwrap();let now=jiff::Timestamp::now().as_millisecond();
    let observations:Vec<_>=snapshot.runtime_bindings.iter().map(|entry|RuntimeObservation{
        binding:entry.id.clone(),binding_revision:entry.revision,task_revision:entry.task.as_ref().map(|id|snapshot.tasks.iter().find(|task|&task.id==id).unwrap().revision),
        observed_unix_ms:now,collector:"herdr-git-v2".into(),..Default::default()}).map(|mut observation|{
            if observation.binding==binding.id {observation.pane=ResourceState::Present;observation.agent_present=true;observation.session_identity=Some(ResourceIdentity{device:1,inode:2,born_secs:3,born_nanos:0});observation.agent_identity=Some(AgentIdentity{kind:"fixture".into(),name:"fixture-agent".into()});}observation
        }).collect();
    db.record_observations(snapshot.head,&observations).unwrap();
    let owned=db.adopt_runtime(&binding.id,binding.revision,db.current_head().unwrap(),now,None).unwrap().ownership;
    let before=db.read_snapshot(None).unwrap();drop(db);
    let registered=hp(home.path(),&["--root",root_arg,"plan","wait","demo","register","--task","waiting-adapter","--condition","adapter_recovery","--recovery-binding",&binding.id,"--recovery-binding-revision",&binding.revision.to_string(),"--recovery-ownership-revision",&owned.revision.to_string()]);
    assert!(registered.status.success(),"{}",String::from_utf8_lossy(&registered.stderr));
    let registered:serde_json::Value=serde_json::from_slice(&registered.stdout).unwrap();
    let replay=hp(home.path(),&["--root",root_arg,"plan","wait","demo","replay",registered["wait_id"].as_str().unwrap()]);
    assert!(replay.status.success(),"{}",String::from_utf8_lossy(&replay.stderr));
    let replay:serde_json::Value=serde_json::from_slice(&replay.stdout).unwrap();assert_eq!(replay["wake_requested"],true);assert_eq!(replay["proved"],false);
    assert!(!hp(home.path(),&["--root",root_arg,"plan","wait","demo","register","--task","waiting-adapter","--condition","adapter_recovery","--recovery-binding",&binding.id]).status.success());
    let after=migration::open_active(&project).unwrap().read_snapshot(None).unwrap();
    assert_eq!(after.ownership,before.ownership);assert_eq!(after.attempts,before.attempts);assert_eq!(after.control,before.control);
}

#[test]
#[cfg(feature="state-store")]
fn operation_expiry_is_visible_idempotent_and_never_dispatches() {
    use herdr_projects::{migration,operations::{Outcome,DeliveryState}};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");
    std::fs::write(project.join(".state/ticker.json"),br#"{"notification_retry":{"hash":"fixture-hash"}}"#).unwrap();
    let plan=migration::inspect_with_config(&project,&home.path().join(".config/herdr-projects/config.toml")).unwrap();migration::apply(&project,&plan,true).unwrap();
    let mut db=migration::open_active(&project).unwrap();let imported=db.deliveries().unwrap().remove(0);
    let pending=db.observe_operation(&imported.operation,imported.revision,"test-fixture",Outcome::Retryable{no_effect_evidence:"disposable fake effect never attempted".into()},0).unwrap();
    db.claim_operation(&pending.operation,pending.revision,"crashed-fixture",pending.next_due_ms,1).unwrap();drop(db);
    for expected in ["1 expired","0 expired"] {
        let out=hp(home.path(),&["--root",root_arg,"operations","demo","expire"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert!(String::from_utf8_lossy(&out.stdout).contains(expected));
    }
    assert_eq!(migration::open_active(&project).unwrap().deliveries().unwrap()[0].state,DeliveryState::Ambiguous);
}

#[test]
#[cfg(feature="state-store")]
fn imported_receipt_cli_previews_and_confirms_only_matching_completion() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let bytes=br#"{"nudged":"fixture-hash","notification_retry":{"hash":"fixture-hash"}}"#;
    std::fs::write(project.join(".state/ticker.json"),bytes).unwrap();
    let plan=herdr_projects::migration::inspect(&project).unwrap();herdr_projects::migration::apply(&project,&plan,true).unwrap();
    let out=hp(home.path(),&["--root",root_arg,"operations","demo","receipt-plan"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let report:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(report["confirmed"],0);assert!(report["observations"][0]["receipt"].is_string());let head=report["head"].to_string();
    let args=["--root",root_arg,"operations","demo","observe-imported","--expected-head",&head];
    let out=hp(home.path(),&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert_eq!(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["confirmed"],1);
    assert!(!hp(home.path(),&args).status.success());assert_eq!(std::fs::read(project.join(".state/ticker.json")).unwrap(),bytes);
}

#[test]
#[cfg(feature="state-store")]
fn migrated_runtime_bindings_require_explicit_upgrade_and_are_unverified() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");std::fs::write(project.join("threads/t-0001.toml"),"id='t-0001'\nstatus='resolved'\nrepo='/repo'\n").unwrap();
    let plan=herdr_projects::migration::inspect(&project).unwrap();herdr_projects::migration::apply(&project,&plan,true).unwrap();
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();test_schema::historical(&raw, 4).unwrap();drop(raw);
    let args=["--root",root_arg,"migration","demo","bindings"];
    let out=hp(home.path(),&args);assert!(!out.status.success());assert!(String::from_utf8_lossy(&out.stderr).contains("upgrade-store"));
    assert!(hp(home.path(),&["--root",root_arg,"migration","demo","upgrade-store"]).status.success());
    let out=hp(home.path(),&args);assert!(out.status.success());let view:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(view["bindings"][0]["verification"],"unverified");assert_eq!(view["bindings"][0]["identity"]["repo"],"/repo");
    assert!(!hp(home.path(),&["--root",root_arg,"resume","demo"]).status.success());
}

#[test]
#[cfg(feature="state-store")]
fn reconciliation_cli_records_unrecorded_identity_without_authorizing_execution() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");std::fs::write(project.join("threads/t-1.toml"),"id='t-1'\nstatus='resolved'\n").unwrap();
    let plan=herdr_projects::migration::inspect(&project).unwrap();herdr_projects::migration::apply(&project,&plan,true).unwrap();
    let before=herdr_projects::runtime::snapshot(&project).unwrap();
    let out=hp(home.path(),&["--root",root_arg,"reconcile","demo"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert_eq!(herdr_projects::runtime::snapshot(&project).unwrap(),before);
    let out=hp(home.path(),&["--root",root_arg,"reconcile","demo","--record"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let report:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(report["dispatch_allowed"],false);assert!(report["recorded_head"].is_number());assert_eq!(report["observations"][0]["pane"],"unrecorded");
    let after=herdr_projects::runtime::snapshot(&project).unwrap();assert_eq!(after.tasks,before.tasks);assert_eq!(after.observations.len(),1);assert!(!hp(home.path(),&["--root",root_arg,"resume","demo"]).status.success());
}

#[test]
#[cfg(feature="state-store")]
fn runtime_rebind_cli_requires_revisions_and_retains_legacy_bytes() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let bytes=b"id='t-1'\nstatus='resolved'\n";std::fs::write(project.join("threads/t-1.toml"),bytes).unwrap();let plan=herdr_projects::migration::inspect(&project).unwrap();herdr_projects::migration::apply(&project,&plan,true).unwrap();
    let out=hp(home.path(),&["--root",root_arg,"runtime","demo","inspect"]);assert!(out.status.success());let view:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();let head=view["head"].to_string();
    let path=home.path().join("route.json");std::fs::write(&path,br#"{"socket":"/recorded.sock","workspace_id":"w","tab_id":"t","pane_id":"p","cwd":"/cwd"}"#).unwrap();
    let args=["--root",root_arg,"runtime","demo","rebind","thread:t-1","--route",path.to_str().unwrap(),"--expected-revision","1","--expected-head",&head];
    let out=hp(home.path(),&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let result:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(result["binding"]["revision"],2);assert_eq!(result["binding"]["verification"],"unverified");assert!(!hp(home.path(),&args).status.success());assert_eq!(std::fs::read(project.join("threads/t-1.toml")).unwrap(),bytes);
}

#[test]
#[cfg(feature="state-store")]
fn canonical_lifecycle_cli_does_not_dual_write_legacy_status() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let legacy=std::fs::read(project.join(".state/project.json")).unwrap();let plan=herdr_projects::migration::inspect(&project).unwrap();herdr_projects::migration::apply(&project,&plan,true).unwrap();
    for state in ["active","paused","archived"] {
        if state=="paused" {let config=home.path().join(".config/herdr-projects/config.toml");std::fs::create_dir_all(config.parent().unwrap()).unwrap();std::fs::write(config,"invalid config [").unwrap();}
        let snapshot=herdr_projects::runtime::snapshot(&project).unwrap();let head=snapshot.head.to_string();let revision=snapshot.control.as_ref().unwrap().revision.to_string();
        let out=hp(home.path(),&["--root",root_arg,"runtime","demo","state",state,"--expected-head",&head,"--expected-revision",&revision]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let result:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(result["control"]["state"],state);
    }
    assert_eq!(std::fs::read(project.join(".state/project.json")).unwrap(),legacy);assert!(hp(home.path(),&["--root",root_arg,"migration","demo","recover","--writers-stopped"]).status.success());
}

#[test]
#[cfg(feature="state-store")]
fn canonical_runtime_create_cli_requires_task_fences_and_does_not_forge_legacy_files() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let plan=herdr_projects::migration::inspect(&project).unwrap();herdr_projects::migration::apply(&project,&plan,true).unwrap();let task=herdr_projects::domain::TaskId::new("created").unwrap();let before=herdr_projects::runtime::snapshot(&project).unwrap();let head=herdr_projects::runtime::add_task(&project,task,"created task".into(),before.head).unwrap().to_string();
    let route=home.path().join("route.json");std::fs::write(&route,b"{}").unwrap();
    let args=["--root",root_arg,"runtime","demo","create","--task","created","--task-revision","1","--expected-head",&head,"--route",route.to_str().unwrap()];let out=hp(home.path(),&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let result:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(result["binding"]["id"],"task:created");assert!(result["binding"]["source_path"].is_null());assert_eq!(result["task_revision"],2);assert!(!hp(home.path(),&args).status.success());assert!(!project.join("threads/created.toml").exists());
    let head=result["head"].to_string();let out=hp(home.path(),&["--root",root_arg,"runtime","demo","create","--expected-head",&head,"--route",route.to_str().unwrap()]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert_eq!(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["binding"]["id"],"coordinator");assert!(!project.join(".state/coordinator.json").exists());
}

#[test]
#[cfg(feature="state-store")]
fn canonical_notification_cli_delivers_once_to_recorded_socket() {
    use std::os::unix::fs::PermissionsExt;
    use herdr_projects::{domain::{TaskId,RuntimeRoute,ProjectState},migration,runtime};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");std::fs::write(project.join("inbox/message.md"),"+++\nid='message'\nsummary='private text'\n+++\n").unwrap();let plan=migration::inspect(&project).unwrap();migration::apply(&project,&plan,true).unwrap();
    let head=runtime::snapshot(&project).unwrap().head;let task=TaskId::new("notification").unwrap();let head=runtime::add_task(&project,task.clone(),"notification".into(),head).unwrap();runtime::create_binding(&project,None,None,head,&RuntimeRoute{socket:"/explicit/notification.sock".into(),..Default::default()}).unwrap();assert!(hp(home.path(),&["--root",root_arg,"reconcile","demo","--record"]).status.success());let snapshot=runtime::snapshot(&project).unwrap();runtime::set_state(&project,snapshot.head,snapshot.control.unwrap().revision,ProjectState::Active,&home.path().join(".config/herdr-projects/config.toml")).unwrap();
    let head=runtime::snapshot(&project).unwrap().head.to_string();let out=hp(home.path(),&["--root",root_arg,"operations","demo","notify","notification","--expected-head",&head]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let op:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();let id=op["id"].as_str().unwrap();
    let fake=home.path().join(".local/bin/herdr");std::fs::create_dir_all(fake.parent().unwrap()).unwrap();std::fs::write(&fake,b"#!/bin/sh\nif [ \"$1\" = '--version' ]; then echo 'herdr 0.9.1'; exit 0; fi\n[ \"$HERDR_SOCKET_PATH\" = '/explicit/notification.sock' ] || exit 8\n[ \"$1\" = notification ] && [ \"$2\" = show ] || exit 9\nprintf 'effect\\n' >> \"$HOME/effects\"\nprintf '%s\\n' '{\"result\":{\"shown\":true}}'\n").unwrap();std::fs::set_permissions(&fake,std::fs::Permissions::from_mode(0o700)).unwrap();
    let deliver=|args:&[&str]|Command::new(BIN).env_clear().env("HOME",home.path()).env("HERDR_BIN_PATH",&fake).args(args).output().unwrap();
    let args=["--root",root_arg,"operations","demo","deliver-notification",id,"--expected-revision","1"];let out=deliver(&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert_eq!(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["state"],"confirmed");assert!(!deliver(&args).status.success());assert_eq!(std::fs::read_to_string(home.path().join("effects")).unwrap(),"effect\n");let snapshot=runtime::snapshot(&project).unwrap();assert!(!snapshot.inbox[0].seen&&!snapshot.inbox[0].done);
}

#[cfg(all(feature="state-store",target_os="linux"))]
#[test]
fn ticker_canonical_notification_confirms_or_retains_ambiguity_after_owner_death() {
    use std::{fs,os::unix::{fs::PermissionsExt,net::UnixListener},process::Stdio,time::{Duration,Instant}};
    use herdr_projects::{migration,runtime,domain::{TaskId,RuntimeRoute,ProjectState},operations::DeliveryState,execution_guard::{ProjectGuard,RootGuard}};
    for mode in ["ok","lost","death"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();for action in ["new","pause"]{assert!(hp(home.path(),&["--root",r,action,"demo"]).status.success());}
        let project=root.join("demo");fs::write(project.join("inbox/message.md"),"+++\nid='message'\nsummary='private text'\n+++\n").unwrap();let plan=migration::inspect(&project).unwrap();migration::apply(&project,&plan,true).unwrap();
        let task=TaskId::new("notification").unwrap();let head=runtime::add_task(&project,task.clone(),"notification".into(),runtime::snapshot(&project).unwrap().head).unwrap();let socket=home.path().join("notification.sock");let _listener=UnixListener::bind(&socket).unwrap();runtime::create_binding(&project,None,None,head,&RuntimeRoute{socket:socket.display().to_string(),..Default::default()}).unwrap();
        assert!(hp(home.path(),&["--root",r,"reconcile","demo","--record"]).status.success());let snapshot=runtime::snapshot(&project).unwrap();let config=home.path().join(".config/herdr-projects/config.toml");runtime::set_state(&project,snapshot.head,snapshot.control.unwrap().revision,ProjectState::Active,&config).unwrap();
        let op=runtime::enqueue_notification(&project,&task,runtime::snapshot(&project).unwrap().head,&migration::config_reference(&config).unwrap()).unwrap();
        let helper=home.path().join("herdr");fs::write(&helper,format!(r#"#!/usr/bin/python3
import sys,json,pathlib,os,time,fcntl,sqlite3
root=pathlib.Path({home:?});project=pathlib.Path({project:?});mode={mode:?}
if sys.argv[1:]==['remote-api-bridge','--check']:print('herdr-api-bridge-v1');sys.exit(0)
assert sys.argv[1:]==['remote-api-bridge']
request=json.loads(sys.stdin.readline());assert request['method']=='notification.show'
for lock in [project.parent/'.execution.lock',project/'.state'/'effect.lock',project.parent/'.routine-execution.lock']:
 with open(lock,'r+') as f:
  try:fcntl.flock(f,fcntl.LOCK_EX|fcntl.LOCK_NB)
  except BlockingIOError:pass
  else:(root/'WRONG_LOCK').write_text(str(lock));sys.exit(5)
db=sqlite3.connect(project/'.state'/'state.db');assert db.execute("select count(*) from operation_delivery where state='claimed'").fetchone()[0]==1
if mode=='death' and os.fork()==0:
 os.setsid();os.closerange(3,1024);time.sleep(15);(root/'escaped').write_text('bad');os._exit(0)
with open(root/'sent','a') as f:f.write('send\n')
if mode=='death':time.sleep(60)
if mode=='lost':sys.exit(0)
print(json.dumps({{'id':request['id'],'result':{{'type':'notification_show','shown':True,'reason':'shown'}}}}))
"#,home=home.path().display().to_string(),project=project.display().to_string())).unwrap();fs::set_permissions(&helper,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child{fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&helper).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let read=||runtime::snapshot(&project).unwrap().deliveries.into_iter().find(|d|d.operation==op.id).unwrap();
        let wait=|child:&mut Child,predicate:&dyn Fn()->bool|{let end=Instant::now()+Duration::from_secs(8);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
        let stop=|child:&mut Child|{fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
        let mut child=spawn();wait(&mut child,&||home.path().join("sent").exists());
        if mode=="death" {
            child.0.kill().unwrap();child.0.wait().unwrap();assert!(RootGuard::exclusive(&root).is_err());assert!(ProjectGuard::acquire(&project).is_err());
            let routine=fs::OpenOptions::new().write(true).open(root.join(".routine-execution.lock")).unwrap();assert!(routine.try_lock().is_err());
            let end=Instant::now()+Duration::from_secs(14);loop{if let Ok(guard)=RootGuard::exclusive(&root){drop(guard);break;}assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}
            routine.try_lock().unwrap();routine.unlock().unwrap();assert!(ProjectGuard::acquire(&project).is_ok());assert_eq!(read().state,DeliveryState::Claimed);
            let until=read().lease_until_ms.unwrap();assert_eq!(migration::open_active(&project).unwrap().expire_claims(until).unwrap(),1);assert_eq!(read().state,DeliveryState::Ambiguous);
            std::thread::sleep(Duration::from_secs(6));assert!(!home.path().join("escaped").exists());
        }else{wait(&mut child,&||read().state==if mode=="ok"{DeliveryState::Confirmed}else{DeliveryState::Ambiguous});stop(&mut child);}
        assert!(!home.path().join("WRONG_LOCK").exists());let head=runtime::snapshot(&project).unwrap().head;
        let mut child=spawn();wait(&mut child,&||runtime::snapshot(&project).unwrap().head>head);stop(&mut child);
        assert_eq!(fs::read_to_string(home.path().join("sent")).unwrap(),"send\n");assert_eq!(read().attempts,1);
    }
}

#[test]
#[cfg(feature="state-store")]
fn canonical_finalization_cli_preserves_artifacts_and_awaits_review() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let source=home.path().join("source");std::fs::create_dir_all(source.join("library")).unwrap();std::fs::write(source.join("report.md"),"report for review\n").unwrap();std::fs::write(source.join("library/result"),"result bytes").unwrap();let original=format!("id='t-0001'\nstatus='resolved'\nthread_dir={}\n",serde_json::to_string(source.to_str().unwrap()).unwrap());std::fs::write(project.join("threads/t-0001.toml"),&original).unwrap();let plan=herdr_projects::migration::inspect(&project).unwrap();herdr_projects::migration::apply(&project,&plan,true).unwrap();let head=herdr_projects::runtime::snapshot(&project).unwrap().head.to_string();
    let out=hp(home.path(),&["--root",root_arg,"operations","demo","finalize","thread:t-0001","--reason","operator review","--expected-head",&head]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let op:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();let id=op["id"].as_str().unwrap();let args=["--root",root_arg,"operations","demo","deliver-finalization",id,"--expected-revision","1"];let out=hp(home.path(),&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert_eq!(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["state"],"confirmed");assert!(!hp(home.path(),&args).status.success());let snapshot=herdr_projects::runtime::snapshot(&project).unwrap();let task=snapshot.tasks.iter().find(|t|t.id.as_str()=="legacy-t-0001").unwrap();assert_eq!(task.state,herdr_projects::domain::TaskState::AwaitingReview);assert_eq!(task.revision,2);assert_eq!(std::fs::read_to_string(project.join("threads/t-0001.toml")).unwrap(),original);assert!(source.join("report.md").is_file());
}

#[test]
#[cfg(feature="state-store")]
fn canonical_ownership_cli_adopts_recorded_coordinator_without_prompting() {
    use std::os::unix::{fs::PermissionsExt,net::UnixListener};use herdr_projects::{migration,runtime,domain::RuntimeRoute};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let plan=migration::inspect(&project).unwrap();migration::apply(&project,&plan,true).unwrap();let socket=home.path().join("fixture.sock");let _listener=UnixListener::bind(&socket).unwrap();let head=runtime::snapshot(&project).unwrap().head;runtime::create_binding(&project,None,None,head,&RuntimeRoute{socket:socket.to_str().unwrap().into(),workspace_id:"w".into(),tab_id:"t".into(),pane_id:"p".into(),cwd:project.to_str().unwrap().into(),..Default::default()}).unwrap();
    std::fs::write(home.path().join("panes.json"),serde_json::json!({"result":{"panes":[{"pane_id":"p","tab_id":"t","workspace_id":"w","cwd":project}]}}).to_string()).unwrap();std::fs::write(home.path().join("agents.json"),serde_json::json!({"result":{"agents":[{"pane_id":"p","tab_id":"t","workspace_id":"w","cwd":project,"agent":"claude","name":"coordinator","agent_status":"working"}]}}).to_string()).unwrap();let fake=home.path().join("fake-herdr");std::fs::write(&fake,b"#!/bin/sh\ncase \"$1 $2\" in\n'--version ') echo 'herdr 0.9.1';;\n'pane list') cat \"$HOME/panes.json\";;\n'agent list') cat \"$HOME/agents.json\";;\n*) exit 99;;\nesac\n").unwrap();std::fs::set_permissions(&fake,std::fs::Permissions::from_mode(0o700)).unwrap();let command=|args:&[&str]|Command::new(BIN).env_clear().env("HOME",home.path()).env("HERDR_BIN_PATH",&fake).args(args).output().unwrap();
    let before=runtime::snapshot(&project).unwrap();let out=command(&["--root",root_arg,"reconcile","demo","--plan"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let plan:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(plan["dispatch_allowed"],false);assert!(plan["items"].as_array().unwrap().iter().any(|i|i["action"]=="adopt_resources"));assert_eq!(runtime::snapshot(&project).unwrap(),before);assert!(!command(&["--root",root_arg,"reconcile","demo","--plan","--record"]).status.success());
    let head=runtime::snapshot(&project).unwrap().head.to_string();let out=command(&["--root",root_arg,"runtime","demo","adopt","coordinator","--expected-revision","1","--expected-head",&head]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let change:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(change["ownership"]["origin"],"adopted");assert!(change["ownership"]["attempt"].is_null());assert!(command(&["--root",root_arg,"reconcile","demo","--record"]).status.success());let snapshot=runtime::snapshot(&project).unwrap();let out=command(&["--root",root_arg,"runtime","demo","state","active","--expected-head",&snapshot.head.to_string(),"--expected-revision",&snapshot.control.unwrap().revision.to_string()]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert!(runtime::snapshot(&project).unwrap().attempts.is_empty());
    let before=runtime::snapshot(&project).unwrap();let out=command(&["--root",root_arg,"runtime","demo","relinquish","coordinator","--expected-revision","1","--expected-head",&before.head.to_string(),"--reason","hand back"]);assert!(!out.status.success());assert_eq!(runtime::snapshot(&project).unwrap(),before);
    runtime::set_state(&project,before.head,before.control.unwrap().revision,herdr_projects::domain::ProjectState::Paused,&home.path().join("config.toml")).unwrap();let head=runtime::snapshot(&project).unwrap().head;let out=command(&["--root",root_arg,"runtime","demo","relinquish","coordinator","--expected-revision","1","--expected-head",&head.to_string(),"--reason","hand back"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert!(runtime::snapshot(&project).unwrap().ownership.is_empty());assert!(socket.exists());

}

#[cfg(feature="state-store")]
#[test]
fn scheduler_cli_queues_dependencies_without_launching_or_rewriting_legacy_tasks() {
    use herdr_projects::{migration,runtime,domain::TaskId};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let original=std::fs::read(project.join("TASKS.md")).unwrap();let plan=migration::inspect(&project).unwrap();migration::apply(&project,&plan,true).unwrap();for id in ["a","b"] {runtime::add_task(&project,TaskId::new(id).unwrap(),id.into(),runtime::snapshot(&project).unwrap().head).unwrap();}
    let request=home.path().join("queue.json");std::fs::write(&request,r#"{"priority":2,"dependencies":[{"predecessor":"b","requirement":"landed_commit"}]}"#).unwrap();let before=runtime::snapshot(&project).unwrap();let out=hp(home.path(),&["--root",root_arg,"task","demo","queue","a","--input-file",request.to_str().unwrap(),"--expected-revision","1","--expected-head",&before.head.to_string()]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let head=runtime::snapshot(&project).unwrap().head;let out=hp(home.path(),&["--root",root_arg,"scheduler","demo","policy","--max-active-workers","2","--max-attempts-per-task","3","--expected-revision","1","--expected-head",&head.to_string()]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let out=hp(home.path(),&["--root",root_arg,"scheduler","demo","inspect"]);assert!(out.status.success());let report:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(report["available_slots"],2);assert_eq!(report["launch_enabled"],false);assert_eq!(report["capability"]["prepared_dispatch"],true);assert_eq!(report["capability"]["automatic_admission"],false);assert_eq!(report["capability"]["dependency_producers"],false);assert_eq!(report["capability"]["integration"],if cfg!(target_os="linux") {"operator_local"} else {"unavailable"});assert_eq!(report["capability"]["blockers"],serde_json::json!(["automatic_admission_does_not_draft_sign_or_reserve"]));let blockers=report["entries"][0]["blockers"].as_array().unwrap();for stage in ["owner_signature_not_scheduled","launch_reserve_not_scheduled","controller_requires_reserved_attempt"]{assert!(blockers.iter().any(|s|s==stage));}assert!(blockers.iter().all(|s|s!="launch_draft_not_scheduled"));assert!(blockers.iter().any(|s|s.as_str().unwrap().contains("verified_dependency_evidence_unavailable:b:landed_commit")));assert!(blockers.iter().chain(report["capability"]["blockers"].as_array().unwrap()).all(|s|{let s=s.as_str().unwrap();!s.contains("launch_preparation_unavailable")&&!s.contains("verifier")&&!s.contains("integrator")&&!s.contains("producer")&&!s.contains("satisfaction")}));
    std::fs::write(&request,r#"{"priority":0,"dependencies":[{"predecessor":"a","requirement":"verified_result"}]}"#).unwrap();let before=runtime::snapshot(&project).unwrap();let out=hp(home.path(),&["--root",root_arg,"task","demo","queue","b","--input-file",request.to_str().unwrap(),"--expected-revision","1","--expected-head",&before.head.to_string()]);assert!(!out.status.success());assert_eq!(runtime::snapshot(&project).unwrap(),before);assert!(before.attempts.is_empty());assert!(before.operations.is_empty());assert_eq!(std::fs::read(project.join("TASKS.md")).unwrap(),original);
    // Queue mutation and policy changes must not decode unrelated cold history.
    runtime::add_task(&project, TaskId::new("cold-history").unwrap(), "retired history".into(), runtime::snapshot(&project).unwrap().head).unwrap();
    let raw = rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
    raw.execute_batch("UPDATE tasks SET title=CAST(x'ff' AS TEXT) WHERE id='cold-history'; INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES('cold-attempt','cold-history',1,'completed',CAST(x'ff' AS TEXT),'historical-slot',1);").unwrap();
    let head = || raw.query_row("SELECT MAX(sequence) FROM events",[],|r|r.get::<_,u64>(0)).unwrap();
    std::fs::write(&request,r#"{"priority":0,"dependencies":[]}"#).unwrap();
    let queued = hp(home.path(), &["--root",root_arg,"task","demo","queue","b","--input-file",request.to_str().unwrap(),"--expected-revision","1","--expected-head",&head().to_string()]);
    assert!(queued.status.success(), "cold history blocked queue mutation: {}", String::from_utf8_lossy(&queued.stderr));
    let policy = hp(home.path(), &["--root",root_arg,"scheduler","demo","policy","--max-active-workers","3","--max-attempts-per-task","3","--expected-revision","2","--expected-head",&head().to_string()]);
    assert!(policy.status.success(), "cold history blocked policy update: {}", String::from_utf8_lossy(&policy.stderr));
    let before_cycle = head();
    std::fs::write(&request,r#"{"priority":0,"dependencies":[{"predecessor":"a","requirement":"verified_result"}]}"#).unwrap();
    let cycle = hp(home.path(), &["--root",root_arg,"task","demo","queue","b","--input-file",request.to_str().unwrap(),"--expected-revision","2","--expected-head",&before_cycle.to_string()]);
    assert!(!cycle.status.success()); assert_eq!(head(),before_cycle);
    assert!(String::from_utf8_lossy(&cycle.stderr).contains("dependency cycle"));
    assert_eq!(raw.query_row("SELECT hex(CAST(title AS BLOB)) FROM tasks WHERE id='cold-history'",[],|r|r.get::<_,String>(0)).unwrap(),"FF");
    assert_eq!(raw.query_row("SELECT hex(CAST(snapshot AS BLOB)) FROM attempts WHERE id='cold-attempt'",[],|r|r.get::<_,String>(0)).unwrap(),"FF");
    assert_eq!(raw.query_row("SELECT count(*) FROM attempts",[],|r|r.get::<_,u64>(0)).unwrap(),1);
    assert_eq!(raw.query_row("SELECT count(*) FROM attempts WHERE termination_observed=0",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    let before_inspect = head();
    let inspected = hp(home.path(), &["--root",root_arg,"scheduler","demo","inspect"]);
    assert!(inspected.status.success(), "cold history blocked scheduler inspection: {}", String::from_utf8_lossy(&inspected.stderr));
    let inspected: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
    assert_eq!(inspected["entries"].as_array().unwrap().len(),2);
    assert_eq!(inspected["retained_attempts"],0); assert_eq!(inspected["available_slots"],3);
    assert_eq!(head(),before_inspect);
    assert_eq!(std::fs::read(project.join("TASKS.md")).unwrap(),original);

}

#[cfg(feature="state-store")]
#[test]
fn cancellation_cli_audits_request_without_releasing_an_unproven_worker() {
    use herdr_projects::{migration,runtime,domain::*};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let plan=migration::inspect(&project).unwrap();migration::apply(&project,&plan,true).unwrap();let head=runtime::snapshot(&project).unwrap().head;runtime::add_task(&project,TaskId::new("a").unwrap(),"task".into(),head).unwrap();let head=runtime::snapshot(&project).unwrap().head;let mut db=migration::open_active(&project).unwrap();db.commit(Commit{expected_head:head,mutations:vec![Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new("adopted").unwrap(),task:TaskId::new("a").unwrap(),revision:1,state:AttemptState::Lost,snapshot:None,reservation:"fixture".into(),termination_observed:false}}]}).unwrap();drop(db);
    let before=runtime::snapshot(&project).unwrap();let args=["--root",root_arg,"task","demo","cancel-attempt","adopted","--expected-revision","1","--expected-head",&before.head.to_string(),"--reason","operator stop request"];let output=hp(home.path(),&args);assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));let result:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();assert_eq!(result["released"],false);let after=runtime::snapshot(&project).unwrap();assert!(after.attempts[0].retains_capacity());assert_eq!(after.cancellations.len(),1);assert!(!hp(home.path(),&args).status.success());assert_eq!(runtime::snapshot(&project).unwrap(),after);
}

#[cfg(target_os="linux")]
#[test]
fn ticker_native_copy_publishes_announces_and_does_not_recopy_after_restart() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::fs::PermissionsExt};
    use sha2::{Digest,Sha256};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
    assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
    let source=home.path().join("source '$λ");fs::create_dir_all(source.join("library/empty")).unwrap();
    fs::write(source.join("report.md"),b"new report\0\xff").unwrap();fs::write(source.join("library/item"),b"binary\0\xff").unwrap();
    let hash=format!("{:x}",Sha256::digest(b"new report\0\xff"));
    let socket=home.path().join("session.sock");let _listener=std::os::unix::net::UnixListener::bind(&socket).unwrap();
    fs::write(project.join(".state/coordinator.json"),serde_json::to_vec(&serde_json::json!({"socket":socket,"workspace_id":"w1","tab_id":"w1:t1","pane_id":"w1:p1","agent_name":"coordinator","cwd":project})).unwrap()).unwrap();
    let record=project.join("threads/t-0001.toml");
    fs::write(&record,toml::to_string(&serde_json::json!({"id":"t-0001","status":"open","kind":"adopted","thread_dir":source,"cwd":source,"workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1","agent":"claude","agent_name":"worker","title":"Fixture","created":jiff::Timestamp::now().to_string()})).unwrap()).unwrap();
    fs::write(home.path().join("agents.json"),serde_json::to_vec(&serde_json::json!({"result":{"agents":[{"workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1","cwd":source,"name":"worker","agent":"claude","agent_status":"idle"}]}})).unwrap()).unwrap();
    fs::write(home.path().join("panes.json"),serde_json::to_vec(&serde_json::json!({"result":{"panes":[{"workspace_id":"w1","tab_id":"w1:t1","pane_id":"w1:p1","cwd":project},{"workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1","cwd":source}]}})).unwrap()).unwrap();
    let fake=home.path().join("herdr");fs::write(&fake,r#"#!/usr/bin/python3
import os,sys,json,pathlib,time,fcntl
root=pathlib.Path(os.environ['HOME']);args=sys.argv[1:]
if args in [['agent','list'],['pane','list']]:
 with open(root/'root/.execution.lock','a') as lock:
  end=time.monotonic()+1
  while True:
   try:fcntl.flock(lock,fcntl.LOCK_SH|fcntl.LOCK_NB);break
   except BlockingIOError:
    if time.monotonic()>end:(root/'WRONG_SYNC_OBSERVATION').touch();sys.exit(2)
    time.sleep(.01)
 if args==['agent','list']:
  count=int((root/'agent-polls').read_text())+1 if (root/'agent-polls').exists() else 1
  (root/'agent-polls').write_text(str(count))
 else:count=int((root/'agent-polls').read_text())
 if count==2:
  (root/'slow-observation').touch();time.sleep(8)
 if args==['pane','list']:
  with open(root/'polls','a') as f:f.write('poll\n')
 print((root/('agents.json' if args==['agent','list'] else 'panes.json')).read_text())
else:print('{"result":{"shown":true}}')
"#).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
    struct Child(std::process::Child);
    impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
    let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&fake).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let read=||->toml::Value {toml::from_str(&fs::read_to_string(&record).unwrap()).unwrap()};
    let wait=|child:&mut Child,predicate:&dyn Fn()->bool| {
        let deadline=Instant::now()+Duration::from_secs(75);
        while !predicate(){assert!(child.0.try_wait().unwrap().is_none(),"ticker exited");assert!(Instant::now()<deadline,"ticker log: {}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}
    };
    let stop=|child:&mut Child| {
        fs::write(root.join(".ticker.stop"),b"").unwrap();let deadline=Instant::now()+Duration::from_secs(5);
        loop {if let Some(status)=child.0.try_wait().unwrap(){assert!(status.success());break;}assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(10));}
        fs::remove_file(root.join(".ticker.stop")).unwrap();
    };
    let mut child=spawn();wait(&mut child,&||read().get("copy_receipt").is_some());stop(&mut child);
    assert!(home.path().join("slow-observation").exists());assert!(!home.path().join("WRONG_SYNC_OBSERVATION").exists());
    assert_eq!(read()["report_hash"].as_str(),Some(hash.as_str()));assert_eq!(read()["copy_receipt"]["sequence"].as_integer(),Some(1));
    assert_eq!(fs::read(project.join("threads/t-0001.md")).unwrap(),b"new report\0\xff");let item=project.join("library/t-0001/item");assert_eq!(fs::read(&item).unwrap(),b"binary\0\xff");assert!(project.join("library/t-0001/empty").is_dir());
    let mut child=spawn();wait(&mut child,&||read().get("last_review_item_hash").and_then(|v|v.as_str())==Some(hash.as_str()));stop(&mut child);
    let notices=fs::read_dir(project.join("inbox")).unwrap().filter_map(|e|e.ok()).filter(|e|e.file_name().to_string_lossy().starts_with("review-")).count();assert_eq!(notices,1);
    fs::write(&item,b"retained after unchanged report").unwrap();let polls=fs::read(home.path().join("polls")).unwrap().len();
    let mut child=spawn();wait(&mut child,&||fs::read(home.path().join("polls")).unwrap().len()>=polls+10);stop(&mut child);
    assert_eq!(read()["copy_receipt"]["sequence"].as_integer(),Some(1));assert_eq!(read()["live_copy_sequence"].as_integer(),Some(1));assert_eq!(fs::read(item).unwrap(),b"retained after unchanged report");
}

#[test]
fn native_report_hash_is_bounded_binary_and_configuration_independent() {
    use std::fs;use sha2::{Digest,Sha256};
    let home=tempfile::tempdir().unwrap();let source=home.path().join("source '$λ");fs::create_dir(&source).unwrap();
    let run=||Command::new(BIN).env_clear().args(["report-hash","--path"]).arg(&source).output().unwrap();
    let missing=run();assert!(missing.status.success());assert_eq!(serde_json::from_slice::<serde_json::Value>(&missing.stdout).unwrap(),serde_json::json!({"hash":null}));
    fs::write(source.join("report.md"),b"binary\0\xff").unwrap();let output=run();assert!(output.status.success());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["hash"],format!("{:x}",Sha256::digest(b"binary\0\xff")));
    fs::File::create(source.join("report.md")).unwrap().set_len(50*1024*1024+1).unwrap();assert!(!run().status.success());
    fs::remove_file(source.join("report.md")).unwrap();std::os::unix::fs::symlink("/etc/passwd",source.join("report.md")).unwrap();assert!(!run().status.success());
    let name=std::ffi::CString::new(source.join("report.md").as_os_str().as_encoded_bytes()).unwrap();fs::remove_file(source.join("report.md")).unwrap();assert_eq!(unsafe{libc::mkfifo(name.as_ptr(),0o600)},0);assert!(!run().status.success());
}

#[cfg(target_os="linux")]
#[test]
fn native_ticker_claims_legacy_routine_and_restart_delivers_without_rerun() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::fs::PermissionsExt};
    use sha2::{Digest,Sha256};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
    assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
    let socket=home.path().join("session.sock");let _listener=std::os::unix::net::UnixListener::bind(&socket).unwrap();
    fs::write(project.join(".state/coordinator.json"),serde_json::to_vec(&serde_json::json!({"socket":socket,"workspace_id":"w1","tab_id":"w1:t1","pane_id":"w1:p1","agent_name":"coordinator","cwd":project})).unwrap()).unwrap();
    let command="printf run >> executions; printf routine-result";
    fs::write(project.join("routines/check.md"),format!("+++\nschedule = \"every 24h\"\ncommand = {}\n+++\nInspect output.\n",serde_json::to_string(command).unwrap())).unwrap();
    let cfg=home.path().join(".config/herdr-projects");fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("config.toml"),format!("[safety.\"{}\"]\nroutine_commands = true\n",project.display())).unwrap();
    fs::write(cfg.join("approved-routines.json"),serde_json::to_vec(&serde_json::json!([{"project":project,"routine":"check","command_sha256":format!("{:x}",Sha256::digest(command.as_bytes())),"approved":"fixture"}])).unwrap()).unwrap();
    let state=project.join(".state/ticker.json");fs::write(&state,b"{\"routines\":{\"check\":{\"last_run\":\"2026-01-01T00:00:00Z\"}}}").unwrap();
    let fake=home.path().join("herdr");fs::write(&fake,b"#!/bin/sh\ncase \"$1 $2\" in\n'agent list') echo '{\"result\":{\"agents\":[]}}';;\n'pane list') echo '{\"result\":{\"panes\":[]}}';;\n*) echo '{\"result\":{\"shown\":true}}';;\nesac\n").unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
    struct Child(std::process::Child);
    impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
    let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&fake).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let read=||->serde_json::Value {serde_json::from_slice(&fs::read(&state).unwrap()).unwrap()};
    let wait=|child:&mut Child,predicate:&dyn Fn()->bool| {
        // The first asynchronous session result is applied on the next 15 s pass.
        let deadline=Instant::now()+Duration::from_secs(35);
        while !predicate(){assert!(child.0.try_wait().unwrap().is_none(),"ticker exited");assert!(Instant::now()<deadline,"ticker log: {}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}
    };
    let stop=|child:&mut Child| {
        fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);
        while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}
        fs::remove_file(root.join(".ticker.stop")).unwrap();
    };
    let mut child=spawn();wait(&mut child,&||read()["routines"]["check"]["dispatch"]["result"].is_object());stop(&mut child);
    let mut child=spawn();wait(&mut child,&||read()["routines"]["check"]["dispatch"].is_null());stop(&mut child);
    assert_eq!(fs::read(project.join("executions")).unwrap(),b"run");
    let items=fs::read_dir(project.join("inbox")).unwrap().filter_map(Result::ok).filter_map(|entry|fs::read_to_string(entry.path()).ok()).filter(|text|text.contains("routine-result")).count();assert_eq!(items,1);
}

#[cfg(target_os="linux")]
#[test]
fn ticker_native_merged_finalization_resolves_and_replays_notice_after_restart() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::fs::PermissionsExt};
    use sha2::{Digest,Sha256};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
    assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
    let source=home.path().join("source '$λ");fs::create_dir_all(source.join("library/empty")).unwrap();
    fs::write(source.join("report.md"),b"PR: https://github.com/example/repo/pull/1\ncomplete\n").unwrap();fs::write(source.join("library/item"),b"binary\0\xff").unwrap();
    let hash=format!("{:x}",Sha256::digest(b"PR: https://github.com/example/repo/pull/1\ncomplete\n"));
    let socket=home.path().join("session.sock");let _listener=std::os::unix::net::UnixListener::bind(&socket).unwrap();
    fs::write(project.join(".state/coordinator.json"),serde_json::to_vec(&serde_json::json!({"socket":socket,"workspace_id":"w1","tab_id":"w1:t1","pane_id":"w1:p1","agent_name":"coordinator","cwd":project})).unwrap()).unwrap();
    let record=project.join("threads/t-0001.toml");
    fs::write(&record,toml::to_string(&serde_json::json!({"id":"t-0001","status":"open","kind":"adopted","thread_dir":source,"cwd":source,"workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1","agent":"claude","agent_name":"worker","title":"Fixture","created":jiff::Timestamp::now().to_string()})).unwrap()).unwrap();
    fs::write(home.path().join("agents.json"),serde_json::to_vec(&serde_json::json!({"result":{"agents":[{"workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1","cwd":source,"name":"worker","agent":"claude","agent_status":"idle"}]}})).unwrap()).unwrap();
    fs::write(home.path().join("panes.json"),serde_json::to_vec(&serde_json::json!({"result":{"panes":[{"workspace_id":"w1","tab_id":"w1:t1","pane_id":"w1:p1","cwd":project},{"workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1","cwd":source}]}})).unwrap()).unwrap();
    let fake=home.path().join("herdr");fs::write(&fake,b"#!/bin/sh\ncase \"$1 $2\" in\n'agent list') /bin/cat \"$HOME/agents.json\";;\n'pane list') echo poll >> \"$HOME/polls\"; /bin/cat \"$HOME/panes.json\";;\n*) echo '{\"result\":{\"shown\":true}}';;\nesac\n").unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
    struct Child(std::process::Child);
    impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
    let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&fake).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let read=||->toml::Value {toml::from_str(&fs::read_to_string(&record).unwrap()).unwrap()};
    let wait=|child:&mut Child,predicate:&dyn Fn()->bool| {
        let deadline=Instant::now()+Duration::from_secs(45);
        while !predicate(){assert!(child.0.try_wait().unwrap().is_none(),"ticker exited");assert!(Instant::now()<deadline,"ticker log: {}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}
    };
    let stop=|child:&mut Child| {
        fs::write(root.join(".ticker.stop"),b"").unwrap();let deadline=Instant::now()+Duration::from_secs(5);
        loop {if let Some(status)=child.0.try_wait().unwrap(){assert!(status.success());break;}assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(10));}
        fs::remove_file(root.join(".ticker.stop")).unwrap();
    };
    let url="https://github.com/example/repo/pull/1";
    let mut saved=read();let table=saved.as_table_mut().unwrap();
    for (key,value) in [("pr",url),("pr_state","MERGED"),("report_hash",hash.as_str()),("acked_report_hash",hash.as_str()),("last_review_item_hash",hash.as_str()),("last_state","idle"),("last_group","idle")] {table.insert(key.into(),toml::Value::String(value.into()));}
    fs::write(&record,toml::to_string(&saved).unwrap()).unwrap();fs::write(project.join("threads/t-0001.md"),fs::read(source.join("report.md")).unwrap()).unwrap();
    let fingerprint=format!("{:x}",Sha256::digest(serde_json::json!(["t-0001",saved["created"].as_str().unwrap(),0,"adopted","","","","","",source,"w2","w2:t1","w2:p1","claude","worker",source,url]).to_string().as_bytes()));
    fs::write(project.join(".state/ticker.json"),serde_json::to_vec(&serde_json::json!({"last_pr_check":jiff::Timestamp::now().to_string(),"finalizations":{"t-0001":{"operation_id":format!("merged-{fingerprint}"),"fingerprint":fingerprint,"pr":url,"reason":"merged"}}})).unwrap()).unwrap();
    let mut child=spawn();wait(&mut child,&||read()["status"].as_str()==Some("resolved"));stop(&mut child);
    assert_eq!(read()["resolved_reason"].as_str(),Some("merged"));assert_eq!(read()["final_copy_sequence"].as_integer(),Some(1));assert!(read().get("pending_final_copy").is_none());
    assert!(!read()["artifact_snapshot"].as_str().unwrap().is_empty());assert_eq!(read()["copy_receipt"]["sequence"].as_integer(),Some(1));
    assert_eq!(fs::read(project.join("library/t-0001/item")).unwrap(),b"binary\0\xff");
    let mut child=spawn();wait(&mut child,&||read().get("pending_final_notice").is_none()&&serde_json::from_slice::<serde_json::Value>(&fs::read(project.join(".state/ticker.json")).unwrap()).unwrap()["finalizations"].as_object().is_some_and(|v|v.is_empty()));stop(&mut child);
    assert_eq!(read()["final_copy_sequence"].as_integer(),Some(1));
    assert_eq!(fs::read_dir(project.join("inbox")).unwrap().filter_map(|e|e.ok()).filter(|e|e.file_name().to_string_lossy().starts_with("final-")).count(),1);
}

#[cfg(target_os="linux")]
#[test]
fn ticker_native_briefs_confirm_or_recover_uncertainty_without_replay() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::{fs::PermissionsExt,net::UnixListener}};
    for outcome in ["confirmed","lost"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
        assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
        let socket=home.path().join("session.sock");let _listener=UnixListener::bind(&socket).unwrap();
        fs::write(project.join(".state/coordinator.json"),serde_json::json!({"socket":socket}).to_string()).unwrap();
        let source=home.path().join("source");fs::create_dir(&source).unwrap();
        let agent=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":source,"name":"worker","agent":"claude","agent_status":"idle"});
        let record=project.join("threads/t-0001.toml");fs::write(&record,toml::to_string(&serde_json::json!({"id":"t-0001","status":"open","kind":"adopted","prompt_pending":true,"thread_dir":source,"cwd":source,"workspace_id":"w","tab_id":"tab","pane_id":"p","agent":"claude","agent_name":"worker","created":jiff::Timestamp::now().to_string()})).unwrap()).unwrap();
        fs::write(home.path().join("agents.json"),serde_json::json!({"result":{"agents":[agent.clone()]}}).to_string()).unwrap();
        fs::write(home.path().join("panes.json"),serde_json::json!({"result":{"panes":[{"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":source}]}}).to_string()).unwrap();
        fs::write(home.path().join("ack.json"),serde_json::json!({"result":{"type":"agent_prompted","agent":agent}}).to_string()).unwrap();
        let response=if outcome=="lost" {"exit 1"}else{"/bin/cat \"$HOME/ack.json\""};
        let fake=home.path().join("herdr");fs::write(&fake,format!("#!/bin/sh\ncase \"$1 $2\" in\n'agent list') /bin/cat \"$HOME/agents.json\";;\n'pane list') echo poll >> \"$HOME/polls\"; /bin/cat \"$HOME/panes.json\";;\n'agent prompt') printf send >> \"$HOME/sent\"; {response};;\n*) echo '{{\"result\":{{\"shown\":true}}}}';;\nesac\n")).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&fake).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let read=||->toml::Value {toml::from_str(&fs::read_to_string(&record).unwrap()).unwrap()};
        let wait=|child:&mut Child,predicate:&dyn Fn()->bool| {let end=Instant::now()+Duration::from_secs(45);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
        let stop=|child:&mut Child| {fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
        let mut child=spawn();wait(&mut child,&||fs::read(home.path().join("sent")).is_ok_and(|b|b==b"send")&&read().get("prompt_claim").is_some_and(|c|c.get("phase").and_then(|p|p.as_str())==Some(if outcome=="confirmed"{"confirmed"}else{"pending"})));stop(&mut child);
        let polls=fs::read(home.path().join("polls")).unwrap().len();let mut child=spawn();
        wait(&mut child,&||fs::read(home.path().join("polls")).unwrap().len()>polls&&read()["prompt_claim"]["notified"].as_bool()==Some(true));stop(&mut child);
        assert_eq!(fs::read(home.path().join("sent")).unwrap(),b"send");assert_eq!(read()["prompt_sequence"].as_integer(),Some(1));
        if outcome=="confirmed" {assert_eq!(read()["prompt_pending"].as_bool(),Some(false));}
        else {assert_eq!(read()["status"].as_str(),Some("failed"));assert_eq!(read()["prompt_claim"]["phase"].as_str(),Some("uncertain"));let notices=fs::read_dir(project.join("inbox")).unwrap().filter_map(Result::ok).filter(|e|e.file_name().to_string_lossy().starts_with("brief-")).count();assert_eq!(notices,1);}
    }
}

#[cfg(target_os="linux")]
#[test]
fn ticker_remote_briefs_confirm_or_recover_uncertainty_without_replay() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::{fs::PermissionsExt,net::UnixListener}};
    for outcome in ["confirmed","lost"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
        assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
        let socket=home.path().join("session.sock");let _listener=UnixListener::bind(&socket).unwrap();
        fs::write(project.join(".state/coordinator.json"),serde_json::json!({"socket":socket}).to_string()).unwrap();
        let source=home.path().join("source");fs::create_dir(&source).unwrap();
        let agent=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":source,"name":"worker","agent":"claude","agent_status":"idle"});
        let record=project.join("threads/t-0001.toml");fs::write(&record,toml::to_string(&serde_json::json!({"id":"t-0001","status":"open","kind":"adopted","prompt_pending":true,"machine":"saved","thread_dir":source,"cwd":source,"workspace_id":"w","tab_id":"tab","pane_id":"p","agent":"claude","agent_name":"worker","created":jiff::Timestamp::now().to_string()})).unwrap()).unwrap();
        fs::write(home.path().join("agents.json"),serde_json::json!({"result":{"agents":[agent.clone()]}}).to_string()).unwrap();
        fs::write(home.path().join("panes.json"),serde_json::json!({"result":{"panes":[{"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":source}]}}).to_string()).unwrap();
        fs::write(home.path().join("ack.json"),serde_json::json!({"result":{"type":"agent_prompted","agent":agent}}).to_string()).unwrap();
        let route=serde_json::json!([{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","label":"saved","target":"fixture.invalid","session":"named-session","enabled":true,"selected":false}]);
        fs::write(home.path().join("routes.json"),route.to_string()).unwrap();fs::write(home.path().join("outcome"),outcome).unwrap();
        let fake=home.path().join("herdr");fs::write(&fake,r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME']);args=sys.argv[1:];remote=args[:2]==['--machine','saved']
if remote:args=args[2:]
if args==['machine','list','--json']:print((root/'routes.json').read_text())
elif args==['agent','list']:print((root/'agents.json').read_text() if remote else '{"result":{"agents":[]}}')
elif args==['pane','list']:
 with open(root/'polls','a') as f:f.write('poll')
 print((root/'panes.json').read_text() if remote else '{"result":{"panes":[]}}')
elif args[:2] in [['agent','prompt'],['agent','start']]:
 (root/'WRONG_SYNC_EFFECT').touch();sys.exit(2)
else:print('{"result":{"shown":true}}')
"#).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
        let bridge=home.path().join("remote herdr");fs::write(&bridge,r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME'])
assert sys.argv[1:4]==['--session','named-session','remote-api-bridge']
if sys.argv[4:]==['--check']:print('herdr-api-bridge-v1');sys.exit(0)
r=json.loads(sys.stdin.readline())
if r['method']=='agent.list':result=json.loads((root/'agents.json').read_text())['result']
elif r['method']=='pane.list':result=json.loads((root/'panes.json').read_text())['result']
elif r['method']=='agent.prompt':
 assert r['params']['target']=='p'
 with open(root/'sent','a') as f:f.write('send')
 if (root/'outcome').read_text()=='lost':sys.exit(1)
 result=json.loads((root/'ack.json').read_text())['result']
else:sys.exit(3)
print(json.dumps({'id':r['id'],'result':result}))
"#).unwrap();fs::set_permissions(&bridge,fs::Permissions::from_mode(0o700)).unwrap();
        let ssh=home.path().join("ssh");fs::write(&ssh,r#"#!/usr/bin/python3
import sys,subprocess
assert sys.argv[-2]=='fixture.invalid'
if 'remote-api-bridge' in sys.argv[-1]:assert sys.argv[1:4]==['-T','-o','StrictHostKeyChecking=yes']
sys.exit(subprocess.call(sys.argv[-1],shell=True))
"#).unwrap();fs::set_permissions(&ssh,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH",format!("{}:/usr/bin:/bin",home.path().display())).env("HERDR_BIN_PATH",&fake).env("HERDR_PROJECTS_REMOTE_HERDR_BIN",&bridge).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let read=||->toml::Value {toml::from_str(&fs::read_to_string(&record).unwrap()).unwrap()};
        let wait=|child:&mut Child,predicate:&dyn Fn()->bool| {let end=Instant::now()+Duration::from_secs(45);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
        let stop=|child:&mut Child| {fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
        let mut child=spawn();wait(&mut child,&||fs::read(home.path().join("sent")).is_ok_and(|b|b==b"send")&&read().get("prompt_claim").is_some_and(|c|c.get("phase").and_then(|p|p.as_str())==Some(if outcome=="confirmed"{"confirmed"}else{"pending"})));stop(&mut child);
        let polls=fs::read(home.path().join("polls")).unwrap().len();let mut child=spawn();
        wait(&mut child,&||fs::read(home.path().join("polls")).unwrap().len()>polls&&read()["prompt_claim"]["notified"].as_bool()==Some(true));stop(&mut child);
        assert!(!home.path().join("WRONG_SYNC_EFFECT").exists());assert_eq!(fs::read(home.path().join("sent")).unwrap(),b"send");assert_eq!(read()["prompt_sequence"].as_integer(),Some(1));
        if outcome=="confirmed" {assert_eq!(read()["prompt_pending"].as_bool(),Some(false));}
        else {assert_eq!(read()["status"].as_str(),Some("failed"));assert_eq!(read()["prompt_claim"]["phase"].as_str(),Some("uncertain"));let notices=fs::read_dir(project.join("inbox")).unwrap().filter_map(Result::ok).filter(|e|e.file_name().to_string_lossy().starts_with("brief-")).count();assert_eq!(notices,1);}
    }
}

#[cfg(target_os="linux")]
#[test]
fn ticker_local_and_remote_launches_acknowledge_once_and_recover_lost_replies() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::{fs::PermissionsExt,net::UnixListener}};
    for is_remote in [false,true] {
    for outcome in ["confirmed","lost"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
        assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
        let socket=home.path().join("session.sock");let _listener=UnixListener::bind(&socket).unwrap();
        fs::write(project.join(".state/coordinator.json"),serde_json::json!({"socket":socket}).to_string()).unwrap();
        let source=home.path().join("source");fs::create_dir(&source).unwrap();
        let agent=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":source,"name":"worker","agent":"claude","agent_status":"blocked","terminal_id":"terminal","launch_pending":true});
        let record=project.join("threads/t-0001.toml");fs::write(&record,toml::to_string(&serde_json::json!({"id":"t-0001","status":"open","kind":"adopted","prompt_pending":true,"machine":if is_remote{"saved"}else{""},"thread_dir":source,"cwd":source,"workspace_id":"w","tab_id":"tab","pane_id":"p","agent":"claude","agent_name":"worker","created":jiff::Timestamp::now().to_string()})).unwrap()).unwrap();
        fs::write(home.path().join("agents.json"),serde_json::json!({"result":{"agents":[agent.clone()]}}).to_string()).unwrap();
        fs::write(home.path().join("panes.json"),serde_json::json!({"result":{"panes":[{"workspace_id":"w","tab_id":"tab","pane_id":"p","terminal_id":"terminal","cwd":source}]}}).to_string()).unwrap();
        fs::write(home.path().join("ack.json"),serde_json::json!({"result":{"type":"agent_started","agent":agent,"argv":["claude"]}}).to_string()).unwrap();
        let route=serde_json::json!([{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","label":"saved","target":"fixture.invalid","session":"named-session","enabled":true,"selected":false}]);
        fs::write(home.path().join("routes.json"),route.to_string()).unwrap();fs::write(home.path().join("outcome"),outcome).unwrap();fs::write(home.path().join("remote-mode"),if is_remote{"yes"}else{"no"}).unwrap();
        let fake=home.path().join("herdr");fs::write(&fake,r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME']);args=sys.argv[1:];remote=args[:2]==['--machine','saved']
if remote:args=args[2:]
remote_mode=(root/'remote-mode').read_text()=='yes'
if args[:1]==['remote-api-bridge']:os.execv(str(root/'remote herdr'),[str(root/'remote herdr'),*args])
if args==['machine','list','--json']:print((root/'routes.json').read_text())
elif args==['agent','list']:print((root/'agents.json').read_text() if (remote or not remote_mode) and (root/'started').exists() else '{"result":{"agents":[]}}')
elif args==['pane','list']:
 with open(root/'polls','a') as f:f.write('poll')
 print((root/'panes.json').read_text() if remote or not remote_mode else '{"result":{"panes":[]}}')
elif args[:2] in [['agent','prompt'],['agent','start']]:
 (root/'WRONG_SYNC_EFFECT').touch();sys.exit(2)
else:print('{"result":{"shown":true}}')
"#).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
        let bridge=home.path().join("remote herdr");fs::write(&bridge,r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME'])
args=sys.argv[1:]
if args[:2]==['--session','named-session']:args=args[2:]
assert args[0]=='remote-api-bridge'
if args[1:]==['--check']:print('herdr-api-bridge-v1');sys.exit(0)
r=json.loads(sys.stdin.readline())
if r['method']=='agent.list':result=json.loads((root/'agents.json').read_text())['result'] if (root/'started').exists() else {'agents':[]}
elif r['method']=='pane.list':result=json.loads((root/'panes.json').read_text())['result']
elif r['method']=='agent.start':
 assert r['params']=={'name':'worker','kind':'claude','pane_id':'p','args':[],'timeout_ms':20000}
 with open(root/'started','a') as f:f.write('start')
 if (root/'outcome').read_text()=='lost':sys.exit(1)
 result=json.loads((root/'ack.json').read_text())['result'];del result['agent']['agent'];result['agent']['agent_status']='unknown'
else:sys.exit(3)
print(json.dumps({'id':r['id'],'result':result}))
"#).unwrap();fs::set_permissions(&bridge,fs::Permissions::from_mode(0o700)).unwrap();
        let ssh=home.path().join("ssh");fs::write(&ssh,r#"#!/usr/bin/python3
import sys,subprocess
assert sys.argv[-2]=='fixture.invalid'
if 'remote-api-bridge' in sys.argv[-1]:assert sys.argv[1:4]==['-T','-o','StrictHostKeyChecking=yes']
sys.exit(subprocess.call(sys.argv[-1],shell=True))
"#).unwrap();fs::set_permissions(&ssh,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH",format!("{}:/usr/bin:/bin",home.path().display())).env("HERDR_BIN_PATH",&fake).env("HERDR_PROJECTS_REMOTE_HERDR_BIN",&bridge).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let read=||->toml::Value {toml::from_str(&fs::read_to_string(&record).unwrap()).unwrap()};
        let wait=|child:&mut Child,predicate:&dyn Fn()->bool| {let end=Instant::now()+Duration::from_secs(45);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
        let stop=|child:&mut Child| {fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
        let mut child=spawn();wait(&mut child,&||fs::read(home.path().join("started")).is_ok_and(|b|b==b"start")&&read().get("launch_claim").is_some_and(|c|c.get("phase").and_then(|p|p.as_str())==Some(if outcome=="confirmed"{"confirmed"}else{"pending"})));stop(&mut child);
        let polls=fs::read(home.path().join("polls")).unwrap().len();let mut child=spawn();
        wait(&mut child,&||fs::read(home.path().join("polls")).unwrap().len()>polls&&read()["launch_claim"]["notified"].as_bool()==Some(true));stop(&mut child);
        assert!(!home.path().join("WRONG_SYNC_EFFECT").exists());assert_eq!(fs::read(home.path().join("started")).unwrap(),b"start");assert_eq!(read()["launch_sequence"].as_integer(),Some(1));
        if outcome=="confirmed" {assert_eq!(read()["prompt_pending"].as_bool(),Some(true));assert_eq!(read()["launch_claim"]["phase"].as_str(),Some("confirmed"));}
        else {assert_eq!(read()["status"].as_str(),Some("failed"));assert_eq!(read()["launch_claim"]["phase"].as_str(),Some("uncertain"));let notices=fs::read_dir(project.join("inbox")).unwrap().filter_map(Result::ok).filter(|e|e.file_name().to_string_lossy().starts_with("launch-")).count();assert_eq!(notices,1);}
    }
}

}

#[test]
#[cfg(target_os="linux")]
fn ticker_coordinator_prime_confirms_or_recovers_once_across_restart() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::{fs::PermissionsExt,net::UnixListener}};
    for outcome in ["confirmed","lost"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
        assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
        let socket=home.path().join("session.sock");let _listener=UnixListener::bind(&socket).unwrap();
        let record=project.join(".state/coordinator.json");
        fs::write(&record,serde_json::json!({"socket":socket,"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":project,"agent_name":"coordinator","prime_pending":true,"prime_request":1}).to_string()).unwrap();
        let a=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":project,"name":"coordinator","agent":"claude","agent_status":"idle","terminal_id":"terminal"});
        fs::write(home.path().join("agent.json"),a.to_string()).unwrap();fs::write(home.path().join("outcome"),outcome).unwrap();
        let fake=home.path().join("herdr");fs::write(&fake,r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME']);args=sys.argv[1:];a=json.loads((root/'agent.json').read_text())
if args==['remote-api-bridge','--check']:print('herdr-api-bridge-v1')
elif args==['remote-api-bridge']:
 r=json.load(sys.stdin);assert r['method']=='agent.prompt';assert r['params']['target']=='p';assert ' context demo' in r['params']['text']
 with open(root/'sent','a') as f:f.write('send')
 if (root/'outcome').read_text()=='lost':sys.exit(1)
 print(json.dumps({'id':r['id'],'result':{'type':'agent_prompted','agent':a}}))
elif args==['agent','list']:print(json.dumps({'result':{'agents':[a]}}))
elif args==['pane','list']:
 with open(root/'polls','a') as f:f.write('poll')
 print(json.dumps({'result':{'panes':[a]}}))
elif args[:2] in [['agent','prompt'],['agent','start']]:
 (root/'WRONG_SYNC_EFFECT').touch();sys.exit(2)
else:print('{"result":{"shown":true}}')
"#).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&fake).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let read=||->serde_json::Value{serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap()};
        let wait=|child:&mut Child,predicate:&dyn Fn()->bool|{let end=Instant::now()+Duration::from_secs(30);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
        let stop=|child:&mut Child|{fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
        let mut child=spawn();wait(&mut child,&||home.path().join("sent").exists()&&read()["prime_claim"]["delivery"]["phase"].as_str()==Some(if outcome=="confirmed"{"confirmed"}else{"pending"}));stop(&mut child);
        let polls=fs::read(home.path().join("polls")).unwrap().len();let mut child=spawn();wait(&mut child,&||fs::read(home.path().join("polls")).unwrap().len()>polls&&read()["prime_claim"]["delivery"]["notified"].as_bool()==Some(true));stop(&mut child);
        assert!(!home.path().join("WRONG_SYNC_EFFECT").exists());assert_eq!(fs::read(home.path().join("sent")).unwrap(),b"send");assert_eq!(read()["prime_sequence"],1);assert_eq!(read()["prime_pending"],outcome=="lost");
        if outcome=="lost" {assert_eq!(read()["prime_claim"]["delivery"]["phase"],"uncertain");assert_eq!(fs::read_dir(project.join("inbox")).unwrap().filter_map(Result::ok).filter(|e|e.file_name().to_string_lossy().starts_with("coordinator-prime-")).count(),1);}
    }
}

#[test]
#[cfg(target_os="linux")]
fn ticker_coordinator_start_then_prime_recover_without_replaying_start() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::{fs::PermissionsExt,net::UnixListener}};
    for outcome in ["confirmed","lost"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
        assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
        let socket=home.path().join("session.sock");let _listener=UnixListener::bind(&socket).unwrap();
        let record=project.join(".state/coordinator.json");
        fs::write(&record,serde_json::json!({"socket":socket,"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":project,"agent_name":"coordinator","prime_pending":true,"prime_request":1}).to_string()).unwrap();
        let a=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":project,"name":"coordinator","agent":"claude","agent_status":"idle","terminal_id":"terminal"});
        fs::write(home.path().join("agent.json"),a.to_string()).unwrap();fs::write(home.path().join("outcome"),outcome).unwrap();
        let fake=home.path().join("herdr");fs::write(&fake,r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME']);args=sys.argv[1:];a=json.loads((root/'agent.json').read_text())
if args==['remote-api-bridge','--check']:print('herdr-api-bridge-v1')
elif args==['remote-api-bridge']:
 r=json.load(sys.stdin)
 if r['method']=='agent.start':
  assert r['params']=={'name':'coordinator','kind':'claude','pane_id':'p','args':[],'timeout_ms':20000}
  with open(root/'sent','a') as f:f.write('start')
  if (root/'outcome').read_text()=='lost':sys.exit(1)
  del a['agent'];a['launch_pending']=True;a['agent_status']='unknown'
  result={'type':'agent_started','agent':a,'argv':['claude']}
 elif r['method']=='agent.prompt':
  assert r['params']['target']=='p'
  with open(root/'primed','a') as f:f.write('prime')
  result={'type':'agent_prompted','agent':a}
 else:sys.exit(3)
 print(json.dumps({'id':r['id'],'result':result}))
elif args==['agent','list']:print(json.dumps({'result':{'agents':[a] if (root/'sent').exists() else []}}))
elif args==['pane','list']:
 with open(root/'polls','a') as f:f.write('poll')
 print(json.dumps({'result':{'panes':[a]}}))
elif args[:2] in [['agent','prompt'],['agent','start']]:
 (root/'WRONG_SYNC_EFFECT').touch();sys.exit(2)
else:print('{"result":{"shown":true}}')
"#).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&fake).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let read=||->serde_json::Value{serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap()};
        let wait=|child:&mut Child,predicate:&dyn Fn()->bool|{let end=Instant::now()+Duration::from_secs(30);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
        let stop=|child:&mut Child|{fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
        let mut child=spawn();wait(&mut child,&||home.path().join("sent").exists()&&read()["launch_claim"]["phase"].as_str()==Some(if outcome=="confirmed"{"confirmed"}else{"pending"}));stop(&mut child);
        if outcome=="confirmed" {let mut child=spawn();wait(&mut child,&||read()["prime_pending"]==false);stop(&mut child);assert_eq!(fs::read(home.path().join("primed")).unwrap(),b"prime");}else{assert!(!home.path().join("primed").exists());}
        let polls=fs::read(home.path().join("polls")).unwrap().len();let mut child=spawn();wait(&mut child,&||fs::read(home.path().join("polls")).unwrap().len()>polls&&read()["launch_claim"]["notified"].as_bool()==Some(true));stop(&mut child);
        assert!(!home.path().join("WRONG_SYNC_EFFECT").exists());assert_eq!(fs::read(home.path().join("sent")).unwrap(),b"start");assert_eq!(read()["launch_sequence"],1);assert_eq!(read()["prime_pending"],outcome=="lost");
        if outcome=="lost" {assert_eq!(read()["launch_claim"]["phase"],"uncertain");assert_eq!(fs::read_dir(project.join("inbox")).unwrap().filter_map(Result::ok).filter(|e|e.file_name().to_string_lossy().starts_with("coordinator-start-")).count(),1);}
    }
}

#[test]
#[cfg(target_os="linux")]
fn ticker_notifications_recover_across_restart_and_reconcile_through_cli() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::{fs::PermissionsExt,net::UnixListener}};
    for nudge in [false,true] {for outcome in ["confirmed","lost"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let p=root.join("demo");
        if nudge {let path=p.join("PROJECT.md");fs::write(&path,fs::read_to_string(&path).unwrap().replace("nudge = false","nudge = true")).unwrap();}
        let socket=home.path().join("session.sock");let _listener=UnixListener::bind(&socket).unwrap();fs::write(p.join(".state/coordinator.json"),serde_json::json!({"socket":socket,"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":p,"agent_name":"coordinator"}).to_string()).unwrap();
        let item=|id:&str|fs::write(p.join(format!("inbox/{id}.md")),format!("+++\nid='{id}'\nkind='test'\nsummary='Fixture'\n+++\n")).unwrap();item("item-a");
        let a=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":p,"name":"coordinator","agent":"claude","agent_status":"idle","terminal_id":"terminal"});fs::write(home.path().join("agent.json"),a.to_string()).unwrap();fs::write(home.path().join("outcome"),outcome).unwrap();
        let fake=home.path().join("herdr");fs::write(&fake,r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME']);args=sys.argv[1:];a=json.loads((root/'agent.json').read_text())
if args==['remote-api-bridge','--check']:print('herdr-api-bridge-v1')
elif args==['remote-api-bridge']:
 r=json.load(sys.stdin)
 if r['method'] in ['agent.list','pane.list','pane.report_metadata']:
  result={'agents':[a]} if r['method']=='agent.list' else {'panes':[a]} if r['method']=='pane.list' else {'type':'ok'}
  print(json.dumps({'id':r['id'],'result':result}));sys.exit(0)
 with open(root/'sent','a') as f:f.write('send')
 if (root/'outcome').read_text()=='lost':sys.exit(1)
 if r['method']=='agent.prompt':
  assert r['params']['text'].startswith('[herdr-projects ticker: automated, not the user, approves nothing]')
  result={'type':'agent_prompted','agent':a}
 else:
  assert r['method']=='notification.show'
  result={'type':'notification_show','shown':True,'reason':'shown'}
 print(json.dumps({'id':r['id'],'result':result}))
elif args==['agent','list']:print(json.dumps({'result':{'agents':[a]}}))
elif args==['pane','list']:
 with open(root/'polls','a') as f:f.write('poll')
 print(json.dumps({'result':{'panes':[a]}}))
elif args[:2] in [['agent','prompt'],['notification','show']]:
 (root/'WRONG_SYNC_EFFECT').touch();sys.exit(2)
else:print('{"result":{"shown":true}}')
"#).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child{fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let command=||{let mut c=Command::new(BIN);c.env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&fake).args(["--root",r]);c};
        let spawn=||Child(command().args(["ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let record=p.join(".state/ticker.json");let read=||->serde_json::Value{serde_json::from_str(&fs::read_to_string(&record).unwrap_or_else(|_|"{}".into())).unwrap()};
        let wait=|child:&mut Child,predicate:&dyn Fn()->bool|{let end=Instant::now()+Duration::from_secs(40);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
        let stop=|child:&mut Child|{fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
        let mut child=spawn();wait(&mut child,&||home.path().join("sent").exists()&&read()["notification_claim"]["phase"]==if outcome=="confirmed"{"confirmed"}else{"pending"});stop(&mut child);
        let polls=fs::read(home.path().join("polls")).unwrap().len();if outcome=="lost"{item("item-b");}
        let mut child=spawn();wait(&mut child,&||fs::read(home.path().join("polls")).unwrap().len()>polls&&read()["notification_claim"]["phase"]==if outcome=="confirmed"{"confirmed"}else{"uncertain"});
        assert_eq!(fs::read(home.path().join("sent")).unwrap(),b"send");
        if outcome=="lost" {
            let inspected=command().args(["notification","demo","inspect"]).output().unwrap();assert!(inspected.status.success());let value:serde_json::Value=serde_json::from_slice(&inspected.stdout).unwrap();assert_eq!(value["claim"]["phase"],"uncertain");
            let refused=command().args(["notification","demo","retry","--sequence","1"]).output().unwrap();assert!(!refused.status.success());assert_eq!(read()["notification_sequence"],1);
            fs::write(home.path().join("outcome"),"confirmed").unwrap();
            let args=if nudge{vec!["notification","demo","retry","--sequence","1","--accept-possible-duplicate"]}else{vec!["notification","demo","acknowledge","--sequence","1"]};
            let end=Instant::now()+Duration::from_secs(5);loop{let o=command().args(&args).output().unwrap();if o.status.success(){break;}assert!(Instant::now()<end,"{}",String::from_utf8_lossy(&o.stderr));std::thread::sleep(Duration::from_millis(20));}
            stop(&mut child);let mut child=spawn();wait(&mut child,&||read()["notification_claim"]["phase"]=="confirmed"&&read()["notification_sequence"]==2);stop(&mut child);
            assert_eq!(fs::read(home.path().join("sent")).unwrap(),b"sendsend");
            if nudge{assert_eq!(read()["notification_claim"]["retry_of"],1);}else{assert_eq!(read()["notification_claim"]["batch"]["ids"],serde_json::json!(["item-b"]));assert_eq!(read()["notification_suppressed"],serde_json::json!(["item-a"]));}
        }else{stop(&mut child);}
        assert!(!home.path().join("WRONG_SYNC_EFFECT").exists());
    }}
}

#[cfg(target_os="linux")]
#[test]
fn ticker_tokens_use_supervised_local_remote_and_coordinator_refreshes_after_restart() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::{fs::PermissionsExt,net::UnixListener}};
    for mode in ["local","remote","coordinator"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let p=root.join("demo");
        let socket=home.path().join("session.sock");let _listener=UnixListener::bind(&socket).unwrap();let source=home.path().join("source");fs::create_dir(&source).unwrap();
        let a=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","terminal_id":"terminal","cwd":source,"name":"worker","agent":"claude","agent_status":"working"});fs::write(home.path().join("agent.json"),a.to_string()).unwrap();fs::write(home.path().join("mode"),mode).unwrap();
        let mut c=serde_json::json!({"socket":socket});
        if mode=="coordinator" {for key in ["workspace_id","tab_id","pane_id","cwd"]{c[key]=a[key].clone();}c["agent_name"]="worker".into();}
        else{fs::write(p.join("threads/t-0001.toml"),toml::to_string(&serde_json::json!({"id":"t-0001","status":"open","kind":"adopted","machine":if mode=="remote"{"saved"}else{""},"thread_dir":source,"cwd":source,"workspace_id":"w","tab_id":"tab","pane_id":"p","agent":"claude","agent_name":"worker","created":jiff::Timestamp::now().to_string()})).unwrap()).unwrap();}
        fs::write(p.join(".state/coordinator.json"),c.to_string()).unwrap();
        fs::write(home.path().join("routes.json"),serde_json::json!([{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","label":"saved","target":"fixture.invalid","session":"named-session","enabled":true}]).to_string()).unwrap();
        let fake=home.path().join("herdr");fs::write(&fake,r#"#!/usr/bin/python3
import os,sys,json,pathlib
root=pathlib.Path(os.environ['HOME']);args=sys.argv[1:];a=json.loads((root/'agent.json').read_text());mode=(root/'mode').read_text()
remote=args[:2]==['--machine','saved']
if remote:args=args[2:]
if args[:2]==['--session','named-session']:args=args[2:];remote=True
if args==['machine','list','--json']:print((root/'routes.json').read_text())
elif args==['remote-api-bridge','--check']:print('herdr-api-bridge-v1')
elif args==['remote-api-bridge']:
 assert (mode=='remote')==remote
 r=json.load(sys.stdin)
 if r['method']=='agent.list':result={'agents':[a]}
 elif r['method']=='pane.list':result={'panes':[a]}
 elif r['method']=='pane.report_metadata':
  assert set(r['params'])=={'pane_id','source','ttl_ms','tokens'}
  assert r['params']['ttl_ms']==300000 and r['params']['source']=='herdr-projects'
  with open(root/'tokens','a') as f:f.write(json.dumps(r['params'])+'\n')
  result={'type':'ok'}
 else:sys.exit(3)
 print(json.dumps({'id':r['id'],'result':result}))
elif args==['agent','list']:print(json.dumps({'result':{'agents':[a] if remote or mode!='remote' else []}}))
elif args==['pane','list']:print(json.dumps({'result':{'panes':[a] if remote or mode!='remote' else []}}))
elif args[:2] in [['pane','report-metadata'],['agent','prompt'],['agent','start'],['notification','show']]:
 (root/'WRONG_SYNC_EFFECT').touch();sys.exit(2)
else:print('{"result":{"shown":true}}')
"#).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
        let ssh=home.path().join("ssh");fs::write(&ssh,"#!/usr/bin/python3\nimport sys,subprocess\nassert sys.argv[-2]=='fixture.invalid'\nif 'remote-api-bridge' in sys.argv[-1]:assert sys.argv[1:4]==['-T','-o','StrictHostKeyChecking=yes']\nsys.exit(subprocess.call(sys.argv[-1],shell=True))\n").unwrap();fs::set_permissions(&ssh,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child{fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let read=||fs::read_to_string(home.path().join("tokens")).unwrap_or_default().lines().map(|s|serde_json::from_str::<serde_json::Value>(s).unwrap()).collect::<Vec<_>>();
        for expected in 1..=2 {
            let mut child=Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH",format!("{}:/usr/bin:/bin",home.path().display())).env("HERDR_BIN_PATH",&fake).env("HERDR_PROJECTS_REMOTE_HERDR_BIN",&fake).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
            let end=Instant::now()+Duration::from_secs(45);while read().len()<expected {assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{mode}: {}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}
            fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();
        }
        let values=read();assert_eq!(values.len(),2);assert_eq!(values[0],values[1]);assert_eq!(values[0]["tokens"]["thread"],if mode=="coordinator"{"coordinator"}else{"t-0001"});if mode!="coordinator"{assert_eq!(values[0]["tokens"]["review"],"working");}
        assert!(!home.path().join("WRONG_SYNC_EFFECT").exists());
    }
}

#[cfg(feature="state-store")]
#[test]
fn ticker_canonical_finalization_preserves_once_and_recovers_receipt_after_restart() {
    use std::{fs,process::Stdio,time::{Duration,Instant}};
    use herdr_projects::{migration,runtime,operations::DeliveryState,domain::TaskState};
    for interrupted in [false,true] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
        for action in ["new","pause"] {assert!(hp(home.path(),&["--root",r,action,"demo"]).status.success());}
        let project=root.join("demo");let source=home.path().join("source");fs::create_dir_all(source.join("library")).unwrap();fs::write(source.join("report.md"),"report for review\n").unwrap();fs::write(source.join("library/result"),b"preserved bytes").unwrap();
        let original=format!("id='t-0001'\nstatus='resolved'\nthread_dir={}\n",serde_json::to_string(source.to_str().unwrap()).unwrap());fs::write(project.join("threads/t-0001.toml"),&original).unwrap();let plan=migration::inspect(&project).unwrap();migration::apply(&project,&plan,true).unwrap();
        let head=runtime::snapshot(&project).unwrap().head.to_string();let out=hp(home.path(),&["--root",r,"operations","demo","finalize","thread:t-0001","--reason","review","--expected-head",&head]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let op:herdr_projects::domain::Operation=serde_json::from_slice(&out.stdout).unwrap();
        let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
        if interrupted {raw.execute_batch("CREATE TRIGGER reject_confirmation BEFORE UPDATE ON operation_delivery WHEN NEW.state='confirmed' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();}
        struct Child(std::process::Child);impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH","/bin/false").args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let read=||runtime::snapshot(&project).unwrap().deliveries.into_iter().find(|d|d.operation==op.id).unwrap();
        let wait=|child:&mut Child,predicate:&dyn Fn()->bool|{let deadline=Instant::now()+Duration::from_secs(10);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<deadline,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
        let stop=|child:&mut Child|{fs::write(root.join(".ticker.stop"),b"").unwrap();let deadline=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
        let receipt_path=project.join(".state/finalization-receipts").join(format!("{}.json",op.id.as_str()));let mut child=spawn();
        if interrupted {
            wait(&mut child,&||receipt_path.is_file());child.0.kill().unwrap();child.0.wait().unwrap();
            assert_eq!(read().state,DeliveryState::Claimed);raw.execute_batch("DROP TRIGGER reject_confirmation;").unwrap();
            migration::open_active(&project).unwrap().expire_claims(read().lease_until_ms.unwrap()+1).unwrap();fs::remove_dir_all(&source).unwrap();
            let mut child=spawn();wait(&mut child,&||read().state==DeliveryState::Confirmed);stop(&mut child);
        }else{wait(&mut child,&||read().state==DeliveryState::Confirmed);stop(&mut child);fs::remove_dir_all(&source).unwrap();}
        let receipt:herdr_projects::operations::finalization::FinalizationReceipt=serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
        assert_eq!(fs::read(project.join(".state/canonical-artifacts").join(&receipt.artifact_key).join(&receipt.snapshot).join("library/result")).unwrap(),b"preserved bytes");
        let before=runtime::snapshot(&project).unwrap();let mut child=spawn();wait(&mut child,&||runtime::snapshot(&project).unwrap().head>before.head);stop(&mut child);
        let after=runtime::snapshot(&project).unwrap();assert_eq!(read().attempts,1);assert_eq!(read().state,DeliveryState::Confirmed);assert_eq!(after.tasks.iter().find(|t|Some(&t.id)==op.task.as_ref()).unwrap().state,TaskState::AwaitingReview);assert_eq!(fs::read_to_string(project.join("threads/t-0001.toml")).unwrap(),original);
        assert_eq!(fs::read_dir(project.join(".state/canonical-artifacts").join(&receipt.artifact_key)).unwrap().count(),1);
    }
}

#[cfg(all(feature="state-store",target_os="linux"))]
#[test]
fn ticker_canonical_routine_admits_from_hint_and_restart_keeps_one_execution() {
    use std::{fs,process::Stdio,time::{Duration,Instant}};
    use herdr_projects::{domain::*,authority,migration,runtime,operations::DeliveryState};
    use sha2::{Digest,Sha256};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();for action in ["new","pause"]{assert!(hp(home.path(),&["--root",r,action,"demo"]).status.success());}
    let key=home.path().join("owner");assert!(Command::new("/usr/bin/ssh-keygen").args(["-q","-t","ed25519","-N","","-f"]).arg(&key).output().unwrap().status.success());
    let public=fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");let project=root.join("demo");let config=home.path().join(".config/herdr-projects/config.toml");fs::create_dir_all(config.parent().unwrap()).unwrap();fs::write(&config,format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n[safety.{:?}]\nroutine_commands=true\n",project.display().to_string())).unwrap();
    let plan=migration::inspect_with_config(&project,&config).unwrap();migration::apply(&project,&plan,true).unwrap();let s=runtime::snapshot(&project).unwrap();runtime::set_state(&project,s.head,s.control.unwrap().revision,ProjectState::Active,&config).unwrap();
    let script=project.join("check.sh");let bytes=b"printf once >> ROUTINE_MARKER\n";fs::write(&script,bytes).unwrap();
    let definition=RoutineDefinition{version:1,name:"check".into(),revision:1,project_store:project.join(".state/state.db").canonicalize().unwrap().display().to_string(),authority:authority::policy_reference(&project).unwrap(),config:migration::config_reference(&config).unwrap(),enabled:true,schedule:"every 1h".into(),timezone:"UTC".into(),start_unix_ms:jiff::Timestamp::now().as_millisecond()-1000,missed:MissedRunPolicy::CoalesceLatest,overlap:OverlapPolicy::Skip,script:script.display().to_string(),script_sha256:format!("{:x}",Sha256::digest(bytes)),cwd:project.display().to_string(),deadline_ms:1000,output_cap_bytes:4000};
    let document=home.path().join("routine.json");fs::write(&document,serde_json::to_vec(&definition).unwrap()).unwrap();assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y","sign","-f"]).arg(&key).args(["-n",authority::ROUTINE_SIGNATURE_NAMESPACE]).arg(&document).output().unwrap().status.success());
    let signature=home.path().join("routine.json.sig");let out=hp(home.path(),&["--root",r,"routine-store","demo","import",document.to_str().unwrap(),signature.to_str().unwrap(),"--expected-head",&runtime::snapshot(&project).unwrap().head.to_string()]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    struct Child(std::process::Child);impl Drop for Child{fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
    let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH","/bin/false").args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let wait=|child:&mut Child,predicate:&dyn Fn()->bool|{let deadline=Instant::now()+Duration::from_secs(35);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<deadline,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
    let stop=|child:&mut Child|{fs::write(root.join(".ticker.stop"),b"").unwrap();let deadline=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
    let mut child=spawn();wait(&mut child,&||runtime::snapshot(&project).unwrap().routine_receipts.len()==1);stop(&mut child);
    let before=runtime::snapshot(&project).unwrap();assert_eq!(before.deliveries.len(),1);assert_eq!(before.deliveries[0].state,DeliveryState::Confirmed);assert_eq!(before.deliveries[0].attempts,1);assert!(before.routine_receipts[0].cleanup_verified&&before.routine_receipts[0].succeeded);
    // No bindings or due occurrence means a successful restart need not append
    // an event. Keep it alive across the initial pass and the next 15-second tick.
    let restarted=Instant::now();let mut child=spawn();wait(&mut child,&||restarted.elapsed()>=Duration::from_secs(16));stop(&mut child);
    let after=runtime::snapshot(&project).unwrap();assert_eq!(after.routine_receipts,before.routine_receipts);assert_eq!(after.deliveries,before.deliveries);assert_eq!(fs::read(project.join("ROUTINE_MARKER")).unwrap(),b"once");
}

/// An active project with an owner key, a SHA-256 repository with a base commit
/// and a candidate commit adding `files`, driven only through the CLI and ticker.
#[cfg(all(feature="state-store",target_os="linux"))]
struct VerifyFixture {home:tempfile::TempDir,root:std::path::PathBuf,project:std::path::PathBuf,key:std::path::PathBuf,repo:std::path::PathBuf,store:String,base:String,candidate:String}
#[cfg(all(feature="state-store",target_os="linux"))]
struct Ticker(std::process::Child);
#[cfg(all(feature="state-store",target_os="linux"))]
impl Drop for Ticker {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
#[cfg(all(feature="state-store",target_os="linux"))]
impl VerifyFixture {
    fn new(files:&[(&str,String)])->Self {Self::with_worker(files,"kind='claude'\npermission_policy='interactive'\n[profiles.worker.budget]\nmax_wall_seconds=60\nunknown_usage='allow_with_warning'\n")}
    /// `worker` is the body of the `[profiles.worker]` table.
    fn with_worker(files:&[(&str,String)],worker:&str)->Self {
        use std::fs;use herdr_projects::{domain::ProjectState,migration,runtime};
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
        for action in ["new","pause"]{assert!(hp(home.path(),&["--root",r,action,"demo"]).status.success());}
        let key=home.path().join("owner");assert!(Command::new("/usr/bin/ssh-keygen").args(["-q","-t","ed25519","-N","","-f"]).arg(&key).output().unwrap().status.success());
        let public=fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let project=root.join("demo");let config=home.path().join(".config/herdr-projects/config.toml");fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config,format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n[profiles.worker]\n{worker}")).unwrap();
        let plan=migration::inspect_with_config(&project,&config).unwrap();migration::apply(&project,&plan,true).unwrap();
        let s=runtime::snapshot(&project).unwrap();runtime::set_state(&project,s.head,s.control.unwrap().revision,ProjectState::Active,&config).unwrap();
        let store=project.join(".state/state.db").canonicalize().unwrap().display().to_string();
        let repo=home.path().join("repo");fs::create_dir(&repo).unwrap();
        let mut fixture=VerifyFixture{home,root,project,key,repo,store,base:String::new(),candidate:String::new()};
        fixture.git(&["init","-q","--object-format=sha256"]);fs::create_dir(fixture.repo.join("src")).unwrap();fs::write(fixture.repo.join("src/base.txt"),"base\n").unwrap();
        fixture.git(&["add","."]);fixture.git(&["commit","-qm","base"]);fixture.base=fixture.git(&["rev-parse","HEAD"]);
        for (path,text) in files {fs::write(fixture.repo.join(path),text).unwrap();}
        fixture.git(&["add","."]);fixture.git(&["commit","-qm","result"]);fixture.candidate=fixture.git(&["rev-parse","HEAD"]);
        fixture
    }
    fn r(&self)->&str {self.root.to_str().unwrap()}
    fn db(&self)->rusqlite::Connection {rusqlite::Connection::open(self.project.join(".state/state.db")).unwrap()}
    fn git(&self,args:&[&str])->String {
        let out=Command::new("/usr/bin/git").env_clear().env("PATH","/usr/bin:/bin").env("HOME",self.home.path()).env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null")
            .env("GIT_AUTHOR_NAME","fixture").env("GIT_AUTHOR_EMAIL","fixture@example.com").env("GIT_COMMITTER_NAME","fixture").env("GIT_COMMITTER_EMAIL","fixture@example.com")
            .current_dir(&self.repo).args(args).output().unwrap();
        assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    /// A task with a running attempt, a signed contract carrying `policies`, and one submitted result.
    fn submit(&self,task:&str,policies:&[(&str,String)])->String {self.submit_at(task,policies,&self.candidate)}
    fn submit_at(&self,task:&str,policies:&[(&str,String)],candidate:&str)->String {
        use std::fs;use herdr_projects::{authority::CONTRACT_SIGNATURE_NAMESPACE,domain::TaskId,runtime};
        let head=runtime::add_task(&self.project,TaskId::new(task).unwrap(),"work".into(),runtime::snapshot(&self.project).unwrap().head).unwrap();
        self.db().execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,1,'running',NULL,?1,0)",[format!("{task}-attempt"),task.to_owned()]).unwrap();
        let repository=self.repo.canonicalize().unwrap().display().to_string();
        let mut document=serde_json::to_vec_pretty(&serde_json::json!({
            "version":3,"outputs":[{"path":"src/lib.rs","kind":"git_file"}],"scope":{"paths":[{"path":"src/","access":"write"}]},
            "project_store":self.store,"expected_head":head,"task_id":task,"contract_revision":1,"deliverable":"ship","non_goals":"no launch",
            "acceptance_policies":policies.iter().map(|(id,text)|serde_json::json!({"id":id,"text":text})).collect::<Vec<_>>(),
            "repository":repository,"base_oid":self.base,"object_format":"sha256","dependencies":[],"capability_flags":[],
            "profile_kind":"codex","retry_class":"none","result_schema_id":"result-v1","route":"verify_then_integrate","authority":herdr_projects::authority::policy_reference(&self.project).unwrap()
        })).unwrap();document.push(b'\n');
        let doc_path=self.home.path().join(format!("{task}-contract.json"));fs::write(&doc_path,&document).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y","sign","-f"]).arg(&self.key).args(["-n",CONTRACT_SIGNATURE_NAMESPACE]).arg(&doc_path).status().unwrap().success());
        let installed=hp(self.home.path(),&["--root",self.r(),"task","demo","contract","put","--input-file",doc_path.to_str().unwrap(),"--signature",doc_path.with_extension("json.sig").to_str().unwrap()]);
        assert!(installed.status.success(),"{}",String::from_utf8_lossy(&installed.stderr));let installed:serde_json::Value=serde_json::from_slice(&installed.stdout).unwrap();
        let objects=self.git(&["rev-list","--objects","--all"]).lines().map(|line|{let oid=line.split_whitespace().next().unwrap();serde_json::json!({"oid":oid,"relative_path":format!("{}/{}",&oid[..2],&oid[2..])})}).collect::<Vec<_>>();
        // Every worker claims success; only verification evidence may release anything.
        let submission=self.home.path().join(format!("{task}-result.json"));
        fs::write(&submission,serde_json::to_vec(&serde_json::json!({"idempotency_key":format!("{task}-key"),"task_id":task,"contract_revision":1,"contract_digest":installed["digest"],"attempt_id":format!("{task}-attempt"),
            "repository":repository,"base_oid":self.base,"candidate_oid":candidate,"object_format":"sha256",
            "artifact_manifest":[{"path":"src/lib.rs","oid":candidate}],"claimed_checks":["all checks passed"],"objects":objects})).unwrap()).unwrap();
        let submitted=hp(self.home.path(),&["--root",self.r(),"result","demo","submit","--input-file",submission.to_str().unwrap()]);
        assert!(submitted.status.success(),"{}",String::from_utf8_lossy(&submitted.stderr));
        serde_json::from_slice::<serde_json::Value>(&submitted.stdout).unwrap()["submission_id"].as_str().unwrap().to_owned()
    }
    fn automate(&self) {
        let head=herdr_projects::runtime::snapshot(&self.project).unwrap().head.to_string();
        let enabled=hp(self.home.path(),&["--root",self.r(),"result","demo","auto","--verify","on","--expected-head",&head]);
        assert!(enabled.status.success(),"{}",String::from_utf8_lossy(&enabled.stderr));
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&enabled.stdout).unwrap()["verify"],true);
    }
    fn jobs(&self)->Vec<(herdr_projects::domain::Operation,herdr_projects::operations::Delivery)> {
        let snapshot=herdr_projects::runtime::snapshot(&self.project).unwrap();
        snapshot.operations.into_iter().filter(|op|op.kind=="verification.run").map(|op|{let d=snapshot.deliveries.iter().find(|d|d.operation==op.id).unwrap().clone();(op,d)}).collect()
    }
    fn spawn(&self)->Ticker {self.spawn_with("/bin/false")}
    fn spawn_with(&self,herdr:&str)->Ticker {
        Ticker(Command::new(BIN).env_clear().env("HOME",self.home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",herdr).args(["--root",self.r(),"ticker","run"])
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap())
    }
    fn wait(&self,child:&mut Ticker,seconds:u64,predicate:&dyn Fn()->bool) {
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(seconds);
        while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(std::time::Instant::now()<deadline,"{}\nverification jobs: {}",std::fs::read_to_string(self.root.join(".ticker.log")).unwrap_or_default(),String::from_utf8_lossy(&hp(self.home.path(),&["--root",self.r(),"result","demo","jobs"]).stdout));std::thread::sleep(std::time::Duration::from_millis(10));}
    }
    fn stop(&self,child:&mut Ticker) {
        std::fs::write(self.root.join(".ticker.stop"),b"").unwrap();let deadline=std::time::Instant::now()+std::time::Duration::from_secs(8);
        while child.0.try_wait().unwrap().is_none(){assert!(std::time::Instant::now()<deadline);std::thread::sleep(std::time::Duration::from_millis(10));}
        std::fs::remove_file(self.root.join(".ticker.stop")).unwrap();
    }
    /// Retain a launchable `worker` profile over fake Herdr and agent binaries.
    /// Native interaction needs a real agent session, so only that evidence is
    /// planted, in the verifier's exact shape; launch revalidates everything else live.
    fn launchable_profile(&self)->herdr_projects::domain::VersionedReference {self.fake_launchable_profile("claude","2.1.0 (Claude Code)")}
    fn fake_launchable_profile(&self,name:&str,version:&str)->herdr_projects::domain::VersionedReference {
        use std::{fs,os::unix::fs::PermissionsExt};
        let bin=self.home.path().join("bin");let agent_home=self.home.path().join("agent-home");fs::create_dir_all(&bin).unwrap();fs::create_dir_all(&agent_home).unwrap();
        let (herdr,agent)=(bin.join("herdr"),bin.join(name));
        // Herdr answers one native request with a pong that offers workspace creation.
        fs::write(&herdr,"#!/usr/bin/python3\nimport sys,json\nif sys.argv[1:]==['--version']:print('herdr 0.9.1');sys.exit(0)\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'result':{'type':'pong','version':'0.9.1','capabilities':{'workspace_create_command':True}}}))\n").unwrap();
        fs::write(&agent,format!("#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = --version ] || exit 2\nprintf '%s\\n' '{version}'\n")).unwrap();
        for path in [&herdr,&agent] {fs::set_permissions(path,fs::Permissions::from_mode(0o700)).unwrap();}
        self.launchable_profile_with(&herdr,&agent,&agent_home)
    }
    /// Prepare `worker` over the given binaries and plant only the native
    /// interaction evidence (the verifier would need its own agent session).
    fn launchable_profile_with(&self,herdr:&Path,agent:&Path,agent_home:&Path)->herdr_projects::domain::VersionedReference {
        use std::{fs,os::unix::fs::MetadataExt};
        use herdr_projects::{domain::*,worker_supervision::{ProcessIncarnation,SupervisorIdentity}};
        use sha2::{Digest,Sha256};
        let out=hp(self.home.path(),&["--root",self.r(),"profile","prepare","demo","worker","--herdr-executable",herdr.to_str().unwrap(),
            "--agent-executable",agent.to_str().unwrap(),"--execution-home",agent_home.to_str().unwrap()]);
        assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
        let mut profile:FrozenProfile=serde_json::from_value(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["profile"].clone()).unwrap();
        #[derive(serde::Serialize)] struct Interaction{session:ResourceIdentity,terminal:&'static str,readiness_manifest:&'static str,prompt_digest:String,acknowledged_unix_ms:i64}
        #[derive(serde::Serialize)] struct Evidence{version:u32,prepared_profile:VersionedReference,supervisor:SupervisorIdentity,native_kind:String,observed_unix_ms:i64,stopped_unix_ms:i64,interaction:Interaction}
        let evidence=Evidence{version:2,prepared_profile:profile.reference().unwrap(),native_kind:profile.kind.clone(),observed_unix_ms:1000,stopped_unix_ms:1001,
            supervisor:SupervisorIdentity{version:1,boot_id:"00000000-0000-0000-0000-000000000001".into(),host_id:None,observer_namespace:(1,2),worker_namespace:(1,3),
                outer:ProcessIncarnation{pid:20,device:1,inode:4},init:ProcessIncarnation{pid:21,device:1,inode:5}},
            interaction:Interaction{session:ResourceIdentity{device:1,inode:2,born_secs:1,born_nanos:0},terminal:"fixture-terminal",readiness_manifest:"fixture-manifest",prompt_digest:"a".repeat(64),acknowledged_unix_ms:999}};
        let hash=format!("{:x}",Sha256::digest(serde_json::to_vec(&evidence).unwrap()));
        let supported=CapabilityEvidence::Supported{evidence:VersionedReference{id:format!("native-transport-{hash}"),revision:1,digest:hash}};
        let c=&mut profile.capabilities;
        (c.launch,c.stop,c.readiness_observation,c.prompt_submission)=(supported.clone(),supported.clone(),supported.clone(),supported);
        let reference=profile.reference().unwrap();
        let path=self.project.join(".state/state.db").canonicalize().unwrap();let metadata=fs::metadata(&path).unwrap();
        let report=serde_json::json!({"preparation":{"profile":profile,"reference":reference,"launchable":true,"protocol_capable":false,"certified":false},
            "evidence":evidence,"source_store":[path,metadata.dev(),metadata.ino()]}).to_string();
        self.db().execute("INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,(SELECT max(sequence) FROM events))",
            rusqlite::params![reference.digest,report,format!("{:x}",Sha256::digest(report.as_bytes()))]).unwrap();
        reference
    }
}

#[cfg(all(feature="state-store",target_os="linux"))]
#[test]
fn ticker_enqueues_one_verification_job_per_policy_across_restart() {
    use herdr_projects::operations::DeliveryState;
    use sha2::{Digest,Sha256};
    let f=VerifyFixture::new(&[("src/lib.rs","pub fn result() {}\n".into())]);
    let policies=[("builds",r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#.to_owned()),("clean",r#"{"version":1,"checks":["/usr/bin/true"]}"#.to_owned())];
    let submission_id=f.submit("task",&policies);
    // The metrics file is rewritten after every ticker turn has polled each canonical project.
    let metrics=f.root.join(".ticker-metrics.json");
    let turn=||{let _=std::fs::remove_file(&metrics);let mut child=f.spawn();f.wait(&mut child,35,&||metrics.is_file());f.stop(&mut child);};
    let runs=||f.db().query_row("SELECT count(*) FROM verification_runs",[],|row|row.get::<_,u64>(0)).unwrap();
    turn();
    assert!(f.jobs().is_empty(),"automation is off by default");
    f.automate();
    let mut expected=policies.iter().map(|(id,text)|{
        let digest=format!("{:x}",Sha256::digest(text.as_bytes()));
        format!("{:x}",Sha256::digest(serde_json::to_vec(&serde_json::json!([f.store,submission_id,1,id,digest])).unwrap()))
    }).collect::<Vec<_>>();expected.sort();
    // The first pass only enqueues; nothing has been offered or run yet.
    turn();
    let jobs=f.jobs();assert_eq!(jobs.iter().map(|(op,_)|op.id.as_str().to_owned()).collect::<Vec<_>>(),expected);
    assert!(jobs.iter().all(|(_,d)|d.state==DeliveryState::Pending&&d.attempts==0));assert_eq!(runs(),0);
    // An operator retires one job before it runs; it never runs until explicitly retried.
    let (retired,delivery)=jobs[0].clone();let head=herdr_projects::runtime::snapshot(&f.project).unwrap().head.to_string();
    let out=hp(f.home.path(),&["--root",f.r(),"operations","demo","retire",retired.id.as_str(),"--reason","fixture","--expected-revision",&delivery.revision.to_string(),"--expected-head",&head]);
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let state=|id:&herdr_projects::domain::OperationId|f.jobs().into_iter().find(|(op,_)|&op.id==id).unwrap().1;
    let mut child=f.spawn();f.wait(&mut child,90,&||f.jobs().iter().any(|(op,d)|op.id!=retired.id&&d.state==DeliveryState::Confirmed));f.stop(&mut child);
    assert_eq!(state(&retired.id).state,DeliveryState::PermanentFailure);assert_eq!(runs(),1);
    let out=hp(f.home.path(),&["--root",f.r(),"result","demo","retry-verification",retired.id.as_str(),"--expected-revision",&state(&retired.id).revision.to_string()]);
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let listed:serde_json::Value=serde_json::from_slice(&hp(f.home.path(),&["--root",f.r(),"result","demo","jobs"]).stdout).unwrap();
    assert_eq!(listed.as_array().unwrap().iter().find(|job|job["operation"]==retired.id.as_str()).unwrap()["delivery"]["state"],"pending");
    let settled=||{let jobs=f.jobs();jobs.len()==2&&jobs.iter().all(|(_,d)|d.state==DeliveryState::Confirmed)};
    let mut child=f.spawn();f.wait(&mut child,90,&settled);f.stop(&mut child);
    // Each job ran once under its own key: one run per policy, never a duplicate.
    let check=||{
        let found=f.jobs();
        assert_eq!(found.iter().map(|(op,_)|op.id.as_str().to_owned()).collect::<Vec<_>>(),expected);
        for (op,delivery) in &found {
            assert_eq!(delivery.state,DeliveryState::Confirmed);assert_eq!(delivery.attempts,1);
            assert_eq!(f.db().query_row("SELECT count(*) FROM verification_runs WHERE idempotency_key=?1",[op.id.as_str()],|row|row.get::<_,u64>(0)).unwrap(),1);
        }
        assert_eq!(runs(),2);
    };
    check();
    turn();
    check();
}

#[cfg(all(feature="state-store",target_os="linux"))]
#[test]
fn ticker_replaces_result_jobs_bound_to_an_older_task_revision() {
    use herdr_projects::operations::DeliveryState;
    let f=VerifyFixture::new(&[("src/lib.rs","pub fn result() {}\n".into())]);
    let submission_id=f.submit("task",&[("clean",r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#.to_owned())]);
    let metrics=f.root.join(".ticker-metrics.json");
    let turn=||{let _=std::fs::remove_file(&metrics);let mut child=f.spawn();f.wait(&mut child,35,&||metrics.is_file());f.stop(&mut child);};
    f.automate();
    // The first pass only enqueues, fenced on the task revision at that moment.
    turn();
    let jobs=f.jobs();assert_eq!(jobs.len(),1);let (old,delivery)=jobs[0].clone();
    assert_eq!((delivery.state,delivery.attempts),(DeliveryState::Pending,0));
    // The task revision moves before the job runs, so the old job can never be claimed.
    let snapshot=herdr_projects::runtime::snapshot(&f.project).unwrap();
    let revision=snapshot.tasks.iter().find(|t|t.id.as_str()=="task").unwrap().revision;assert_eq!(old.expected_revision,revision);
    let out=hp(f.home.path(),&["--root",f.r(),"task","demo","rename","task","--title","renamed","--expected-revision",&revision.to_string(),"--expected-head",&snapshot.head.to_string()]);
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let replaced=||f.jobs().into_iter().find(|(op,_)|op.id!=old.id);
    let mut child=f.spawn();f.wait(&mut child,90,&||replaced().is_some_and(|(_,d)|d.state==DeliveryState::Confirmed));f.stop(&mut child);
    // The old job is retired without ever being claimed, and says why.
    let listed:serde_json::Value=serde_json::from_slice(&hp(f.home.path(),&["--root",f.r(),"result","demo","jobs"]).stdout).unwrap();
    let retired=listed.as_array().unwrap().iter().find(|job|job["operation"]==old.id.as_str()).unwrap();
    assert_eq!(retired["delivery"]["state"],"permanent_failure");assert_eq!(retired["delivery"]["attempts"],0);
    assert!(retired["delivery"]["last_outcome"].to_string().contains("task_revision_changed"),"{retired}");
    // The replacement is bound to the new revision and ran once under its own key to an accepted verdict.
    let (new,delivery)=replaced().unwrap();
    assert_eq!(new.expected_revision,revision+1);assert_eq!(new.payload["submission_id"],submission_id.as_str());assert_eq!(delivery.attempts,1);
    let (key,state,reason):(String,String,Option<String>)=f.db().query_row("SELECT idempotency_key,state,reason FROM verification_runs WHERE submission_id=?1",[&submission_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!((key.as_str(),state.as_str(),reason),(new.id.as_str(),"accepted",None));
    // Nothing further is enqueued or retired on a later turn.
    turn();
    assert_eq!(f.jobs().len(),2);
    assert_eq!(f.db().query_row("SELECT count(*) FROM verification_runs",[],|row|row.get::<_,u64>(0)).unwrap(),1);
    // An integration job fenced on a revision that later moves is replaced the same way.
    f.git(&["branch","integration",&f.base]);
    let configured=hp(f.home.path(),&["--root",f.r(),"result","demo","configure-integration","--repository",f.repo.to_str().unwrap(),"--reference","refs/heads/integration"]);
    assert!(configured.status.success(),"{}",String::from_utf8_lossy(&configured.stderr));
    let head=herdr_projects::runtime::snapshot(&f.project).unwrap().head.to_string();
    assert!(hp(f.home.path(),&["--root",f.r(),"result","demo","auto","--integrate","on","--expected-head",&head]).status.success());
    let integrations=||{let snapshot=herdr_projects::runtime::snapshot(&f.project).unwrap();snapshot.operations.into_iter().filter(|op|op.kind=="integration.run")
        .map(|op|{let d=snapshot.deliveries.iter().find(|d|d.operation==op.id).unwrap().clone();(op,d)}).collect::<Vec<_>>()};
    turn();
    let found=integrations();assert_eq!(found.len(),1);let (old,delivery)=found[0].clone();
    assert_eq!((old.expected_revision,delivery.state,delivery.attempts),(revision+1,DeliveryState::Pending,0));
    let head=herdr_projects::runtime::snapshot(&f.project).unwrap().head.to_string();
    let out=hp(f.home.path(),&["--root",f.r(),"task","demo","rename","task","--title","again","--expected-revision",&(revision+1).to_string(),"--expected-head",&head]);
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let replaced=||integrations().into_iter().find(|(op,_)|op.id!=old.id);
    let mut child=f.spawn();f.wait(&mut child,90,&||replaced().is_some_and(|(_,d)|d.state==DeliveryState::Confirmed));f.stop(&mut child);
    let (new,_)=replaced().unwrap();assert_eq!(new.expected_revision,revision+2);
    let listed:serde_json::Value=serde_json::from_slice(&hp(f.home.path(),&["--root",f.r(),"result","demo","jobs"]).stdout).unwrap();
    let retired=listed.as_array().unwrap().iter().find(|job|job["operation"]==old.id.as_str()).unwrap();
    assert_eq!((retired["delivery"]["state"].as_str(),retired["delivery"]["attempts"].as_u64()),(Some("permanent_failure"),Some(0)));
    assert!(retired["delivery"]["last_outcome"].to_string().contains("task_revision_changed"),"{retired}");
    assert_eq!(f.db().query_row("SELECT count(*) FROM integrated_commits",[],|row|row.get::<_,u64>(0)).unwrap(),1);
}

#[cfg(all(feature="state-store",target_os="linux"))]
#[test]
fn ticker_auto_verifies_once_and_recovers_after_kill() {
    use std::{io::{Read,Write},sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}}};
    use herdr_projects::{migration,runtime,operations::DeliveryState};
    // The check speaks git's native protocol to a local fixture that holds each
    // reply until released, so the ticker can be killed while the check runs.
    let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();let port=listener.local_addr().unwrap().port();
    let released=Arc::new(AtomicBool::new(false));let connections=Arc::new(AtomicUsize::new(0));
    {let released=released.clone();let connections=connections.clone();std::thread::spawn(move||for stream in listener.incoming(){
        let Ok(mut stream)=stream else{continue};connections.fetch_add(1,Ordering::SeqCst);let released=released.clone();
        std::thread::spawn(move||{let mut buffer=[0u8;4096];let _=stream.read(&mut buffer);while !released.load(Ordering::SeqCst){std::thread::sleep(std::time::Duration::from_millis(10));}let _=stream.write_all(b"0000");let _=stream.read(&mut buffer);});
    });}
    // Three separated conflicting hunks: `git merge-file -p` exits 3 without writing.
    let hunks=|side:&str|(1..=3).map(|n|format!("{side}{n}\n{}",(1..=8).map(|m|format!("same{n}{m}\n")).collect::<String>())).collect::<String>();
    let f=VerifyFixture::new(&[("src/lib.rs","pub fn result() {}\n".into()),("src/m-base.txt",hunks("base")),("src/m-ours.txt",hunks("ours")),("src/m-theirs.txt",hunks("theirs"))]);
    let wait_submission=f.submit("wait",&[("waits",format!(r#"{{"version":1,"checks":["/usr/bin/git","ls-remote","git://127.0.0.1:{port}/fixture"]}}"#))]);
    let fail_submission=f.submit("fail",&[("exits-3",r#"{"version":1,"checks":["/usr/bin/git","merge-file","-p","src/m-ours.txt","src/m-base.txt","src/m-theirs.txt"]}"#.to_owned())]);
    for (consumer,predecessor) in [("wait-consumer","wait"),("fail-consumer","fail")] {
        let head=runtime::add_task(&f.project,herdr_projects::domain::TaskId::new(consumer).unwrap(),consumer.into(),runtime::snapshot(&f.project).unwrap().head).unwrap();
        let request=f.home.path().join(format!("{consumer}.json"));
        std::fs::write(&request,serde_json::json!({"priority":0,"dependencies":[{"predecessor":predecessor,"requirement":"verified_result"}]}).to_string()).unwrap();
        let queued=hp(f.home.path(),&["--root",f.r(),"task","demo","queue",consumer,"--input-file",request.to_str().unwrap(),"--expected-revision","1","--expected-head",&head.to_string()]);
        assert!(queued.status.success(),"{}",String::from_utf8_lossy(&queued.stderr));
    }
    f.automate();
    let job=|submission:&str|f.jobs().into_iter().find(|(op,_)|op.payload["submission_id"]==submission);
    let runs=|submission:&str|f.db().query_row("SELECT count(*) FROM verification_runs WHERE submission_id=?1",[submission],|row|row.get::<_,u64>(0)).unwrap();
    let scratch=f.project.join(".verify-scratch");
    let mut child=f.spawn();
    f.wait(&mut child,90,&||connections.load(Ordering::SeqCst)>=1&&job(&wait_submission).is_some_and(|(_,d)|d.state==DeliveryState::Claimed));
    let (op,_)=job(&wait_submission).unwrap();
    assert!(scratch.join(op.id.as_str()).is_dir(),"the running job uses its deterministic scratch directory");
    child.0.kill().unwrap();child.0.wait().unwrap();
    // The orphaned check may finish, but only the killed controller could have recorded it.
    released.store(true,Ordering::SeqCst);
    let (_,claimed)=job(&wait_submission).unwrap();assert_eq!(claimed.state,DeliveryState::Claimed);assert_eq!(runs(&wait_submission),0);
    migration::open_active(&f.project).unwrap().expire_claims(claimed.lease_until_ms.unwrap()+1).unwrap();
    assert_eq!(job(&wait_submission).unwrap().1.state,DeliveryState::Ambiguous);
    let mut child=f.spawn();
    f.wait(&mut child,120,&||f.jobs().len()==2&&f.jobs().iter().all(|(_,d)|d.state==DeliveryState::Confirmed));
    f.stop(&mut child);
    let raw=f.db();
    // The lost reply was observed by key and redelivered under the same key: exactly one run and one receipt.
    let (op,delivery)=job(&wait_submission).unwrap();assert_eq!(delivery.attempts,2);
    let (state,key,exit_status):(String,String,Option<i64>)=raw.query_row("SELECT state,idempotency_key,exit_status FROM verification_runs WHERE submission_id=?1",[&wait_submission],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!((state.as_str(),key.as_str(),exit_status),("accepted",op.id.as_str(),Some(0)));
    assert_eq!(runs(&wait_submission),1);
    assert_eq!(raw.query_row("SELECT count(*) FROM verified_results WHERE submission_id=?1",[&wait_submission],|row|row.get::<_,u64>(0)).unwrap(),1);
    let valid=|task:&str|raw.query_row("SELECT count(*) FROM dependency_satisfactions WHERE task_id=?1 AND state='valid'",[task],|row|row.get::<_,u64>(0)).unwrap();
    assert_eq!(valid("wait-consumer"),1);
    // A policy exiting 3 is a recorded rejection with its real status; it releases nothing.
    let (state,reason,exit_status):(String,Option<String>,Option<i64>)=raw.query_row("SELECT state,reason,exit_status FROM verification_runs WHERE submission_id=?1",[&fail_submission],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!((state.as_str(),reason.as_deref(),exit_status),("rejected",Some("checks_failed"),Some(3)));
    assert_eq!(runs(&fail_submission),1);
    assert_eq!(raw.query_row("SELECT count(*) FROM verified_results WHERE submission_id=?1",[&fail_submission],|row|row.get::<_,u64>(0)).unwrap(),0);
    assert_eq!(valid("fail-consumer"),0);
    assert_eq!(std::fs::read_dir(&scratch).map(|entries|entries.count()).unwrap_or(0),0,"scratch is removed after every job");
}

#[cfg(all(feature="state-store",target_os="linux"))]
#[test]
fn ticker_auto_verification_releases_project_ownership_during_the_check() {
    use std::{io::{Read,Write},sync::{Arc,atomic::{AtomicUsize,Ordering}}};
    use herdr_projects::{migration,runtime,operations::DeliveryState};
    // A local git-protocol fixture holds connection `n` until `released > n`.
    let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();let port=listener.local_addr().unwrap().port();
    let released=Arc::new(AtomicUsize::new(0));let connections=Arc::new(AtomicUsize::new(0));
    {let released=released.clone();let connections=connections.clone();std::thread::spawn(move||for stream in listener.incoming(){
        let Ok(mut stream)=stream else{continue};let index=connections.fetch_add(1,Ordering::SeqCst);let released=released.clone();
        std::thread::spawn(move||{let mut buffer=[0u8;4096];let _=stream.read(&mut buffer);while released.load(Ordering::SeqCst)<=index{std::thread::sleep(std::time::Duration::from_millis(10));}let _=stream.write_all(b"0000");let _=stream.read(&mut buffer);});
    });}
    let f=VerifyFixture::new(&[("src/lib.rs","pub fn result() {}\n".into())]);
    let waits=[("waits",format!(r#"{{"version":1,"checks":["/usr/bin/git","ls-remote","git://127.0.0.1:{port}/fixture"]}}"#))];
    let first=f.submit("first",&waits);
    runtime::add_task(&f.project,herdr_projects::domain::TaskId::new("other").unwrap(),"other".into(),runtime::snapshot(&f.project).unwrap().head).unwrap();
    f.automate();
    let job=|submission:&str|f.jobs().into_iter().find(|(op,_)|op.payload["submission_id"]==submission);
    let runs=|submission:&str|f.db().query_row("SELECT count(*) FROM verification_runs WHERE submission_id=?1",[submission],|row|row.get::<_,u64>(0)).unwrap();
    // Renaming needs project ownership. The ticker's own turn holds it briefly,
    // so only a lock held for the whole check makes every attempt fail.
    let rename=|task:&str,title:&str|{
        for _ in 0..40 {
            let snapshot=runtime::snapshot(&f.project).unwrap();let revision=snapshot.tasks.iter().find(|t|t.id.as_str()==task).unwrap().revision;
            let out=hp(f.home.path(),&["--root",f.r(),"task","demo","rename",task,"--title",title,"--expected-revision",&revision.to_string(),"--expected-head",&snapshot.head.to_string()]);
            if out.status.success() {return;}
            let stderr=String::from_utf8_lossy(&out.stderr).into_owned();
            assert!(stderr.contains("owns"),"{stderr}");std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("the running verification check kept project ownership");
    };
    let scratch=f.project.join(".verify-scratch");
    let mut child=f.spawn();
    f.wait(&mut child,90,&||connections.load(Ordering::SeqCst)>=1&&job(&first).is_some_and(|(_,d)|d.state==DeliveryState::Claimed));
    // While the isolated check runs, another project effect proceeds, but
    // root-exclusive maintenance is still refused.
    rename("other","renamed during the check");
    assert!(herdr_projects::execution_guard::RootGuard::exclusive(&f.root).is_err(),"the check keeps the root shared");
    assert_eq!(runs(&first),0);
    released.store(1,Ordering::SeqCst);
    f.wait(&mut child,90,&||job(&first).is_some_and(|(_,d)|d.state==DeliveryState::Confirmed));
    f.stop(&mut child);
    let (op,delivery)=job(&first).unwrap();assert_eq!(delivery.attempts,1);
    let (state,key):(String,String)=f.db().query_row("SELECT state,idempotency_key FROM verification_runs WHERE submission_id=?1",[&first],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!((state.as_str(),key.as_str(),runs(&first)),("accepted",op.id.as_str(),1));
    assert_eq!(runtime::snapshot(&f.project).unwrap().tasks.iter().find(|t|t.id.as_str()=="other").unwrap().title,"renamed during the check");
    // The job's own task moves during the check: no verdict is recorded, and
    // recovery replaces the job with one fenced on the new revision.
    let second=f.submit("second",&waits);
    let mut child=f.spawn();
    f.wait(&mut child,90,&||connections.load(Ordering::SeqCst)>=2&&job(&second).is_some_and(|(_,d)|d.state==DeliveryState::Claimed));
    let (old,_)=job(&second).unwrap();
    rename("second","moved during the check");
    released.store(2,Ordering::SeqCst);
    f.wait(&mut child,90,&||!scratch.join(old.id.as_str()).exists());
    f.stop(&mut child);
    let (_,claimed)=job(&second).unwrap();
    assert_eq!((claimed.state,runs(&second)),(DeliveryState::Claimed,0),"a stale check records nothing");
    migration::open_active(&f.project).unwrap().expire_claims(claimed.lease_until_ms.unwrap()+1).unwrap();
    released.store(usize::MAX,Ordering::SeqCst);
    let replaced=||f.jobs().into_iter().find(|(op,_)|op.payload["submission_id"]==second.as_str()&&op.id!=old.id);
    let mut child=f.spawn();
    f.wait(&mut child,120,&||replaced().is_some_and(|(_,d)|d.state==DeliveryState::Confirmed));
    f.stop(&mut child);
    let retired=f.jobs().into_iter().find(|(op,_)|op.id==old.id).unwrap().1;
    assert_eq!(retired.state,DeliveryState::PermanentFailure);
    assert!(serde_json::to_string(&retired.last_outcome).unwrap().contains("task_revision_changed"),"{:?}",retired.last_outcome);
    let (new,_)=replaced().unwrap();
    let (state,key):(String,String)=f.db().query_row("SELECT state,idempotency_key FROM verification_runs WHERE submission_id=?1",[&second],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!((state.as_str(),key.as_str(),runs(&second)),("accepted",new.id.as_str(),1));
    assert_eq!(std::fs::read_dir(&scratch).map(|entries|entries.count()).unwrap_or(0),0,"scratch is removed after every job");
}

#[cfg(all(feature="state-store",target_os="linux"))]
#[test]
fn ticker_auto_integrates_two_results_serially_and_recovers_stale_and_crash() {
    use std::{io::{Read,Write},sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}}};
    use herdr_projects::{migration,runtime,operations::{DeliveryState,Outcome}};
    // A local git-protocol fixture holds only the second connection (the
    // integration candidate check of `two`) until released.
    let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();let port=listener.local_addr().unwrap().port();
    let released=Arc::new(AtomicBool::new(false));let connections=Arc::new(AtomicUsize::new(0));
    {let released=released.clone();let connections=connections.clone();std::thread::spawn(move||for stream in listener.incoming(){
        let Ok(mut stream)=stream else{continue};let index=connections.fetch_add(1,Ordering::SeqCst);let released=released.clone();
        std::thread::spawn(move||{let mut buffer=[0u8;4096];let _=stream.read(&mut buffer);while index==1&&!released.load(Ordering::SeqCst){std::thread::sleep(std::time::Duration::from_millis(10));}let _=stream.write_all(b"0000");let _=stream.read(&mut buffer);});
    });}
    let lib="pub fn result() {}\n";
    let f=VerifyFixture::new(&[("src/lib.rs",lib.into()),("src/one.txt","one\n".into())]);
    // Independent candidates on the same base; each adds the same required output.
    let candidate=|name:&str|{f.git(&["checkout","-q","-b",name,&f.base]);std::fs::write(f.repo.join("src/lib.rs"),lib).unwrap();std::fs::write(f.repo.join(format!("src/{name}.txt")),name).unwrap();f.git(&["add","."]);f.git(&["commit","-qm",name]);f.git(&["rev-parse","HEAD"])};
    let clean=[("clean",r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#.to_owned())];
    let waits=[("waits",format!(r#"{{"version":1,"checks":["/usr/bin/git","ls-remote","git://127.0.0.1:{port}/fixture"]}}"#))];
    let c2=candidate("two");
    let one=f.submit("one",&clean);let two=f.submit_at("two",&waits,&c2);
    const TARGET:&str="refs/heads/integration";
    f.git(&["branch","integration",&f.base]);
    let configured=hp(f.home.path(),&["--root",f.r(),"result","demo","configure-integration","--repository",f.repo.to_str().unwrap(),"--reference",TARGET]);
    assert!(configured.status.success(),"{}",String::from_utf8_lossy(&configured.stderr));
    let auto=|args:&[&str]|{let head=runtime::snapshot(&f.project).unwrap().head.to_string();let mut all=vec!["--root",f.r(),"result","demo","auto"];all.extend_from_slice(args);all.extend(["--expected-head",&head]);
        let out=hp(f.home.path(),&all);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()};
    let control=auto(&["--verify","on"]);assert_eq!((control["verify"].clone(),control["integrate"].clone()),(true.into(),false.into()),"integration is off by default");
    assert_eq!(auto(&["--integrate","on"])["integrate"],true);
    let job=|submission:&str|{let snapshot=runtime::snapshot(&f.project).unwrap();snapshot.operations.into_iter().find(|op|op.kind=="integration.run"&&op.payload["submission_id"]==submission)
        .map(|op|{let d=snapshot.deliveries.iter().find(|d|d.operation==op.id).unwrap().clone();(op,d)})};
    let tip=||f.git(&["rev-parse",TARGET]);
    let integrated=||f.db().query_row("SELECT count(*) FROM integrated_commits",[],|row|row.get::<_,u64>(0)).unwrap();
    let mut child=f.spawn();
    f.wait(&mut child,150,&||connections.load(Ordering::SeqCst)>=2&&job(&two).is_some_and(|(_,d)|d.state==DeliveryState::Claimed));
    // `two` is killed while its candidate checks run; the target admits no other job meanwhile.
    let (op,_)=job(&two).unwrap();
    assert_eq!(f.db().query_row("SELECT state FROM integration_operations WHERE idempotency_key=?1",[op.id.as_str()],|row|row.get::<_,String>(0)).unwrap(),"candidate_prepared");
    let before=integrated();assert!(job(&one).is_none_or(|(_,d)|d.state==DeliveryState::Confirmed&&before==1));
    child.0.kill().unwrap();child.0.wait().unwrap();released.store(true,Ordering::SeqCst);
    let (_,claimed)=job(&two).unwrap();assert_eq!(claimed.state,DeliveryState::Claimed);assert_eq!(integrated(),before);
    migration::open_active(&f.project).unwrap().expire_claims(claimed.lease_until_ms.unwrap()+1).unwrap();
    let mut child=f.spawn();
    f.wait(&mut child,150,&||[&one,&two].iter().all(|s|job(s).is_some_and(|(_,d)|d.state==DeliveryState::Confirmed)));
    f.stop(&mut child);
    // Serial on the target: the second publication's first parent is the first.
    assert_eq!(integrated(),2);
    let landed=f.db().prepare("SELECT commit_oid FROM integrated_commits ORDER BY created_unix_ms,rowid").unwrap().query_map([],|row|row.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    let (first,second)=(landed[0].clone(),landed[1].clone());assert_eq!(tip(),second);
    assert_eq!(f.git(&["rev-parse",&format!("{first}^1")]),f.base);assert_eq!(f.git(&["rev-parse",&format!("{second}^1")]),first);
    let mut merged=[f.git(&["rev-parse",&format!("{first}^2")]),f.git(&["rev-parse",&format!("{second}^2")])];merged.sort();
    let mut candidates=[f.candidate.clone(),c2.clone()];candidates.sort();assert_eq!(merged,candidates);
    assert_eq!(f.git(&["show",&format!("{second}:src/one.txt")]),"one");assert_eq!(f.git(&["show",&format!("{second}:src/two.txt")]),"two");
    let raw=f.db();
    assert_eq!(raw.query_row("SELECT count(*) FROM integration_operations WHERE idempotency_key=?1",[op.id.as_str()],|row|row.get::<_,u64>(0)).unwrap(),1);
    assert_eq!(raw.query_row("SELECT count(*) FROM integration_candidates c JOIN integration_operations o ON o.operation_id=c.operation_id WHERE o.idempotency_key=?1",[op.id.as_str()],|row|row.get::<_,u64>(0)).unwrap(),1);
    assert!(landed.contains(&raw.query_row("SELECT commit_oid FROM integrated_commits i JOIN integration_operations o ON o.operation_id=i.operation_id WHERE o.idempotency_key=?1",[op.id.as_str()],|row|row.get::<_,String>(0)).unwrap()));
    assert_eq!(job(&two).unwrap().1.attempts,2);
    assert_eq!(std::fs::read_dir(f.project.join(".integrate-scratch")).map(|entries|entries.count()).unwrap_or(0),0,"scratch is removed after every job");
    // Submission history does not enter the producer's turn: thousands of
    // unrelated submissions leave its pending projection empty.
    raw.execute("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<3000)
        INSERT INTO result_submissions SELECT printf('%064x',i),project_store,'seed-'||i,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,memory_snapshot_id,artifact_manifest,claimed_checks,created_unix_ms
        FROM n,result_submissions WHERE submission_id=?1",[&one]).unwrap();
    raw.execute("DELETE FROM pending_verification_work WHERE submission_id GLOB '0000*'",[]).unwrap();
    let pending=||f.db().query_row("SELECT count(*) FROM pending_integration_work",[],|row|row.get::<_,u64>(0)).unwrap();assert_eq!(pending(),0);
    // The target moves outside the controller between verification and integration.
    auto(&["--integrate","off"]);
    // Every policy of a contract is rechecked on the integrated candidate; each
    // of these passes alone and fails only once the other result is combined.
    let empty=f.git(&["hash-object","-t","tree","/dev/null"]);
    let absent=|id:&str,path:&str|(id.to_owned(),format!(r#"{{"version":1,"checks":["/usr/bin/git","diff","--quiet","{empty}","HEAD","--","{path}"]}}"#));
    let (no_four,no_three)=(absent("no-four","src/four.txt"),absent("no-three","src/three.txt"));
    let c3=candidate("three");let three=f.submit_at("three",&[(clean[0].0,clean[0].1.clone()),(no_four.0.as_str(),no_four.1.clone())],&c3);
    let verified=||{let runs=f.jobs().into_iter().filter(|(op,_)|op.payload["submission_id"]==three.as_str()).collect::<Vec<_>>();runs.len()==2&&runs.iter().all(|(_,d)|d.state==DeliveryState::Confirmed)};
    let mut child=f.spawn();f.wait(&mut child,90,&verified);f.stop(&mut child);
    assert!(job(&three).is_none());
    let moved=f.git(&["commit-tree",&format!("{second}^{{tree}}"),"-p",&second,"-m","outside the controller"]);
    f.git(&["update-ref",TARGET,&moved,&second]);
    // A target checked out in a user worktree is refused before any build.
    let worktree=f.home.path().join("user-worktree");f.git(&["worktree","add","-q",worktree.to_str().unwrap(),"integration"]);
    auto(&["--integrate","on"]);
    let refused=||job(&three).is_some_and(|(_,d)|matches!(&d.last_outcome,Some(Outcome::Retryable{no_effect_evidence}) if no_effect_evidence.contains("checked out")));
    let mut child=f.spawn();f.wait(&mut child,90,&refused);
    assert_eq!(tip(),moved);assert_eq!(integrated(),2);
    f.git(&["worktree","remove",worktree.to_str().unwrap()]);
    f.wait(&mut child,90,&||job(&three).is_some_and(|(_,d)|d.state==DeliveryState::PermanentFailure));f.stop(&mut child);
    let Some(Outcome::PermanentFailure{diagnostic})=job(&three).unwrap().1.last_outcome else {panic!("stale job is blocked")};
    assert!(diagnostic.contains("target moved"),"{diagnostic}");
    let listed=String::from_utf8(hp(f.home.path(),&["--root",f.r(),"result","demo","jobs"]).stdout).unwrap();assert!(listed.contains("target moved"),"{listed}");
    assert_eq!(tip(),moved,"the moved ref is not overwritten");assert_eq!(integrated(),2);
    assert_eq!(f.db().query_row("SELECT count(*) FROM integration_operations",[],|row|row.get::<_,u64>(0)).unwrap(),2,"nothing is rebuilt");
    assert_eq!(pending(),0);
    // Operator retry: only a permanently failed integration job, at its revision.
    assert!(listed.contains("retry-integration"),"{listed}");
    let retry=|submission:&str,revision:u64|{let (op,_)=job(submission).unwrap();hp(f.home.path(),&["--root",f.r(),"result","demo","retry-integration",op.id.as_str(),"--expected-revision",&revision.to_string()])};
    let revision=|submission:&str|job(submission).unwrap().1.revision;
    assert!(!retry(&one,revision(&one)).status.success(),"a confirmed job is not retried");
    assert!(!retry(&three,revision(&three)+1).status.success(),"a stale revision is refused");
    // Retried before the cause is resolved, staleness is rechecked: the moved ref is not overwritten.
    let out=retry(&three,revision(&three));assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    assert_eq!(job(&three).unwrap().1.state,DeliveryState::Pending);
    let mut child=f.spawn();f.wait(&mut child,90,&||job(&three).is_some_and(|(_,d)|d.state==DeliveryState::PermanentFailure));f.stop(&mut child);
    assert_eq!(tip(),moved);assert_eq!(integrated(),2);
    // The operator resets the target; the retried job publishes under the same key.
    f.git(&["update-ref",TARGET,&second,&moved]);
    let out=retry(&three,revision(&three));assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let mut child=f.spawn();f.wait(&mut child,90,&||job(&three).is_some_and(|(_,d)|d.state==DeliveryState::Confirmed));f.stop(&mut child);
    let third=tip();assert_eq!(integrated(),3);assert_eq!(f.git(&["rev-parse",&format!("{third}^1")]),second);
    // The receipt lists every policy that ran on the integrated candidate.
    let checks=|submission:&str|{let (op,_)=job(submission).unwrap();f.db().prepare("SELECT p.policy_id,p.passed FROM integration_policy_checks p JOIN integration_operations o ON o.operation_id=p.operation_id WHERE o.idempotency_key=?1 ORDER BY p.policy_id")
        .unwrap().query_map([op.id.as_str()],|row|Ok((row.get::<_,String>(0)?,row.get::<_,bool>(1)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()};
    assert_eq!(checks(&three),[("clean".to_owned(),true),("no-four".to_owned(),true)]);
    assert_eq!(f.db().query_row("SELECT count(*) FROM integrated_commits i JOIN integration_policy_checks p ON p.operation_id=i.operation_id WHERE i.commit_oid=?1 AND p.passed=1",[&third],|row|row.get::<_,u64>(0)).unwrap(),2);
    // `four` passes both policies alone; combined with `three`, its second policy fails.
    let c4=candidate("four");let four=f.submit_at("four",&[(clean[0].0,clean[0].1.clone()),(no_three.0.as_str(),no_three.1.clone())],&c4);
    let mut child=f.spawn();f.wait(&mut child,150,&||job(&four).is_some_and(|(_,d)|d.state==DeliveryState::PermanentFailure));f.stop(&mut child);
    assert!(f.jobs().iter().filter(|(op,_)|op.payload["submission_id"]==four.as_str()).all(|(_,d)|d.state==DeliveryState::Confirmed),"both policies pass alone");
    let Some(Outcome::PermanentFailure{diagnostic})=job(&four).unwrap().1.last_outcome else {panic!("combined check fails")};
    assert!(diagnostic.contains("checks_failed")&&diagnostic.contains("no-three"),"{diagnostic}");
    assert_eq!(checks(&four),[("clean".to_owned(),true),("no-three".to_owned(),false)]);
    assert_eq!(tip(),third,"nothing is published");assert_eq!(integrated(),3);
}

#[cfg(all(feature="state-store",target_os="linux"))]
#[test]
fn ticker_auto_chain_releases_verified_integrated_and_fan_in_dependents() {
    use std::{io::{Read,Write},sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}}};
    use herdr_projects::{migration,runtime,domain::TaskId,operations::DeliveryState};
    // Only `a`'s policy speaks git's native protocol to this local fixture: its
    // first connection is verification, its second the integration candidate
    // check, which is held so the ticker can be killed between the two.
    let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();let port=listener.local_addr().unwrap().port();
    let held=Arc::new(AtomicBool::new(true));let connections=Arc::new(AtomicUsize::new(0));
    {let held=held.clone();let connections=connections.clone();std::thread::spawn(move||for stream in listener.incoming(){
        let Ok(mut stream)=stream else{continue};let index=connections.fetch_add(1,Ordering::SeqCst);let held=held.clone();
        std::thread::spawn(move||{let mut buffer=[0u8;4096];let _=stream.read(&mut buffer);while index==1&&held.load(Ordering::SeqCst){std::thread::sleep(std::time::Duration::from_millis(10));}let _=stream.write_all(b"0000");let _=stream.read(&mut buffer);});
    });}
    let lib="pub fn result() {}\n";
    let f=VerifyFixture::new(&[("src/lib.rs",lib.into()),("src/a.txt","a\n".into())]);
    f.git(&["checkout","-q","-b","e",&f.base]);std::fs::write(f.repo.join("src/lib.rs"),lib).unwrap();std::fs::write(f.repo.join("src/e.txt"),"e\n").unwrap();
    f.git(&["add","."]);f.git(&["commit","-qm","e"]);let e_candidate=f.git(&["rev-parse","HEAD"]);
    let clean=[("clean",r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#.to_owned())];
    let waits=[("waits",format!(r#"{{"version":1,"checks":["/usr/bin/git","ls-remote","git://127.0.0.1:{port}/fixture"]}}"#))];
    // Both workers submit and claim success; that claim is all that exists so far.
    let a=f.submit("a",&waits);let e=f.submit_at("e",&clean,&e_candidate);
    for (consumer,edges) in [("b",vec![("a","verified_result")]),("c",vec![("a","integrated_commit")]),("d",vec![("a","integrated_commit"),("e","integrated_commit")])] {
        let head=runtime::add_task(&f.project,TaskId::new(consumer).unwrap(),format!("consumer {consumer}"),runtime::snapshot(&f.project).unwrap().head).unwrap();
        let request=f.home.path().join(format!("{consumer}-queue.json"));
        std::fs::write(&request,serde_json::json!({"priority":0,"dependencies":edges.iter().map(|(p,r)|serde_json::json!({"predecessor":p,"requirement":r})).collect::<Vec<_>>()}).to_string()).unwrap();
        let queued=hp(f.home.path(),&["--root",f.r(),"task","demo","queue",consumer,"--input-file",request.to_str().unwrap(),"--expected-revision","1","--expected-head",&head.to_string()]);
        assert!(queued.status.success(),"{}",String::from_utf8_lossy(&queued.stderr));
    }
    const TARGET:&str="refs/heads/integration";
    f.git(&["branch","integration",&f.base]);
    let configured=hp(f.home.path(),&["--root",f.r(),"result","demo","configure-integration","--repository",f.repo.to_str().unwrap(),"--reference",TARGET]);
    assert!(configured.status.success(),"{}",String::from_utf8_lossy(&configured.stderr));
    // Dependency blockers of one queued consumer, sorted. Factory admission stays
    // off (the default), so an edge whose evidence counts reports only
    // `admission_disabled:<edge>`; missing evidence reports `..._unavailable`.
    let dependency=|task:&str|{
        let out=hp(f.home.path(),&["--root",f.r(),"scheduler","demo","inspect"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
        let report:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();
        let entry=report["entries"].as_array().unwrap().iter().find(|entry|entry["task"]==task).unwrap().clone();
        let mut found=entry["blockers"].as_array().unwrap().iter().map(|s|s.as_str().unwrap().to_owned())
            .filter(|s|["verified_dependency_evidence_unavailable:","predecessor_failed:","admission_disabled:"].iter().any(|p|s.starts_with(p))).collect::<Vec<_>>();
        found.sort();found
    };
    let missing=|p:&str,r:&str|format!("verified_dependency_evidence_unavailable:{p}:{r}");
    let counts=|r:&str|format!("admission_disabled:{r}");
    let satisfactions=||f.db().prepare("SELECT task_id,predecessor_task,requirement,state,evidence_id FROM dependency_satisfactions ORDER BY task_id,predecessor_task,requirement,created_unix_ms")
        .unwrap().query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    let count=|sql:&str|f.db().query_row(sql,[],|row|row.get::<_,u64>(0)).unwrap();
    // No false release: submissions and worker success claims release nothing.
    assert_eq!(dependency("b"),[missing("a","verified_result")]);
    assert_eq!(dependency("c"),[missing("a","integrated_commit")]);
    assert_eq!(dependency("d"),[missing("a","integrated_commit"),missing("e","integrated_commit")]);
    assert!(satisfactions().is_empty());assert_eq!(count("SELECT count(*) FROM verification_runs"),0);
    for switch in ["--verify","--integrate"] {
        let head=runtime::snapshot(&f.project).unwrap().head.to_string();
        let out=hp(f.home.path(),&["--root",f.r(),"result","demo","auto",switch,"on","--expected-head",&head]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    }
    let job=|submission:&str|{let snapshot=runtime::snapshot(&f.project).unwrap();snapshot.operations.into_iter().find(|op|op.kind=="integration.run"&&op.payload["submission_id"]==submission)
        .map(|op|{let d=snapshot.deliveries.iter().find(|d|d.operation==op.id).unwrap().clone();(op,d)})};
    let integrated_by=|task:&str|f.db().prepare("SELECT i.integrated_id,i.commit_oid FROM integrated_commits i JOIN integration_operations o ON o.operation_id=i.operation_id
        JOIN verified_results v ON v.result_id=o.verified_result_id JOIN verification_runs r ON r.run_id=v.run_id WHERE r.task_id=?1").unwrap()
        .query_map([task],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    let verified=|submission:&str|f.db().query_row("SELECT result_id FROM verified_results WHERE submission_id=?1",[submission],|row|row.get::<_,String>(0)).unwrap();
    // Only the ticker works from here. It is killed while `a` is verified but not yet integrated.
    let mut child=f.spawn();
    f.wait(&mut child,150,&||connections.load(Ordering::SeqCst)>=2&&job(&a).is_some_and(|(_,d)|d.state==DeliveryState::Claimed));
    assert!(integrated_by("a").is_empty());
    let a_result=verified(&a);
    assert_eq!(dependency("b"),[counts("verified_result")],"a verified result releases its dependent before integration");
    assert_eq!(dependency("c"),[missing("a","integrated_commit")],"verification alone does not release an integrated-commit edge");
    assert!(dependency("d").contains(&missing("a","integrated_commit")));
    let rows=satisfactions();
    assert_eq!(rows[0],("b".into(),"a".into(),"verified_result".into(),"valid".into(),a_result.clone()));
    assert!(rows.iter().all(|(task,predecessor,..)|task=="b"||(task=="d"&&predecessor=="e")),"{rows:?}");
    child.0.kill().unwrap();child.0.wait().unwrap();held.store(false,Ordering::SeqCst);
    let (_,claimed)=job(&a).unwrap();assert_eq!(claimed.state,DeliveryState::Claimed);assert!(integrated_by("a").is_empty());
    migration::open_active(&f.project).unwrap().expire_claims(claimed.lease_until_ms.unwrap()+1).unwrap();
    let mut child=f.spawn();
    f.wait(&mut child,150,&||[&a,&e].iter().all(|s|job(s).is_some_and(|(_,d)|d.state==DeliveryState::Confirmed)));
    f.stop(&mut child);
    // One run, one verified result, one integration and one publication per predecessor, across the kill.
    assert_eq!(count("SELECT count(*) FROM verification_runs"),2);assert_eq!(count("SELECT count(*) FROM verified_results"),2);
    assert_eq!(count("SELECT count(*) FROM integration_operations"),2);assert_eq!(count("SELECT count(*) FROM integrated_commits"),2);
    assert!(f.jobs().iter().all(|(_,d)|d.state==DeliveryState::Confirmed&&d.attempts==1));assert_eq!(job(&a).unwrap().1.attempts,2);
    let (a_integrated,a_commit)=integrated_by("a").pop().unwrap();let (e_integrated,e_commit)=integrated_by("e").pop().unwrap();
    let tip=f.git(&["rev-parse",TARGET]);
    // The integrated-commit dependent is released on `a`'s integrated SHA, which carries `a`'s result.
    assert_eq!(dependency("c"),[counts("integrated_commit")]);
    assert_eq!(f.git(&["rev-parse",&format!("{a_commit}^2")]),f.candidate);assert_eq!(f.git(&["show",&format!("{a_commit}:src/a.txt")]),"a");
    // The fan-in dependent is released only with both, on a tip containing both.
    assert_eq!(dependency("d"),[counts("integrated_commit"),counts("integrated_commit")]);
    for commit in [&a_commit,&e_commit,&f.candidate,&e_candidate] {f.git(&["merge-base","--is-ancestor",commit,&tip]);}
    assert!(tip==a_commit||tip==e_commit);
    assert_eq!(dependency("b"),[counts("verified_result")]);
    let valid=|t:&str,p:&str,r:&str,evidence:&str|(t.to_owned(),p.to_owned(),r.to_owned(),"valid".to_owned(),evidence.to_owned());
    assert_eq!(satisfactions(),[valid("b","a","verified_result",&a_result),valid("c","a","integrated_commit",&a_integrated),
        valid("d","a","integrated_commit",&a_integrated),valid("d","e","integrated_commit",&e_integrated)],"exactly one satisfaction per edge");
    // Independent check: a fresh clone of the target ref holds both results and passes the policy.
    let clone=f.home.path().join("fresh");let clone=clone.to_str().unwrap();
    f.git(&["clone","-q","--branch","integration",f.repo.to_str().unwrap(),clone]);
    assert_eq!(f.git(&["-C",clone,"rev-parse","HEAD"]),tip);
    f.git(&["-C",clone,"diff","--quiet"]);
    assert_eq!(f.git(&["-C",clone,"ls-files"]).lines().collect::<Vec<_>>(),["src/a.txt","src/base.txt","src/e.txt","src/lib.rs"]);

    // Card 5: the operator drafts, signs and reserves a launch of the released
    // dependent `c` on `a`'s integrated SHA, and its worker brief is built.
    use herdr_projects::domain::RuntimeRoute;
    let profile=f.launchable_profile();
    let head=||runtime::snapshot(&f.project).unwrap().head;
    // `g` waits on an integration of `b` that never happens.
    let request=f.home.path().join("g-queue.json");
    std::fs::write(&request,r#"{"priority":0,"dependencies":[{"predecessor":"b","requirement":"integrated_commit"}]}"#).unwrap();
    runtime::add_task(&f.project,TaskId::new("g").unwrap(),"consumer g".into(),head()).unwrap();
    let queued=hp(f.home.path(),&["--root",f.r(),"task","demo","queue","g","--input-file",request.to_str().unwrap(),"--expected-revision","1","--expected-head",&head().to_string()]);
    assert!(queued.status.success(),"{}",String::from_utf8_lossy(&queued.stderr));
    let policy=runtime::snapshot(&f.project).unwrap().scheduler.unwrap().policy.revision.to_string();
    let out=hp(f.home.path(),&["--root",f.r(),"scheduler","demo","policy","--max-active-workers","4","--max-attempts-per-task","3","--expected-revision",&policy,"--expected-head",&head().to_string()]);
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let socket=f.home.path().join("native.sock");let _listener=std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let repository=f.repo.canonicalize().unwrap();
    // Unused local routes in the repository for `c` and `g`; recording their observation re-admits the project.
    let config=f.home.path().join(".config/herdr-projects/config.toml");
    let mut bindings=std::collections::BTreeMap::new();
    for task in ["c","g"] {
        let revision=runtime::snapshot(&f.project).unwrap().tasks.into_iter().find(|t|t.id.as_str()==task).unwrap().revision;
        let change=runtime::create_binding(&f.project,Some(&TaskId::new(task).unwrap()),Some(revision),head(),
            &RuntimeRoute{socket:socket.display().to_string(),cwd:repository.display().to_string(),..Default::default()}).unwrap();
        bindings.insert(task,(change.binding,change.task_revision));
    }
    let observations=bindings.values().map(|(binding,task_revision)|herdr_projects::reconcile::RuntimeObservation{binding:binding.id.clone(),binding_revision:binding.revision,
        task_revision:*task_revision,observed_unix_ms:jiff::Timestamp::now().as_millisecond(),collector:"herdr-git-v2".into(),
        config_digest:migration::config_reference(&config).unwrap().digest,..Default::default()}).collect::<Vec<_>>();
    migration::open_active(&f.project).unwrap().record_observations(head(),&observations).unwrap();
    // The fixture's submitting workers (raw `running` rows) have exited.
    f.db().execute("UPDATE attempts SET state='completed',termination_observed=1 WHERE id IN ('a-attempt','e-attempt')",[]).unwrap();
    let control=runtime::snapshot(&f.project).unwrap().control.unwrap().revision;
    runtime::set_state(&f.project,head(),control,herdr_projects::domain::ProjectState::Active,&config).unwrap();
    let selection=|task:&str,knowledge:serde_json::Value|{
        let path=f.home.path().join(format!("{task}-selection.json"));
        std::fs::write(&path,serde_json::json!({"task":task,"binding":bindings[task].0.id,"profile":profile,"knowledge":knowledge,"repositories":[repository]}).to_string()).unwrap();
        path
    };
    let draft=|path:&Path|hp(f.home.path(),&["--root",f.r(),"launch","demo","draft","--selection",path.to_str().unwrap(),"--expected-head",&head().to_string()]);
    let g=selection("g",serde_json::json!({"id":"unused","revision":1,"digest":"0".repeat(64)}));
    let before=head();let refused=draft(&g);
    assert!(!refused.status.success());assert!(String::from_utf8_lossy(&refused.stderr).contains("not released"),"{}",String::from_utf8_lossy(&refused.stderr));
    assert_eq!(head(),before,"a refused draft writes nothing");
    let scope=f.home.path().join("c-scope.json");
    std::fs::write(&scope,r#"{"schema_version":1,"task_id":"c","profile":"worker","domains":[],"paths":[],"pinned_keys":[],"sensitivity":"default"}"#).unwrap();
    std::fs::write(f.project.join("PROJECT.md"),"Retained instructions for dependent c").unwrap();
    let c_selection=selection("c",serde_json::Value::Null);
    let out=hp(f.home.path(),&["--root",f.r(),"memory","demo","snapshot","--task","c","--profile","worker","--input-file",scope.to_str().unwrap(),"--worker"]);
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let snapshot:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();
    let mut chosen:serde_json::Value=serde_json::from_slice(&std::fs::read(&c_selection).unwrap()).unwrap();
    chosen["knowledge"]=serde_json::json!({"id":snapshot["id"],"revision":1,"digest":snapshot["manifest_hash"]});
    std::fs::write(&c_selection,chosen.to_string()).unwrap();
    // A single integrated-commit dependency still needs a base containing it.
    f.git(&["checkout","-q","--detach",&f.base]);
    let before=head();let refused=draft(&c_selection);
    assert!(!refused.status.success());assert!(String::from_utf8_lossy(&refused.stderr).contains("integration_missing"),"{}",String::from_utf8_lossy(&refused.stderr));
    assert_eq!(head(),before);
    f.git(&["checkout","-q","--detach",&a_commit]);
    let drafted=draft(&c_selection);assert!(drafted.status.success(),"{}",String::from_utf8_lossy(&drafted.stderr));
    let drafted:serde_json::Value=serde_json::from_slice(&drafted.stdout).unwrap();
    assert_eq!(drafted["inputs"]["repositories"][0]["commit"],a_commit.as_str(),"the worktree base is a's integrated SHA");
    let satisfaction=f.db().query_row("SELECT satisfaction_id FROM dependency_satisfactions WHERE task_id='c' AND state='valid' AND evidence_id=?1",[&a_integrated],|row|row.get::<_,String>(0)).unwrap();
    assert_eq!(drafted["inputs"]["dependencies"],serde_json::json!([{"task":"a","task_revision":1,"requirement":"integrated_commit",
        "evidence":{"id":satisfaction,"revision":1,"digest":satisfaction}}]),"the grant covers the exact dependency binding");
    let brief=drafted["brief"]["text"].as_str().unwrap();assert!(brief.contains("Retained instructions for dependent c"),"{brief}");
    let document=f.home.path().join("c-approval.json");std::fs::write(&document,serde_json::to_vec_pretty(&drafted["approval"]).unwrap()).unwrap();
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y","sign","-f"]).arg(&f.key).args(["-n",herdr_projects::authority::SIGNATURE_NAMESPACE]).arg(&document).output().unwrap().status.success());
    let out=hp(f.home.path(),&["--root",f.r(),"approval","demo","import",document.to_str().unwrap(),document.with_extension("json.sig").to_str().unwrap(),"--expected-head",&head().to_string()]);
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let approval:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(approval,drafted["inputs"]["approval"]);
    let out=hp(f.home.path(),&["--root",f.r(),"launch","demo","reserve","--selection",c_selection.to_str().unwrap(),"--approval-digest",approval["digest"].as_str().unwrap(),"--expected-head",&head().to_string()]);
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let reservation:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(reservation["record"]["inputs"],drafted["inputs"]);
    let attempt=reservation["record"]["attempt"].as_str().unwrap();assert_eq!(attempt,drafted["brief"]["attempt_id"]);
    assert_eq!(herdr_projects::memory::render_attempt_brief(&f.project,attempt).unwrap().text,brief,"the reserved attempt carries the worker prompt");
    // The prepared worktree is checked out at a's integrated SHA.
    let operation=herdr_projects::domain::OperationId::new(reservation["record"]["operation"].as_str().unwrap()).unwrap();
    let receipts=herdr_projects::worktree_preparation::prepare(&f.project,&operation,1,std::time::Instant::now()+std::time::Duration::from_secs(45),Default::default()).unwrap();
    assert_eq!(receipts.len(),1);
    assert_eq!(f.git(&["-C",&receipts[0].plan.path,"rev-parse","HEAD"]),a_commit);
}

/// Local stand-in for a Herdr server that runs the launched command for real,
/// outside the caller's short-lived bridge namespace. The supervisor reads its
/// release line from a FIFO standing in for the pane.
#[cfg(all(feature="state-store",target_os="linux"))]
const RUNNING_HERDR_SERVER: &str = r#"
import json,os,sys,socket,subprocess
path=sys.argv[1];root=os.path.dirname(path);s={}
server=socket.socket(socket.AF_UNIX);server.bind(path);server.listen()
while True:
 c,_=server.accept();f=c.makefile('rw');r=json.loads(f.readline());m=r['method'];p=r.get('params') or {}
 pane={'pane_id':'w1:p1','workspace_id':'w1','tab_id':'w1:t1','terminal_id':'term1','cwd':s.get('cwd')}
 agent=dict(pane,agent='claude',interactive_ready=True,agent_status='idle',**({'name':s['name']} if 'name' in s else {}))
 res=None
 if m=='ping':res={'type':'pong','version':'0.9.1','capabilities':{'workspace_create_command':True}}
 elif m=='workspace.create_command' and 'pid' not in s:
  fifo=os.path.join(root,'input');os.mkfifo(fifo);fd=os.open(fifo,os.O_RDWR)
  child=subprocess.Popen(p['command'],cwd=p['cwd'],stdin=fd,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True,env={'PATH':'/usr/bin:/bin'})
  s.update(pid=child.pid,argv=p['command'],cwd=p['cwd'],label=p['label'],fifo=fifo)
  res={'type':'workspace_created','workspace':{'workspace_id':'w1'},'root_pane':{'pane_id':'w1:p1'}}
 elif m=='workspace.list':res={'type':'workspace_list','workspaces':[{'workspace_id':'w1','label':s['label'],'pane_count':1,'tab_count':1}] if 'pid' in s else []}
 elif m=='pane.list':res={'panes':[pane] if 'pid' in s else []}
 elif m=='pane.get':res={'pane':pane}
 elif m=='pane.process_info':res={'process_info':{'pane_id':'w1:p1','foreground_processes':[{'pid':s['pid'],'argv':s['argv']}] if 'pid' in s else []}}
 elif m=='pane.send_input':
  fd=os.open(s['fifo'],os.O_WRONLY);os.write(fd,p['text'].encode());os.close(fd);s['released']=True;res={'type':'ok'}
 elif m=='agent.list':res={'type':'agent_list','agents':[agent] if s.get('released') else []}
 elif m=='agent.rename':s['name']=agent['name']=p['name'];res={'type':'agent_info','agent':agent}
 elif m=='agent.explain':res={'type':'agent_explain','explain':{'agent':'claude','state':'idle','manifest_source':'bundled','manifest_version':'2026.09.14.1',
  'matched_rule':{'id':'prompt','state':'idle'},'visible_idle':True,'visible_blocker':False,'visible_working':False,'screen_detection_skipped':False,
  'skip_state_update':False,'local_override_shadowing_remote':False,'fallback_reason':None,'warning':None}}
 elif m=='agent.prompt':res={'type':'agent_prompted','agent':agent}
 if res is not None:f.write(json.dumps({'id':r['id'],'result':res})+'\n');f.flush()
 c.close()
"#;

/// F1 follow-up: a worker whose result is verified and integrated ends by
/// completion. Its termination is proven before capacity is released, its
/// task succeeds instead of being cancelled, and its dependent stays released.
#[cfg(all(feature="state-store",target_os="linux"))]
#[test]
fn completing_a_verified_worker_stops_it_and_keeps_its_dependent_released() {
    use std::{fs,os::unix::fs::PermissionsExt,time::{Duration,Instant}};
    use herdr_projects::{migration,runtime,domain::{TaskId,RuntimeRoute,AttemptState,TaskState},operations::DeliveryState};
    let f=VerifyFixture::with_worker(&[("src/start.txt","start\n".into())],
        "kind='claude'\npermission_policy='interactive'\n[profiles.worker.budget]\nmax_wall_seconds=600\nunknown_usage='allow_with_warning'\n");
    let start=f.candidate.clone();let repository=f.repo.canonicalize().unwrap();
    let head=||runtime::snapshot(&f.project).unwrap().head;
    let run=|args:&[&str]|{let mut all=vec!["--root",f.r()];all.extend_from_slice(args);let deadline=Instant::now()+Duration::from_secs(60);
        loop {let out=hp(f.home.path(),&all);if out.status.success()||!String::from_utf8_lossy(&out.stderr).contains("owns lock")||Instant::now()>deadline{return out;}std::thread::sleep(Duration::from_millis(200));}};
    let cli=|args:&[&str]|{let out=run(args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap_or(serde_json::Value::Null)};
    let sign=|path:&Path,namespace:&str|assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y","sign","-f"]).arg(&f.key).args(["-n",namespace]).arg(path).output().unwrap().status.success());
    // A Herdr stand-in that really runs the supervised worker, and an agent that stays up.
    let bin=f.home.path().join("bin");let agent_home=f.home.path().join("agent-home");
    for dir in [&bin,&agent_home,&f.home.path().join("lab-a"),&f.home.path().join("lab-b")] {fs::create_dir_all(dir).unwrap();}
    let (herdr,agent)=(bin.join("herdr"),bin.join("claude"));
    fs::write(&herdr,"#!/usr/bin/python3\nimport os,socket,sys\nif sys.argv[1:]==['--version']:print('herdr 0.9.1');sys.exit(0)\nimport json\nprobe={('pane','list'):'pane.list',('agent','list'):'agent.list'}.get(tuple(sys.argv[1:]))\n\
assert probe or sys.argv[1:]==['remote-api-bridge']\nline=json.dumps({'id':'probe','method':probe}).encode()+b'\\n' if probe else sys.stdin.buffer.readline()\n\
c=socket.socket(socket.AF_UNIX);c.connect(os.environ['HERDR_SOCKET_PATH']);c.sendall(line)\nreply=c.makefile('rb').readline()\nif not reply:sys.exit(1)\n\
sys.stdout.buffer.write(json.dumps({'result':json.loads(reply)['result']}).encode() if probe else reply)\n").unwrap();
    // The agent must be the exact executable it names, so build a tiny one.
    let source=f.home.path().join("agent.rs");
    fs::write(&source,"fn main(){if std::env::args().nth(1).as_deref()==Some(\"--version\"){println!(\"2.1.0 (Claude Code)\");return}loop{std::thread::park()}}").unwrap();
    assert!(Command::new("rustc").args(["--edition","2021","-o"]).arg(&agent).arg(&source).status().unwrap().success());
    for path in [&herdr,&agent] {fs::set_permissions(path,fs::Permissions::from_mode(0o700)).unwrap();}
    let sockets=[f.home.path().join("lab-a/native.sock"),f.home.path().join("lab-b/native.sock")];
    struct Lab(Vec<std::process::Child>,std::path::PathBuf);
    impl Drop for Lab {fn drop(&mut self){
        let _=Command::new("/usr/bin/pkill").args(["-KILL","-f"]).arg(&self.1).status();
        for server in &mut self.0 {let _=server.kill();let _=server.wait();}
    }}
    let _lab=Lab(sockets.iter().map(|socket|Command::new("/usr/bin/python3").args(["-c",RUNNING_HERDR_SERVER]).arg(socket).spawn().unwrap()).collect(),agent_home.clone());
    for socket in &sockets {let deadline=Instant::now()+Duration::from_secs(10);while !socket.exists(){assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(10));}}

    // `a` has a signed verify-then-integrate contract; `b` needs a's integrated commit.
    let added=runtime::add_task(&f.project,TaskId::new("a").unwrap(),"task a".into(),head()).unwrap();
    let mut contract=serde_json::to_vec_pretty(&serde_json::json!({
        "version":3,"outputs":[{"path":"src/a.txt","kind":"git_file"}],"scope":{"paths":[{"path":"src/","access":"write"}]},
        "project_store":f.store,"expected_head":added,"task_id":"a","contract_revision":1,"deliverable":"src/a.txt","non_goals":"no other change",
        "acceptance_policies":[{"id":"clean","text":r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#}],
        "repository":repository,"base_oid":start,"object_format":"sha256","dependencies":[],"capability_flags":[],
        "profile_kind":"claude","retry_class":"none","result_schema_id":"result-v1","route":"verify_then_integrate","authority":herdr_projects::authority::policy_reference(&f.project).unwrap()
    })).unwrap();contract.push(b'\n');
    let contract_path=f.home.path().join("a-contract.json");fs::write(&contract_path,&contract).unwrap();sign(&contract_path,herdr_projects::authority::CONTRACT_SIGNATURE_NAMESPACE);
    let installed=cli(&["task","demo","contract","put","--input-file",contract_path.to_str().unwrap(),"--signature",contract_path.with_extension("json.sig").to_str().unwrap()]);
    runtime::add_task(&f.project,TaskId::new("b").unwrap(),"task b".into(),head()).unwrap();
    for (task,edges) in [("a",serde_json::json!([])),("b",serde_json::json!([{"predecessor":"a","requirement":"integrated_commit"}]))] {
        let request=f.home.path().join(format!("{task}-queue.json"));fs::write(&request,serde_json::json!({"priority":0,"dependencies":edges}).to_string()).unwrap();
        cli(&["task","demo","queue",task,"--input-file",request.to_str().unwrap(),"--expected-revision","1","--expected-head",&head().to_string()]);
    }
    let policy=runtime::snapshot(&f.project).unwrap().scheduler.unwrap().policy.revision.to_string();
    cli(&["scheduler","demo","policy","--max-active-workers","4","--max-attempts-per-task","3","--expected-revision",&policy,"--expected-head",&head().to_string()]);
    let config=f.home.path().join(".config/herdr-projects/config.toml");
    let mut bindings=std::collections::BTreeMap::new();
    for (task,socket) in ["a","b"].into_iter().zip(&sockets) {
        let revision=runtime::snapshot(&f.project).unwrap().tasks.into_iter().find(|t|t.id.as_str()==task).unwrap().revision;
        let change=runtime::create_binding(&f.project,Some(&TaskId::new(task).unwrap()),Some(revision),head(),
            &RuntimeRoute{socket:socket.display().to_string(),cwd:repository.display().to_string(),..Default::default()}).unwrap();
        bindings.insert(task,(change.binding,change.task_revision));
    }
    let observations=bindings.values().map(|(binding,task_revision)|herdr_projects::reconcile::RuntimeObservation{binding:binding.id.clone(),binding_revision:binding.revision,
        task_revision:*task_revision,observed_unix_ms:jiff::Timestamp::now().as_millisecond(),collector:"herdr-git-v2".into(),
        config_digest:migration::config_reference(&config).unwrap().digest,..Default::default()}).collect::<Vec<_>>();
    migration::open_active(&f.project).unwrap().record_observations(head(),&observations).unwrap();
    let control=runtime::snapshot(&f.project).unwrap().control.unwrap().revision;
    runtime::set_state(&f.project,head(),control,herdr_projects::domain::ProjectState::Active,&config).unwrap();
    let profile=f.launchable_profile_with(&herdr,&agent,&agent_home);
    // Draft (and for `a`, sign and reserve) a launch from retained instructions.
    let draft=|task:&str|->std::path::PathBuf {
        fs::write(f.project.join("PROJECT.md"),format!("Instructions for {task}")).unwrap();
        let scope=f.home.path().join(format!("{task}-scope.json"));
        fs::write(&scope,serde_json::json!({"schema_version":1,"task_id":task,"profile":"worker","domains":[],"paths":[],"pinned_keys":[],"sensitivity":"default"}).to_string()).unwrap();
        let snapshot=cli(&["memory","demo","snapshot","--task",task,"--profile","worker","--input-file",scope.to_str().unwrap(),"--worker"]);
        let selection=f.home.path().join(format!("{task}-selection.json"));
        fs::write(&selection,serde_json::json!({"task":task,"binding":bindings[task].0.id,"profile":profile,
            "knowledge":{"id":snapshot["id"],"revision":1,"digest":snapshot["manifest_hash"]},"repositories":[repository]}).to_string()).unwrap();
        selection
    };
    let selection=draft("a");
    let drafted=cli(&["launch","demo","draft","--selection",selection.to_str().unwrap(),"--expected-head",&head().to_string()]);
    let document=f.home.path().join("a-approval.json");fs::write(&document,serde_json::to_vec_pretty(&drafted["approval"]).unwrap()).unwrap();
    sign(&document,herdr_projects::authority::SIGNATURE_NAMESPACE);
    let approval=cli(&["approval","demo","import",document.to_str().unwrap(),document.with_extension("json.sig").to_str().unwrap(),"--expected-head",&head().to_string()]);
    let reservation=cli(&["launch","demo","reserve","--selection",selection.to_str().unwrap(),"--approval-digest",approval["digest"].as_str().unwrap(),"--expected-head",&head().to_string()]);
    let attempt=reservation["record"]["attempt"].as_str().unwrap().to_owned();
    let herdr_path=herdr.to_str().unwrap().to_owned();
    // A running ticker may be mid-publication; read again.
    let snapshot=||{let deadline=Instant::now()+Duration::from_secs(10);loop{match runtime::snapshot(&f.project){Ok(s)=>return s,Err(e)=>{assert!(Instant::now()<deadline,"{e:#}");std::thread::sleep(Duration::from_millis(20));}}}};
    let a_attempt=||snapshot().attempts.into_iter().find(|x|x.id.as_str()==attempt).unwrap();
    let task=|id:&str|snapshot().tasks.into_iter().find(|t|t.id.as_str()==id).unwrap();
    let events=|kind:&str|snapshot().events.into_iter().filter(|e|e.kind==kind).collect::<Vec<_>>();
    // The ticker launches `a`'s worker and delivers its brief; the worker keeps running.
    let briefed=||{let state=snapshot();state.operations.iter().filter(|o|o.kind=="runtime.worker_brief")
        .any(|o|state.deliveries.iter().any(|d|d.operation==o.id&&d.state==DeliveryState::Confirmed))};
    let mut child=f.spawn_with(&herdr_path);
    f.wait(&mut child,120,&briefed);
    f.stop(&mut child);
    let started:herdr_projects::domain::LaunchStartedReceipt=serde_json::from_value(events("runtime.launch_started")[0].payload.clone()).unwrap();
    let supervisor=started.supervisor.clone().unwrap();
    assert!(!herdr_projects::worker_supervision::SupervisorObservation::recover_exited(&supervisor).unwrap(),"the worker is running");

    // The harness plays the worker's result; its claims are untrusted.
    f.git(&["checkout","-q","-b","a-result",&start]);fs::write(f.repo.join("src/a.txt"),"a\n").unwrap();f.git(&["add","src"]);f.git(&["commit","-qm","a"]);
    let a_candidate=f.git(&["rev-parse","HEAD"]);f.git(&["checkout","-q","--detach",&start]);
    let objects=f.git(&["rev-list","--objects","--all"]).lines().map(|line|{let oid=line.split_whitespace().next().unwrap();serde_json::json!({"oid":oid,"relative_path":format!("{}/{}",&oid[..2],&oid[2..])})}).collect::<Vec<_>>();
    let submission=f.home.path().join("a-result.json");
    fs::write(&submission,serde_json::to_vec(&serde_json::json!({"idempotency_key":"a-result","task_id":"a","contract_revision":1,"contract_digest":installed["digest"],"attempt_id":attempt,
        "repository":repository,"base_oid":start,"candidate_oid":a_candidate,"object_format":"sha256",
        "artifact_manifest":[{"path":"src/a.txt","oid":a_candidate}],"claimed_checks":["worker says done"],"objects":objects})).unwrap()).unwrap();
    let a=cli(&["result","demo","submit","--input-file",submission.to_str().unwrap()])["submission_id"].as_str().unwrap().to_owned();
    // A submission alone is not accepted evidence: completion is refused and writes nothing.
    let complete=|revision:u64|run(&["task","demo","complete","a","--expected-revision",&revision.to_string()]);
    let before=head();let refused=complete(task("a").revision);
    assert!(!refused.status.success());assert!(String::from_utf8_lossy(&refused.stderr).contains("accepted verification"),"{}",String::from_utf8_lossy(&refused.stderr));
    assert_eq!(head(),before);

    // Automatic verification and integration accept and integrate a's result.
    const TARGET:&str="refs/heads/integration";
    f.git(&["branch","integration",&start]);
    cli(&["result","demo","configure-integration","--repository",repository.to_str().unwrap(),"--reference",TARGET]);
    for switch in ["--verify","--integrate"] {cli(&["result","demo","auto",switch,"on","--expected-head",&head().to_string()]);}
    let integrated=||snapshot().operations.into_iter().find(|op|op.kind=="integration.run"&&op.payload["submission_id"]==a.as_str())
        .is_some_and(|op|snapshot().deliveries.iter().any(|d|d.operation==op.id&&d.state==DeliveryState::Confirmed));
    let mut child=f.spawn_with(&herdr_path);
    f.wait(&mut child,150,&integrated);
    f.stop(&mut child);
    let a_commit=f.git(&["rev-parse",TARGET]);
    let blockers=|id:&str|{
        let report=cli(&["scheduler","demo","inspect"]);
        let entry=report["entries"].as_array().unwrap().iter().find(|entry|entry["task"]==id).unwrap().clone();
        let mut found=entry["blockers"].as_array().unwrap().iter().map(|s|s.as_str().unwrap().to_owned())
            .filter(|s|["verified_dependency_evidence_unavailable:","predecessor_failed:","admission_disabled:"].iter().any(|p|s.starts_with(p))).collect::<Vec<_>>();
        found.sort();(found,report["retained_attempts"].as_u64().unwrap())
    };
    assert_eq!(blockers("b"),(vec!["admission_disabled:integrated_commit".to_owned()],1),"b is released; a's worker still holds capacity");
    assert!(a_attempt().retains_capacity());

    // Completion is recorded once; a repeat is an idempotent no-op.
    let revision=task("a").revision;
    let requested=complete(revision);assert!(requested.status.success(),"{}",String::from_utf8_lossy(&requested.stderr));
    let requested:serde_json::Value=serde_json::from_slice(&requested.stdout).unwrap();
    assert_eq!((requested["attempt"].as_str(),requested["replayed"].as_bool()),(Some(attempt.as_str()),Some(false)));
    assert!(a_attempt().retains_capacity(),"capacity is held until termination is proven");
    let before=head();let replay=complete(revision);assert!(replay.status.success(),"{}",String::from_utf8_lossy(&replay.stderr));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&replay.stdout).unwrap()["replayed"],true);assert_eq!(head(),before);
    // The controller stops the worker through the existing termination path.
    let mut child=f.spawn_with(&herdr_path);
    f.wait(&mut child,120,&||a_attempt().termination_observed);
    f.stop(&mut child);
    assert!(herdr_projects::worker_supervision::SupervisorObservation::recover_exited(&supervisor).unwrap(),"the worker has exited");
    let stop:herdr_projects::domain::WorkerTerminationReceipt=serde_json::from_value(events("runtime.worker_terminated")[0].payload.clone()).unwrap();
    assert_eq!(stop.cause,herdr_projects::domain::WorkerTerminationCause::Completion);
    let (finished,a_task)=(a_attempt(),task("a"));
    assert_eq!((finished.state,finished.retains_capacity()),(AttemptState::Completed,false));
    assert_eq!((a_task.state,a_task.active_attempt),(TaskState::Succeeded,None));
    assert!(snapshot().cancellations.is_empty(),"completion is not a cancellation");
    // The dependent is still released and the capacity is free.
    assert_eq!(blockers("b"),(vec!["admission_disabled:integrated_commit".to_owned()],0));
    let before=head();let replay=complete(a_task.revision);assert!(replay.status.success(),"{}",String::from_utf8_lossy(&replay.stderr));assert_eq!(head(),before);
    // b is launchable on a's integrated SHA.
    f.git(&["checkout","-q","--detach",&a_commit]);
    let selection=draft("b");
    let drafted=cli(&["launch","demo","draft","--selection",selection.to_str().unwrap(),"--expected-head",&head().to_string()]);
    assert_eq!(drafted["inputs"]["repositories"][0]["commit"],a_commit.as_str());
    assert_eq!(drafted["inputs"]["dependencies"][0]["task"],"a");
}

/// F1.7 acceptance. `HP_LIVE_F1_MODE=fixture` is the dry run: the harness plays
/// both workers. `live` (only via `scripts/test-live-f1`) launches two real
/// agents through the ticker on an isolated stock Herdr server; worker output is
/// untrusted and only verification and integration evidence release anything.
#[cfg(all(feature="state-store",target_os="linux"))]
#[test]
#[ignore = "F1.7: HP_LIVE_F1_MODE=fixture, or live with HP_LIVE_HERDR, HP_LIVE_AGENT and HP_LIVE_AUTH_FILE via scripts/test-live-f1"]
fn live_f1_worker_result_integrates_and_dependent_launches_on_integrated_sha() {
    use std::{fs,io::{Read,Write},os::unix::fs::PermissionsExt,sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}},time::{Duration,Instant}};
    use std::path::PathBuf;
    use herdr_projects::{migration,runtime,domain::{TaskId,RuntimeRoute},operations::{DeliveryState,Outcome}};
    let live=match std::env::var("HP_LIVE_F1_MODE").as_deref(){Ok("live")=>true,Ok("fixture")=>false,_=>panic!("set HP_LIVE_F1_MODE=fixture or live")};
    let clock=Instant::now();
    let say=|text:&str|eprintln!("[f1.7 +{:>4}s {}] {text}",clock.elapsed().as_secs(),&jiff::Timestamp::now().to_string()[11..19]);
    say(if live {"mode: live (two real agent sessions)"} else {"mode: fixture dry run (harness plays both workers)"});
    // Only `a`'s `await` policy speaks git's native protocol to this fixture: its
    // first connection is verification, the second the integration candidate
    // check, which is held so the ticker can be killed mid-integration. The
    // integration job rechecks the first accepted policy by id, hence `await`.
    let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();let port=listener.local_addr().unwrap().port();
    let held=Arc::new(AtomicBool::new(true));let connections=Arc::new(AtomicUsize::new(0));
    {let held=held.clone();let connections=connections.clone();std::thread::spawn(move||for stream in listener.incoming(){
        let Ok(mut stream)=stream else{continue};let index=connections.fetch_add(1,Ordering::SeqCst);let held=held.clone();
        std::thread::spawn(move||{let mut buffer=[0u8;4096];let _=stream.read(&mut buffer);while index==1&&held.load(Ordering::SeqCst){std::thread::sleep(Duration::from_millis(10));}let _=stream.write_all(b"0000");let _=stream.read(&mut buffer);});
    });}
    let f=VerifyFixture::with_worker(&[("src/start.txt","disposable F1.7 repository\n".into())],
        "kind='codex'\npermission_policy='interactive'\n[profiles.worker.budget]\nmax_wall_seconds=1200\nunknown_usage='allow_with_warning'\n");
    let start=f.candidate.clone();let branch=f.git(&["symbolic-ref","--short","HEAD"]);
    let repository=f.repo.canonicalize().unwrap();
    let head=||runtime::snapshot(&f.project).unwrap().head;
    let ok=|out:std::process::Output|{assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap_or(serde_json::Value::Null)};
    // A running ticker can hold the effect lock briefly; the CLI asks for a retry.
    let cli=|args:&[&str]|{let mut all=vec!["--root",f.r()];all.extend_from_slice(args);let deadline=Instant::now()+Duration::from_secs(60);
        loop {let out=hp(f.home.path(),&all);if out.status.success()||!String::from_utf8_lossy(&out.stderr).contains("owns lock")||Instant::now()>deadline{return ok(out);}std::thread::sleep(Duration::from_millis(200));}};
    let sign=|path:&Path,namespace:&str|assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y","sign","-f"]).arg(&f.key).args(["-n",namespace]).arg(path).output().unwrap().status.success());

    // Native lab: one isolated stock Herdr server per worker (live), or bare
    // sockets (fixture). Two live workers on one server break start naming:
    // its inventory fence compares every agent, and the other worker's entry
    // changes (see the F1.7 gate note).
    let lab=tempfile::Builder::new().prefix("hpf17-").tempdir_in("/tmp").unwrap();
    let sockets=[lab.path().join("a/native.sock"),lab.path().join("b/native.sock")];
    let agent_home=f.home.path().join("agent-home");fs::create_dir_all(&agent_home).unwrap();
    struct Cleanup{servers:Vec<std::process::Child>,agent_home:PathBuf}
    impl Drop for Cleanup {fn drop(&mut self){
        // Any supervised worker carries its unique execution home in its argv.
        let _=Command::new("/usr/bin/pkill").args(["-KILL","-f"]).arg(&self.agent_home).status();
        for server in &mut self.servers{let _=server.kill();let _=server.wait();}
    }}
    let mut cleanup=Cleanup{servers:Vec::new(),agent_home:agent_home.clone()};
    let mut _fixture_sockets=Vec::new();
    let (herdr,agent)=if live {
        let herdr=PathBuf::from(std::env::var_os("HP_LIVE_HERDR").expect("HP_LIVE_HERDR"));
        let agent=PathBuf::from(std::env::var_os("HP_LIVE_AGENT").expect("HP_LIVE_AGENT"));
        let auth=PathBuf::from(std::env::var_os("HP_LIVE_AUTH_FILE").expect("HP_LIVE_AUTH_FILE"));
        assert!(herdr.is_absolute()&&agent.is_absolute()&&auth.is_absolute());
        for socket in &sockets {
            let dir=socket.parent().unwrap();
            let (home,runtime_dir,config)=(dir.join("home"),dir.join("runtime"),dir.join("config.toml"));
            fs::create_dir_all(&home).unwrap();fs::create_dir(&runtime_dir).unwrap();fs::set_permissions(&runtime_dir,fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(&config,"onboarding = false\n[terminal]\ndefault_shell = '/bin/sh'\nshell_mode = 'non_login'\n[update]\nversion_check = false\nmanifest_check = false\n").unwrap();
            let log=fs::File::create(dir.join("server.log")).unwrap();
            cleanup.servers.push(Command::new(&herdr).arg("server").env_clear().env("HOME",&home).env("PATH","/usr/bin:/bin").env("SHELL","/bin/sh")
                .env("TERM","xterm-256color").env("LANG","C.UTF-8").env("HERDR_CONFIG_PATH",&config).env("HERDR_SOCKET_PATH",socket)
                .env("XDG_RUNTIME_DIR",&runtime_dir).current_dir(dir).stdin(std::process::Stdio::null()).stdout(log.try_clone().unwrap()).stderr(log).spawn().unwrap());
            let deadline=Instant::now()+Duration::from_secs(15);
            while !socket.exists(){assert!(Instant::now()<deadline,"native server failed to start");std::thread::sleep(Duration::from_millis(25));}
        }
        // Credentials: a private copy in the disposable execution home, never read here.
        let codex=agent_home.join(".codex");fs::create_dir(&codex).unwrap();fs::set_permissions(&codex,fs::Permissions::from_mode(0o700)).unwrap();
        let metadata=fs::symlink_metadata(&auth).unwrap();assert!(metadata.is_file()&&metadata.len()<=256*1024&&metadata.permissions().mode()&0o077==0);
        fs::copy(&auth,codex.join("auth.json")).unwrap();fs::set_permissions(codex.join("auth.json"),fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(agent_home.join(".gitconfig"),"[user]\n\tname = F1.7 worker\n\temail = worker@example.invalid\n").unwrap();
        (herdr,agent)
    } else {
        for socket in &sockets {fs::create_dir_all(socket.parent().unwrap()).unwrap();_fixture_sockets.push(std::os::unix::net::UnixListener::bind(socket).unwrap());}
        (PathBuf::new(),PathBuf::new())
    };
    // Codex's workspace-write sandbox makes Git metadata (including a linked
    // worktree's gitdir) read-only, so a sandboxed worker cannot commit. The
    // workers run unsandboxed in a disposable home with exact trust entries.
    let trust=|worktrees:&[&str]|{
        let mut text="approval_policy = 'never'\nsandbox_mode = 'danger-full-access'\nmodel_reasoning_effort = 'low'\ncheck_for_update_on_startup = false\n".to_owned();
        for path in [f.project.to_str().unwrap(),repository.to_str().unwrap()].iter().chain(worktrees) {text.push_str(&format!("[projects.{}]\ntrust_level = 'trusted'\n",serde_json::to_string(path).unwrap()));}
        if live {fs::write(agent_home.join(".codex/config.toml"),text).unwrap();}
    };
    trust(&[]);

    // Tasks: `a` (signed contract, verify_then_integrate) and `b` (integrated commit of `a`).
    let policies=[("content",r#"{"version":1,"checks":["/usr/bin/git","grep","--quiet","--no-index","-e","^F17_LIVE_A_OK$","--","src/a.txt"]}"#.to_owned()),
        ("clean",r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#.to_owned()),
        ("await",format!(r#"{{"version":1,"checks":["/usr/bin/git","ls-remote","git://127.0.0.1:{port}/fixture"]}}"#))];
    let added=runtime::add_task(&f.project,TaskId::new("a").unwrap(),"F1.7 live task a".into(),head()).unwrap();
    let mut contract=serde_json::to_vec_pretty(&serde_json::json!({
        "version":3,"outputs":[{"path":"src/a.txt","kind":"git_file"}],"scope":{"paths":[{"path":"src/","access":"write"}]},
        "project_store":f.store,"expected_head":added,"task_id":"a","contract_revision":1,"deliverable":"src/a.txt holds the marker","non_goals":"no other change",
        "acceptance_policies":policies.iter().map(|(id,text)|serde_json::json!({"id":id,"text":text})).collect::<Vec<_>>(),
        "repository":repository,"base_oid":start,"object_format":"sha256","dependencies":[],"capability_flags":[],
        "profile_kind":"codex","retry_class":"none","result_schema_id":"result-v1","route":"verify_then_integrate","authority":herdr_projects::authority::policy_reference(&f.project).unwrap()
    })).unwrap();contract.push(b'\n');
    let contract_path=f.home.path().join("a-contract.json");fs::write(&contract_path,&contract).unwrap();sign(&contract_path,herdr_projects::authority::CONTRACT_SIGNATURE_NAMESPACE);
    let installed=cli(&["task","demo","contract","put","--input-file",contract_path.to_str().unwrap(),"--signature",contract_path.with_extension("json.sig").to_str().unwrap()]);
    runtime::add_task(&f.project,TaskId::new("b").unwrap(),"F1.7 live dependent b".into(),head()).unwrap();
    for (task,edges) in [("a",serde_json::json!([])),("b",serde_json::json!([{"predecessor":"a","requirement":"integrated_commit"}]))] {
        let request=f.home.path().join(format!("{task}-queue.json"));fs::write(&request,serde_json::json!({"priority":0,"dependencies":edges}).to_string()).unwrap();
        cli(&["task","demo","queue",task,"--input-file",request.to_str().unwrap(),"--expected-revision","1","--expected-head",&head().to_string()]);
    }
    let policy=runtime::snapshot(&f.project).unwrap().scheduler.unwrap().policy.revision.to_string();
    cli(&["scheduler","demo","policy","--max-active-workers","4","--max-attempts-per-task","3","--expected-revision",&policy,"--expected-head",&head().to_string()]);
    let config=f.home.path().join(".config/herdr-projects/config.toml");
    let mut bindings=std::collections::BTreeMap::new();
    for (task,socket) in ["a","b"].into_iter().zip(&sockets) {
        let revision=runtime::snapshot(&f.project).unwrap().tasks.into_iter().find(|t|t.id.as_str()==task).unwrap().revision;
        let change=runtime::create_binding(&f.project,Some(&TaskId::new(task).unwrap()),Some(revision),head(),
            &RuntimeRoute{socket:socket.display().to_string(),cwd:repository.display().to_string(),..Default::default()}).unwrap();
        bindings.insert(task,(change.binding,change.task_revision));
    }
    let observations=bindings.values().map(|(binding,task_revision)|herdr_projects::reconcile::RuntimeObservation{binding:binding.id.clone(),binding_revision:binding.revision,
        task_revision:*task_revision,observed_unix_ms:jiff::Timestamp::now().as_millisecond(),collector:"herdr-git-v2".into(),
        config_digest:migration::config_reference(&config).unwrap().digest,..Default::default()}).collect::<Vec<_>>();
    migration::open_active(&f.project).unwrap().record_observations(head(),&observations).unwrap();
    let control=runtime::snapshot(&f.project).unwrap().control.unwrap().revision;
    runtime::set_state(&f.project,head(),control,herdr_projects::domain::ProjectState::Active,&config).unwrap();
    let profile=if live {f.launchable_profile_with(&herdr,&agent,&agent_home)} else {f.fake_launchable_profile("codex","codex-cli 0.154.0")};
    const TARGET:&str="refs/heads/integration";
    f.git(&["branch","integration",&start]);
    cli(&["result","demo","configure-integration","--repository",repository.to_str().unwrap(),"--reference",TARGET]);
    for switch in ["--verify","--integrate"] {cli(&["result","demo","auto",switch,"on","--expected-head",&head().to_string()]);}
    say(&format!("setup: repository start {start}, target {TARGET}, automation verify+integrate on"));

    // Operator launch: retained instructions, draft, owner signature, reservation.
    let launch=|task:&str,instructions:&str|->(String,PathBuf) {
        fs::write(f.project.join("PROJECT.md"),instructions).unwrap();
        let scope=f.home.path().join(format!("{task}-scope.json"));
        fs::write(&scope,serde_json::json!({"schema_version":1,"task_id":task,"profile":"worker","domains":[],"paths":[],"pinned_keys":[],"sensitivity":"default"}).to_string()).unwrap();
        let snapshot=cli(&["memory","demo","snapshot","--task",task,"--profile","worker","--input-file",scope.to_str().unwrap(),"--worker"]);
        let selection=f.home.path().join(format!("{task}-selection.json"));
        fs::write(&selection,serde_json::json!({"task":task,"binding":bindings[task].0.id,"profile":profile,
            "knowledge":{"id":snapshot["id"],"revision":1,"digest":snapshot["manifest_hash"]},"repositories":[repository]}).to_string()).unwrap();
        let drafted=cli(&["launch","demo","draft","--selection",selection.to_str().unwrap(),"--expected-head",&head().to_string()]);
        assert!(drafted["brief"]["text"].as_str().unwrap().contains(instructions),"the brief carries the retained instructions");
        let document=f.home.path().join(format!("{task}-approval.json"));fs::write(&document,serde_json::to_vec_pretty(&drafted["approval"]).unwrap()).unwrap();
        sign(&document,herdr_projects::authority::SIGNATURE_NAMESPACE);
        let approval=cli(&["approval","demo","import",document.to_str().unwrap(),document.with_extension("json.sig").to_str().unwrap(),"--expected-head",&head().to_string()]);
        let reservation=cli(&["launch","demo","reserve","--selection",selection.to_str().unwrap(),"--approval-digest",approval["digest"].as_str().unwrap(),"--expected-head",&head().to_string()]);
        assert_eq!(reservation["record"]["inputs"],drafted["inputs"]);
        let attempt=reservation["record"]["attempt"].as_str().unwrap().to_owned();
        let inputs:herdr_projects::domain::LaunchInputs=serde_json::from_value(drafted["inputs"].clone()).unwrap();
        let plan=herdr_projects::domain::worktree_plans(&inputs,&herdr_projects::domain::AttemptId::new(attempt.clone()).unwrap()).unwrap().remove(0);
        say(&format!("{task}: drafted, signed and reserved attempt {attempt}; worktree base {}",inputs.repositories[0].commit));
        (attempt,PathBuf::from(plan.path))
    };
    let wt_git=|path:&Path,args:&[&str]|{let mut all=vec!["-C",path.to_str().unwrap()];all.extend_from_slice(args);f.git(&all)};
    let events=|kind:&str|runtime::snapshot(&f.project).unwrap().events.into_iter().filter(|e|e.kind==kind).count();
    // Report each runtime milestone once, as the controller records it.
    let seen=std::cell::RefCell::new(std::collections::BTreeSet::new());
    let milestones=||for event in runtime::snapshot(&f.project).unwrap().events {
        if event.kind.starts_with("runtime.")&&seen.borrow_mut().insert((event.kind.clone(),event.entity.clone())) {say(&format!("controller: {} {}",event.kind,&event.entity[..event.entity.len().min(20)]));}
    };

    // 1. Worker A edits and commits in its worktree.
    let herdr_bin=herdr.to_str().unwrap().to_owned();
    let spawn=||if live {f.spawn_with(&herdr_bin)} else {f.spawn()};
    let (a_attempt,a_worktree,mut child)=if live {
        let (attempt,worktree)=launch("a","F1.7 disposable acceptance task A. Work only in the current directory, which is a disposable git worktree. Create the file src/a.txt containing exactly the line F17_LIVE_A_OK followed by a newline. Then run: git add src/a.txt && git commit -m 'Add F1.7 marker A'. Do not change any other file, do not push, and do not use the network. After the commit exists, stop and reply DONE.");
        trust(&[worktree.to_str().unwrap()]);
        let mut child=spawn();
        f.wait(&mut child,300,&||{milestones();worktree.join("src/a.txt").is_file()}&&wt_git(&worktree,&["rev-parse","HEAD"])!=start&&wt_git(&worktree,&["status","--porcelain"]).is_empty());
        say(&format!("a: worker launched ({} launch_started) and committed in its worktree",events("runtime.launch_started")));
        (attempt,worktree,child)
    } else {
        f.db().execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES('a-attempt','a',1,'running',NULL,'a-attempt',0)",[]).unwrap();
        let worktree=f.home.path().join("a-worktree");f.git(&["worktree","add","-q","--detach",worktree.to_str().unwrap(),&start]);
        fs::write(worktree.join("src/a.txt"),"F17_LIVE_A_OK\n").unwrap();wt_git(&worktree,&["add","src/a.txt"]);wt_git(&worktree,&["commit","-qm","Add F1.7 marker A"]);
        ("a-attempt".to_owned(),worktree,spawn())
    };
    let a_candidate=wt_git(&a_worktree,&["rev-parse","HEAD"]);
    assert_eq!(wt_git(&a_worktree,&["rev-parse","HEAD^"]),start);
    assert_eq!(fs::read_to_string(a_worktree.join("src/a.txt")).unwrap(),"F17_LIVE_A_OK\n");
    say(&format!("a: candidate {a_candidate}: {}",wt_git(&a_worktree,&["log","-1","--format=%s <%ae>"])));

    // 2. The harness submits A's result; the worker's claims are untrusted.
    let objects=f.git(&["rev-list","--objects","--all",&a_candidate]).lines().map(|line|{let oid=line.split_whitespace().next().unwrap();serde_json::json!({"oid":oid,"relative_path":format!("{}/{}",&oid[..2],&oid[2..])})}).collect::<Vec<_>>();
    let submission=f.home.path().join("a-result.json");
    fs::write(&submission,serde_json::to_vec(&serde_json::json!({"idempotency_key":"a-live-result","task_id":"a","contract_revision":1,"contract_digest":installed["digest"],"attempt_id":a_attempt,
        "repository":repository,"base_oid":start,"candidate_oid":a_candidate,"object_format":"sha256",
        "artifact_manifest":[{"path":"src/a.txt","oid":a_candidate}],"claimed_checks":["worker says all checks passed"],"objects":objects})).unwrap()).unwrap();
    let a=cli(&["result","demo","submit","--input-file",submission.to_str().unwrap()])["submission_id"].as_str().unwrap().to_owned();
    say(&format!("a: submitted {a}"));
    let job=|submission:&str|{let snapshot=runtime::snapshot(&f.project).unwrap();snapshot.operations.into_iter().find(|op|op.kind=="integration.run"&&op.payload["submission_id"]==submission)
        .map(|op|{let d=snapshot.deliveries.iter().find(|d|d.operation==op.id).unwrap().clone();(op,d)})};
    let count=|sql:&str|f.db().query_row(sql,[],|row|row.get::<_,u64>(0)).unwrap();
    let integrated_by=|task:&str|f.db().prepare("SELECT i.integrated_id,i.commit_oid FROM integrated_commits i JOIN integration_operations o ON o.operation_id=i.operation_id
        JOIN verified_results v ON v.result_id=o.verified_result_id JOIN verification_runs r ON r.run_id=v.run_id WHERE r.task_id=?1").unwrap()
        .query_map([task],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    let blockers=|task:&str|{
        let report=cli(&["scheduler","demo","inspect"]);
        let entry=report["entries"].as_array().unwrap().iter().find(|entry|entry["task"]==task).unwrap().clone();
        let mut found=entry["blockers"].as_array().unwrap().iter().map(|s|s.as_str().unwrap().to_owned())
            .filter(|s|["verified_dependency_evidence_unavailable:","predecessor_failed:","admission_disabled:"].iter().any(|p|s.starts_with(p))).collect::<Vec<_>>();
        found.sort();found
    };
    assert_eq!(blockers("b"),["verified_dependency_evidence_unavailable:a:integrated_commit"],"a submission releases nothing");

    // 3a. Kill -9 the ticker while A's integration candidate check runs.
    f.wait(&mut child,240,&||connections.load(Ordering::SeqCst)>=2&&job(&a).is_some_and(|(_,d)|d.state==DeliveryState::Claimed));
    assert!(integrated_by("a").is_empty());
    assert_eq!(blockers("b"),["verified_dependency_evidence_unavailable:a:integrated_commit"],"verification alone does not release b");
    let (op,_)=job(&a).unwrap();
    child.0.kill().unwrap();child.0.wait().unwrap();held.store(false,Ordering::SeqCst);
    say(&format!("fault 1: ticker killed with SIGKILL while integration {} was claimed (state {})",&op.id.as_str()[..12],
        f.db().query_row("SELECT state FROM integration_operations WHERE idempotency_key=?1",[op.id.as_str()],|row|row.get::<_,String>(0)).unwrap()));
    let (_,claimed)=job(&a).unwrap();assert_eq!(claimed.state,DeliveryState::Claimed);assert!(integrated_by("a").is_empty());
    // A restart recovers only after the dead owner's lease lapses. Advance it for
    // this job alone when nothing else is claimed; otherwise wait it out.
    let snapshot=runtime::snapshot(&f.project).unwrap();
    // The integration's own `integration.lease` claim belongs to the killed job.
    let others=snapshot.deliveries.iter().filter(|d|d.state==DeliveryState::Claimed&&d.operation!=op.id
            &&!snapshot.operations.iter().any(|o|o.id==d.operation&&o.kind=="integration.lease"))
        .map(|d|snapshot.operations.iter().find(|o|o.id==d.operation).map_or("?".into(),|o|format!("{}@{}",o.kind,d.lease_until_ms.unwrap_or(0)-claimed.lease_until_ms.unwrap()))).collect::<Vec<_>>();
    say(&format!("other claims at the kill: {others:?}"));
    let others=others.len();
    if others==0 {migration::open_active(&f.project).unwrap().expire_claims(claimed.lease_until_ms.unwrap()+1).unwrap();}
    else {
        say(&format!("{others} other claims held; waiting for the lease to lapse"));
        while jiff::Timestamp::now().as_millisecond()<=claimed.lease_until_ms.unwrap(){std::thread::sleep(Duration::from_secs(1));}
        cli(&["operations","demo","expire"]);
    }
    let mut child=spawn();
    f.wait(&mut child,240,&||job(&a).is_some_and(|(_,d)|d.state==DeliveryState::Confirmed));
    let (a_integrated,a_commit)=integrated_by("a").pop().unwrap();
    assert_eq!(count("SELECT count(*) FROM verification_runs WHERE task_id='a'"),3,"one run per policy");
    assert_eq!(count("SELECT count(*) FROM integration_operations"),1);assert_eq!(count("SELECT count(*) FROM integrated_commits"),1);
    assert_eq!(job(&a).unwrap().1.attempts,2);
    assert_eq!(f.git(&["rev-parse",TARGET]),a_commit);
    assert_eq!(f.git(&["rev-parse",&format!("{a_commit}^1")]),start);assert_eq!(f.git(&["rev-parse",&format!("{a_commit}^2")]),a_candidate);
    assert_eq!(f.git(&["show",&format!("{a_commit}:src/a.txt")]),"F17_LIVE_A_OK");
    assert_eq!(blockers("b"),["admission_disabled:integrated_commit"],"b is released by a's integrated commit");
    say(&format!("a: restarted ticker integrated once: {a_commit} (delivery attempts 2, 1 integration, 1 publication); b released"));

    // 3b. Stale target: a fixture-verified result `s` is blocked, then recovered by the operator.
    cli(&["result","demo","auto","--integrate","off","--expected-head",&head().to_string()]);
    f.git(&["checkout","-q","-b","s",&start]);fs::write(f.repo.join("src/s.txt"),"s\n").unwrap();fs::write(f.repo.join("src/lib.rs"),"pub fn s() {}\n").unwrap();f.git(&["add","src"]);f.git(&["commit","-qm","fixture s"]);
    let s_candidate=f.git(&["rev-parse","HEAD"]);f.git(&["checkout","-q",&branch]);
    let clean=[("clean",r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#.to_owned())];
    let s=f.submit_at("s",&clean,&s_candidate);
    f.wait(&mut child,300,&||f.jobs().iter().any(|(op,d)|op.payload["submission_id"]==s.as_str()&&d.state==DeliveryState::Confirmed));
    assert_eq!(f.db().query_row("SELECT state FROM verification_runs WHERE submission_id=?1",[&s],|row|row.get::<_,String>(0)).unwrap(),"accepted");
    assert!(job(&s).is_none());
    let moved=f.git(&["commit-tree",&format!("{a_commit}^{{tree}}"),"-p",&a_commit,"-m","moved outside the controller"]);
    f.git(&["update-ref",TARGET,&moved,&a_commit]);
    cli(&["result","demo","auto","--integrate","on","--expected-head",&head().to_string()]);
    f.wait(&mut child,300,&||job(&s).is_some_and(|(_,d)|d.state==DeliveryState::PermanentFailure));
    let Some(Outcome::PermanentFailure{diagnostic})=job(&s).unwrap().1.last_outcome else {panic!("stale job is blocked")};
    assert!(diagnostic.contains("target moved"),"{diagnostic}");
    assert_eq!(f.git(&["rev-parse",TARGET]),moved,"the moved ref is not overwritten");assert_eq!(count("SELECT count(*) FROM integrated_commits"),1);
    say(&format!("fault 2: target moved to {moved}; s blocked: {diagnostic}"));
    let s_result=f.db().query_row("SELECT result_id FROM verified_results WHERE submission_id=?1",[&s],|row|row.get::<_,String>(0)).unwrap();
    let work=f.home.path().join("s-operator-integration");
    cli(&["result","demo","integrate",&s_result,"--repository",repository.to_str().unwrap(),"--idempotency-key","s-operator-recovery","--work-dir",work.to_str().unwrap()]);
    let tip=f.git(&["rev-parse",TARGET]);
    assert_eq!(f.git(&["rev-parse",&format!("{tip}^1")]),moved);assert_eq!(f.git(&["rev-parse",&format!("{tip}^2")]),s_candidate);
    assert_eq!(f.git(&["show",&format!("{tip}:src/s.txt")]),"s");assert_eq!(f.git(&["show",&format!("{tip}:src/a.txt")]),"F17_LIVE_A_OK");
    assert_eq!(count("SELECT count(*) FROM integrated_commits"),2);
    f.git(&["merge-base","--is-ancestor",&a_commit,&tip]);
    say(&format!("recovery: operator integrated s onto the moved target: {tip}"));

    // 4. Dependent B launches on A's integrated SHA.
    if live {f.stop(&mut child);}
    f.git(&["checkout","-q","--detach",&a_commit]);
    let b_prompt="F1.7 disposable acceptance task B. Work only in the current directory. Read src/a.txt and create src/b.txt containing exactly the same bytes. Do not commit, do not change any other file, and do not use the network. Then stop and reply DONE.";
    let (b_attempt,b_worktree)=launch("b",b_prompt);
    let record=runtime::snapshot(&f.project).unwrap().attempt_inputs.into_iter().find(|r|r.attempt.as_str()==b_attempt).unwrap();
    assert_eq!(record.inputs.repositories[0].commit,a_commit,"b's pinned base is a's integrated SHA");
    let satisfaction=f.db().query_row("SELECT satisfaction_id FROM dependency_satisfactions WHERE task_id='b' AND state='valid' AND evidence_id=?1",[&a_integrated],|row|row.get::<_,String>(0)).unwrap();
    assert_eq!(record.inputs.dependencies.len(),1);assert_eq!(record.inputs.dependencies[0].evidence.id,satisfaction);
    if live {
        trust(&[a_worktree.to_str().unwrap(),b_worktree.to_str().unwrap()]);
        child=spawn();
        f.wait(&mut child,300,&||{milestones();b_worktree.join("src/b.txt").is_file()}&&fs::read(b_worktree.join("src/b.txt")).unwrap()==b"F17_LIVE_A_OK\n");
    } else {
        let operation=record.operation.clone();
        let receipts=herdr_projects::worktree_preparation::prepare(&f.project,&operation,1,Instant::now()+Duration::from_secs(45),Default::default()).unwrap();
        assert_eq!(PathBuf::from(&receipts[0].plan.path),b_worktree);
        fs::copy(b_worktree.join("src/a.txt"),b_worktree.join("src/b.txt")).unwrap();
    }
    assert_eq!(wt_git(&b_worktree,&["rev-parse","HEAD"]),a_commit,"b's worktree is checked out at a's integrated SHA");
    say(&format!("b: worker started on {} and wrote src/b.txt from a's integrated file",wt_git(&b_worktree,&["rev-parse","HEAD"])));

    if live {
        assert_eq!(events("runtime.launch_started"),2,"the controller started both workers");
        // Stop both workers through the product and wait for proven termination.
        for attempt in [&a_attempt,&b_attempt] {
            let revision=runtime::snapshot(&f.project).unwrap().attempts.into_iter().find(|x|x.id.as_str()==attempt.as_str()).unwrap().revision;
            cli(&["task","demo","cancel-attempt",attempt,"--expected-revision",&revision.to_string(),"--expected-head",&head().to_string(),"--reason","F1.7 acceptance complete"]);
        }
        f.wait(&mut child,180,&||runtime::snapshot(&f.project).unwrap().attempts.iter().filter(|x|[&a_attempt,&b_attempt].contains(&&x.id.as_str().to_owned())).all(|x|x.termination_observed));
        f.stop(&mut child);
        for kind in ["runtime.launch_started","runtime.worker_terminated"] {assert_eq!(events(kind),2,"{kind}");}
        let brief=runtime::snapshot(&f.project).unwrap();
        let confirmed=brief.operations.iter().filter(|o|o.kind=="runtime.worker_brief").filter(|o|brief.deliveries.iter().any(|d|d.operation==o.id&&d.state==DeliveryState::Confirmed)).count();
        assert_eq!(confirmed,2,"each worker received its brief once");
        // Sanitized transcript: only the workers' shell commands and final replies.
        let mut rollouts=Vec::new();let mut stack=vec![agent_home.join(".codex/sessions")];
        while let Some(dir)=stack.pop(){for entry in fs::read_dir(&dir).into_iter().flatten().flatten(){let p=entry.path();if p.is_dir(){stack.push(p)}else if p.extension().is_some_and(|e|e=="jsonl"){rollouts.push(p)}}}
        rollouts.sort();
        for rollout in rollouts {
            eprintln!("--- worker session transcript excerpt");
            for line in fs::read_to_string(&rollout).unwrap().lines() {
                let Ok(value)=serde_json::from_str::<serde_json::Value>(line) else{continue};
                let payload=&value["payload"];
                let text=match (value["type"].as_str(),payload["type"].as_str()) {
                    (Some("response_item"),Some("function_call"))=>format!("$ {}",payload["arguments"].as_str().unwrap_or_default()),
                    (Some("response_item"),Some("custom_tool_call"))=>format!("$ {}",payload["input"].as_str().unwrap_or_default()),
                    (Some("response_item"),Some("local_shell_call"))=>format!("$ {}",payload["action"]["command"]),
                    (Some("response_item"),Some("message")) if payload["role"]=="assistant"=>format!("> {}",payload["content"].as_array().into_iter().flatten().filter_map(|c|c["text"].as_str()).collect::<Vec<_>>().join(" ")),
                    _=>continue,
                };
                eprintln!("{}",text.chars().take(200).collect::<String>());
            }
        }
    } else {f.stop(&mut child);}
    say(&format!("PASS: a integrated {a_commit}; s recovered {tip}; b base {a_commit}; launches {}",events("runtime.launch_started")));
    drop(cleanup);
}

#[cfg(feature="state-store")]
#[test]
fn profile_prepare_uses_pinned_owner_config_and_keeps_unknown_capabilities() {
    use std::{fs, os::unix::fs::PermissionsExt};
    use herdr_projects::{migration,runtime};
    let home=tempfile::tempdir().unwrap();
    let caller=tempfile::tempdir().unwrap();
    let root=home.path().join("root");
    for action in ["new","pause"] {
        let result=hp(home.path(), &["--root",root.to_str().unwrap(),action,"demo"]);
        assert!(result.status.success(),"{}",String::from_utf8_lossy(&result.stderr));
    }
    let config=home.path().join("owner.toml");
    fs::write(&config,"[authority]\nversion=1\nrevision=7\napproval_public_key='ssh-ed25519 AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'\n[profiles.worker]\nkind='claude'\npermission_policy='interactive'\nextra_args=['PRIVATE_ARG']\n[profiles.worker.budget]\nmax_wall_seconds=60\nunknown_usage='allow_with_warning'\n").unwrap();
    let project=root.join("demo");
    let plan=migration::inspect_with_config(&project,&config).unwrap();
    migration::apply(&project,&plan,true).unwrap();
    let herdr=home.path().join("herdr-bin");
    let agent=home.path().join("agent-bin");
    for (path,version) in [(&herdr,"herdr 0.9.1"),(&agent,"2.1.0 (Claude Code)")] {
        fs::write(path,format!("#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = --version ] || exit 2\nprintf '%s\\n' '{version}'\n")).unwrap();
        fs::set_permissions(path,fs::Permissions::from_mode(0o700)).unwrap();
    }
    let before=runtime::snapshot(&project).unwrap();
    let output=hp(caller.path(),&["--root",root.to_str().unwrap(),"profile","prepare","demo","worker",
        "--herdr-executable",herdr.to_str().unwrap(),"--agent-executable",agent.to_str().unwrap(),
        "--execution-home",home.path().to_str().unwrap()]);
    assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let result:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["profile"]["permission_policy"]["revision"],7);
    assert_eq!(result["profile"]["config"]["path"],config.to_str().unwrap());
    assert_eq!(result["profile"]["capabilities"]["launch"]["status"],"unknown");
    assert_eq!(result["launchable"],false);
    assert_eq!(result["certified"],false);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE_ARG"));
    assert_eq!(runtime::snapshot(&project).unwrap(),before);
    assert!(!caller.path().join(".config").exists());
}

#[cfg(all(feature = "state-store", target_os = "linux"))]
#[test]
fn task_contract_put_and_result_submit_keep_worker_bytes_untrusted() {
    use herdr_projects::{authority::CONTRACT_SIGNATURE_NAMESPACE, domain::*, migration, runtime};
    use std::process::Command;
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    for action in ["new", "pause"] {
        assert!(hp(home.path(), &["--root", root_arg, action, "demo"]).status.success());
    }
    let help = hp(home.path(), &["task", "demo", "contract", "put", "--help"]);
    assert!(help.status.success(), "{}", String::from_utf8_lossy(&help.stderr));
    let help_text = String::from_utf8_lossy(&help.stdout);
    assert!(help_text.contains("--input-file") && help_text.contains("--signature"));
    let submit_help = hp(home.path(), &["result", "demo", "submit", "--help"]);
    assert!(submit_help.status.success());
    assert!(String::from_utf8_lossy(&submit_help.stdout).contains("--input-file"));
    assert!(hp(home.path(), &["result", "demo", "show", "--help"]).status.success());
    assert!(!hp(home.path(), &["plan", "propose", "demo"]).status.success());
    let key = home.path().join("owner");
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).status().unwrap().success());
    let public = std::fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    let project = root.join("demo");
    let config = home.path().join("owner.toml");
    std::fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n")).unwrap();
    let plan = migration::inspect_with_config(&project, &config).unwrap();
    migration::apply(&project, &plan, true).unwrap();
    let snapshot = runtime::snapshot(&project).unwrap();
    runtime::set_state(&project, snapshot.head, snapshot.control.unwrap().revision, ProjectState::Active, &config).unwrap();
    let head = runtime::add_task(&project, TaskId::new("task").unwrap(), "work".into(), runtime::snapshot(&project).unwrap().head).unwrap();
    let db_path = project.join(".state/state.db");
    rusqlite::Connection::open(&db_path).unwrap().execute(
        "INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES('attempt-1','task',1,'running',NULL,'slot-1',0)",
        [],
    ).unwrap();
    let repo = home.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("/usr/bin/git").env_clear().env("PATH","/usr/bin:/bin").env("HOME",home.path())
            .env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null")
            .env("GIT_AUTHOR_NAME","fixture").env("GIT_AUTHOR_EMAIL","fixture@example.com")
            .env("GIT_COMMITTER_NAME","fixture").env("GIT_COMMITTER_EMAIL","fixture@example.com")
            .current_dir(&repo).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    git(&["init","-q","--object-format=sha256"]);
    std::fs::write(repo.join("outside-parent.txt"), "unchanged outside scope").unwrap();
    std::fs::write(repo.join(".gitignore"), "/verification-artifact.tar\n").unwrap();
    std::fs::write(repo.join("push"), "a literal output filename\n").unwrap();
    std::fs::create_dir(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/required.txt"), "required baseline output\n").unwrap();
    git(&["add","."]); git(&["commit","-qm","base"]);
    let base = git(&["rev-parse","HEAD"]);
    std::fs::write(repo.join("src/lib.rs"), "pub fn result() {}\n").unwrap();
    git(&["add","."]); git(&["commit","-qm","allowed output"]);
    let candidate = git(&["rev-parse","HEAD"]);
    let objects = || git(&["rev-list","--objects","--all"]).lines().map(|line| {
        let oid=line.split_whitespace().next().unwrap();
        serde_json::json!({"oid":oid,"relative_path":format!("{}/{}",&oid[..2],&oid[2..])})
    }).collect::<Vec<_>>();
    let policy = r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#;
    let authority = herdr_projects::authority::policy_reference(&project).unwrap();
    let mut document = serde_json::to_vec_pretty(&serde_json::json!({
        "version": 3,
        "outputs": [{"path":"src/lib.rs","kind":"git_file"},{"path":"src/required.txt","kind":"git_file"},{"path":"push","kind":"git_file"}],
        "scope": {"paths":[{"path":"src/","access":"write"},{"path":"push","access":"write"}]},
        "project_store": db_path.canonicalize().unwrap().display().to_string(),
        "expected_head": head,
        "task_id": "task",
        "contract_revision": 1,
        "deliverable": "ship",
        "non_goals": "no launch",
        "acceptance_policies": [{"id": "builds", "text": policy}],
        "repository": repo.canonicalize().unwrap().display().to_string(),
        "base_oid": base,
        "object_format": "sha256",
        "dependencies": [],
        "capability_flags": [],
        "profile_kind": "codex",
        "retry_class": "none",
        "result_schema_id": "result-v1",
        "route": "verify_then_integrate",
        "authority": authority
    })).unwrap();
    document.push(b'\n');
    let doc_path = home.path().join("contract.json");
    std::fs::write(&doc_path, &document).unwrap();
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&key).args(["-n", CONTRACT_SIGNATURE_NAMESPACE]).arg(&doc_path).status().unwrap().success());
    let sig_path = home.path().join("contract.json.sig");
    let before = runtime::snapshot(&project).unwrap();
    let huge = home.path().join("huge.json");
    std::fs::write(&huge, vec![b' '; 256 * 1024]).unwrap();
    let refused = hp(home.path(), &["--root", root_arg, "task", "demo", "contract", "put", "--input-file", huge.to_str().unwrap(), "--signature", sig_path.to_str().unwrap()]);
    assert!(!refused.status.success());
    assert_eq!(runtime::snapshot(&project).unwrap().head, before.head);
    // Signed invalid declarations must fail through the public installation command.
    let original: serde_json::Value = serde_json::from_slice(&document).unwrap();
    let mut invalid_outputs = vec![serde_json::Value::Null, serde_json::json!([])];
    for path in ["elsewhere/file", "src/../escape", "src/./file", "src//file", "src/*.txt", "src/", "src/.git/config", "/absolute"] {
        invalid_outputs.push(serde_json::json!([{"path":path,"kind":"git_file"}]));
    }
    invalid_outputs.push(serde_json::json!([{"path":"src/lib.rs","kind":"unknown"}]));
    for count in [2, 65] {
        invalid_outputs.push(serde_json::Value::Array(vec![original["outputs"][0].clone(); count]));
    }
    for (index, outputs) in invalid_outputs.into_iter().enumerate() {
        let mut invalid = original.clone(); invalid["outputs"] = outputs;
        let path = home.path().join(format!("invalid-outputs-{index}.json"));
        std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&key).args(["-n", CONTRACT_SIGNATURE_NAMESPACE]).arg(&path).status().unwrap().success());
        let signature = path.with_extension("json.sig");
        let rejected = hp(home.path(), &["--root", root_arg, "task", "demo", "contract", "put", "--input-file", path.to_str().unwrap(), "--signature", signature.to_str().unwrap()]);
        assert!(!rejected.status.success(), "accepted invalid output declaration {index}");
        assert_eq!(runtime::snapshot(&project).unwrap().head, before.head);
        assert_eq!(rusqlite::Connection::open(&db_path).unwrap().query_row("SELECT count(*) FROM task_contracts", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
    }
    let installed = hp(home.path(), &["--root", root_arg, "task", "demo", "contract", "put", "--input-file", doc_path.to_str().unwrap(), "--signature", sig_path.to_str().unwrap()]);
    assert!(installed.status.success(), "{}", String::from_utf8_lossy(&installed.stderr));
    let installed: serde_json::Value = serde_json::from_slice(&installed.stdout).unwrap();
    assert_eq!(installed["replayed"], false);
    let reserialized = serde_json::to_vec(&serde_json::from_slice::<serde_json::Value>(&document).unwrap()).unwrap();
    let rewritten = home.path().join("rewritten.json");
    std::fs::write(&rewritten, &reserialized).unwrap();
    let mismatched = hp(home.path(), &["--root", root_arg, "task", "demo", "contract", "put", "--input-file", rewritten.to_str().unwrap(), "--signature", sig_path.to_str().unwrap()]);
    assert!(!mismatched.status.success());
    let submission = home.path().join("result.json");
    std::fs::write(&submission, serde_json::to_vec(&serde_json::json!({
        "idempotency_key": "cli-key",
        "task_id": "task",
        "contract_revision": 1,
        "contract_digest": installed["digest"],
        "attempt_id": "attempt-1",
        "repository": repo.canonicalize().unwrap().display().to_string(),
        "base_oid": base,
        "candidate_oid": candidate,
        "object_format": "sha256",
        "artifact_manifest": [{"path": "src/lib.rs", "oid": candidate},{"path":"src/required.txt","oid":candidate},{"path":"push","oid":candidate}],
        "claimed_checks": ["cargo test"],
        "objects": objects()
    })).unwrap()).unwrap();
    let untrusted:serde_json::Value=serde_json::from_slice(&std::fs::read(&submission).unwrap()).unwrap();
    let before_submit=runtime::snapshot(&project).unwrap();
    let forged=home.path().join("forged-result.json");
    for field in ["verified","verification_receipt"] {
        let mut claimed=untrusted.clone();
        claimed[field]=if field=="verified" {serde_json::json!(true)} else {serde_json::json!({"run_id":"a".repeat(64),"result_id":"b".repeat(64),"commit_oid":candidate,"tree_oid":candidate,"object_format":"sha256","policy_digest":"c".repeat(64),"isolation":"linux-unshare-user-pid-mount-v1","exit_status":0,"memory_fence":0})};
        std::fs::write(&forged,claimed.to_string()).unwrap();
        let refused=hp(home.path(),&["--root",root_arg,"result","demo","submit","--input-file",forged.to_str().unwrap()]);
        assert!(!refused.status.success(),"worker-supplied {field} was accepted");
        assert_eq!(runtime::snapshot(&project).unwrap(),before_submit);
    }
    let mut missing: serde_json::Value = serde_json::from_slice(&std::fs::read(&submission).unwrap()).unwrap();
    missing["artifact_manifest"] = serde_json::json!([]);
    let missing_path = home.path().join("missing-output.json");
    std::fs::write(&missing_path, serde_json::to_vec(&missing).unwrap()).unwrap();
    let refused = hp(home.path(), &["--root", root_arg, "result", "demo", "submit", "--input-file", missing_path.to_str().unwrap()]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("required output"));
    assert_eq!(runtime::snapshot(&project).unwrap(), before_submit);
    let submitted = hp(home.path(), &["--root", root_arg, "result", "demo", "submit", "--input-file", submission.to_str().unwrap()]);
    assert!(submitted.status.success(), "{}", String::from_utf8_lossy(&submitted.stderr));
    let again = hp(home.path(), &["--root", root_arg, "result", "demo", "submit", "--input-file", submission.to_str().unwrap()]);
    assert!(again.status.success(), "{}", String::from_utf8_lossy(&again.stderr));
    let first: serde_json::Value = serde_json::from_slice(&submitted.stdout).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(first["submission_id"], second["submission_id"]);
    assert_eq!(second["replayed"], true);
    let shown = hp(home.path(), &["--root", root_arg, "result", "demo", "show"]);
    assert!(shown.status.success(), "{}", String::from_utf8_lossy(&shown.stderr));
    let shown: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown.as_array().unwrap().len(), 1);
    assert!(shown[0].get("verified").is_none());
    assert_eq!(shown[0]["claimed_checks"], serde_json::json!(["cargo test"]));
    let raw=rusqlite::Connection::open(&db_path).unwrap();
    for table in ["verification_runs","verified_results","dependency_satisfactions","feedback_items"] {
        assert_eq!(raw.query_row(&format!("SELECT count(*) FROM {table}"),[],|row|row.get::<_,u64>(0)).unwrap(),0,"result submission manufactured {table}");
    }
    let after_submit=runtime::snapshot(&project).unwrap();assert_eq!(after_submit.tasks,before_submit.tasks);assert_eq!(after_submit.attempts,before_submit.attempts);
    let tasks: i64 = rusqlite::Connection::open(&db_path).unwrap().query_row("SELECT count(*) FROM tasks", [], |row| row.get(0)).unwrap();
    assert_eq!(tasks, runtime::snapshot(&project).unwrap().tasks.len() as i64);
    // Verify real retained SHA-256 Git objects through the operator CLI.
    let policy_path = home.path().join("scope-policy.json"); std::fs::write(&policy_path, policy).unwrap();
    let work = home.path().join("scope-work");
    let verify = |id: &str, key: &str| hp(home.path(), &["--root",root_arg,"result","demo","verify",id,
        "--policy-id","builds","--policy-file",policy_path.to_str().unwrap(),"--idempotency-key",key,
        "--work-dir",work.to_str().unwrap(),"--timeout-seconds","30"]);
    let good = verify(first["submission_id"].as_str().unwrap(), "scope-good");
    assert!(good.status.success(), "{}", String::from_utf8_lossy(&good.stderr));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&good.stdout).unwrap()["state"], "accepted");
    // A newer historical receipt must not hide an older usable receipt from
    // the same active attempt when a consumer is queued after both runs.
    let newer = verify(first["submission_id"].as_str().unwrap(), "scope-newer-historical");
    assert!(newer.status.success(), "{}", String::from_utf8_lossy(&newer.stderr));
    let newer: serde_json::Value = serde_json::from_slice(&newer.stdout).unwrap();
    raw.execute("DELETE FROM verification_contract_checks WHERE result_id=?1", [newer["receipt"]["result_id"].as_str().unwrap()]).unwrap();
    // A historical receipt has no proof that today's scope/output checks ran.
    // Exercise receipt reuse through the public queue/scheduler workflow.
    runtime::add_task(&project, TaskId::new("scope-consumer").unwrap(), "consume result".into(), runtime::snapshot(&project).unwrap().head).unwrap();
    let queue_scope = home.path().join("queue-scope.json");
    std::fs::write(&queue_scope, r#"{"priority":0,"dependencies":[{"predecessor":"task","requirement":"verified_result"}]}"#).unwrap();
    // A database-side stall during evidence attachment must share the deadline
    // and roll back the queue mutation and its events. Timeout bounds the test
    // itself if the production interruption ever regresses.
    let before_queue = runtime::snapshot(&project).unwrap();
    raw.execute_batch("CREATE TRIGGER stall_receipt_attachment BEFORE INSERT ON dependency_satisfactions WHEN NEW.task_id='scope-consumer' BEGIN SELECT count(*) FROM (WITH RECURSIVE slow(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM slow WHERE n<1000000000) SELECT n FROM slow); END;").unwrap();
    let stalled = Command::new("/usr/bin/timeout").env_clear().env("HOME",home.path())
        .args(["--kill-after=1","10",BIN,"--root",root_arg,"task","demo","queue","scope-consumer","--input-file",queue_scope.to_str().unwrap(),"--expected-revision","1","--expected-head",&before_queue.head.to_string()]).output().unwrap();
    assert!(!stalled.status.success()); assert_ne!(stalled.status.code(), Some(124), "receipt attachment exceeded the test watchdog");
    assert!(String::from_utf8_lossy(&stalled.stderr).to_ascii_lowercase().contains("deadline"), "{}", String::from_utf8_lossy(&stalled.stderr));
    assert_eq!(runtime::snapshot(&project).unwrap(), before_queue);
    assert_eq!(raw.query_row("SELECT count(*) FROM task_queue WHERE task_id='scope-consumer'",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    assert_eq!(raw.query_row("SELECT count(*) FROM dependency_satisfactions WHERE task_id='scope-consumer'",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    raw.execute_batch("DROP TRIGGER stall_receipt_attachment;").unwrap();
    let queued = hp(home.path(), &["--root",root_arg,"task","demo","queue","scope-consumer","--input-file",queue_scope.to_str().unwrap(),"--expected-revision","1","--expected-head",&runtime::snapshot(&project).unwrap().head.to_string()]);
    assert!(queued.status.success(), "{}", String::from_utf8_lossy(&queued.stderr));
    let evidence_missing = || {
        let report = hp(home.path(), &["--root",root_arg,"scheduler","demo","inspect"]);
        assert!(report.status.success(), "{}", String::from_utf8_lossy(&report.stderr));
        let report: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
        report["entries"].as_array().unwrap().iter().find(|entry| entry["task"] == "scope-consumer").unwrap()["blockers"]
            .as_array().unwrap().iter().any(|item| item.as_str().unwrap().contains("verified_dependency_evidence_unavailable"))
    };
    assert!(!evidence_missing(), "fresh scope-checked receipt should satisfy the dependency");
    // Configure a target with an independent change, then return to the worker candidate.
    git(&["checkout","-qb","factory-integration",&base]);
    std::fs::write(repo.join("target-only.txt"), "independent target change\n").unwrap();
    git(&["add","target-only.txt"]); git(&["commit","-qm","target advance"]);
    let target_before = git(&["rev-parse","HEAD"]);
    let configure = || hp(home.path(), &["--root",root_arg,"result","demo","configure-integration","--repository",repo.to_str().unwrap(),"--reference","refs/heads/factory-integration"]);
    let checked_out = configure(); assert!(!checked_out.status.success());
    assert!(String::from_utf8_lossy(&checked_out.stderr).contains("checked out"));
    git(&["checkout","--detach",&candidate]);
    let configured = configure(); assert!(configured.status.success(), "{}", String::from_utf8_lossy(&configured.stderr));
    assert!(configure().status.success());
    let integration_work = home.path().join("integration-work");
    let integrate = |result: &str| hp(home.path(), &["--root",root_arg,"result","demo","integrate",result,
        "--repository",repo.to_str().unwrap(),"--idempotency-key","integrate-scope","--work-dir",integration_work.to_str().unwrap()]);
    let good: serde_json::Value = serde_json::from_slice(&good.stdout).unwrap();
    let old_result = good["receipt"]["result_id"].as_str().unwrap();
    // Version 1 attests earlier scope/output checks, not post-execution identity.
    raw.execute("UPDATE verification_contract_checks SET version=1", []).unwrap();
    let before_refusal = runtime::snapshot(&project).unwrap();
    let historical_integration = integrate(old_result); assert!(!historical_integration.status.success());
    assert!(String::from_utf8_lossy(&historical_integration.stderr).contains("fresh post-execution verification"));
    assert!(!integration_work.exists());
    assert_eq!(git(&["rev-parse","refs/heads/factory-integration"]), target_before);
    assert_eq!(runtime::snapshot(&project).unwrap(), before_refusal);

    assert!(evidence_missing(), "historical receipt must not certify newly introduced checks");
    let historical = verify(first["submission_id"].as_str().unwrap(), "scope-good");
    assert!(historical.status.success());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&historical.stdout).unwrap()["replayed"], true);
    assert!(evidence_missing(), "historical replay must not invent contract-check evidence");
    assert_eq!(raw.query_row("SELECT version FROM verification_contract_checks WHERE result_id=?1",[old_result],|row|row.get::<_,u32>(0)).unwrap(),1);
    let rechecked = verify(first["submission_id"].as_str().unwrap(), "scope-rechecked");
    assert!(rechecked.status.success(), "{}", String::from_utf8_lossy(&rechecked.stderr));
    assert!(!evidence_missing(), "fresh verification should restore usable evidence");
    let rechecked: serde_json::Value = serde_json::from_slice(&rechecked.stdout).unwrap();
    let result_id = rechecked["receipt"]["result_id"].as_str().unwrap();
    assert_eq!(raw.query_row("SELECT version FROM verification_contract_checks WHERE result_id=?1",[result_id],|row|row.get::<_,u32>(0)).unwrap(),2);
    std::fs::create_dir(&integration_work).unwrap();
    std::fs::write(integration_work.join("keep"), b"operator data").unwrap();
    assert!(!integrate(result_id).status.success());
    assert_eq!(std::fs::read(integration_work.join("keep")).unwrap(), b"operator data");
    std::fs::remove_dir_all(&integration_work).unwrap();
    // Fail after the candidate is retained, then resume it from a new scratch dir.
    raw.execute_batch("CREATE TRIGGER refuse_integration_checks BEFORE UPDATE OF checks_passed ON integration_operations WHEN NEW.checks_passed=1 BEGIN SELECT RAISE(ABORT,'injected checks write failure'); END;").unwrap();
    let incomplete = integrate(result_id); assert!(!incomplete.status.success());
    assert_eq!(raw.query_row("SELECT state FROM integration_operations WHERE idempotency_key='integrate-scope'",[],|r|r.get::<_,String>(0)).unwrap(), "candidate_prepared", "{}", String::from_utf8_lossy(&incomplete.stderr));
    assert_eq!(raw.query_row("SELECT checks_passed FROM integration_operations WHERE idempotency_key='integrate-scope'",[],|r|r.get::<_,bool>(0)).unwrap(), false);
    assert_eq!(git(&["rev-parse","refs/heads/factory-integration"]), target_before);
    assert!(!integration_work.exists());
    raw.execute_batch("DROP TRIGGER refuse_integration_checks; CREATE TRIGGER refuse_integration_receipt BEFORE INSERT ON integrated_commits BEGIN SELECT RAISE(ABORT,'injected receipt write failure'); END;").unwrap();
    // The next failure occurs after Git CAS but before the receipt commits.
    let interrupted = integrate(result_id); assert!(!interrupted.status.success());
    assert_eq!(raw.query_row("SELECT state FROM integration_operations WHERE idempotency_key='integrate-scope'",[],|r|r.get::<_,String>(0)).unwrap(), "validating", "{}", String::from_utf8_lossy(&interrupted.stderr));
    let published = git(&["rev-parse","refs/heads/factory-integration"]);
    assert_ne!(published, target_before);
    assert_eq!(raw.query_row("SELECT count(*) FROM integrated_commits",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    assert!(!integration_work.exists());
    raw.execute_batch("DROP TRIGGER refuse_integration_receipt;").unwrap();
    let integrated = hp(home.path(), &["--root",root_arg,"result","demo","reconcile-integration","--repository",repo.to_str().unwrap(),"--idempotency-key","integrate-scope"]);
    assert!(integrated.status.success(), "{}", String::from_utf8_lossy(&integrated.stderr));
    assert_eq!(git(&["rev-parse","refs/heads/factory-integration"]), published);
    let integrated: serde_json::Value = serde_json::from_slice(&integrated.stdout).unwrap();
    assert_eq!(integrated["state"], "integrated");
    let merged = git(&["rev-parse","refs/heads/factory-integration"]);
    assert_eq!(integrated["commit_oid"], merged);
    assert_eq!(git(&["rev-parse",&format!("{merged}^1")]), target_before);
    assert_eq!(git(&["rev-parse",&format!("{merged}^2")]), candidate);
    assert_eq!(git(&["show",&format!("{merged}:target-only.txt")]), "independent target change");
    assert!(!git(&["show",&format!("{merged}:src/lib.rs")]).is_empty());
    assert!(!integration_work.exists());
    let after_integration = runtime::snapshot(&project).unwrap();
    assert_eq!(after_integration.attempts, before_refusal.attempts);
    let replayed = integrate(result_id); assert!(replayed.status.success());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&replayed.stdout).unwrap(), integrated);
    let reconciled = hp(home.path(), &["--root",root_arg,"result","demo","reconcile-integration","--repository",repo.to_str().unwrap(),"--idempotency-key","integrate-scope"]);
    assert!(reconciled.status.success(), "{}", String::from_utf8_lossy(&reconciled.stderr));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&reconciled.stdout).unwrap(), integrated);
    assert_eq!(runtime::snapshot(&project).unwrap(), after_integration);
    assert_eq!(raw.query_row("SELECT count(*) FROM integrated_commits",[],|r|r.get::<_,u64>(0)).unwrap(),1);
    // Integrated dependencies require proof of the combined tree, independently
    // of the worker-tree verifier's receipt and historical integration status.
    let queue_integrated = |name: &str| {
        runtime::add_task(&project, TaskId::new(name).unwrap(), "consume merged result".into(), runtime::snapshot(&project).unwrap().head).unwrap();
        let input = home.path().join(format!("{name}.json"));
        std::fs::write(&input, r#"{"priority":0,"dependencies":[{"predecessor":"task","requirement":"integrated_commit"}]}"#).unwrap();
        let out = hp(home.path(), &["--root",root_arg,"task","demo","queue",name,"--input-file",input.to_str().unwrap(),"--expected-revision","1","--expected-head",&runtime::snapshot(&project).unwrap().head.to_string()]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    let integrated_missing = |name: &str| {
        let report = hp(home.path(), &["--root",root_arg,"scheduler","demo","inspect"]);
        assert!(report.status.success(), "{}", String::from_utf8_lossy(&report.stderr));
        let report: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
        report["entries"].as_array().unwrap().iter().find(|entry| entry["task"] == name).unwrap()["blockers"]
            .as_array().unwrap().iter().any(|item| item.as_str().unwrap().contains("verified_dependency_evidence_unavailable"))
    };
    queue_integrated("integrated-consumer");
    assert!(!integrated_missing("integrated-consumer"));
    raw.execute("DELETE FROM integration_contract_checks", []).unwrap();
    assert!(integrated_missing("integrated-consumer"), "historical integration must not certify merged outputs");
    let historical = integrate(result_id); assert!(historical.status.success());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&historical.stdout).unwrap(), integrated);
    assert!(integrated_missing("integrated-consumer"), "replay must not invent merged-output proof");
    queue_integrated("late-integrated-consumer");
    assert!(integrated_missing("late-integrated-consumer"));
    assert_eq!(raw.query_row("SELECT count(*) FROM dependency_satisfactions WHERE task_id='late-integrated-consumer' AND state='valid'",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    let fresh = hp(home.path(), &["--root",root_arg,"result","demo","integrate",result_id,
        "--repository",repo.to_str().unwrap(),"--idempotency-key","integrate-fresh-proof","--work-dir",integration_work.to_str().unwrap()]);
    assert!(fresh.status.success(), "{}", String::from_utf8_lossy(&fresh.stderr));
    assert!(!integrated_missing("integrated-consumer")); assert!(!integrated_missing("late-integrated-consumer"));
    assert_eq!(raw.query_row("SELECT count(*) FROM integrated_commits",[],|r|r.get::<_,u64>(0)).unwrap(),2);
    assert_eq!(raw.query_row("SELECT count(*) FROM integration_contract_checks",[],|r|r.get::<_,u64>(0)).unwrap(),1);
    // Moving an outside file into src still changes the unauthorized old path.
    git(&["mv","outside-parent.txt","src/renamed.txt"]); git(&["commit","-qm","unauthorized rename"]);
    let bad_oid = git(&["rev-parse","HEAD"]);
    let mut bad = untrusted.clone(); bad["idempotency_key"] = "scope-bad-result".into();
    bad["candidate_oid"] = bad_oid.into(); bad["objects"] = serde_json::json!(objects());
    std::fs::write(&submission, serde_json::to_vec(&bad).unwrap()).unwrap();
    let submitted = hp(home.path(), &["--root",root_arg,"result","demo","submit","--input-file",submission.to_str().unwrap()]);
    assert!(submitted.status.success(), "{}", String::from_utf8_lossy(&submitted.stderr));
    let submitted: serde_json::Value=serde_json::from_slice(&submitted.stdout).unwrap();
    let bad_id=submitted["submission_id"].as_str().unwrap();
    let before_violation=runtime::snapshot(&project).unwrap();
    raw.execute_batch("CREATE TRIGGER refuse_scope_stop BEFORE INSERT ON attempt_cancellations BEGIN SELECT RAISE(ABORT,'injected stop failure'); END;").unwrap();
    let failed=verify(bad_id,"scope-bad"); assert!(!failed.status.success());
    assert_eq!(runtime::snapshot(&project).unwrap(),before_violation);
    assert_eq!(raw.query_row("SELECT count(*) FROM verification_runs",[],|r|r.get::<_,u64>(0)).unwrap(),3);
    assert_eq!(raw.query_row("SELECT count(*) FROM feedback_items",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    raw.execute_batch("DROP TRIGGER refuse_scope_stop;").unwrap();
    let rejected=verify(bad_id,"scope-bad"); assert!(!rejected.status.success());
    let rejected:serde_json::Value=serde_json::from_slice(&rejected.stdout).unwrap();
    assert_eq!(rejected["reason"],"scope_violation"); assert!(rejected["receipt"].is_null());
    let after_violation=runtime::snapshot(&project).unwrap();
    let held=after_violation.attempts.iter().find(|a|a.id.as_str()=="attempt-1").unwrap();
    assert!(held.retains_capacity()); assert_eq!(held.reservation,"slot-1");
    assert_eq!(after_violation.cancellations.len(),1);
    assert_eq!(raw.query_row("SELECT count(*) FROM verified_results",[],|r|r.get::<_,u64>(0)).unwrap(),3);
    assert_eq!(raw.query_row("SELECT count(*) FROM feedback_items WHERE reason='scope_violation'",[],|r|r.get::<_,u64>(0)).unwrap(),1);
    assert_eq!(raw.query_row("SELECT count(*) FROM replan_pending_feedback",[],|r|r.get::<_,u64>(0)).unwrap(),1);
    let replay=verify(bad_id,"scope-bad"); assert!(!replay.status.success());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&replay.stdout).unwrap()["replayed"],true);
    assert_eq!(runtime::snapshot(&project).unwrap(),after_violation);
    let feedback_id:String=raw.query_row("SELECT feedback_id FROM feedback_items WHERE reason='scope_violation'",[],|r|r.get(0)).unwrap();
    let replan=hp(home.path(), &["--root",root_arg,"feedback","demo","replan",&feedback_id]);
    assert!(replan.status.success(), "{}", String::from_utf8_lossy(&replan.stderr));
    assert_eq!(raw.query_row("SELECT count(*) FROM replan_feedback_links WHERE feedback_id=?1",[&feedback_id],|r|r.get::<_,u64>(0)).unwrap(),1);
    assert_eq!(runtime::snapshot(&project).unwrap().attempts,after_violation.attempts);


    // Direct signed installation must reject cycles even before queue edges exist.
    for task in ["peer", "leaf", "queued"] {
        runtime::add_task(&project, TaskId::new(task).unwrap(), task.into(), runtime::snapshot(&project).unwrap().head).unwrap();
    }
    let install = |name: &str, task: &str, revision: u64, predecessors: &[&str]| {
        let mut next: serde_json::Value = serde_json::from_slice(&document).unwrap();
        next["task_id"] = task.into(); next["contract_revision"] = revision.into();
        next["expected_head"] = runtime::snapshot(&project).unwrap().head.into();
        next["dependencies"] = serde_json::json!(predecessors.iter().map(|predecessor| serde_json::json!({"predecessor":predecessor,"edge":"verified_result","policy_id":"builds"})).collect::<Vec<_>>());
        let path = home.path().join(format!("{name}.json"));
        std::fs::write(&path, serde_json::to_vec(&next).unwrap()).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&key).args(["-n", CONTRACT_SIGNATURE_NAMESPACE]).arg(&path).status().unwrap().success());
        let signature = path.with_extension("json.sig");
        hp(home.path(), &["--root", root_arg, "task", "demo", "contract", "put", "--input-file", path.to_str().unwrap(), "--signature", signature.to_str().unwrap()])
    };
    for (name, task, dependencies) in [("peer-contract", "peer", vec!["task"]), ("leaf-contract", "leaf", vec!["peer"])] {
        let output = install(name, task, 1, &dependencies);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    let queue = home.path().join("queue-cycle.json");
    std::fs::write(&queue, r#"{"priority":0,"dependencies":[{"predecessor":"task","requirement":"verified_result"}]}"#).unwrap();
    let output = hp(home.path(), &["--root", root_arg, "task", "demo", "queue", "queued", "--input-file", queue.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &runtime::snapshot(&project).unwrap().head.to_string()]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let before_cycle = runtime::snapshot(&project).unwrap();
    for (name, dependency) in [("direct-cycle", "peer"), ("transitive-cycle", "leaf"), ("queue-cycle-contract", "queued")] {
        let output = install(name, "task", 2, &[dependency]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("dependency cycle"), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(runtime::snapshot(&project).unwrap(), before_cycle);
        assert_eq!(raw.query_row("SELECT count(*) FROM task_contracts WHERE task_id='task'", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
    }
    // An old revision's edge must not reject a now-acyclic replacement graph.
    let output = install("peer-remove-edge", "peer", 2, &[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let output = install("acyclic-root", "task", 2, &["leaf"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let after = runtime::snapshot(&project).unwrap();
    assert_eq!(after.attempts, before_cycle.attempts);
    assert_eq!(raw.query_row("SELECT count(*) FROM task_contracts WHERE task_id='task'", [], |row| row.get::<_,u64>(0)).unwrap(), 2);

    // A target-side deletion of an unchanged required output merges cleanly.
    // The signed policy alone (git diff --quiet) cannot establish its presence.
    for kind in ["missing", "symlink", "directory", "ancestor-symlink"] {
        git(&["checkout","factory-integration"]);
        let required = repo.join("src/required.txt");
        if kind == "ancestor-symlink" {
            git(&["rm","-r","src"]);
            std::fs::create_dir(repo.join("payload")).unwrap();
            std::fs::write(repo.join("payload/required.txt"), "looks like a required file").unwrap();
            std::fs::write(repo.join("payload/lib.rs"), "pub fn result() {}\n").unwrap();
            std::os::unix::fs::symlink("payload", repo.join("src")).unwrap();
        } else {
            if required.symlink_metadata().is_ok() { std::fs::remove_file(&required).unwrap(); }
            match kind {
                "symlink" => std::os::unix::fs::symlink("lib.rs", &required).unwrap(),
                "directory" => { std::fs::create_dir(&required).unwrap(); std::fs::write(required.join("child"), "not a file").unwrap(); },
                _ => {},
            }
        }
        git(&["add","-A"]); git(&["commit","-qm",&format!("target output {kind}")]);
        let before_target = git(&["rev-parse","HEAD"]);
        git(&["checkout","--detach",&candidate]);
        let before_attempts = runtime::snapshot(&project).unwrap().attempts;
        let key = format!("integrate-output-{kind}");
        let run = || hp(home.path(), &["--root",root_arg,"result","demo","integrate",result_id,
            "--repository",repo.to_str().unwrap(),"--idempotency-key",&key,"--work-dir",integration_work.to_str().unwrap()]);
        let blocked = run();
        assert!(!blocked.status.success(), "integration accepted {kind} required output");
        let blocked: serde_json::Value = serde_json::from_slice(&blocked.stdout).unwrap();
        assert_eq!(blocked["state"], "blocked"); assert_eq!(blocked["reason"], "required_output_missing");
        assert_eq!(git(&["rev-parse","refs/heads/factory-integration"]), before_target);
        assert_eq!(raw.query_row("SELECT count(*) FROM integrated_commits",[],|r|r.get::<_,u64>(0)).unwrap(),2);
        assert_eq!(runtime::snapshot(&project).unwrap().attempts, before_attempts);
        assert!(!integration_work.exists());
        let feedback = raw.query_row("SELECT count(*) FROM feedback_items WHERE reason='required_output_missing'",[],|r|r.get::<_,u64>(0)).unwrap();
        let replay = run(); assert!(!replay.status.success());
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&replay.stdout).unwrap(), blocked);
        assert_eq!(raw.query_row("SELECT count(*) FROM feedback_items WHERE reason='required_output_missing'",[],|r|r.get::<_,u64>(0)).unwrap(), feedback);
        assert!(feedback > 0);
    }

    // Signed checks must preserve exact source/index identity through execution,
    // and rejected runs must retain the actual check's exit status.
    let alternate_blob=git(&["rev-parse",&format!("{candidate}:outside-parent.txt")]);
    let cases=[
        ("index",vec!["update-index".to_string(),"--cacheinfo".into(),format!("100644,{alternate_blob},src/lib.rs")],"tampered_tree",0),
        ("head",vec!["update-ref".into(),"HEAD".into(),base.clone()],"tampered_tree",0),
        ("tracked",vec!["restore".into(),format!("--source={base}"),"--worktree".into(),"--".into(),"src/lib.rs".into()],"tampered_tree",0),
        ("exit128",vec!["cat-file".into(),"-e".into(),"not-an-object".into()],"checks_failed",128),
        ("ignored-output",vec!["archive".into(),"--format=tar".into(),"--output=verification-artifact.tar".into(),"HEAD".into()],"",0),
    ];
    let mut outcome_failures=Vec::new();
    for (name,args,reason,exit_status) in cases {
        let task=format!("verify-outcome-{name}");let attempt=format!("{task}-attempt");
        let head=runtime::add_task(&project,TaskId::new(task.clone()).unwrap(),task.clone(),runtime::snapshot(&project).unwrap().head).unwrap();
        raw.execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,1,'running',NULL,?1,0)",rusqlite::params![attempt,task]).unwrap();
        let argv=std::iter::once("/usr/bin/git".to_string()).chain(args).collect::<Vec<_>>();
        let body=serde_json::json!({"version":1,"checks":argv}).to_string();
        let mut contract=original.clone();contract["task_id"]=task.clone().into();contract["expected_head"]=head.into();
        contract["acceptance_policies"]=serde_json::json!([{"id":"builds","text":body}]);
        if name=="ignored-output" {
            contract["version"]=1.into();contract.as_object_mut().unwrap().remove("outputs");
            contract["scope"]=serde_json::json!({"paths":[]});
            contract["acceptance_policies"].as_array_mut().unwrap().push(
                serde_json::json!({"id":"second-policy","text":body}));
        }
        let document_path=home.path().join(format!("{task}.json"));
        std::fs::write(&document_path,serde_json::to_vec(&contract).unwrap()).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y","sign","-f"]).arg(&key).args(["-n",CONTRACT_SIGNATURE_NAMESPACE]).arg(&document_path).status().unwrap().success());
        let signature=document_path.with_extension("json.sig");
        let installed=hp(home.path(), &["--root",root_arg,"task","demo","contract","put","--input-file",document_path.to_str().unwrap(),"--signature",signature.to_str().unwrap()]);
        assert!(installed.status.success(),"{}",String::from_utf8_lossy(&installed.stderr));
        let installed:serde_json::Value=serde_json::from_slice(&installed.stdout).unwrap();
        let mut proposal=untrusted.clone();proposal["task_id"]=task.into();proposal["attempt_id"]=attempt.into();
        proposal["contract_digest"]=installed["digest"].clone();proposal["idempotency_key"]=format!("outcome-{name}").into();proposal["objects"]=serde_json::json!(objects());
        let submission_path=home.path().join(format!("outcome-{name}-submission.json"));
        std::fs::write(&submission_path,serde_json::to_vec(&proposal).unwrap()).unwrap();
        let submitted=hp(home.path(), &["--root",root_arg,"result","demo","submit","--input-file",submission_path.to_str().unwrap()]);
        assert!(submitted.status.success(),"{}",String::from_utf8_lossy(&submitted.stderr));
        let submitted:serde_json::Value=serde_json::from_slice(&submitted.stdout).unwrap();
        std::fs::write(&policy_path,&body).unwrap();
        let before_attempts=runtime::snapshot(&project).unwrap().attempts;
        let output=verify(submitted["submission_id"].as_str().unwrap(),&format!("outcome-{name}"));
        let outcome:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();
        let observed:Option<i64>=raw.query_row("SELECT exit_status FROM verification_runs WHERE run_id=?1",[outcome["run_id"].as_str().unwrap()],|row|row.get(0)).unwrap();
        eprintln!("verifier outcome {name}: state={}, reason={}, exit_status={observed:?}",outcome["state"],outcome["reason"]);
        let expected_accept=reason.is_empty();
        let expected_reason=if expected_accept {serde_json::Value::Null} else {serde_json::json!(reason)};
        if output.status.success()!=expected_accept||outcome["state"]!=if expected_accept {"accepted"} else {"rejected"}||outcome["reason"]!=expected_reason||outcome["receipt"].is_null()==expected_accept||observed!=Some(exit_status) {
            outcome_failures.push(format!("{name}: {outcome}, exit_status={observed:?}"));
        }
        assert_eq!(runtime::snapshot(&project).unwrap().attempts,before_attempts);
        assert!(!work.exists());
        let replay=verify(submitted["submission_id"].as_str().unwrap(),&format!("outcome-{name}"));
        let replay:serde_json::Value=serde_json::from_slice(&replay.stdout).unwrap();
        assert_eq!(replay["run_id"],outcome["run_id"]);assert_eq!(replay["replayed"],true);
        assert_eq!(raw.query_row("SELECT exit_status FROM verification_runs WHERE run_id=?1",[outcome["run_id"].as_str().unwrap()],|row|row.get::<_,Option<i64>>(0)).unwrap(),observed);
        let receipts:u64=raw.query_row("SELECT count(*) FROM verified_results WHERE run_id=?1",[outcome["run_id"].as_str().unwrap()],|row|row.get(0)).unwrap();
        assert_eq!(receipts,u64::from(expected_accept));
        if expected_accept {
            let old=outcome["receipt"]["result_id"].as_str().unwrap();
            raw.execute("UPDATE verification_contract_checks SET version=1 WHERE result_id=?1",[old]).unwrap();
            let consumer="unscoped-historical-consumer";
            let head=runtime::add_task(&project,TaskId::new(consumer).unwrap(),consumer.into(),runtime::snapshot(&project).unwrap().head).unwrap();
            let request=home.path().join("unscoped-history-queue.json");
            std::fs::write(&request,serde_json::json!({"priority":0,"dependencies":[{"predecessor":proposal["task_id"],"requirement":"verified_result"}]}).to_string()).unwrap();
            let queued=hp(home.path(), &["--root",root_arg,"task","demo","queue",consumer,"--input-file",request.to_str().unwrap(),"--expected-revision","1","--expected-head",&head.to_string()]);
            assert!(queued.status.success(),"{}",String::from_utf8_lossy(&queued.stderr));
            let usable=||raw.query_row("SELECT count(*) FROM dependency_satisfactions WHERE task_id=?1 AND state='valid'",[consumer],|row|row.get::<_,u64>(0)).unwrap();
            assert_eq!(usable(),0,"unscoped historical proof must not release a dependency");
            let replay=verify(submitted["submission_id"].as_str().unwrap(),&format!("outcome-{name}"));assert!(replay.status.success());
            assert_eq!(usable(),0);
            assert_eq!(raw.query_row("SELECT version FROM verification_contract_checks WHERE result_id=?1",[old],|row|row.get::<_,u32>(0)).unwrap(),1);
            let fresh=verify(submitted["submission_id"].as_str().unwrap(),"unscoped-fresh-proof");
            assert!(fresh.status.success(),"{}",String::from_utf8_lossy(&fresh.stderr));
            assert_eq!(usable(),1);
            let submission_id=submitted["submission_id"].as_str().unwrap();
            let pending=||raw.query_row("SELECT count(*) FROM pending_verification_work WHERE submission_id=?1",[submission_id],|row|row.get::<_,u64>(0)).unwrap();
            assert_eq!(pending(),1,"running one policy, even repeatedly, must not hide another pending policy");
            let second=hp(home.path(), &["--root",root_arg,"result","demo","verify",submission_id,
                "--policy-id","second-policy","--policy-file",policy_path.to_str().unwrap(),
                "--idempotency-key","second-policy-run","--work-dir",work.to_str().unwrap()]);
            assert!(second.status.success(),"{}",String::from_utf8_lossy(&second.stderr));
            assert_eq!(pending(),0,"all policies now have completed runs");
        }
    }
    assert!(outcome_failures.is_empty(),"verifier outcome failures: {outcome_failures:?}");

    // Capability inspection must ignore profiles for other adapters/store identities,
    // but still validate the selected report rather than trusting its indexed fields.
    use std::os::unix::fs::MetadataExt;
    let capability_head = runtime::add_task(&project, TaskId::new("capability-reader").unwrap(), "inspect capabilities".into(), runtime::snapshot(&project).unwrap().head).unwrap();
    let mut contract = original.clone();
    contract["task_id"] = "capability-reader".into();
    contract["expected_head"] = capability_head.into();
    contract["capability_flags"] = serde_json::json!(["discovered"]);
    let path = home.path().join("capability-reader.json");
    std::fs::write(&path, serde_json::to_vec(&contract).unwrap()).unwrap();
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y","sign","-f"]).arg(&key).args(["-n",CONTRACT_SIGNATURE_NAMESPACE]).arg(&path).status().unwrap().success());
    let signature = path.with_extension("json.sig");
    let installed = hp(home.path(), &["--root",root_arg,"task","demo","contract","put","--input-file",path.to_str().unwrap(),"--signature",signature.to_str().unwrap()]);
    assert!(installed.status.success(), "{}", String::from_utf8_lossy(&installed.stderr));
    let request = home.path().join("capability-queue.json");
    std::fs::write(&request,r#"{"priority":0,"dependencies":[]}"#).unwrap();
    let queued = hp(home.path(), &["--root",root_arg,"task","demo","queue","capability-reader","--input-file",request.to_str().unwrap(),"--expected-revision","1","--expected-head",&runtime::snapshot(&project).unwrap().head.to_string()]);
    assert!(queued.status.success(), "{}", String::from_utf8_lossy(&queued.stderr));
    let store_path = db_path.canonicalize().unwrap();
    let metadata = std::fs::metadata(&store_path).unwrap();
    let insert_profile = |id:&str, kind:&str, inode:u64| {
        let report = serde_json::json!({"source_store":[store_path,metadata.dev(),inode],"preparation":{"profile":{"kind":kind}}}).to_string();
        raw.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('profile.native_retained',?1,1,1,'{}')",[id]).unwrap();
        raw.execute("INSERT INTO native_profiles VALUES(?1,?2,?3,(SELECT max(sequence) FROM events))",rusqlite::params![id,report,"0".repeat(64)]).unwrap();
    };
    insert_profile(&"1".repeat(64), "claude", metadata.ino());
    insert_profile(&"2".repeat(64), "codex", metadata.ino()+1);
    let inspect = || hp(home.path(), &["--root",root_arg,"scheduler","demo","inspect"]);
    let before_inspect:u64 = raw.query_row("SELECT max(sequence) FROM events",[],|r|r.get(0)).unwrap();
    let report = inspect();
    assert!(report.status.success(), "unrelated native profile history blocked inspection: {}", String::from_utf8_lossy(&report.stderr));
    let report:serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    let entry = report["entries"].as_array().unwrap().iter().find(|entry|entry["task"]=="capability-reader").unwrap();
    assert!(entry["blockers"].as_array().unwrap().iter().any(|blocker|blocker=="capability_unsupported"));
    assert_eq!(raw.query_row("SELECT max(sequence) FROM events",[],|r|r.get::<_,u64>(0)).unwrap(),before_inspect);
    insert_profile(&"3".repeat(64), "codex", metadata.ino());
    let selected_corrupt = inspect();
    assert!(!selected_corrupt.status.success());
    assert!(String::from_utf8_lossy(&selected_corrupt.stderr).contains("native profile report digest mismatch"));

}

#[cfg(all(feature = "state-store", target_os = "linux"))]
#[test]
fn signed_factory_admission_command_stores_raw_bytes_or_writes_a_denial() {
    use herdr_projects::{authority, domain::ProjectState, integration, migration, runtime};
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::process::Command;
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    for action in ["new", "pause"] {
        assert!(hp(home.path(), &["--root", root_arg, action, "demo"]).status.success());
    }
    let help = hp(home.path(), &["factory", "admission", "--help"]);
    assert!(help.status.success(), "{}", String::from_utf8_lossy(&help.stderr));
    let help_text = String::from_utf8_lossy(&help.stdout);
    assert!(help_text.contains("--enable") && help_text.contains("--disable"));
    assert!(help_text.contains("--policy") && help_text.contains("--signature") && help_text.contains("--evidence"));
    assert!(!help_text.contains("--sql"));
    let key = home.path().join("owner");
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).status().unwrap().success());
    let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    let project = root.join("demo");
    let config = home.path().join("owner.toml");
    fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n")).unwrap();
    let plan = migration::inspect_with_config(&project, &config).unwrap();
    migration::apply(&project, &plan, true).unwrap();
    let snapshot = runtime::snapshot(&project).unwrap();
    runtime::set_state(&project, snapshot.head, snapshot.control.unwrap().revision, ProjectState::Active, &config).unwrap();
    let db_path = project.join(".state/state.db");
    let store = db_path.canonicalize().unwrap().display().to_string();
    let column = || -> String {
        rusqlite::Connection::open(&db_path).unwrap().query_row(
            "SELECT factory_admission FROM project_control WHERE singleton=1",
            [],
            |row| row.get(0),
        ).unwrap()
    };
    let policy_rows = || -> i64 {
        rusqlite::Connection::open(&db_path).unwrap().query_row(
            "SELECT count(*) FROM factory_admission_policies",
            [],
            |row| row.get(0),
        ).unwrap()
    };
    let denials = || authority::denials(&project).unwrap().into_iter().filter(|denial| denial.class == "admission").count();
    assert_eq!(column(), "off");
    assert_eq!(policy_rows(), 0);
    let unsigned = hp(home.path(), &["--root", root_arg, "factory", "admission", "demo", "--enable"]);
    assert!(!unsigned.status.success());
    let sql_shaped = hp(home.path(), &["--root", root_arg, "factory", "admission", "demo", "--sql", "UPDATE project_control SET factory_admission='on'"]);
    assert!(!sql_shaped.status.success());
    assert_eq!(column(), "off");
    assert_eq!(denials(), 0);
    let repo = home.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let status = Command::new("/usr/bin/git").args(args).current_dir(&repo)
            .env("GIT_AUTHOR_NAME", "integrator").env("GIT_AUTHOR_EMAIL", "integrator@example.com")
            .env("GIT_COMMITTER_NAME", "integrator").env("GIT_COMMITTER_EMAIL", "integrator@example.com")
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0").env("GIT_NO_LAZY_FETCH", "1")
            .status().unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "--initial-branch", "main"]);
    fs::write(repo.join("README"), b"base\n").unwrap();
    git(&["add", "README"]);
    git(&["commit", "-m", "base"]);
    let repo = repo.canonicalize().unwrap();
    {
        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        integration::configure_integration_ref(&mut db, &repo, "refs/heads/integration").unwrap();
    }
    let authority_ref = serde_json::to_value(authority::policy_reference(&project).unwrap()).unwrap();
    let manifest = serde_json::to_vec(&serde_json::json!({"vertical_slice": "pass", "git_sha": "fixture"})).unwrap();
    let digest = format!("{:x}", Sha256::digest(&manifest));
    let evidence = home.path().join("manifest.json");
    fs::write(&evidence, &manifest).unwrap();
    let sign = |bytes: &[u8], name: &str| {
        let path = home.path().join(name);
        fs::write(&path, bytes).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&key)
            .args(["-n", authority::ADMISSION_SIGNATURE_NAMESPACE]).arg(&path).status().unwrap().success());
        (path, home.path().join(format!("{name}.sig")))
    };
    let document = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "enabled": true,
        "project_store": store,
        "evidence_digest": digest,
        "authority": authority_ref,
    })).unwrap();
    let (policy, signature) = sign(&document, "enable.json");
    let run = |args: &[String]| {
        let mut cmd = vec!["--root".into(), root_arg.into(), "factory".into(), "admission".into(), "demo".into()];
        cmd.extend(args.iter().cloned());
        let borrowed: Vec<&str> = cmd.iter().map(String::as_str).collect();
        hp(home.path(), &borrowed)
    };
    let enable_args = [
        "--enable".into(),
        "--policy".into(), policy.display().to_string(),
        "--signature".into(), signature.display().to_string(),
        "--evidence".into(), evidence.display().to_string(),
    ];
    let missing = run(&enable_args);
    assert!(!missing.status.success(), "{}", String::from_utf8_lossy(&missing.stdout));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("integration ref is missing"), "{}", String::from_utf8_lossy(&missing.stderr));
    assert_eq!(column(), "off");
    assert_eq!(policy_rows(), 0);
    assert_eq!(denials(), 1);
    git(&["branch", "integration"]);
    git(&["checkout", "integration"]);
    let checked_out = run(&enable_args);
    assert!(!checked_out.status.success());
    assert!(String::from_utf8_lossy(&checked_out.stderr).contains("integration ref is checked out"), "{}", String::from_utf8_lossy(&checked_out.stderr));
    assert_eq!(column(), "off");
    assert_eq!(denials(), 2);
    git(&["checkout", "main"]);
    let wrong_evidence = home.path().join("wrong.json");
    fs::write(&wrong_evidence, br#"{"vertical_slice":"pass","tampered":true}"#).unwrap();
    let mut wrong_args = enable_args.clone();
    *wrong_args.last_mut().unwrap() = wrong_evidence.display().to_string();
    let wrong = run(&wrong_args);
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("evidence digest does not match"), "{}", String::from_utf8_lossy(&wrong.stderr));
    assert_eq!(column(), "off");
    assert_eq!(denials(), 3);
    let reserialized = serde_json::to_vec_pretty(&serde_json::from_slice::<serde_json::Value>(&document).unwrap()).unwrap();
    assert_ne!(reserialized, document);
    let (rewritten, _) = sign(&reserialized, "rewritten.json");
    let mut rewritten_args = enable_args.clone();
    rewritten_args[2] = rewritten.display().to_string();
    let mismatched = run(&rewritten_args);
    assert!(!mismatched.status.success());
    assert!(String::from_utf8_lossy(&mismatched.stderr).to_ascii_lowercase().contains("signature"), "{}", String::from_utf8_lossy(&mismatched.stderr));
    assert_eq!(column(), "off");
    assert_eq!(denials(), 4);
    assert!(authority::denials(&project).unwrap().iter().any(|denial| denial.class == "admission" && denial.command == "enable" && denial.reason_code == "signature_failed"));
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("UPDATE store_meta SET schema_version=29; PRAGMA user_version=29;").unwrap();
    }
    let old_schema = run(&enable_args);
    assert!(!old_schema.status.success(), "{}", String::from_utf8_lossy(&old_schema.stderr));
    assert_eq!(column(), "off");
    assert_eq!(policy_rows(), 0);
    assert_eq!(denials(), 5);
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, 29);
        conn.execute_batch("UPDATE store_meta SET schema_version=30; PRAGMA user_version=30;").unwrap();
    }
    let truthy = serde_json::to_vec(&serde_json::json!({"vertical_slice": true})).unwrap();
    let truthy_path = home.path().join("true.json");
    fs::write(&truthy_path, &truthy).unwrap();
    let truthy_doc = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "enabled": true,
        "project_store": store,
        "evidence_digest": format!("{:x}", Sha256::digest(&truthy)),
        "authority": authority_ref,
    })).unwrap();
    let (truthy_policy, truthy_sig) = sign(&truthy_doc, "true-policy.json");
    let truthy_out = run(&[
        "--enable".into(),
        "--policy".into(), truthy_policy.display().to_string(),
        "--signature".into(), truthy_sig.display().to_string(),
        "--evidence".into(), truthy_path.display().to_string(),
    ]);
    assert!(!truthy_out.status.success());
    assert!(String::from_utf8_lossy(&truthy_out.stderr).contains("vertical slice evidence is not pass"), "{}", String::from_utf8_lossy(&truthy_out.stderr));
    assert_eq!(column(), "off");
    assert_eq!(denials(), 6);
    let enabled = run(&enable_args);
    assert!(enabled.status.success(), "{}", String::from_utf8_lossy(&enabled.stderr));
    let enabled: serde_json::Value = serde_json::from_slice(&enabled.stdout).unwrap();
    assert_eq!(enabled["factory_admission"], "on");
    assert_eq!(enabled["replayed"], false);
    assert_eq!(enabled["policy_digest"], format!("{:x}", Sha256::digest(&document)));
    assert_eq!(column(), "on");
    assert_eq!(policy_rows(), 1);
    assert_eq!(denials(), 6);
    let stored: Vec<u8> = rusqlite::Connection::open(&db_path).unwrap().query_row(
        "SELECT raw_bytes FROM factory_admission_policies WHERE policy_digest=?1",
        [enabled["policy_digest"].as_str().unwrap()],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(stored, document);
    let replay = run(&enable_args);
    assert!(replay.status.success(), "{}", String::from_utf8_lossy(&replay.stderr));
    let replay: serde_json::Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(policy_rows(), 1);
    assert_eq!(column(), "on");
    assert_eq!(denials(), 6);
    git(&["checkout", "integration"]);
    let disable_doc = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "enabled": false,
        "project_store": store,
        "evidence_digest": digest,
        "authority": authority_ref,
    })).unwrap();
    let (disable_policy, disable_sig) = sign(&disable_doc, "disable.json");
    let disabled = run(&[
        "--disable".into(),
        "--policy".into(), disable_policy.display().to_string(),
        "--signature".into(), disable_sig.display().to_string(),
    ]);
    assert!(disabled.status.success(), "{}", String::from_utf8_lossy(&disabled.stderr));
    let disabled: serde_json::Value = serde_json::from_slice(&disabled.stdout).unwrap();
    assert_eq!(disabled["factory_admission"], "off");
    assert_eq!(disabled["replayed"], false);
    assert_eq!(column(), "off");
    assert_eq!(policy_rows(), 2);
    let stored_disable: Vec<u8> = rusqlite::Connection::open(&db_path).unwrap().query_row(
        "SELECT raw_bytes FROM factory_admission_policies WHERE policy_digest=?1",
        [disabled["policy_digest"].as_str().unwrap()],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(stored_disable, disable_doc);
    assert_eq!(denials(), 6);
}

#[cfg(feature = "state-store")]
#[path = "../src/store/test_schema.rs"]
mod test_schema;

#[test]
#[cfg(feature="state-store")]
fn factory_status_cli_redacts_history_preserves_capacity_and_refuses_unknown_schema() {
    use herdr_projects::{domain::*,migration,store::SCHEMA};
    fn forbid(value:&serde_json::Value) {
        match value {
            serde_json::Value::Object(map)=>for (key,child) in map {
                assert!(!matches!(key.to_ascii_lowercase().as_str(),"env"|"environment"|"argv"|"secret"|"secrets"|"path"),"{key}");forbid(child);
            },
            serde_json::Value::Array(items)=>items.iter().for_each(forbid),
            serde_json::Value::String(text)=>assert!(!text.contains("SECRET_TOKEN_DO_NOT_LEAK"),"{text}"),
            _=>{},
        }
    }
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
    for action in ["new","pause"] {assert!(hp(home.path(),&["--root",r,action,"demo"]).status.success());}
    let project=root.join("demo");let plan=migration::inspect_with_config(&project,&home.path().join(".config/herdr-projects/config.toml")).unwrap();migration::apply(&project,&plan,true).unwrap();
    let mut db=migration::open_active(&project).unwrap();
        let mut mutations = vec![
            Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new("kept").unwrap(),
                    revision: 1,
                    state: TaskState::Running,
                    title: "SECRET_TOKEN_DO_NOT_LEAK".into(),
                    active_attempt: None,
                },
            },
            Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: AttemptId::new("attempt-kept").unwrap(),
                    task: TaskId::new("kept").unwrap(),
                    revision: 1,
                    // Losing contact is not termination evidence. Exercise the
                    // production status/capacity reader for that distinction.
                    state: AttemptState::Lost,
                    snapshot: None,
                    reservation: "slot-kept".into(),
                    termination_observed: false,
                },
            },
        ];
        for index in 0..40 {
            mutations.push(Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new(format!("retired-{index:02}")).unwrap(),
                    revision: 1,
                    state: TaskState::Succeeded,
                    title: format!("SECRET_TOKEN_DO_NOT_LEAK-{index}").into(),
                    active_attempt: None,
                },
            });
            mutations.push(Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: AttemptId::new(format!("attempt-retired-{index:02}")).unwrap(),
                    task: TaskId::new(format!("retired-{index:02}")).unwrap(),
                    revision: 1,
                    state: AttemptState::Completed,
                    snapshot: None,
                    reservation: format!("slot-retired-{index:02}"),
                    termination_observed: true,
                },
            });
        }
    db.commit(Commit{expected_head:db.current_head().unwrap(),mutations}).unwrap();
    let before=db.read_snapshot(None).unwrap();
    let active_rows=db.active_inventory_page_rows().unwrap();drop(db);
    std::fs::write(project.join(".state/admission-paused.json"),r#"{"reason":"disk_full","token":"SECRET_TOKEN_DO_NOT_LEAK"}"#).unwrap();
    let command=["--root",r,"factory","status","demo"];
    let output=hp(home.path(),&command);assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let status:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();forbid(&status);
    assert_eq!(status["prepared_dispatch"],true);assert_eq!(status["factory_admission"],"off");
    assert_eq!(status["admission_paused"],true);assert_eq!(status["pause_reason"],"disk_full");
    assert!(status["blockers"].as_array().unwrap().contains(&serde_json::json!("admission_paused")));
    assert!(!status["blockers"].as_array().unwrap().contains(&serde_json::json!("promotion_conflict")));
    assert_eq!(status["counters"]["retained_slots"],1);assert!(status["counters"]["rows_decoded"].is_null());
    assert_eq!(status["counters"]["active_inventory_page_rows"],active_rows);assert!(active_rows>0 && active_rows<41);
    assert_eq!(migration::open_active(&project).unwrap().read_snapshot(None).unwrap(),before);

    // An irrelevant malformed historical ID makes full snapshot decoding fail.
    // Status must still succeed through the bounded current-state reader.
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
    raw.execute("INSERT INTO tasks VALUES('retired/invalid',1,'succeeded','SECRET_TOKEN_DO_NOT_LEAK',NULL)",[]).unwrap();
    assert!(migration::open_active(&project).unwrap().read_snapshot(None).is_err());
    let output=hp(home.path(),&command);assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let cold:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();forbid(&cold);
    assert_eq!(cold["counters"]["retained_slots"],1);assert_eq!(cold["counters"]["active_inventory_page_rows"],active_rows);
    assert_eq!(raw.query_row("SELECT termination_observed FROM attempts WHERE id='attempt-kept'",[],|row|row.get::<_,bool>(0)).unwrap(),false);
    raw.execute("DELETE FROM tasks WHERE id='retired/invalid'",[]).unwrap();
    for (version,message) in [(SCHEMA+1,"store schema is newer than this binary"),(0,"unsupported_schema")] {
        raw.pragma_update(None,"user_version",version).unwrap();
        let output=hp(home.path(),&command);assert!(!output.status.success());
        let refused:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();forbid(&refused);
        assert_eq!(refused["schema"],version);assert_eq!(refused["error"],message);assert!(refused.get("counters").is_none());
        assert_eq!(raw.query_row("PRAGMA user_version",[],|row|row.get::<_,u32>(0)).unwrap(),version);
    }
    raw.pragma_update(None,"user_version",SCHEMA).unwrap();
    assert_eq!(migration::open_active(&project).unwrap().read_snapshot(None).unwrap(),before);
}

#[test]
#[cfg(feature="state-store")]
fn historical_store_cli_requires_explicit_upgrade_and_preserves_intent_and_capacity() {
    use herdr_projects::{domain::*,migration,store::SCHEMA};
    for version in 32..=42 {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
        for command in ["new","pause"] {assert!(hp(home.path(),&["--root",r,command,"demo"]).status.success());}
        let project=root.join("demo");let plan=migration::inspect_with_config(&project,&home.path().join(".config/herdr-projects/config.toml")).unwrap();migration::apply(&project,&plan,true).unwrap();
        let path=project.join(".state/state.db");
        let mut db=migration::open_active(&project).unwrap();
        db.commit(Commit{expected_head:db.current_head().unwrap(),mutations:vec![
            Mutation::Task{expected:None,next:Task{id:TaskId::new("kept").unwrap(),revision:1,state:TaskState::Running,title:"retained execution".into(),active_attempt:None}},
            Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new("attempt-kept").unwrap(),task:TaskId::new("kept").unwrap(),revision:1,state:AttemptState::Running,snapshot:None,reservation:"retained-capacity".into(),termination_observed:false}},
        ]}).unwrap();
        let proposal=br#"{"version":1,"contracts":[{"task_id":"planned","text":"retained planning intent","dependencies":[]}]}"#;
        let receipt=db.apply_plan_proposal(proposal,0,"retained-plan").unwrap();
        let before=db.read_snapshot(None).unwrap();drop(db);
        let raw=rusqlite::Connection::open(&path).unwrap();test_schema::historical(&raw,version).unwrap();
        let schema=||raw.query_row("PRAGMA user_version",[],|row|row.get::<_,u32>(0)).unwrap();
        let has_projection=||raw.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='plan_task_intents')",[],|row|row.get::<_,bool>(0)).unwrap();
        assert_eq!(schema(),version);assert!(!has_projection());
        let status=hp(home.path(),&["--root",r,"factory","status","demo"]);
        assert!(status.status.success(),"schema {version}: {}",String::from_utf8_lossy(&status.stderr));
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&status.stdout).unwrap()["schema"],version);
        assert_eq!(schema(),version);assert!(!has_projection());
        let inspect=hp(home.path(),&["--root",r,"plan","inspect","demo"]);assert!(!inspect.status.success());assert_eq!(schema(),version);
        let guard=herdr_projects::execution_guard::ProjectGuard::acquire(&project).unwrap();
        let upgrade=["--root",r,"migration","demo","upgrade-store"];
        assert!(!hp(home.path(),&upgrade).status.success());assert_eq!(schema(),version);assert!(!has_projection());drop(guard);
        let upgraded=hp(home.path(),&upgrade);assert!(upgraded.status.success(),"schema {version}: {}",String::from_utf8_lossy(&upgraded.stderr));
        assert_eq!(schema(),SCHEMA);assert!(has_projection());
        let after=migration::open_active(&project).unwrap().read_snapshot(None).unwrap();
        assert_eq!(after.head,before.head);assert_eq!(after.tasks,before.tasks);assert_eq!(after.attempts,before.attempts);assert_eq!(after.control,before.control);
        let retained:(Vec<u8>,String)=raw.query_row("SELECT payload,payload_digest FROM plan_proposals WHERE proposal_id=?1",[&receipt.proposal_id],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();assert_eq!(retained,(proposal.to_vec(),receipt.digest));
        let inspect=hp(home.path(),&["--root",r,"plan","inspect","demo"]);assert!(inspect.status.success());
        let page:serde_json::Value=serde_json::from_slice(&inspect.stdout).unwrap();assert_eq!(page["plan_revision"],1);assert_eq!(page["entries"][0]["text"],"retained planning intent");assert_eq!(page["entries"][0]["proposal_id"],receipt.proposal_id);
        assert!(hp(home.path(),&upgrade).status.success());
        assert_eq!(migration::open_active(&project).unwrap().read_snapshot(None).unwrap(),after);
        assert_eq!(raw.query_row("SELECT count(*) FROM pragma_foreign_key_check",[],|row|row.get::<_,u64>(0)).unwrap(),0);
    }
}
