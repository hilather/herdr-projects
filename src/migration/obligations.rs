//! Preserve legacy accepted intent without assuming an error proves no effect.
use super::*;
use crate::domain::{Operation,OperationId};
use serde_json::Value;
#[derive(Debug,Default,Deserialize)]
#[serde(default)]
struct Retry { attempts:u32,next_attempt:String,last_error:String,blocked:bool }
#[derive(Debug,Deserialize)]
struct PendingEvent { id:String,kind:String,subject:String,summary:String,body:String,#[serde(default)] retry:Retry }
#[derive(Debug,Deserialize)]
struct PendingFinalization { operation_id:String,fingerprint:String,pr:String,reason:String,#[serde(default)] retry:Retry,#[serde(default)] partial_noted:bool }
#[derive(Debug,Default,Deserialize)]
#[serde(default)]
struct Notification { hash:String,retry:Retry }
const PROJECT_TASK:&str="legacy-project-obligations";
fn due(retry:&Retry)->Result<i64> {
    if retry.next_attempt.is_empty(){return Ok(0);}
    let at: jiff::Timestamp=retry.next_attempt.parse().context("invalid legacy retry timestamp")?;
    let ms=at.as_millisecond();ensure!(ms>=0,"negative legacy retry timestamp");Ok(ms)
}
fn operation(kind:&str,key:&str,task:TaskId,payload:Value,retry:&Retry)->Result<Operation> {
    ensure!(!key.trim().is_empty(),"empty legacy intent identity");
    // These fields remain inside payload as provenance as well as delivery state.
    let _ = (&retry.last_error,retry.blocked);
    Ok(Operation{id:OperationId::new(crate::operations::legacy_id(kind,key)).map_err(anyhow::Error::msg)?,task:Some(task),kind:kind.into(),target:key.into(),payload_version:1,payload,expected_revision:1,due_unix_ms:due(retry)?,idempotency_key:format!("{kind}:{key}")})
}
pub(super) fn convert(project:&Path,sources:&[Source],tasks:&mut Vec<Task>)->Result<Vec<Operation>> {
    let path=project.join(".state/ticker.json");
    let value:Value=if exists(&path) {serde_json::from_slice(&read(&path)?)?} else {serde_json::json!({})};
    let mut operations=Vec::new();
    for source in sources.iter().filter(|s|s.kind=="thread") {
        let bytes=read(&project.join(&source.path))?;
        ensure!(hash(&bytes)==source.digest,"thread changed during obligation conversion");
        let value:toml::Value=toml::from_str(std::str::from_utf8(&bytes)?)?;
        let receipt=value.get("copy_receipt").map(|v|v.clone().try_into::<crate::copy_receipt::CopyReceipt>()).transpose()?;
        if let Some(receipt)=&receipt { receipt.validate()?; }
        if let Some(pending)=value.get("pending_copy_notice") {
            let notice:crate::copy_receipt::CopyNotice=pending.clone().try_into()?;
            let id=value.get("id").and_then(|v|v.as_str()).context("missing copy notice thread")?;
            notice.validate(id,receipt.as_ref().context("copy notice has no receipt")?)?;
            let task=TaskId::new(format!("legacy-{id}")).map_err(anyhow::Error::msg)?;
            ensure!(tasks.iter().any(|t|t.id==task),"copy notice refers to unknown thread");
            operations.push(operation("legacy.inbox",&notice.id,task,serde_json::to_value(&notice)?,&Retry::default())?);
        }
        if let Some(pending)=value.get("pending_status_notice") {
            let notice:crate::status_notice::StatusNotice=pending.clone().try_into()?;
            let id=value.get("id").and_then(|v|v.as_str()).context("missing status notice thread")?;
            let sequence=value.get("status_notice_sequence").and_then(|v|v.as_integer()).context("missing status notice sequence")?;
            ensure!(sequence>=0,"negative status notice sequence");
            notice.validate(id,sequence as u64)?;
            let task=TaskId::new(format!("legacy-{id}")).map_err(anyhow::Error::msg)?;
            ensure!(tasks.iter().any(|t|t.id==task),"status notice refers to unknown thread");
            operations.push(operation("legacy.inbox",&notice.id,task,serde_json::to_value(&notice)?,&Retry::default())?);
        }
    }
    if let Some(events)=value.get("pending_events") {
        for (key,value) in events.as_object().context("pending_events must be an object")? {
            let event:PendingEvent=serde_json::from_value(value.clone())?;
            ensure!(!event.id.is_empty() && key==&event.id,"pending event map/id mismatch");
            let _ = (&event.kind,&event.subject,&event.summary,&event.body);
            operations.push(operation("legacy.inbox",key,TaskId::new(PROJECT_TASK).unwrap(),value.clone(),&event.retry)?);
        }
    }
    if let Some(finalizations)=value.get("finalizations") {
        for (key,value) in finalizations.as_object().context("finalizations must be an object")? {
            let record:PendingFinalization=serde_json::from_value(value.clone())?;
            ensure!(!record.operation_id.is_empty()&&!record.fingerprint.is_empty()&&!record.pr.is_empty(),"incomplete finalization identity");
            let task=TaskId::new(format!("legacy-{key}")).map_err(anyhow::Error::msg)?;
            ensure!(tasks.iter().any(|t|t.id==task),"finalization refers to unknown thread");
            let _ = (&record.reason,record.partial_noted);
            operations.push(operation("legacy.finalize",&record.operation_id,task,value.clone(),&record.retry)?);
        }
    }
    if let Some(value)=value.get("notification_retry") {
        let notification:Notification=serde_json::from_value(value.clone())?;
        if !notification.hash.is_empty() {
            operations.push(operation("legacy.notify",&notification.hash,TaskId::new(PROJECT_TASK).unwrap(),value.clone(),&notification.retry)?);
        } else {ensure!(notification.retry.attempts==0&&notification.retry.next_attempt.is_empty()&&notification.retry.last_error.is_empty()&&!notification.retry.blocked,"notification retry has no hash");}
    }
    if operations.iter().any(|o|o.task.as_ref().map(TaskId::as_str)==Some(PROJECT_TASK)) {
        tasks.push(Task{id:TaskId::new(PROJECT_TASK).unwrap(),revision:1,state:TaskState::Blocked,title:"Imported project delivery obligations; reconciliation required".into(),active_attempt:None});
    }
    let mut ids=BTreeSet::new();for op in &operations {ensure!(ids.insert(op.id.clone()),"duplicate imported operation identity");}
    operations.sort_by(|a,b|a.id.cmp(&b.id));Ok(operations)
}
