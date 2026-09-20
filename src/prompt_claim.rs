//! Historical claims for terminal brief submission.
use serde::{Serialize,Deserialize};
#[derive(Debug,Clone,Serialize,Deserialize,PartialEq)]
#[serde(rename_all="kebab-case")]
pub enum Phase {Pending,Confirmed,Uncertain}
#[derive(Debug,Clone,Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Claim {pub sequence:u64,pub execution:String,pub prompt:String,pub phase:Phase,pub error:String,pub notified:bool}
impl Claim {
    pub fn validate(&self,sequence:u64)->anyhow::Result<()> {
        anyhow::ensure!(self.sequence==sequence&&sequence>0&&sequence<=i64::MAX as u64,"invalid brief claim sequence");
        anyhow::ensure!(self.execution.len()==64&&self.execution.bytes().all(|b|b.is_ascii_hexdigit())&&!self.prompt.is_empty()&&self.prompt.len()<=32768&&self.error.len()<=4096,"invalid brief claim");
        anyhow::ensure!(match self.phase {Phase::Pending=>self.error.is_empty()&&!self.notified,Phase::Confirmed=>self.error.is_empty()&&self.notified,Phase::Uncertain=>!self.error.is_empty()},"invalid brief claim outcome");Ok(())
    }
}
