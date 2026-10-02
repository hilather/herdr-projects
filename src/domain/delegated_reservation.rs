//! A subject-signed exact action under an owner-issued finite delegation.
use super::{ApprovalGrant, ApprovalScope, LaunchInputs, PreparedDelegation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegatedReservationRequest {
    pub schema_version: u32,
    pub grant_id: String,
    pub subject: String,
    pub store_incarnation: String,
    pub idempotency_key: String,
    pub expected_head: u64,
    pub issued_unix_ms: i64,
    pub inputs: LaunchInputs,
}

/// Constructed only after subject-signature verification by trusted ingress.
/// ```compile_fail
/// use herdr_farm::domain::PreparedDelegatedReservation;
/// let _: PreparedDelegatedReservation = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct PreparedDelegatedReservation {
    pub(crate) request: DelegatedReservationRequest,
    pub(crate) raw: Vec<u8>,
    pub(crate) digest: String,
}

impl PreparedDelegatedReservation {
    pub(crate) fn parse_verified(raw: &[u8]) -> Result<Self, String> {
        if raw.len() > 65_536 { return Err("delegated request exceeds 65536 bytes".into()); }
        let request: DelegatedReservationRequest = serde_json::from_slice(raw)
            .map_err(|_| "invalid delegated reservation request".to_string())?;
        let digest_ok = |value: &str| value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if request.schema_version != 1 || !digest_ok(&request.grant_id) || !digest_ok(&request.store_incarnation)
            || super::TaskId::new(&request.idempotency_key).is_err() || request.subject.is_empty()
            || request.subject.len() > 256 || request.subject.chars().any(char::is_control)
            || request.expected_head > i64::MAX as u64 || request.issued_unix_ms < 0 {
            return Err("invalid delegated reservation envelope".into());
        }
        Ok(Self { request, raw: raw.to_vec(), digest: format!("{:x}", Sha256::digest(raw)) })
    }

    pub(crate) fn approval(&self, grant: &PreparedDelegation) -> Result<ApprovalGrant, String> {
        if self.request.grant_id != grant.digest || self.request.subject != grant.subject {
            return Err("delegated request subject or grant mismatch".into());
        }
        let approval = ApprovalGrant {
            version: 1,
            scope: ApprovalScope::for_launch(&self.request.inputs)?,
            policy: grant.authority.clone(),
            issued_unix_ms: self.request.issued_unix_ms,
            expires_unix_ms: grant.expires_unix_ms,
        };
        approval.validate()?;
        Ok(approval)
    }
}
