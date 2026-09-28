use serde::{Deserialize,Serialize};
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize,Default)]
#[serde(default)]
pub struct InboxContent {
    pub id:String,pub kind:String,pub subject:String,pub created:String,pub summary:String,pub body:String,
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct InboxItem { pub revision:u64,pub content:InboxContent,pub seen:bool,pub done:bool }
/// Outcome of a memory-review reminder delivery. `AlreadyDelivered` means a
/// row with this stable id and identical delivery bytes committed earlier, so
/// a retry between the insert commit and the caller's counter update must
/// advance that counter exactly once, never insert again.
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub enum ReminderOutcome { Delivered, AlreadyDelivered }
impl InboxContent {
    pub fn validate(&self)->Result<(),String> {
        if self.id.is_empty() || self.id.len()>512 || self.id.starts_with('.') || self.id.contains("..") || !self.id.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_.".contains(&b)) {return Err("invalid inbox identity".into());}
        Ok(())
    }
    pub(crate) fn same_delivery(&self,other:&Self)->bool {
        self.id==other.id && self.kind==other.kind && self.subject==other.subject && self.summary==other.summary && self.body.trim_end()==other.body.trim_matches('\n').trim_end()
    }
}
