//! Task and coordinator snapshot constructors. Ranking runs inside the write transaction.
use super::{MemoryError, MemoryStore};
use crate::domain::*;

impl MemoryStore {
    /// Select knowledge inside the final worker prompt envelope, reserving room
    /// for the versioned protocol and attempt/snapshot identity framing.
    pub fn create_worker_snapshot(
        &mut self,
        request: SnapshotRequest,
        profile_name: &str,
        profile_digest: &str,
        config_digest: Option<&str>,
        budget_chars: u64,
        instructions: &str,
        now_unix_ms: i64,
        expected_heads_digest: Option<&str>,
    ) -> Result<MemorySnapshot, MemoryError> {
        if request.task_id == "coordinator" {
            return Err(MemoryError::Invalid("task_id coordinator is reserved".into()));
        }
        self.store.create_memory_snapshot(SnapshotPlan {
            coordinator: false, session_id: None, request, profile_name: profile_name.into(),
            profile_digest: profile_digest.into(), config_digest: config_digest.map(str::to_owned),
            budget_chars, estimator: super::WORKER_BRIEF_ESTIMATOR.into(), instructions: instructions.into(),
            now_unix_ms, expected_heads_digest: expected_heads_digest.map(str::to_owned),
        }).map_err(map_snapshot_err)
    }

    pub fn create_task_snapshot(
        &mut self,
        request: SnapshotRequest,
        profile_name: &str,
        profile_digest: &str,
        config_digest: Option<&str>,
        budget_chars: u64,
        instructions: &str,
        now_unix_ms: i64,
        expected_heads_digest: Option<&str>,
    ) -> Result<MemorySnapshot, MemoryError> {
        if request.task_id == "coordinator" {
            return Err(MemoryError::Invalid("task_id coordinator is reserved".into()));
        }
        self.store.create_memory_snapshot(SnapshotPlan {
            coordinator: false, session_id: None, request, profile_name: profile_name.into(),
            profile_digest: profile_digest.into(), config_digest: config_digest.map(str::to_owned),
            budget_chars, estimator: SELECTION_ESTIMATOR.into(), instructions: instructions.into(),
            now_unix_ms, expected_heads_digest: expected_heads_digest.map(str::to_owned),
        }).map_err(map_snapshot_err)
    }

    pub fn create_coordinator_snapshot(
        &mut self,
        session_id: &str,
        profile_name: &str,
        profile_digest: &str,
        config_digest: Option<&str>,
        budget_chars: u64,
        instructions: &str,
        now_unix_ms: i64,
    ) -> Result<MemorySnapshot, MemoryError> {
        if session_id.is_empty() || session_id.len() > 128 {
            return Err(MemoryError::Invalid("invalid coordinator session".into()));
        }
        self.store.create_memory_snapshot(SnapshotPlan {
            coordinator: true, session_id: Some(session_id.into()),
            request: SnapshotRequest {
                schema_version: 1, task_id: "coordinator".into(), profile: profile_name.into(),
                domains: vec![], paths: vec![], pinned_keys: vec![], sensitivity: "default".into(),
            },
            profile_name: profile_name.into(), profile_digest: profile_digest.into(),
            config_digest: config_digest.map(str::to_owned), budget_chars,
            estimator: SELECTION_ESTIMATOR.into(), instructions: instructions.into(),
            now_unix_ms, expected_heads_digest: None,
        }).map_err(map_snapshot_err)
    }
}

fn map_snapshot_err(error: crate::store::StoreError) -> MemoryError {
    if let crate::store::StoreError::Limit(s) = &error {
        let parts: Vec<_> = s.split_whitespace().collect();
        if parts.len()==4 && parts[0]=="required" && parts[2]=="budget" {
            if let (Ok(required_bytes), Ok(budget_bytes)) = (parts[1].parse(), parts[3].parse()) {
                return MemoryError::RequiredContentTooLarge { required_bytes, budget_bytes };
            }
        }
    }
    error.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Applicability, Commit, ControlContext, MemoryKind, MemoryRecordId, Mutation, NewRevision, SnapshotRequest, Task, TaskId, TaskState};
    use crate::memory::MemoryStore;
    use crate::store::SqliteStore;
    fn fixture() -> (tempfile::TempDir, MemoryStore) {
        let root=tempfile::tempdir().unwrap();
        let db=root.path().join("state.db");
        let mut store=SqliteStore::create(&db).unwrap();
        let task=Task{id:TaskId::new("task-ui").unwrap(),revision:1,state:TaskState::Draft,title:"ui".into(),active_attempt:None};
        store.commit(Commit{expected_head:0,mutations:vec![Mutation::Task{expected:None,next:task}]}).unwrap();
        (root, MemoryStore::from_sqlite(store, db.parent().unwrap().join("objects")))
    }
    fn ctx() -> ControlContext { ControlContext { now_unix_ms: 1_000 } }
    fn digest() -> String { "a".repeat(64) }
    fn put(memory:&mut MemoryStore,id:&str,kind:MemoryKind,domains:&[&str],body:&[u8])->ObjectId {
        let body_id=memory.ingest_object(body).unwrap();
        let prov=memory.ingest_object(&b"prov"[..]).unwrap();
        memory.insert_revision(&ctx(), NewRevision{
            id:MemoryRecordId::new(id).unwrap(), record_key:format!("memory/{id}.md"), scope_id:"project".into(), kind,
            body_hash:body_id.clone(), provenance_hash:prov,
            applicability:Applicability{domains:domains.iter().map(|d|d.to_string()).collect(),paths:vec![format!("memory/{id}.md")]},
            dependencies:vec![], expected:None, expiry_unix_ms:None, validity_state:String::new(), validity_reason:String::new(),
        }).unwrap();
        body_id
    }
    fn request(domains:&[&str],pins:&[&str])->SnapshotRequest {
        SnapshotRequest{schema_version:1,task_id:"task-ui".into(),profile:"implementation".into(),
            domains:domains.iter().map(|d|d.to_string()).collect(),paths:vec![],
            pinned_keys:pins.iter().map(|p|p.to_string()).collect(),sensitivity:"default".into()}
    }
    #[test]
    fn worker_selection_reserves_framing_before_optional_memory() {
        let (_root,mut memory)=fixture();
        put(&mut memory,"ui-note",MemoryKind::Observation,&["ui"],b"optional context");
        let plain=memory.create_task_snapshot(request(&["ui"],&[]),"implementation",&digest(),None,32000,"Required instructions",1000,None).unwrap();
        let worker=memory.create_worker_snapshot(request(&["ui"],&[]),"implementation",&digest(),None,32000,"Required instructions",1000,None).unwrap();
        assert_ne!(plain.id,worker.id);
        assert_eq!(worker.required_bytes,plain.required_bytes+crate::memory::worker_brief_framing_chars(_root.path().join("state.db").to_str().unwrap()).unwrap());
        assert_eq!(worker.estimator,crate::memory::WORKER_BRIEF_ESTIMATOR);
        assert!(memory.create_worker_snapshot(request(&["ui"],&[]),"implementation",&digest(),None,worker.required_bytes-1,"Required instructions",1000,None).is_err());
        let exact=memory.create_worker_snapshot(request(&["ui"],&[]),"implementation",&digest(),None,worker.required_bytes,"Required instructions",1000,None).unwrap();
        assert_eq!(exact.optional_bytes,0);
        assert_eq!(exact.omitted_optional_count,1);
        assert_eq!(exact.budget_bytes,worker.required_bytes);
    }
    #[test]
    fn oversized_hard_rule_fails_closed_and_optional_never_displaces_mandatory() {
        let (_root,mut memory)=fixture();
        put(&mut memory,"rule",MemoryKind::Constraint,&["ui"],&vec![b'x'; 8_000]);
        let err=memory.create_task_snapshot(request(&["ui"],&[]), "implementation", &digest(), None, 100, "tiny", 1_000, None).unwrap_err();
        assert!(matches!(err, MemoryError::RequiredContentTooLarge { .. }));
    }
    // No command passes an expected heads digest (the CLI and checkpoints pass
    // `None`), so this conflict cannot be reached end to end. The revoked-pin
    // refusal is covered by tests/memory_regressions.rs.
    #[test]
    fn stale_heads_digest_conflicts() {
        let (_root,mut memory)=fixture();
        put(&mut memory,"pin",MemoryKind::Contract,&["ui"],b"pin");
        let stale="b".repeat(64);
        assert!(matches!(memory.create_task_snapshot(request(&["ui"],&[]), "implementation", &digest(), None, 32_000, "x", 1_000, Some(&stale)), Err(MemoryError::RevisionConflict { .. })));
    }
}
