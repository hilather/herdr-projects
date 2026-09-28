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
    if i.task_contract.as_ref().is_some_and(|r| !reference(r)) { return Err(invalid("invalid task contract reference")); }
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
    read_input_rows(db,None,budget)
}
pub(super) fn read_input(db:&Connection,operation:&OperationId,budget:Option<&read_budget::ReadBudget>)->Result<AttemptInputRecord> {
    read_input_rows(db,Some(operation),budget)?.pop().ok_or_else(||StoreError::Corrupt("selected launch input missing or orphaned".into()))
}
pub(crate) fn read_attempt_input(db:&Connection,attempt:&str,budget:Option<&read_budget::ReadBudget>)->Result<AttemptInputRecord> {
    let operation:String=read_budget::one(db,"SELECT operation_id FROM attempt_inputs WHERE attempt_id=?1",[attempt],budget,&[],|row|row.get(0))?;
    let record=read_input(db,&OperationId::new(operation).map_err(StoreError::Corrupt)?,budget)?;
    if record.attempt.as_str()!=attempt {return Err(StoreError::Corrupt("selected attempt input mismatch".into()));}
    Ok(record)
}
impl SqliteStore {
    pub(crate) fn sealed_attempt_input(&self,attempt:&str,budget:Option<&read_budget::ReadBudget>)->Result<AttemptInputRecord> {
        read_attempt_input(&self.connection,attempt,budget)
    }
}
pub(super) fn read_input_rows(db:&Connection,operation:Option<&OperationId>,budget:Option<&read_budget::ReadBudget>)->Result<Vec<AttemptInputRecord>> {
    let columns="SELECT i.attempt_id,i.operation_id,i.payload,i.payload_hash,a.task_id,o.task_id,o.kind,o.target,o.expected_revision,o.payload,o.payload_hash,o.payload_version,o.idempotency_key FROM attempt_inputs i JOIN attempts a ON a.id=i.attempt_id JOIN operations o ON o.id=i.operation_id";
    let sql=format!("{columns} {}",if operation.is_some(){"WHERE i.operation_id=?1"}else{"ORDER BY i.attempt_id"});
    let mut stmt=db.prepare(&sql)?;
    let mut rows=if let Some(operation)=operation {stmt.query([operation.as_str()])?} else {stmt.query([])?};
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
        if r.get::<_,u32>(11)?!=1 || r.get::<_,String>(12)?!=o {return Err(StoreError::Corrupt("launch operation identity mismatch".into()));}
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
        self.admit_prepared(prepared,expected_head,now,false,None,None,None)?.ok_or_else(||invalid("reservation missing"))
    }

    /// A draft checks the same admission conditions without requiring an approval
    /// that cannot be signed until the exact action has been constructed.
    /// This path returns before any task, attempt, event or operation is written.
    pub(crate) fn validate_launch_draft(&mut self,inputs:&LaunchInputs,expected_head:u64,now:i64)->Result<()> {
        self.admit_prepared(&[PreparedLaunch{inputs:inputs.clone()}],expected_head,now,true,None,None,None)?;
        Ok(())
    }

    pub(crate) fn reserve_prepared_controlled(&mut self,prepared:&[PreparedLaunch],expected_head:u64,now:i64,control:&super::controlled::ReadControl,budget:&read_budget::ReadBudget)->Result<Reservation> {
        control.check()?;
        self.admit_prepared(prepared,expected_head,now,false,Some(control),Some(budget),None)?.ok_or_else(||invalid("reservation missing"))
    }
    pub(super) fn reserve_delegated_prepared(&mut self, prepared:&[PreparedLaunch], delegated:&PreparedDelegatedReservation, now:i64)->Result<Reservation> {
        self.admit_prepared(prepared,delegated.request.expected_head,now,false,None,None,Some(delegated))?.ok_or_else(||invalid("delegated reservation missing"))
    }
    fn admit_prepared(&mut self,prepared:&[PreparedLaunch],expected_head:u64,now:i64,draft:bool,control:Option<&super::controlled::ReadControl>,budget:Option<&read_budget::ReadBudget>,delegated:Option<&PreparedDelegatedReservation>)->Result<Option<Reservation>> {
        super::delivery::now_check(now)?;if prepared.is_empty()||prepared.len()>128{return Err(invalid("reservation requires 1–128 ready preparations"));}
        let path=std::fs::canonicalize(self.connection.path().ok_or_else(||invalid("store path missing"))?).map_err(|e|StoreError::Io(e.to_string()))?;
        let mut seen=BTreeSet::new();for p in prepared {validate_inputs(&p.inputs)?;if p.inputs.version!=2 {return Err(invalid("new reservations require effective profile evidence"));}if Path::new(&p.inputs.project_store)!=path||!seen.insert(&p.inputs.task){return Err(invalid("preparation belongs to another store or duplicates a task"));}}
        let opened:u32=self.connection.query_row("PRAGMA user_version",[],|r|r.get(0))?;
        // Ancestry is a process. Prove it before the write transaction so a hang cannot block prepared dispatch.
        let proofs=if opened>=30 {
            let mut proofs=Vec::with_capacity(prepared.len());
            for preparation in prepared {proofs.push(super::satisfaction::prove_integrated_base(&self.connection,&preparation.inputs,control,budget)?);}
            proofs
        } else {Vec::new()};
        if let Some(control)=control {control.check()?;}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        let version:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;if version<13{return Err(StoreError::UnsupportedSchema(version));}
        // Older stores have no satisfaction rows. Newer stores check dependency
        // evidence whether or not automatic admission is on: an operator-signed
        // launch of a released dependent binds the same current satisfactions.
        let evidence=version>=30;
        if let Some(delegated)=delegated {
            if prepared.len()!=1 || draft { return Err(invalid("delegation requires one exact reservation")); }
            if let Some(result)=super::delegated_reservation::replay(&tx,delegated)? {tx.commit()?;return Ok(Some(result));}
        }
        if head(&tx)?!=expected_head{return Err(StoreError::Conflict);}
        let policy=super::scheduler::read_policy(&tx,budget)?;let control=super::control::read_with_budget(&tx,budget)?;
        if control.state!=ProjectState::Active||control.reconciliation_required{return Err(invalid("project is not admitted"));}
        let retained:u64=tx.query_row("SELECT count(*) FROM (SELECT id FROM attempts WHERE termination_observed=0 LIMIT 1025)",[],|r|r.get(0))?;
        if retained>=u64::from(policy.max_active_workers){return Err(invalid("project worker capacity is full"));}
        let attempts=read_retained_attempts_with_budget(&tx,budget)?;
        let mut queues=Vec::new();let mut task_ids=BTreeSet::new();let mut bindings=Vec::new();let mut ownership=Vec::new();
        for preparation in prepared {
            let i=&preparation.inputs;
            let queue=super::admission_read::queue_record_with_budget(&tx,i.task.as_str(),budget)?;
            task_ids.insert(i.task.as_str().to_string());
            task_ids.extend(queue.dependencies.iter().map(|d|d.predecessor.as_str().to_string()));
            queues.push(queue);
            bindings.push(super::runtime::read_binding(&tx,&i.binding,budget)?.ok_or(StoreError::Conflict)?);
            if let Some(owned)=super::ownership::read_binding(&tx,&i.binding,budget)? {ownership.push(owned);}
        }
        let tasks=task_ids.iter().map(|id|read_task_with_budget(&tx,id,budget)).collect::<Result<Vec<_>>>()?;
        if let Some(delegated)=delegated {super::delegated_reservation::install_derived(&tx,delegated,&prepared[0].inputs,now,budget)?;}
        let scheduler=SchedulerSnapshot{policy,queue:queues};
        let queued:BTreeMap<_,_>=scheduler.queue.iter().map(|q|(&q.task,q)).collect();let mut ranked=Vec::new();
        for preparation in prepared {
            let i=&preparation.inputs;if i.scheduler_revision!=scheduler.policy.revision||i.control_epoch!=control.epoch||i.config.digest!=control.config_digest{return Err(StoreError::Conflict);}
            let task=tasks.iter().find(|t|t.id==i.task&&t.revision==i.task_revision).cloned().ok_or(StoreError::Conflict)?;let queue=queued.get(&task.id).ok_or(StoreError::Conflict)?;
            if !evidence {
                if !i.dependencies.is_empty()||!queue.dependencies.is_empty(){return Err(invalid("dependency evidence producers are not available"));}
            } else {
                let proof=proofs.iter().find(|proof|proof.task==i.task);
                super::satisfaction::require_dependency_evidence_with_budget(&tx,i,&queue.dependencies,&tasks,proof,budget)?;
            }
            super::contract_binding::validate_with_budget(&tx,i,now,budget)?;
            super::worker_knowledge::validate(&tx,i,now)?;
            super::budget::check_with_budget(&tx,i.budget.as_ref(),false,budget)?;
            if !draft {super::approvals::validate_preparation_with_budget(&tx,i,now,budget)?;}
            if task.state!=TaskState::Queued||task.active_attempt.is_some()||queue.enqueued_unix_ms>now||attempts.iter().any(|a|a.task==task.id&&a.retains_capacity()) {return Err(invalid("task is not ready for reservation"));}
            if super::admission_read::attempt_count(&tx,task.id.as_str(),scheduler.policy.max_attempts_per_task)? >= u64::from(scheduler.policy.max_attempts_per_task) {return Err(invalid("task attempt limit reached"));}
            let binding=bindings.iter().find(|b|b.id==i.binding&&b.revision==i.binding_revision&&b.task.as_ref()==Some(&task.id)).ok_or(StoreError::Conflict)?;
            if super::ownership::identity_digest(binding)?!=i.binding_digest{return Err(StoreError::Conflict);}
            if !binding.identity.agent.is_empty()&&i.effective_profile.as_ref().map(|p|p.kind.as_str())!=Some(binding.identity.agent.as_str()) {return Err(invalid("profile kind differs from runtime binding"));}
            // Existing worker identities cannot be turned into a new launch.
            if !binding.identity.pane_id.is_empty()||ownership.iter().any(|o|o.binding==binding.id&&(o.attempt.is_some()||o.session.is_some()||o.agent.is_some())) {return Err(invalid("existing worker identity requires reconciliation"));}
            if !binding.identity.repo.is_empty()&&!i.repositories.iter().any(|r|r.repository==binding.identity.repo){return Err(invalid("recorded repository lacks a pinned input"));}
            let score=(now-queue.enqueued_unix_ms)/60_000+queue.priority as i64;ranked.push((score,queue.enqueue_sequence,&preparation.inputs));
        }
        ranked.sort_by(|a,b|b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.task.cmp(&b.2.task)));let inputs=ranked[0].2.clone();
        // Same overlap the ranker uses. A direct reserve cannot skip it, and the
        // holder stays on the revision current when that attempt was reserved.
        // Disabling automatic dispatch does not waive canonical resource
        // ownership for explicit owner or delegated reservations and drafts.
        if super::satisfaction::overlap_with_retained_with_budget(&tx, inputs.task.as_str(), &attempts,budget)? {
            return Err(invalid("resource_conflict"));
        }
        if draft {return Ok(None);}
        if version>=43 {tx.execute("DELETE FROM admission_scan_cursor",[])?;}
        let(attempt_id,operation_id)=record_ids(&inputs)?;
        let task_revision=inputs.task_revision.checked_add(1).ok_or_else(||invalid("task revision exhausted"))?;
        let mut task=tasks.iter().find(|t|t.id==inputs.task).cloned().ok_or(StoreError::Conflict)?;task.revision=task_revision;task.state=TaskState::Running;task.active_attempt=Some(attempt_id.clone());
        // Telemetry: classify before the first attempt row; later attempts reuse it.
        if version>=49 {super::dispatch_log::classify_in_transaction(&tx,&inputs,queued.get(&inputs.task).ok_or(StoreError::Conflict)?.dependencies.len(),now,budget)?;}
        let record=AttemptInputRecord{attempt:attempt_id.clone(),operation:operation_id.clone(),inputs};let payload=serde_json::to_string(&record).map_err(|e|invalid(&e.to_string()))?;if payload.len()>MAX_RECORD_BYTES{return Err(invalid("attempt inputs exceed 1 MiB"));}
        let digest=format!("{:x}",Sha256::digest(payload.as_bytes()));let attempt=Attempt{id:attempt_id.clone(),task:record.inputs.task.clone(),revision:1,state:AttemptState::Reserved,snapshot:record.inputs.memory.as_ref().map(|r|r.id.clone()),reservation:format!("worker:{}",attempt_id.as_str()),termination_observed:false};
        tx.execute("INSERT INTO attempts VALUES(?1,?2,1,'reserved',?4,?3,0)",params![attempt_id.as_str(),attempt.task.as_str(),attempt.reservation,attempt.snapshot])?;
        tx.execute("UPDATE tasks SET revision=?2,state='running',active_attempt=?3 WHERE id=?1",params![attempt.task.as_str(),integer(task_revision)?,attempt_id.as_str()])?;
        tx.execute("INSERT INTO operations VALUES(?1,?2,'runtime.launch',?3,1,?4,?5,?6,?7,?1)",params![operation_id.as_str(),attempt.task.as_str(),record.inputs.binding,payload,digest,integer(task_revision)?,now])?;
        tx.execute("INSERT INTO attempt_inputs VALUES(?1,?2,?3,?4)",params![attempt_id.as_str(),operation_id.as_str(),payload,digest])?;
        event(&tx,"attempt.reserved",attempt_id.as_str(),1,&record)?;event(&tx,"task.changed",attempt.task.as_str(),task_revision,&task)?;
        event(&tx,"operation.enqueued",operation_id.as_str(),task_revision,&record)?;
        let result=Reservation{head:head(&tx)?,record,task_revision};
        if let Some(delegated)=delegated {super::delegated_reservation::record(&tx,delegated,&result)?;}
        super::consumer_bindings::reconcile_task(&tx,attempt.task.as_str(),budget)?;
        #[cfg(test)]
        tests::crash_boundary("before_reservation_commit");
        tx.commit()?;Ok(Some(result))
    }

    pub fn cancel_attempt(&mut self,id:&AttemptId,expected_revision:u64,expected_head:u64,reason:&str,now:i64)->Result<CancellationChange> {
        self.cancel_attempt_with_budget(id,expected_revision,expected_head,reason,now,None)
    }
    pub(super) fn cancel_attempt_with_budget(&mut self,id:&AttemptId,expected_revision:u64,expected_head:u64,reason:&str,now:i64,budget:Option<&read_budget::ReadBudget>)->Result<CancellationChange> {
        if let Some(budget)=budget {budget.check()?;}
        super::delivery::now_check(now)?;if reason.trim().is_empty()||reason.len()>4000||reason.chars().any(char::is_control){return Err(invalid("cancellation reason must contain 1–4000 bytes without control characters"));}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;if head(&tx)?!=expected_head{return Err(StoreError::Conflict);}
        let result=cancel_attempt_in_transaction(&tx,id,expected_revision,reason,now,budget)?;
        tx.commit()?;Ok(result)
    }
}

/// Shared atomic cancellation, preserving the existing proof-before-release rule.
pub(super) fn cancel_attempt_in_transaction(tx:&Connection,id:&AttemptId,expected_revision:u64,reason:&str,now:i64,budget:Option<&read_budget::ReadBudget>)->Result<CancellationChange> {
        if let Some(budget)=budget {budget.check()?;}
        super::delivery::now_check(now)?;if reason.trim().is_empty()||reason.len()>4000||reason.chars().any(char::is_control){return Err(invalid("cancellation reason must contain 1–4000 bytes without control characters"));}
        schema(tx)?;let expected_head=head(tx)?;
        let mut attempt=read_attempt(tx,id)?;
        if attempt.revision!=expected_revision{return Err(StoreError::Conflict);}
        let mut task=read_task(tx,attempt.task.as_str())?;
        let old_reason:Option<String>=tx.query_row("SELECT reason FROM attempt_cancellations WHERE attempt_id=?1",[id.as_str()],|r|r.get(0)).optional()?;
        if let Some(old_reason)=old_reason {
            if old_reason!=reason{return Err(StoreError::Conflict);}
            return Ok(CancellationChange{head:expected_head,attempt:id.clone(),released:!attempt.retains_capacity(),attempt_revision:attempt.revision,task_revision:task.revision});
        }
        if !attempt.retains_capacity(){return Err(invalid("attempt already has termination evidence"));}
        let operation:Option<String>=tx.query_row("SELECT operation_id FROM attempt_inputs WHERE attempt_id=?1",[id.as_str()],|r|r.get(0)).optional()?;
        let record=operation.map(|operation|read_input(tx,&OperationId::new(operation).map_err(StoreError::Corrupt)?,budget)).transpose()?;
        let mut released=false;
        if let Some(record)=&record {
            let delivery=super::delivery::delivery(tx,&record.operation)?;
            let selected_binding=super::runtime::read_binding(tx,&record.inputs.binding,budget)?;
            let binding=selected_binding.as_ref().filter(|b|b.revision==record.inputs.binding_revision&&b.task.as_ref()==Some(&attempt.task));
            let owned=super::ownership::read_binding(tx,&record.inputs.binding,budget)?;
            let attempt_owned:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_ownership WHERE attempt_id=?1)",[id.as_str()],|r|r.get(0))?;
            let no_external_worker=binding.is_some_and(|b|b.identity.pane_id.is_empty())&&!attempt_owned&&!owned.as_ref().is_some_and(|o|o.attempt.is_some()||o.agent.is_some()||o.session.is_some());
            let matching_binding=binding.map(|b|super::ownership::identity_digest(b).map(|d|d==record.inputs.binding_digest)).transpose()?.unwrap_or(false);
            let other_retained:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE task_id=?1 AND id<>?2 AND termination_observed=0)",params![attempt.task.as_str(),attempt.id.as_str()],|r|r.get(0))?;
            let prior_claim:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE kind='operation.claimed' AND entity=?1)",[record.operation.as_str()],|r|r.get(0))?;
            let another_launch:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations o JOIN operation_delivery d ON d.operation_id=o.id WHERE o.task_id=?1 AND o.kind='runtime.launch' AND o.id<>?2 AND d.state NOT IN ('confirmed','permanent_failure'))",params![attempt.task.as_str(),record.operation.as_str()],|r|r.get(0))?;
            let never_claimed=!prior_claim&&!another_launch&&delivery.state==DeliveryState::Pending&&delivery.attempts==0&&delivery.epoch==0&&delivery.owner.is_none()&&delivery.lease_until_ms.is_none()&&delivery.last_outcome.is_none();
            if attempt.state==AttemptState::Reserved&&task.state==TaskState::Running&&task.active_attempt.as_ref()==Some(id)&&Some(task.revision)==record.inputs.task_revision.checked_add(1)&&never_claimed&&matching_binding&&no_external_worker&&!other_retained {
                super::delivery::update_outcome(tx,&delivery,&Outcome::PermanentFailure{diagnostic:format!("cancelled before any launch claim: {reason}")},now,"operator.cancellation")?;released=true;attempt.state=AttemptState::Cancelled;attempt.termination_observed=true;task.state=TaskState::Cancelled;task.active_attempt=None;
            }
        }
        attempt.revision=attempt.revision.checked_add(1).ok_or_else(||invalid("attempt revision exhausted"))?;
        tx.execute("UPDATE attempts SET revision=?2,state=?3,termination_observed=?4 WHERE id=?1",params![id.as_str(),integer(attempt.revision)?,attempt.state.as_str(),attempt.termination_observed])?;
        if task.active_attempt.as_ref()==Some(id)||released {task.revision=task.revision.checked_add(1).ok_or_else(||invalid("task revision exhausted"))?;tx.execute("UPDATE tasks SET revision=?2,state=?3,active_attempt=?4 WHERE id=?1",params![task.id.as_str(),integer(task.revision)?,task.state.as_str(),task.active_attempt.as_ref().map(AttemptId::as_str)])?;event(tx,"task.changed",task.id.as_str(),task.revision,&task)?;}
        let request=CancellationRequest{attempt:id.clone(),requested_unix_ms:now,reason:reason.into()};tx.execute("INSERT INTO attempt_cancellations VALUES(?1,?2,?3)",params![id.as_str(),now,reason])?;
        event(tx,"attempt.cancellation_requested",id.as_str(),attempt.revision,&serde_json::json!({"request":request,"released":released,"attempt":attempt}))?;
        super::consumer_bindings::reconcile_task(tx,attempt.task.as_str(),budget)?;
        if let Some(budget)=budget {budget.check()?;}
        Ok(CancellationChange{head:head(tx)?,attempt:id.clone(),released,attempt_revision:attempt.revision,task_revision:task.revision})
}

#[cfg(test)]
pub(super) mod tests;
