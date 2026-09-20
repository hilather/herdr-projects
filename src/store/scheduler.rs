use super::*;
use std::collections::{BTreeMap,BTreeSet,VecDeque};

fn schema(db:&Connection)->Result<()> {check_schema(db)?;let version:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;if version<10 {return Err(StoreError::UnsupportedSchema(version));}Ok(())}
fn invalid(message:&str)->StoreError {StoreError::Invalid(message.into())}
fn graph(tasks:&[Task],queue:&[QueueRecord])->Result<()> {
    if queue.len()>10_000{return Err(invalid("queued task inventory exceeds 10000"));}
    let task_ids:BTreeSet<_>=tasks.iter().map(|t|&t.id).collect();
    let mut degree:BTreeMap<TaskId,usize>=BTreeMap::new();let mut followers:BTreeMap<TaskId,Vec<TaskId>>=BTreeMap::new();let mut edge_count=0;
    for record in queue {
        if !task_ids.contains(&record.task){return Err(invalid("queued task is missing"));}
        degree.entry(record.task.clone()).or_default();let mut seen=BTreeSet::new();
        for edge in &record.dependencies {
            edge_count+=1;if edge_count>100_000||record.dependencies.len()>256{return Err(invalid("dependency inventory exceeds bounds"));}
            if edge.predecessor==record.task||!task_ids.contains(&edge.predecessor)||!seen.insert(&edge.predecessor){return Err(invalid("missing, duplicate or self dependency"));}
            degree.entry(edge.predecessor.clone()).or_default();*degree.get_mut(&record.task).unwrap()+=1;followers.entry(edge.predecessor.clone()).or_default().push(record.task.clone());
        }
    }
    let mut ready:VecDeque<_>=degree.iter().filter(|(_,n)|**n==0).map(|(id,_)|id.clone()).collect();let mut visited=0;
    while let Some(id)=ready.pop_front(){visited+=1;if let Some(next)=followers.get(&id){for id in next {let n=degree.get_mut(id).unwrap();*n-=1;if *n==0{ready.push_back(id.clone());}}}}
    if visited!=degree.len(){return Err(invalid("dependency cycle"));}Ok(())
}
pub(super) fn read(db:&Connection)->Result<SchedulerSnapshot> {
    let policy=db.query_row("SELECT revision,max_active_workers,max_attempts_per_task FROM scheduler_policy WHERE singleton=1",[],|r|Ok(SchedulerPolicy{revision:r.get(0)?,max_active_workers:r.get(1)?,max_attempts_per_task:r.get(2)?}))?;
    let mut stmt=db.prepare("SELECT task_id,priority,enqueued_unix_ms,enqueue_sequence FROM task_queue ORDER BY enqueue_sequence,task_id")?;
    let mut queue=stmt.query_map([],|r|Ok(QueueRecord{task:TaskId::new(r.get::<_,String>(0)?).map_err(|_|rusqlite::Error::InvalidQuery)?,priority:r.get(1)?,enqueued_unix_ms:r.get(2)?,enqueue_sequence:r.get(3)?,dependencies:Vec::new()}))?.collect::<std::result::Result<Vec<_>,_>>()?;
    let index:BTreeMap<_,_>=queue.iter().enumerate().map(|(i,q)|(q.task.as_str().to_string(),i)).collect();
    let mut stmt=db.prepare("SELECT task_id,predecessor_id,requirement FROM task_dependencies ORDER BY task_id,predecessor_id")?;
    for row in stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))? {
        let(task,predecessor,requirement)=row?;let requirement=match requirement.as_str(){"verified_result"=>DependencyRequirement::VerifiedResult,"integration_candidate"=>DependencyRequirement::IntegrationCandidate,"landed_commit"=>DependencyRequirement::LandedCommit,_=>return Err(StoreError::Corrupt("unknown dependency requirement".into()))};
        queue[*index.get(&task).ok_or_else(||StoreError::Corrupt("dependency has no queue record".into()))?].dependencies.push(Dependency{predecessor:TaskId::new(predecessor).map_err(StoreError::Corrupt)?,requirement});
    }
    graph(&read_tasks(db)?,&queue)?;Ok(SchedulerSnapshot{policy,queue})
}
impl SqliteStore {
    pub fn set_scheduler_policy(&mut self,head_expected:u64,revision:u64,max_active_workers:u32,max_attempts_per_task:u32)->Result<u64> {
        if max_active_workers>1024||!(1..=32).contains(&max_attempts_per_task){return Err(invalid("invalid scheduler limits"));}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;if head(&tx)?!=head_expected{return Err(StoreError::Conflict);}
        let old=read(&tx)?.policy;if old.revision!=revision{return Err(StoreError::Conflict);}
        if old.max_active_workers==max_active_workers&&old.max_attempts_per_task==max_attempts_per_task{return Ok(head_expected);}
        let policy=SchedulerPolicy{revision:revision.checked_add(1).ok_or_else(||invalid("policy revision exhausted"))?,max_active_workers,max_attempts_per_task};
        tx.execute("UPDATE scheduler_policy SET revision=?1,max_active_workers=?2,max_attempts_per_task=?3 WHERE singleton=1",params![integer(policy.revision)?,max_active_workers,max_attempts_per_task])?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('scheduler.policy_changed','scheduler',?1,1,?2)",params![integer(policy.revision)?,serde_json::to_string(&policy).map_err(|e|invalid(&e.to_string()))?])?;
        let result=head(&tx)?;tx.commit()?;Ok(result)
    }
    pub fn queue_task(&mut self,id:&TaskId,revision:u64,head_expected:u64,request:&QueueRequest,now:i64)->Result<u64> {
        super::delivery::now_check(now)?;if !(-20..=20).contains(&request.priority)||request.dependencies.len()>256{return Err(invalid("queue priority/dependency limits exceeded"));}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;if head(&tx)?!=head_expected{return Err(StoreError::Conflict);}
        let tasks=read_tasks(&tx)?;let mut task=tasks.iter().find(|t|&t.id==id&&t.revision==revision).cloned().ok_or(StoreError::Conflict)?;
        if task.active_attempt.is_some()||matches!(task.state,TaskState::Running|TaskState::Succeeded|TaskState::Cancelled)||read_attempts(&tx)?.iter().any(|a|&a.task==id&&a.retains_capacity()){return Err(invalid("task execution or terminal disposition must be reconciled before queueing"));}
        let mut queue=read(&tx)?.queue;let previous=queue.iter().find(|q|&q.task==id).cloned();let mut dependencies=request.dependencies.clone();dependencies.sort_by(|a,b|a.predecessor.cmp(&b.predecessor));
        if previous.as_ref().is_some_and(|p|p.priority==request.priority&&p.dependencies==dependencies)&&task.state==TaskState::Queued{return Ok(head_expected);}
        let record=QueueRecord{task:id.clone(),priority:request.priority,enqueued_unix_ms:previous.as_ref().map(|p|p.enqueued_unix_ms).unwrap_or(now),enqueue_sequence:previous.as_ref().map(|p|p.enqueue_sequence).unwrap_or(head_expected.checked_add(1).ok_or_else(||invalid("sequence exhausted"))?),dependencies};
        queue.retain(|q|&q.task!=id);queue.push(record.clone());graph(&tasks,&queue)?;
        task.revision=revision.checked_add(1).ok_or_else(||invalid("task revision exhausted"))?;task.state=TaskState::Queued;
        tx.execute("UPDATE tasks SET revision=?2,state='queued' WHERE id=?1",params![id.as_str(),integer(task.revision)?])?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('task.changed',?1,?2,1,?3)",params![id.as_str(),integer(task.revision)?,serde_json::to_string(&task).map_err(|e|invalid(&e.to_string()))?])?;
        tx.execute("INSERT INTO task_queue VALUES(?1,?2,?3,?4) ON CONFLICT(task_id) DO UPDATE SET priority=excluded.priority",params![id.as_str(),record.priority,record.enqueued_unix_ms,integer(record.enqueue_sequence)?])?;
        tx.execute("DELETE FROM task_dependencies WHERE task_id=?1",[id.as_str()])?;
        for edge in &record.dependencies {tx.execute("INSERT INTO task_dependencies VALUES(?1,?2,?3)",params![id.as_str(),edge.predecessor.as_str(),edge.requirement.as_str()])?;}
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('scheduler.task_queued',?1,?2,1,?3)",params![id.as_str(),integer(task.revision)?,serde_json::to_string(&record).map_err(|e|invalid(&e.to_string()))?])?;
        let result=head(&tx)?;tx.commit()?;Ok(result)
    }
    pub fn queue_report(&mut self,now:i64)->Result<QueueReport> {
        super::delivery::now_check(now)?;let tx=self.connection.transaction()?;schema(&tx)?;let snapshot=read(&tx)?;let tasks=read_tasks(&tx)?;let attempts=read_attempts(&tx)?;let control=super::control::read(&tx)?;
        let retained_attempts=attempts.iter().filter(|a|a.retains_capacity()).count();let available_slots=(snapshot.policy.max_active_workers as usize).saturating_sub(retained_attempts);let mut entries=Vec::new();
        for record in &snapshot.queue {
            let task=tasks.iter().find(|t|t.id==record.task).ok_or(StoreError::Conflict)?;let mut blockers=Vec::new();
            if task.state!=TaskState::Queued {blockers.push(format!("task_state:{:?}",task.state));}
            if control.state!=ProjectState::Active||control.reconciliation_required {blockers.push("project_not_admitted".into());}
            if available_slots==0{blockers.push("capacity_full".into());}
            if attempts.iter().any(|a|a.task==task.id&&a.retains_capacity()){blockers.push("task_capacity_retained".into());}
            if attempts.iter().filter(|a|a.task==task.id).count()>=snapshot.policy.max_attempts_per_task as usize {blockers.push("attempt_limit".into());}
            for edge in &record.dependencies {
                let predecessor=tasks.iter().find(|t|t.id==edge.predecessor).ok_or(StoreError::Conflict)?;
                let reason=if matches!(predecessor.state,TaskState::Failed|TaskState::Cancelled){"predecessor_failed"}else{"verified_dependency_evidence_unavailable"};blockers.push(format!("{reason}:{}:{}",edge.predecessor.as_str(),edge.requirement.as_str()));
            }
            // Profile/authority and verified-result producers are later W04/W07
            // work. Queue eligibility must not manufacture their evidence.
            blockers.push("launch_preparation_unavailable".into());
            let age=now.saturating_sub(record.enqueued_unix_ms).max(0)/60_000;let score=age+record.priority as i64;
            entries.push((record.enqueue_sequence,QueueEntry{task:task.id.clone(),task_revision:task.revision,effective_priority:score,blockers}));
        }
        entries.sort_by(|a,b|b.1.effective_priority.cmp(&a.1.effective_priority).then(a.0.cmp(&b.0)).then(a.1.task.cmp(&b.1.task)));
        let report=QueueReport{head:head(&tx)?,policy:snapshot.policy,retained_attempts,available_slots,launch_enabled:false,entries:entries.into_iter().map(|(_,e)|e).collect()};tx.commit()?;Ok(report)
    }
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
    let raw=Connection::open(&path).unwrap();raw.execute_batch("DROP TRIGGER operation_delivery_monotonic; DROP TABLE attempt_cancellations; DROP TABLE attempt_inputs; DROP TABLE task_dependencies; DROP TABLE task_queue; DROP TABLE scheduler_policy; UPDATE store_meta SET schema_version=9; PRAGMA user_version=9;").unwrap();drop(raw);let mut db=SqliteStore::open(&path).unwrap();db.upgrade_v1().unwrap();assert_eq!(db.read_snapshot(None).unwrap().tasks,before.tasks);let head=db.read_snapshot(None).unwrap().head;db.queue_task(&TaskId::new("task-0").unwrap(),1,head,&QueueRequest{priority:0,dependencies:vec![]},0).unwrap();assert_eq!(db.queue_report(0).unwrap().entries.len(),1);
}
