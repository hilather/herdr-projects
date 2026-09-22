use super::*;
use crate::{launch_preparation::{self, LaunchDraft, LaunchSelection}, store::SqliteStore};
use std::process::Command;

struct LaunchFixture {
    f: Fixture,
    key: PathBuf,
    _socket: std::os::unix::net::UnixListener,
    selection: LaunchSelection,
}

fn git(path: &Path, args: &[&str]) -> String {
    let output=Command::new("/usr/bin/git").current_dir(path).env_clear()
        .env("PATH","/usr/bin:/bin").env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null")
        .args(["-c","core.hooksPath=/dev/null","-c","user.name=fixture","-c","user.email=fixture@example.invalid"])
        .args(args).output().unwrap();
    assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().into()
}

impl LaunchFixture {
    fn new(repository: bool) -> Self { Self::with_cwd(repository, "") }
    fn with_cwd(repository: bool, suffix: &str) -> Self {
        let f=Fixture::new();
        let socket=std::os::unix::net::UnixListener::bind(f._root.path().join("native.sock")).unwrap();
        fs::write(&f.herdr,format!(r#"#!/usr/bin/python3
import sys,json,pathlib
if sys.argv[1:]==['--version']:print('herdr 0.9.1');sys.exit(0)
r=json.loads(sys.stdin.readline())
result={{'type':'pong','version':'0.9.1','capabilities':{{'workspace_create_command':True}}}}
p=pathlib.Path({root:?})/'server-capability.json'
if p.exists():result=json.loads(p.read_text())
print(json.dumps({{'id':r['id'],'result':result}}))
"#,root=f._root.path().display().to_string())).unwrap();
        let key=f._root.path().join("signer");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q","-t","ed25519","-N","","-f"])
            .arg(&key).status().unwrap().success());
        let public=fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        fs::write(&f.config,fs::read_to_string(&f.config).unwrap()
            .replace(&format!("ssh-ed25519 {}","A".repeat(48)),&public)
            .replace("extra_args=['PRIVATE_ARG']","extra_args=[]")).unwrap();
        let mut verified=retention_fixture(&f);
        verified.evidence.interaction=Some(InteractionEvidence {
            session:ResourceIdentity{device:1,inode:2,born_secs:1,born_nanos:0}, terminal:"fixture-terminal".into(),
            readiness_manifest:"fixture-manifest".into(),prompt_digest:"a".repeat(64),acknowledged_unix_ms:999,
        });
        verified.preparation=f.prepare().unwrap();
        super::super::native::apply_evidence(&mut verified.preparation,&verified.evidence).unwrap();
        let profile=verified.retain(&f.project).unwrap();
        let frozen=&verified.preparation.profile;
        let work=f._root.path().join("work");fs::create_dir(&work).unwrap();
        if repository {
            git(&work,&["init","--quiet"]);
            fs::write(work.join("file"),"original\n").unwrap();
            git(&work,&["add","file"]);git(&work,&["commit","--quiet","-m","initial"]);
        }
        let mut db=SqliteStore::open(&f.project.join(".state/state.db")).unwrap();
        let task=TaskId::new("launch-task").unwrap();
        let state=db.read_snapshot(None).unwrap();
        db.commit(Commit{expected_head:state.head,mutations:vec![Mutation::Task{expected:None,next:Task {
            id:task.clone(),revision:1,state:TaskState::Draft,title:"Retained task".into(),active_attempt:None,
        }}]}).unwrap();
        let state=db.read_snapshot(None).unwrap();
        let route=RuntimeRoute{socket:f._root.path().join("native.sock").display().to_string(),cwd:if suffix.is_empty() {work.display().to_string()} else {work.join(suffix).display().to_string()},..Default::default()};
        db.create_runtime(Some(&task),Some(1),state.head,&route).unwrap();
        let state=db.read_snapshot(None).unwrap();
        db.queue_task(&task,2,state.head,&QueueRequest{priority:0,dependencies:vec![]},crate::canonical_worker::now()).unwrap();
        let state=db.read_snapshot(None).unwrap();
        db.set_scheduler_policy(state.head,state.scheduler.as_ref().unwrap().policy.revision,2,3).unwrap();
        let state=db.read_snapshot(None).unwrap();
        let binding=state.runtime_bindings.iter().find(|b|b.task.as_ref()==Some(&task)).unwrap().clone();
        let now=crate::canonical_worker::now();
        db.record_observations(state.head,&[crate::reconcile::RuntimeObservation {
            binding:binding.id.clone(),binding_revision:binding.revision,task_revision:Some(3),
            observed_unix_ms:now,collector:"herdr-git-v2".into(),config_digest:frozen.config.digest.clone(),..Default::default()
        }]).unwrap();
        let state=db.read_snapshot(None).unwrap();
        crate::runtime::set_state(&f.project,state.head,state.control.unwrap().revision,ProjectState::Active,&f.config).unwrap();
        let mut memory=crate::memory::MemoryStore::from_sqlite(db,f.project.join(".state/objects"));
        let snapshot=memory.create_worker_snapshot(SnapshotRequest {
            schema_version:1,task_id:task.as_str().into(),profile:frozen.name.clone(),domains:vec![],paths:vec![],pinned_keys:vec![],sensitivity:"default".into(),
        },&frozen.name,&frozen.definition_digest,frozen.config.digest.as_deref(),32000,"Retained project instructions",now,None).unwrap();
        let knowledge=VersionedReference{id:snapshot.id.as_str().into(),revision:1,digest:snapshot.manifest_hash};
        Self { selection:LaunchSelection { task,binding:binding.id,profile,knowledge,repositories:if repository {vec![work]}else{vec![]} }, f,key,_socket:socket }
    }
    fn state(&self) -> Snapshot { crate::runtime::snapshot(&self.f.project).unwrap() }
    fn draft(&self) -> LaunchDraft {
        launch_preparation::draft(&self.f.project,&self.selection,self.state().head,Duration::from_secs(120),Instant::now()+Duration::from_secs(20),Default::default()).unwrap()
    }
    fn install(&self, draft: &LaunchDraft) -> VersionedReference {
        let document=self.f._root.path().join("approval.json");
        fs::write(&document,serde_json::to_vec_pretty(&draft.approval).unwrap()).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y","sign","-f"]).arg(&self.key)
            .args(["-n",crate::authority::SIGNATURE_NAMESPACE]).arg(&document).output().unwrap().status.success());
        crate::authority::import_signed(&self.f.project,&document,&document.with_extension("json.sig"),self.state().head).unwrap()
    }
    fn reserve(&self, approval: &VersionedReference) -> Result<Reservation> {
        launch_preparation::reserve(&self.f.project,&self.selection,approval,self.state().head,Instant::now()+Duration::from_secs(20),Default::default())
    }
}

#[test]
fn draft_signature_and_reservation_preserve_the_exact_brief_and_one_use_boundary() {
    let f=LaunchFixture::new(true);
    let before=f.state();
    fs::write(f.f.project.join("PROJECT.md"),"New text must not replace retained instructions").unwrap();
    let draft=f.draft();
    assert_eq!(f.state(),before);
    assert!(draft.brief.text.contains("Retained project instructions"));
    assert!(!draft.brief.text.contains("New text"));
    assert_eq!(draft.inputs.repositories.len(),1);
    assert_eq!(draft.worktrees.len(),1);
    assert!(draft.worktrees[0].path.contains(&draft.brief.attempt_id));
    assert_eq!(draft.inputs.repositories[0].commit,git(&f.selection.repositories[0],&["rev-parse","HEAD"]));
    let approval=f.install(&draft);
    assert_eq!(approval,draft.inputs.approval);
    let reservation=f.reserve(&approval).unwrap();
    assert_eq!(reservation.record.inputs,draft.inputs);
    assert_eq!(reservation.record.attempt.as_str(),draft.brief.attempt_id);
    assert_eq!(crate::memory::render_attempt_brief(&f.f.project,reservation.record.attempt.as_str()).unwrap().text,draft.brief.text);
    let reserved=f.state();
    assert_eq!(reserved.attempts.len(),1);
    assert!(reserved.approvals[0].consumed.is_none());
    assert_eq!(reserved.operations.iter().filter(|o|o.kind=="runtime.launch").count(),1);
    assert!(f.reserve(&approval).is_err());
    assert_eq!(f.state(),reserved);
}

#[test]
fn unsigned_or_changed_launch_inputs_never_create_a_reservation() {
    for changed in ["unsigned","task","knowledge","head","repository","revoked","profile"] {
        let mut f=LaunchFixture::new(true);
        let draft=f.draft();
        let approval=if changed=="unsigned" {draft.inputs.approval.clone()}else{f.install(&draft)};
        match changed {
            "task" => {
                let mut db=SqliteStore::open(&f.f.project.join(".state/state.db")).unwrap();
                let state=db.read_snapshot(None).unwrap();let mut task=state.tasks.iter().find(|t|t.id==f.selection.task).unwrap().clone();
                task.revision+=1;task.title="Changed task".into();
                db.commit(Commit{expected_head:state.head,mutations:vec![Mutation::Task{expected:Some(task.revision-1),next:task}]}).unwrap();
            }
            "knowledge" => f.selection.knowledge.digest="0".repeat(64),
            "repository" => {
                let path=&f.selection.repositories[0];fs::write(path.join("file"),"changed\n").unwrap();
                git(path,&["add","file"]);git(path,&["commit","--quiet","-m","changed"]);
            }
            "revoked" => {crate::authority::revoke(&f.f.project,&approval.id,f.state().head,"operator revoked").unwrap();}
            "profile" => fs::write(&f.f.agent,"changed").unwrap(),
            _=>{},
        }
        let before=f.state();
        let result=if changed=="head" {
            launch_preparation::reserve(&f.f.project,&f.selection,&approval,draft.head,Instant::now()+Duration::from_secs(20),Default::default())
        } else { f.reserve(&approval) };
        assert!(result.is_err(),"{changed}");
        assert_eq!(f.state(),before,"{changed}");
    }
}

#[test]
fn repository_observation_ignores_replacement_refs_and_refuses_lazy_fetch() {
    let f=LaunchFixture::new(true);let path=&f.selection.repositories[0];
    let original=git(path,&["rev-parse","HEAD"]);
    fs::write(path.join("file"),"second\n").unwrap();git(path,&["add","file"]);git(path,&["commit","--quiet","-m","second"]);
    let current=git(path,&["rev-parse","HEAD"]);let actual_tree=git(path,&["rev-parse","HEAD^{tree}"]);
    git(path,&["replace",&current,&original]);
    assert_ne!(git(path,&["rev-parse","HEAD^{tree}"]),actual_tree);
    let draft=f.draft();assert_eq!(draft.inputs.repositories[0].tree,actual_tree);
    git(path,&["config","remote.origin.promisor","true"]);
    let before=f.state();
    assert!(launch_preparation::draft(&f.f.project,&f.selection,before.head,Duration::from_secs(60),Instant::now()+Duration::from_secs(20),Default::default()).is_err());
    assert_eq!(f.state(),before);
}

#[test]
fn draft_preflight_rejects_closed_capacity_without_writing_approval_or_task_state() {
    let f=LaunchFixture::new(false);
    let mut db=SqliteStore::open(&f.f.project.join(".state/state.db")).unwrap();
    let state=db.read_snapshot(None).unwrap();
    db.set_scheduler_policy(state.head,state.scheduler.unwrap().policy.revision,0,3).unwrap();
    let before=f.state();
    assert!(launch_preparation::draft(&f.f.project,&f.selection,before.head,Duration::from_secs(60),Instant::now()+Duration::from_secs(20),Default::default()).is_err());
    assert_eq!(f.state(),before);
}

#[test]
fn signed_worktree_creation_retains_exact_checkout_and_recovers_without_replay() {
    let f=LaunchFixture::new(true);let draft=f.draft();let approval=f.install(&draft);let reservation=f.reserve(&approval).unwrap();
    // Tracked and untracked source edits must remain untouched; only the approved
    // immutable commit is materialized in the fresh worker worktree.
    let source=&f.selection.repositories[0];fs::write(source.join("file"),"local dirty edit\n").unwrap();fs::write(source.join("untracked"),"keep\n").unwrap();
    let run=|revision|crate::worktree_preparation::prepare(&f.f.project,&reservation.record.operation,revision,Instant::now()+Duration::from_secs(45),Default::default());
    let receipts=run(1).unwrap();assert_eq!(receipts.len(),1);assert_eq!(receipts[0].plan,draft.worktrees[0]);
    let target=Path::new(&receipts[0].plan.path);assert_eq!(fs::read_to_string(target.join("file")).unwrap(),"original\n");assert!(!target.join("untracked").exists());
    assert_eq!(fs::read_to_string(source.join("file")).unwrap(),"local dirty edit\n");assert_eq!(fs::read_to_string(source.join("untracked")).unwrap(),"keep\n");
    let before=f.state();let revision=before.deliveries[0].revision;assert_eq!(before.deliveries[0].attempts,1);assert!(before.approvals[0].consumed.is_some());
    assert_eq!(run(revision).unwrap(),receipts);assert_eq!(f.state(),before);
    assert_eq!(before.events.iter().filter(|e|e.kind=="runtime.worktrees_creation").count(),1);assert_eq!(before.events.iter().filter(|e|e.kind=="runtime.worktrees_ready").count(),1);
    fs::write(target.join("file"),"partial worker result\n").unwrap();assert!(run(revision).is_err());assert_eq!(f.state(),before);assert_eq!(fs::read_to_string(target.join("file")).unwrap(),"partial worker result\n");
    git(target,&["update-index","--assume-unchanged","file"]);
    assert!(git(target,&["status","--porcelain"]).is_empty());
    assert!(run(revision).is_err(),"stat-cache/index flags must not hide changed bytes");assert_eq!(f.state(),before);
    git(target,&["update-index","--no-assume-unchanged","file"]);fs::write(target.join("file"),"original\n").unwrap();
    fs::write(target.join(".gitignore"),"ignored\n").unwrap();fs::write(target.join("ignored"),"preserve ignored data\n").unwrap();
    assert!(run(revision).is_err());assert_eq!(fs::read_to_string(target.join("ignored")).unwrap(),"preserve ignored data\n");
}

#[test]
fn worktree_refusals_precede_approval_consumption_and_effects() {
    for mode in ["filter","partial","branch","directory","symlink","binary","intent_commit"] {
        let f=LaunchFixture::new(true);let draft=f.draft();let approval=f.install(&draft);let reservation=f.reserve(&approval).unwrap();
        let source=&f.selection.repositories[0];let path=Path::new(&draft.worktrees[0].path);
        match mode {
            "filter"=>{git(source,&["config","filter.evil.smudge","touch MUST_NOT_RUN"]);},
            "partial"=>{git(source,&["config","remote.origin.promisor","true"]);},
            "branch"=>{git(source,&["branch",&draft.worktrees[0].branch]);},
            "directory"=>{fs::create_dir_all(path).unwrap();fs::write(path.join("keep"),"unrelated").unwrap();},
            "symlink"=>{std::os::unix::fs::symlink(source,f.f.project.join(".state/worktrees")).unwrap();},
            "binary"=>{fs::write(&f.f.agent,"#!/bin/sh\nexit 99\n").unwrap();},
            _=>{rusqlite::Connection::open(f.f.project.join(".state/state.db")).unwrap().execute_batch("CREATE TRIGGER refuse_worktree_intent AFTER INSERT ON events WHEN NEW.kind='runtime.worktrees_creation' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();}
        }
        let before=f.state();assert!(crate::worktree_preparation::prepare(&f.f.project,&reservation.record.operation,1,Instant::now()+Duration::from_secs(45),Default::default()).is_err(),"{mode}");
        assert_eq!(f.state(),before,"{mode}");assert!(!source.join("MUST_NOT_RUN").exists());
        if mode=="directory" {assert_eq!(fs::read_to_string(path.join("keep")).unwrap(),"unrelated");}
    }
}

#[test]
fn lost_worktree_receipt_commit_recovers_after_approval_revocation_without_git_add() {
    let f=LaunchFixture::new(true);let draft=f.draft();let approval=f.install(&draft);let reservation=f.reserve(&approval).unwrap();
    let raw=rusqlite::Connection::open(f.f.project.join(".state/state.db")).unwrap();
    raw.execute_batch("CREATE TRIGGER refuse_worktree_receipt BEFORE INSERT ON events WHEN NEW.kind='runtime.worktrees_ready' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    let run=|revision|crate::worktree_preparation::prepare(&f.f.project,&reservation.record.operation,revision,Instant::now()+Duration::from_secs(45),Default::default());
    assert!(run(1).is_err());let before=f.state();assert!(Path::new(&draft.worktrees[0].path).join("file").exists());assert!(!before.events.iter().any(|e|e.kind=="runtime.worktrees_ready"));
    raw.execute_batch("DROP TRIGGER refuse_worktree_receipt").unwrap();
    // Revocation prevents new effects but not read-only recovery of resources.
    crate::authority::revoke(&f.f.project,&approval.id,before.head,"recover only").unwrap();
    let receipts=run(f.state().deliveries[0].revision).unwrap();assert_eq!(receipts[0].plan,draft.worktrees[0]);assert_eq!(f.state().deliveries[0].attempts,1);
}

#[test]
fn worktree_creation_supports_multiple_repositories_binary_files_and_symlinks() {
    let mut f=LaunchFixture::new(true);let other=f.f._root.path().join("second-repository");fs::create_dir(&other).unwrap();git(&other,&["init","--quiet"]);
    fs::write(other.join("binary"),[0,255,10,13,128,0]).unwrap();
    fs::write(other.join("executable"),"#!/bin/sh\nexit 0\n").unwrap();fs::set_permissions(other.join("executable"),fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("/outside/target-not-followed",other.join("link")).unwrap();
    git(&other,&["add","."]);git(&other,&["commit","--quiet","-m","binary fixture"]);f.selection.repositories.push(other.clone());
    let draft=f.draft();assert_eq!(draft.worktrees.len(),2);let approval=f.install(&draft);let reservation=f.reserve(&approval).unwrap();
    let receipts=crate::worktree_preparation::prepare(&f.f.project,&reservation.record.operation,1,Instant::now()+Duration::from_secs(45),Default::default()).unwrap();assert_eq!(receipts.len(),2);
    let binary=receipts.iter().find(|r|r.plan.source.repository==other.to_str().unwrap()).unwrap();let target=Path::new(&binary.plan.path);
    assert_eq!(fs::read(target.join("binary")).unwrap(),[0,255,10,13,128,0]);
    assert_eq!(fs::read_link(target.join("link")).unwrap(),Path::new("/outside/target-not-followed"));
    assert_ne!(receipts[0].directory,receipts[1].directory);assert_ne!(receipts[0].common_identity,receipts[1].common_identity);
    let mut db=crate::migration::open_active(&f.f.project).unwrap();let state=f.state();
    db.cancel_attempt(&reservation.record.attempt,state.attempts[0].revision,state.head,"preserve stopped checkouts",crate::canonical_worker::now()).unwrap();
    let state=f.state();
    crate::canonical_worker::reconcile_termination(&f.f.project,&reservation.record.attempt,state.attempts[0].revision,Instant::now()+Duration::from_secs(10),Default::default()).unwrap().unwrap();
    let stopped=f.state();
    let files=crate::worktree_preservation::capture_stopped_files(&f.f.project,&reservation.record.attempt,stopped.head,Instant::now()+Duration::from_secs(10),Default::default()).unwrap();assert_eq!(files.len(),2);
    assert_eq!(fs::read_link(target.join("link")).unwrap(),Path::new("/outside/target-not-followed"));
    let snapshots=crate::worktree_preservation::capture_stopped_repository(&f.f.project,&reservation.record.attempt,stopped.head,Instant::now()+Duration::from_secs(10),Default::default()).unwrap();
    assert_eq!(snapshots.len(),2);
    assert!(snapshots.iter().all(|s|s.manifest.git.is_some()));
    assert_eq!(snapshots.iter().map(|s|s.manifest.worktree.clone()).collect::<Vec<_>>(),receipts);
    let snapshot=snapshots.iter().find(|s|s.manifest.worktree.plan.source.repository==other.to_str().unwrap()).unwrap();
    assert_eq!(snapshot.manifest.version,3);
    let link=snapshot.manifest.entries.iter().find(|e|e.path=="link").unwrap();assert!(link.symlink&&!link.directory&&!link.executable);
    let target_bytes=fs::read(snapshot.directory.join(&link.sha256)).unwrap();assert_eq!(target_bytes,b"/outside/target-not-followed");
    let restored=f.f._root.path().join("restored-link");std::os::unix::fs::symlink(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&target_bytes),&restored).unwrap();
    assert_eq!(fs::read_link(&restored).unwrap(),fs::read_link(target.join("link")).unwrap());
    let entry=snapshot.manifest.entries.iter().find(|e|e.path=="binary").unwrap();
    assert_eq!(fs::read(snapshot.directory.join(&entry.sha256)).unwrap(),[0,255,10,13,128,0]);
    assert_eq!(f.state(),stopped);
}

fn worktree_inventory(project:&Path) -> Result<Vec<(String,WorktreePlan)>> {
    let mut budget=crate::store::identity_inventory::Budget::new(50*1024*1024,1024,Instant::now()+Duration::from_secs(10),Default::default())?;
    crate::migration::read_worktree_inventory(project,&mut budget)
}

#[test]
fn worktree_inventory_retains_uncertain_paths_and_refuses_corrupt_provenance() {
    let f=LaunchFixture::new(true);let draft=f.draft();let approval=f.install(&draft);let reservation=f.reserve(&approval).unwrap();
    let raw=rusqlite::Connection::open(f.f.project.join(".state/state.db")).unwrap();
    raw.execute_batch("CREATE TRIGGER refuse_tree_ready BEFORE INSERT ON events WHEN NEW.kind='runtime.worktrees_ready' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(crate::worktree_preparation::prepare(&f.f.project,&reservation.record.operation,1,Instant::now()+Duration::from_secs(45),Default::default()).is_err());
    let expected=vec![(f.selection.binding.clone(),draft.worktrees[0].clone())];
    assert_eq!(worktree_inventory(&f.f.project).unwrap(),expected);
    // A competing owner must see the reference even without a ready receipt.
    assert!(crate::canonical_worker::inventory::check_worktrees(&f.f.project,"another-binding",&draft.worktrees,Instant::now()+Duration::from_secs(10),Default::default()).is_err());
    crate::canonical_worker::inventory::check_worktrees(&f.f.project,&f.selection.binding,&draft.worktrees,Instant::now()+Duration::from_secs(10),Default::default()).unwrap();
    let competitor=f.f.project.parent().unwrap().join("competing-project");fs::create_dir(&competitor).unwrap();
    for overlap in [draft.worktrees[0].path.clone(),Path::new(&draft.worktrees[0].path).parent().unwrap().display().to_string(),format!("{}/nested",draft.worktrees[0].path)] {
        let mut plans=draft.worktrees.clone();plans[0].path=overlap;
        let error=crate::canonical_worker::inventory::check_worktrees(&competitor,"other",&plans,Instant::now()+Duration::from_secs(10),Default::default()).unwrap_err();
        assert!(error.to_string().contains("worktree path is referenced"),"{error}");
    }
    raw.execute_batch("DROP TRIGGER refuse_tree_ready").unwrap();
    crate::worktree_preparation::prepare(&f.f.project,&reservation.record.operation,f.state().deliveries[0].revision,Instant::now()+Duration::from_secs(45),Default::default()).unwrap();
    let before=f.state();assert_eq!(worktree_inventory(&f.f.project).unwrap(),expected);assert_eq!(f.state(),before);
    let event=before.events.iter().find(|e|e.kind=="runtime.worktrees_creation").unwrap();
    for mode in ["path","token","operation","duplicate","oversized","orphan_ready"] {
        raw.execute_batch("SAVEPOINT corrupt_inventory").unwrap();
        match mode {
            "duplicate"=>{raw.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) SELECT kind,entity,revision,payload_version,payload FROM events WHERE kind='runtime.worktrees_creation'",[]).unwrap();},
            "oversized"=>{raw.execute("UPDATE events SET payload=?1 WHERE kind='runtime.worktrees_creation'",[serde_json::to_string(&"x".repeat(1024*1024+1)).unwrap()]).unwrap();},
            "orphan_ready"=>{raw.execute("DELETE FROM events WHERE kind='runtime.worktrees_creation'",[]).unwrap();},
            _=>{let mut bad=event.payload.clone();match mode {"path"=>bad["plans"][0]["path"]="/tmp/foreign".into(),"token"=>bad["token"]="bad".into(),_=>bad["operation"]="other".into()};raw.execute("UPDATE events SET payload=?1 WHERE kind='runtime.worktrees_creation'",[serde_json::to_string(&bad).unwrap()]).unwrap();}
        }
        // Publish corruption for a separate read-only connection, then restore
        // the exact payload/state without relying on an in-process snapshot.
        raw.execute_batch("RELEASE corrupt_inventory").unwrap();
        let error=worktree_inventory(&f.f.project).unwrap_err();
        if mode=="oversized" {assert!(error.to_string().contains("exceeds bounds"),"{error}");}
        raw.execute("DELETE FROM events WHERE kind='runtime.worktrees_creation'",[]).unwrap();
        raw.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worktrees_creation',?1,?2,1,?3)",rusqlite::params![event.entity,event.revision,serde_json::to_string(&event.payload).unwrap()]).unwrap();
    }
    raw.execute_batch("CREATE TEMP TABLE kept_bindings AS SELECT * FROM runtime_bindings; DELETE FROM runtime_bindings;").unwrap();
    assert!(worktree_inventory(&f.f.project).is_err(),"orphaned binding must retain uncertainty");
    raw.execute_batch("INSERT INTO runtime_bindings SELECT * FROM kept_bindings; DROP TABLE kept_bindings;").unwrap();
    let mut cancelled=crate::store::identity_inventory::Budget::new(50*1024*1024,1024,Instant::now()+Duration::from_secs(10),Default::default()).unwrap();cancelled.cancellation.cancel();
    assert!(crate::migration::read_worktree_inventory(&f.f.project,&mut cancelled).is_err());
    let mut empty=crate::store::identity_inventory::Budget::new(50*1024*1024,0,Instant::now()+Duration::from_secs(10),Default::default()).unwrap();
    assert!(crate::migration::read_worktree_inventory(&f.f.project,&mut empty).is_err());
    // Missing filesystem resources do not silently drop retained ownership.
    let target=Path::new(&draft.worktrees[0].path);fs::rename(target,target.with_file_name("preserved-tree")).unwrap();
    assert_eq!(worktree_inventory(&f.f.project).unwrap(),expected);
}

#[test]
fn legacy_worktree_reference_blocks_new_creation_before_consuming_approval() {
    let f=LaunchFixture::new(true);let draft=f.draft();let approval=f.install(&draft);let reservation=f.reserve(&approval).unwrap();
    let neighbor=f.f.project.parent().unwrap().join("legacy-neighbor");fs::create_dir(&neighbor).unwrap();fs::create_dir(neighbor.join(".state")).unwrap();fs::create_dir(neighbor.join("threads")).unwrap();
    fs::write(neighbor.join("PROJECT.md"),"legacy fixture").unwrap();
    fs::write(neighbor.join("threads/t-0001.toml"),format!("id='t-0001'\nworktree_path={}\n",serde_json::to_string(&draft.worktrees[0].path).unwrap())).unwrap();
    let before=f.state();assert!(crate::worktree_preparation::prepare(&f.f.project,&reservation.record.operation,1,Instant::now()+Duration::from_secs(45),Default::default()).is_err());
    assert_eq!(f.state(),before);assert!(!Path::new(&draft.worktrees[0].path).exists());
}

#[test]
fn started_worktree_proof_allows_output_but_rejects_reassociation() {
    let f=LaunchFixture::new(true); let draft=f.draft(); let approval=f.install(&draft); let reservation=f.reserve(&approval).unwrap();
    let receipts=crate::worktree_preparation::prepare(&f.f.project,&reservation.record.operation,1,Instant::now()+Duration::from_secs(45),Default::default()).unwrap();
    let state=f.state(); let receipt=&receipts[0]; let target=Path::new(&receipt.plan.path);
    fs::write(target.join("file"),"worker output").unwrap();
    let proof=crate::worktree_preparation::pin_started_held(&f.f.project,&state,&reservation.record,Instant::now()+Duration::from_secs(10),Default::default()).unwrap();
    proof.check().unwrap();
    for path in [target.join(".git"),Path::new(&receipt.git_directory).join("gitdir"),Path::new(&receipt.git_directory).join("commondir"),Path::new(&receipt.git_directory).join("locked"),Path::new(&receipt.git_directory).join("HEAD")] {
        let original=fs::read(&path).unwrap(); fs::write(&path,"foreign").unwrap();
        assert!(proof.check().is_err(),"{}",path.display());
        assert!(crate::worktree_preparation::pin_started_held(&f.f.project,&state,&reservation.record,Instant::now()+Duration::from_secs(10),Default::default()).is_err());
        fs::write(&path,original).unwrap(); proof.check().unwrap();
    }
    // A valid foreign branch or detached HEAD is still the wrong checkout
    // identity. Content and commits on the approved branch are legitimate.
    let head_path=Path::new(&receipt.git_directory).join("HEAD");
    let original=fs::read(&head_path).unwrap();
    git(target,&["branch","foreign",&receipt.plan.source.commit]);
    for changed in ["ref: refs/heads/foreign\n".into(),format!("{}\n",receipt.plan.source.commit)] {
        fs::write(&head_path,changed).unwrap();
        assert!(proof.check().is_err());
        assert!(crate::worktree_preparation::pin_started_held(&f.f.project,&state,&reservation.record,Instant::now()+Duration::from_secs(10),Default::default()).is_err());
    }
    fs::write(&head_path,original).unwrap();
    git(target,&["-c","commit.gpgsign=false","commit","-am","worker result"]);
    assert_ne!(git(target,&["rev-parse","HEAD"]),receipt.plan.source.commit);
    proof.check().unwrap();
    crate::worktree_preparation::pin_started_held(&f.f.project,&state,&reservation.record,Instant::now()+Duration::from_secs(10),Default::default()).unwrap().check().unwrap();
    assert_eq!(f.state(),state,"identity observation must not rewrite canonical records");
}

#[test]
fn worktree_route_maps_root_and_subdirectory_and_refuses_foreign_sources() {
    let f=LaunchFixture::new(true); let draft=f.draft();
    let attempt=AttemptId::new(draft.brief.attempt_id.clone()).unwrap();
    let mut identity=f.state().runtime_bindings[0].identity.clone();
    let source=draft.inputs.repositories[0].repository.clone();
    for suffix in ["", "/", "/subdir", "/subdir//"] {
        identity.cwd=format!("{source}{suffix}");
        let (route,plan)=worktree_execution_route(&draft.inputs,&attempt,&identity).unwrap();
        let plan=plan.unwrap();
        assert_eq!(route.cwd,if suffix.contains("subdir") {format!("{}/subdir",plan.path)} else {plan.path});
    }
    identity.cwd="/tmp/foreign".into();
    assert!(worktree_execution_route(&draft.inputs,&attempt,&identity).is_err());
    identity.cwd=source;identity.repo="/tmp/foreign".into();
    assert!(worktree_execution_route(&draft.inputs,&attempt,&identity).is_err());
}

#[test]
fn source_only_working_directory_is_refused_before_approval_consumption() {
    let f=LaunchFixture::with_cwd(true,"untracked-dir");
    fs::create_dir(f.selection.repositories[0].join("untracked-dir")).unwrap();
    let draft=f.draft();let approval=f.install(&draft);let reservation=f.reserve(&approval).unwrap();
    let before=f.state();
    let error=crate::worktree_preparation::prepare(&f.f.project,&reservation.record.operation,1,Instant::now()+Duration::from_secs(45),Default::default()).unwrap_err();
    assert!(error.to_string().contains("working directory is absent"),"{error:#}");
    assert_eq!(f.state(),before);
    assert!(!Path::new(&draft.worktrees[0].path).exists());
}

#[test]
fn worktree_only_stop_fences_late_launch_and_preserves_uncertain_resources() {
    for mode in ["ready", "partial", "missing", "missing_unobserved", "uncreated", "expired", "native_intent", "rollback", "destination"] {
        let f=LaunchFixture::new(true);let draft=f.draft();let approval=f.install(&draft);let reservation=f.reserve(&approval).unwrap();
        let receipts=crate::worktree_preparation::prepare(&f.f.project,&reservation.record.operation,1,Instant::now()+Duration::from_secs(45),Default::default()).unwrap();
        let target=Path::new(&receipts[0].plan.path);
        let mut db=crate::migration::open_active(&f.f.project).unwrap();
        let initial=f.state();
        // Active preparation is not a termination hint or permission to stop.
        assert!(crate::canonical_worker::reconcile_termination(&f.f.project,&reservation.record.attempt,initial.attempts[0].revision,Instant::now()+Duration::from_secs(10),Default::default()).unwrap().is_none());
        assert_eq!(f.state(),initial);
        assert!(!f.f.project.join(".state/worktree-file-snapshots").exists());
        let mut budget=crate::store::identity_inventory::Budget::new(2*1024*1024,1024,Instant::now()+Duration::from_secs(5),Default::default()).unwrap();
        assert!(crate::migration::read_controller_effect_hint(&f.f.project,&mut budget,0,crate::canonical_worker::now()).unwrap().is_none());
        let raw=rusqlite::Connection::open(f.f.project.join(".state/state.db")).unwrap();
        if mode=="partial" {
            raw.execute("DELETE FROM events WHERE kind='runtime.worktrees_ready'",[]).unwrap();
            fs::write(target.join("file"),"partial result").unwrap();
        }
        if matches!(mode,"missing"|"missing_unobserved") {
            fs::rename(target,target.with_file_name("preserved-tree")).unwrap();
            if mode=="missing_unobserved" {raw.execute("DELETE FROM events WHERE kind='runtime.worktrees_ready'",[]).unwrap();}
        }
        if mode=="uncreated" {
            raw.execute("DELETE FROM events WHERE kind='runtime.worktrees_ready'",[]).unwrap();
            let source=Path::new(&receipts[0].plan.source.repository);
            git(source,&["worktree","unlock",target.to_str().unwrap()]);
            git(source,&["worktree","remove",target.to_str().unwrap()]);
            git(source,&["branch","-D",&receipts[0].plan.branch]);
        }
        let output=worker_output_path(&reservation.record.inputs,&reservation.record.attempt).unwrap();
        fs::create_dir_all(&output).unwrap();fs::write(Path::new(&output).join("report.md"),b"preparation note").unwrap();

        if mode=="expired" {
            db.expire_claims(initial.deliveries[0].lease_until_ms.unwrap()).unwrap();
            let expiry:String=raw.query_row("SELECT payload FROM events WHERE kind='operation.outcome' AND entity=?1",[reservation.record.operation.as_str()],|r|r.get(0)).unwrap();
            raw.execute("UPDATE events SET payload=json_set(payload,'$.actor','untrusted') WHERE kind='operation.outcome' AND entity=?1",[reservation.record.operation.as_str()]).unwrap();
            let mut bad_budget=crate::store::identity_inventory::Budget::new(2*1024*1024,1024,Instant::now()+Duration::from_secs(5),Default::default()).unwrap();
            assert!(crate::migration::read_worktree_inventory(&f.f.project,&mut bad_budget).is_err());
            raw.execute("UPDATE events SET payload=?2 WHERE kind='operation.outcome' AND entity=?1",rusqlite::params![reservation.record.operation.as_str(),expiry]).unwrap();
        } else {
            let state=f.state();db.cancel_attempt(&reservation.record.attempt,state.attempts[0].revision,state.head,"cancel preparation",crate::canonical_worker::now()).unwrap();
        }
        let mut budget=crate::store::identity_inventory::Budget::new(2*1024*1024,1024,Instant::now()+Duration::from_secs(5),Default::default()).unwrap();
        let inventory=crate::migration::read_worktree_inventory(&f.f.project,&mut budget).unwrap();
        assert_eq!(inventory.len(),1);
        if mode=="native_intent" {raw.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.launch_creation',?1,1,1,'{}')",[reservation.record.operation.as_str()]).unwrap();}
        let before=f.state();
        let run=||crate::canonical_worker::reconcile_termination(&f.f.project,&reservation.record.attempt,before.attempts[0].revision,Instant::now()+Duration::from_secs(10),Default::default());
        // A surviving Git supervisor's inherited root barrier blocks retirement.
        let guard=crate::execution_guard::RootGuard::exclusive(f.f.project.parent().unwrap()).unwrap();
        let inherited=guard.inherit().unwrap();drop(guard);
        assert!(run().is_err());assert_eq!(f.state(),before);drop(inherited);
        if mode=="native_intent" {assert!(run().is_err());assert_eq!(f.state(),before);continue;}
        if matches!(mode,"missing"|"missing_unobserved") {
            assert!(run().is_err());assert_eq!(f.state(),before);assert!(before.attempts[0].retains_capacity());
            fs::rename(target.with_file_name("preserved-tree"),target).unwrap();
        }
        if mode=="destination" {
            let destination=f.f.project.join(".state/worktree-file-snapshots");
            std::os::unix::fs::symlink(target,&destination).unwrap();assert!(run().is_err());assert_eq!(f.state(),before);
            fs::remove_file(destination).unwrap();
        }

        let mut budget=crate::store::identity_inventory::Budget::new(2*1024*1024,1024,Instant::now()+Duration::from_secs(5),Default::default()).unwrap();
        let hint=crate::migration::read_controller_effect_hint(&f.f.project,&mut budget,0,crate::canonical_worker::now()).unwrap().unwrap();
        assert_eq!(hint.operation.kind,"runtime.worker_termination");
        assert_eq!(hint.operation.target,reservation.record.attempt.as_str());
        if mode=="ready" {
            let guard=crate::execution_guard::RootGuard::exclusive(f.f.project.parent().unwrap()).unwrap();
            assert!(db.stop_worktree_preparation(&reservation.record.attempt,before.attempts[0].revision,before.head,&guard,|_,_|Ok((vec![],AttemptOutputReference{source:output.clone(),digest:None})),crate::canonical_worker::now()).is_err());
            assert_eq!(f.state(),before);
        }
        if mode=="rollback" {
            raw.execute_batch("CREATE TRIGGER reject_worktree_stop BEFORE INSERT ON events WHEN NEW.kind='runtime.worktrees_stopped' BEGIN SELECT RAISE(ABORT,'fixture refusal'); END;").unwrap();
            assert!(run().is_err());assert_eq!(f.state(),before);
            raw.execute_batch("DROP TRIGGER reject_worktree_stop").unwrap();
        }
        let stopped=run().unwrap().unwrap();assert!(stopped.termination_observed);assert!(!stopped.retains_capacity());
        assert_eq!(stopped.state,if mode=="expired" {AttemptState::Failed} else {AttemptState::Cancelled});
        let after=f.state();assert!(after.tasks[0].active_attempt.is_none());
        let evidence=&after.events.iter().find(|e|e.kind=="runtime.worktrees_stopped").unwrap().payload;
        assert_eq!(evidence["version"],2);
        let snapshots:Vec<PreparationSnapshotReference>=serde_json::from_value(evidence["repository_snapshots"].clone()).unwrap();
        assert_eq!(snapshots.len(),1);assert_eq!(snapshots[0].plan,receipts[0].plan);
        if mode=="uncreated" {assert!(snapshots[0].digest.is_none());}else{
            let directory=f.f.project.join(".state/worktree-file-snapshots").join(reservation.record.attempt.as_str()).join(snapshots[0].digest.as_ref().unwrap());
            let manifest:crate::worktree_preservation::Manifest=serde_json::from_slice(&fs::read(directory.join("manifest.json")).unwrap()).unwrap();
            assert_eq!(manifest.scope,"repository_state");assert!(manifest.git.is_some());
            let file=manifest.entries.iter().find(|e|e.path=="file").unwrap();
            assert_eq!(fs::read(directory.join(&file.sha256)).unwrap(),if mode=="partial"{b"partial result".as_slice()}else{b"original\n".as_slice()});
            assert_eq!(fs::read_dir(directory.parent().unwrap()).unwrap().count(),1);
        }
        let output:AttemptOutputReference=serde_json::from_value(evidence["output_snapshot"].clone()).unwrap();
        let directory=f.f.project.join(".state/worker-output-snapshots").join(reservation.record.attempt.as_str()).join(output.digest.unwrap());
        let manifest:crate::worktree_preservation::OutputManifest=serde_json::from_slice(&fs::read(directory.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(fs::read(directory.join(&manifest.entries[0].sha256)).unwrap(),b"preparation note");

        assert_eq!(after.deliveries[0].state,crate::operations::DeliveryState::PermanentFailure);
        assert_eq!(after.approvals,before.approvals);
        assert!(crate::canonical_worker::create_resource(&f.f.project,&reservation.record.operation,before.deliveries[0].revision,Instant::now()+Duration::from_secs(10),Default::default()).is_err());
        assert!(crate::canonical_worker::create_resource(&f.f.project,&reservation.record.operation,after.deliveries[0].revision,Instant::now()+Duration::from_secs(10),Default::default()).is_err());
        assert!(crate::worktree_preparation::prepare(&f.f.project,&reservation.record.operation,after.deliveries[0].revision,Instant::now()+Duration::from_secs(10),Default::default()).is_err());
        assert_eq!(crate::canonical_worker::reconcile_termination(&f.f.project,&reservation.record.attempt,stopped.revision,Instant::now()+Duration::from_secs(10),Default::default()).unwrap(),Some(stopped));
        assert_eq!(f.state(),after);
        let mut budget=crate::store::identity_inventory::Budget::new(2*1024*1024,1024,Instant::now()+Duration::from_secs(5),Default::default()).unwrap();
        assert_eq!(crate::migration::read_worktree_inventory(&f.f.project,&mut budget).unwrap(),inventory);
        if mode!="uncreated" {assert_eq!(fs::read_to_string(target.join("file")).unwrap(),if mode=="partial" {"partial result"} else {"original\n"});}
    }
}


#[test]
fn preparation_stop_preserves_created_repositories_and_records_uncreated_plans_in_order() {
    let mut f=LaunchFixture::new(true);let other=f.f._root.path().join("second-repository");fs::create_dir(&other).unwrap();git(&other,&["init","--quiet"]);
    fs::write(other.join("second"),b"second repository").unwrap();git(&other,&["add","."]);git(&other,&["commit","--quiet","-m","second fixture"]);f.selection.repositories.push(other);
    let draft=f.draft();let approval=f.install(&draft);let reservation=f.reserve(&approval).unwrap();
    let receipts=crate::worktree_preparation::prepare(&f.f.project,&reservation.record.operation,1,Instant::now()+Duration::from_secs(45),Default::default()).unwrap();assert_eq!(receipts.len(),2);
    let raw=rusqlite::Connection::open(f.f.project.join(".state/state.db")).unwrap();raw.execute("DELETE FROM events WHERE kind='runtime.worktrees_ready'",[]).unwrap();
    let absent=&receipts[0];let source=Path::new(&absent.plan.source.repository);git(source,&["worktree","unlock",&absent.plan.path]);git(source,&["worktree","remove",&absent.plan.path]);git(source,&["branch","-D",&absent.plan.branch]);
    let present=Path::new(&receipts[1].plan.path);fs::write(present.join("partial.bin"),[0,255,9]).unwrap();fs::remove_file(Path::new(&receipts[1].git_directory).join("index")).unwrap();
    let mut db=crate::migration::open_active(&f.f.project).unwrap();let before=f.state();db.cancel_attempt(&reservation.record.attempt,before.attempts[0].revision,before.head,"partial multi-repo cancellation",crate::canonical_worker::now()).unwrap();
    let state=f.state();crate::canonical_worker::reconcile_termination(&f.f.project,&reservation.record.attempt,state.attempts[0].revision,Instant::now()+Duration::from_secs(45),Default::default()).unwrap().unwrap();
    let after=f.state();let event=&after.events.iter().find(|e|e.kind=="runtime.worktrees_stopped").unwrap().payload;
    let snapshots:Vec<PreparationSnapshotReference>=serde_json::from_value(event["repository_snapshots"].clone()).unwrap();assert_eq!(snapshots.len(),2);
    assert_eq!(snapshots[0].plan,receipts[0].plan);assert!(snapshots[0].digest.is_none());assert_eq!(snapshots[1].plan,receipts[1].plan);
    let directory=f.f.project.join(".state/worktree-file-snapshots").join(reservation.record.attempt.as_str()).join(snapshots[1].digest.as_ref().unwrap());
    let manifest:crate::worktree_preservation::Manifest=serde_json::from_slice(&fs::read(directory.join("manifest.json")).unwrap()).unwrap();assert!(manifest.git.as_ref().unwrap().state.index.is_none());
    let file=manifest.entries.iter().find(|e|e.path=="partial.bin").unwrap();assert_eq!(fs::read(directory.join(&file.sha256)).unwrap(),[0,255,9]);assert_eq!(fs::read(present.join("partial.bin")).unwrap(),[0,255,9]);
    assert!(after.attempts[0].termination_observed&&!after.attempts[0].retains_capacity());
}
