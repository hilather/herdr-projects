//! Durable evidence that one agent-start command may have been submitted.
use serde::{Serialize,Deserialize};
pub use crate::prompt_claim::Phase;
#[derive(Debug,Clone,Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub sequence:u64,pub generation:u64,pub execution:String,pub arguments_digest:String,pub route_digest:String,
    pub terminal:String,pub phase:Phase,pub error:String,pub notified:bool,
}
impl Claim {
    pub fn validate(&self,sequence:u64)->anyhow::Result<()> {
        anyhow::ensure!(self.generation<=i64::MAX as u64&&self.sequence==sequence&&sequence>0&&sequence<=i64::MAX as u64,"invalid launch claim sequence");
        for digest in [&self.execution,&self.arguments_digest,&self.route_digest] {anyhow::ensure!(digest.len()==64&&digest.bytes().all(|b|b.is_ascii_hexdigit()),"invalid launch claim digest");}
        anyhow::ensure!(!self.terminal.is_empty()&&self.terminal.len()<=256&&!self.terminal.chars().any(char::is_control)&&self.error.len()<=4096,"invalid launch claim identity");
        anyhow::ensure!(match self.phase {Phase::Pending=>self.error.is_empty()&&!self.notified,Phase::Confirmed=>self.error.is_empty()&&self.notified,Phase::Uncertain=>!self.error.is_empty()},"invalid launch claim outcome");Ok(())
    }
}
