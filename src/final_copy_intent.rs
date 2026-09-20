//! Exact retained bytes and purpose of an interrupted automatic final copy.
use anyhow::{Result,ensure};
use serde::{Serialize,Deserialize};
use crate::copy_receipt::CopyReceipt;

#[derive(Debug,Clone,Serialize,Deserialize,PartialEq)]
#[serde(tag="kind",rename_all="kebab-case",deny_unknown_fields)]
pub enum Purpose {
    Idle {days:u32,started:String,last_state_change:String,last_report_change:String},
    Merged {pr:String},
}
impl Purpose {
    pub fn validate(&self)->Result<()> {
        match self {
            Self::Idle{days,started,last_state_change,last_report_change}=>{
                ensure!(*days>0&&started.len()<=64,"invalid idle finalization interval");started.parse::<jiff::Timestamp>()?;
                for stamp in [last_state_change,last_report_change] {ensure!(stamp.len()<=64,"invalid idle timestamp");if !stamp.is_empty(){stamp.parse::<jiff::Timestamp>()?;}}
                ensure!(!last_state_change.is_empty()||!last_report_change.is_empty(),"idle finalization has no observation timestamp");
            },
            Self::Merged{pr}=>ensure!(!pr.is_empty()&&pr.len()<=4096&&!pr.chars().any(char::is_control),"invalid merged finalization target"),
        }
        Ok(())
    }
    pub fn reason(&self)->&'static str {match self {Self::Idle{..}=>"auto",Self::Merged{..}=>"merged"}}
}
#[derive(Debug,Clone,Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FinalCopyIntent {
    pub sequence:u64,pub execution:String,pub authority:String,pub previous_hash:String,
    pub previous_receipt:Option<CopyReceipt>,pub report_hash:Option<String>,
    pub stage_digest:String,pub snapshot:Option<String>,pub operation:String,pub purpose:Purpose,
}
impl FinalCopyIntent {
    pub fn validate(&self)->Result<()> {
        ensure!(self.sequence>0&&self.sequence<=i64::MAX as u64,"invalid final-copy sequence");
        for hash in [&self.execution,&self.authority,&self.stage_digest].into_iter().chain(self.report_hash.iter()).chain(self.snapshot.iter()) {
            ensure!(hash.len()==64&&hash.bytes().all(|b|b.is_ascii_hexdigit()),"invalid final-copy digest");
        }
        ensure!(self.previous_hash.len()<=256&&!self.operation.is_empty()&&self.operation.len()<=256&&!self.operation.chars().any(char::is_control),"invalid final-copy identity");
        if let Some(receipt)=&self.previous_receipt {receipt.validate()?;}
        self.purpose.validate()
    }
    pub fn stage_name(&self,thread:&str)->String {format!("final-{thread}-{}-{}",self.sequence,self.stage_digest)}
}
#[derive(Debug,Clone,Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Notice {pub id:String,pub sequence:u64,pub execution:String,pub summary:String,pub body:String}
impl Notice {
    pub fn validate(&self,thread:&str,sequence:u64)->Result<()> {
        ensure!(self.sequence==sequence&&sequence>0&&self.execution.len()==64&&self.execution.bytes().all(|b|b.is_ascii_hexdigit())&&self.id==format!("final-{thread}-{}-{sequence}",self.execution),"final-copy notice identity mismatch");
        ensure!(self.id.len()<=256&&!self.id.is_empty()&&self.id.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'-'),"invalid final-copy notice identity");
        ensure!(self.summary.len()<=8192&&self.body.len()<=32768,"final-copy notice exceeds bounds");Ok(())
    }
}
