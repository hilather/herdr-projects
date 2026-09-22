//! Shared profile encoding. These user-owned definitions do not grant launch authority.
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileDefinition {
    pub kind: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub environment: Vec<String>,
    pub permission_policy: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub budget: Option<Budget>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub max_wall_seconds: Option<u64>,
    pub soft_input_tokens: Option<u64>,
    pub soft_output_tokens: Option<u64>,
    pub unknown_usage: UnknownUsage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownUsage {
    AllowWithWarning,
    Block,
}

impl ProfileDefinition {
    /// Validate user intent without echoing arguments, environment values or
    /// malformed source. This establishes no adapter capability or authority.
    pub fn validate(&self) -> anyhow::Result<()> {
        use anyhow::ensure;
        fn identifier(s: &str) -> bool {
            !s.is_empty()
                && s.len() <= 64
                && s.as_bytes()[0].is_ascii_alphabetic()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        }
        ensure!(identifier(&self.kind), "invalid agent kind identifier");
        ensure!(
            identifier(&self.permission_policy),
            "invalid profile permission policy reference"
        );
        ensure!(
            self.extra_args.len() <= 128
                && self.extra_args.iter().map(String::len).sum::<usize>() <= 65536
                && !self.extra_args.iter().any(|s| s.contains('\0')),
            "profile arguments exceed bounds or contain NUL"
        );
        for intent in [&self.model, &self.reasoning_effort].into_iter().flatten() {
            ensure!(
                !intent.is_empty() && intent.len() <= 256 && !intent.chars().any(char::is_control),
                "invalid profile model or reasoning intent"
            );
        }
        ensure!(
            self.environment.len() <= 128,
            "too many profile environment references"
        );
        let mut names = std::collections::BTreeSet::new();
        for name in &self.environment {
            ensure!(
                !name.is_empty()
                    && name.len() <= 128
                    && (name.as_bytes()[0].is_ascii_alphabetic() || name.starts_with('_'))
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "profile environment must contain variable names, never values"
            );
            ensure!(
                names.insert(name),
                "duplicate profile environment reference"
            );
        }
        if let Some(budget) = &self.budget {
            ensure!(
                [
                    budget.max_wall_seconds,
                    budget.soft_input_tokens,
                    budget.soft_output_tokens
                ]
                .iter()
                .flatten()
                .all(|n| *n > 0 && *n <= i64::MAX as u64),
                "profile budget limits must be positive bounded integers"
            );
        }
        Ok(())
    }

    /// Character estimate used before submission. It is not provider usage
    /// telemetry and must not be presented as an exact token count.
    pub fn input_budget_chars(&self) -> anyhow::Result<u64> {
        self.budget
            .as_ref()
            .and_then(|b| b.soft_input_tokens)
            .map(|n| {
                n.checked_mul(4).ok_or_else(|| {
                    anyhow::anyhow!("profile token budget exceeds character conversion bounds")
                })
            })
            .unwrap_or(Ok(32_000))
    }

    pub fn validate_gated_preparation(&self, prompt_chars: u64) -> anyhow::Result<u64> {
        use anyhow::{Context, ensure};
        self.validate()?;
        ensure!(
            self.model.is_none() && self.reasoning_effort.is_none() && self.environment.is_empty(),
            "worker profile needs an unsupported model or environment mapping"
        );
        let budget = self
            .budget
            .as_ref()
            .context("worker profile requires a wall deadline")?;
        let wall = budget
            .max_wall_seconds
            .context("worker profile requires a wall deadline")?;
        ensure!(
            (1..=604800).contains(&wall),
            "worker wall deadline exceeds supported bounds"
        );
        ensure!(
            budget.unknown_usage == UnknownUsage::AllowWithWarning,
            "worker profile blocks unavailable provider usage; verified usage telemetry is required"
        );
        ensure!(
            prompt_chars <= self.input_budget_chars()?,
            "complete retained brief exceeds the profile input budget"
        );
        Ok(wall)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn definition() -> ProfileDefinition {
        toml::from_str("kind='claude'\npermission_policy='interactive'\n[budget]\nmax_wall_seconds=10\nsoft_input_tokens=100\nunknown_usage='allow_with_warning'").unwrap()
    }
    #[test]
    fn preparation_checks_whole_brief_and_supported_budget_policy() {
        let mut profile = definition();
        assert_eq!(profile.validate_gated_preparation(400).unwrap(), 10);
        assert!(profile.validate_gated_preparation(401).is_err());
        profile.budget.as_mut().unwrap().unknown_usage = UnknownUsage::Block;
        assert!(profile.validate_gated_preparation(1).is_err());
        profile.budget.as_mut().unwrap().unknown_usage = UnknownUsage::AllowWithWarning;
        for wall in [0, 604801, u64::MAX] {
            profile.budget.as_mut().unwrap().max_wall_seconds = Some(wall);
            assert!(profile.validate_gated_preparation(1).is_err());
        }
        profile.budget.as_mut().unwrap().max_wall_seconds = Some(10);
        profile.budget.as_mut().unwrap().soft_input_tokens = Some(i64::MAX as u64);
        assert!(profile.validate_gated_preparation(1).is_err());
        profile.budget = None;
        assert!(profile.validate_gated_preparation(1).is_err());
    }
    #[test]
    fn shared_validation_and_mapping_errors_do_not_expose_values() {
        for field in 0..7 {
            let mut profile = definition();
            match field {
                0 => profile.extra_args = vec!["PRIVATE\0VALUE".into()],
                1 => profile.environment = vec!["TOKEN=PRIVATE".into()],
                2 => profile.permission_policy = "PRIVATE!".into(),
                3 => profile.model = Some("PRIVATE".into()),
                4 => profile.reasoning_effort = Some("PRIVATE".into()),
                5 => profile.environment = vec!["PRIVATE".into()],
                _ => profile.kind = "PRIVATE!".into(),
            }
            let error = profile.validate_gated_preparation(1).unwrap_err();
            assert!(!format!("{error:#}").contains("PRIVATE"));
        }
    }
}

/// Parse only observed version formats; this does not establish capability support.
pub fn observed_version(kind: &str, text: &str) -> Option<String> {
    let text = text.trim();
    let token = match kind {
        "herdr" => text.strip_prefix("herdr ")?,
        "codex" => text
            .strip_prefix("codex-cli ")
            .or_else(|| text.strip_prefix("codex "))?,
        "claude" => text.strip_suffix(" (Claude Code)")?,
        _ => return None,
    };
    if token.len() > 96
        || token.is_empty()
        || !token
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-+".contains(&c))
    {
        return None;
    }
    let core = token.split(['-', '+']).next()?;
    let parts = core.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|p| {
            p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || p.parse::<u64>().is_err()
        })
    {
        return None;
    }
    Some(token.into())
}

/// Re-read only the owner configuration bound by a frozen profile. This neither
/// grants capabilities nor executes the configured arguments.
#[cfg(feature = "state-store")]
pub(crate) fn frozen_definition(profile: &crate::domain::FrozenProfile) -> anyhow::Result<ProfileDefinition> {
    use anyhow::{Context, ensure};
    use sha2::{Digest, Sha256};
    use std::path::Path;
    let bytes = crate::migration::read_plan_file(Path::new(&profile.config.path))?;
    ensure!(
        bytes.len() <= 1_048_576
            && profile.config.digest.as_deref()
                == Some(format!("{:x}", Sha256::digest(&bytes)).as_str()),
        "worker profile configuration changed"
    );
    let config: toml::Value = toml::from_str(
        std::str::from_utf8(&bytes).map_err(|_| anyhow::anyhow!("invalid worker configuration"))?,
    )
    .map_err(|_| anyhow::anyhow!("invalid worker configuration (contents withheld)"))?;
    let value = config
        .get("profiles")
        .and_then(|v| v.get(&profile.name))
        .context("named worker profile missing")?;
    let definition: ProfileDefinition = value
        .clone()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid worker profile (contents withheld)"))?;
    ensure!(
        format!("{:x}", Sha256::digest(serde_json::to_vec(&definition)?))
            == profile.definition_digest
            && definition.kind == profile.kind,
        "worker profile definition changed"
    );
    definition.validate()?;
    ensure!(
        profile.environment_names.is_empty(),
        "retained environment mapping is unsupported"
    );
    ensure!(
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&definition.extra_args)?)
        ) == profile.arguments_digest,
        "effective worker arguments changed"
    );
    Ok(definition)
}
