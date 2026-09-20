//! Named-profile budget envelopes. Pure configuration resolution; never launches
//! or writes store/format/probe state. A sealed per-attempt FrozenProfile producer
//! remains remaining T04.3 acceptance.
use std::path::Path;

use anyhow::{Result, ensure};
use serde::Serialize;

use super::probe::Probe;
use super::profiles::{Inspection, UnknownUsage, load, unique_name_for_kind};

const DEFAULT_SOFT_INPUT_CHARS: u64 = 32_000;
const ESTIMATOR: &str = "char-count-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileBudgetEnvelope {
    pub max_wall_seconds: Option<u64>,
    pub soft_input_chars: u64,
    pub soft_output_chars: Option<u64>,
    pub unknown_usage: UnknownUsage,
    pub estimator: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ResolvedProfile {
    pub name: String,
    pub kind: String,
    pub definition_digest: String,
    pub config_digest: String,
    pub budget: ProfileBudgetEnvelope,
    pub inspection: Inspection,
    /// Always null here: Probe supplies versions, not adapter/policy evidence.
    pub frozen: Option<serde_json::Value>,
}

fn chars(tokens: Option<u64>) -> Result<Option<u64>> {
    tokens.map(|n| n.checked_mul(4).ok_or_else(|| anyhow::anyhow!("profile token budget exceeds character conversion bounds"))).transpose()
}

fn envelope(budget: Option<super::profiles::Budget>) -> Result<ProfileBudgetEnvelope> {
    Ok(match budget {
        None => ProfileBudgetEnvelope {
            max_wall_seconds: None,
            soft_input_chars: DEFAULT_SOFT_INPUT_CHARS,
            soft_output_chars: None,
            unknown_usage: UnknownUsage::AllowWithWarning,
            estimator: ESTIMATOR,
        },
        Some(budget) => ProfileBudgetEnvelope {
            max_wall_seconds: budget.max_wall_seconds,
            soft_input_chars: chars(budget.soft_input_tokens)?.unwrap_or(DEFAULT_SOFT_INPUT_CHARS),
            soft_output_chars: chars(budget.soft_output_tokens)?,
            unknown_usage: budget.unknown_usage,
            estimator: ESTIMATOR,
        },
    })
}

/// Resolve one named profile. `evidence` is optional Probe; it never writes and
/// never constructs a launch-valid FrozenProfile.
pub fn resolve(name: &str, config: &Path, evidence: Option<&Probe>) -> Result<ResolvedProfile> {
    let (mut inspection, budget) = load(config, name)?;
    if let Some(probe) = evidence {
        let observed = probe.profile();
        ensure!(observed.name == inspection.name && observed.config_digest == inspection.config_digest && observed.profile_digest == inspection.profile_digest,
            "probe evidence does not match the current named profile");
        inspection.agent_version = observed.agent_version.clone();
        inspection.herdr_version = observed.herdr_version.clone();
        inspection.launchable = false;
        inspection.protocol_capable = false;
        inspection.certified = false;
    }
    Ok(ResolvedProfile {
        name: inspection.name.clone(),
        kind: inspection.kind.clone(),
        definition_digest: inspection.profile_digest.clone(),
        config_digest: inspection.config_digest.clone(),
        budget: envelope(budget)?,
        inspection,
        frozen: None,
    })
}

pub fn resolve_agent_kind(kind: &str, config: &Path, evidence: Option<&Probe>) -> Result<ResolvedProfile> {
    resolve(&unique_name_for_kind(config, kind)?, config, evidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::profiles::Inspection;

    #[test]
    fn mock_workflow_flags_distinguish_launchable_protocol_and_certified() {
        let cases = [
            ("mock-launch-only", true, false, false),
            ("mock-protocol", true, true, false),
            ("mock-certified", true, true, true),
        ];
        let flags: Vec<_> = cases.iter().map(|(name, launchable, protocol, certified)| {
            let inspection = Inspection::mock_workflow(name, *launchable, *protocol, *certified);
            assert_eq!(inspection.name, *name);
            (inspection.launchable, inspection.protocol_capable, inspection.certified)
        }).collect();
        assert_eq!(flags, vec![(true, false, false), (true, true, false), (true, true, true)]);
        let live = Inspection::mock_workflow("live-probe", false, false, false);
        assert!(!live.launchable && !live.protocol_capable && !live.certified);
    }

    #[test]
    fn resolve_uses_named_profile_budget_and_never_emits_argv_or_frozen() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "[profiles.implementation]\nkind='codex'\npermission_policy='interactive'\nextra_args=['SECRET_ARG']\n[profiles.implementation.budget]\nmax_wall_seconds=12\nsoft_input_tokens=100\nsoft_output_tokens=50\nunknown_usage='block'\n[profiles.planner]\nkind='claude'\npermission_policy='interactive'\nextra_args=['OTHER_SECRET']\n").unwrap();
        let resolved = resolve("implementation", &path, None).unwrap();
        assert_eq!(resolved.kind, "codex");
        assert_eq!(resolved.budget.soft_input_chars, 400);
        assert_eq!(resolved.budget.soft_output_chars, Some(200));
        assert_eq!(resolved.budget.max_wall_seconds, Some(12));
        assert_eq!(resolved.budget.unknown_usage, UnknownUsage::Block);
        assert_eq!(resolved.budget.estimator, "char-count-v1");
        assert!(resolved.frozen.is_none());
        assert!(!resolved.inspection.launchable && !resolved.inspection.certified);
        let json = serde_json::to_string(&resolved).unwrap();
        assert!(!json.contains("SECRET"));
        assert!(!json.contains("extra_args"));
        assert_eq!(resolve_agent_kind("claude", &path, None).unwrap().name, "planner");
        assert!(resolve_agent_kind("codex", &path, None).is_ok());
        std::fs::write(&path, "[profiles.a]\nkind='codex'\npermission_policy='interactive'\n[profiles.b]\nkind='codex'\npermission_policy='interactive'\n").unwrap();
        let error = resolve_agent_kind("codex", &path, None).unwrap_err().to_string();
        assert!(error.contains("multiple named profiles"));
        assert!(resolve_agent_kind("muse", &path, None).unwrap_err().to_string().contains("no named profile"));
        std::fs::write(&path, "[profiles.implementation]\nkind='codex'\npermission_policy='interactive'\n").unwrap();
        assert!(resolve_agent_kind("implementation", &path, None).is_err(), "must not look up profiles.<kind>");
        assert_eq!(resolve_agent_kind("codex", &path, None).unwrap().name, "implementation");
    }

    #[test]
    fn missing_budget_defaults_to_brief_character_cap() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "[profiles.planner]\nkind='claude'\npermission_policy='interactive'\n").unwrap();
        let resolved = resolve("planner", &path, None).unwrap();
        assert_eq!(resolved.budget.soft_input_chars, 32_000);
        assert_eq!(resolved.budget.soft_output_chars, None);
        assert_eq!(resolved.budget.unknown_usage, UnknownUsage::AllowWithWarning);
        assert!(resolved.frozen.is_none());
    }
}
