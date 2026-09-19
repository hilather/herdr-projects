//! Observation is evidence, not execution authority. An absent pane alone cannot
//! prove worker termination or release a reservation.
use serde::{Deserialize,Serialize};
#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum ResourceState { Unrecorded, Unknown, Absent, Mismatch, Present }
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
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
}
impl RuntimeObservation {
    pub(crate) fn validate(&self)->Result<(),String> {
        if self.binding.is_empty() || self.binding.len()>512 || self.binding_revision==0 || self.task_revision==Some(0)
            || self.observed_unix_ms<0 || self.collector!="herdr-git-v1" || self.diagnostic.len()>8192
            || self.config_digest.as_ref().is_some_and(|d|d.len()!=64||!d.bytes().all(|b|b.is_ascii_hexdigit())) {
            return Err("invalid runtime observation".into());
        }
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
