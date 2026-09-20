use super::*;
use std::{fs,time::Duration};
use crate::{domain::*,runtime};
use sha2::{Digest,Sha256};

fn fixture()->(tempfile::TempDir,std::path::PathBuf) {
    let root=tempfile::tempdir().unwrap();let project=root.path().join("demo");
    for dir in [".state","threads","inbox"]{fs::create_dir_all(project.join(dir)).unwrap();}
    fs::write(project.join("PROJECT.md"),"+++\nname='Demo'\n+++\nInstructions\n").unwrap();
    fs::write(project.join("TASKS.md"),"# Tasks\n").unwrap();fs::write(project.join("MEMORY.md"),"").unwrap();
    fs::write(project.join(".state/project.json"),r#"{"status":"paused"}"#).unwrap();
    let config=root.path().join("owner.toml");
    fs::write(&config,format!("[authority]\nversion=1\nrevision=1\napproval_public_key='ssh-ed25519 {}'\n[safety.{:?}]\nroutine_commands=true\n","A".repeat(44),project.display().to_string())).unwrap();
    let migration=migration::inspect_with_config(&project,&config).unwrap();migration::apply(&project,&migration,true).unwrap();
    let snapshot=runtime::snapshot(&project).unwrap();runtime::set_state(&project,snapshot.head,snapshot.control.unwrap().revision,ProjectState::Active,&config).unwrap();
    for name in ["a-first","b-second"] {
        let script=project.join(format!("{name}.sh"));let bytes=b"touch MUST_NOT_EXECUTE\n";fs::write(&script,bytes).unwrap();
        let definition=RoutineDefinition{version:1,name:name.into(),revision:1,project_store:project.join(".state/state.db").canonicalize().unwrap().display().to_string(),authority:crate::authority::policy_reference(&project).unwrap(),config:migration::config_reference(&config).unwrap(),enabled:true,schedule:"every 1h".into(),timezone:"UTC".into(),start_unix_ms:jiff::Timestamp::now().as_millisecond()-1000,missed:MissedRunPolicy::CoalesceLatest,overlap:OverlapPolicy::Skip,script:script.display().to_string(),script_sha256:format!("{:x}",Sha256::digest(bytes)),cwd:project.display().to_string(),deadline_ms:1000,output_cap_bytes:4000};
        // Unit fixture enters the sealed prepared-definition service. Signature
        // ingress and real ticker execution have separate CLI coverage.
        let mut db=migration::open_active(&project).unwrap();let head=db.read_snapshot(None).unwrap().head;db.install_routine(&PreparedRoutine{definition},head).unwrap();
    }
    (root,project)
}
fn deadline()->Instant {Instant::now()+Duration::from_secs(5)}

#[test]
fn held_planning_rotates_after_withdrawn_script_and_only_records_intent() {
    let(_root,project)=fixture();let guard=ProjectGuard::acquire(&project).unwrap();let cancellation=Cancellation::default();
    fs::write(project.join("a-first.sh"),"changed after approval").unwrap();
    let mut last=None;
    for expected in ["a-first","b-second","a-first","b-second"] {
        let report=schedule_next_guarded(&project,last.as_deref(),&guard,&cancellation,deadline()).unwrap();
        assert!(report.active);assert_eq!(report.selected_name.as_deref(),Some(expected));assert_eq!(report.diagnostic.is_some(),expected=="a-first");last=report.selected_name;
    }
    let snapshot=runtime::snapshot(&project).unwrap();assert_eq!(snapshot.routine_occurrences.len(),1);assert_eq!(snapshot.deliveries[0].attempts,0);assert!(!project.join("MUST_NOT_EXECUTE").exists());
    assert!(ProjectGuard::acquire(&project).is_err());
}

#[test]
fn held_planning_refuses_wrong_guard_cancelled_expired_and_busy_record_lock() {
    let(root,project)=fixture();let other=root.path().join("other");fs::create_dir_all(other.join(".state")).unwrap();let wrong=ProjectGuard::acquire(&other).unwrap();let cancellation=Cancellation::default();
    assert!(schedule_next_guarded(&project,None,&wrong,&cancellation,deadline()).is_err());
    let guard=ProjectGuard::acquire(&project).unwrap();
    assert!(schedule_next_guarded(&project,None,&guard,&cancellation,Instant::now()).is_err());
    cancellation.cancel();assert!(schedule_next_guarded(&project,None,&guard,&cancellation,deadline()).is_err());
    let cancellation=Cancellation::default();let record=exclusive_file(&project.join(".state/lock")).unwrap();
    assert!(schedule_next_guarded(&project,None,&guard,&cancellation,deadline()).is_err());assert!(ProjectGuard::acquire(&project).is_err());drop(record);
    assert!(runtime::snapshot(&project).unwrap().routine_occurrences.is_empty());
    assert!(schedule_next_guarded(&project,None,&guard,&cancellation,deadline()).unwrap().active);
}

#[test]
fn held_planning_validates_without_record_lock_and_checks_cancellation_before_commit() {
    let(_root,project)=fixture();let guard=ProjectGuard::acquire(&project).unwrap();let cancellation=Cancellation::default();
    let result=plan(&project,None,&guard,&cancellation,deadline(),|definition|{
        let _record=exclusive_file(&project.join(".state/lock"))?;
        super::super::validate_current(definition)?;cancellation.cancel();Ok(())
    });
    assert!(result.unwrap_err().to_string().contains("cancelled"));assert!(runtime::snapshot(&project).unwrap().routine_occurrences.is_empty());assert!(ProjectGuard::acquire(&project).is_err());
}

#[test]
fn held_planning_never_rebases_a_stale_prepared_head() {
    let(_root,project)=fixture();let guard=ProjectGuard::acquire(&project).unwrap();let cancellation=Cancellation::default();
    let result=plan(&project,None,&guard,&cancellation,deadline(),|definition|{
        super::super::validate_current(definition)?;
        // Deliberate writer bypasses cooperative ownership to exercise the
        // transaction fence after validation, before planning commits.
        let mut db=migration::open_active(&project)?;let head=db.read_snapshot(None)?.head;
        db.commit(Commit{expected_head:head,mutations:vec![Mutation::Task{expected:None,next:Task{id:TaskId::new("interloper").unwrap(),revision:1,state:TaskState::Draft,title:"injected writer".into(),active_attempt:None}}]})?;Ok(())
    });
    assert!(result.is_err());let snapshot=runtime::snapshot(&project).unwrap();assert_eq!(snapshot.tasks.len(),1);assert!(snapshot.routine_occurrences.is_empty());
    assert!(schedule_next_guarded(&project,None,&guard,&cancellation,deadline()).unwrap().active);
}

#[test]
fn held_planning_known_inactive_state_returns_no_cursor_or_liveness() {
    let(root,project)=fixture();let snapshot=runtime::snapshot(&project).unwrap();runtime::set_state(&project,snapshot.head,snapshot.control.unwrap().revision,ProjectState::Paused,&root.path().join("owner.toml")).unwrap();
    let guard=ProjectGuard::acquire(&project).unwrap();let report=schedule_next_guarded(&project,Some("a-first"),&guard,&Cancellation::default(),deadline()).unwrap();assert!(!report.active);assert!(report.selected_name.is_none());assert!(report.diagnostic.is_none());assert!(runtime::snapshot(&project).unwrap().routine_occurrences.is_empty());
}
