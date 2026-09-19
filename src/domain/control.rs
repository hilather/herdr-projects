use serde::{Deserialize,Serialize};
#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum ProjectState { Paused, Active, Archived }
impl ProjectState {
    pub(crate) fn as_str(self)->&'static str {match self{Self::Paused=>"paused",Self::Active=>"active",Self::Archived=>"archived"}}
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct ProjectControl {
    pub revision:u64,
    pub epoch:u64,
    pub state:ProjectState,
    pub reconciliation_required:bool,
    pub config_digest:Option<String>,
}
#[derive(Debug,Serialize)]
pub struct AdmissionReport { pub head:u64,pub blockers:Vec<String> }
#[derive(Debug,Serialize)]
pub struct ControlChange { pub head:u64,pub control:ProjectControl }
