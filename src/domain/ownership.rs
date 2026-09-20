use serde::{Deserialize,Serialize};
use super::AttemptId;

/// Filesystem incarnation: device/inode plus birth time, never a pathname alone.
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceIdentity {pub device:u64,pub inode:u64,pub born_secs:u64,pub born_nanos:u32}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIdentity {pub kind:String,pub name:String}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeOwnership {
    pub binding:String,
    pub revision:u64,
    pub binding_revision:u64,
    pub identity_digest:String,
    /// Adopted resources never acquire destructive cleanup authority.
    pub origin:String,
    pub attempt:Option<AttemptId>,
    pub session:Option<ResourceIdentity>,
    pub worktree:Option<ResourceIdentity>,
    pub agent:Option<AgentIdentity>,
    pub config_digest:Option<String>,
    pub observed_unix_ms:i64,
}
