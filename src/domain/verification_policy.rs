//! Versioned executable acceptance policy, shared by contract ingress and verifier.
use anyhow::{Result, bail};
use serde::Deserialize;
use std::{collections::BTreeMap, path::Path};

#[derive(Deserialize)]
pub(crate) struct ExecutionPolicy {
    pub version: u32,
    pub checks: Vec<String>,
    #[serde(default)]
    pub rerun_on_failure: u8,
    #[serde(default)]
    pub named_checks: BTreeMap<String, Vec<String>>,
    pub stress: Option<Stress>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Stress {
    pub checks: Vec<String>,
    pub repetitions: u8,
    pub concurrency: u8,
    pub load: Option<Vec<String>>,
}
fn argv(args: &[String]) -> bool {
    !args.is_empty()
        && args.len() <= 32
        && Path::new(&args[0]).is_absolute()
        && args
            .iter()
            .all(|s| !s.is_empty() && s.len() <= 4096 && !s.contains('\0'))
}
impl ExecutionPolicy {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > 4000 {
            bail!("policy document exceeds bounds");
        }
        let policy: Self = serde_json::from_slice(bytes)?;
        let raw: serde_json::Value = serde_json::from_slice(bytes)?;
        if !matches!(policy.version, 1 | 2)
            || !argv(&policy.checks)
            || policy.rerun_on_failure > 2
            || policy.named_checks.len() > 6
            || (policy.version == 1
                && ["rerun_on_failure", "stress", "named_checks"]
                    .iter()
                    .any(|key| raw.get(key).is_some()))
        {
            bail!("policy checks exceed bounds or require version 2");
        }
        for (name, args) in &policy.named_checks {
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
                || !argv(args)
            {
                bail!("invalid named check");
            }
        }
        if let Some(stress) = &policy.stress {
            if !(1..=6).contains(&stress.repetitions)
                || !(1..=5).contains(&stress.concurrency)
                || stress.checks.is_empty()
                || stress.checks.len() > 6
                || stress
                    .checks
                    .iter()
                    .any(|name| !policy.named_checks.contains_key(name))
                || stress.load.as_ref().is_some_and(|args| !argv(args))
            {
                bail!("stress policy exceeds bounds");
            }
            let unique: std::collections::BTreeSet<_> = stress.checks.iter().collect();
            if unique.len() != stress.checks.len() {
                bail!("duplicate stress check");
            }
        }
        Ok(policy)
    }
    pub(crate) fn commands(&self) -> impl Iterator<Item = &[String]> {
        std::iter::once(self.checks.as_slice())
            .chain(self.named_checks.values().map(Vec::as_slice))
            .chain(self.stress.iter().filter_map(|s| s.load.as_deref()))
    }
}
