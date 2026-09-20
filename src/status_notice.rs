//! Durable legacy status delivery, shared with the canonical importer.
use anyhow::{Result,ensure};
use serde::{Deserialize,Serialize};

#[derive(Debug,Clone,Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StatusNotice {
    pub id:String,
    pub kind:String,
    pub subject:String,
    pub summary:String,
    pub body:String,
    pub execution:String,
    pub sequence:u64,
    pub previous_group:String,
    pub next_group:String,
}
impl StatusNotice {
    pub fn validate(&self,thread_id:&str,sequence:u64)->Result<()> {
        ensure!(self.subject==thread_id && self.kind=="thread-state","invalid status notice subject/kind");
        ensure!(self.sequence>0 && self.sequence==sequence,"invalid status notice sequence");
        ensure!(self.execution.len()==64 && self.execution.bytes().all(|b|b.is_ascii_hexdigit()),"invalid status notice execution");
        ensure!(self.id==format!("status-{}-{}-{}",self.subject,self.execution,self.sequence),"invalid status notice identity");
        ensure!(!self.previous_group.is_empty() && self.previous_group!=self.next_group,"invalid status transition");
        ensure!(matches!(self.next_group.as_str(),"waiting-on-you"|"landing"|"idle"),"unsupported status notice group");
        Ok(())
    }
}
