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

fn constraint_lines(project: &Path, memory: &mut MemoryStore, snap: &MemorySnapshot) -> Result<String, MemoryError> {
    let mut out = String::from("## Mandatory constraints\n");
    let mut any = false;
    for entry in snap.entries.iter().filter(|e| e.role == "mandatory") {
        let rec = memory.store.memory_record(entry.record_id.as_str()).map_err(MemoryError::from)?
            .ok_or_else(|| MemoryError::Invalid("checkpoint snapshot record missing".into()))?;
        let rev = memory.store.memory_revision(entry.record_id.as_str(), entry.revision).map_err(MemoryError::from)?
            .ok_or_else(|| MemoryError::Invalid("checkpoint snapshot revision missing".into()))?;
        let path = objects_dir(project).join("sha256").join(&rev.body_hash.as_str()[..2]).join(rev.body_hash.as_str());
        let body = std::fs::read(&path).unwrap_or_default();
        let text = String::from_utf8_lossy(&body);
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
    let (text, head, unseen) = crate::runtime::context_snapshot(project)?;
    Ok((text, head, unseen, snapshot))
}

/// New/restarted/uncertain sessions get a full checkpoint. Deltas only after ack.
pub fn coordinator_context(project: &Path, herdr_session: &str, profile: &CheckpointProfile, instructions: &str) -> Result<CoordinatorContext> {
    ensure!(!profile.name.is_empty() && profile.digest.len()==64, "context requires a named profile digest");
    let now = jiff::Timestamp::now().as_millisecond();
    let mut db = migration::open_active(project)?;
    ensure!(db.read_snapshot(None)?.schema_version >= 20, "upgrade-store is required for coordinator checkpoints");
    let session = db.upsert_coordinator_session(herdr_session, now)?;
    let last = match session.last_checkpoint_id.as_deref() {
        Some(id) => db.coordinator_checkpoint(id)?,
        None => None,
    };
    let head = db.read_snapshot(None)?.head;
    let delta_ok = last.as_ref().is_some_and(|c| c.acked && session.cursor_seq <= head);
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
    let checkpoint_id = format!("chk-{:x}", Sha256::digest(format!("{}:{}:{}:{}", session.id, snap.id.as_str(), head, now).as_bytes()));
    let header = format!("Checkpoint {checkpoint_id} kind={kind} snapshot={} through_seq={head}. Acknowledge with `context PROJECT --ack {checkpoint_id}`.\n\n", snap.id.as_str());
    let text = format!("{header}{body}");
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

pub fn ack_checkpoint(project: &Path, checkpoint_id: &str) -> Result<CoordinatorCheckpoint> {
    let mut db = migration::open_active(project)?;
    Ok(db.ack_coordinator_checkpoint(checkpoint_id)?)
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
        ack_checkpoint(&project, &first.checkpoint_id).unwrap();
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
        ack_checkpoint(&project, &first.checkpoint_id).unwrap();
        let cursor = {
            let mut db = migration::open_active(&project).unwrap();
            db.coordinator_session("sess-2").unwrap().unwrap().cursor_seq
        };
        let tiny = CheckpointProfile { name: "planner".into(), digest: "a".repeat(64), config_digest: None, budget_chars: 10 };
        assert!(coordinator_context(&project, "sess-2", &tiny, "instructions").is_err());
        let after = {
            let mut db = migration::open_active(&project).unwrap();
            db.coordinator_session("sess-2").unwrap().unwrap()
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
}
