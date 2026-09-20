//! Observation is evidence, not execution authority. An absent pane alone cannot
//! prove worker termination or release a reservation.
use serde::{Deserialize,Serialize};
#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize,Default)]
#[serde(rename_all="snake_case")]
pub enum ResourceState { #[default] Unrecorded, Unknown, Absent, Mismatch, Present }
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize,Default)]
#[serde(deny_unknown_fields)]
pub struct RuntimeObservation {
    pub binding:String,
    pub binding_revision:u64,
    pub task_revision:Option<u64>,
    pub observed_unix_ms:i64,
    pub pane:ResourceState,
    pub worktree:ResourceState,
    pub agent_present:bool,
    /// Versioned collector identity only; raw command output is never persisted.
    pub collector:String,
    pub config_digest:Option<String>,
    pub diagnostic:String,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub session_identity:Option<crate::domain::ResourceIdentity>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub worktree_identity:Option<crate::domain::ResourceIdentity>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub agent_identity:Option<crate::domain::AgentIdentity>,
}
impl RuntimeObservation {
    pub(crate) fn validate(&self)->Result<(),String> {
        if self.binding.is_empty() || self.binding.len()>512 || self.binding_revision==0 || self.task_revision==Some(0)
            || self.observed_unix_ms<0 || !matches!(self.collector.as_str(),"herdr-git-v1"|"herdr-git-v2") || self.diagnostic.len()>8192
            || self.config_digest.as_ref().is_some_and(|d|d.len()!=64||!d.bytes().all(|b|b.is_ascii_hexdigit())) {
            return Err("invalid runtime observation".into());
        }
        if self.session_identity.iter().chain(self.worktree_identity.iter()).any(|i|i.born_nanos>=1_000_000_000)
            || self.agent_identity.as_ref().is_some_and(|a|a.kind.len()>128||a.name.len()>512||a.kind.chars().chain(a.name.chars()).any(char::is_control))
            || (self.collector=="herdr-git-v1"&&(self.session_identity.is_some()||self.worktree_identity.is_some()||self.agent_identity.is_some())) {
            return Err("invalid resource incarnation evidence".into());
        }
        if self.agent_identity.is_some()&&!self.agent_present {return Err("agent identity requires a present agent".into());}
        if self.agent_present && self.pane!=ResourceState::Present {return Err("agent cannot establish mismatched/absent ownership".into());}
        Ok(())
    }
}
#[derive(Debug,Serialize)]
pub struct ObservationBatch {
    pub expected_head:u64,
    pub observations:Vec<RuntimeObservation>,
    pub dispatch_allowed:bool,
    pub recorded_head:Option<u64>,
}
