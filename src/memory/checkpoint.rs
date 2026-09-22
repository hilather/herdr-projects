//! Coordinator checkpoints and delta context. Does not change worker briefs.
use super::{MemoryError, MemoryStore};
use crate::domain::*;
use crate::migration;
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct CoordinatorContext {
    pub text: String,
    pub head: u64,
    pub unseen: Vec<String>,
    pub checkpoint_id: String,
    pub kind: String,
}

fn objects_dir(project: &Path) -> std::path::PathBuf { project.join(".state/objects") }

fn constraint_lines(_project: &Path, memory: &mut MemoryStore, snap: &MemorySnapshot) -> Result<String, MemoryError> {
    let mut out = String::from("## Mandatory constraints\n");
    let mut any = false;
    for entry in snap.entries.iter().filter(|e| e.role == "mandatory") {
        let rec = memory.store.memory_record(entry.record_id.as_str()).map_err(MemoryError::from)?
            .ok_or_else(|| MemoryError::Invalid("checkpoint snapshot record missing".into()))?;
        let rev = memory.store.memory_revision(entry.record_id.as_str(), entry.revision).map_err(MemoryError::from)?
            .ok_or_else(|| MemoryError::Invalid("checkpoint snapshot revision missing".into()))?;
        let body = super::read_object(&memory.objects, &rev.body_hash)?;
        let text = std::str::from_utf8(&body).map_err(|_| MemoryError::Invalid("memory body is not UTF-8".into()))?;
        any = true;
        out.push_str(&format!("- {} ({})\n{}\n", rec.record_key, rec.kind.as_str(), text.trim()));
    }
    if !any { out.push_str("(none)\n"); }
    Ok(out)
}

fn blockers(snapshot: &crate::domain::Snapshot) -> String {
    let mut out = String::from("## Unresolved blockers\n");
    let mut any = false;
    for task in &snapshot.tasks {
        if matches!(task.state, TaskState::Blocked) {
            any = true;
            out.push_str(&format!("- task {} revision {}: {}\n", task.id.as_str(), task.revision, task.title.replace(['\n','\r']," ")));
        }
    }
    for item in snapshot.inbox.iter().filter(|i| !i.done) {
        any = true;
        out.push_str(&format!("- inbox {}: {}\n", item.content.id, item.content.summary));
    }
    if !any { out.push_str("(none)\n"); }
    out
}

fn base_context(project: &Path) -> Result<(String, u64, Vec<String>, crate::domain::Snapshot)> {
    let snapshot = crate::runtime::snapshot(project)?;
    let (text, head, unseen) = crate::runtime::context_from_snapshot(project, &snapshot)?;
    Ok((text, head, unseen, snapshot))
}

/// A fresh token is required after restart or compaction uncertainty. Tokens are
/// explicit local session handles, not authentication against a same-user process.
pub fn new_coordinator_session() -> Result<String> {
    use std::io::Read;
    let mut bytes=[0u8;32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(format!("context-{:x}",Sha256::digest(bytes)))
}

fn bound_session(project:&Path, snapshot:&crate::domain::Snapshot, token:&str)->Result<String> {
    ensure!(!token.is_empty() && token.len()<=128 && token.bytes().all(|b|b.is_ascii_alphanumeric() || b"-_.:".contains(&b)), "invalid coordinator session token");
    let route=snapshot.runtime_bindings.iter().find(|b|b.id=="coordinator");
    let control=snapshot.control.as_ref();
    Ok(format!("context-bound-{:x}",Sha256::digest(serde_json::to_vec(&serde_json::json!([
        "coordinator-session-v2",project.join(".state/state.db").canonicalize()?.to_string_lossy(),token,route,control.map(|c|c.epoch),control.and_then(|c|c.config_digest.as_ref())
    ]))?)))
}

#[cfg(test)]
pub(crate) fn coordinator_session_key(project:&Path,token:&str)->Result<String> {
    bound_session(project,&crate::runtime::snapshot(project)?,token)
}

/// New/restarted/uncertain sessions get a full checkpoint. Deltas only after ack.
pub fn coordinator_context(project: &Path, herdr_session: &str, profile: &CheckpointProfile, instructions: &str) -> Result<CoordinatorContext> {
    ensure!(!profile.name.is_empty() && profile.digest.len()==64, "context requires a named profile digest");
    let now = jiff::Timestamp::now().as_millisecond();
    let mut db = migration::open_active(project)?;
    ensure!(db.read_snapshot(None)?.schema_version >= 20, "upgrade-store is required for coordinator checkpoints");
    let session_key = bound_session(project,&db.read_snapshot(None)?, herdr_session)?;
    let session = db.upsert_coordinator_session(&session_key, now)?;
    let last = match session.last_checkpoint_id.as_deref() {
        Some(id) => db.coordinator_checkpoint(id)?,
        None => None,
    };
    let head = db.read_snapshot(None)?.head;
    let mut delta_ok = last.as_ref().is_some_and(|c| c.acked && session.cursor_seq <= head);
    if let Some(checkpoint) = last.as_ref().filter(|_|delta_ok) {
        let prior = db.read_memory_snapshot(&checkpoint.snapshot_id)?;
        let retained = db.memory_snapshot_inputs(&checkpoint.snapshot_id)?;
        delta_ok = prior.profile_name==profile.name && prior.profile_digest==profile.digest
            && prior.config_digest==profile.config_digest && prior.budget_bytes==profile.budget_chars
            && retained.instructions==instructions;
    }
    let kind = if delta_ok { "delta" } else { "full" };
    let from_seq = if kind=="delta" { session.cursor_seq } else { 0 };
    let mut memory = MemoryStore::from_sqlite(db, objects_dir(project));
    let snap = match memory.create_coordinator_snapshot(&session.id, &profile.name, &profile.digest, profile.config_digest.as_deref(), profile.budget_chars, instructions, now) {
        Ok(s) => s,
        Err(error) => return Err(anyhow::anyhow!("{error}")),
    };
    let (base, head, unseen, snapshot) = match base_context(project) {
        Ok(v) => v,
        Err(error) => return Err(error),
    };
    ensure!(bound_session(project,&snapshot,herdr_session)? == session_key, "coordinator binding changed while building context; retry");
    ensure!(snap.sequence == head, "project changed while building checkpoint; retry context");
    let constraints = constraint_lines(project, &mut memory, &snap).map_err(|e| anyhow::anyhow!("{e}"))?;
    let blocker_text = blockers(&snapshot);
    let mut body = String::new();
    if kind=="full" {
        body.push_str(&base);
        body.push('\n');
        body.push_str(&constraints);
        body.push('\n');
        body.push_str(&blocker_text);
    } else {
        body.push_str(&format!("Runtime owner: SQLite; event head {head}. Checkpoint kind=delta from_seq={from_seq}.\n\n"));
        body.push_str(&base);
        body.push('\n');
        body.push_str(&constraints);
        body.push('\n');
        body.push_str(&blocker_text);
        body.push_str("\n## Changes since last ack\n");
        let mut any=false;
        for event in snapshot.events.iter().filter(|e| e.sequence > from_seq) {
            any=true;
            body.push_str(&format!("- seq {} {} {}\n", event.sequence, event.kind, event.entity));
        }
        for task in &snapshot.tasks {
            any=true;
            body.push_str(&format!("- task {} revision {} {:?}: {}\n", task.id.as_str(), task.revision, task.state, task.title.replace(['\n','\r']," ")));
        }
        if !any { body.push_str("(none)\n"); }
    }
    let checkpoint_id = format!("chk-{:x}", Sha256::digest(format!("{}:{}:{}:{}:{}", session.id, snap.id.as_str(), head, now, new_coordinator_session()?).as_bytes()));
    let header = format!("Checkpoint {checkpoint_id} kind={kind} snapshot={} through_seq={head}. Session {herdr_session}. Continue with `context PROJECT --session {herdr_session}`. Acknowledge with `context PROJECT --session {herdr_session} --ack {checkpoint_id}`.\n\n", snap.id.as_str());
    let text = format!("{header}{body}");
    let count = text.chars().count() as u64;
    ensure!(count <= profile.budget_chars, "required {count} budget {}", profile.budget_chars);
    let full_chars = if kind=="full" { text.chars().count() as u64 } else { 0 };
    let delta_chars = if kind=="delta" { text.chars().count() as u64 } else { 0 };
    let row = CoordinatorCheckpoint {
        id: checkpoint_id.clone(), session_id: session.id.clone(), kind: kind.into(),
        snapshot_id: snap.id.as_str().into(), from_seq, through_seq: head,
        manifest_hash: snap.manifest_hash.clone(), full_chars, delta_chars, created_unix_ms: now, acked: false,
    };
    memory.store.insert_coordinator_checkpoint(&row).map_err(MemoryError::from).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(CoordinatorContext { text, head, unseen, checkpoint_id, kind: kind.into() })
}

pub fn ack_checkpoint(project: &Path, checkpoint_id: &str, session_token: &str) -> Result<CoordinatorCheckpoint> {
    let _guard=super::mutation_guard(project)?;
    let mut db = migration::open_active(project)?;
    let snapshot=db.read_snapshot(None)?;
    let key=bound_session(project,&snapshot,session_token)?;
    Ok(db.ack_coordinator_checkpoint(checkpoint_id,&key,snapshot.head)?)
}

pub fn last_checkpoint_sizes(project: &Path) -> Result<Option<CheckpointSizes>> {
    let mut db = migration::open_active(project)?;
    if db.read_snapshot(None)?.schema_version < 20 { return Ok(None); }
    Ok(db.last_checkpoint_sizes()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Applicability, ControlContext, MemoryKind, MemoryPolicyOp, MemoryRecordId, NewRevision, Task, TaskId, TaskState, Commit, Mutation};
    use std::fs;
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        let project = temp.path().join("demo");
        fs::create_dir(&project).unwrap();
        fs::create_dir(project.join(".state")).unwrap();
        fs::create_dir(project.join("threads")).unwrap();
        fs::create_dir(project.join("inbox")).unwrap();
        fs::write(project.join("PROJECT.md"), "+++\nname = 'Demo'\n+++\nInstructions\n").unwrap();
        fs::write(project.join("TASKS.md"), "# Tasks\n- [ ] Pending\n").unwrap();
        fs::write(project.join("MEMORY.md"), "# Memory\n").unwrap();
        fs::write(project.join(".state/project.json"), r#"{"status":"paused"}"#).unwrap();
        let plan = migration::inspect(&project).unwrap();
        migration::apply(&project, &plan, true).unwrap();
        (temp, project)
    }
    fn profile() -> CheckpointProfile {
        CheckpointProfile { name: "planner".into(), digest: "a".repeat(64), config_digest: None, budget_chars: 32_000 }
    }
    fn put_hard(project: &Path, id: &str, body: &[u8]) {
        let mut memory = MemoryStore::from_sqlite(migration::open_active(project).unwrap(), objects_dir(project));
        let body_id = memory.ingest_object(body).unwrap();
        let prov = memory.ingest_object(&b"prov"[..]).unwrap();
        memory.insert_revision(&ControlContext { now_unix_ms: 1_000 }, NewRevision {
            id: MemoryRecordId::new(id).unwrap(), record_key: format!("memory/{id}.md"), scope_id: "project".into(),
            kind: MemoryKind::Constraint, body_hash: body_id, provenance_hash: prov,
            applicability: Applicability { domains: vec![], paths: vec![format!("memory/{id}.md")] },
            dependencies: vec![], expected: None, expiry_unix_ms: None, validity_state: String::new(), validity_reason: String::new(),
        }).unwrap();
        let head = memory.store.read_snapshot(None).unwrap().head;
        memory.store.apply_memory_op(MemoryPolicyOp::HardRule, &format!("memory/{id}.md"), head).unwrap();
    }
    #[test]
    fn new_session_is_full_ack_then_delta_keeps_blockers_and_records_sizes() {
        let (_temp, project) = fixture();
        put_hard(&project, "rule", b"must not ship secrets");
        let mut db = migration::open_active(&project).unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        let blocked = Task { id: TaskId::new("blocked-1").unwrap(), revision: 1, state: TaskState::Blocked, title: "blocked work".into(), active_attempt: None };
        db.commit(Commit { expected_head: head, mutations: vec![Mutation::Task { expected: None, next: blocked }] }).unwrap();
        drop(db);
        let first = coordinator_context(&project, "sess-1", &profile(), "instructions").unwrap();
        assert_eq!(first.kind, "full");
        assert!(first.text.contains("must not ship secrets"));
        assert!(first.text.contains("blocked work"));
        assert!(first.text.contains("Checkpoint"));
        let sizes = last_checkpoint_sizes(&project).unwrap().unwrap();
        assert!(sizes.full_chars > 0);
        assert_eq!(sizes.delta_chars, 0);
        ack_checkpoint(&project, &first.checkpoint_id, "sess-1").unwrap();
        let second = coordinator_context(&project, "sess-1", &profile(), "instructions").unwrap();
        assert_eq!(second.kind, "delta");
        assert!(second.text.contains("blocked work"));
        assert!(second.text.contains("must not ship secrets"));
        let snap = {
            let mut db = migration::open_active(&project).unwrap();
            let row = db.coordinator_checkpoint(&first.checkpoint_id).unwrap().unwrap();
            db.read_memory_snapshot(&row.snapshot_id).unwrap()
        };
        assert_eq!(snap.task_id, "coordinator");
        let raw = rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
        let n: i64 = raw.query_row("SELECT COUNT(*) FROM tasks WHERE id='coordinator'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
        assert!(raw.query_row("SELECT 1 FROM coordinator_checkpoints WHERE snapshot_id=?1", [snap.id.as_str()], |r| r.get::<_,i64>(0)).is_ok());
    }
    #[test]
    fn failed_read_leaves_cursor_and_missing_checkpoint_falls_back_to_full() {
        let (_temp, project) = fixture();
        put_hard(&project, "rule", &vec![b'x'; 400]);
        let first = coordinator_context(&project, "sess-2", &profile(), "instructions").unwrap();
        ack_checkpoint(&project, &first.checkpoint_id, "sess-2").unwrap();
        let cursor = {
            let mut db = migration::open_active(&project).unwrap();
            db.coordinator_session(&coordinator_session_key(&project,"sess-2").unwrap()).unwrap().unwrap().cursor_seq
        };
        let tiny = CheckpointProfile { name: "planner".into(), digest: "a".repeat(64), config_digest: None, budget_chars: 10 };
        assert!(coordinator_context(&project, "sess-2", &tiny, "instructions").is_err());
        let after = {
            let mut db = migration::open_active(&project).unwrap();
            db.coordinator_session(&coordinator_session_key(&project,"sess-2").unwrap()).unwrap().unwrap()
        };
        assert_eq!(after.cursor_seq, cursor);
        assert!(after.last_checkpoint_id.is_some());
        {
            let raw = rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
            raw.execute("UPDATE coordinator_sessions SET last_checkpoint_id='missing-chk'", []).unwrap();
        }
        let fallback = coordinator_context(&project, "sess-2", &profile(), "instructions").unwrap();
        assert_eq!(fallback.kind, "full");
    }
    #[test]
    fn checkpoint_publication_rejects_mixed_heads_and_foreign_session_snapshots() {
        let (_temp, project) = fixture();
        let first = coordinator_context(&project, "session-a", &profile(), "instructions").unwrap();
        let mut db = migration::open_active(&project).unwrap();
        let original = db.coordinator_checkpoint(&first.checkpoint_id).unwrap().unwrap();
        let other = db.upsert_coordinator_session("session-b", 10).unwrap();
        let mut forged = original.clone();
        forged.id = "foreign-checkpoint".into();
        forged.session_id = other.id;
        assert!(db.insert_coordinator_checkpoint(&forged).is_err());
        assert!(db.coordinator_session("session-b").unwrap().unwrap().last_checkpoint_id.is_none());

        let head = db.read_snapshot(None).unwrap().head;
        db.commit(Commit { expected_head: head, mutations: vec![Mutation::Task {
            expected: None, next: Task { id: TaskId::new("concurrent").unwrap(), revision: 1,
                state: TaskState::Draft, title: "Changed during render".into(), active_attempt: None }
        }]}).unwrap();
        let mut stale = original.clone();
        stale.id = "stale-checkpoint".into();
        assert!(db.insert_coordinator_checkpoint(&stale).is_err());
        // Claiming the newer head cannot launder an older memory snapshot either.
        stale.through_seq = db.read_snapshot(None).unwrap().head;
        assert!(db.insert_coordinator_checkpoint(&stale).is_err());
        assert_eq!(db.coordinator_session(&coordinator_session_key(&project,"session-a").unwrap()).unwrap().unwrap().last_checkpoint_id.as_deref(), Some(original.id.as_str()));
        assert!(db.coordinator_checkpoint("stale-checkpoint").unwrap().is_none());
        let next = coordinator_context(&project, "session-a", &profile(), "instructions").unwrap();
        assert_eq!(next.head, stale.through_seq);
        assert!(next.text.contains("Changed during render"));
    }

    #[test]
    fn acknowledgments_require_current_session_generation_and_exact_identity() {
        let (_temp, project) = fixture();
        let first = coordinator_context(&project,"one",&profile(),"instructions").unwrap();
        let second = coordinator_context(&project,"two",&profile(),"instructions").unwrap();
        assert!(ack_checkpoint(&project,&first.checkpoint_id,"two").is_err());
        assert!(ack_checkpoint(&project,&second.checkpoint_id,"one").is_err());
        let mut db=migration::open_active(&project).unwrap();
        assert!(!db.coordinator_checkpoint(&first.checkpoint_id).unwrap().unwrap().acked);
        ack_checkpoint(&project,&first.checkpoint_id,"one").unwrap();
        let old_key=coordinator_session_key(&project,"one").unwrap();
        let old=db.coordinator_session(&old_key).unwrap().unwrap();
        let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
        raw.execute("UPDATE project_control SET epoch=epoch+1,revision=revision+1",[]).unwrap();
        assert!(ack_checkpoint(&project,&first.checkpoint_id,"one").is_err());
        assert_eq!(db.coordinator_session(&old_key).unwrap().unwrap(),old);
        let fresh=coordinator_context(&project,"one",&profile(),"instructions").unwrap();
        assert_eq!(fresh.kind,"full");
        ack_checkpoint(&project,&fresh.checkpoint_id,"one").unwrap();
        let replay=ack_checkpoint(&project,&fresh.checkpoint_id,"one").unwrap();
        assert!(replay.acked);
        assert_ne!(new_coordinator_session().unwrap(),new_coordinator_session().unwrap());
    }

    #[test]
    fn changed_profile_or_instructions_force_full_context() {
        let (_temp, project) = fixture();
        let initial=coordinator_context(&project,"session",&profile(),"instructions").unwrap();
        ack_checkpoint(&project,&initial.checkpoint_id,"session").unwrap();
        let mut changed=profile();changed.digest="b".repeat(64);
        let next=coordinator_context(&project,"session",&changed,"instructions").unwrap();
        assert_eq!(next.kind,"full");
        ack_checkpoint(&project,&next.checkpoint_id,"session").unwrap();
        let next=coordinator_context(&project,"session",&changed,"changed instructions").unwrap();
        assert_eq!(next.kind,"full");
    }

}
