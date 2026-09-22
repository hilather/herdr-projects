//! Immutable, credential-free effective profile evidence retained with an attempt.
//! Deserializing a record does not grant launch authority: PreparedLaunch remains sealed.
use super::VersionedReference;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityEvidence {
    Unknown,
    Unsupported { evidence: VersionedReference },
    Supported { evidence: VersionedReference },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileCapabilities {
    pub launch: CapabilityEvidence,
    pub readiness_observation: CapabilityEvidence,
    pub prompt_submission: CapabilityEvidence,
    pub stop: CapabilityEvidence,
    pub checkpoint_acknowledgment: CapabilityEvidence,
    pub structured_usage: CapabilityEvidence,
    pub resume: CapabilityEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableIdentity {
    pub path: String,
    pub digest: String,
    /// Exact version, including prerelease/build suffixes.
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenProfile {
    pub version: u32,
    pub name: String,
    pub kind: String,
    pub definition_digest: String,
    pub config: crate::migration::ConfigReference,
    /// Effective argv is reconstructed only from matching user-owned config.
    /// Do not put argv or environment values in projections, events or briefs.
    pub arguments_digest: String,
    pub environment_names: Vec<String>,
    /// Explicit credential-store/config home for the fixed clean environment.
    /// No arbitrary environment values or credentials are retained here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_home: Option<String>,
    pub permission_policy: VersionedReference,
    pub agent: ExecutableIdentity,
    pub herdr: ExecutableIdentity,
    /// Binds the adapter/version mapping that produced these effective inputs.
    pub adapter: VersionedReference,
    pub capabilities: ProfileCapabilities,
    pub workflow_certificate: Option<VersionedReference>,
}

fn hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
fn reference(value: &VersionedReference) -> bool {
    !value.id.is_empty()
        && value.id.len() <= 512
        && !value.id.chars().any(char::is_control)
        && value.revision > 0
        && hash(&value.digest)
}
fn executable(value: &ExecutableIdentity) -> bool {
    std::path::Path::new(&value.path).is_absolute()
        && value.path.len() <= 4096
        && !value.path.chars().any(char::is_control)
        && hash(&value.digest)
        && !value.version.is_empty()
        && value.version.len() <= 96
        && value
            .version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
}

impl FrozenProfile {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || !identifier(&self.name)
            || !identifier(&self.kind)
            || !hash(&self.definition_digest)
            || !hash(&self.arguments_digest)
            || !std::path::Path::new(&self.config.path).is_absolute()
            || self.config.path.len() > 4096
            || self.config.path.chars().any(char::is_control)
            || self.config.digest.as_ref().is_some_and(|d| !hash(d))
            || !reference(&self.permission_policy)
            || !reference(&self.adapter)
            || !executable(&self.agent)
            || !executable(&self.herdr)
            || self
                .workflow_certificate
                .as_ref()
                .is_some_and(|r| !reference(r))
        {
            return Err("invalid frozen profile identity".into());
        }
        if self.execution_home.as_ref().is_some_and(|home| {
            !std::path::Path::new(home).is_absolute()
                || home.len() > 4096
                || home.chars().any(char::is_control)
        }) {
            return Err("invalid execution home".into());
        }
        let mut names = std::collections::BTreeSet::new();
        if self.environment_names.len() > 128 {
            return Err("too many environment references".into());
        }
        for name in &self.environment_names {
            if name.is_empty()
                || name.len() > 128
                || !(name.as_bytes()[0].is_ascii_alphabetic() || name.starts_with('_'))
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || !names.insert(name)
            {
                return Err("invalid frozen environment reference".into());
            }
        }
        let c = &self.capabilities;
        for capability in [
            &c.launch,
            &c.readiness_observation,
            &c.prompt_submission,
            &c.stop,
            &c.checkpoint_acknowledgment,
            &c.structured_usage,
            &c.resume,
        ] {
            match capability {
                CapabilityEvidence::Unknown => {}
                CapabilityEvidence::Supported { evidence }
                | CapabilityEvidence::Unsupported { evidence }
                    if reference(evidence) => {}
                _ => return Err("invalid capability evidence reference".into()),
            }
        }
        Ok(())
    }

    pub fn validate_for_launch(&self) -> Result<(), String> {
        self.validate()?;
        let c = &self.capabilities;
        if [
            &c.launch,
            &c.readiness_observation,
            &c.prompt_submission,
            &c.stop,
        ]
        .iter()
        .any(|c| !matches!(c, CapabilityEvidence::Supported { .. }))
        {
            return Err(
                "launch requires verified launch, readiness, prompt and stop capabilities".into(),
            );
        }
        Ok(())
    }

    pub fn reference(&self) -> Result<VersionedReference, String> {
        self.validate()?;
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(self).map_err(|_| "profile encoding failed")?)
        );
        Ok(VersionedReference {
            id: format!("profile-{digest}"),
            revision: 1,
            digest,
        })
    }
}

#[cfg(test)]
pub(crate) fn fixture(config: crate::migration::ConfigReference) -> FrozenProfile {
    let evidence = VersionedReference {
        id: "test-only-evidence".into(),
        revision: 1,
        digest: "a".repeat(64),
    };
    let supported = CapabilityEvidence::Supported {
        evidence: evidence.clone(),
    };
    FrozenProfile {
        version: 1,
        name: "fixture".into(),
        kind: "claude".into(),
        definition_digest: "b".repeat(64),
        config,
        arguments_digest: "c".repeat(64),
        environment_names: vec![],
        execution_home: None,
        permission_policy: evidence.clone(),
        adapter: evidence,
        agent: ExecutableIdentity {
            path: "/fixture/claude".into(),
            digest: "d".repeat(64),
            version: "1.0.0-preview.1".into(),
        },
        herdr: ExecutableIdentity {
            path: "/fixture/herdr".into(),
            digest: "e".repeat(64),
            version: "0.9.1".into(),
        },
        capabilities: ProfileCapabilities {
            launch: supported.clone(),
            readiness_observation: supported.clone(),
            prompt_submission: supported.clone(),
            stop: supported,
            checkpoint_acknowledgment: CapabilityEvidence::Unknown,
            structured_usage: CapabilityEvidence::Unknown,
            resume: CapabilityEvidence::Unknown,
        },
        workflow_certificate: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_capabilities_remain_explicit_and_identity_covers_effective_inputs() {
        let profile = fixture(crate::migration::ConfigReference {
            path: "/fixture/config.toml".into(),
            digest: None,
        });
        assert!(profile.validate_for_launch().is_ok());
        let original = profile.reference().unwrap();
        for field in 0..5 {
            let mut changed = profile.clone();
            match field {
                0 => changed.agent.version = "1.0.0-preview.2".into(),
                1 => changed.arguments_digest = "f".repeat(64),
                2 => changed.config.digest = Some("f".repeat(64)),
                3 => changed.kind = "codex".into(),
                _ => changed.environment_names.push("TOKEN_NAME".into()),
            }
            assert_ne!(changed.reference().unwrap(), original);
        }
        let mut unknown = profile.clone();
        unknown.capabilities.stop = CapabilityEvidence::Unknown;
        assert!(unknown.validate().is_ok());
        assert!(unknown.validate_for_launch().is_err());
        let mut invalid = profile;
        invalid.environment_names = vec!["TOKEN=SECRET".into()];
        assert!(!invalid.validate().unwrap_err().contains("SECRET"));
    }
}
