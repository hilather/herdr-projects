//! Store-backed operator commands while dispatch remains frozen for reconciliation.
use anyhow::{Context,Result,ensure};
use std::path::Path;
use crate::{domain::{Commit,Mutation,Snapshot,Task,TaskId,TaskState},migration};

pub fn snapshot(project:&Path)->Result<Snapshot> { migration::open_active(project)?.read_snapshot(None).map_err(Into::into) }
pub fn add_task(project:&Path,id:TaskId,title:String,expected_head:u64)->Result<u64> {
    ensure!(!title.trim().is_empty() && title.len()<=16_000,"title must contain 1–16000 bytes");
    let _maintenance=migration::runtime_mutation(project)?;
    let mut db=migration::open_active(project)?;
    Ok(db.commit(Commit{expected_head,mutations:vec![Mutation::Task{expected:None,next:Task{id,revision:1,state:TaskState::Draft,title,active_attempt:None}}]})?)
}
pub fn rename_task(project:&Path,id:&TaskId,title:String,expected_revision:u64,expected_head:u64)->Result<u64> {
    ensure!(!title.trim().is_empty() && title.len()<=16_000,"title must contain 1–16000 bytes");
    let _maintenance=migration::runtime_mutation(project)?;
    let mut db=migration::open_active(project)?;
    let snapshot=db.read_snapshot(Some(expected_head))?;
    let mut task=snapshot.tasks.into_iter().find(|t|&t.id==id).context("task not found")?;
    ensure!(task.revision==expected_revision,"task revision conflict");
    ensure!(task.active_attempt.is_none(),"cannot edit a task with an active attempt");
    task.revision=task.revision.checked_add(1).context("task revision exhausted")?;task.title=title;
    Ok(db.commit(Commit{expected_head,mutations:vec![Mutation::Task{expected:Some(expected_revision),next:task}]})?)
}
pub fn context(project:&Path)->Result<String> { Ok(context_snapshot(project)?.0) }
pub fn context_snapshot(project:&Path)->Result<(String,u64,Vec<String>)> {
    let snapshot=snapshot(project)?;
    let control=snapshot.control.as_ref().map(|c|format!("{:?}; epoch {}; reconciliation required: {}",c.state,c.epoch,c.reconciliation_required)).unwrap_or_else(||"schema upgrade required".into());
    let mut text=format!("Runtime owner: SQLite; event head {}. Control: {}. Existing resources require separate ownership authorization.\nUse `task PROJECT list/show/add/rename` for task state; inspect `operations PROJECT inspect` for durable obligations.\nTASKS.md and thread/runtime legacy records are pre-cutover originals: do not edit them as live state.\nMEMORY.md and memory Markdown remain authoritative and editable; do not infer verified outcomes from narrative reports.\n\n",snapshot.head,control);
    let unseen=snapshot.inbox.iter().filter(|i|!i.done&&!i.seen).map(|i|i.content.id.clone()).collect();
    for item in snapshot.inbox.iter().filter(|i|!i.done) {text.push_str(&format!("Inbox {}: {}\n{}\n",item.content.id,item.content.summary,item.content.body));}
    for task in snapshot.tasks {text.push_str(&format!("{} revision {} {:?}: {}\n",task.id.as_str(),task.revision,task.state,task.title.replace(['\n','\r']," ")));}
    for name in ["PROJECT.md","MEMORY.md"] {
        let bytes=migration::read_plan_file(&project.join(name))?;
        text.push_str(&format!("\n--- {name} (user-owned legacy text) ---\n{}\n",String::from_utf8(bytes)?));
    }
    Ok((text,snapshot.head,unseen))
}

pub fn drain_inbox(project:&Path,expected_head:u64)->Result<usize> {
    let _maintenance=migration::runtime_mutation(project)?;
    Ok(migration::open_active(project)?.drain_inbox(expected_head,jiff::Timestamp::now().as_millisecond())?)
}
pub fn update_inbox(project:&Path,expected_head:u64,ids:&[String],done:bool)->Result<usize> {
    let _maintenance=migration::runtime_mutation(project)?;
    Ok(migration::open_active(project)?.update_inbox(expected_head,ids,done)?)
}

/// Expiry records ambiguity only, so it is safe during the dispatch freeze.
pub fn expire_operations(project:&Path)->Result<usize> {
    let _maintenance=migration::runtime_mutation(project)?;
    Ok(migration::open_active(project)?.expire_claims(jiff::Timestamp::now().as_millisecond())?)
}

pub fn observe_imported_receipts(project:&Path,expected_head:Option<u64>)->Result<crate::operations::receipts::ReceiptReport> {
    let _maintenance=migration::runtime_mutation(project)?;
    let mut db=migration::open_active(project)?;
    let head=match expected_head {Some(head)=>head,None=>db.read_snapshot(None)?.head};
    Ok(db.observe_imported_receipts(head,jiff::Timestamp::now().as_millisecond(),expected_head.is_some())?)
}

pub fn record_observations(project:&Path,batch:&crate::reconcile::ObservationBatch)->Result<u64> {
    ensure!(!batch.dispatch_allowed,"observations cannot authorize dispatch");
    let _maintenance=migration::runtime_mutation(project)?;
    record_observations_held(project,batch)
}
/// Caller retains the root execution lease while committing live evidence.
pub fn record_observations_held(project:&Path,batch:&crate::reconcile::ObservationBatch)->Result<u64> {
    ensure!(!batch.dispatch_allowed,"observations cannot authorize dispatch");
    let mut db=migration::open_active(project)?;
    let head=db.record_observations(batch.expected_head,&batch.observations)?;
    migration::publish_control_marker(project,&db)?;Ok(head)
}

pub fn rebind(project:&Path,id:&str,expected_revision:u64,expected_head:u64,route:&crate::domain::RuntimeRoute)->Result<crate::domain::RouteChange> {
    let _maintenance=migration::runtime_mutation(project)?;
    let mut db=migration::open_active(project)?;
    let change=db.rebind_runtime(id,expected_revision,expected_head,route)?;
    migration::publish_control_marker(project,&db)?;
    Ok(change)
}


pub fn admission(project:&Path,config:&Path)->Result<crate::domain::AdmissionReport> {
    let config=migration::config_reference(config)?;
    Ok(migration::open_active(project)?.admission_report(jiff::Timestamp::now().as_millisecond(),config.digest.as_deref())?)
}
pub fn set_state(project:&Path,expected_head:u64,expected_revision:u64,state:crate::domain::ProjectState,config:&Path)->Result<crate::domain::ControlChange> {
    let _maintenance=migration::runtime_mutation(project)?;
    let config=if state==crate::domain::ProjectState::Active {migration::config_reference(config)?.digest}else{None};
    let mut db=migration::open_active(project)?;
    let change=db.set_project_state(expected_head,expected_revision,state,jiff::Timestamp::now().as_millisecond(),config.as_deref())?;
    migration::publish_control_marker(project,&db)?;
    Ok(change)
}

pub fn retire_operation(project:&Path,id:&crate::domain::OperationId,revision:u64,head:u64,reason:&str)->Result<crate::operations::Delivery> {
    let _maintenance=migration::runtime_mutation(project)?;
    Ok(migration::open_active(project)?.retire_operation(id,revision,head,reason,jiff::Timestamp::now().as_millisecond())?)
}

pub fn create_binding(project:&Path,task:Option<&TaskId>,task_revision:Option<u64>,expected_head:u64,route:&crate::domain::RuntimeRoute)->Result<crate::domain::RouteChange> {
    let _maintenance=migration::runtime_mutation(project)?;
    let mut db=migration::open_active(project)?;
    let change=db.create_runtime(task,task_revision,expected_head,route)?;
    migration::publish_control_marker(project,&db)?;
    Ok(change)
}

/// Explicit operator notification request; no external effect during enqueue.
pub fn enqueue_notification(project:&Path,task:&TaskId,expected_head:u64,config:&migration::ConfigReference)->Result<crate::domain::Operation> {
    let _maintenance=migration::runtime_mutation(project)?;
    ensure!(migration::config_reference(Path::new(&config.path))?==*config,"config changed before notification enqueue");
    let mut db=migration::open_active(project)?;
    let snapshot=db.read_snapshot(Some(expected_head))?;
    let slug=project.file_name().and_then(|s|s.to_str()).context("invalid project slug")?;
    let operation=crate::operations::notification::build(&snapshot,task,slug,config.clone(),jiff::Timestamp::now().as_millisecond())?;
    db.commit(Commit{expected_head,mutations:vec![Mutation::Enqueue(operation.clone())]})?;
    Ok(operation)
}

pub fn enqueue_finalization(project:&Path,expected_head:u64,operation:crate::domain::Operation)->Result<crate::domain::Operation> {
    let _maintenance=migration::runtime_mutation(project)?;let mut db=migration::open_active(project)?;
    let payload=crate::operations::finalization::Finalization::decode(&operation)?;
    let config=migration::config_reference(Path::new(&payload.config.path))?;
    payload.validate(&operation,&db.read_snapshot(Some(expected_head))?,&config)?;
    db.commit(Commit{expected_head,mutations:vec![Mutation::Enqueue(operation.clone())]})?;Ok(operation)
}

/// Caller retains the root execution lease after live/conflict inspection.
pub fn adopt_observed(project:&Path,id:&str,revision:u64,head:u64,config:&migration::ConfigReference)->Result<crate::store::OwnershipChange> {
    ensure!(migration::config_reference(Path::new(&config.path))?==*config,"config changed before adoption commit");
    let mut db=migration::open_active(project)?;
    let change=db.adopt_runtime(id,revision,head,jiff::Timestamp::now().as_millisecond(),config.digest.as_deref())?;
    migration::publish_control_marker(project,&db)?;Ok(change)
}

/// Explicitly withdraw an adopted claim, retaining the binding and audit history.
pub fn relinquish(project:&Path,id:&str,expected_revision:u64,expected_head:u64,reason:&str)->Result<u64> {
    let _maintenance=migration::runtime_mutation(project)?;
    let mut db=migration::open_active(project)?;
    let head=db.relinquish_runtime(id,expected_revision,expected_head,reason)?;
    migration::publish_control_marker(project,&db)?;Ok(head)
}
