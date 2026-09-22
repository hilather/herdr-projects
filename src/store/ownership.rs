use super::*;
use crate::reconcile::{RuntimeObservation,ResourceState};

pub(crate) fn identity_digest(binding:&RuntimeBinding)->Result<String> {Ok(format!("{:x}",Sha256::digest(serde_json::to_vec(&binding.identity).map_err(|e|StoreError::Invalid(e.to_string()))?)))}
pub(super) fn read_all(db:&Connection)->Result<Vec<RuntimeOwnership>> {read_all_with_budget(db,None)}
pub(super) fn read_all_with_budget(db:&Connection,budget:Option<&read_budget::ReadBudget>)->Result<Vec<RuntimeOwnership>> {
    let mut stmt=db.prepare("SELECT binding_id,revision,binding_revision,attempt_id,payload,payload_hash FROM runtime_ownership ORDER BY binding_id")?;
    let mut rows=stmt.query([])?;
    let mut result=Vec::new();
    while let Some(r)=rows.next()? {
        if let Some(budget)=budget {budget.row(r,&[(4,1)])?;}
        let values=(r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,u64>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?);
        let(id,revision,binding_revision,attempt,payload,hash)=values;if format!("{:x}",Sha256::digest(payload.as_bytes()))!=hash{return Err(StoreError::Corrupt("ownership payload hash mismatch".into()));}
        let owned:RuntimeOwnership=serde_json::from_str(&payload).map_err(|_|StoreError::Corrupt("invalid ownership payload".into()))?;
        if owned.binding!=id||owned.revision!=revision||owned.binding_revision!=binding_revision||owned.attempt.as_ref().map(AttemptId::as_str)!=attempt.as_deref()||!matches!(owned.origin.as_str(),"adopted"|"launched")||!crate::operations::finalization::hash(&owned.identity_digest)||owned.observed_unix_ms<0 {return Err(StoreError::Corrupt("ownership row identity mismatch".into()));}result.push(owned);
    }
    Ok(result)
}
pub(crate) fn observed(binding:&RuntimeBinding,task_revision:Option<u64>,observation:&RuntimeObservation,now:i64,config:Option<&str>)->bool {
    let has_pane=!binding.identity.pane_id.is_empty();let has_tree=!binding.identity.worktree_path.is_empty();
    binding.identity.machine.is_empty()&&observation.collector=="herdr-git-v2"&&observation.binding==binding.id&&observation.binding_revision==binding.revision&&observation.task_revision==task_revision&&observation.observed_unix_ms<=now&&now-observation.observed_unix_ms<=30_000&&observation.config_digest.as_deref()==config
        &&if has_pane {observation.pane==ResourceState::Present&&observation.agent_present&&observation.session_identity.is_some()&&observation.agent_identity.as_ref().is_some_and(|a|!a.kind.is_empty())}else{observation.pane==ResourceState::Unrecorded&&!observation.agent_present&&observation.session_identity.is_none()&&observation.agent_identity.is_none()}
        &&if has_tree {observation.worktree==ResourceState::Present&&observation.worktree_identity.is_some()}else{observation.worktree==ResourceState::Unrecorded&&observation.worktree_identity.is_none()}
}
pub(crate) fn matches(owned:&RuntimeOwnership,binding:&RuntimeBinding,observation:&RuntimeObservation)->Result<bool> {
    Ok(owned.binding==binding.id&&owned.binding_revision==binding.revision&&owned.identity_digest==identity_digest(binding)?&&owned.session==observation.session_identity&&owned.worktree==observation.worktree_identity&&owned.agent==observation.agent_identity&&owned.config_digest==observation.config_digest)
}
#[derive(Debug,serde::Serialize)]
pub struct OwnershipChange {pub head:u64,pub ownership:RuntimeOwnership,pub task_revision:Option<u64>}
impl SqliteStore {
    /// Caller holds the root execution lease and checks all known projects for
    /// conflicts. Live inspection occurs outside this atomic adoption transaction.
    pub fn adopt_runtime(&mut self,id:&str,expected_binding_revision:u64,expected_head:u64,now:i64,config:Option<&str>)->Result<OwnershipChange> {
        super::delivery::now_check(now)?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;let schema:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;if schema<9{return Err(StoreError::UnsupportedSchema(schema));}
        if head(&tx)?!=expected_head{return Err(StoreError::Conflict);}
        if super::control::read(&tx)?.state==ProjectState::Archived{return Err(StoreError::Invalid("restore archived project before adoption".into()));}
        let binding=super::runtime::read_all(&tx)?.into_iter().find(|b|b.id==id&&b.revision==expected_binding_revision).ok_or(StoreError::Conflict)?;
        RuntimeRoute::from_identity(&binding.identity).validate().map_err(StoreError::Invalid)?;
        if binding.identity.pane_id.is_empty()&&binding.identity.worktree_path.is_empty(){return Err(StoreError::Invalid("no resources to adopt".into()));}
        let mut task=binding.task.as_ref().map(|id|read_tasks(&tx)?.into_iter().find(|t|&t.id==id).ok_or(StoreError::Conflict)).transpose()?;
        let observation=super::observations::read_all(&tx)?.into_iter().find(|o|o.binding==id).ok_or(StoreError::Conflict)?;
        if !observed(&binding,task.as_ref().map(|t|t.revision),&observation,now,config){return Err(StoreError::Invalid("fresh exact local resource evidence required before adoption".into()));}
        let old=read_all(&tx)?.into_iter().find(|o|o.binding==id);
        if let Some(old)=&old {if matches(old,&binding,&observation)? {return Ok(OwnershipChange{head:expected_head,ownership:old.clone(),task_revision:task.map(|t|t.revision)});}}
        if let Some(task)=&task {
            if task.active_attempt.is_some()||matches!(task.state,TaskState::Running|TaskState::Succeeded|TaskState::Cancelled)||read_attempts(&tx)?.iter().any(|a|a.task==task.id&&a.retains_capacity()) {return Err(StoreError::Invalid("reconcile retained execution before adopting task resources".into()));}
        }
        // Claims may be explicitly relinquished; immutable adoption events keep
        // their generations from being reused after the active row is removed.
        let previous:u64=tx.query_row("SELECT COALESCE(MAX(revision),0) FROM events WHERE kind IN ('runtime.adopted','runtime.launched') AND entity=?1",[id],|r|r.get(0))?;
        let revision=previous.max(old.as_ref().map(|o|o.revision).unwrap_or(0)).checked_add(1).ok_or_else(||StoreError::Invalid("ownership revision exhausted".into()))?;
        let attempt=if observation.agent_present&&task.is_some() {Some(AttemptId::new(format!("adopt-{:x}",Sha256::digest(format!("{id}:{revision}").as_bytes()))).map_err(StoreError::Invalid)?)}else{None};
        if let (Some(task),Some(attempt))=(&task,&attempt) {
            let reservation=format!("pane:{:x}",Sha256::digest(serde_json::to_vec(&(&binding.identity.socket,&binding.identity.pane_id,&observation.session_identity)).map_err(|e|StoreError::Invalid(e.to_string()))?));
            let record=Attempt{id:attempt.clone(),task:task.id.clone(),revision:1,state:AttemptState::Running,snapshot:None,reservation,termination_observed:false};
            tx.execute("INSERT INTO attempts VALUES(?1,?2,1,'running',NULL,?3,0)",params![attempt.as_str(),task.id.as_str(),record.reservation])?;
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('attempt.adopted',?1,1,1,?2)",params![attempt.as_str(),serde_json::to_string(&record).map_err(|e|StoreError::Invalid(e.to_string()))?])?;
        }
        if let Some(task)=&mut task {
            task.revision=task.revision.checked_add(1).ok_or_else(||StoreError::Invalid("task revision exhausted".into()))?;
            if let Some(attempt)=&attempt {task.state=TaskState::Running;task.active_attempt=Some(attempt.clone());}
            tx.execute("UPDATE tasks SET revision=?2,state=?3,active_attempt=?4 WHERE id=?1",params![task.id.as_str(),integer(task.revision)?,task.state.as_str(),task.active_attempt.as_ref().map(AttemptId::as_str)])?;
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('task.changed',?1,?2,1,?3)",params![task.id.as_str(),integer(task.revision)?,serde_json::to_string(task).map_err(|e|StoreError::Invalid(e.to_string()))?])?;
        }
        let owned=RuntimeOwnership{binding:id.into(),revision,binding_revision:binding.revision,identity_digest:identity_digest(&binding)?,origin:"adopted".into(),attempt,session:observation.session_identity,worktree:observation.worktree_identity,agent:observation.agent_identity,config_digest:observation.config_digest,observed_unix_ms:observation.observed_unix_ms};
        let payload=serde_json::to_string(&owned).map_err(|e|StoreError::Invalid(e.to_string()))?;
        tx.execute("INSERT INTO runtime_ownership VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision,binding_revision=excluded.binding_revision,attempt_id=excluded.attempt_id,payload=excluded.payload,payload_hash=excluded.payload_hash",params![id,integer(revision)?,integer(binding.revision)?,owned.attempt.as_ref().map(AttemptId::as_str),payload,format!("{:x}",Sha256::digest(payload.as_bytes()))])?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.adopted',?1,?2,1,?3)",params![id,integer(revision)?,payload])?;
        super::control::invalidate(&tx)?;
        let result=OwnershipChange{head:head(&tx)?,ownership:owned,task_revision:task.map(|t|t.revision)};tx.commit()?;Ok(result)
    }
}

impl SqliteStore {
    /// Relinquishment withdraws authority only. It neither stops a process nor
    /// removes a resource, and never releases uncertain attempt capacity.
    pub fn relinquish_runtime(&mut self,id:&str,expected_revision:u64,expected_head:u64,reason:&str)->Result<u64> {
        if reason.trim().is_empty()||reason.len()>4000||reason.chars().any(char::is_control) {return Err(StoreError::Invalid("reason must contain 1–4000 bytes without control characters".into()));}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        let schema:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;if schema<9{return Err(StoreError::UnsupportedSchema(schema));}
        if head(&tx)?!=expected_head{return Err(StoreError::Conflict);}
        if super::control::read(&tx)?.state==ProjectState::Active {return Err(StoreError::Invalid("pause the project before relinquishing ownership".into()));}
        let owned=read_all(&tx)?.into_iter().find(|o|o.binding==id&&o.revision==expected_revision).ok_or(StoreError::Conflict)?;
        let binding=super::runtime::read_all(&tx)?.into_iter().find(|b|b.id==id&&b.revision==owned.binding_revision).ok_or(StoreError::Conflict)?;
        if owned.identity_digest!=identity_digest(&binding)? {return Err(StoreError::Conflict);}
        let attempts=read_attempts(&tx)?;
        if let Some(attempt)=&owned.attempt {
            let attempt=attempts.iter().find(|a|&a.id==attempt&&binding.task.as_ref()==Some(&a.task)).ok_or(StoreError::Conflict)?;
            if attempt.retains_capacity() {return Err(StoreError::Invalid("attempt termination remains unobserved; ownership retained".into()));}
        }
        if let Some(id)=&binding.task {
            let mut task=read_tasks(&tx)?.into_iter().find(|t|&t.id==id).ok_or(StoreError::Conflict)?;
            if task.active_attempt.is_some()||task.state==TaskState::Running||attempts.iter().any(|a|&a.task==id&&a.retains_capacity()) {return Err(StoreError::Invalid("reconcile every retained task attempt before relinquishing ownership".into()));}
            task.revision=task.revision.checked_add(1).ok_or_else(||StoreError::Invalid("task revision exhausted".into()))?;
            tx.execute("UPDATE tasks SET revision=?2 WHERE id=?1",params![id.as_str(),integer(task.revision)?])?;
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('task.changed',?1,?2,1,?3)",params![id.as_str(),integer(task.revision)?,serde_json::to_string(&task).map_err(|e|StoreError::Invalid(e.to_string()))?])?;
        }
        tx.execute("DELETE FROM runtime_ownership WHERE binding_id=?1",[id])?;
        tx.execute("DELETE FROM runtime_observations WHERE binding_id=?1",[id])?;
        let payload=serde_json::json!({"ownership":owned,"reason":reason,"resources_removed":false});
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.relinquished',?1,?2,1,?3)",params![id,integer(expected_revision)?,payload.to_string()])?;
        super::control::invalidate(&tx)?;let head=head(&tx)?;tx.commit()?;Ok(head)
    }
}
