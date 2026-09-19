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
    /// None for records created canonically after cutover.
    pub source_path:Option<String>,
    pub source_digest:Option<String>,
    /// Original imported session provenance; later route edits are audited separately.
    pub session_source_digest:Option<String>,
    pub verification:RuntimeVerification,
    pub identity:RuntimeIdentity,
}

/// Explicit operator replacement of session routing, never an ownership grant.
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize,Default)]
#[serde(default,deny_unknown_fields)]
pub struct RuntimeRoute {
    pub machine:String,
    pub socket:String,
    pub workspace_id:String,
    pub tab_id:String,
    pub pane_id:String,
    pub cwd:String,
}
impl RuntimeRoute {
    pub fn validate(&self)->Result<(),String> {
        let fields=[&self.machine,&self.socket,&self.workspace_id,&self.tab_id,&self.pane_id,&self.cwd];
        if fields.iter().any(|v|v.len()>4096||v.chars().any(char::is_control)) {return Err("runtime route fields must be bounded and contain no control characters".into());}
        if [&self.socket,&self.cwd].iter().any(|v|!v.is_empty()&&!std::path::Path::new(v).is_absolute()) {return Err("socket and cwd must be absolute or empty".into());}
        if self.machine.starts_with('-')||!self.machine.bytes().all(|b|b.is_ascii_alphanumeric()||b"._-:@[]".contains(&b)) {return Err("invalid machine identifier".into());}
        if [&self.workspace_id,&self.tab_id,&self.pane_id].iter().any(|s|!s.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_.:".contains(&b))) {return Err("invalid Herdr identity".into());}
        if !self.pane_id.is_empty()&&[&self.socket,&self.workspace_id,&self.tab_id,&self.cwd].iter().any(|s|s.is_empty()) {return Err("a recorded pane requires socket, workspace, tab and cwd".into());}
        Ok(())
    }
    pub fn from_identity(identity:&RuntimeIdentity)->Self {
        Self{machine:identity.machine.clone(),socket:identity.socket.clone(),workspace_id:identity.workspace_id.clone(),tab_id:identity.tab_id.clone(),pane_id:identity.pane_id.clone(),cwd:identity.cwd.clone()}
    }
}
#[derive(Debug,Serialize)]
pub struct RouteChange {
    pub head:u64,
    pub binding:RuntimeBinding,
    pub task_revision:Option<u64>,
}
