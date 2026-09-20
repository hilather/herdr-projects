//! Owner-signed sequential memory policy. Non-cutover ops apply to memory rows.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use super::VersionedReference;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum MemoryPolicyOp { HardRule, Cutover, ImportAck, RevokeHead }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPolicy {
    pub version: u32,
    pub project_store: String,
    pub revision: u64,
    pub authority: VersionedReference,
    pub expected_head: u64,
    pub op: MemoryPolicyOp,
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub record_key: Option<String>,
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub memory_plan_digest: Option<String>,
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub expected_memory_owner: Option<String>,
}

impl MemoryPolicy {
    pub fn validate(&self) -> Result<(), String> {
        let a = &self.authority;
        if self.version != 1 || self.revision == 0 || self.revision > i64::MAX as u64
            || !std::path::Path::new(&self.project_store).is_absolute() || self.project_store.len() > 4096
            || self.project_store.chars().any(char::is_control)
            || a.id.is_empty() || a.id.len() > 512 || a.id.chars().any(char::is_control)
            || a.revision == 0 || a.revision > i64::MAX as u64 || a.digest.len() != 64
            || !a.digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.expected_head > i64::MAX as u64 {
            return Err("invalid memory policy".into());
        }
        match self.op {
            MemoryPolicyOp::Cutover => {
                let digest = self.memory_plan_digest.as_deref().ok_or("cutover requires memory_plan_digest")?;
                if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    || self.expected_memory_owner.as_deref() != Some("legacy-markdown")
                    || self.record_key.is_some() {
                    return Err("invalid memory cutover payload".into());
                }
            }
            MemoryPolicyOp::HardRule | MemoryPolicyOp::ImportAck | MemoryPolicyOp::RevokeHead => {
                let key = self.record_key.as_deref().ok_or("memory policy requires record_key")?;
                if key.is_empty() || key.len() > 512 || key.chars().any(char::is_control)
                    || std::path::Path::new(key).is_absolute() || key.split('/').any(|p| p.is_empty() || p == "." || p == "..")
                    || self.memory_plan_digest.is_some() || self.expected_memory_owner.is_some() {
                    return Err("invalid memory record_key".into());
                }
            }
        }
        Ok(())
    }
    pub fn reference(&self) -> Result<VersionedReference, String> {
        self.validate()?;
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(self).map_err(|_| "memory policy encoding failed")?));
        Ok(VersionedReference { id: "project-memory-policy".into(), revision: self.revision, digest })
    }
}

/// Only trusted signature verification constructs an installable memory policy.
pub struct PreparedMemoryPolicy { pub(crate) policy: MemoryPolicy }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorityDenial {
    pub id: String,
    pub unix_ms: i64,
    pub class: String,
    pub command: String,
    pub actor_channel: String,
    pub reason_code: String,
    pub policy_digest: String,
    pub expected_head: Option<u64>,
    pub actual_head: Option<u64>,
}
