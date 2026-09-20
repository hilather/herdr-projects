//! Durable coordinator priming intent, separate from its mutable pane record.
use serde::{Serialize,Deserialize};
#[derive(Debug,Clone,Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Claim {pub request:u64,pub delivery:crate::prompt_claim::Claim}
impl Claim {
    pub fn validate(&self,sequence:u64,request:u64)->anyhow::Result<()> {
        anyhow::ensure!(self.request<=request&&request<=i64::MAX as u64,"invalid coordinator prime request");self.delivery.validate(sequence)
    }
}
