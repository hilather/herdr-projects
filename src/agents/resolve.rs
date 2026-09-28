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
