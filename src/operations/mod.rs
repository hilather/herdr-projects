//! Durable delivery protocol. No external effect is performed by these types.
use crate::domain::OperationId;
use serde::{Deserialize, Serialize};
#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum DeliveryState { Pending, Claimed, Ambiguous, Confirmed, PermanentFailure }
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct Delivery {
    pub operation:OperationId,
    pub revision:u64,
    pub state:DeliveryState,
    pub epoch:u64,
    pub attempts:u32,
    pub owner:Option<String>,
    pub lease_until_ms:Option<i64>,
    pub next_due_ms:i64,
    pub last_outcome:Option<Outcome>,
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(tag="kind",rename_all="snake_case")]
pub enum Outcome {
    Confirmed { observed_identity:String },
    Retryable { no_effect_evidence:String },
    Ambiguous { observation_required:String },
    PermanentFailure { diagnostic:String },
}
impl Outcome {
    pub(crate) fn evidence(&self)->&str {
        match self { Self::Confirmed{observed_identity}=>observed_identity,Self::Retryable{no_effect_evidence}=>no_effect_evidence,Self::Ambiguous{observation_required}=>observation_required,Self::PermanentFailure{diagnostic}=>diagnostic }
    }
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct Claim {
    pub operation:OperationId,
    pub revision:u64,
    pub owner:String,
    pub epoch:u64,
    pub lease_until_ms:i64,
}

pub mod dispatch;
pub mod receipts;

/// Stable identity of an accepted legacy obligation, shared by import and observation.
pub(crate) fn legacy_id(kind:&str,key:&str)->String {
    use sha2::{Digest,Sha256};
    format!("legacy-{:x}",Sha256::digest(format!("{kind}:{key}").as_bytes()))
}
pub mod notification;
