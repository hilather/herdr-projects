//! Typed advisory subscriptions. A matching trigger never grants authority.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitTrigger {
    ApprovalDecision { approval_id: String, task_revision: u64 },
    AttemptCapacityReleased { attempt_id: super::AttemptId, after_revision: u64 },
    OwnedRuntimeRecovered { binding_id: String, binding_revision: u64, ownership_revision: u64 },
}

impl WaitTrigger {
    pub fn validate(&self, condition: &str) -> Result<(), String> {
        match self {
            Self::OwnedRuntimeRecovered { binding_id, binding_revision, ownership_revision } => {
                if condition != "adapter_recovery" || binding_id.is_empty() || binding_id.len()>512
                    || binding_id.chars().any(char::is_control) || *binding_revision==0 || *ownership_revision==0
                    || *binding_revision>i64::MAX as u64 || *ownership_revision>i64::MAX as u64 {
                    return Err("runtime recovery wait requires exact binding and ownership revisions".into());
                }
            }
            Self::AttemptCapacityReleased { after_revision, .. } => {
                if condition != "resource_availability" || *after_revision == 0 || *after_revision >= i64::MAX as u64 {
                    return Err("capacity wait requires an attempt and a prior revision".into());
                }
            }
            Self::ApprovalDecision { approval_id, task_revision } => {
                let digest = approval_id.strip_prefix("approval-").unwrap_or("");
                if condition != "user_decision" || digest.len() != 64
                    || !digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    || *task_revision == 0 || *task_revision > i64::MAX as u64 {
                    return Err("approval decision wait requires an exact approval ID and task revision".into());
                }
            }
        }
        Ok(())
    }
}
