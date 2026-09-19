//! Persisted Phase B records; scheduling and execution policy belong to later waves.
use serde::{Deserialize, Serialize};

macro_rules! identifier {
    ($($name:ident),+) => { $(
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                if value.is_empty() || value.len() > 128 || !value.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)) {
                    return Err("identifier must contain 1–128 ASCII letters, digits, -, _, . or :".into());
                }
                Ok(Self(value))
            }
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl TryFrom<String> for $name { type Error = String; fn try_from(s: String) -> Result<Self, String> { Self::new(s) } }
        impl From<$name> for String { fn from(id: $name) -> String { id.0 } }
    )+ };
}
identifier!(TaskId, AttemptId, OperationId);

macro_rules! states {
    ($name:ident { $($variant:ident => $value:literal),+ }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
        impl $name { pub(crate) fn as_str(self) -> &'static str { match self { $(Self::$variant => $value),+ } } }
    };
}
states!(TaskState { Draft => "draft", Queued => "queued", Ready => "ready", Running => "running", AwaitingReview => "awaiting_review", Blocked => "blocked", Succeeded => "succeeded", Failed => "failed", Cancelled => "cancelled" });
states!(AttemptState { Reserved => "reserved", Launching => "launching", Running => "running", AwaitingInput => "awaiting_input", Completed => "completed", Failed => "failed", Cancelled => "cancelled", Lost => "lost" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub revision: u64,
    pub state: TaskState,
    pub title: String,
    pub active_attempt: Option<AttemptId>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub id: AttemptId,
    pub task: TaskId,
    pub revision: u64,
    pub state: AttemptState,
    /// Opaque identity only; W05 owns snapshot content and authority.
    pub snapshot: Option<String>,
    pub reservation: String,
    pub termination_observed: bool,
}
impl Attempt {
    pub fn retains_capacity(&self) -> bool { !self.termination_observed }
}
/// Immutable intent only. W03.3 owns claims, delivery and fencing APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    pub id: OperationId,
    pub task: TaskId,
    pub kind: String,
    pub target: String,
    pub payload_version: u32,
    pub payload: serde_json::Value,
    pub expected_revision: u64,
    pub due_unix_ms: i64,
    pub idempotency_key: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub sequence: u64,
    pub kind: String,
    pub entity: String,
    pub revision: u64,
    pub payload_version: u32,
    pub payload: serde_json::Value,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version:u32,
    pub head: u64,
    pub tasks: Vec<Task>,
    pub attempts: Vec<Attempt>,
    pub operations: Vec<Operation>,
    pub deliveries: Vec<crate::operations::Delivery>,
    pub inbox: Vec<InboxItem>,
    pub events: Vec<Event>,
}
pub enum Mutation {
    Task { expected: Option<u64>, next: Task },
    Attempt { expected: Option<u64>, next: Attempt },
    Enqueue(Operation),
}
pub struct Commit {
    pub expected_head: u64,
    pub mutations: Vec<Mutation>,
}

mod inbox;
pub use inbox::{InboxContent,InboxItem};
