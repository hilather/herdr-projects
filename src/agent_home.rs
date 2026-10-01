//! Execution-home preparation for supported canonical worker kinds (Codex and
//! Claude Code). It writes the agent's own configuration inside the isolated
//! execution home: the pinned model and reasoning effort, a permission mode
//! suited to the worker sandbox, trust for exact working directories, and a
//! quiet startup. It never copies a login; the sandbox binds the owner's single
//! login file into the home (see `worker_supervision::Isolation::with_login`).
//! Profiles stay free of passthrough arguments: model and effort reach the
//! agent only through this configuration, and verification reads them back.
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const MAX_CONFIG: u64 = 256 * 1024;
/// Claude tools the worker may use without prompting: the OS sandbox, not the
/// agent's prompts, is the boundary for a canonical worker.
const CLAUDE_ALLOW: &[&str] = &["Bash", "Read", "Edit", "Write", "Glob", "Grep"];

pub fn supported(kind: &str) -> bool {
    matches!(kind, "codex" | "claude")
}

/// Model and effort names written into agent configuration: lowercase
/// identifiers, so nothing in them can be mistaken for markup or a path.
pub fn valid_pin(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._:-".contains(&b))
}

/// The login file shared with the owner's CLI, relative to the owner's home and
/// to the execution home (the same relative path in both).
pub fn login_file(kind: &str) -> Option<&'static str> {
    match kind {
        "codex" => Some(".codex/auth.json"),
        "claude" => Some(".claude/.credentials.json"),
        _ => None,
    }
}

/// What the agent's own configuration pins, as read back from the home.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pins {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

fn read_config(path: &Path) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(metadata.is_file() && metadata.len() <= MAX_CONFIG, "agent configuration must be a regular file of at most 256 KiB");
    let mut text = String::new();
    fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?.take(MAX_CONFIG).read_to_string(&mut text)
        .map_err(|_| anyhow::anyhow!("agent configuration is not UTF-8 text"))?;
    Ok(Some(text))
}

fn private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) => ensure!(m.is_dir() && m.uid() == unsafe { libc::geteuid() }, "agent configuration directory is not an owner-controlled directory"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::DirBuilder::new().mode(0o700).create(path)?,
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Replace `path` atomically with `bytes` (owner-only), unless already equal.
fn write_config(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Ok(Some(old)) = read_config(path)
        && old.as_bytes() == bytes
    {
        return Ok(());
    }
    let temporary = path.with_extension(format!("hp-{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })?;
    Ok(())
}

/// Each directory by its given name and, when different, its canonical one.
fn trusted_paths(paths: &[&Path]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for path in paths {
        ensure!(path.is_absolute(), "trusted directory {} must be absolute", path.display());
        for form in [Some(path.to_path_buf()), path.canonicalize().ok()].into_iter().flatten() {
            let text = form.to_str().context("trusted directory is not UTF-8")?.to_owned();
            if !out.contains(&text) {
                out.push(text);
            }
        }
    }
    Ok(out)
}

/// Prepare `home` for `kind`: pin `model`/`effort` (when given), set the
/// sandbox-suited permission defaults (only where the owner has not set them
/// in this home), trust exactly `trusted` (absolute directories) and, for
/// Claude Code (whose edit tools stop at the working directory; Codex gets
/// the same list as a launch argument), name the extra `writable` directories
/// the worker sandbox already makes writable. Safe to repeat; unrelated settings in the home's configuration are preserved.
pub fn prepare(kind: &str, home: &Path, pins: &Pins, trusted: &[&Path], writable: &[String]) -> Result<()> {
    for value in [&pins.model, &pins.reasoning_effort].into_iter().flatten() {
        ensure!(valid_pin(value), "invalid model or reasoning effort pin");
    }
    ensure!(home.is_absolute(), "execution home must be absolute");
    let trusted = trusted_paths(trusted)?;
    match kind {
        "codex" => prepare_codex(home, pins, &trusted),
        "claude" => prepare_claude(home, pins, &trusted, writable),
        _ => {
            ensure!(pins.model.is_none() && pins.reasoning_effort.is_none(), "profile kind has no verified model or effort mapping");
            Ok(())
        }
    }
}

fn prepare_codex(home: &Path, pins: &Pins, trusted: &[String]) -> Result<()> {
    let directory = home.join(".codex");
    private_dir(&directory)?;
    let path = directory.join("config.toml");
    let mut table: toml::Table = match read_config(&path)? {
        Some(text) => toml::from_str(&text).map_err(|_| anyhow::anyhow!("invalid Codex configuration (contents withheld)"))?,
        None => toml::Table::new(),
    };
    for (key, value) in [("approval_policy", "never"), ("sandbox_mode", "workspace-write")] {
        table.entry(key).or_insert_with(|| value.into());
    }
    table.entry("check_for_update_on_startup").or_insert(false.into());
    if let Some(model) = &pins.model {
        table.insert("model".into(), model.clone().into());
    }
    if let Some(effort) = &pins.reasoning_effort {
        table.insert("model_reasoning_effort".into(), effort.clone().into());
    }
    let workspace = table.entry("sandbox_workspace_write").or_insert_with(|| toml::Table::new().into());
    let workspace = workspace.as_table_mut().context("invalid Codex sandbox_workspace_write table")?;
    workspace.entry("network_access").or_insert(false.into());
    let projects = table.entry("projects").or_insert_with(|| toml::Table::new().into());
    let projects = projects.as_table_mut().context("invalid Codex projects table")?;
    for directory in trusted {
        let entry = projects.entry(directory.clone()).or_insert_with(|| toml::Table::new().into());
        entry.as_table_mut().context("invalid Codex project entry")?.insert("trust_level".into(), "trusted".into());
    }
    write_config(&path, toml::to_string(&table)?.as_bytes())
}

fn json_object(text: Option<String>) -> Result<serde_json::Map<String, serde_json::Value>> {
    match text {
        Some(text) => match serde_json::from_str(&text) {
            Ok(serde_json::Value::Object(map)) => Ok(map),
            _ => bail!("invalid Claude configuration (contents withheld)"),
        },
        None => Ok(Default::default()),
    }
}

fn prepare_claude(home: &Path, pins: &Pins, trusted: &[String], writable: &[String]) -> Result<()> {
    use serde_json::{Value, json};
    let directory = home.join(".claude");
    private_dir(&directory)?;
    let path = directory.join("settings.json");
    let mut settings = json_object(read_config(&path)?)?;
    if let Some(model) = &pins.model {
        settings.insert("model".into(), json!(model));
    }
    if let Some(effort) = &pins.reasoning_effort {
        settings.insert("effortLevel".into(), json!(effort));
    }
    let permissions = settings.entry("permissions").or_insert_with(|| json!({}));
    let permissions = permissions.as_object_mut().context("invalid Claude permissions")?;
    permissions.entry("defaultMode").or_insert_with(|| json!("acceptEdits"));
    let allow = permissions.entry("allow").or_insert_with(|| json!([]));
    let allow = allow.as_array_mut().context("invalid Claude permission allow list")?;
    for tool in CLAUDE_ALLOW {
        if !allow.iter().any(|v| v.as_str() == Some(tool)) {
            allow.push(json!(tool));
        }
    }
    let directories = permissions.entry("additionalDirectories").or_insert_with(|| json!([]));
    let directories = directories.as_array_mut().context("invalid Claude additional directories")?;
    for directory in writable {
        ensure!(Path::new(directory).is_absolute(), "writable directory must be absolute");
        if !directories.iter().any(|v| v.as_str() == Some(directory)) {
            directories.push(json!(directory));
        }
    }
    let env = settings.entry("env").or_insert_with(|| json!({}));
    env.as_object_mut().context("invalid Claude env")?.entry("DISABLE_AUTOUPDATER").or_insert_with(|| json!("1"));
    write_config(&path, (serde_json::to_string_pretty(&Value::Object(settings))? + "\n").as_bytes())?;

    // Onboarding and per-directory trust live beside the directory, not in it.
    let state_path = home.join(".claude.json");
    let mut state = json_object(read_config(&state_path)?)?;
    state.entry("hasCompletedOnboarding").or_insert_with(|| json!(true));
    state.entry("theme").or_insert_with(|| json!("dark"));
    let projects = state.entry("projects").or_insert_with(|| json!({}));
    let projects = projects.as_object_mut().context("invalid Claude projects")?;
    for directory in trusted {
        let entry = projects.entry(directory.clone()).or_insert_with(|| json!({}));
        let entry = entry.as_object_mut().context("invalid Claude project entry")?;
        entry.insert("hasTrustDialogAccepted".into(), json!(true));
        entry.insert("hasCompletedProjectOnboarding".into(), json!(true));
    }
    write_config(&state_path, (serde_json::to_string_pretty(&Value::Object(state))? + "\n").as_bytes())
}

/// The model and effort the agent's configuration in `home` pins right now.
pub fn read_pins(kind: &str, home: &Path) -> Result<Pins> {
    match kind {
        "codex" => {
            let text = read_config(&home.join(".codex/config.toml"))?;
            let table: toml::Table = match text {
                Some(text) => toml::from_str(&text).map_err(|_| anyhow::anyhow!("invalid Codex configuration (contents withheld)"))?,
                None => return Ok(Pins::default()),
            };
            let text = |key: &str| table.get(key).and_then(toml::Value::as_str).map(str::to_owned);
            Ok(Pins { model: text("model"), reasoning_effort: text("model_reasoning_effort") })
        }
        "claude" => {
            let settings = json_object(read_config(&home.join(".claude/settings.json"))?)?;
            let text = |key: &str| settings.get(key).and_then(|v| v.as_str()).map(str::to_owned);
            Ok(Pins { model: text("model"), reasoning_effort: text("effortLevel") })
        }
        _ => Ok(Pins::default()),
    }
}

/// Whether a lowercase visible screen shows the pinned model: its id, or for
/// Claude Code the display name it prints (`claude-sonnet-5-5` -> `sonnet 5.5`).
pub fn screen_shows_model(kind: &str, model: &str, screen: &str) -> bool {
    let screen = screen.to_lowercase();
    if screen.contains(&model.to_lowercase()) {
        return true;
    }
    if kind != "claude" {
        return false;
    }
    let mut parts = model.strip_prefix("claude-").unwrap_or(model).split('-');
    let Some(family) = parts.next().filter(|f| f.bytes().all(|b| b.is_ascii_alphabetic())) else { return false };
    let version: Vec<&str> = parts.take_while(|p| p.len() <= 2 && p.bytes().all(|b| b.is_ascii_digit())).collect();
    !version.is_empty() && screen.contains(&format!("{family} {}", version.join(".")))
}

/// Where an owner login lives for `kind`: `override_path` (absolute, from the
/// pinned owner configuration), or the first owner home holding the file.
pub fn login_source(kind: &str, owner_homes: &[String], override_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = override_path {
        return Some(path.to_owned());
    }
    let relative = login_file(kind)?;
    let candidates: Vec<PathBuf> = owner_homes.iter().map(|h| Path::new(h).join(relative)).collect();
    candidates.iter().find(|p| p.is_file()).or(candidates.first()).cloned()
}
