//! External configuration references contain fingerprints, never settings values.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigReference {
    pub path: String,
    pub digest: Option<String>,
}

pub fn config_reference(path: &Path) -> Result<ConfigReference> {
    ensure!(path.is_absolute(), "config reference must be absolute");
    let digest = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => anyhow::bail!("external config is unreadable (contents withheld)"),
        Ok(_) => {
            let bytes = read(path).map_err(|_| anyhow::anyhow!("external config is unreadable or exceeds bounds (contents withheld)"))?;
            let valid = std::str::from_utf8(&bytes).ok()
                .and_then(|text| toml::from_str::<toml::Table>(text).ok()).is_some();
            ensure!(valid, "external config is invalid (contents withheld)");
            Some(hash(&bytes))
        }
    };
    Ok(ConfigReference { path: path.to_str().context("non-UTF-8 config path")?.into(), digest })
}

/// Version 2 binds external config presence and bytes into the migration identity.
/// It preserves a reference only: user settings are neither copied nor rewritten.
pub fn inspect_with_config(project: &Path, config: &Path) -> Result<Plan> {
    let mut plan = inspect(project)?;
    plan.version = 2;
    plan.config = Some(config_reference(config)?);
    plan.digest = plan_digest(&plan)?;
    Ok(plan)
}

pub(super) fn plan_digest(plan: &Plan) -> Result<String> {
    match (plan.version, &plan.config) {
        (1, None) => Ok(hash(&serde_json::to_vec(&plan.sources)?)),
        (2, Some(config)) => {
            ensure!(Path::new(&config.path).is_absolute(), "config reference must be absolute");
            ensure!(config.digest.as_ref().is_none_or(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit())), "invalid config fingerprint");
            Ok(hash(&serde_json::to_vec(&(2, &plan.sources, config))?))
        }
        _ => anyhow::bail!("unsupported migration plan version or config reference"),
    }
}

/// CLI plans must be bound to this operator's resolved config location. Older
/// prepared journals remain recoverable through the original library protocol.
pub fn require_config_path(plan: &Plan, config: &Path) -> Result<()> {
    ensure!(plan.version == 2 && plan.config.as_ref().is_some_and(|r| Path::new(&r.path) == config),
        "plan is not bound to the current config path; regenerate the migration plan");
    Ok(())
}
