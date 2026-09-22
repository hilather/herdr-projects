//! Capacity reservation and cancellation transactions. No external effect runs here.
use super::*;
use crate::operations::{DeliveryState,Outcome};
use std::collections::{BTreeMap,BTreeSet};
fn invalid(s:&str)->StoreError {StoreError::Invalid(s.into())}
fn schema(db:&Connection)->Result<()> {check_schema(db)?;let n:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;if n<11{return Err(StoreError::UnsupportedSchema(n));}Ok(())}
fn hash(s:&str)->bool {s.len()==64&&s.bytes().all(|c|c.is_ascii_hexdigit())}
fn reference(r:&VersionedReference)->bool {!r.id.is_empty()&&r.id.len()<=512&&!r.id.chars().any(char::is_control)&&r.revision>0&&hash(&r.digest)}
pub(super) fn validate_inputs(i:&LaunchInputs)->Result<()> {
    if !matches!(i.version,1|2)||!Path::new(&i.project_store).is_absolute()||i.task_revision==0||i.scheduler_revision==0||i.control_epoch==0||i.binding.is_empty()||i.binding.len()>512||i.binding_revision==0||!hash(&i.binding_digest)||!reference(&i.profile)||!reference(&i.approval)||!Path::new(&i.config.path).is_absolute()||i.config.digest.as_ref().is_some_and(|d|!hash(d)) {return Err(invalid("invalid sealed launch inputs"));}
    match (i.version,&i.effective_profile) {
        (1,None)=>{},
        (2,Some(profile))=>{
            profile.validate_for_launch().map_err(|s|invalid(&s))?;
            if profile.config!=i.config||profile.reference().map_err(|s|invalid(&s))?!=i.profile {return Err(invalid("effective profile binding mismatch"));}
        },
        _=>return Err(invalid("launch input version/profile mismatch")),
    }
    if i.dependencies.len()>256||i.repositories.len()>64||i.memory.as_ref().is_some_and(|r|!reference(r))||i.budget.as_ref().is_some_and(|r|!reference(r)){return Err(invalid("invalid input references"));}
    let mut repos=BTreeSet::new();for r in &i.repositories {if !Path::new(&r.repository).is_absolute()||!repos.insert(&r.repository)||![&r.commit,&r.tree].iter().all(|s|matches!(s.len(),40|64)&&s.bytes().all(|c|c.is_ascii_hexdigit())) {return Err(invalid("invalid repository input vector"));}}
    let mut tasks=BTreeSet::new();for d in &i.dependencies {if d.task==i.task||d.task_revision==0||!reference(&d.evidence)||!tasks.insert(&d.task){return Err(invalid("invalid dependency input vector"));}}
    Ok(())
}
pub(crate) fn record_ids(i:&LaunchInputs)->Result<(AttemptId,OperationId)> {
    let digest=format!("{:x}",Sha256::digest(serde_json::to_vec(i).map_err(|e|invalid(&e.to_string()))?));
    Ok((AttemptId::new(format!("attempt-{digest}")).map_err(StoreError::Invalid)?,OperationId::new(format!("launch-{digest}")).map_err(StoreError::Invalid)?))
}
pub(super) fn read_inputs(db:&Connection)->Result<Vec<AttemptInputRecord>> {read_inputs_with_budget(db,None)}
pub(super) fn read_inputs_with_budget(db:&Connection,budget:Option<&read_budget::ReadBudget>)->Result<Vec<AttemptInputRecord>> {
    let broken:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM attempt_inputs i LEFT JOIN attempts a ON a.id=i.attempt_id LEFT JOIN operations o ON o.id=i.operation_id WHERE a.id IS NULL OR o.id IS NULL) OR EXISTS(SELECT 1 FROM operations o LEFT JOIN attempt_inputs i ON i.operation_id=o.id WHERE o.kind='runtime.launch' AND (i.attempt_id IS NULL OR o.payload_version<>1 OR o.idempotency_key<>o.id))",[],|r|r.get(0))?;
    if broken {return Err(StoreError::Corrupt("launch input inventory mismatch".into()));}
    let mut stmt=db.prepare("SELECT i.attempt_id,i.operation_id,i.payload,i.payload_hash,a.task_id,o.task_id,o.kind,o.target,o.expected_revision,o.payload,o.payload_hash FROM attempt_inputs i JOIN attempts a ON a.id=i.attempt_id JOIN operations o ON o.id=i.operation_id ORDER BY i.attempt_id")?;
    let mut rows=stmt.query([])?;
    let mut result=Vec::new();
    while let Some(r)=rows.next()? {
        if let Some(budget)=budget {
            use rusqlite::types::ValueRef;
            let payload_ref=match r.get_ref(2)? {ValueRef::Text(b)|ValueRef::Blob(b)=>b,_=>&[]};
            let op_ref=match r.get_ref(9)? {ValueRef::Text(b)|ValueRef::Blob(b)=>b,_=>&[]};
            if payload_ref==op_ref {budget.row(r,&[(2,2)])?;} else {budget.row(r,&[(2,2),(9,2)])?;}
        }
        let(a,o,payload,digest,task,op_task,kind,target,revision,op_payload,op_digest)=(r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?,r.get::<_,u64>(8)?,r.get::<_,String>(9)?,r.get::<_,String>(10)?);
        let record:AttemptInputRecord=serde_json::from_str(&payload).map_err(|_|StoreError::Corrupt("invalid attempt input payload".into()))?;validate_inputs(&record.inputs)?;
        let(expected_attempt,expected_op)=record_ids(&record.inputs)?;
        if record.attempt.as_str()!=a||record.operation.as_str()!=o||record.attempt!=expected_attempt||record.operation!=expected_op||record.inputs.task.as_str()!=task||task!=op_task||kind!="runtime.launch"||target!=record.inputs.binding||Some(revision)!=record.inputs.task_revision.checked_add(1)||payload!=op_payload||digest!=op_digest||format!("{:x}",Sha256::digest(payload.as_bytes()))!=digest {return Err(StoreError::Corrupt("attempt/launch input binding mismatch".into()));}
        result.push(record);
    }
    Ok(result)
}
pub(super) fn read_cancellations(db:&Connection)->Result<Vec<CancellationRequest>> {read_cancellations_with_budget(db,None)}
pub(super) fn read_cancellations_with_budget(db:&Connection,budget:Option<&read_budget::ReadBudget>)->Result<Vec<CancellationRequest>> {
    let mut stmt=db.prepare("SELECT attempt_id,requested_unix_ms,reason FROM attempt_cancellations ORDER BY attempt_id")?;
    let mut rows=stmt.query([])?;
    let mut result=Vec::new();
    while let Some(r)=rows.next()? {
        if let Some(budget)=budget {budget.row(r,&[])?;}
        result.push(CancellationRequest{attempt:AttemptId::new(r.get::<_,String>(0)?).map_err(StoreError::Corrupt)?,requested_unix_ms:r.get(1)?,reason:r.get(2)?});
    }
    Ok(result)
}
fn event(db:&Connection,kind:&str,id:&str,revision:u64,payload:&impl serde::Serialize)->Result<()> {db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,?2,?3,1,?4)",params![kind,id,integer(revision)?,serde_json::to_string(payload).map_err(|e|invalid(&e.to_string()))?])?;Ok(())}

impl SqliteStore {
    /// Select the oldest/aged highest-priority trusted preparation. Approval,
    /// capacity and all mutable input checks stay inside the transaction.
    pub fn reserve_prepared(&mut self,prepared:&[PreparedLaunch],expected_head:u64,now:i64)->Result<Reservation> {
        self.admit_prepared(prepared,expected_head,now,false)?.ok_or_else(||invalid("reservation missing"))
    }

    /// A draft checks the same admission conditions without requiring an approval
    /// that cannot be signed until the exact action has been constructed.
    /// This path returns before any task, attempt, event or operation is written.
    pub(crate) fn validate_launch_draft(&mut self,inputs:&LaunchInputs,expected_head:u64,now:i64)->Result<()> {
        self.admit_prepared(&[PreparedLaunch{inputs:inputs.clone()}],expected_head,now,true)?;
        Ok(())
    }

    fn admit_prepared(&mut self,prepared:&[PreparedLaunch],expected_head:u64,now:i64,draft:bool)->Result<Option<Reservation>> {
        super::delivery::now_check(now)?;if prepared.is_empty()||prepared.len()>128{return Err(invalid("reservation requires 1–128 ready preparations"));}
        let path=std::fs::canonicalize(self.connection.path().ok_or_else(||invalid("store path missing"))?).map_err(|e|StoreError::Io(e.to_string()))?;
        let mut seen=BTreeSet::new();for p in prepared {validate_inputs(&p.inputs)?;if p.inputs.version!=2 {return Err(invalid("new reservations require effective profile evidence"));}if Path::new(&p.inputs.project_store)!=path||!seen.insert(&p.inputs.task){return Err(invalid("preparation belongs to another store or duplicates a task"));}}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        let version:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;if version<13{return Err(StoreError::UnsupportedSchema(version));}
        if head(&tx)?!=expected_head{return Err(StoreError::Conflict);}
        let scheduler=super::scheduler::read(&tx)?;let control=super::control::read(&tx)?;
        if control.state!=ProjectState::Active||control.reconciliation_required{return Err(invalid("project is not admitted"));}
        let tasks=read_tasks(&tx)?;let attempts=read_attempts(&tx)?;let bindings=super::runtime::read_all(&tx)?;let ownership=super::ownership::read_all(&tx)?;
        if attempts.iter().filter(|a|a.retains_capacity()).count()>=scheduler.policy.max_active_workers as usize{return Err(invalid("project worker capacity is full"));}
        let queued:BTreeMap<_,_>=scheduler.queue.iter().map(|q|(&q.task,q)).collect();let mut ranked=Vec::new();
        for preparation in prepared {
            let i=&preparation.inputs;if i.scheduler_revision!=scheduler.policy.revision||i.control_epoch!=control.epoch||i.config.digest!=control.config_digest{return Err(StoreError::Conflict);}
            if !i.dependencies.is_empty(){return Err(invalid("dependency evidence producers are not available"));}
            super::worker_knowledge::validate(&tx,i,now)?;
            super::budget::check(&tx,i.budget.as_ref(),false)?;
            if !draft {super::approvals::validate_preparation(&tx,i,now)?;}
            let task=tasks.iter().find(|t|t.id==i.task&&t.revision==i.task_revision).ok_or(StoreError::Conflict)?;let queue=queued.get(&task.id).ok_or(StoreError::Conflict)?;
            if task.state!=TaskState::Queued||task.active_attempt.is_some()||!queue.dependencies.is_empty()||queue.enqueued_unix_ms>now||attempts.iter().any(|a|a.task==task.id&&a.retains_capacity()) {return Err(invalid("task is not ready for reservation"));}
            if attempts.iter().filter(|a|a.task==task.id).count()>=scheduler.policy.max_attempts_per_task as usize{return Err(invalid("task attempt limit reached"));}
            let binding=bindings.iter().find(|b|b.id==i.binding&&b.revision==i.binding_revision&&b.task.as_ref()==Some(&task.id)).ok_or(StoreError::Conflict)?;
            if super::ownership::identity_digest(binding)?!=i.binding_digest{return Err(StoreError::Conflict);}
            if !binding.identity.agent.is_empty()&&i.effective_profile.as_ref().map(|p|p.kind.as_str())!=Some(binding.identity.agent.as_str()) {return Err(invalid("profile kind differs from runtime binding"));}
            // Existing worker identities cannot be turned into a new launch.
            if !binding.identity.pane_id.is_empty()||ownership.iter().any(|o|o.binding==binding.id&&(o.attempt.is_some()||o.session.is_some()||o.agent.is_some())) {return Err(invalid("existing worker identity requires reconciliation"));}
            if !binding.identity.repo.is_empty()&&!i.repositories.iter().any(|r|r.repository==binding.identity.repo){return Err(invalid("recorded repository lacks a pinned input"));}
            let score=(now-queue.enqueued_unix_ms)/60_000+queue.priority as i64;ranked.push((score,queue.enqueue_sequence,&preparation.inputs));
        }
        if draft {return Ok(None);}
        ranked.sort_by(|a,b|b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.task.cmp(&b.2.task)));let inputs=ranked[0].2.clone();let(attempt_id,operation_id)=record_ids(&inputs)?;
        let task_revision=inputs.task_revision.checked_add(1).ok_or_else(||invalid("task revision exhausted"))?;
        let mut task=tasks.iter().find(|t|t.id==inputs.task).cloned().ok_or(StoreError::Conflict)?;task.revision=task_revision;task.state=TaskState::Running;task.active_attempt=Some(attempt_id.clone());
        let record=AttemptInputRecord{attempt:attempt_id.clone(),operation:operation_id.clone(),inputs};let payload=serde_json::to_string(&record).map_err(|e|invalid(&e.to_string()))?;if payload.len()>MAX_RECORD_BYTES{return Err(invalid("attempt inputs exceed 1 MiB"));}
        let digest=format!("{:x}",Sha256::digest(payload.as_bytes()));let attempt=Attempt{id:attempt_id.clone(),task:record.inputs.task.clone(),revision:1,state:AttemptState::Reserved,snapshot:record.inputs.memory.as_ref().map(|r|r.id.clone()),reservation:format!("worker:{}",attempt_id.as_str()),termination_observed:false};
        tx.execute("INSERT INTO attempts VALUES(?1,?2,1,'reserved',?4,?3,0)",params![attempt_id.as_str(),attempt.task.as_str(),attempt.reservation,attempt.snapshot])?;
        tx.execute("UPDATE tasks SET revision=?2,state='running',active_attempt=?3 WHERE id=?1",params![attempt.task.as_str(),integer(task_revision)?,attempt_id.as_str()])?;
        tx.execute("INSERT INTO operations VALUES(?1,?2,'runtime.launch',?3,1,?4,?5,?6,?7,?1)",params![operation_id.as_str(),attempt.task.as_str(),record.inputs.binding,payload,digest,integer(task_revision)?,now])?;
        tx.execute("INSERT INTO attempt_inputs VALUES(?1,?2,?3,?4)",params![attempt_id.as_str(),operation_id.as_str(),payload,digest])?;
        event(&tx,"attempt.reserved",attempt_id.as_str(),1,&record)?;event(&tx,"task.changed",attempt.task.as_str(),task_revision,&task)?;
        event(&tx,"operation.enqueued",operation_id.as_str(),task_revision,&record)?;
        let result=Reservation{head:head(&tx)?,record,task_revision};
        #[cfg(test)]
        tests::crash_boundary("before_reservation_commit");
        tx.commit()?;Ok(Some(result))
    }

    pub fn cancel_attempt(&mut self,id:&AttemptId,expected_revision:u64,expected_head:u64,reason:&str,now:i64)->Result<CancellationChange> {
        super::delivery::now_check(now)?;if reason.trim().is_empty()||reason.len()>4000||reason.chars().any(char::is_control){return Err(invalid("cancellation reason must contain 1–4000 bytes without control characters"));}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;if head(&tx)?!=expected_head{return Err(StoreError::Conflict);}
        let attempts=read_attempts(&tx)?;let mut attempt=attempts.iter().find(|a|&a.id==id&&a.revision==expected_revision).cloned().ok_or(StoreError::Conflict)?;
        let mut task=read_tasks(&tx)?.into_iter().find(|t|t.id==attempt.task).ok_or(StoreError::Conflict)?;
        if let Some(old)=read_cancellations(&tx)?.into_iter().find(|r|&r.attempt==id) {
            if old.reason!=reason{return Err(StoreError::Conflict);}
            return Ok(CancellationChange{head:expected_head,attempt:id.clone(),released:!attempt.retains_capacity(),attempt_revision:attempt.revision,task_revision:task.revision});
        }
        if !attempt.retains_capacity(){return Err(invalid("attempt already has termination evidence"));}
        let record=read_inputs(&tx)?.into_iter().find(|r|&r.attempt==id);
        let mut released=false;
        if let Some(record)=&record {
            let delivery=super::delivery::delivery(&tx,&record.operation)?;let bindings=super::runtime::read_all(&tx)?;let owned=super::ownership::read_all(&tx)?;
            let binding=bindings.iter().find(|b|b.id==record.inputs.binding&&b.revision==record.inputs.binding_revision&&b.task.as_ref()==Some(&attempt.task));
            let no_external_worker=binding.is_some_and(|b|b.identity.pane_id.is_empty())&&!owned.iter().any(|o|o.attempt.as_ref()==Some(id)||o.binding==record.inputs.binding&&(o.attempt.is_some()||o.agent.is_some()||o.session.is_some()));
            let matching_binding=binding.map(|b|super::ownership::identity_digest(b).map(|d|d==record.inputs.binding_digest)).transpose()?.unwrap_or(false);
            let other_retained=attempts.iter().any(|a|a.task==attempt.task&&a.id!=attempt.id&&a.retains_capacity());
            let prior_claim:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE kind='operation.claimed' AND entity=?1)",[record.operation.as_str()],|r|r.get(0))?;
            let another_launch:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations o JOIN operation_delivery d ON d.operation_id=o.id WHERE o.task_id=?1 AND o.kind='runtime.launch' AND o.id<>?2 AND d.state NOT IN ('confirmed','permanent_failure'))",params![attempt.task.as_str(),record.operation.as_str()],|r|r.get(0))?;
            let never_claimed=!prior_claim&&!another_launch&&delivery.state==DeliveryState::Pending&&delivery.attempts==0&&delivery.epoch==0&&delivery.owner.is_none()&&delivery.lease_until_ms.is_none()&&delivery.last_outcome.is_none();
            if attempt.state==AttemptState::Reserved&&task.state==TaskState::Running&&task.active_attempt.as_ref()==Some(id)&&Some(task.revision)==record.inputs.task_revision.checked_add(1)&&never_claimed&&matching_binding&&no_external_worker&&!other_retained {
                super::delivery::update_outcome(&tx,&delivery,&Outcome::PermanentFailure{diagnostic:format!("cancelled before any launch claim: {reason}")},now,"operator.cancellation")?;released=true;attempt.state=AttemptState::Cancelled;attempt.termination_observed=true;task.state=TaskState::Cancelled;task.active_attempt=None;
            }
        }
        attempt.revision=attempt.revision.checked_add(1).ok_or_else(||invalid("attempt revision exhausted"))?;
        tx.execute("UPDATE attempts SET revision=?2,state=?3,termination_observed=?4 WHERE id=?1",params![id.as_str(),integer(attempt.revision)?,attempt.state.as_str(),attempt.termination_observed])?;
        if task.active_attempt.as_ref()==Some(id)||released {task.revision=task.revision.checked_add(1).ok_or_else(||invalid("task revision exhausted"))?;tx.execute("UPDATE tasks SET revision=?2,state=?3,active_attempt=?4 WHERE id=?1",params![task.id.as_str(),integer(task.revision)?,task.state.as_str(),task.active_attempt.as_ref().map(AttemptId::as_str)])?;event(&tx,"task.changed",task.id.as_str(),task.revision,&task)?;}
        let request=CancellationRequest{attempt:id.clone(),requested_unix_ms:now,reason:reason.into()};tx.execute("INSERT INTO attempt_cancellations VALUES(?1,?2,?3)",params![id.as_str(),now,reason])?;
        event(&tx,"attempt.cancellation_requested",id.as_str(),attempt.revision,&serde_json::json!({"request":request,"released":released,"attempt":attempt}))?;
        let result=CancellationChange{head:head(&tx)?,attempt:id.clone(),released,attempt_revision:attempt.revision,task_revision:task.revision};tx.commit()?;Ok(result)
    }
}

#[cfg(test)]
pub(super) mod tests;
