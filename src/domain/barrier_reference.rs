//! An exact historical release reference; current applicability is revalidated.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BarrierReleaseReference {
    pub schema_version: u32,
    pub barrier_id: String,
    pub release_sequence: u64,
    pub authorization_digest: String,
}

impl BarrierReleaseReference {
    pub fn validate(&self) -> Result<(), String> {
        let hash = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if self.schema_version != 1 || self.release_sequence == 0 || self.release_sequence > i64::MAX as u64
            || !hash(&self.barrier_id) || !hash(&self.authorization_digest) {
            return Err("invalid barrier release reference".into());
        }
        Ok(())
    }
}
