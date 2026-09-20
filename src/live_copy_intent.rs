//! Durable reference to the exact bytes of an interrupted live projection.
use anyhow::{Result,ensure};
use serde::{Deserialize,Serialize};
use crate::copy_receipt::CopyReceipt;
#[derive(Debug,Clone,Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LiveCopyIntent {
    pub sequence:u64,
    pub execution:String,
    pub authority:String,
    pub previous_hash:String,
    pub previous_receipt:Option<CopyReceipt>,
    pub report_hash:String,
    pub stage_digest:String,
}
impl LiveCopyIntent {
    pub fn validate(&self)->Result<()> {
        ensure!(self.sequence>0&&self.sequence<=i64::MAX as u64,"invalid live-copy intent sequence");
        for hash in [&self.execution,&self.authority,&self.report_hash,&self.stage_digest] {ensure!(hash.len()==64&&hash.bytes().all(|b|b.is_ascii_hexdigit()),"invalid live-copy intent hash");}
        ensure!(self.previous_hash.len()<=256,"invalid previous report hash");
        if let Some(receipt)=&self.previous_receipt {receipt.validate()?;}
        Ok(())
    }
    pub fn stage_name(&self,thread:&str)->String {format!("{thread}-{}-{}",self.sequence,self.stage_digest)}
}
