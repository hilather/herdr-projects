use serde::{Deserialize,Serialize};
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize,Default)]
#[serde(default)]
pub struct InboxContent {
    pub id:String,pub kind:String,pub subject:String,pub created:String,pub summary:String,pub body:String,
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct InboxItem { pub revision:u64,pub content:InboxContent,pub seen:bool,pub done:bool }
impl InboxContent {
    pub fn validate(&self)->Result<(),String> {
        if self.id.is_empty() || self.id.len()>512 || self.id.starts_with('.') || self.id.contains("..") || !self.id.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_.".contains(&b)) {return Err("invalid inbox identity".into());}
        Ok(())
    }
    pub(crate) fn same_delivery(&self,other:&Self)->bool {
        self.id==other.id && self.kind==other.kind && self.subject==other.subject && self.summary==other.summary && self.body.trim_end()==other.body.trim_matches('\n').trim_end()
    }
}
