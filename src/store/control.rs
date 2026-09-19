use super::*;
use crate::{operations::DeliveryState,reconcile::ResourceState};

pub(super) fn read(db:&Connection)->Result<ProjectControl> {
    let (revision,epoch,state,required,config_digest):(u64,u64,String,bool,Option<String>)=db.query_row("SELECT revision,epoch,state,reconciliation_required,config_digest FROM project_control WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    if config_digest.as_ref().is_some_and(|d|d.len()!=64||!d.bytes().all(|b|b.is_ascii_hexdigit())) {return Err(StoreError::Corrupt("invalid control config fingerprint".into()));}
    Ok(ProjectControl{revision,epoch,state:match state.as_str(){"paused"=>ProjectState::Paused,"active"=>ProjectState::Active,"archived"=>ProjectState::Archived,_=>return Err(StoreError::Corrupt("invalid project control state".into()))},reconciliation_required:required,config_digest})
}
pub(super) fn import_status(db:&Connection)->Result<()> {
    let sources=super::import::read_sources(db)?;
    if let Some(source)=sources.iter().find(|s|s.path==".state/project.json") {
        let value:serde_json::Value=serde_json::from_slice(&source.bytes).map_err(|_|StoreError::Corrupt("invalid project lifecycle provenance".into()))?;
        let state=value["status"].as_str().filter(|s|matches!(*s,"paused"|"archived")).ok_or_else(||StoreError::Corrupt("unsupported imported lifecycle state".into()))?;
        db.execute("UPDATE project_control SET state=?1 WHERE singleton=1",[state])?;
    }
    Ok(())
}
fn schema(db:&Connection)->Result<()> {
    check_schema(db)?;let version:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;
    if version<7{return Err(StoreError::UnsupportedSchema(version));}Ok(())
}
fn increment(n:u64)->Result<u64>{n.checked_add(1).filter(|n|*n<=i64::MAX as u64).ok_or_else(||StoreError::Invalid("control counter exhausted".into()))}
fn write(db:&Connection,control:&ProjectControl,kind:&str)->Result<()> {
    db.execute("UPDATE project_control SET revision=?1,epoch=?2,state=?3,reconciliation_required=?4,config_digest=?5 WHERE singleton=1",params![integer(control.revision)?,integer(control.epoch)?,control.state.as_str(),control.reconciliation_required,control.config_digest])?;
    db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,'project',?2,1,?3)",params![kind,integer(control.revision)?,serde_json::to_string(control).map_err(|e|StoreError::Invalid(e.to_string()))?])?;Ok(())
}
fn blockers(db:&Connection,now:i64,config:Option<&str>)->Result<Vec<String>> {
    super::delivery::now_check(now)?;
    if config.is_some_and(|d|d.len()!=64||!d.bytes().all(|b|b.is_ascii_hexdigit())) {return Err(StoreError::Invalid("invalid config fingerprint".into()));}
    let mut reasons=Vec::new();
    if read(db)?.state==ProjectState::Archived {reasons.push("restore archived project to paused before resuming".into());}
    let tasks=read_tasks(db)?;
    if tasks.iter().any(|t|t.active_attempt.is_some()||t.state==TaskState::Running) {reasons.push("task has active or unresolved execution".into());}
    if read_attempts(db)?.iter().any(|a|a.retains_capacity()) {reasons.push("attempt termination remains unobserved; capacity retained".into());}
    if super::delivery::read_all(db)?.iter().any(|d|!matches!(d.state,DeliveryState::Confirmed|DeliveryState::PermanentFailure)) {reasons.push("unfinished delivery intents require drain, observation or explicit retirement".into());}
    let observations=super::observations::read_all(db)?;
    for binding in super::runtime::read_all(db)? {
        if !binding.identity.pane_id.is_empty()||!binding.identity.worktree_path.is_empty()||!binding.identity.machine.is_empty() {
            reasons.push(format!("{}: live resource ownership/adoption is not established",binding.id));continue;
        }
        let task_revision=binding.task.as_ref().and_then(|id|tasks.iter().find(|t|&t.id==id).map(|t|t.revision));
        let valid=observations.iter().any(|o|o.binding==binding.id&&o.binding_revision==binding.revision&&o.task_revision==task_revision&&o.observed_unix_ms<=now&&now-o.observed_unix_ms<=30_000&&o.config_digest.as_deref()==config&&o.pane==ResourceState::Unrecorded&&o.worktree==ResourceState::Unrecorded&&!o.agent_present);
        if !valid {reasons.push(format!("{}: fresh matching observation required",binding.id));}
    }
    Ok(reasons)
}
impl SqliteStore {
    pub fn project_control(&self)->Result<Option<ProjectControl>> {
        check_schema(&self.connection)?;let version:u32=self.connection.query_row("PRAGMA user_version",[],|r|r.get(0))?;
        if version<7 {return Ok(None);}Ok(Some(read(&self.connection)?))
    }

    pub fn admission_report(&mut self,now:i64,config:Option<&str>)->Result<AdmissionReport> {
        let tx=self.connection.transaction()?;schema(&tx)?;let report=AdmissionReport{head:head(&tx)?,blockers:blockers(&tx,now,config)?};tx.commit()?;Ok(report)
    }
    /// Lifecycle changes are atomic with their audit and fence epoch. Resume is
    /// currently supported only when no existing resources need adoption.
    pub fn set_project_state(&mut self,expected_head:u64,expected_revision:u64,state:ProjectState,now:i64,config:Option<&str>)->Result<ControlChange> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        if head(&tx)?!=expected_head{return Err(StoreError::Conflict);}
        let mut control=read(&tx)?;if control.revision!=expected_revision{return Err(StoreError::Conflict);}
        if state==ProjectState::Active {
            if control.state==ProjectState::Archived{return Err(StoreError::Invalid("restore archived project to paused before resuming".into()));}
            let reasons=blockers(&tx,now,config)?;
            if !reasons.is_empty(){return Err(StoreError::Invalid(reasons.join("; ")));}
        }
        let required=state!=ProjectState::Active;
        let digest=if required{None}else{config.map(String::from)};
        if control.state!=state||control.reconciliation_required!=required||control.config_digest!=digest {
            control.state=state;control.reconciliation_required=required;control.config_digest=digest;control.revision=increment(control.revision)?;control.epoch=increment(control.epoch)?;write(&tx,&control,"project.control_changed")?;
        }
        let result=ControlChange{head:head(&tx)?,control};tx.commit()?;Ok(result)
    }
    /// Adapters check this immediately before an effect, in addition to task and
    /// binding fences and retained resource ownership. An active state alone is
    /// not permission to adopt a pane or bypass scoped command authority.
    pub fn validate_control_epoch(&mut self,epoch:u64,config:Option<&str>)->Result<()> {
        schema(&self.connection)?;let control=read(&self.connection)?;
        if control.epoch!=epoch||control.state!=ProjectState::Active||control.reconciliation_required||control.config_digest.as_deref()!=config{return Err(StoreError::Conflict);}Ok(())
    }
}

pub(super) fn invalidate(db:&Connection)->Result<()> {
    let version:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;
    if version>=7 {
        let mut control=read(db)?;
        control.reconciliation_required=true;control.config_digest=None;
        if control.state==ProjectState::Active{control.state=ProjectState::Paused;}
        control.revision=increment(control.revision)?;control.epoch=increment(control.epoch)?;write(db,&control,"project.reconciliation_invalidated")?;
    }
    Ok(())
}
