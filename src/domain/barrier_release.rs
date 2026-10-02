//! Exact owner authorization for one frozen barrier, separate from its token.
use super::VersionedReference;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BarrierReleaseAuthorization {
    pub schema_version: u32,
    pub action: String,
    pub project_store: String,
    pub store_incarnation: String,
    pub authority: VersionedReference,
    pub config_digest: String,
    pub control_revision: u64,
    pub control_epoch: u64,
    pub expected_head: u64,
    pub barrier_id: String,
    pub memory_manifest_version: u32,
    pub memory_manifest_digest: String,
    pub required_set_generation: u64,
    pub release_policy: String,
    pub issued_unix_ms: i64,
    pub expires_unix_ms: i64,
}

/// Signature-verified bytes, constructible only by trusted crate ingress.
/// Parsing a request is not proof of authority.
/// ```compile_fail
/// use herdr_farm::domain::PreparedBarrierRelease;
/// let _: PreparedBarrierRelease = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct PreparedBarrierRelease {
    pub(crate) document: BarrierReleaseAuthorization,
    pub(crate) raw: Vec<u8>,
    pub(crate) digest: String,
}

impl PreparedBarrierRelease {
    pub(crate) fn parse_verified(raw: &[u8]) -> Result<Self, String> {
        if raw.len() > 65_536 {
            return Err("barrier authorization exceeds 65536 bytes".into());
        }
        let document: BarrierReleaseAuthorization = serde_json::from_slice(raw)
            .map_err(|_| "invalid barrier authorization document".to_string())?;
        let hash = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        let path = std::path::Path::new(&document.project_store);
        if document.schema_version != 1
            || document.action != "release_barrier"
            || document.memory_manifest_version != 2
            || document.release_policy != "all_members_ready_v1"
            || document.authority.id != "owner-approval-policy"
            || document.authority.revision == 0
            || document.authority.revision > i64::MAX as u64
            || document.control_revision == 0
            || document.control_revision > i64::MAX as u64
            || document.control_epoch == 0
            || document.control_epoch > i64::MAX as u64
            || document.expected_head > i64::MAX as u64
            || document.required_set_generation > i64::MAX as u64
            || document.issued_unix_ms < 0
            || document.expires_unix_ms <= document.issued_unix_ms
            || !path.is_absolute()
            || document.project_store.len() > 4096
            || document.project_store.chars().any(char::is_control)
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            || [
                &document.store_incarnation,
                &document.authority.digest,
                &document.config_digest,
                &document.barrier_id,
                &document.memory_manifest_digest,
            ]
            .into_iter()
            .any(|value| !hash(value))
        {
            return Err("invalid barrier authorization envelope".into());
        }
        Ok(Self {
            document,
            raw: raw.to_vec(),
            digest: format!("{:x}", Sha256::digest(raw)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const RAW: &[u8] = include_bytes!("../../contracts/factory/barrier-release-v1.json");

    #[test]
    fn barrier_authorization_pins_exact_fixed_bytes() {
        let prepared = PreparedBarrierRelease::parse_verified(RAW).unwrap();
        assert_eq!(prepared.raw, RAW);
        assert_eq!(
            prepared.digest,
            include_str!("../../contracts/factory/barrier-release-v1.sha256").trim()
        );
        let mut changed = RAW.to_vec();
        changed.push(b' ');
        let changed = PreparedBarrierRelease::parse_verified(&changed).unwrap();
        assert_eq!(prepared.document, changed.document);
        assert_ne!(prepared.digest, changed.digest);
    }

    #[test]
    fn barrier_authorization_refuses_ambiguous_or_unsupported_fields() {
        let raw = std::str::from_utf8(RAW).unwrap();
        for needle in ["\"schema_version\": 1", "\"revision\": 1"] {
            let changed = raw.replacen(needle, &format!("{needle}, {needle}"), 1);
            assert_ne!(changed, raw);
            assert!(PreparedBarrierRelease::parse_verified(changed.as_bytes()).is_err());
        }
        for pointer in ["", "/authority"] {
            let mut value: serde_json::Value = serde_json::from_slice(RAW).unwrap();
            value
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("role".into(), "owner".into());
            assert!(
                PreparedBarrierRelease::parse_verified(&serde_json::to_vec(&value).unwrap())
                    .is_err()
            );
        }
        for (field, replacement) in [
            ("schema_version", serde_json::json!(2)),
            ("action", serde_json::json!("launch")),
            ("memory_manifest_version", serde_json::json!(1)),
            ("release_policy", serde_json::json!("skip_memory")),
            ("store_incarnation", serde_json::json!("A".repeat(64))),
            ("config_digest", serde_json::json!(null)),
            ("control_revision", serde_json::json!(0)),
            ("control_epoch", serde_json::json!(u64::MAX)),
            ("expected_head", serde_json::json!(u64::MAX)),
            ("required_set_generation", serde_json::json!(u64::MAX)),
            ("issued_unix_ms", serde_json::json!(-1)),
            ("expires_unix_ms", serde_json::json!(0)),
            ("project_store", serde_json::json!("relative/state.db")),
            ("project_store", serde_json::json!("/fixture/../state.db")),
        ] {
            let mut value: serde_json::Value = serde_json::from_slice(RAW).unwrap();
            value[field] = replacement;
            assert!(
                PreparedBarrierRelease::parse_verified(&serde_json::to_vec(&value).unwrap())
                    .is_err(),
                "{field}"
            );
        }
        let mut oversized = RAW.to_vec();
        oversized.resize(65_537, b' ');
        assert!(PreparedBarrierRelease::parse_verified(&oversized).is_err());
    }
}
