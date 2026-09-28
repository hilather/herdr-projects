use super::*;

pub(super) fn current(db:&Connection,id:&str,revision:u64,budget:Option<&read_budget::ReadBudget>)->Result<Option<(String,String,i64)>> {
    // Remove column affinity from the timestamp comparison so SQLite can use
    // the expression-index key, rather than scan every sample of this binding.
    read_budget::optional(db,
        "SELECT e.kind,e.entity,e.sequence FROM runtime_observations o CROSS JOIN events e INDEXED BY runtime_observation_versions ON e.entity=o.binding_id AND e.revision=o.binding_revision AND json_extract(e.payload,'$.observed_unix_ms')=+o.observed_unix_ms AND e.payload=o.payload WHERE o.binding_id=?1 AND o.binding_revision=?2 AND e.kind='runtime.observed' ORDER BY e.sequence DESC LIMIT 1",
        params![id,integer(revision)?],budget,&[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))
}

pub(super) fn validate_reference(db:&Connection,id:&str,revision:u64,generation:u64,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    let binding=super::super::runtime::read_binding(db,id,budget)?.ok_or(StoreError::Conflict)?;
    let owned=super::super::ownership::read_binding(db,id,budget)?.ok_or(StoreError::Conflict)?;
    if binding.revision!=revision || owned.binding_revision!=revision || owned.revision!=generation
        || !binding.identity.machine.is_empty() || (binding.identity.pane_id.is_empty()&&binding.identity.worktree_path.is_empty()) {
        return Err(invalid("runtime recovery requires the exact retained local ownership claim"));
    }
    Ok(())
}

pub(super) fn relevant(db:&Connection,id:&str,revision:u64,generation:u64,sequence:i64,budget:Option<&read_budget::ReadBudget>)->Result<bool> {
    let Some(binding)=super::super::runtime::read_binding(db,id,budget)? else {return Ok(false);};
    let Some(owned)=super::super::ownership::read_binding(db,id,budget)? else {return Ok(false);};
    if binding.revision!=revision || owned.binding_revision!=revision || owned.revision!=generation
        || (binding.identity.pane_id.is_empty()&&binding.identity.worktree_path.is_empty()) {return Ok(false);}
    let Some(observation)=super::super::observations::read_binding(db,id,budget)? else {return Ok(false);};
    let task_revision=binding.task.as_ref().map(|task|super::super::read_task_with_budget(db,task.as_str(),budget).map(|task|task.revision)).transpose()?;
    if !super::super::ownership::observed(&binding,task_revision,&observation,jiff::Timestamp::now().as_millisecond(),owned.config_digest.as_deref())
        || !super::super::ownership::matches(&owned,&binding,&observation)? {return Ok(false);}
    let event:Option<(String,u32)>=read_budget::optional(db,
        "SELECT payload,payload_version FROM events WHERE sequence=?1 AND kind='runtime.observed' AND entity=?2 AND revision=?3",
        params![sequence,id,integer(revision)?],budget,&[(0,1)],|row|Ok((row.get(0)?,row.get(1)?)))?;
    let Some((payload,version))=event else {return Ok(false);};
    if version!=1 || payload.len()>MAX_RECORD_BYTES {return Err(StoreError::Corrupt("invalid recovery observation event".into()));}
    let published:crate::reconcile::RuntimeObservation=serde_json::from_str(&payload).map_err(|_|StoreError::Corrupt("invalid recovery observation event".into()))?;
    Ok(published==observation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{domain::*,reconcile::{RuntimeObservation,ResourceState}};

    fn fixture()->(tempfile::TempDir,SqliteStore,RuntimeObservation,WaitTrigger) {
        let root=tempfile::tempdir().unwrap();let mut db=SqliteStore::create(&root.path().join("state.db")).unwrap();
        db.commit(Commit{expected_head:db.current_head().unwrap(),mutations:vec![Mutation::Task{expected:None,next:Task{id:TaskId::new("parent").unwrap(),revision:1,state:TaskState::Running,title:"parent".into(),active_attempt:None}}]}).unwrap();
        let route=RuntimeRoute{socket:"/tmp/recovery.sock".into(),workspace_id:"w".into(),tab_id:"t".into(),pane_id:"p".into(),cwd:"/tmp".into(),..Default::default()};
        let binding=db.create_runtime(None,None,db.current_head().unwrap(),&route).unwrap().binding;
        let now=jiff::Timestamp::now().as_millisecond();
        let observed=RuntimeObservation{binding:binding.id.clone(),binding_revision:binding.revision,observed_unix_ms:now-1000,
            pane:ResourceState::Present,agent_present:true,collector:"herdr-git-v2".into(),session_identity:Some(ResourceIdentity{device:1,inode:2,born_secs:3,born_nanos:0}),
            agent_identity:Some(AgentIdentity{kind:"fixture".into(),name:"agent".into()}),..Default::default()};
        db.record_observations(db.current_head().unwrap(),std::slice::from_ref(&observed)).unwrap();
        let owner=db.adopt_runtime(&binding.id,binding.revision,db.current_head().unwrap(),now,None).unwrap().ownership;
        let trigger=WaitTrigger::OwnedRuntimeRecovered{binding_id:binding.id,binding_revision:binding.revision,ownership_revision:owner.revision};
        (root,db,observed,trigger)
    }

    #[test]
    fn recovery_wait_refuses_stale_future_and_mismatched_observations() {
        for mode in ["stale","future","session","agent","config","unknown","legacy"] {
            let (_root,mut db,mut observed,trigger)=fixture();
            match mode {
                "stale"=>{ // Model retained evidence from a controller that has been stopped.
                    observed.observed_unix_ms-=60_000;
                    db.connection.execute("DELETE FROM runtime_observations",[]).unwrap();
                }
                "future"=>observed.observed_unix_ms+=60_000,
                "session"=>observed.session_identity.as_mut().unwrap().inode+=1,
                "agent"=>observed.agent_identity.as_mut().unwrap().name="replacement".into(),
                "config"=>observed.config_digest=Some("a".repeat(64)),
                "legacy"=>{observed.collector="herdr-git-v1".into();observed.session_identity=None;observed.agent_identity=None;}
                _=>{observed.pane=ResourceState::Unknown;observed.agent_present=false;observed.session_identity=None;observed.agent_identity=None;}
            }
            db.record_observations(db.current_head().unwrap(),&[observed]).unwrap();
            let wait=db.register_wait_with_trigger("parent",None,"adapter_recovery",None,Some(&trigger)).unwrap();
            assert!(!db.replay_wait(&wait.wait_id).unwrap().wake_requested,"{mode}");
        }
    }


    #[test]
    fn recovery_wait_uses_indexed_current_publication_despite_late_old_events() {
        use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
        let (_root,mut db,observed,trigger)=fixture();
        let expected=current(&db.connection,&observed.binding,observed.binding_revision,None).unwrap().unwrap();
        let mut work=Vec::new();
        for history in [false,true] {
            if history {
                db.connection.execute("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO events(kind,entity,revision,payload_version,payload) SELECT 'runtime.observed',?1,1,1,json_object('observed_unix_ms',x) FROM n",[&observed.binding]).unwrap();
            }
            let steps=Arc::new(AtomicUsize::new(0));let counter=steps.clone();
            db.connection.progress_handler(1,Some(move||{counter.fetch_add(1,Ordering::Relaxed);false}));
            assert_eq!(current(&db.connection,&observed.binding,observed.binding_revision,None).unwrap(),Some(expected.clone()));
            db.connection.progress_handler(0,None::<fn()->bool>);work.push(steps.load(Ordering::Relaxed));
        }
        eprintln!("recovery publication lookup SQL steps at 0/10000 old samples: {work:?}");
        assert!(work[1]<=work[0]+100,"historical observations increased selection work: {work:?}");
        let wait=db.register_wait_with_trigger("parent",None,"adapter_recovery",None,Some(&trigger)).unwrap();
        assert!(db.replay_wait(&wait.wait_id).unwrap().wake_requested);
    }

    #[test]
    fn recovery_wait_rejects_corruption_and_changed_ownership_generation() {
        let (_root,mut db,good,trigger)=fixture();
        // Force registration to await a subsequent publication.
        db.connection.execute("DELETE FROM runtime_observations",[]).unwrap();
        let wait=db.register_wait_with_trigger("parent",None,"adapter_recovery",None,Some(&trigger)).unwrap();
        db.record_observations(db.current_head().unwrap(),std::slice::from_ref(&good)).unwrap();
        let head=db.current_head().unwrap();
        db.connection.execute("UPDATE runtime_observations SET payload_hash=?1",["0".repeat(64)]).unwrap();
        assert!(db.replay_wait(&wait.wait_id).is_err());assert_eq!(db.current_head().unwrap(),head);
        let payload=serde_json::to_string(&good).unwrap();
        db.connection.execute("UPDATE runtime_observations SET payload_hash=?1",[sha256_hex(payload.as_bytes())]).unwrap();
        let mut owned=super::super::super::ownership::read_binding(&db.connection,&good.binding,None).unwrap().unwrap();
        owned.revision+=1;let payload=serde_json::to_string(&owned).unwrap();
        db.connection.execute("UPDATE runtime_ownership SET revision=?1,payload=?2,payload_hash=?3",params![owned.revision,payload,sha256_hex(payload.as_bytes())]).unwrap();
        assert!(!db.replay_wait(&wait.wait_id).unwrap().wake_requested);
        assert!(db.register_wait_with_trigger("parent",None,"adapter_recovery",Some(1),Some(&trigger)).is_err());
    }
}
