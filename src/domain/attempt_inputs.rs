use serde::{Deserialize,Serialize};
use super::{AttemptId,TaskId,OperationId,DependencyRequirement};
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionedReference {pub id:String,pub revision:u64,pub digest:String}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryInput {pub repository:String,pub commit:String,pub tree:String}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyInput {pub task:TaskId,pub task_revision:u64,pub requirement:DependencyRequirement,pub evidence:VersionedReference}
/// Values are immutable evidence references, not credentials or executable argv.
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchInputs {
    pub version:u32,
    pub project_store:String,
    pub task:TaskId,
    /// Task revision before reservation; the launch binds the next revision.
    pub task_revision:u64,
    pub scheduler_revision:u64,
    pub control_epoch:u64,
    pub binding:String,
    pub binding_revision:u64,
    pub binding_digest:String,
    pub profile:VersionedReference,
    /// Version 2 inputs retain effective profile evidence, not just an opaque ID.
    /// Omit on serialization for byte-stable version 1 historical identities.
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub effective_profile:Option<super::FrozenProfile>,
    pub approval:VersionedReference,
    pub config:crate::migration::ConfigReference,
    pub repositories:Vec<RepositoryInput>,
    pub dependencies:Vec<DependencyInput>,
    pub memory:Option<VersionedReference>,
    pub budget:Option<VersionedReference>,
}
/// Only trusted in-crate preparation producers can construct this capability.
/// The launch preparation service derives these inputs from a live profile proof
/// and canonical task, knowledge, repository and approval state. Raw JSON cannot
/// supply this capability.
/// ```compile_fail
/// use herdr_projects::domain::PreparedLaunch;
/// let _: PreparedLaunch = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug,Clone)]
pub struct PreparedLaunch {pub(crate) inputs:LaunchInputs}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptInputRecord {pub attempt:AttemptId,pub operation:OperationId,pub inputs:LaunchInputs}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct CancellationRequest {pub attempt:AttemptId,pub requested_unix_ms:i64,pub reason:String}
#[derive(Debug,Serialize)]
pub struct Reservation {pub head:u64,pub record:AttemptInputRecord,pub task_revision:u64}
#[derive(Debug,Serialize)]
pub struct CancellationChange {pub head:u64,pub attempt:AttemptId,pub released:bool,pub attempt_revision:u64,pub task_revision:u64}
