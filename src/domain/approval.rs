//! Approval data binds an intended action, never authenticates its issuer.
//! Only the forthcoming trusted control ingress may grant/consume these records.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use super::{LaunchInputs, TaskId, VersionedReference};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalClass { RuntimeLaunch }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalScope {
    pub version: u32,
    pub class: ApprovalClass,
    pub project_store: String,
    pub task: TaskId,
    /// Revision the operation expects after the reservation transaction.
    pub task_revision: u64,
    pub target: String,
    pub action_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalGrant {
    pub version: u32,
    pub scope: ApprovalScope,
    pub policy: VersionedReference,
    pub issued_unix_ms: i64,
    pub expires_unix_ms: i64,
}

/// Trusted ingress capability, not deserializable and not constructible by callers.
/// Production issuance remains unavailable until control-route authentication exists.
#[derive(Debug, Clone)]
pub struct PreparedApproval { pub(crate) grant: ApprovalGrant }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalUse {
    pub operation: super::OperationId,
    pub claim_revision: u64,
    pub claim_epoch: u64,
    pub consumed_unix_ms: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRevocation { pub revoked_unix_ms: i64, pub reason: String }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub grant: ApprovalGrant,
    pub reference: VersionedReference,
    pub revoked: Option<ApprovalRevocation>,
    pub consumed: Option<ApprovalUse>,
}

fn hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn ordered(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: std::collections::BTreeMap<_, _> = map.into_iter().collect();
            serde_json::Value::Object(sorted.into_iter().map(|(key, value)| (key, ordered(value))).collect())
        },
        serde_json::Value::Array(values) => serde_json::Value::Array(values.into_iter().map(ordered).collect()),
        scalar => scalar,
    }
}

impl ApprovalScope {
    /// Bind all effective launch inputs except the approval reference itself.
    /// Including that reference would create a circular approval/action hash.
    /// The final attempt and operation IDs still include the approval reference.
    pub fn for_launch(inputs: &LaunchInputs) -> Result<Self, String> {
        if inputs.version != 2 || inputs.effective_profile.is_none()
            || !std::path::Path::new(&inputs.project_store).is_absolute()
            || !text(&inputs.project_store, 4096) || !text(&inputs.binding, 512)
            || inputs.task_revision == 0 {
            return Err("approval scope requires version-2 launch inputs".into());
        }
        let task_revision = inputs.task_revision.checked_add(1)
            .filter(|r| *r <= i64::MAX as u64).ok_or("approval task revision exhausted")?;
        let mut action = serde_json::to_value(inputs).map_err(|_| "approval action encoding failed")?;
        action.as_object_mut().ok_or("invalid approval action")?.remove("approval");
        // Sort explicitly so serde_json feature unification cannot change the
        // action hash. Array order and exact string values remain significant.
        let bytes = serde_json::to_vec(&ordered(serde_json::json!({"version":1,"class":"runtime_launch","inputs":action})))
            .map_err(|_| "approval action encoding failed")?;
        if bytes.len() > 1024 * 1024 { return Err("approval action exceeds one MiB".into()); }
        let scope = Self {
            version: 1, class: ApprovalClass::RuntimeLaunch,
            project_store: inputs.project_store.clone(), task: inputs.task.clone(),
            task_revision, target: inputs.binding.clone(),
            action_digest: format!("{:x}", Sha256::digest(bytes)),
        };
        scope.validate()?;
        Ok(scope)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 || !text(&self.project_store, 4096)
            || !std::path::Path::new(&self.project_store).is_absolute()
            || self.task_revision == 0 || self.task_revision > i64::MAX as u64
            || !text(&self.target, 512) || !hash(&self.action_digest) {
            return Err("invalid approval scope".into());
        }
        Ok(())
    }
}

impl ApprovalGrant {
    pub fn validate(&self) -> Result<(), String> {
        self.scope.validate()?;
        if self.version != 1 || !text(&self.policy.id, 512) || self.policy.revision == 0
            || self.policy.revision > i64::MAX as u64 || !hash(&self.policy.digest)
            || self.issued_unix_ms < 0 || self.expires_unix_ms <= self.issued_unix_ms {
            return Err("invalid approval grant".into());
        }
        Ok(())
    }

    /// Content identity only. The policy reference and record provenance must be
    /// checked by trusted ingress; a matching hash is not a signature.
    pub fn reference(&self) -> Result<VersionedReference, String> {
        self.validate()?;
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(self).map_err(|_| "approval encoding failed")?));
        Ok(VersionedReference { id: format!("approval-{digest}"), revision: 1, digest })
    }

    /// This predicate is necessary but insufficient for authorization. A claim
    /// transaction must additionally check current policy/revocation and consume
    /// the grant once. The project path comes from the store, not worker input.
    pub fn matches_launch(&self, inputs: &LaunchInputs, actual_project_store: &str, now: i64) -> Result<(), String> {
        self.validate()?;
        if now < self.issued_unix_ms || now >= self.expires_unix_ms
            || actual_project_store != self.scope.project_store
            || self.scope != ApprovalScope::for_launch(inputs)?
            || self.reference()? != inputs.approval {
            return Err("approval is stale or bound to a different action".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn inputs() -> LaunchInputs {
        let mut inputs: LaunchInputs = serde_json::from_str(include_str!("../../tests/fixtures/launch-inputs-v1.json")).unwrap();
        inputs.version = 2;
        let profile = crate::domain::profile::fixture(inputs.config.clone());
        inputs.profile = profile.reference().unwrap();
        inputs.effective_profile = Some(profile);
        inputs
    }
    fn grant(inputs: &mut LaunchInputs) -> ApprovalGrant {
        let grant = ApprovalGrant { version: 1, scope: ApprovalScope::for_launch(inputs).unwrap(),
            policy: VersionedReference { id: "fixture-policy".into(), revision: 1, digest: "d".repeat(64) },
            issued_unix_ms: 100, expires_unix_ms: 200 };
        inputs.approval = grant.reference().unwrap();
        grant
    }
    #[test]
    fn approval_identity_is_non_circular_but_final_reference_is_required() {
        let mut inputs = inputs();
        let original = ApprovalScope::for_launch(&inputs).unwrap();
        assert_eq!(original.action_digest, "51e0e442c8e2930fc699289ba82aa7832f3610e75b892d02e8ad42d42c64888f");
        let grant = grant(&mut inputs);
        assert_eq!(grant.reference().unwrap().digest, "003b41ced67b6aaf84d11fcc47f18e9f62132b081d9c3037aeb90acaee2d9967");
        assert_eq!(original, ApprovalScope::for_launch(&inputs).unwrap());
        grant.matches_launch(&inputs, &inputs.project_store, 100).unwrap();
        inputs.approval.digest = "e".repeat(64);
        assert_eq!(original, ApprovalScope::for_launch(&inputs).unwrap());
        assert!(grant.matches_launch(&inputs, &inputs.project_store, 100).is_err());
    }
    #[test]
    fn changed_actions_projects_revisions_and_expiry_cannot_reuse_a_grant() {
        let mut inputs = inputs();let grant = grant(&mut inputs);
        for time in [99,200,201] { assert!(grant.matches_launch(&inputs, &inputs.project_store, time).is_err()); }
        assert!(grant.matches_launch(&inputs, "/another/state.db", 101).is_err());
        for field in 0..10 {
            let mut changed = inputs.clone();
            match field {
                0 => changed.task_revision += 1,
                1 => changed.binding_revision += 1,
                2 => changed.control_epoch += 1,
                3 => changed.scheduler_revision += 1,
                4 => changed.config.digest = Some("f".repeat(64)),
                5 => changed.binding = "different".into(),
                6 => changed.effective_profile.as_mut().unwrap().arguments_digest = "f".repeat(64),
                7 => changed.effective_profile.as_mut().unwrap().agent.version = "9.0.0".into(),
                8 => changed.profile.digest = "f".repeat(64),
                _ => changed.repositories.push(super::super::RepositoryInput { repository: "/repo".into(), commit: "a".repeat(40), tree: "b".repeat(40) }),
            }
            assert!(grant.matches_launch(&changed, &inputs.project_store, 101).is_err());
        }
    }
    #[test]
    fn actor_claims_and_unknown_operation_classes_are_not_accepted_as_grant_fields() {
        let mut inputs = inputs();let grant = grant(&mut inputs);
        let mut json = serde_json::to_value(&grant).unwrap();json["actor"] = "human".into();
        assert!(serde_json::from_value::<ApprovalGrant>(json).is_err());
        let mut json = serde_json::to_value(&grant).unwrap();json["scope"]["class"] = "memory_promote".into();
        assert!(serde_json::from_value::<ApprovalGrant>(json).is_err());
    }
}
