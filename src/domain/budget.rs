//! Owner-signed lifetime admission limits. Provider usage is not inferred.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use super::VersionedReference;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum UnknownUsagePolicy { Refuse, AllowIncomplete }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetLimits {
    /// Lifetime count, including imported, cancelled and unterminated attempts.
    pub max_attempts: Option<u64>,
    /// Admission threshold only; never a provider billing cap.
    pub max_provider_tokens: Option<u64>,
    pub unknown_usage: UnknownUsagePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetPolicy {
    pub version: u32,
    pub project_store: String,
    pub revision: u64,
    pub authority: VersionedReference,
    pub limits: BudgetLimits,
}
impl BudgetPolicy {
    pub fn validate(&self)->Result<(),String> {
        let a=&self.authority;
        if self.version!=1 || self.revision==0 || self.revision>i64::MAX as u64
            || !std::path::Path::new(&self.project_store).is_absolute() || self.project_store.len()>4096
            || self.project_store.chars().any(char::is_control)
            || a.id.is_empty() || a.id.len()>512 || a.id.chars().any(char::is_control)
            || a.revision==0 || a.revision>i64::MAX as u64 || a.digest.len()!=64
            || !a.digest.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))
            || [self.limits.max_attempts,self.limits.max_provider_tokens].into_iter().flatten().any(|v|v>i64::MAX as u64) {
            return Err("invalid budget policy".into());
        }
        Ok(())
    }
    pub fn reference(&self)->Result<VersionedReference,String> {
        self.validate()?;
        let digest=format!("{:x}",Sha256::digest(serde_json::to_vec(self).map_err(|_|"budget encoding failed")?));
        Ok(VersionedReference{id:"project-budget".into(),revision:self.revision,digest})
    }
}
/// Only trusted signature verification constructs an installable policy.
pub struct PreparedBudget { pub(crate) policy: BudgetPolicy }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum UsageAvailability { Unknown }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetReport {
    pub policy: Option<BudgetPolicy>,
    pub admitted_attempts: u64,
    pub provider_tokens: UsageAvailability,
    pub incomplete: bool,
    pub blockers: Vec<String>,
}
