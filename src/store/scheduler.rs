use super::*;
use std::collections::{BTreeMap,BTreeSet,VecDeque};

fn schema(db:&Connection)->Result<()> {check_schema(db)?;let version:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;if version<10 {return Err(StoreError::UnsupportedSchema(version));}Ok(())}
fn invalid(message:&str)->StoreError {StoreError::Invalid(message.into())}
pub(crate) fn graph(tasks:&[Task],queue:&[QueueRecord])->Result<()> {
    let task_ids:BTreeSet<_>=tasks.iter().map(|t|t.id.clone()).collect();
    graph_ids(&task_ids,queue,None)
}
fn graph_ids(task_ids:&BTreeSet<TaskId>,queue:&[QueueRecord],budget:Option<&read_budget::ReadBudget>)->Result<()> {
    if queue.len()>10_000{return Err(invalid("queued task inventory exceeds 10000"));}
    let mut degree:BTreeMap<TaskId,usize>=BTreeMap::new();let mut followers:BTreeMap<TaskId,Vec<TaskId>>=BTreeMap::new();let mut edge_count=0;
    for record in queue {
        if let Some(budget)=budget {budget.check()?;}
        if !task_ids.contains(&record.task){return Err(invalid("queued task is missing"));}
        degree.entry(record.task.clone()).or_default();let mut seen=BTreeSet::new();
        for edge in &record.dependencies {
            edge_count+=1;if edge_count>100_000||record.dependencies.len()>256{return Err(invalid("dependency inventory exceeds bounds"));}
            if edge.predecessor==record.task||!task_ids.contains(&edge.predecessor)||!seen.insert(&edge.predecessor){return Err(invalid("missing, duplicate or self dependency"));}
            degree.entry(edge.predecessor.clone()).or_default();*degree.get_mut(&record.task).unwrap()+=1;followers.entry(edge.predecessor.clone()).or_default().push(record.task.clone());
        }
    }
    let mut ready:VecDeque<_>=degree.iter().filter(|(_,n)|**n==0).map(|(id,_)|id.clone()).collect();let mut visited=0;
    while let Some(id)=ready.pop_front(){if let Some(budget)=budget {budget.check()?;}visited+=1;if let Some(next)=followers.get(&id){for id in next {let n=degree.get_mut(id).unwrap();*n-=1;if *n==0{ready.push_back(id.clone());}}}}
    if visited!=degree.len(){return Err(invalid("dependency cycle"));}Ok(())
}
pub(super) fn read(db:&Connection)->Result<SchedulerSnapshot> {read_with_tasks(db,&read_tasks(db)?,None)}
pub(super) fn read_policy(db:&Connection,budget:Option<&read_budget::ReadBudget>)->Result<SchedulerPolicy> {
    let mut stmt=db.prepare("SELECT revision,max_active_workers,max_attempts_per_task FROM scheduler_policy WHERE singleton=1")?;
    let mut rows=stmt.query([])?;
    let r=rows.next()?.ok_or_else(||StoreError::from(rusqlite::Error::QueryReturnedNoRows))?;
    if let Some(budget)=budget {budget.row(r,&[])?;}
    let policy=SchedulerPolicy{revision:r.get(0)?,max_active_workers:r.get(1)?,max_attempts_per_task:r.get(2)?};
    if policy.revision==0 || policy.max_active_workers>1024 || !(1..=32).contains(&policy.max_attempts_per_task) {return Err(StoreError::Corrupt("invalid scheduler policy".into()));}
    Ok(policy)
}
pub(super) fn read_with_tasks(db:&Connection,tasks:&[Task],budget:Option<&read_budget::ReadBudget>)->Result<SchedulerSnapshot> {
    let policy=read_policy(db,budget)?;
    let mut stmt=db.prepare("SELECT task_id,priority,enqueued_unix_ms,enqueue_sequence FROM task_queue ORDER BY enqueue_sequence,task_id")?;
    let mut rows=stmt.query([])?;
    let mut queue=Vec::new();
    while let Some(r)=rows.next()? {
        if let Some(budget)=budget {budget.row(r,&[])?;}
        queue.push(QueueRecord{task:TaskId::new(r.get::<_,String>(0)?).map_err(StoreError::Corrupt)?,priority:r.get(1)?,enqueued_unix_ms:r.get(2)?,enqueue_sequence:r.get(3)?,dependencies:Vec::new()});
    }
    let index:BTreeMap<_,_>=queue.iter().enumerate().map(|(i,q)|(q.task.as_str().to_string(),i)).collect();
    let mut stmt=db.prepare("SELECT task_id,predecessor_id,requirement FROM task_dependencies ORDER BY task_id,predecessor_id")?;
    let mut rows=stmt.query([])?;
    while let Some(r)=rows.next()? {
        if let Some(budget)=budget {budget.row(r,&[])?;}
        let task:String=r.get(0)?;let predecessor:String=r.get(1)?;let requirement:String=r.get(2)?;
        let requirement=match requirement.as_str(){"verified_result"=>DependencyRequirement::VerifiedResult,"integrated_commit"=>DependencyRequirement::IntegratedCommit,"integration_candidate"=>DependencyRequirement::IntegrationCandidate,"landed_commit"=>DependencyRequirement::LandedCommit,_=>return Err(StoreError::Corrupt("unknown dependency requirement".into()))};
        queue[*index.get(&task).ok_or_else(||StoreError::Corrupt("dependency has no queue record".into()))?].dependencies.push(Dependency{predecessor:TaskId::new(predecessor).map_err(StoreError::Corrupt)?,requirement});
    }
    graph(tasks,&queue)?;Ok(SchedulerSnapshot{policy,queue})
}
// Graph validation needs identities and edges, not unrelated task payloads or
// retired attempts. Preserve validation of the whole bounded queue graph.
fn queue_graph(db:&Connection,budget:&read_budget::ReadBudget)->Result<(BTreeSet<TaskId>,Vec<QueueRecord>)> {
    let mut identities=BTreeSet::new();
    let mut queue=Vec::new();
    let mut stmt=db.prepare("SELECT q.task_id,q.priority,q.enqueued_unix_ms,q.enqueue_sequence,t.id FROM task_queue q LEFT JOIN tasks t ON t.id=q.task_id ORDER BY q.enqueue_sequence,q.task_id LIMIT 10001")?;
    let mut rows=stmt.query([])?;
    while let Some(row)=rows.next()? {
        budget.row(row,&[])?;
        if queue.len()==10_000 {return Err(invalid("queued task inventory exceeds 10000"));}
        if row.get::<_,Option<String>>(4)?.is_none() {return Err(invalid("queued task is missing"));}
        let id=TaskId::new(row.get::<_,String>(0)?).map_err(StoreError::Corrupt)?;
        identities.insert(id.clone());
        queue.push(QueueRecord{task:id,priority:row.get(1)?,enqueued_unix_ms:row.get(2)?,enqueue_sequence:row.get(3)?,dependencies:Vec::new()});
    }
    let index:BTreeMap<_,_>=queue.iter().enumerate().map(|(i,q)|(q.task.as_str().to_string(),i)).collect();
    let mut stmt=db.prepare("SELECT d.task_id,d.predecessor_id,d.requirement,t.id FROM task_dependencies d LEFT JOIN tasks t ON t.id=d.predecessor_id ORDER BY d.task_id,d.predecessor_id LIMIT 100001")?;
    let mut rows=stmt.query([])?;
    let mut edges=0;
    while let Some(row)=rows.next()? {
        budget.row(row,&[])?;edges+=1;
        if edges>100_000 {return Err(invalid("dependency inventory exceeds bounds"));}
        if row.get::<_,Option<String>>(3)?.is_none() {return Err(invalid("missing, duplicate or self dependency"));}
        let task:String=row.get(0)?;
        let predecessor=TaskId::new(row.get::<_,String>(1)?).map_err(StoreError::Corrupt)?;
        let requirement=match row.get::<_,String>(2)?.as_str(){
            "verified_result"=>DependencyRequirement::VerifiedResult,"integrated_commit"=>DependencyRequirement::IntegratedCommit,
            "integration_candidate"=>DependencyRequirement::IntegrationCandidate,"landed_commit"=>DependencyRequirement::LandedCommit,
            _=>return Err(StoreError::Corrupt("unknown dependency requirement".into())),
        };
        let at=*index.get(&task).ok_or_else(||StoreError::Corrupt("dependency has no queue record".into()))?;
        if queue[at].dependencies.len()==256 {return Err(invalid("dependency inventory exceeds bounds"));}
        identities.insert(predecessor.clone());
        queue[at].dependencies.push(Dependency{predecessor,requirement});
    }
    graph_ids(&identities,&queue,Some(budget))?;
    Ok((identities,queue))
}

fn queue_task_on(tx:&Connection,id:&TaskId,revision:u64,head_expected:u64,request:&QueueRequest,now:i64,budget:&read_budget::ReadBudget)->Result<u64> {
        super::delivery::now_check(now)?;if !(-20..=20).contains(&request.priority)||request.dependencies.len()>256{return Err(invalid("queue priority/dependency limits exceeded"));}
        schema(tx)?;if head(tx)?!=head_expected{return Err(StoreError::Conflict);}
        let mut task=read_task_with_budget(tx,id.as_str(),Some(budget))?;if task.revision!=revision{return Err(StoreError::Conflict);}
        if task.active_attempt.is_some()||matches!(task.state,TaskState::Running|TaskState::Succeeded|TaskState::Cancelled)||tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE task_id=?1 AND termination_observed=0)",[id.as_str()],|row|row.get::<_,bool>(0))?{return Err(invalid("task execution or terminal disposition must be reconciled before queueing"));}
        let (mut identities,mut queue)=queue_graph(tx,budget)?;let previous=queue.iter().find(|q|&q.task==id).cloned();let mut dependencies=request.dependencies.clone();dependencies.sort_by(|a,b|a.predecessor.cmp(&b.predecessor));
        if previous.as_ref().is_some_and(|p|p.priority==request.priority&&p.dependencies==dependencies)&&task.state==TaskState::Queued{return Ok(head_expected);}
        let record=QueueRecord{task:id.clone(),priority:request.priority,enqueued_unix_ms:previous.as_ref().map(|p|p.enqueued_unix_ms).unwrap_or(now),enqueue_sequence:previous.as_ref().map(|p|p.enqueue_sequence).unwrap_or(head_expected.checked_add(1).ok_or_else(||invalid("sequence exhausted"))?),dependencies};
        identities.insert(id.clone());
        for edge in &record.dependencies {budget.check()?;if tx.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)",[edge.predecessor.as_str()],|row|row.get::<_,bool>(0))? {identities.insert(edge.predecessor.clone());}}
        queue.retain(|q|&q.task!=id);queue.push(record.clone());graph_ids(&identities,&queue,Some(budget))?;
        task.revision=revision.checked_add(1).ok_or_else(||invalid("task revision exhausted"))?;task.state=TaskState::Queued;
        tx.execute("UPDATE tasks SET revision=?2,state='queued' WHERE id=?1",params![id.as_str(),integer(task.revision)?])?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('task.changed',?1,?2,1,?3)",params![id.as_str(),integer(task.revision)?,serde_json::to_string(&task).map_err(|e|invalid(&e.to_string()))?])?;
        tx.execute("INSERT INTO task_queue VALUES(?1,?2,?3,?4) ON CONFLICT(task_id) DO UPDATE SET priority=excluded.priority",params![id.as_str(),record.priority,record.enqueued_unix_ms,integer(record.enqueue_sequence)?])?;
        tx.execute("DELETE FROM task_dependencies WHERE task_id=?1",[id.as_str()])?;
        for edge in &record.dependencies {tx.execute("INSERT INTO task_dependencies VALUES(?1,?2,?3)",params![id.as_str(),edge.predecessor.as_str(),edge.requirement.as_str()])?;}
        super::satisfaction::attach_stored_receipts_with_budget(tx, id.as_str(), budget)?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('scheduler.task_queued',?1,?2,1,?3)",params![id.as_str(),integer(task.revision)?,serde_json::to_string(&record).map_err(|e|invalid(&e.to_string()))?])?;
        budget.check()?;head(tx)

}

impl SqliteStore {
    pub fn set_scheduler_policy(&mut self,head_expected:u64,revision:u64,max_active_workers:u32,max_attempts_per_task:u32)->Result<u64> {
        self.set_scheduler_policy_with_budget(head_expected,revision,max_active_workers,max_attempts_per_task,None)
    }
    pub(crate) fn set_scheduler_policy_with_budget(&mut self,head_expected:u64,revision:u64,max_active_workers:u32,max_attempts_per_task:u32,budget:Option<&read_budget::ReadBudget>)->Result<u64> {
        if let Some(budget)=budget {budget.check()?;}
        if max_active_workers>1024||!(1..=32).contains(&max_attempts_per_task){return Err(invalid("invalid scheduler limits"));}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;if head(&tx)?!=head_expected{return Err(StoreError::Conflict);}
        let old=read_policy(&tx,budget)?;if old.revision!=revision{return Err(StoreError::Conflict);}
        if old.max_active_workers==max_active_workers&&old.max_attempts_per_task==max_attempts_per_task{return Ok(head_expected);}
        let policy=SchedulerPolicy{revision:revision.checked_add(1).ok_or_else(||invalid("policy revision exhausted"))?,max_active_workers,max_attempts_per_task};
        tx.execute("UPDATE scheduler_policy SET revision=?1,max_active_workers=?2,max_attempts_per_task=?3 WHERE singleton=1",params![integer(policy.revision)?,max_active_workers,max_attempts_per_task])?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('scheduler.policy_changed','scheduler',?1,1,?2)",params![integer(policy.revision)?,serde_json::to_string(&policy).map_err(|e|invalid(&e.to_string()))?])?;
        let result=head(&tx)?;if let Some(budget)=budget {budget.check()?;}tx.commit()?;Ok(result)
    }
    pub fn queue_task(&mut self,id:&TaskId,revision:u64,head_expected:u64,request:&QueueRequest,now:i64)->Result<u64> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result=read_budget::with_local_deadline(&tx, |budget| queue_task_on(&tx,id,revision,head_expected,request,now,budget))?;
        tx.commit()?;Ok(result)
    }
    pub(crate) fn queue_task_with_budget(&mut self,id:&TaskId,revision:u64,head_expected:u64,request:&QueueRequest,now:i64,budget:&read_budget::ReadBudget)->Result<u64> {
        budget.check()?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result=queue_task_on(&tx,id,revision,head_expected,request,now,budget)?;
        budget.check()?;tx.commit()?;Ok(result)
    }
    pub fn queue_report(&mut self,now:i64)->Result<QueueReport> {
        let tx=self.connection.transaction()?;
        let report=read_budget::with_local_deadline(&tx,|budget|queue_report_on(&tx,now,budget))?;
        tx.commit()?;Ok(report)
    }
    pub(crate) fn queue_report_with_budget(&mut self,now:i64,budget:&read_budget::ReadBudget)->Result<QueueReport> {
        budget.check()?;
        let tx=self.connection.transaction()?;
        let report=queue_report_on(&tx,now,budget)?;
        budget.check()?;tx.commit()?;Ok(report)
    }
}

fn unused_launch_grant(db:&Connection,task:&str,version:u32,budget:&read_budget::ReadBudget)->Result<bool> {
    if version<13 {return Ok(false);}
    let mut stmt=db.prepare("SELECT g.id FROM approval_grants g WHERE json_extract(g.payload,'$.scope.task')=?1 AND json_extract(g.payload,'$.scope.class')='runtime_launch' AND NOT EXISTS(SELECT 1 FROM approval_uses u WHERE u.approval_id=g.id) ORDER BY g.id LIMIT 129")?;
    let mut rows=stmt.query([task])?;
    let mut count=0;
    while let Some(row)=rows.next()? {
        budget.row(row,&[])?;count+=1;
        if count>128 {return Err(StoreError::Limit("queue approval candidates exceed 128".into()));}
        let id:String=row.get(0)?;
        let grant=super::approvals::grant(db,&id,Some(budget))?;
        if grant.scope.task.as_str()==task && grant.scope.class==ApprovalClass::RuntimeLaunch {return Ok(true);}
    }
    Ok(false)
}

fn queue_report_on(db:&Connection,now:i64,budget:&read_budget::ReadBudget)->Result<QueueReport> {
    super::delivery::now_check(now)?;schema(db)?;
    let policy=read_policy(db,Some(budget))?;
    let (_,queue)=queue_graph(db,budget)?;
    let control=super::control::read_with_budget(db,Some(budget))?;
    let version:u32=db.query_row("PRAGMA user_version",[],|row|row.get(0))?;
    let admission_on=super::satisfaction::admission_enabled(db)?;
    let retained_attempts:usize=db.query_row("SELECT count(*) FROM attempts WHERE termination_observed=0",[],|row|row.get(0))?;
    let available_slots=(policy.max_active_workers as usize).saturating_sub(retained_attempts);
    let budget_blockers=super::budget::admission_blockers_with_budget(db,budget)?;
    let mut tasks=BTreeMap::new();
    for record in &queue {
        for id in std::iter::once(&record.task).chain(record.dependencies.iter().map(|edge|&edge.predecessor)) {
            if !tasks.contains_key(id) {tasks.insert(id.clone(),read_task_with_budget(db,id.as_str(),Some(budget))?);}
        }
    }
    let mut entries=Vec::new();
    for record in &queue {
        budget.check()?;
        let task=&tasks[&record.task];let mut blockers=Vec::new();
        if !super::contract_binding::queue_matches_with_budget(db,task.id.as_str(),Some(budget))? {blockers.push("contract_dependency_mismatch".into());}
        if task.state!=TaskState::Queued {blockers.push(format!("task_state:{:?}",task.state));}
        if control.state!=ProjectState::Active||control.reconciliation_required {blockers.push("project_not_admitted".into());}
        if available_slots==0 {blockers.push("capacity_full".into());}
        let held:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE task_id=?1 AND termination_observed=0)",[task.id.as_str()],|row|row.get(0))?;
        if held {blockers.push("task_capacity_retained".into());}
        if super::admission_read::attempt_count(db,task.id.as_str(),policy.max_attempts_per_task)? >= policy.max_attempts_per_task as u64 {blockers.push("attempt_limit".into());}
        for edge in &record.dependencies {
            if let Some(blocker)=super::satisfaction::dependency_blocker_with_budget(db,task.id.as_str(),&tasks[&edge.predecessor],edge.requirement,admission_on,Some(budget))? {blockers.push(blocker);}
        }
        if let Some(blocker)=super::capabilities::queue_capability_blocker_with_budget(db,task.id.as_str(),now,Some(budget))? {blockers.push(blocker);}
        blockers.extend(budget_blockers.iter().cloned());
        if !unused_launch_grant(db,task.id.as_str(),version,budget)? {blockers.push("owner_signature_not_scheduled".into());}
        let retained_launch:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE task_id=?1 AND termination_observed=0 AND state IN ('reserved','launching','running','awaiting_input'))",[task.id.as_str()],|row|row.get(0))?;
        if !retained_launch {blockers.push("launch_reserve_not_scheduled".into());blockers.push("controller_requires_reserved_attempt".into());}
        let score=now.saturating_sub(record.enqueued_unix_ms).max(0)/60_000+record.priority as i64;
        entries.push((record.enqueue_sequence,QueueEntry{task:task.id.clone(),task_revision:task.revision,effective_priority:score,blockers}));
    }
    entries.sort_by(|a,b|b.1.effective_priority.cmp(&a.1.effective_priority).then(a.0.cmp(&b.0)).then(a.1.task.cmp(&b.1.task)));
    let entries:Vec<_>=entries.into_iter().map(|(_,entry)|entry).collect();
    let launch_enabled=automatic_launch_enabled(admission_on,&entries);
    let capability=CapabilityReport{prepared_dispatch:true,automatic_admission:admission_on,dependency_producers:false,integration:if cfg!(target_os="linux") {"operator_local"} else {"unavailable"},blockers:vec!["automatic_admission_does_not_draft_sign_or_reserve".into()]};
    budget.check()?;
    Ok(QueueReport{head:head(db)?,policy,retained_attempts,available_slots,launch_enabled,capability,entries})
}

pub(super) fn queue_blockers(db:&Connection,now:i64,tasks:&[Task],attempts:&[Attempt],scheduler:&SchedulerSnapshot,control:&ProjectControl,approvals:&[ApprovalRecord])->Result<Vec<QueueEntry>> {
    let admission_on=super::satisfaction::admission_enabled(db)?;
    let retained_attempts=attempts.iter().filter(|a|a.retains_capacity()).count();let available_slots=(scheduler.policy.max_active_workers as usize).saturating_sub(retained_attempts);
    let budget_blockers=super::budget::report(db,false)?.blockers;let mut entries=Vec::new();
    for record in &scheduler.queue {
        let task=tasks.iter().find(|t|t.id==record.task).ok_or(StoreError::Conflict)?;let mut blockers=Vec::new();
        if !super::contract_binding::queue_matches(db, task.id.as_str())? { blockers.push("contract_dependency_mismatch".into()); }
        if task.state!=TaskState::Queued {blockers.push(format!("task_state:{:?}",task.state));}
        if control.state!=ProjectState::Active||control.reconciliation_required {blockers.push("project_not_admitted".into());}
        if available_slots==0{blockers.push("capacity_full".into());}
        if attempts.iter().any(|a|a.task==task.id&&a.retains_capacity()){blockers.push("task_capacity_retained".into());}
        if attempts.iter().filter(|a|a.task==task.id).count()>=scheduler.policy.max_attempts_per_task as usize {blockers.push("attempt_limit".into());}
        for edge in &record.dependencies {
            let predecessor=tasks.iter().find(|t|t.id==edge.predecessor).ok_or(StoreError::Conflict)?;
            // A missing receipt stays unavailable. A valid receipt still does not admit while the flag is off.
            if let Some(blocker)=super::satisfaction::dependency_blocker(db, task.id.as_str(), predecessor, edge.requirement, admission_on)? {blockers.push(blocker);}
        }
        // A level the selected profile has not shown. This does not certify the profile.
        if let Some(blocker)=super::capabilities::queue_capability_blocker(db, task.id.as_str(), now)? {blockers.push(blocker);}
        blockers.extend(budget_blockers.iter().cloned());
        let signed=approvals.iter().any(|record|record.consumed.is_none()&&record.grant.scope.class==ApprovalClass::RuntimeLaunch&&record.grant.scope.task==task.id);
        let retained_launch=attempts.iter().any(|attempt|attempt.task==task.id&&attempt.retains_capacity()&&matches!(attempt.state,AttemptState::Reserved|AttemptState::Launching|AttemptState::Running|AttemptState::AwaitingInput));
        if !signed {blockers.push("owner_signature_not_scheduled".into());}
        if !retained_launch {blockers.push("launch_reserve_not_scheduled".into());blockers.push("controller_requires_reserved_attempt".into());}
        let age=now.saturating_sub(record.enqueued_unix_ms).max(0)/60_000;let score=age+record.priority as i64;
        entries.push((record.enqueue_sequence,QueueEntry{task:task.id.clone(),task_revision:task.revision,effective_priority:score,blockers}));
    }
    entries.sort_by(|a,b|b.1.effective_priority.cmp(&a.1.effective_priority).then(a.0.cmp(&b.0)).then(a.1.task.cmp(&b.1.task)));
    Ok(entries.into_iter().map(|(_,entry)|entry).collect())
}

pub(super) fn automatic_launch_enabled(admission_on:bool, entries:&[QueueEntry])->bool {
    // Grant, capacity, and dependency blockers still keep this false when every entry has one.
    admission_on && entries.iter().any(|entry| entry.blockers.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture()->(tempfile::TempDir,SqliteStore) {
        let temp=tempfile::tempdir().unwrap();let mut db=SqliteStore::create(&temp.path().join("state.db")).unwrap();let mutations=["a","b","c"].into_iter().map(|id|Mutation::Task{expected:None,next:Task{id:TaskId::new(id).unwrap(),revision:1,state:TaskState::Draft,title:id.into(),active_attempt:None}}).collect();db.commit(Commit{expected_head:0,mutations}).unwrap();(temp,db)
    }
    fn request(priority:i32,deps:&[&str])->QueueRequest {QueueRequest{priority,dependencies:deps.iter().map(|id|Dependency{predecessor:TaskId::new(*id).unwrap(),requirement:DependencyRequirement::VerifiedResult}).collect()}}
    fn queue(db:&mut SqliteStore,id:&str,input:&QueueRequest,now:i64)->Result<u64> {let s=db.read_snapshot(None)?;let task=s.tasks.iter().find(|t|t.id.as_str()==id).unwrap();db.queue_task(&task.id,task.revision,s.head,input,now)}
    #[test]
    fn graph_refusals_and_mid_transaction_failure_preserve_all_state() {
        let(temp,mut db)=fixture();queue(&mut db,"a",&request(0,&["b"]),0).unwrap();queue(&mut db,"b",&request(0,&["c"]),0).unwrap();
        for deps in [vec!["a"],vec!["c"],vec!["missing"],vec!["a","a"]] {let before=db.read_snapshot(None).unwrap();assert!(queue(&mut db,"c",&request(0,&deps),1).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);}
        let before=db.read_snapshot(None).unwrap();let raw=Connection::open(temp.path().join("state.db")).unwrap();raw.execute_batch("CREATE TRIGGER fail_queue BEFORE INSERT ON events WHEN NEW.kind='scheduler.task_queued' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();assert!(queue(&mut db,"c",&request(0,&[]),2).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
    }
    #[test]
    fn aging_outweighs_new_priority_and_requeue_preserves_original_age() {
        let(_temp,mut db)=fixture();queue(&mut db,"a",&request(-20,&[]),0).unwrap();queue(&mut db,"b",&request(20,&[]),41*60_000).unwrap();let report=db.queue_report(41*60_000).unwrap();assert_eq!(report.entries[0].task.as_str(),"a");assert!(!report.launch_enabled);
        let before=db.read_snapshot(None).unwrap();queue(&mut db,"a",&request(-19,&[]),42*60_000).unwrap();let after=db.read_snapshot(None).unwrap();let old=&before.scheduler.unwrap().queue[0];let new=&after.scheduler.unwrap().queue[0];assert_eq!((old.enqueued_unix_ms,old.enqueue_sequence),(new.enqueued_unix_ms,new.enqueue_sequence));let head=after.head;assert_eq!(queue(&mut db,"a",&request(-19,&[]),99*60_000).unwrap(),head);
    }
    #[test]
    fn all_unterminated_states_count_and_lowering_policy_never_revokes_attempts() {
        let(_temp,mut db)=fixture();queue(&mut db,"a",&request(0,&[]),0).unwrap();let before=db.read_snapshot(None).unwrap();let attempts=[AttemptState::AwaitingInput,AttemptState::Lost,AttemptState::Completed].into_iter().enumerate().map(|(i,state)|Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new(format!("attempt-{i}")).unwrap(),task:TaskId::new("b").unwrap(),revision:1,state,snapshot:None,reservation:format!("slot-{i}"),termination_observed:false}}).collect();db.commit(Commit{expected_head:before.head,mutations:attempts}).unwrap();let before=db.read_snapshot(None).unwrap();db.set_scheduler_policy(before.head,1,2,3).unwrap();let report=db.queue_report(0).unwrap();assert_eq!(report.retained_attempts,3);assert_eq!(report.available_slots,0);assert!(report.entries[0].blockers.contains(&"capacity_full".into()));assert_eq!(db.read_snapshot(None).unwrap().attempts,before.attempts);assert!(queue(&mut db,"b",&request(0,&[]),0).is_err());
    }
    #[test]
    fn narrative_success_never_satisfies_verified_dependencies() {
        let(_temp,mut db)=fixture();queue(&mut db,"a",&request(0,&["b"]),0).unwrap();let s=db.read_snapshot(None).unwrap();let mut task=s.tasks.iter().find(|t|t.id.as_str()=="b").unwrap().clone();task.revision=2;task.state=TaskState::Succeeded;db.commit(Commit{expected_head:s.head,mutations:vec![Mutation::Task{expected:Some(1),next:task}]}).unwrap();let report=db.queue_report(0).unwrap();assert!(report.entries[0].blockers.iter().any(|b|b.starts_with("verified_dependency_evidence_unavailable:b")));assert!(db.read_snapshot(None).unwrap().attempts.is_empty());
    }
    #[test]
    fn prepared_dispatch_stays_true_while_dependency_evidence_stays_blocked() {
        let(_temp,mut db)=fixture();queue(&mut db,"a",&request(0,&["b"]),0).unwrap();let report=db.queue_report(0).unwrap();
        assert!(!report.launch_enabled);assert!(report.capability.prepared_dispatch);assert!(!report.capability.automatic_admission);assert!(!report.capability.dependency_producers);assert_eq!(report.capability.integration,if cfg!(target_os="linux") {"operator_local"} else {"unavailable"});
        assert_eq!(report.capability.blockers,vec!["automatic_admission_does_not_draft_sign_or_reserve".to_string()]);
        assert!(report.entries[0].blockers.iter().any(|b|b=="verified_dependency_evidence_unavailable:b:verified_result"));
        for stage in ["owner_signature_not_scheduled","launch_reserve_not_scheduled","controller_requires_reserved_attempt"] {assert!(report.entries[0].blockers.iter().any(|b|b==stage));}
        assert!(report.entries.iter().flat_map(|entry|&entry.blockers).chain(report.capability.blockers.iter()).all(|b|b!="launch_draft_not_scheduled"));
        let claims_producer=|blocker:&str|blocker.contains("launch_preparation_unavailable")||blocker.contains("verifier")||blocker.contains("integrator")||blocker.contains("producer")||blocker.contains("satisfaction");
        assert!(report.capability.blockers.iter().chain(report.entries.iter().flat_map(|entry|&entry.blockers)).all(|blocker|!claims_producer(blocker)));
    }
    #[test]
    fn retained_reserved_attempt_omits_reserve_blocker_and_grant_omits_signature_blocker() {
        let(temp,mut db)=fixture();let s=db.read_snapshot(None).unwrap();db.commit(Commit{expected_head:s.head,mutations:["d","e"].into_iter().map(|id|Mutation::Task{expected:None,next:Task{id:TaskId::new(id).unwrap(),revision:1,state:TaskState::Draft,title:id.into(),active_attempt:None}}).collect()}).unwrap();for id in ["a","b","c","d","e"]{queue(&mut db,id,&request(0,&[]),0).unwrap();}
        let s=db.read_snapshot(None).unwrap();
        db.commit(Commit{expected_head:s.head,mutations:vec![Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new("reserved-a").unwrap(),task:TaskId::new("a").unwrap(),revision:1,state:AttemptState::Reserved,snapshot:None,reservation:"worker:reserved-a".into(),termination_observed:false}},Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new("lost-b").unwrap(),task:TaskId::new("b").unwrap(),revision:1,state:AttemptState::Lost,snapshot:None,reservation:"slot".into(),termination_observed:false}},Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new("launching-d").unwrap(),task:TaskId::new("d").unwrap(),revision:1,state:AttemptState::Launching,snapshot:None,reservation:"worker:launching-d".into(),termination_observed:false}},Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new("awaiting-e").unwrap(),task:TaskId::new("e").unwrap(),revision:1,state:AttemptState::AwaitingInput,snapshot:None,reservation:"worker:awaiting-e".into(),termination_observed:false}}]}).unwrap();
        let s=db.read_snapshot(None).unwrap();let task=s.tasks.iter().find(|t|t.id.as_str()=="c").unwrap();
        let store_path=std::fs::canonicalize(temp.path().join("state.db")).unwrap().display().to_string();
        let profile=crate::domain::profile::fixture(crate::migration::ConfigReference{path:store_path.clone(),digest:None});
        let inputs=LaunchInputs{task_contract: None, version:2,project_store:store_path.clone(),task:task.id.clone(),task_revision:task.revision,scheduler_revision:1,control_epoch:0,binding:"local".into(),binding_revision:1,binding_digest:"a".repeat(64),profile:profile.reference().unwrap(),effective_profile:Some(profile.clone()),approval:VersionedReference{id:"pending".into(),revision:1,digest:"0".repeat(64)},config:crate::migration::ConfigReference{path:store_path,digest:None},repositories:vec![],dependencies:vec![],memory:None,budget:None};
        let grant=ApprovalGrant{version:1,scope:ApprovalScope::for_launch(&inputs).unwrap(),policy:profile.permission_policy,issued_unix_ms:0,expires_unix_ms:100_000};
        db.install_approval(&PreparedApproval{grant},s.head,1_000).unwrap();
        let report=db.queue_report(1_000).unwrap();assert!(!report.launch_enabled);
        let entry=|id:&str|&report.entries.iter().find(|entry|entry.task.as_str()==id).unwrap().blockers;
        assert!(entry("a").iter().all(|b|b!="launch_reserve_not_scheduled"&&b!="controller_requires_reserved_attempt"));
        assert!(entry("d").iter().all(|b|b!="launch_reserve_not_scheduled"&&b!="controller_requires_reserved_attempt"));
        assert!(entry("e").iter().all(|b|b!="launch_reserve_not_scheduled"&&b!="controller_requires_reserved_attempt"));
        assert!(entry("a").iter().any(|b|b=="owner_signature_not_scheduled"));
        assert!(entry("b").iter().any(|b|b=="launch_reserve_not_scheduled")&&entry("b").iter().any(|b|b=="controller_requires_reserved_attempt"));
        assert!(entry("c").iter().all(|b|b!="owner_signature_not_scheduled"));
        assert!(entry("c").iter().any(|b|b=="launch_reserve_not_scheduled"));
        assert!(report.entries.iter().flat_map(|entry|&entry.blockers).all(|b|b!="launch_draft_not_scheduled"));
        assert_eq!(report.capability.blockers,vec!["automatic_admission_does_not_draft_sign_or_reserve".to_string()]);
    }
    #[test]
    fn competing_policy_writers_cannot_both_win_the_same_revision() {
        use std::sync::{Arc,Barrier};let(temp,mut db)=fixture();let head=db.read_snapshot(None).unwrap().head;let barrier=Arc::new(Barrier::new(2));let threads:Vec<_>=[2,3].into_iter().map(|cap|{let path=temp.path().join("state.db");let barrier=barrier.clone();std::thread::spawn(move||{let mut db=SqliteStore::open(&path).unwrap();barrier.wait();db.set_scheduler_policy(head,1,cap,3)})}).collect();let result:Vec<_>=threads.into_iter().map(|t|t.join().unwrap()).collect();assert_eq!(result.iter().filter(|r|r.is_ok()).count(),1);assert_eq!(db.read_snapshot(None).unwrap().scheduler.unwrap().policy.revision,2);
    }
}

#[cfg(test)]
#[test]
fn unrelated_task_count_never_makes_a_committed_store_unreadable() {
    let temp=tempfile::tempdir().unwrap();let path=temp.path().join("state.db");let mut db=SqliteStore::create(&path).unwrap();
    for chunk in 0..11 {
        let count=if chunk==10 {1}else{1000};let head=db.read_snapshot(None).unwrap().head;let mutations=(0..count).map(|i|{let id=TaskId::new(format!("task-{}",chunk*1000+i)).unwrap();Mutation::Task{expected:None,next:Task{id,revision:1,state:TaskState::Draft,title:"unqueued".into(),active_attempt:None}}}).collect();db.commit(Commit{expected_head:head,mutations}).unwrap();
    }
    let before=db.read_snapshot(None).unwrap();assert_eq!(before.tasks.len(),10_001);assert!(db.queue_report(0).unwrap().entries.is_empty());drop(db);
    let raw=Connection::open(&path).unwrap();crate::store::test_schema::historical(&raw, 9).unwrap();drop(raw);let mut db=SqliteStore::open(&path).unwrap();db.upgrade_v1().unwrap();assert_eq!(db.read_snapshot(None).unwrap().tasks,before.tasks);let head=db.read_snapshot(None).unwrap().head;db.queue_task(&TaskId::new("task-0").unwrap(),1,head,&QueueRequest{priority:0,dependencies:vec![]},0).unwrap();assert_eq!(db.queue_report(0).unwrap().entries.len(),1);
}
