//! Coordinator checkpoint identities. Not part of Snapshot.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorSession {
    pub id: String,
    pub created_unix_ms: i64,
    pub herdr_session: String,
    pub last_checkpoint_id: Option<String>,
    pub cursor_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorCheckpoint {
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub snapshot_id: String,
    pub from_seq: u64,
    pub through_seq: u64,
    pub manifest_hash: String,
    pub full_chars: u64,
    pub delta_chars: u64,
    pub created_unix_ms: i64,
    pub acked: bool,
}

#[derive(Debug, Clone)]
pub struct CheckpointProfile {
    pub name: String,
    pub digest: String,
    pub config_digest: Option<String>,
    pub budget_chars: u64,
}

#[derive(Debug, Clone)]
pub struct CheckpointSizes {
    pub full_chars: u64,
    pub delta_chars: u64,
    pub created_unix_ms: i64,
    pub checkpoint_id: String,
}
