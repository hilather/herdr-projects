//! Native launch acknowledgments are data. Only a trusted adapter may seal one
//! after checking the exact live session, terminal and executable identities.
use super::{AgentIdentity, AttemptId, OperationId, ResourceIdentity, RuntimeRoute, WorktreeSnapshotReference, AttemptOutputReference};
use serde::{Deserialize, Serialize};

/// Herdr v0.9.1 permits 1–32 lowercase letters, digits, '-' and '_', starting
/// with a letter. Full attempt IDs exceed that native limit.
pub fn worker_agent_name(attempt: &AttemptId) -> String {
    use sha2::{Digest, Sha256};
    let digest = format!("{:x}", Sha256::digest(attempt.as_str().as_bytes()));
    format!("hp-{}", &digest[..29])
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchStartedReceipt {
    pub version: u32,
    pub attempt: AttemptId,
    pub operation: OperationId,
    pub route: RuntimeRoute,
    pub terminal: String,
    pub session: ResourceIdentity,
    pub agent: AgentIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<crate::worker_supervision::SupervisorIdentity>,
    pub observed_unix_ms: i64,
}

/// Not deserializable: parsing a reply does not establish its source or authority.
/// ```compile_fail
/// use herdr_projects::domain::PreparedLaunchStarted;
/// let _: PreparedLaunchStarted = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug)]
pub struct PreparedLaunchStarted {
    pub(crate) receipt: LaunchStartedReceipt,
}

/// Exact target selected durably before agent submission. Its existence does
/// not say that any agent was started in the terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchTarget {
    pub version: u32,
    pub attempt: AttemptId,
    pub operation: OperationId,
    pub route: RuntimeRoute,
    pub terminal: String,
    pub session: ResourceIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<crate::worker_supervision::SupervisorIdentity>,
    pub observed_unix_ms: i64,
}

#[derive(Debug)]
pub struct PreparedLaunchTarget {
    pub(crate) target: LaunchTarget,
}

/// The complete outbound brief is rendered from retained knowledge. Only its
/// digest and character count belong in the operation ledger, never its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerBriefIntent {
    pub version: u32,
    pub attempt: AttemptId,
    pub launch: OperationId,
    pub binding: String,
    pub binding_revision: u64,
    pub ownership_revision: u64,
    pub knowledge: Option<super::VersionedReference>,
    pub prompt_digest: String,
    pub prompt_chars: u64,
}

/// Created by retained-brief rendering, not JSON deserialization.
pub struct PreparedWorkerBrief {
    pub(crate) intent: WorkerBriefIntent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerBriefReceipt {
    pub version: u32,
    pub operation: OperationId,
    pub intent: WorkerBriefIntent,
    pub session: ResourceIdentity,
    pub terminal: String,
    pub agent: AgentIdentity,
    pub observed_unix_ms: i64,
}

/// Only the native adapter may establish delivery. Text observed on screen or
/// a caller-supplied outcome string is insufficient.
pub struct PreparedWorkerBriefReceipt {
    pub(crate) receipt: WorkerBriefReceipt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerTerminationCause {
    Cancellation,
    ProcessExit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerTerminationReceipt {
    pub version: u32,
    pub attempt: AttemptId,
    pub launch: OperationId,
    pub binding: String,
    pub binding_revision: u64,
    pub ownership_revision: u64,
    pub supervisor: crate::worker_supervision::SupervisorIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_reboot: Option<crate::worker_supervision::HostRebootEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repository_snapshots: Vec<WorktreeSnapshotReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_snapshot: Option<AttemptOutputReference>,
    /// Resources remain recorded and owned; termination never deletes them or
    /// certifies artifact finalization. Review/copy obligations survive release.
    pub retained_resources: super::RuntimeIdentity,
    pub cause: WorkerTerminationCause,
    pub observed_unix_ms: i64,
}
pub struct PreparedWorkerTermination {
    pub(crate) receipt: WorkerTerminationReceipt,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_attempt_ids_map_to_bounded_native_agent_names() {
        let first = AttemptId::new(format!("attempt-{}", "a".repeat(64))).unwrap();
        let second = AttemptId::new(format!("attempt-{}", "b".repeat(64))).unwrap();
        let name = worker_agent_name(&first);
        assert_eq!(name.len(), 32);
        assert!(name.as_bytes()[0].is_ascii_lowercase());
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_".contains(&b))
        );
        assert_eq!(name, worker_agent_name(&first));
        assert_ne!(name, worker_agent_name(&second));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchStoppedReceipt {
    pub version: u32,
    pub target: LaunchTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_reboot: Option<crate::worker_supervision::HostRebootEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repository_snapshots: Vec<WorktreeSnapshotReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_snapshot: Option<AttemptOutputReference>,
    pub observed_unix_ms: i64,
}
pub struct PreparedLaunchStopped {
    pub(crate) receipt: LaunchStoppedReceipt,
}

/// Durable pre-effect identity for one gated resource creation request. Argument
/// bytes stay out of the ledger; recovery compares the observed vector's digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchCreationIntent {
    /// 1: existing workspace tab or historical shell/bootstrap workspace.
    /// 2: literal supervised first pane via workspace.create_command.
    pub version: u32,
    pub operation: OperationId,
    pub attempt: AttemptId,
    pub route: RuntimeRoute,
    pub session: ResourceIdentity,
    pub command_digest: String,
    /// Random per-creation marker passed only to the workspace bootstrap process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_token: Option<String>,
    /// Explicit warning: character estimates are not provider usage telemetry.
    /// Historical intents lack this field and remain readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_warning: Option<LaunchUsageWarning>,
}
pub struct PreparedLaunchCreation {
    pub(crate) intent: LaunchCreationIntent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchUsageWarning {
    ProviderUsageUnavailable,
}

/// One-use boundary committed before gate input. It records permission to make
/// one submission attempt, not proof of submission, process start or readiness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchReleaseIntent {
    pub version: u32,
    pub target: LaunchTarget,
    pub observed_unix_ms: i64,
}
pub struct PreparedLaunchRelease {
    pub(crate) intent: LaunchReleaseIntent,
}

/// Sealed permission to attempt deterministic native naming once. The name is
/// derived from the target attempt and cannot be supplied by a caller.
pub struct PreparedLaunchName {
    pub(crate) intent: LaunchReleaseIntent,
}

/// Observed bootstrap terminal of a newly created workspace. This is a resource
/// receipt, not a worker start or proof that its bootstrap shell has stopped.
pub struct PreparedLaunchWorkspace {
    pub(crate) target: LaunchTarget,
}
/// One-use boundary before laying out a gated worker in its new workspace.
pub struct PreparedLaunchLayout {
    pub(crate) workspace: LaunchTarget,
}
