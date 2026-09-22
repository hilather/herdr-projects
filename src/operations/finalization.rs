//! Artifact finalization receipts mean preserved bytes, never verified success.
use anyhow::{Context,Result,ensure};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
use crate::{domain::{Operation,OperationId,ProjectState,Snapshot,RuntimeBinding,TaskState},migration::ConfigReference};

pub fn digest(bytes:&[u8])->String {format!("{:x}",Sha256::digest(bytes))}
pub fn hash(value:&str)->bool {value.len()==64&&value.bytes().all(|b|b.is_ascii_hexdigit())}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finalization {
    pub authority:String,
    pub binding:String,
    pub binding_revision:u64,
    pub control_epoch:u64,
    pub config:ConfigReference,
    pub source:String,
    pub report_hash:String,
    pub reason:String,
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalizationReceipt {
    pub operation_hash:String,
    pub artifact_key:String,
    pub snapshot:String,
    pub report_hash:String,
    pub binding_revision:u64,
}
impl Finalization {
    pub fn decode(op:&Operation)->Result<Self> {
        ensure!(op.kind=="runtime.finalization"&&op.payload_version==1,"unsupported finalization operation");
        serde_json::from_value(op.payload.clone()).context("invalid finalization payload")
    }
    pub fn artifact_key(&self)->String {format!("runtime-{}",digest(self.binding.as_bytes()))}
    pub fn validate<'a>(&self,op:&Operation,snapshot:&'a Snapshot,config:&ConfigReference)->Result<&'a RuntimeBinding> {
        ensure!(self.authority=="operator.artifact_finalization"&&&self.config==config,"finalization authority or config changed");
        ensure!(hash(&self.report_hash)&&!self.reason.trim().is_empty()&&self.reason.len()<=4096,"invalid finalization evidence/reason");
        self.validate_state(op,snapshot.control.as_ref().context("upgrade-store required")?,&snapshot.tasks,&snapshot.attempts,&snapshot.runtime_bindings)
    }
    pub fn validate_state<'a>(&self,op:&Operation,control:&crate::domain::ProjectControl,tasks:&[crate::domain::Task],attempts:&[crate::domain::Attempt],bindings:&'a [RuntimeBinding])->Result<&'a RuntimeBinding> {
        ensure!(self.authority=="operator.artifact_finalization"&&hash(&self.report_hash)&&!self.reason.trim().is_empty()&&self.reason.len()<=4096,"invalid finalization scope or evidence");
        ensure!(control.state!=ProjectState::Archived&&control.epoch==self.control_epoch,"finalization lifecycle changed");
        let task=tasks.iter().find(|t|Some(&t.id)==op.task.as_ref()&&t.revision==op.expected_revision).context("finalization task changed")?;
        ensure!(task.active_attempt.is_none()&&!matches!(task.state,TaskState::Running|TaskState::Succeeded),"finalization task is active or already succeeded");
        ensure!(!attempts.iter().any(|a|a.task==task.id&&a.retains_capacity()),"attempt termination must be reconciled before finalization");
        let binding=bindings.iter().find(|b|b.id==self.binding&&b.revision==self.binding_revision&&b.task==op.task).context("finalization binding changed")?;
        ensure!(op.target==binding.id&&binding.identity.machine.is_empty()&&binding.identity.thread_dir==self.source&&std::path::Path::new(&self.source).is_absolute(),"finalization requires the recorded local artifact source");
        let id=self.operation_id(op.expected_revision)?;ensure!(op.id==id&&op.idempotency_key==id.as_str(),"finalization identity mismatch");Ok(binding)
    }
    fn operation_id(&self,revision:u64)->Result<OperationId> {OperationId::new(format!("finalize-{}",digest(&serde_json::to_vec(&(self,revision))?))).map_err(anyhow::Error::msg)}
    pub fn operation(self,snapshot:&Snapshot,now:i64)->Result<Operation> {
        let binding=snapshot.runtime_bindings.iter().find(|b|b.id==self.binding).context("binding not found")?;
        let task=binding.task.as_ref().context("coordinator cannot be finalized as a task")?;
        let task=snapshot.tasks.iter().find(|t|&t.id==task).context("task not found")?;
        let id=self.operation_id(task.revision)?;
        let op=Operation{id:id.clone(),task:Some(task.id.clone()),kind:"runtime.finalization".into(),target:self.binding.clone(),payload_version:1,payload:serde_json::to_value(&self)?,expected_revision:task.revision,due_unix_ms:now,idempotency_key:id.as_str().into()};self.validate(&op,snapshot,&self.config)?;Ok(op)
    }
}
impl FinalizationReceipt {
    pub fn validate(&self,op:&Operation,payload:&Finalization)->Result<()> {
        ensure!(self.operation_hash==digest(&serde_json::to_vec(op)?)&&self.artifact_key==payload.artifact_key()&&self.binding_revision==payload.binding_revision&&self.report_hash==payload.report_hash&&hash(&self.snapshot),"finalization receipt does not match immutable intent");Ok(())
    }
}
