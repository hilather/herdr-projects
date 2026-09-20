//! User-owned profile configuration. Inspection never grants launch authority.
use std::{collections::BTreeSet, path::Path};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    kind: String,
    #[serde(default)]
    extra_args: Vec<String>,
    #[serde(default)]
    environment: Vec<String>,
    permission_policy: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    budget: Option<Budget>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Budget {
    pub max_wall_seconds: Option<u64>,
    pub soft_input_tokens: Option<u64>,
    pub soft_output_tokens: Option<u64>,
    pub unknown_usage: UnknownUsage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownUsage { AllowWithWarning, Block }

/// Absence of adapter evidence is distinct from tested lack of support.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Capability { Unknown }

#[derive(Debug, Serialize)]
struct Capabilities {
    launch: Capability,
    readiness_observation: Capability,
    prompt_submission: Capability,
    stop: Capability,
    checkpoint_acknowledgment: Capability,
    structured_usage: Capability,
    resume: Capability,
}

#[derive(Debug, Serialize)]
pub struct Inspection {
    schema_version: u32,
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) config_digest: String,
    pub(super) profile_digest: String,
    extra_argument_count: usize,
    environment_reference_count: usize,
    model_requested: bool,
    reasoning_effort_requested: bool,
    budget_requested: bool,
    capabilities: Capabilities,
    pub(super) agent_version: Option<String>,
    pub(super) herdr_version: Option<String>,
    pub(super) launchable: bool,
    pub(super) protocol_capable: bool,
    pub(super) certified: bool,
    blockers: Vec<&'static str>,
}

#[cfg(test)]
impl Inspection {
    pub fn mock_workflow(name: &'static str, launchable: bool, protocol_capable: bool, certified: bool) -> Self {
        Self {
            schema_version: 1,
            name: name.into(),
            kind: "claude".into(),
            config_digest: "a".repeat(64),
            profile_digest: "b".repeat(64),
            extra_argument_count: 0,
            environment_reference_count: 0,
            model_requested: false,
            reasoning_effort_requested: false,
            budget_requested: false,
            capabilities: Capabilities {
                launch: Capability::Unknown, readiness_observation: Capability::Unknown,
                prompt_submission: Capability::Unknown, stop: Capability::Unknown,
                checkpoint_acknowledgment: Capability::Unknown, structured_usage: Capability::Unknown,
                resume: Capability::Unknown,
            },
            agent_version: None, herdr_version: None,
            launchable, protocol_capable, certified, blockers: vec![],
        }
    }
}

fn identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}

fn bounded_intent(value: &Option<String>) -> bool {
    value.as_ref().is_none_or(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
}

fn validate(profile: &Profile) -> Result<()> {
    super::arguments(&profile.kind, Some(&profile.kind), &profile.extra_args, "profile extra_args")?;
    ensure!(identifier(&profile.permission_policy), "invalid profile permission policy reference");
    ensure!(bounded_intent(&profile.model) && bounded_intent(&profile.reasoning_effort), "invalid profile model or reasoning intent");
    ensure!(profile.environment.len() <= 128, "too many profile environment references");
    let mut names = BTreeSet::new();
    for name in &profile.environment {
        ensure!(!name.is_empty() && name.len() <= 128
            && (name.as_bytes()[0].is_ascii_alphabetic() || name.starts_with('_'))
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
            "profile environment must contain variable names, never values");
        ensure!(names.insert(name), "duplicate profile environment reference");
    }
    if let Some(budget) = &profile.budget {
        ensure!([budget.max_wall_seconds, budget.soft_input_tokens, budget.soft_output_tokens]
            .iter().flatten().all(|n| *n > 0 && *n <= i64::MAX as u64), "profile budget limits must be positive bounded integers");
    }
    Ok(())
}

pub fn inspect(path: &Path, name: &str) -> Result<Inspection> {
    Ok(load(path, name)?.0)
}

pub(super) fn load(path: &Path, name: &str) -> Result<(Inspection, Option<Budget>)> {
    ensure!(identifier(name), "invalid profile name");
    let text = crate::paths::read_root_config(path)?;
    let text = text.as_deref().ok_or_else(|| anyhow::anyhow!("no profile configuration exists"))?;
    load_text(text, name)
}

/// Unique named profile whose `kind` field equals `kind`. Never looks up `profiles.<kind>`.
pub fn unique_name_for_kind(path: &Path, kind: &str) -> Result<String> {
    ensure!(identifier(kind), "invalid agent kind identifier");
    let text = crate::paths::read_root_config(path)?;
    let text = text.as_deref().ok_or_else(|| anyhow::anyhow!("no profile configuration exists"))?;
    ensure!(text.len() <= 1_048_576, "profile configuration exceeds one MiB");
    let config: toml::Value = toml::from_str(text).map_err(|_| anyhow::anyhow!("invalid profile configuration TOML (source redacted)"))?;
    let profiles = config.get("profiles").and_then(toml::Value::as_table)
        .ok_or_else(|| anyhow::anyhow!("configuration has no profiles table"))?;
    ensure!(profiles.len() <= 128, "too many profiles");
    let mut matches = Vec::new();
    for (name, value) in profiles {
        let profile: Profile = value.clone().try_into()
            .map_err(|_| anyhow::anyhow!("invalid profile fields (source redacted); check the documented schema"))?;
        validate(&profile)?;
        if profile.kind == kind { matches.push(name.clone()); }
    }
    match matches.as_slice() {
        [name] => Ok(name.clone()),
        [] => bail!("no named profile has kind `{kind}`"),
        names => bail!("multiple named profiles have kind `{kind}`: {}", names.join(", ")),
    }
}

#[cfg(test)]
fn inspect_text(text: &str, name: &str) -> Result<Inspection> { Ok(load_text(text, name)?.0) }

fn load_text(text: &str, name: &str) -> Result<(Inspection, Option<Budget>)> {
    ensure!(text.len() <= 1_048_576, "profile configuration exceeds one MiB");
    // TOML errors may include credential-bearing source lines. Never propagate them.
    let config: toml::Value = toml::from_str(text).map_err(|_| anyhow::anyhow!("invalid profile configuration TOML (source redacted)"))?;
    let profiles = config.get("profiles").and_then(toml::Value::as_table)
        .ok_or_else(|| anyhow::anyhow!("configuration has no profiles table"))?;
    ensure!(profiles.len() <= 128, "too many profiles");
    let Some(value) = profiles.get(name) else { bail!("requested profile is not configured"); };
    let profile: Profile = value.clone().try_into()
        .map_err(|_| anyhow::anyhow!("invalid profile fields (source redacted); check the documented schema"))?;
    validate(&profile)?;
    let mut blockers = vec!["installed-version compatibility has not been verified", "no verified launch adapter evidence", "permission policy has not been resolved", "profile is not bound to an immutable attempt"];
    if profile.model.is_some() { blockers.push("model request requires a verified adapter mapping"); }
    if profile.reasoning_effort.is_some() { blockers.push("reasoning effort requires a verified adapter mapping"); }
    if !profile.environment.is_empty() { blockers.push("environment references require an approved execution environment"); }
    if profile.budget.is_some() { blockers.push("budget request requires admission and usage policy resolution"); }
    let inspection = Inspection {
        schema_version: 1,
        name: name.to_owned(),
        kind: profile.kind.clone(),
        config_digest: format!("{:x}", Sha256::digest(text.as_bytes())),
        profile_digest: format!("{:x}", Sha256::digest(serde_json::to_vec(&profile)?)),
        extra_argument_count: profile.extra_args.len(),
        environment_reference_count: profile.environment.len(),
        model_requested: profile.model.is_some(),
        reasoning_effort_requested: profile.reasoning_effort.is_some(),
        budget_requested: profile.budget.is_some(),
        capabilities: Capabilities {
            launch: Capability::Unknown, readiness_observation: Capability::Unknown,
            prompt_submission: Capability::Unknown, stop: Capability::Unknown,
            checkpoint_acknowledgment: Capability::Unknown, structured_usage: Capability::Unknown,
            resume: Capability::Unknown,
        },
        agent_version: None, herdr_version: None,
        launchable: false, protocol_capable: false, certified: false, blockers,
    };
    Ok((inspection, profile.budget))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_keep_separate_flags_and_redact_all_requested_values() {
        let text = "[profiles.planner]\nkind='claude'\npermission_policy='interactive'\nextra_args=['SECRET_ARG']\n[profiles.worker]\nkind='codex'\npermission_policy='interactive'\nextra_args=['OTHER_SECRET']\nmodel='SECRET_MODEL'\nreasoning_effort='SECRET_EFFORT'\nenvironment=['SECRET_ENV']\n";
        let planner = inspect_text(text, "planner").unwrap();
        let worker = inspect_text(text, "worker").unwrap();
        assert_eq!(planner.kind, "claude");
        assert_eq!(worker.kind, "codex");
        assert_ne!(planner.profile_digest, worker.profile_digest);
        let output = serde_json::to_string(&worker).unwrap();
        assert!(!output.contains("SECRET"));
        assert!(!worker.launchable && !worker.protocol_capable && !worker.certified);
        assert!(worker.blockers.iter().any(|b| b.contains("model request")));
        assert_eq!(planner.config_digest, worker.config_digest);
        let changed = inspect_text(&text.replace("OTHER_SECRET", "CHANGED"), "worker").unwrap();
        assert_ne!(changed.profile_digest, worker.profile_digest);
        assert_ne!(changed.config_digest, worker.config_digest);
    }

    #[test]
    fn malformed_and_unsupported_configuration_never_echoes_secrets() {
        for tail in ["extra_args='SECRET'", "environment=['TOKEN=SECRET']", "unknown='SECRET'", "model=123 # SECRET", "extra_args=['SECRET'", "environment=['TOKEN','TOKEN']", "[profiles.p.budget]\nmax_wall_seconds=0\nunknown_usage='block'"] {
            let text = format!("[profiles.p]\nkind='claude'\npermission_policy='interactive'\n{tail}");
            let error = inspect_text(&text, "p").err().expect("must refuse invalid profile");
            assert!(!format!("{error:#}").contains("SECRET"));
        }
    }
}
