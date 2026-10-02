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
        ensure!(self.environment.is_empty(), "worker profile needs an unsupported environment mapping");
        // Model and effort are mapped only for kinds whose own configuration
        // the execution-home preparation writes and verification reads back.
        ensure!(
            (self.model.is_none() && self.reasoning_effort.is_none())
                || (crate::agent_home::supported(&self.kind)
                    && [&self.model, &self.reasoning_effort].into_iter().flatten().all(|v| crate::agent_home::valid_pin(v))),
            "worker profile needs an unsupported model or effort mapping"
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

/// Owner-declared extra paths hidden from isolated agents, read from the same
/// digest-pinned configuration bytes as the frozen profile (`[worker_isolation]
/// hide = [...]`): absolute paths, or `~/`-relative to each owner home. Use it
/// for an owner signing key or other secret outside the fixed hidden set.
#[cfg(feature = "state-store")]
pub(crate) fn frozen_isolation_hides(profile: &crate::domain::FrozenProfile) -> anyhow::Result<Vec<String>> {
    Ok(frozen_isolation(profile)?.hide)
}

/// The `[worker_isolation]` table of the digest-pinned owner configuration.
/// `share_login` shares the worker's login (default on): for Codex the owner's
/// `auth.json` (an explicit `codex = "/abs/path"` under `[worker_isolation.login]`
/// binds another file instead); for Claude Code the setup-token file named by
/// `claude_token_file = "/abs/path"` there, never a bound credentials file.
#[cfg(feature = "state-store")]
#[derive(Default)]
pub(crate) struct IsolationConfig {
    pub hide: Vec<String>,
    pub share_login: Option<bool>,
    pub login: std::collections::BTreeMap<String, std::path::PathBuf>,
    /// The long-lived Claude setup-token file (`[worker_isolation.login]
    /// claude_token_file`): a Claude worker's login.
    pub claude_token_file: Option<std::path::PathBuf>,
}

#[cfg(feature = "state-store")]
impl IsolationConfig {
    /// The login file override for `kind`, or `None` for the owner's default.
    pub fn login_override(&self, kind: &str) -> Option<std::path::PathBuf> {
        self.login.get(kind).cloned()
    }
    pub fn shares_login(&self) -> bool {
        self.share_login.unwrap_or(true)
    }
}

#[cfg(feature = "state-store")]
pub(crate) fn frozen_isolation(profile: &crate::domain::FrozenProfile) -> anyhow::Result<IsolationConfig> {
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
    let Some(table) = config.get("worker_isolation") else { return Ok(IsolationConfig::default()) };
    let table = table.as_table().context("invalid worker_isolation table")?;
    ensure!(table.keys().all(|k| matches!(k.as_str(), "hide" | "share_login" | "login")), "unknown worker_isolation setting");
    let share_login = table.get("share_login").map(|v| v.as_bool().context("worker_isolation.share_login must be a boolean")).transpose()?;
    let mut login = std::collections::BTreeMap::new();
    let mut claude_token_file = None;
    if let Some(entries) = table.get("login") {
        for (key, path) in entries.as_table().context("worker_isolation.login must be a table")? {
            let path = path.as_str().context("worker_isolation.login paths must be strings")?;
            ensure!(path.starts_with('/') && path.len() <= 1024 && !path.contains('\0'), "worker_isolation.login paths must be absolute");
            match key.as_str() {
                "claude_token_file" => claude_token_file = Some(std::path::PathBuf::from(path)),
                "codex" => {
                    login.insert(key.clone(), std::path::PathBuf::from(path));
                }
                "claude" => anyhow::bail!("a Claude worker no longer binds a credentials file; set worker_isolation.login.claude_token_file to a `claude setup-token` file"),
                _ => anyhow::bail!("worker_isolation.login takes codex or claude_token_file"),
            }
        }
    }
    let hide = table
        .get("hide")
        .map(|v| v.as_array().context("worker_isolation.hide must be a list of paths"))
        .transpose()?
        .map(|v| v.iter().map(|p| p.as_str().map(str::to_owned).context("worker_isolation.hide entries must be strings")).collect::<anyhow::Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();
    ensure!(
        hide.len() <= 16 && hide.iter().all(|p| p.starts_with('/') || p.starts_with("~/")),
        "worker_isolation.hide takes at most 16 absolute or ~/ paths"
    );
    Ok(IsolationConfig { hide, share_login, login, claude_token_file })
}

/// Share the owner's login with the worker the profile launches, unless the
/// pinned owner configuration turns sharing off (`share_login = false`, for an
/// owner who copies a token into the execution home themselves).
#[cfg(feature = "state-store")]
pub(crate) fn share_login(
    isolation: crate::worker_supervision::Isolation,
    profile: &crate::domain::FrozenProfile,
    home: &std::path::Path,
) -> anyhow::Result<crate::worker_supervision::Isolation> {
    let config = frozen_isolation(profile)?;
    if !config.shares_login() {
        return Ok(isolation);
    }
    if profile.kind == "claude" {
        // A profile without a token file has no worker login; verification and
        // `launch run` refuse such a profile (see [`check_worker_login`]).
        return match &config.claude_token_file {
            Some(file) => isolation.with_login_token_file(file),
            None => Ok(isolation),
        };
    }
    isolation.with_shared_login(&profile.kind, home, config.login_override(&profile.kind).as_deref())
}

/// The Claude setup-token file of a profile whose worker login is shared, or the
/// refusal that stops a launch which could only fail with "login expired".
#[cfg(feature = "state-store")]
fn claude_token_file(config: &IsolationConfig) -> anyhow::Result<std::path::PathBuf> {
    config.claude_token_file.clone().ok_or_else(|| anyhow::anyhow!(
        "no Claude worker login is configured: create a long-lived token with `claude setup-token`, save it in a 0600 file outside the project and the agent directories, set `claude_token_file = \"/abs/path\"` under [worker_isolation.login] in the owner configuration, then prepare and verify the profile again"))
}

/// Refuse a frozen Claude profile whose pinned owner configuration names no
/// usable setup-token file, before anything is reserved for it. Other kinds and
/// profiles that do not share a login pass.
#[cfg(feature = "state-store")]
pub fn check_worker_login(profile: &crate::domain::FrozenProfile, project: &std::path::Path) -> anyhow::Result<()> {
    let config = frozen_isolation(profile)?;
    if profile.kind != "claude" || !config.shares_login() {
        return Ok(());
    }
    crate::agent_home::check_token_file(&claude_token_file(&config)?, project, std::path::Path::new(""))
}
