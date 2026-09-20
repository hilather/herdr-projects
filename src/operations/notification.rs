//! Immutable, explicitly requested session notifications. Never terminal input.
use anyhow::{Context,Result,ensure};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
use crate::{domain::{Operation,OperationId,ProjectState,RuntimeBinding,RuntimeRoute,Snapshot,TaskId},migration::ConfigReference};

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    pub authority:String,
    pub binding_revision:u64,
    pub control_epoch:u64,
    pub config:ConfigReference,
    pub inbox_ids:Vec<String>,
    pub title:String,
    pub body:String,
}
fn unseen(snapshot:&Snapshot)->Vec<String> {
    let mut ids:Vec<_>=snapshot.inbox.iter().filter(|i|!i.seen&&!i.done).map(|i|i.content.id.clone()).collect();ids.sort();ids
}
fn identity(ids:&[String])->String {format!("notify-{:x}",Sha256::digest(serde_json::to_vec(ids).expect("string list is serializable")))}

pub fn build(snapshot:&Snapshot,task:&TaskId,project_slug:&str,config:ConfigReference,now:i64)->Result<Operation> {
    let binding=snapshot.runtime_bindings.iter().find(|b|b.id=="coordinator").context("register a coordinator notification route first")?;
    let task=snapshot.tasks.iter().find(|t|&t.id==task).context("task not found")?;
    let inbox_ids=unseen(snapshot);ensure!(!inbox_ids.is_empty(),"no unseen inbox items");ensure!(inbox_ids.len()<=1000,"notification batch exceeds 1000 inbox items");
    let id=identity(&inbox_ids);
    let notification=Notification{authority:"operator.session_notification".into(),binding_revision:binding.revision,control_epoch:snapshot.control.as_ref().context("upgrade-store required")?.epoch,config,inbox_ids,title:format!("herdr-projects: {project_slug}"),body:format!("{} new inbox item(s). The coordinator reads them at its next turn.",unseen(snapshot).len())};
    let op=Operation{id:OperationId::new(id.clone()).map_err(anyhow::Error::msg)?,task:Some(task.id.clone()),kind:"runtime.notification".into(),target:"coordinator".into(),payload_version:1,payload:serde_json::to_value(&notification)?,expected_revision:task.revision,due_unix_ms:now,idempotency_key:id};
    notification.validate(&op,snapshot,&notification.config)?;Ok(op)
}
impl Notification {
    pub fn decode(operation:&Operation)->Result<Self> {
        ensure!(operation.kind=="runtime.notification"&&operation.payload_version==1&&operation.target=="coordinator","unsupported notification operation");
        serde_json::from_value(operation.payload.clone()).context("invalid notification payload")
    }
    pub fn validate<'a>(&self,operation:&Operation,snapshot:&'a Snapshot,config:&ConfigReference)->Result<&'a RuntimeBinding> {
        ensure!(self.authority=="operator.session_notification","notification lacks explicit operator scope");
        ensure!(&self.config==config,"notification config reference changed");
        ensure!(self.title.starts_with("herdr-projects: ")&&self.title.len()<=256&&!self.title.chars().any(char::is_control),"invalid notification title");
        ensure!(!self.inbox_ids.is_empty()&&self.inbox_ids.len()<=1000&&self.inbox_ids==unseen(snapshot),"notification inbox set changed");
        ensure!(self.body==format!("{} new inbox item(s). The coordinator reads them at its next turn.",self.inbox_ids.len()),"invalid notification body");
        let id=identity(&self.inbox_ids);ensure!(operation.id.as_str()==id&&operation.idempotency_key==id,"notification identity mismatch");
        let control=snapshot.control.as_ref().context("upgrade-store required")?;
        ensure!(control.state==ProjectState::Active&&!control.reconciliation_required&&control.epoch==self.control_epoch&&control.config_digest==config.digest,"notification lifecycle admission changed");
        ensure!(snapshot.tasks.iter().any(|t|Some(&t.id)==operation.task.as_ref()&&t.revision==operation.expected_revision),"notification task changed");
        let binding=snapshot.runtime_bindings.iter().find(|b|b.id=="coordinator"&&b.revision==self.binding_revision&&b.task.is_none()).context("notification route changed")?;
        RuntimeRoute::from_identity(&binding.identity).validate().map_err(anyhow::Error::msg)?;
        ensure!(binding.identity.machine.is_empty()&&std::path::Path::new(&binding.identity.socket).is_absolute(),"notification requires an explicit local session socket");
        Ok(binding)
    }
}
