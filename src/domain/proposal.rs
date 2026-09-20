//! Worker memory proposals. Untrusted candidates; never authoritative memory.
use serde::{Deserialize, Serialize};
use super::Applicability;

pub const PROPOSAL_VALIDATOR_ID: &str = "proposal-validation";
pub const PROPOSAL_VALIDATOR_VERSION: u32 = 1;
pub const PROPOSAL_LIMIT: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalProducer {
    pub task_id: String,
    pub attempt_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedRevision {
    pub record_id: String,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalRepository {
    pub id: String,
    pub commit: String,
    pub tree: String,
    pub dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalEvidence {
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub object: Option<String>,
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub validation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalChange {
    pub record_key: String,
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub expected: Option<ObservedRevision>,
    pub kind: String,
    pub scope: Applicability,
    pub claim: String,
    pub body_object: String,
    #[serde(default)]
    pub evidence: Vec<ProposalEvidence>,
    #[serde(default)]
    pub based_on: Vec<ObservedRevision>,
    pub impact: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalDocument {
    pub schema_version: u32,
    pub proposal_id: String,
    pub producer: ProposalProducer,
    pub input_snapshot_id: String,
    #[serde(default)]
    pub observed_revisions: Vec<ObservedRevision>,
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub repository: Option<ProposalRepository>,
    pub changes: Vec<ProposalChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalReceipt {
    pub proposal_id: String,
    pub payload_digest: String,
    pub review_state: String,
    pub validation: String,
    pub reason: String,
    pub reused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredProposal {
    pub id: String,
    pub payload_digest: String,
    pub task_id: String,
    pub attempt_id: String,
    pub snapshot_id: Option<String>,
    pub review_state: String,
    pub created_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewDocument {
    pub schema_version: u32,
    pub proposal_id: String,
    pub decision: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDecision {
    pub id: String,
    pub proposal_id: String,
    pub payload_digest: String,
    pub decision: String,
    pub classification: String,
    pub reviewed_heads: String,
    pub reason: String,
    pub created_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotionReceipt {
    pub proposal_id: String,
    pub decision_id: String,
    pub sequence: u64,
    pub change_ids: Vec<String>,
    pub reused: bool,
}
