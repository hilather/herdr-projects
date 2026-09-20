//! Durable legacy inbox delivery claims; uncertainty fences overlapping batches.
use serde::{Serialize,Deserialize};
use anyhow::{Result,ensure};
use sha2::{Digest,Sha256};
#[derive(Debug,Clone,PartialEq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {pub ids:Vec<String>,pub hash:String}
pub fn valid_id(id:&str)->bool {!id.is_empty()&&id.len()<=252&&!id.starts_with('.')&&!id.contains("..")&&id.chars().all(|c|c.is_ascii_alphanumeric()||matches!(c,'-'|'_'|'.'))}
pub fn digest(bytes:&[u8])->String {format!("{:x}",Sha256::digest(bytes))}
fn hash(s:&str)->bool{s.len()==64&&s.bytes().all(|b|b.is_ascii_hexdigit())}
impl Batch {
    pub fn new(ids:Vec<String>)->Result<Self>{let batch=Self{hash:digest(ids.join("\n").as_bytes()),ids};batch.validate()?;Ok(batch)}
    pub fn validate(&self)->Result<()> {
        ensure!(self.ids.len()<=1024&&self.ids.iter().map(String::len).sum::<usize>()<=48*1024,"notification batch exceeds identity budget");
        ensure!(self.ids.iter().all(|id|valid_id(id))&&self.ids.windows(2).all(|pair|pair[0]<pair[1]),"notification batch identities are invalid or unordered");
        ensure!(self.hash==digest(self.ids.join("\n").as_bytes()),"notification batch digest mismatch");Ok(())
    }
}
#[derive(Debug,Clone,Copy,PartialEq,Serialize,Deserialize)]
#[serde(rename_all="kebab-case")]
pub enum Mode {Nudge,Toast,Legacy}
#[derive(Debug,Clone,Copy,PartialEq,Serialize,Deserialize)]
#[serde(rename_all="kebab-case")]
pub enum Phase {Ready,Pending,Confirmed,Uncertain,Suppressed,NotShown}
#[derive(Debug,Clone,PartialEq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {pub sequence:u64,pub batch:Option<Batch>,pub mode:Mode,pub authority:String,pub payload:String,pub phase:Phase,pub retry_of:Option<u64>,pub error:String}
impl Claim {
    pub fn validate(&self,sequence:u64)->Result<()> {
        ensure!(self.sequence==sequence&&sequence>0&&sequence<=i64::MAX as u64,"invalid notification sequence");
        ensure!(self.retry_of.is_none_or(|n|n>0&&n<sequence)&&self.error.len()<=4096&&self.payload.len()<=32768,"invalid notification claim bounds");
        if self.mode==Mode::Legacy {ensure!(self.batch.is_none()&&self.authority.is_empty()&&self.payload.is_empty()&&matches!(self.phase,Phase::Uncertain|Phase::Suppressed),"invalid legacy notification uncertainty");}
        else {let batch=self.batch.as_ref().ok_or_else(||anyhow::anyhow!("notification batch missing"))?;batch.validate()?;ensure!(!batch.ids.is_empty()&&hash(&self.authority)&&!self.payload.is_empty(),"invalid notification authority");}
        ensure!(match self.phase {Phase::Ready=>self.retry_of.is_some()&&self.error.is_empty(),Phase::Pending|Phase::Confirmed=>self.error.is_empty(),Phase::Uncertain|Phase::Suppressed|Phase::NotShown=>!self.error.is_empty()},"invalid notification outcome");
        ensure!(self.phase!=Phase::NotShown||self.mode==Mode::Toast,"only native toast rejection proves not shown");Ok(())
    }
}
