use serde::{Deserialize,Serialize};
use super::TaskId;
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerPolicy {pub revision:u64,pub max_active_workers:u32,pub max_attempts_per_task:u32}
#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum DependencyRequirement {VerifiedResult,IntegrationCandidate,LandedCommit}
impl DependencyRequirement {pub(crate) fn as_str(self)->&'static str {match self {Self::VerifiedResult=>"verified_result",Self::IntegrationCandidate=>"integration_candidate",Self::LandedCommit=>"landed_commit"}}}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {pub predecessor:TaskId,pub requirement:DependencyRequirement}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueRequest {pub priority:i32,pub dependencies:Vec<Dependency>}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct QueueRecord {pub task:TaskId,pub priority:i32,pub enqueued_unix_ms:i64,pub enqueue_sequence:u64,pub dependencies:Vec<Dependency>}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct SchedulerSnapshot {pub policy:SchedulerPolicy,pub queue:Vec<QueueRecord>}
#[derive(Debug,Serialize)]
pub struct QueueEntry {pub task:TaskId,pub task_revision:u64,pub effective_priority:i64,pub blockers:Vec<String>}
#[derive(Debug,Serialize)]
pub struct QueueReport {pub head:u64,pub policy:SchedulerPolicy,pub retained_attempts:usize,pub available_slots:usize,pub launch_enabled:bool,pub entries:Vec<QueueEntry>}
