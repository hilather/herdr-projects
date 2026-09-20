use super::*;
use tempfile::TempDir;
fn fixture()->(TempDir,std::path::PathBuf) {
    let temp=TempDir::new().unwrap(); let project=temp.path().join("demo");
    fs::create_dir(&project).unwrap(); fs::create_dir(project.join(".state")).unwrap(); fs::create_dir(project.join("threads")).unwrap(); fs::create_dir(project.join("inbox")).unwrap();
    fs::write(project.join("PROJECT.md"),"+++\nname = 'Demo'\n+++\nInstructions\n").unwrap();
    fs::write(project.join("TASKS.md"),"# Tasks\n- [ ] Pending\n- [x] Narrative only\nUnstructured notes stay intact\n").unwrap();
    fs::write(project.join("MEMORY.md"),"Unverified memory\n").unwrap();
    fs::write(project.join(".state/project.json"),r#"{"status":"paused"}"#).unwrap();
    fs::write(project.join(".state/inbox-seen.json"),r#"["previously-seen"]"#).unwrap();
    fs::write(project.join("threads/t-0001.toml"),"id='t-0001'\nstatus='resolved'\ntitle='legacy thread'\nfuture_field='preserve me'\n").unwrap();
    (temp,project)
}
fn prepare(project:&Path)->Journal {
    let plan=inspect(project).unwrap(); assert!(plan.blockers.is_empty(),"{:?}",plan.blockers);
    mkdir(&project.join(".state/migration")).unwrap();
    let journal=Journal{version:1,phase:Phase::Prepared,plan}; save(project,&journal).unwrap(); journal
}
#[test]
fn plan_apply_export_restore_preserve_exact_sources_and_memory_authority() {
    let (temp,project)=fixture(); let plan=inspect(&project).unwrap();
    let before=plan.sources.iter().map(|s|(s.path.clone(),read(&project.join(&s.path)).unwrap())).collect::<Vec<_>>();
    assert!(!project.join(".state/state.db").exists());
    assert_eq!(plan.tasks.len(),3); assert!(plan.blockers.is_empty());
    assert_eq!(apply(&project,&plan,true).unwrap().phase,Phase::Active);
    assert_eq!(recover(&project,true).unwrap().phase,Phase::Active);
    for (path,bytes) in &before { assert_eq!(read(&project.join(path)).unwrap(),*bytes); }
    let marker:Format=serde_json::from_slice(&read(&project.join(".state/format.json")).unwrap()).unwrap();
    assert_eq!(marker.memory,"legacy-markdown"); assert!(marker.reconciliation_required);
    let mut db=SqliteStore::open(&project.join(".state/state.db")).unwrap();
    let snapshot=db.read_snapshot(None).unwrap(); assert!(snapshot.tasks.iter().all(|t|t.state!=TaskState::Succeeded));
    assert!(!db.imported_sources().unwrap().iter().any(|s|s.path=="MEMORY.md"));
    let restore=temp.path().join("restored"); restore_backup(&project,&restore).unwrap();
    for (path,bytes) in &before { assert_eq!(read(&restore.join(path)).unwrap(),*bytes); }
    assert!(!restore.join(".state/format.json").exists());
    assert!(restore_backup(&project,&restore).is_err());
}
#[test]
fn stale_plan_unknown_fields_and_corruption_never_switch_authority() {
    for change in ["edit","bad-thread","bad-runtime","live","unknown-state"] {
        let (_temp,project)=fixture(); let plan=inspect(&project).unwrap();
        match change {
            "edit"=>fs::write(project.join("TASKS.md"),"edited").unwrap(),
            "bad-thread"=>fs::write(project.join("threads/t-0001.toml"),"id='t-0001'\nstatus='resolved'\npane_id=42").unwrap(),
            "bad-runtime"=>fs::write(project.join(".state/ticker.json"),r#"{"finalizations":42}"#).unwrap(),
            "live"=>fs::write(project.join(".state/coordinator.json"),r#"{"pane_id":"live-pane"}"#).unwrap(),
            _=>fs::write(project.join(".state/project.json"),r#"{"status":"future"}"#).unwrap(),
        }
        assert!(apply(&project,&plan,true).is_err());
        if change!="edit" { assert!(!inspect(&project).unwrap().blockers.is_empty()); }
        assert!(!project.join(".state/format.json").exists());
        assert!(!project.join(".state/state.db").exists());
    }
}
#[test]
fn maintenance_lock_and_confirmation_are_required() {
    let (_temp,project)=fixture(); let plan=inspect(&project).unwrap();
    assert!(apply(&project,&plan,false).is_err());
    let held=Maintenance::acquire(&project).unwrap(); assert!(apply(&project,&plan,true).is_err()); drop(held);
    apply(&project,&plan,true).unwrap(); assert!(abort(&project).is_err());
}
#[test]
fn recover_every_publication_boundary_and_empty_database() {
    for phase in [Phase::Prepared,Phase::Imported,Phase::Verified,Phase::CutoverPending] {
        for already_published in [false,true] {
            if already_published && phase!=Phase::CutoverPending { continue; }
            let (_temp,project)=fixture(); let mut journal=prepare(&project);
            backup(&project,&journal.plan).unwrap();
            let staged=project.join(".state/migration/state.db");
            let mut db=SqliteStore::create(&staged).unwrap();
            if phase!=Phase::Prepared { db.import_legacy(&journal.plan.digest,&expected_import(&project,&journal.plan).unwrap(),&journal.plan.tasks).unwrap(); }
            drop(db); journal.phase=phase.clone(); save(&project,&journal).unwrap();
            if already_published { fs::rename(staged,project.join(".state/state.db")).unwrap(); }
            fs::write(project.join(".state/migration/format.next"),b"partial marker").unwrap();
            assert_eq!(recover(&project,true).unwrap().phase,Phase::Active);
        }
    }
}
#[test]
fn interrupted_directory_reservation_retries_and_invalid_store_can_abort_losslessly() {
    let (_temp,project)=fixture(); let plan=inspect(&project).unwrap();
    mkdir(&project.join(".state/migration")).unwrap(); fs::write(project.join(".state/migration/journal.next"),b"partial").unwrap();
    apply(&project,&plan,true).unwrap();
    let (_temp,project)=fixture(); prepare(&project);
    fs::write(project.join(".state/migration/state.db"),b"interrupted").unwrap();
    assert!(recover(&project,true).is_err());
    let archive=abort(&project).unwrap();
    assert_eq!(fs::read(archive.join("state.db")).unwrap(),b"interrupted");
    assert!(!journal_path(&project).exists());
    let plan=inspect(&project).unwrap(); apply(&project,&plan,true).unwrap();
}
#[test]
fn altered_backup_marker_and_projection_remain_visible_and_preserved() {
    let (_temp,project)=fixture(); let plan=inspect(&project).unwrap(); apply(&project,&plan,true).unwrap();
    let mut db=SqliteStore::open(&project.join(".state/state.db")).unwrap();
    let dir=crate::projections::export(&project,&mut db).unwrap(); drop(db);
    fs::write(dir.join("TASKS.md"),b"manual edit").unwrap();
    assert!(recover(&project,true).is_err()); assert_eq!(fs::read(dir.join("TASKS.md")).unwrap(),b"manual edit");
    fs::write(project.join(".state/format.json"),b"bad marker").unwrap(); assert!(recover(&project,true).is_err());
    fs::write(project.join(".state/migration/backup/TASKS.md"),b"bad backup").unwrap();
    assert!(restore_backup(&project,&project.parent().unwrap().join("restore")).is_err());
}
#[test]
fn symlink_sources_and_controlled_ancestors_refuse_without_external_writes() {
    let (temp,project)=fixture();
    std::os::unix::fs::symlink("TASKS.md",project.join("linked")).unwrap(); assert!(inspect(&project).is_err()); fs::remove_file(project.join("linked")).unwrap();
    let journal=prepare(&project); let outside=temp.path().join("outside"); fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside,project.join(".state/migration/backup")).unwrap();
    assert!(backup(&project,&journal.plan).is_err()); assert_eq!(fs::read_dir(&outside).unwrap().count(),0);
    fs::remove_file(project.join(".state/migration/backup")).unwrap();
    fs::rename(project.join(".state/migration"),&outside.join("moved")).unwrap();
    std::os::unix::fs::symlink(outside.join("moved"),project.join(".state/migration")).unwrap(); assert!(recover(&project,true).is_err());
}
#[test]
fn schema_v1_upgrade_is_explicit_and_retains_existing_rows() {
    let temp=TempDir::new().unwrap(); let path=temp.path().join("state.db");
    let raw=rusqlite::Connection::open(&path).unwrap(); raw.execute_batch(include_str!("../../migrations/0001_project_store.sql")).unwrap();
    raw.execute("INSERT INTO tasks VALUES('legacy',1,'draft','Keep',NULL)",[]).unwrap(); drop(raw);
    let mut db=SqliteStore::open(&path).unwrap(); assert!(db.imported_sources().is_err());
    db.upgrade_v1().unwrap(); assert_eq!(db.read_snapshot(None).unwrap().tasks[0].title,"Keep");
    assert!(db.imported_sources().unwrap().is_empty());
}

#[test]
fn crash_child() {
    let Some(root)=std::env::var_os("HP_MIGRATION_CRASH_ROOT") else{return;};
    let project=std::path::PathBuf::from(root).join("demo"); let plan=inspect(&project).unwrap();
    apply(&project,&plan,true).unwrap();
}
#[test]
fn killed_process_recovers_at_each_durable_journal_phase() {
    use std::{process::{Command,Stdio},time::{Instant,Duration}};
    for phase in ["Prepared","Imported","Verified","CutoverPending","Active"] {
        let (temp,project)=fixture();
        let mut child=Command::new(std::env::current_exe().unwrap()).args(["--exact","migration::tests::crash_child","--nocapture"])
            .env("HP_MIGRATION_CRASH_ROOT",temp.path()).env("HP_MIGRATION_CRASH_PHASE",phase).stdout(Stdio::null()).stderr(Stdio::inherit()).spawn().unwrap();
        let deadline=Instant::now()+Duration::from_secs(10);
        while !temp.path().join("crash-ready").exists() {
            if Instant::now()>deadline||child.try_wait().unwrap().is_some(){let _=child.kill();let _=child.wait();panic!("child did not reach {phase}");}
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().unwrap();child.wait().unwrap();
        assert_eq!(recover(&project,true).unwrap().phase,Phase::Active);
    }
}
#[test]
fn malformed_known_settings_and_inbox_fields_block_cutover() {
    for (path,bytes) in [
        ("PROJECT.md","+++\nnudge='bad'\n+++\n"),
        ("PROJECT.md","+++\nmax_parallel_threads='bad'\n+++\n"),
        ("inbox/item.md","+++\nid='item'\nkind=42\n+++\n"),
        ("inbox/item.md","+++\nid='other'\nkind='routine'\n+++\n"),
    ] {
        let (_temp,project)=fixture(); fs::write(project.join(path),bytes).unwrap();
        let plan=inspect(&project).unwrap(); assert!(!plan.blockers.is_empty(),"{path}"); assert!(apply(&project,&plan,true).is_err());
    }
}
#[test]
fn partial_projection_temporary_is_replaced_without_touching_final_edits() {
    let (_temp,project)=fixture(); let plan=inspect(&project).unwrap(); apply(&project,&plan,true).unwrap();
    let mut db=SqliteStore::open(&project.join(".state/state.db")).unwrap(); let dir=crate::projections::export(&project,&mut db).unwrap();
    let before=fs::read(dir.join("TASKS.md")).unwrap(); fs::remove_file(dir.join("TASKS.md")).unwrap();
    let partial=dir.join(format!(".TASKS.md.{}.next",std::process::id())); fs::write(&partial,b"partial").unwrap();
    crate::projections::export(&project,&mut db).unwrap(); assert_eq!(fs::read(dir.join("TASKS.md")).unwrap(),before); assert!(!partial.exists());
    fs::write(dir.join("TASKS.md"),b"user edit").unwrap(); assert!(crate::projections::export(&project,&mut db).is_err());
    assert_eq!(fs::read(dir.join("TASKS.md")).unwrap(),b"user edit");
}
#[test]
fn pending_obligations_import_atomically_as_ambiguous_without_losing_retry_state() {
    use crate::operations::DeliveryState;
    let (_temp,project)=fixture();
    let pending=serde_json::json!({
        "pending_events":{"event-1":{"id":"event-1","kind":"needs-input","subject":"t-0001","summary":"s","body":"keep bytes","retry":{"attempts":2,"next_attempt":"2026-09-19T12:00:00Z","last_error":"lost acknowledgement","blocked":false}}},
        "finalizations":{"t-0001":{"operation_id":"final-1","fingerprint":"identity","pr":"https://example.invalid/pr/1","reason":"merged","retry":{"attempts":3,"blocked":true}}},
        "notification_retry":{"hash":"notification-hash","retry":{"attempts":1}}
    });
    let bytes=serde_json::to_vec(&pending).unwrap();fs::write(project.join(".state/ticker.json"),&bytes).unwrap();
    let plan=inspect(&project).unwrap();assert!(plan.blockers.is_empty(),"{:?}",plan.blockers);assert_eq!(plan.operations.len(),3);
    apply(&project,&plan,true).unwrap();let mut db=open_active(&project).unwrap();
    assert_eq!(db.read_snapshot(None).unwrap().operations,plan.operations);
    let deliveries=db.deliveries().unwrap();assert_eq!(deliveries.len(),3);assert!(deliveries.iter().all(|d|d.state==DeliveryState::Ambiguous));
    let mut attempts=deliveries.iter().map(|d|d.attempts).collect::<Vec<_>>();attempts.sort();assert_eq!(attempts,vec![1,2,3]);
    assert_eq!(db.import_operation_count().unwrap(),3);
    assert_eq!(db.imported_sources().unwrap().into_iter().find(|s|s.path==".state/ticker.json").unwrap().bytes,bytes);
    for delivery in deliveries {assert!(db.claim_operation(&delivery.operation,delivery.revision,"worker",2_000_000_000_000,1000).is_err());}
    drop(db);assert_eq!(recover(&project,true).unwrap().phase,Phase::Active);
}
#[test]
fn pending_thread_status_notice_imports_without_ticker_state_and_rejects_corruption() {
    let (_temp,project)=fixture();let path=project.join("threads/t-0001.toml");
    let mut value:toml::Value=toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let execution="a".repeat(64);
    let notice=crate::status_notice::StatusNotice{id:format!("status-t-0001-{execution}-1"),kind:"thread-state".into(),subject:"t-0001".into(),summary:"historical idle".into(),body:"original execution".into(),execution,sequence:1,previous_group:"working".into(),next_group:"idle".into()};
    value.as_table_mut().unwrap().insert("status_notice_sequence".into(),toml::Value::Integer(1));value.as_table_mut().unwrap().insert("pending_status_notice".into(),toml::Value::try_from(&notice).unwrap());
    fs::write(&path,toml::to_string(&value).unwrap()).unwrap();
    let plan=inspect(&project).unwrap();assert!(plan.blockers.is_empty(),"{:?}",plan.blockers);assert_eq!(plan.operations.len(),1);
    assert_eq!(plan.operations[0].payload,serde_json::to_value(&notice).unwrap());
    value["status_notice_sequence"]=toml::Value::Integer(2);fs::write(&path,toml::to_string(&value).unwrap()).unwrap();
    assert!(!inspect(&project).unwrap().blockers.is_empty());
    value["status_notice_sequence"]=toml::Value::Integer(1);fs::write(&path,toml::to_string(&value).unwrap()).unwrap();
    apply(&project,&plan,true).unwrap();let mut db=open_active(&project).unwrap();
    assert_eq!(db.read_snapshot(None).unwrap().operations,plan.operations);
    assert_eq!(db.deliveries().unwrap()[0].state,crate::operations::DeliveryState::Ambiguous);
}
#[test]
fn task_edits_and_projection_recovery_preserve_new_state_and_originals() {
    let (_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();
    let original=read(&project.join("TASKS.md")).unwrap();let before=crate::runtime::snapshot(&project).unwrap();
    let id=TaskId::new("new-task").unwrap();let head=crate::runtime::add_task(&project,id.clone(),"New work".into(),before.head).unwrap();
    assert!(crate::runtime::add_task(&project,TaskId::new("stale").unwrap(),"Stale".into(),before.head).is_err());
    assert!(crate::runtime::rename_task(&project,&id,"Wrong revision".into(),2,head).is_err());
    let head=crate::runtime::rename_task(&project,&id,"Changed title".into(),1,head).unwrap();
    recover(&project,true).unwrap();let current=crate::runtime::snapshot(&project).unwrap();assert_eq!(current.head,head);
    assert_eq!(current.tasks.iter().find(|t|t.id==id).unwrap().title,"Changed title");
    assert_eq!(read(&project.join("TASKS.md")).unwrap(),original);
    let context=crate::runtime::context(&project).unwrap();assert!(context.contains("Runtime owner: SQLite")&&context.contains("Changed title")&&context.contains("Unverified memory"));
    assert!(project.join(format!(".state/projections/schema-16-revision-{head}/TASKS.md")).is_file());
}
#[test]
fn corrupt_retry_identity_types_and_dates_block_import() {
    for retry in [serde_json::json!({"attempts":"bad"}),serde_json::json!({"next_attempt":"not a date"}),serde_json::json!({"blocked":4})] {
        let (_temp,project)=fixture();fs::write(project.join(".state/ticker.json"),serde_json::to_vec(&serde_json::json!({"pending_events":{"event":{"id":"event","kind":"x","subject":"s","summary":"s","body":"b","retry":retry}}})).unwrap()).unwrap();
        let plan=inspect(&project).unwrap();assert!(!plan.blockers.is_empty());assert!(apply(&project,&plan,true).is_err());
    }
}
#[test]
fn published_v2_store_upgrades_explicitly_without_losing_migration_identity() {
    let (_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();
    // Reproduce the previous released schema: no operation delivery table or
    // receipt count existed. It retains the same published ownership protocol.
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
    raw.execute_batch("DROP TABLE routine_occurrences; DROP TABLE routine_cursors; DROP TABLE routine_revisions; DROP TABLE budget_policies; DROP TABLE approval_uses; DROP TABLE approval_revocations; DROP TABLE approval_grants; DROP TRIGGER operation_delivery_monotonic; DROP TABLE attempt_cancellations; DROP TABLE attempt_inputs; DROP TABLE task_dependencies; DROP TABLE task_queue; DROP TABLE scheduler_policy; DROP TABLE runtime_ownership; DROP TABLE project_control; DROP TABLE runtime_observations; DROP TABLE runtime_bindings; DROP TABLE inbox_items; DROP TRIGGER operation_delivery_insert; DROP TABLE operation_delivery; ALTER TABLE migration_receipt DROP COLUMN operation_count; UPDATE store_meta SET schema_version=2; PRAGMA user_version=2;").unwrap();drop(raw);
    let before=crate::runtime::snapshot(&project).unwrap();
    upgrade_active(&project).unwrap();let mut db=open_active(&project).unwrap();
    let after=db.read_snapshot(None).unwrap();assert_eq!(after.tasks,before.tasks);assert_eq!(after.head,before.head);assert_eq!(after.schema_version,16);assert!(db.deliveries().unwrap().is_empty());assert_eq!(db.import_operation_count().unwrap(),0);
    drop(db);recover(&project,true).unwrap();
}
#[test]
fn failed_operation_import_rolls_back_sources_tasks_delivery_and_receipt() {
    let (_temp,project)=fixture();
    fs::write(project.join(".state/ticker.json"),br#"{"pending_events":{"one":{"id":"one","kind":"notice","subject":"s","summary":"s","body":"b"}}}"#).unwrap();
    let journal=prepare(&project);backup(&project,&journal.plan).unwrap();
    let mut operations=journal.plan.operations.clone();operations.push(operations[0].clone());
    let mut db=SqliteStore::create(&project.join(".state/migration/state.db")).unwrap();
    assert!(db.import_legacy_with_operations(&journal.plan.digest,&expected_import(&project,&journal.plan).unwrap(),&journal.plan.tasks,&operations).is_err());
    let snapshot=db.read_snapshot(None).unwrap();assert_eq!(snapshot.head,0);assert!(snapshot.tasks.is_empty()&&snapshot.operations.is_empty()&&snapshot.deliveries.is_empty());
    assert!(db.imported_sources().unwrap().is_empty());assert!(!db.has_import().unwrap());
}
fn inbox_source(project:&Path,id:&str,body:&str,done:bool) {
    let dir=project.join(if done{"inbox/done"}else{"inbox"});fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{id}.md")),format!("+++\nid='{id}'\nkind='notice'\nsubject='subject'\ncreated='original-time'\nsummary='summary'\n+++\n\n{body}\n")).unwrap();
}
fn pending_inbox(project:&Path,events:serde_json::Value) {
    fs::write(project.join(".state/ticker.json"),serde_json::to_vec(&serde_json::json!({"pending_events":events})).unwrap()).unwrap();
}
fn event(id:&str,body:&str)->serde_json::Value {serde_json::json!({"id":id,"kind":"notice","subject":"subject","summary":"summary","body":body,"retry":{"attempts":2}})}
#[test]
fn canonical_inbox_drain_preserves_done_seen_and_does_not_replay_other_effects() {
    use crate::operations::DeliveryState;
    let (_temp,project)=fixture();inbox_source(&project,"existing","  indented",true);
    fs::write(project.join(".state/inbox-seen.json"),br#"["existing"]"#).unwrap();
    pending_inbox(&project,serde_json::json!({"existing":event("existing","  indented"),"new":event("new","new body")}));
    let legacy=read(&project.join("inbox/done/existing.md")).unwrap();let plan=inspect(&project).unwrap();assert!(plan.blockers.is_empty());apply(&project,&plan,true).unwrap();
    let before=crate::runtime::snapshot(&project).unwrap();assert!(before.inbox[0].done&&before.inbox[0].seen);assert_eq!(before.inbox[0].content.body,"  indented");
    assert_eq!(crate::runtime::drain_inbox(&project,before.head).unwrap(),2);
    let after=crate::runtime::snapshot(&project).unwrap();assert_eq!(after.inbox.len(),2);assert!(after.deliveries.iter().all(|d|d.state==DeliveryState::Confirmed));
    assert!(after.inbox.iter().find(|i|i.content.id=="existing").unwrap().done);
    assert_eq!(crate::runtime::drain_inbox(&project,after.head).unwrap(),0);
    let(text,head,unseen)=crate::runtime::context_snapshot(&project).unwrap();assert!(text.contains("new body"));assert_eq!(unseen,vec!["new"]);
    crate::runtime::update_inbox(&project,head,&unseen,false).unwrap();let after=crate::runtime::snapshot(&project).unwrap();
    crate::runtime::update_inbox(&project,after.head,&["new".into()],true).unwrap();
    assert_eq!(read(&project.join("inbox/done/existing.md")).unwrap(),legacy);assert!(!project.join("inbox/new.md").exists());
    recover(&project,true).unwrap();
}
#[test]
fn inbox_indentation_conflicts_roll_back_deliveries_and_preserve_bytes() {
    let (_temp,project)=fixture();inbox_source(&project,"existing","  indented  ",false);
    pending_inbox(&project,serde_json::json!({"existing":event("existing","indented"),"new":event("new","new")}));
    let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();let before=crate::runtime::snapshot(&project).unwrap();
    assert_eq!(before.inbox[0].content.body,"  indented  ");assert!(crate::runtime::drain_inbox(&project,before.head).is_err());
    assert_eq!(crate::runtime::snapshot(&project).unwrap(),before);
}
#[test]
fn schema3_inbox_upgrade_reads_provenance_not_changed_legacy_files() {
    let (_temp,project)=fixture();inbox_source(&project,"one","original",false);
    let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();raw.execute_batch("DROP TABLE routine_occurrences; DROP TABLE routine_cursors; DROP TABLE routine_revisions; DROP TABLE budget_policies; DROP TABLE approval_uses; DROP TABLE approval_revocations; DROP TABLE approval_grants; DROP TRIGGER operation_delivery_monotonic; DROP TABLE attempt_cancellations; DROP TABLE attempt_inputs; DROP TABLE task_dependencies; DROP TABLE task_queue; DROP TABLE scheduler_policy; DROP TABLE runtime_ownership; DROP TABLE project_control; DROP TABLE runtime_observations; DROP TABLE runtime_bindings; DROP TABLE inbox_items; UPDATE store_meta SET schema_version=3; PRAGMA user_version=3;").unwrap();drop(raw);
    inbox_source(&project,"one","edited legacy file",false);upgrade_active(&project).unwrap();
    assert_eq!(crate::runtime::snapshot(&project).unwrap().inbox[0].content.body,"original");
}
#[test]
fn old_schema_projection_does_not_conflict_after_inbox_upgrade() {
    let (_temp,project)=fixture();inbox_source(&project,"one","canonical content",false);let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();raw.execute_batch("DROP TABLE routine_occurrences; DROP TABLE routine_cursors; DROP TABLE routine_revisions; DROP TABLE budget_policies; DROP TABLE approval_uses; DROP TABLE approval_revocations; DROP TABLE approval_grants; DROP TRIGGER operation_delivery_monotonic; DROP TABLE attempt_cancellations; DROP TABLE attempt_inputs; DROP TABLE task_dependencies; DROP TABLE task_queue; DROP TABLE scheduler_policy; DROP TABLE runtime_ownership; DROP TABLE project_control; DROP TABLE runtime_observations; DROP TABLE runtime_bindings; DROP TABLE inbox_items; UPDATE store_meta SET schema_version=3; PRAGMA user_version=3;").unwrap();drop(raw);
    let old=crate::projections::export(&project,&mut open_active(&project).unwrap()).unwrap();
    let old_view=read(&old.join("inbox.json")).unwrap();upgrade_active(&project).unwrap();let new=crate::projections::export(&project,&mut open_active(&project).unwrap()).unwrap();
    assert_ne!(old,new);assert_eq!(read(&old.join("inbox.json")).unwrap(),old_view);assert!(String::from_utf8(read(&new.join("inbox.json")).unwrap()).unwrap().contains("canonical content"));
}
#[test]
fn claimed_inbox_intent_is_not_stolen_by_internal_drain() {
    let (_temp,project)=fixture();pending_inbox(&project,serde_json::json!({"one":event("one","body")}));let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();
    let mut db=open_active(&project).unwrap();let d=db.deliveries().unwrap().remove(0);
    let retry=db.observe_operation(&d.operation,d.revision,"observer",crate::operations::Outcome::Retryable{no_effect_evidence:"fixture absent".into()},1000).unwrap();
    let claim=db.claim_operation(&d.operation,retry.revision,"other-worker",retry.next_due_ms,1000).unwrap();let head=db.read_snapshot(None).unwrap().head;
    assert_eq!(db.drain_inbox(head,retry.next_due_ms+1).unwrap(),0);assert_eq!(db.deliveries().unwrap()[0].revision,claim.revision);assert!(db.read_snapshot(None).unwrap().inbox.is_empty());
}

#[test]
fn external_config_changes_block_apply_and_all_unpublished_recovery_phases() {
    for phase in [Phase::Prepared,Phase::Imported,Phase::Verified,Phase::CutoverPending] {
        let (temp,project)=fixture();
        let config=temp.path().join("config.toml");
        let original=b"private_value='original-secret'\n";
        fs::write(&config,original).unwrap();
        let plan=inspect_with_config(&project,&config).unwrap();
        assert_eq!(plan.version,2);
        assert!(!serde_json::to_string(&plan).unwrap().contains("original-secret"));
        fs::write(&config,b"private_value='changed-secret'\n").unwrap();
        assert!(apply(&project,&plan,true).is_err());
        assert!(!project.join(".state/migration").exists());
        fs::write(&config,original).unwrap();
        mkdir(&project.join(".state/migration")).unwrap();
        backup(&project,&plan).unwrap();
        if phase!=Phase::Prepared {
            let mut db=SqliteStore::create(&project.join(".state/migration/state.db")).unwrap();
            db.import_legacy_with_operations(&plan.digest,&expected_import(&project,&plan).unwrap(),&plan.tasks,&plan.operations).unwrap();
        }
        save(&project,&Journal{version:1,phase,plan}).unwrap();
        fs::remove_file(&config).unwrap();
        assert!(recover(&project,true).is_err());
        assert!(!project.join(".state/format.json").exists());
        fs::write(&config,original).unwrap();
        assert_eq!(recover(&project,true).unwrap().phase,Phase::Active);
        // Once ownership is published, later user config edits cannot strand the store.
        fs::write(&config,b"private_value='later'\n").unwrap();
        assert_eq!(recover(&project,true).unwrap().phase,Phase::Active);
    }
}

#[test]
fn external_config_absence_path_and_unsafe_files_are_bound() {
    let (temp,project)=fixture();let config=temp.path().join("missing.toml");
    let plan=inspect_with_config(&project,&config).unwrap();
    assert_eq!(plan.config.as_ref().unwrap().digest,None);
    assert!(require_config_path(&plan,&temp.path().join("other.toml")).is_err());
    assert!(require_config_path(&inspect(&project).unwrap(),&config).is_err());
    fs::write(&config,b"root='/new-root'\n").unwrap();
    assert!(apply(&project,&plan,true).is_err());
    fs::remove_file(&config).unwrap();
    std::os::unix::fs::symlink(temp.path().join("absent-target"),&config).unwrap();
    assert!(inspect_with_config(&project,&config).is_err());
    fs::remove_file(&config).unwrap();
    fs::write(&config,b"secret-not-valid-toml").unwrap();
    let error=inspect_with_config(&project,&config).unwrap_err().to_string();
    assert!(!error.contains("secret-not-valid-toml"));
    fs::remove_file(&config).unwrap();
    apply(&project,&plan,true).unwrap();
    assert_eq!(open_active(&project).unwrap().import_receipt().unwrap().0,plan.digest);
}

fn receipt_fixture(matching:bool)->(TempDir,std::path::PathBuf) {
    let(temp,project)=fixture();
    let thread="id='t-0001'\nstatus='resolved'\npr='https://example.invalid/pr/1'\nresolved_reason='merged'\nlast_finalization='final-1'\n";
    fs::write(project.join("threads/t-0001.toml"),thread).unwrap();
    let fingerprint=crate::operations::receipts::legacy_execution_fingerprint(&toml::from_str(thread).unwrap()).unwrap();
    fs::write(project.join(".state/ticker.json"),serde_json::to_vec(&serde_json::json!({
        "nudged":if matching{"notification-hash"}else{"other-hash"},
        "notification_retry":{"hash":"notification-hash"},
        "finalizations":{"t-0001":{"operation_id":"final-1","fingerprint":if matching{fingerprint}else{"old-identity".into()},"pr":"https://example.invalid/pr/1","reason":"merged"}}
    })).unwrap()).unwrap();
    let plan=inspect(&project).unwrap();assert!(plan.blockers.is_empty());apply(&project,&plan,true).unwrap();(temp,project)
}
#[test]
fn imported_receipts_confirm_atomically_without_live_effects_or_task_promotion() {
    let(_temp,project)=receipt_fixture(true);let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();
    let preview=db.observe_imported_receipts(before.head,100,false).unwrap();assert_eq!(preview.confirmed,0);assert_eq!(preview.head,before.head);assert!(preview.observations.iter().all(|o|o.receipt.is_some()));assert_eq!(db.read_snapshot(None).unwrap(),before);
    // Only DB provenance counts; later edits to legacy files cannot forge evidence.
    fs::write(project.join(".state/ticker.json"),"{}").unwrap();fs::write(project.join("threads/t-0001.toml"),"status='open'").unwrap();
    let report=db.observe_imported_receipts(before.head,100,true).unwrap();assert_eq!(report.confirmed,2);assert_eq!(report.head,before.head+2);
    let after=db.read_snapshot(None).unwrap();assert_eq!(after.tasks,before.tasks);assert_eq!(after.operations,before.operations);assert!(after.deliveries.iter().all(|d|d.state==crate::operations::DeliveryState::Confirmed));
    assert!(db.observe_imported_receipts(before.head,101,true).is_err());assert_eq!(db.observe_imported_receipts(after.head,101,true).unwrap().confirmed,0);
    drop(db);assert_eq!(recover(&project,true).unwrap().phase,Phase::Active);
}
#[test]
fn missing_receipts_and_changed_task_or_claim_never_authorize_confirmation() {
    let(_temp,project)=receipt_fixture(false);let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();
    assert_eq!(db.observe_imported_receipts(before.head,100,true).unwrap().confirmed,0);assert_eq!(db.read_snapshot(None).unwrap(),before);drop(db);
    let(_temp,project)=receipt_fixture(true);let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();
    let mut task=before.tasks.iter().find(|t|t.id.as_str()=="legacy-t-0001").unwrap().clone();task.revision+=1;
    db.commit(crate::domain::Commit{expected_head:before.head,mutations:vec![crate::domain::Mutation::Task{expected:Some(1),next:task}]}).unwrap();
    let notify=before.operations.iter().find(|o|o.kind=="legacy.notify").unwrap();
    let pending=db.observe_operation(&notify.id,1,"fixture",crate::operations::Outcome::Retryable{no_effect_evidence:"test fixture".into()},0).unwrap();
    db.claim_operation(&notify.id,pending.revision,"fixture",pending.next_due_ms,1000).unwrap();
    let before=db.read_snapshot(None).unwrap();assert_eq!(db.observe_imported_receipts(before.head,100,true).unwrap().confirmed,0);assert_eq!(db.read_snapshot(None).unwrap(),before);
}
#[test]
fn corrupt_receipt_provenance_rolls_back_all_confirmations() {
    let(_temp,project)=receipt_fixture(true);let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();raw.execute("UPDATE legacy_sources SET bytes=x'00' WHERE path='threads/t-0001.toml'",[]).unwrap();
    assert!(db.observe_imported_receipts(before.head,100,true).is_err());assert!(db.read_snapshot(None).is_err());
    raw.execute("UPDATE legacy_sources SET bytes=?1 WHERE path='threads/t-0001.toml'",[read(&project.join("threads/t-0001.toml")).unwrap()]).unwrap();
    assert_eq!(db.read_snapshot(None).unwrap(),before);
}

#[test]
fn imported_receipts_cannot_confirm_new_lookalike_operations() {
    use crate::domain::{Commit,Mutation,OperationId};
    for advance in [false,true] {
        let(_temp,project)=receipt_fixture(true);let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();
        let mut lookalike=before.operations.iter().find(|o|o.kind=="legacy.notify").unwrap().clone();
        lookalike.id=OperationId::new("new-lookalike").unwrap();lookalike.idempotency_key="different-intent".into();
        let mut mutations=Vec::new();
        if advance {
            let mut task=before.tasks.iter().find(|t|Some(&t.id)==lookalike.task.as_ref()).unwrap().clone();task.revision+=1;lookalike.expected_revision=task.revision;
            mutations.push(Mutation::Task{expected:Some(1),next:task});
        }
        mutations.push(Mutation::Enqueue(lookalike.clone()));db.commit(Commit{expected_head:before.head,mutations}).unwrap();
        db.claim_operation(&lookalike.id,1,"test-worker",100,1).unwrap();db.expire_claims(101).unwrap();
        let head=db.read_snapshot(None).unwrap().head;let report=db.observe_imported_receipts(head,102,true).unwrap();
        let entry=report.observations.iter().find(|o|o.operation==lookalike.id).unwrap();assert!(entry.receipt.is_none());assert!(entry.blocked.is_some());
        assert_eq!(db.deliveries().unwrap().iter().find(|d|d.operation==lookalike.id).unwrap().state,crate::operations::DeliveryState::Ambiguous);
    }
}

#[test]
fn runtime_bindings_preserve_recorded_identity_without_granting_ownership() {
    let(_temp,project)=fixture();
    fs::write(project.join(".state/coordinator.json"),br#"{"socket":"/recorded/session.sock","workspace_id":"w-1","cwd":"/coordinator","repo":42,"status":7}"#).unwrap();
    fs::write(project.join("threads/t-0001.toml"),"id='t-0001'\nstatus='resolved'\nrepo='/repo'\nbranch='topic'\nworktree_path='/worktree'\nagent='agent'\ncwd='/worktree'\nfuture_key='preserved'\nsocket=42\n").unwrap();
    let plan=inspect(&project).unwrap();assert!(plan.blockers.is_empty());apply(&project,&plan,true).unwrap();
    let snapshot=crate::runtime::snapshot(&project).unwrap();assert_eq!(snapshot.runtime_bindings.len(),2);
    let binding=snapshot.runtime_bindings.iter().find(|b|b.id=="thread:t-0001").unwrap();
    assert_eq!(binding.task.as_ref().unwrap().as_str(),"legacy-t-0001");assert_eq!(binding.identity.socket,"/recorded/session.sock");assert_eq!(binding.identity.worktree_path,"/worktree");assert_eq!(binding.identity.branch,"topic");assert!(binding.identity.execution_fingerprint.is_some());assert!(binding.session_source_digest.is_some());
    assert!(snapshot.runtime_bindings.iter().all(|b|b.verification==crate::domain::RuntimeVerification::Unverified));
    fs::write(project.join("threads/t-0001.toml"),"id='changed'\n").unwrap();fs::write(project.join(".state/coordinator.json"),br#"{"socket":"/wrong.sock"}"#).unwrap();
    assert_eq!(crate::runtime::snapshot(&project).unwrap().runtime_bindings,snapshot.runtime_bindings);
    let export=crate::projections::export(&project,&mut open_active(&project).unwrap()).unwrap();let view:serde_json::Value=serde_json::from_slice(&read(&export.join("runtime.json")).unwrap()).unwrap();assert_eq!(view["bindings"].as_array().unwrap().len(),2);
    assert!(String::from_utf8(open_active(&project).unwrap().imported_sources().unwrap().into_iter().find(|s|s.kind=="thread").unwrap().bytes).unwrap().contains("future_key"));
}
#[test]
fn runtime_upgrade_uses_provenance_preserves_edits_and_previous_exports() {
    let(_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();raw.execute_batch("DROP TABLE routine_occurrences; DROP TABLE routine_cursors; DROP TABLE routine_revisions; DROP TABLE budget_policies; DROP TABLE approval_uses; DROP TABLE approval_revocations; DROP TABLE approval_grants; DROP TRIGGER operation_delivery_monotonic; DROP TABLE attempt_cancellations; DROP TABLE attempt_inputs; DROP TABLE task_dependencies; DROP TABLE task_queue; DROP TABLE scheduler_policy; DROP TABLE runtime_ownership; DROP TABLE project_control; DROP TABLE runtime_observations; DROP TABLE runtime_bindings; UPDATE store_meta SET schema_version=4; PRAGMA user_version=4;").unwrap();drop(raw);
    let before=crate::runtime::snapshot(&project).unwrap();assert!(before.runtime_bindings.is_empty());
    let old=crate::projections::export(&project,&mut open_active(&project).unwrap()).unwrap();let old_bytes=read(&old.join("runtime.json")).unwrap();
    crate::runtime::rename_task(&project,&TaskId::new("legacy-t-0001").unwrap(),"new title".into(),1,before.head).unwrap();
    fs::write(project.join("threads/t-0001.toml"),"bad legacy bytes").unwrap();let before=crate::runtime::snapshot(&project).unwrap();
    upgrade_active(&project).unwrap();let after=crate::runtime::snapshot(&project).unwrap();assert_eq!(after.head,before.head);assert_eq!(after.tasks,before.tasks);assert_eq!(after.runtime_bindings.len(),1);assert_eq!(after.runtime_bindings[0].task.as_ref().unwrap().as_str(),"legacy-t-0001");assert!(after.runtime_bindings[0].identity.socket.is_empty());
    upgrade_active(&project).unwrap();assert_eq!(crate::runtime::snapshot(&project).unwrap(),after);
    let new=crate::projections::export(&project,&mut open_active(&project).unwrap()).unwrap();assert_ne!(old,new);assert_eq!(read(&old.join("runtime.json")).unwrap(),old_bytes);
    recover(&project,true).unwrap();
}
#[test]
fn corrupt_or_missing_runtime_binding_is_visible() {
    for missing in [false,true] {
        let(_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();
        let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
        if missing {raw.execute("DELETE FROM runtime_bindings",[]).unwrap();}else{raw.execute("UPDATE runtime_bindings SET payload='{}'",[]).unwrap();}
        assert!(crate::runtime::snapshot(&project).is_err());
    }
}

#[test]
fn runtime_snapshot_checks_source_bytes_and_missing_links_on_open_connection() {
    for damage in ["thread-bytes","session-bytes","missing-source"] {
        let(_temp,project)=fixture();fs::write(project.join(".state/coordinator.json"),br#"{"socket":"/recorded.sock"}"#).unwrap();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();
        let mut db=open_active(&project).unwrap();let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();raw.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        match damage {
            "thread-bytes"=>{raw.execute("UPDATE legacy_sources SET bytes=x'00' WHERE kind='thread'",[]).unwrap();},
            "session-bytes"=>{raw.execute("UPDATE legacy_sources SET bytes=x'00' WHERE path='.state/coordinator.json'",[]).unwrap();},
            _=>{raw.execute("DELETE FROM legacy_sources WHERE kind='thread'",[]).unwrap();},
        }
        assert!(db.read_snapshot(None).is_err(),"{damage}");
    }
}

#[test]
fn runtime_upgrade_failure_rolls_back_schema_and_can_be_retried() {
    let(_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();raw.execute_batch("DROP TABLE routine_occurrences; DROP TABLE routine_cursors; DROP TABLE routine_revisions; DROP TABLE budget_policies; DROP TABLE approval_uses; DROP TABLE approval_revocations; DROP TABLE approval_grants; DROP TRIGGER operation_delivery_monotonic; DROP TABLE attempt_cancellations; DROP TABLE attempt_inputs; DROP TABLE task_dependencies; DROP TABLE task_queue; DROP TABLE scheduler_policy; DROP TABLE runtime_ownership; DROP TABLE project_control; DROP TABLE runtime_observations; DROP TABLE runtime_bindings; UPDATE store_meta SET schema_version=4; PRAGMA user_version=4; UPDATE legacy_sources SET bytes=x'00' WHERE kind='thread';").unwrap();
    assert!(upgrade_active(&project).is_err());
    assert_eq!(raw.query_row("PRAGMA user_version",[],|r|r.get::<_,u32>(0)).unwrap(),4);
    assert_eq!(raw.query_row("SELECT count(*) FROM sqlite_master WHERE name='runtime_bindings'",[],|r|r.get::<_,u32>(0)).unwrap(),0);
    raw.execute("UPDATE legacy_sources SET bytes=?1 WHERE path='threads/t-0001.toml'",[read(&project.join("threads/t-0001.toml")).unwrap()]).unwrap();
    upgrade_active(&project).unwrap();assert_eq!(crate::runtime::snapshot(&project).unwrap().runtime_bindings.len(),1);
}

#[test]
fn reconciliation_observations_are_atomic_fenced_and_never_release_capacity() {
    use crate::reconcile::{RuntimeObservation,ResourceState};
    let(_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();
    assert!(db.record_observations(before.head,&[]).is_err());
    let binding=&before.runtime_bindings[0];
    let observation=RuntimeObservation{binding:binding.id.clone(),binding_revision:binding.revision,task_revision:Some(1),observed_unix_ms:100,pane:ResourceState::Absent,worktree:ResourceState::Unknown,agent_present:false,collector:"herdr-git-v1".into(),config_digest:None,diagnostic:"no authority granted".into(),..Default::default()};
    let mut stale=observation.clone();stale.binding_revision+=1;
    assert!(db.record_observations(before.head,&[stale]).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
    let head=db.record_observations(before.head,&[observation.clone()]).unwrap();let after=db.read_snapshot(None).unwrap();assert_eq!(after.tasks,before.tasks);assert_eq!(after.attempts,before.attempts);assert_eq!(after.runtime_bindings,before.runtime_bindings);
    assert!(db.record_observations(before.head,&[observation.clone()]).is_err());assert_eq!(db.record_observations(head,&[observation.clone()]).unwrap(),head);
    let mut older=observation.clone();older.observed_unix_ms=99;assert!(db.record_observations(head,&[older]).is_err());
    let mut task=after.tasks.iter().find(|t|Some(&t.id)==binding.task.as_ref()).unwrap().clone();task.revision+=1;
    let changed=db.commit(crate::domain::Commit{expected_head:head,mutations:vec![crate::domain::Mutation::Task{expected:Some(1),next:task}]}).unwrap();
    assert!(db.record_observations(changed,&[observation]).is_err());
}

fn new_route()->crate::domain::RuntimeRoute {
    crate::domain::RuntimeRoute{socket:"/new/session.sock".into(),workspace_id:"w".into(),tab_id:"t".into(),pane_id:"p".into(),cwd:"/worktree".into(),..Default::default()}
}
#[test]
fn runtime_rebind_invalidates_observations_claims_and_original_task_revision() {
    use crate::{operations::Outcome,reconcile::{RuntimeObservation,ResourceState}};
    let(_temp,project)=receipt_fixture(true);let original=read(&project.join("threads/t-0001.toml")).unwrap();let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();
    let op=before.operations.iter().find(|o|o.kind=="legacy.finalize").unwrap();
    let pending=db.observe_operation(&op.id,1,"test",Outcome::Retryable{no_effect_evidence:"test fixture".into()},0).unwrap();let claim=db.claim_operation(&op.id,pending.revision,"test",pending.next_due_ms,1000).unwrap();
    let head=db.read_snapshot(None).unwrap().head;
    let observation=RuntimeObservation{binding:"thread:t-0001".into(),binding_revision:1,task_revision:Some(1),observed_unix_ms:100,pane:ResourceState::Unrecorded,worktree:ResourceState::Unrecorded,agent_present:false,collector:"herdr-git-v1".into(),config_digest:None,diagnostic:"fixture".into(),..Default::default()};
    let head=db.record_observations(head,&[observation.clone()]).unwrap();
    let change=db.rebind_runtime("thread:t-0001",1,head,&new_route()).unwrap();assert_eq!(change.binding.revision,2);assert_eq!(change.task_revision,Some(2));assert!(change.binding.identity.execution_fingerprint.is_none());assert_eq!(change.binding.verification,crate::domain::RuntimeVerification::Unverified);
    let snapshot=db.read_snapshot(None).unwrap();assert!(snapshot.observations.is_empty());assert_eq!(snapshot.operations,before.operations);
    assert!(db.finish_operation(&claim,Outcome::Confirmed{observed_identity:"stale".into()},pending.next_due_ms+1).is_err());assert!(db.record_observations(change.head,&[observation]).is_err());
    assert!(db.rebind_runtime("thread:t-0001",1,head,&new_route()).is_err());assert_eq!(db.rebind_runtime("thread:t-0001",2,change.head,&new_route()).unwrap().head,change.head);
    drop(db);recover(&project,true).unwrap();assert_eq!(crate::runtime::snapshot(&project).unwrap().runtime_bindings[0],change.binding);assert_eq!(read(&project.join("threads/t-0001.toml")).unwrap(),original);
}
#[test]
fn runtime_rebind_refuses_active_attempt_and_duplicate_pane_without_partial_mutation() {
    use crate::domain::{Commit,Mutation,Attempt,AttemptId,AttemptState};
    let(_temp,project)=fixture();fs::write(project.join("threads/t-2.toml"),"id='t-2'\nstatus='resolved'\n").unwrap();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();
    let changed=db.rebind_runtime("thread:t-2",1,before.head,&new_route()).unwrap();let before=db.read_snapshot(None).unwrap();assert!(db.rebind_runtime("thread:t-0001",1,changed.head,&new_route()).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
    let mut task=before.tasks.iter().find(|t|t.id.as_str()=="legacy-t-0001").unwrap().clone();let attempt=Attempt{id:AttemptId::new("lost").unwrap(),task:task.id.clone(),revision:1,state:AttemptState::Lost,snapshot:None,reservation:"must-retain".into(),termination_observed:false};task.revision+=1;task.active_attempt=Some(attempt.id.clone());
    let head=db.commit(Commit{expected_head:before.head,mutations:vec![Mutation::Task{expected:Some(1),next:task},Mutation::Attempt{expected:None,next:attempt}]}).unwrap();let before=db.read_snapshot(None).unwrap();
    assert!(db.rebind_runtime("thread:t-0001",1,head,&Default::default()).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);assert!(before.attempts[0].retains_capacity());
}

#[test]
fn runtime_rebind_refuses_unselected_lost_attempt_that_retains_capacity() {
    use crate::domain::{Commit,Mutation,Attempt,AttemptId,AttemptState};
    let(_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();
    let task=before.tasks.iter().find(|t|t.id.as_str()=="legacy-t-0001").unwrap();assert!(task.active_attempt.is_none());
    let attempt=Attempt{id:AttemptId::new("unselected-lost").unwrap(),task:task.id.clone(),revision:1,state:AttemptState::Lost,snapshot:None,reservation:"retained".into(),termination_observed:false};
    let head=db.commit(Commit{expected_head:before.head,mutations:vec![Mutation::Attempt{expected:None,next:attempt}]}).unwrap();let before=db.read_snapshot(None).unwrap();
    assert!(db.rebind_runtime("thread:t-0001",1,head,&new_route()).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
}

fn unrecorded_observations(snapshot:&crate::domain::Snapshot,now:i64)->Vec<crate::reconcile::RuntimeObservation> {
    snapshot.runtime_bindings.iter().map(|b|crate::reconcile::RuntimeObservation{binding:b.id.clone(),binding_revision:b.revision,task_revision:b.task.as_ref().and_then(|id|snapshot.tasks.iter().find(|t|&t.id==id).map(|t|t.revision)),observed_unix_ms:now,pane:crate::reconcile::ResourceState::Unrecorded,worktree:crate::reconcile::ResourceState::Unrecorded,agent_present:false,collector:"herdr-git-v1".into(),config_digest:None,diagnostic:"fixture recorded no resources".into(),..Default::default()}).collect()
}
#[test]
fn controller_resume_requires_fresh_evidence_and_pause_fences_epoch() {
    use crate::domain::ProjectState;
    let(_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();assert_eq!(before.control.as_ref().unwrap().state,ProjectState::Paused);
    assert!(db.set_project_state(before.head,1,ProjectState::Active,100,None).is_err());
    let head=db.record_observations(before.head,&unrecorded_observations(&before,100)).unwrap();
    assert!(db.set_project_state(head,1,ProjectState::Active,30_101,None).is_err());
    assert!(db.set_project_state(head,1,ProjectState::Active,101,Some(&"a".repeat(64))).is_err());
    let active=db.set_project_state(head,1,ProjectState::Active,101,None).unwrap();assert!(!active.control.reconciliation_required);db.validate_control_epoch(active.control.epoch,None).unwrap();assert!(db.validate_control_epoch(active.control.epoch,Some(&"b".repeat(64))).is_err());
    publish_control_marker(&project,&db).unwrap();let marker:Format=serde_json::from_slice(&read(&project.join(".state/format.json")).unwrap()).unwrap();assert!(!marker.reconciliation_required);
    let paused=db.set_project_state(active.head,active.control.revision,ProjectState::Paused,102,None).unwrap();assert!(paused.control.epoch>active.control.epoch);assert!(db.validate_control_epoch(active.control.epoch,None).is_err());publish_control_marker(&project,&db).unwrap();
    assert_eq!(read(&project.join(".state/project.json")).unwrap(),br#"{"status":"paused"}"#);
}
#[test]
fn interrupted_controller_marker_publication_recovers_forward() {
    use crate::domain::ProjectState;
    let(_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();let head=db.record_observations(before.head,&unrecorded_observations(&before,100)).unwrap();let active=db.set_project_state(head,1,ProjectState::Active,101,None).unwrap();drop(db);
    // Simulates process death after DB commit and before derived marker rename.
    assert!(open_active(&project).is_err());recover(&project,true).unwrap();let mut db=open_active(&project).unwrap();assert_eq!(db.read_snapshot(None).unwrap().control,Some(active.control.clone()));
    let route=new_route();let rebound=db.rebind_runtime("thread:t-0001",1,active.head,&route).unwrap();assert!(open_active(&project).is_err());drop(db);recover(&project,true).unwrap();let after=crate::runtime::snapshot(&project).unwrap();assert_eq!(after.head,rebound.head);assert_eq!(after.control.unwrap().state,ProjectState::Paused);
}
#[test]
fn controller_preserves_archived_state_and_retained_attempts_block_admission() {
    use crate::domain::{ProjectState,Attempt,AttemptId,AttemptState,Commit,Mutation};
    let(_temp,project)=fixture();fs::write(project.join(".state/project.json"),br#"{"status":"archived"}"#).unwrap();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();assert_eq!(before.control.as_ref().unwrap().state,ProjectState::Archived);assert!(db.set_project_state(before.head,1,ProjectState::Active,100,None).is_err());
    let paused=db.set_project_state(before.head,1,ProjectState::Paused,100,None).unwrap();let attempt=Attempt{id:AttemptId::new("lost-unselected").unwrap(),task:TaskId::new("legacy-t-0001").unwrap(),revision:1,state:AttemptState::Lost,snapshot:None,reservation:"held".into(),termination_observed:false};let head=db.commit(Commit{expected_head:paused.head,mutations:vec![Mutation::Attempt{expected:None,next:attempt}]}).unwrap();let snapshot=db.read_snapshot(None).unwrap();let head=db.record_observations(head,&unrecorded_observations(&snapshot,100)).unwrap();assert!(db.set_project_state(head,paused.control.revision,ProjectState::Active,101,None).is_err());assert!(db.read_snapshot(None).unwrap().attempts[0].retains_capacity());
}
#[test]
fn retiring_an_ambiguous_intent_does_not_claim_absence_or_release_resources() {
    let(_temp,project)=receipt_fixture(false);let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();let op=&before.operations[0];
    let result=db.retire_operation(&op.id,1,before.head,"superseded by operator",100).unwrap();assert_eq!(result.state,crate::operations::DeliveryState::PermanentFailure);assert!(serde_json::to_string(&result.last_outcome).unwrap().contains("effect remains possible"));assert!(db.retire_operation(&op.id,1,before.head,"stale",101).is_err());let after=db.read_snapshot(None).unwrap();assert_eq!(after.tasks,before.tasks);assert_eq!(after.attempts,before.attempts);assert_eq!(after.operations,before.operations);
}

#[test]
fn canonical_runtime_creation_preserves_provenance_and_fences_task_and_control() {
    use crate::domain::{ProjectState,TaskId};
    let(_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();let task=TaskId::new("new-task").unwrap();
    let before=crate::runtime::snapshot(&project).unwrap();crate::runtime::add_task(&project,task.clone(),"new task".into(),before.head).unwrap();
    let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();let sources=db.imported_sources().unwrap();
    let head=db.record_observations(before.head,&unrecorded_observations(&before,100)).unwrap();let active=db.set_project_state(head,1,ProjectState::Active,101,None).unwrap();publish_control_marker(&project,&db).unwrap();
    assert!(db.create_runtime(Some(&task),Some(2),active.head,&Default::default()).is_err());
    let created=crate::runtime::create_binding(&project,Some(&task),Some(1),active.head,&new_route()).unwrap();assert_eq!(created.binding.id,"task:new-task");assert!(created.binding.source_path.is_none());assert!(created.binding.source_digest.is_none());assert_eq!(created.task_revision,Some(2));
    let after=crate::runtime::snapshot(&project).unwrap();assert_eq!(after.control.as_ref().unwrap().state,ProjectState::Paused);assert!(after.control.as_ref().unwrap().epoch>active.control.epoch);assert_eq!(db.imported_sources().unwrap(),sources);assert_eq!(after.runtime_bindings.iter().find(|b|b.source_path.is_some()).unwrap(),&before.runtime_bindings[0]);
    assert!(db.validate_control_epoch(active.control.epoch,None).is_err());assert!(db.create_runtime(Some(&task),Some(2),after.head,&Default::default()).is_err());assert_eq!(db.read_snapshot(None).unwrap(),after);
    assert!(db.create_runtime(None,None,after.head,&new_route()).is_err());
    let coordinator=crate::runtime::create_binding(&project,None,None,after.head,&Default::default()).unwrap();assert_eq!(coordinator.binding.id,"coordinator");
    drop(db);recover(&project,true).unwrap();assert_eq!(crate::runtime::snapshot(&project).unwrap().runtime_bindings.len(),3);assert!(!project.join("threads/new-task.toml").exists());assert!(!project.join(".state/coordinator.json").exists());
}

#[test]
fn canonical_runtime_creation_refuses_unselected_retained_attempts() {
    use crate::domain::{TaskId,Attempt,AttemptId,AttemptState,Commit,Mutation};
    let(_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();let task=TaskId::new("new-task").unwrap();
    let before=crate::runtime::snapshot(&project).unwrap();let head=crate::runtime::add_task(&project,task.clone(),"new".into(),before.head).unwrap();let mut db=open_active(&project).unwrap();
    let attempt=Attempt{id:AttemptId::new("lost").unwrap(),task:task.clone(),revision:1,state:AttemptState::Lost,snapshot:None,reservation:"held".into(),termination_observed:false};let head=db.commit(Commit{expected_head:head,mutations:vec![Mutation::Attempt{expected:None,next:attempt}]}).unwrap();let before=db.read_snapshot(None).unwrap();
    assert!(db.create_runtime(Some(&task),Some(1),head,&Default::default()).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
    assert!(db.create_runtime(None,Some(1),head,&Default::default()).is_err());assert!(db.create_runtime(Some(&task),None,head,&Default::default()).is_err());
}

#[test]
fn schema8_upgrade_preserves_observation_foreign_keys_and_import_hashes() {
    let(_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();db.record_observations(before.head,&unrecorded_observations(&before,100)).unwrap();let before=db.read_snapshot(None).unwrap();drop(db);
    // Reconstruct schema7's NOT NULL parent with its live observation child.
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();raw.execute_batch("PRAGMA foreign_keys=OFF; BEGIN; DROP TABLE routine_occurrences; DROP TABLE routine_cursors; DROP TABLE routine_revisions; DROP TABLE budget_policies; DROP TABLE approval_uses; DROP TABLE approval_revocations; DROP TABLE approval_grants; DROP TRIGGER operation_delivery_monotonic; DROP TABLE attempt_cancellations; DROP TABLE attempt_inputs; DROP TABLE task_dependencies; DROP TABLE task_queue; DROP TABLE scheduler_policy; DROP TABLE runtime_ownership; CREATE TABLE old_bindings (id TEXT PRIMARY KEY NOT NULL,task_id TEXT REFERENCES tasks(id),revision INTEGER NOT NULL CHECK(revision>0),source_path TEXT NOT NULL UNIQUE REFERENCES legacy_sources(path),payload TEXT NOT NULL CHECK(json_valid(payload)),payload_hash TEXT NOT NULL CHECK(length(payload_hash)=64)) STRICT; INSERT INTO old_bindings SELECT * FROM runtime_bindings; DROP TABLE runtime_bindings; ALTER TABLE old_bindings RENAME TO runtime_bindings; UPDATE store_meta SET schema_version=7; PRAGMA user_version=7; COMMIT;").unwrap();drop(raw);
    let mut db=open_active(&project).unwrap();let mut expected=before.clone();expected.schema_version=7;expected.scheduler=None;assert_eq!(db.read_snapshot(None).unwrap(),expected);assert!(db.create_runtime(None,None,before.head,&Default::default()).is_err());drop(db);
    upgrade_active(&project).unwrap();let mut db=open_active(&project).unwrap();assert_eq!(db.read_snapshot(None).unwrap(),before);db.integrity_check().unwrap();db.upgrade_v1().unwrap();assert_eq!(db.read_snapshot(None).unwrap(),before);
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();raw.execute_batch("PRAGMA foreign_keys=ON;").unwrap();assert!(raw.execute("DELETE FROM runtime_bindings",[]).is_err());
}

#[test]
fn schema9_queue_upgrade_preserves_old_exports_and_starts_with_closed_capacity() {
    let(_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();raw.execute_batch("DROP TABLE routine_occurrences; DROP TABLE routine_cursors; DROP TABLE routine_revisions; DROP TABLE budget_policies; DROP TABLE approval_uses; DROP TABLE approval_revocations; DROP TABLE approval_grants; DROP TRIGGER operation_delivery_monotonic; DROP TABLE attempt_cancellations; DROP TABLE attempt_inputs; DROP TABLE task_dependencies; DROP TABLE task_queue; DROP TABLE scheduler_policy; UPDATE store_meta SET schema_version=9; PRAGMA user_version=9;").unwrap();drop(raw);let mut db=open_active(&project).unwrap();let before=db.read_snapshot(None).unwrap();assert!(before.scheduler.is_none());let old=crate::projections::export(&project,&mut db).unwrap();let bytes=fs::read(old.join("runtime.json")).unwrap();drop(db);
    upgrade_active(&project).unwrap();let mut db=open_active(&project).unwrap();let after=db.read_snapshot(None).unwrap();assert_eq!(after.tasks,before.tasks);assert_eq!(after.attempts,before.attempts);assert_eq!(after.head,before.head);assert_eq!(after.scheduler.unwrap().policy.max_active_workers,0);crate::projections::export(&project,&mut db).unwrap();assert_eq!(fs::read(old.join("runtime.json")).unwrap(),bytes);
}

#[test]
fn failed_record_lock_releases_project_and_root_ownership() {
    let(_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();
    let record=fs::OpenOptions::new().write(true).open(project.join(".state/lock")).unwrap();record.try_lock().unwrap();
    assert!(runtime_mutation(&project).is_err());
    assert!(crate::execution_guard::ProjectGuard::acquire(&project).is_ok());
    assert!(crate::execution_guard::RootGuard::exclusive(project.parent().unwrap()).is_ok());
    drop(record);assert!(runtime_mutation(&project).is_ok());
}

#[test]
fn guarded_controller_commit_fences_identity_head_and_record_lock() {
    use crate::{execution_guard::ProjectGuard,runtime,reconcile::ObservationBatch};
    let(temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();
    let other=temp.path().join("other");fs::create_dir_all(other.join(".state")).unwrap();
    let before=runtime::snapshot(&project).unwrap();
    let batch=ObservationBatch{expected_head:before.head,observations:unrecorded_observations(&before,100),dispatch_allowed:false,recorded_head:None};
    let wrong=ProjectGuard::acquire(&other).unwrap();
    assert!(runtime::record_controller_observations_guarded(&project,&batch,&wrong).is_err());
    assert_eq!(runtime::snapshot(&project).unwrap(),before);drop(wrong);
    let guard=ProjectGuard::acquire(&project).unwrap();
    let record=fs::OpenOptions::new().write(true).open(project.join(".state/lock")).unwrap();record.try_lock().unwrap();
    assert!(runtime::record_controller_observations_guarded(&project,&batch,&guard).is_err());
    assert_eq!(runtime::snapshot(&project).unwrap(),before);drop(record);
    assert!(runtime::add_task(&project,crate::domain::TaskId::new("blocked").unwrap(),"blocked".into(),before.head).is_err());
    assert!(ProjectGuard::acquire(&other).is_ok());
    runtime::record_controller_observations_guarded(&project,&batch,&guard).unwrap();
    let after=runtime::snapshot(&project).unwrap();assert!(after.head>before.head);
    assert_eq!(after.tasks,before.tasks);assert_eq!(after.attempts,before.attempts);
    assert!(runtime::record_controller_observations_guarded(&project,&batch,&guard).is_err());
    assert_eq!(runtime::snapshot(&project).unwrap(),after);
    assert!(ProjectGuard::acquire(&project).is_err());drop(guard);
    assert!(ProjectGuard::acquire(&project).is_ok());
}

#[test]
fn guarded_controller_commit_expires_claim_without_replay() {
    use crate::{execution_guard::ProjectGuard,runtime,reconcile::ObservationBatch,operations::{Outcome,DeliveryState}};
    let(_temp,project)=receipt_fixture(true);let mut db=open_active(&project).unwrap();
    let before=db.read_snapshot(None).unwrap();let op=before.operations.iter().find(|o|o.kind=="legacy.finalize").unwrap();
    let pending=db.observe_operation(&op.id,1,"fixture",Outcome::Retryable{no_effect_evidence:"fixture".into()},0).unwrap();
    db.claim_operation(&op.id,pending.revision,"fixture",pending.next_due_ms,1000).unwrap();drop(db);
    let guard=ProjectGuard::acquire(&project).unwrap();let before=runtime::snapshot(&project).unwrap();
    let batch=ObservationBatch{expected_head:before.head,observations:unrecorded_observations(&before,100),dispatch_allowed:false,recorded_head:None};
    runtime::record_controller_observations_guarded(&project,&batch,&guard).unwrap();
    let after=runtime::snapshot(&project).unwrap();let delivery=after.deliveries.iter().find(|d|d.operation==op.id).unwrap();
    assert_eq!(delivery.state,DeliveryState::Ambiguous);assert_eq!(delivery.attempts,1);
    assert_eq!(after.operations,before.operations);assert_eq!(after.tasks,before.tasks);assert_eq!(after.attempts,before.attempts);
    assert!(ProjectGuard::acquire(&project).is_err());
}

#[test]
fn guarded_controller_publication_failure_requires_forward_recovery() {
    use crate::{execution_guard::ProjectGuard,runtime,reconcile::{ObservationBatch,ResourceState},domain::{ProjectState,ResourceIdentity,AgentIdentity}};
    let(_temp,project)=fixture();let plan=inspect(&project).unwrap();apply(&project,&plan,true).unwrap();
    let mut db=open_active(&project).unwrap();let head=db.read_snapshot(None).unwrap().head;
    let created=db.create_runtime(None,None,head,&new_route()).unwrap();
    let snapshot=db.read_snapshot(None).unwrap();let mut observations=unrecorded_observations(&snapshot,100);
    let observed=observations.iter_mut().find(|o|o.binding==created.binding.id).unwrap();
    observed.collector="herdr-git-v2".into();observed.pane=ResourceState::Present;observed.agent_present=true;
    observed.session_identity=Some(ResourceIdentity{device:1,inode:2,born_secs:0,born_nanos:0});observed.agent_identity=Some(AgentIdentity{kind:"test".into(),name:"fixture".into()});
    let head=db.record_observations(snapshot.head,&observations).unwrap();
    let adopted=db.adopt_runtime(&created.binding.id,created.binding.revision,head,101,None).unwrap();
    let snapshot=db.read_snapshot(None).unwrap();let head=db.record_observations(adopted.head,&observations).unwrap();
    db.set_project_state(head,snapshot.control.unwrap().revision,ProjectState::Active,102,None).unwrap();publish_control_marker(&project,&db).unwrap();drop(db);
    let guard=ProjectGuard::acquire(&project).unwrap();let before=runtime::snapshot(&project).unwrap();
    let mut observations=unrecorded_observations(&before,103);let absent=observations.iter_mut().find(|o|o.binding==created.binding.id).unwrap();absent.pane=ResourceState::Absent;
    // A hostile temporary path forces failure after the authoritative commit.
    let temporary=project.join(".state/migration/control-format.next");fs::create_dir(&temporary).unwrap();
    let batch=ObservationBatch{expected_head:before.head,observations,dispatch_allowed:false,recorded_head:None};
    assert!(runtime::record_controller_observations_guarded(&project,&batch,&guard).is_err());
    assert!(runtime::snapshot(&project).is_err());assert!(ProjectGuard::acquire(&project).is_err());drop(guard);
    fs::remove_dir(&temporary).unwrap();recover(&project,true).unwrap();
    let after=runtime::snapshot(&project).unwrap();assert!(after.head>before.head);assert!(after.control.unwrap().reconciliation_required);
    assert_eq!(after.tasks,before.tasks);assert_eq!(after.attempts,before.attempts);assert_eq!(after.ownership,before.ownership);
}

#[test]
fn pending_copy_warning_imports_without_ticker_and_invalid_receipt_blocks() {
    let (_temp,project)=fixture();let path=project.join("threads/t-0001.toml");
    let mut value:toml::Value=toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let receipt=crate::copy_receipt::CopyReceipt{sequence:1,execution:"a".repeat(64),report_hash:"b".repeat(64),notes:vec!["library skipped".into()]};
    let notice=crate::copy_receipt::CopyNotice::new("t-0001",receipt.clone()).unwrap();
    value.as_table_mut().unwrap().insert("copy_receipt".into(),toml::Value::try_from(&receipt).unwrap());
    value.as_table_mut().unwrap().insert("pending_copy_notice".into(),toml::Value::try_from(&notice).unwrap());
    fs::write(&path,toml::to_string(&value).unwrap()).unwrap();
    let plan=inspect(&project).unwrap();assert!(plan.blockers.is_empty(),"{:?}",plan.blockers);assert_eq!(plan.operations.len(),1);
    assert_eq!(plan.operations[0].payload,serde_json::to_value(&notice).unwrap());
    value["copy_receipt"]["sequence"]=toml::Value::Integer(2);fs::write(&path,toml::to_string(&value).unwrap()).unwrap();
    assert!(!inspect(&project).unwrap().blockers.is_empty());
    value["copy_receipt"]["sequence"]=toml::Value::Integer(1);fs::write(&path,toml::to_string(&value).unwrap()).unwrap();
    apply(&project,&plan,true).unwrap();let mut db=open_active(&project).unwrap();
    assert_eq!(db.read_snapshot(None).unwrap().operations,plan.operations);
}

#[test]
fn pending_review_notice_imports_without_ticker_and_rejects_identity_corruption() {
    let (_temp,project)=fixture();let path=project.join("threads/t-0001.toml");
    let mut value:toml::Value=toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let execution="a".repeat(64);
    let mut notice=crate::review_notice::ReviewNotice{id:format!("review-t-0001-{execution}-1"),kind:"thread-state".into(),subject:"t-0001".into(),summary:"historical report".into(),body:String::new(),execution,report_hash:"b".repeat(64),sequence:1,copy_receipt:None};
    notice.body=notice.expected_body();
    value.as_table_mut().unwrap().insert("review_notice_sequence".into(),toml::Value::Integer(1));
    value.as_table_mut().unwrap().insert("pending_review_notice".into(),toml::Value::try_from(&notice).unwrap());
    fs::write(&path,toml::to_string(&value).unwrap()).unwrap();
    let plan=inspect(&project).unwrap();assert!(plan.blockers.is_empty(),"{:?}",plan.blockers);assert_eq!(plan.operations.len(),1);
    assert_eq!(plan.operations[0].payload,serde_json::to_value(&notice).unwrap());
    value["review_notice_sequence"]=toml::Value::Integer(2);fs::write(&path,toml::to_string(&value).unwrap()).unwrap();
    assert!(!inspect(&project).unwrap().blockers.is_empty());
    value["review_notice_sequence"]=toml::Value::Integer(1);fs::write(&path,toml::to_string(&value).unwrap()).unwrap();
    apply(&project,&plan,true).unwrap();let mut db=open_active(&project).unwrap();
    assert_eq!(db.read_snapshot(None).unwrap().operations,plan.operations);
}

#[test]
fn pending_live_projection_refuses_migration_until_exact_stage_recovery() {
    let (_temp,project)=fixture();let path=project.join("threads/t-0001.toml");
    let mut value:toml::Value=toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    value.as_table_mut().unwrap().insert("pending_live_copy".into(),toml::Value::Table(Default::default()));
    fs::write(&path,toml::to_string(&value).unwrap()).unwrap();
    assert!(validate_thread(&value).unwrap_err().to_string().contains("pending live projection"));
    let plan=inspect(&project).unwrap();assert!(plan.blockers.iter().any(|b|b.starts_with("threads/t-0001.toml:")),"{:?}",plan.blockers);
}


#[test]
fn final_copy_obligations_and_invalid_counters_block_migration() {
    let (_temp,project)=fixture();let path=project.join("threads/t-0001.toml");
    let original:toml::Value=toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    for (field,value) in [("pending_final_copy",toml::Value::Table(Default::default())),("pending_final_notice",toml::Value::Table(Default::default())),("final_copy_sequence",toml::Value::Integer(-1))] {
        let mut changed=original.clone();changed.as_table_mut().unwrap().insert(field.into(),value);fs::write(&path,toml::to_string(&changed).unwrap()).unwrap();
        assert!(validate_thread(&changed).is_err());assert!(inspect(&project).unwrap().blockers.iter().any(|b|b.starts_with("threads/t-0001.toml:")));
    }
}

#[test]
fn pending_brief_claims_refuse_migration_but_confirmed_history_is_validated() {
    let mut value:toml::Value=toml::from_str("id = 't-0001'\nstatus = 'open'\nprompt_sequence = 1\n").unwrap();
    let claim=crate::prompt_claim::Claim{sequence:1,execution:"a".repeat(64),prompt:"brief".into(),phase:crate::prompt_claim::Phase::Pending,error:String::new(),notified:false};
    value.as_table_mut().unwrap().insert("prompt_claim".into(),toml::Value::try_from(&claim).unwrap());assert!(validate_thread(&value).is_err());
    let confirmed=crate::prompt_claim::Claim{phase:crate::prompt_claim::Phase::Confirmed,notified:true,..claim};value["prompt_claim"]=toml::Value::try_from(&confirmed).unwrap();validate_thread(&value).unwrap();
    value["prompt_claim"]["execution"]=toml::Value::String("invalid".into());assert!(validate_thread(&value).is_err());
}

#[test]
fn pending_or_unresolved_launch_claim_requires_reconciliation_before_import() {
    let mut value:toml::Value=toml::from_str("id='t-0001'\nstatus='open'\nlaunch_sequence=1\n").unwrap();
    let mut claim=crate::launch_claim::Claim{sequence:1,generation:0,execution:"a".repeat(64),arguments_digest:"b".repeat(64),route_digest:"c".repeat(64),terminal:"terminal".into(),phase:crate::launch_claim::Phase::Pending,error:String::new(),notified:false};
    value.as_table_mut().unwrap().insert("launch_claim".into(),toml::Value::try_from(&claim).unwrap());assert!(validate_thread(&value).is_err());
    claim.phase=crate::launch_claim::Phase::Confirmed;claim.notified=true;value["launch_claim"]=toml::Value::try_from(&claim).unwrap();assert!(validate_thread(&value).is_ok());
    claim.phase=crate::launch_claim::Phase::Uncertain;claim.error="lost acknowledgement".into();value["launch_claim"]=toml::Value::try_from(&claim).unwrap();assert!(validate_thread(&value).is_err());value["status"]="resolved".into();assert!(validate_thread(&value).is_ok());
}

#[test]
fn coordinator_prime_import_requires_confirmed_delivery_and_valid_counters() {
    let mut value=serde_json::json!({"prime_request":1,"prime_sequence":1,"prime_claim":null});
    assert!(validate_runtime(".state/coordinator.json",&value).is_ok());
    let mut claim=crate::coordinator_prime::Claim{request:1,delivery:crate::prompt_claim::Claim{sequence:1,execution:"a".repeat(64),prompt:"prime".into(),phase:crate::prompt_claim::Phase::Pending,error:String::new(),notified:false}};
    value["prime_claim"]=serde_json::to_value(&claim).unwrap();assert!(validate_runtime(".state/coordinator.json",&value).is_err());
    claim.delivery.phase=crate::prompt_claim::Phase::Uncertain;claim.delivery.error="lost reply".into();claim.delivery.notified=true;
    value["prime_claim"]=serde_json::to_value(&claim).unwrap();assert!(validate_runtime(".state/coordinator.json",&value).is_err());
    claim.delivery.phase=crate::prompt_claim::Phase::Confirmed;claim.delivery.error.clear();value["prime_claim"]=serde_json::to_value(&claim).unwrap();assert!(validate_runtime(".state/coordinator.json",&value).is_ok());
    value["prime_request"]=0.into();assert!(validate_runtime(".state/coordinator.json",&value).is_err());
}
#[test]
fn coordinator_start_import_requires_confirmed_delivery_and_retains_uncertain_history() {
    let mut value=serde_json::json!({"prime_request":1,"launch_sequence":1,"launch_claim":null});assert!(validate_runtime(".state/coordinator.json",&value).is_ok());
    let mut claim=crate::launch_claim::Claim{sequence:1,generation:1,execution:"a".repeat(64),arguments_digest:"b".repeat(64),route_digest:"c".repeat(64),terminal:"terminal".into(),phase:crate::launch_claim::Phase::Pending,error:String::new(),notified:false};
    value["launch_claim"]=serde_json::to_value(&claim).unwrap();assert!(validate_runtime(".state/coordinator.json",&value).is_err());
    claim.phase=crate::launch_claim::Phase::Confirmed;claim.notified=true;value["launch_claim"]=serde_json::to_value(&claim).unwrap();assert!(validate_runtime(".state/coordinator.json",&value).is_ok());
    claim.phase=crate::launch_claim::Phase::Uncertain;claim.error="lost reply".into();value["launch_claim"]=serde_json::to_value(&claim).unwrap();assert!(validate_runtime(".state/coordinator.json",&value).is_err());
    value["prime_request"]=2.into();assert!(validate_runtime(".state/coordinator.json",&value).is_err());
}
#[test]
fn notification_claims_and_suppression_require_reconciliation_before_import() {
    use crate::notification_claim::{Batch,Claim,Mode,Phase};
    let mut claim=Claim{sequence:1,batch:Some(Batch::new(vec!["item-a".into()]).unwrap()),mode:Mode::Toast,authority:"a".repeat(64),payload:"fixture".into(),phase:Phase::Pending,retry_of:None,error:String::new()};
    let mut value=serde_json::json!({"notification_sequence":1,"notification_claim":claim,"notification_suppressed":[]});assert!(validate_runtime(".state/ticker.json",&value).is_err());
    claim.phase=Phase::Confirmed;value["notification_claim"]=serde_json::to_value(&claim).unwrap();assert!(validate_runtime(".state/ticker.json",&value).is_ok());
    for phase in [Phase::Uncertain,Phase::NotShown] {claim.phase=phase;claim.error="pending delivery".into();value["notification_claim"]=serde_json::to_value(&claim).unwrap();assert!(validate_runtime(".state/ticker.json",&value).is_err());}
    claim.phase=Phase::Suppressed;value["notification_claim"]=serde_json::to_value(&claim).unwrap();assert!(validate_runtime(".state/ticker.json",&value).is_ok());value["notification_suppressed"]=serde_json::json!(["item-a"]);assert!(validate_runtime(".state/ticker.json",&value).is_err());
}
