use serde::{Deserialize,Serialize};
use super::TaskId;

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum RuntimeVerification { Unverified }

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize,Default)]
#[serde(default)]
pub struct RuntimeIdentity {
    pub machine:String,
    /// Historical recorded socket only. Empty never resolves to an ambient session.
    pub socket:String,
    pub workspace_id:String,
    pub tab_id:String,
    pub pane_id:String,
    pub cwd:String,
    pub repo:String,
    pub branch:String,
    pub worktree_path:String,
    pub thread_dir:String,
    pub agent:String,
    pub agent_name:String,
    pub legacy_status:String,
    pub execution_fingerprint:Option<String>,
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct RuntimeBinding {
    pub id:String,
    pub task:Option<TaskId>,
    /// Revision of this runtime record, not the task or legacy lifecycle generation.
    pub revision:u64,
    pub source_path:String,
    pub source_digest:String,
    /// Thread sockets also depend on the preserved coordinator record.
    pub session_source_digest:Option<String>,
    pub verification:RuntimeVerification,
    pub identity:RuntimeIdentity,
}
