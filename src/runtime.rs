//! Store-backed operator commands while dispatch remains frozen for reconciliation.
use anyhow::{Context,Result,ensure};
use std::path::Path;
use crate::{domain::{Commit,Mutation,Snapshot,Task,TaskId,TaskState},migration};

pub fn snapshot(project:&Path)->Result<Snapshot> { migration::open_active(project)?.read_snapshot(None).map_err(Into::into) }
pub fn add_task(project:&Path,id:TaskId,title:String,expected_head:u64)->Result<u64> {
    ensure!(!title.trim().is_empty() && title.len()<=16_000,"title must contain 1–16000 bytes");
    let _maintenance=migration::maintenance(project)?;
    let mut db=migration::open_active(project)?;
    Ok(db.commit(Commit{expected_head,mutations:vec![Mutation::Task{expected:None,next:Task{id,revision:1,state:TaskState::Draft,title,active_attempt:None}}]})?)
}
pub fn rename_task(project:&Path,id:&TaskId,title:String,expected_revision:u64,expected_head:u64)->Result<u64> {
    ensure!(!title.trim().is_empty() && title.len()<=16_000,"title must contain 1–16000 bytes");
    let _maintenance=migration::maintenance(project)?;
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
    let mut text=format!("Runtime owner: SQLite; event head {}. Dispatch is blocked pending reconciliation.\nUse `task PROJECT list/show/add/rename` for task state; inspect `operations PROJECT inspect` for durable obligations.\nTASKS.md and thread/runtime legacy records are pre-cutover originals: do not edit them as live state.\nMEMORY.md and memory Markdown remain authoritative and editable; do not infer verified outcomes from narrative reports.\n\n",snapshot.head);
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
    let _maintenance=migration::maintenance(project)?;
    Ok(migration::open_active(project)?.drain_inbox(expected_head,jiff::Timestamp::now().as_millisecond())?)
}
pub fn update_inbox(project:&Path,expected_head:u64,ids:&[String],done:bool)->Result<usize> {
    let _maintenance=migration::maintenance(project)?;
    Ok(migration::open_active(project)?.update_inbox(expected_head,ids,done)?)
}

/// Expiry records ambiguity only, so it is safe during the dispatch freeze.
pub fn expire_operations(project:&Path)->Result<usize> {
    let _maintenance=migration::maintenance(project)?;
    Ok(migration::open_active(project)?.expire_claims(jiff::Timestamp::now().as_millisecond())?)
}

pub fn observe_imported_receipts(project:&Path,expected_head:Option<u64>)->Result<crate::operations::receipts::ReceiptReport> {
    let _maintenance=migration::maintenance(project)?;
    let mut db=migration::open_active(project)?;
    let head=match expected_head {Some(head)=>head,None=>db.read_snapshot(None)?.head};
    Ok(db.observe_imported_receipts(head,jiff::Timestamp::now().as_millisecond(),expected_head.is_some())?)
}
