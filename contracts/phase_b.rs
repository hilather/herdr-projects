//! Compiled interface contract for W03–W07, not wired to Phase A persistence.
#![allow(dead_code)]
use serde::{Deserialize, Serialize};
macro_rules! ids {
    ($($name:ident),+) => { $(#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub String);)+ };
}
ids!(TaskId, AttemptId, OperationId, SnapshotId, ProposalId, ResultId, ApprovalId);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision(pub u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSeq(pub u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState { Draft, Queued, Ready, Running, AwaitingReview, Blocked, Succeeded, Failed, Cancelled }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState { Reserved, Launching, Running, AwaitingInput, Completed, Failed, Cancelled, Lost }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExternalOutcome {
    Confirmed { observed_identity: String },
    Retryable { no_effect_evidence: String },
    Ambiguous { observation_required: String },
    PermanentFailure { diagnostic: String },
}
impl ExternalOutcome {
    pub fn permits_blind_retry(&self) -> bool { matches!(self, Self::Retryable { .. }) }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task { pub id: TaskId, pub revision: Revision, pub state: TaskState, pub attempt: Option<AttemptId> }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub id: AttemptId, pub task: TaskId, pub revision: Revision, pub state: AttemptState,
    pub snapshot: SnapshotId, pub reservation: String, pub termination_observed: bool,
}
impl Attempt {
    pub fn retains_capacity(&self) -> bool { !self.termination_observed }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    pub id: OperationId, pub payload_version: u32, pub kind: String, pub target: String,
    pub payload_hash: String, pub expected_revision: Revision, pub due_unix_ms: i64,
    pub fencing_epoch: u64, pub attempts: u32, pub last_outcome: Option<ExternalOutcome>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub id: ApprovalId, pub actor: String, pub channel: String, pub operation_class: String,
    pub target: String, pub payload_hash: String, pub revision: Revision,
    pub expires_unix_ms: i64, pub remaining_uses: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBinding {
    pub repository: String, pub commit: String, pub tree: String, pub integration_base: String,
    pub snapshot: SnapshotId, pub criteria_hash: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Disposition { FixtureOnly, UserAccepted { actor: String }, Verified { verifier: String } }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultManifest {
    pub id: ResultId, pub attempt: AttemptId, pub binding: EvidenceBinding,
    pub artifact_hashes: Vec<(String, String)>, pub commands: Vec<CommandEvidence>, pub disposition: Disposition,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEvidence { pub argv: Vec<String>, pub tool_version: String, pub exit_code: Option<i32>, pub output_hash: String }
#[derive(Debug, PartialEq, Eq)]
pub enum Gate { Stale, FixtureOnly, UserDisposition, Verified }
pub fn completion_gate(result: &ResultManifest, current: &EvidenceBinding) -> Gate {
    if &result.binding != current { return Gate::Stale; }
    match result.disposition { Disposition::FixtureOnly => Gate::FixtureOnly, Disposition::UserAccepted { .. } => Gate::UserDisposition, Disposition::Verified { .. } => Gate::Verified }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event { pub sequence: EventSeq, pub kind: String, pub entity: String, pub revision: Revision, pub payload_version: u32, pub payload: serde_json::Value }
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSnapshot { pub tasks: Vec<Task>, pub attempts: Vec<Attempt>, pub operations: Vec<Operation>, pub events: Vec<Event> }
/// Every mutation is evaluated in one short transaction. CAS failure rolls back
/// domain writes, approval consumption, events and outbox intents together.
/// None means insert-only. The store allocates event sequence numbers at commit.
pub enum Mutation {
    Task { expected: Option<Revision>, next: Task },
    Attempt { expected: Option<Revision>, next: Attempt },
    Enqueue(Operation),
    ConsumeApproval { id: ApprovalId, expected: Revision, operation: OperationId },
    Event { kind: String, entity: String, revision: Revision, payload_version: u32, payload: serde_json::Value },
}
pub struct Commit { pub expected_head: EventSeq, pub mutations: Vec<Mutation> }
#[derive(Debug)]
pub enum StoreError { Conflict, Busy, Corrupt(String), UnsupportedSchema(u32), Io(String) }
/// One instance targets one project DB on a local filesystem. External commands,
/// semantic reviews and prompts never run inside commit. Historical snapshots
/// return an error if not retained; they never silently return the current head.
pub trait StateStore {
    fn read_snapshot(&self, at: Option<EventSeq>) -> Result<(EventSeq, ProjectSnapshot), StoreError>;
    fn commit(&mut self, commit: Commit) -> Result<EventSeq, StoreError>;
}
