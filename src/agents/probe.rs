//! Bounded local installation evidence. Never authorizes a launch or certifies a workflow.
use std::{fs::OpenOptions, io::Read, os::unix::fs::{OpenOptionsExt, PermissionsExt}, path::{Path, PathBuf}, time::Duration};
use anyhow::{Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use crate::runner::{Cmd, Runner};
use super::profiles::{Inspection, inspect};

const EXECUTABLE_LIMIT: u64 = 512 * 1024 * 1024;

#[derive(Serialize)]
pub struct Probe {
    schema_version: u32,
    scope: &'static str,
    profile: Inspection,
    herdr: VersionEvidence,
    agent: VersionEvidence,
    /// Digest of the complete evidence, including config, binary and version identities.
    evidence_digest: String,
}

#[derive(Serialize)]
struct VersionEvidence {
    executable: PathBuf,
    executable_digest: String,
    version: Option<String>,
    output_digest: Option<String>,
    status: &'static str,
}

fn identity(path: &Path) -> Result<(PathBuf, String)> {
    ensure!(path.is_absolute(), "probe executable paths must be absolute");
    let canonical = std::fs::canonicalize(path).map_err(|_| anyhow::anyhow!("probe executable cannot be resolved"))?;
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&canonical)
        .map_err(|_| anyhow::anyhow!("probe executable cannot be opened"))?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.permissions().mode() & 0o111 != 0, "probe executable must be an executable regular file");
    ensure!(metadata.len() <= EXECUTABLE_LIMIT, "probe executable exceeds size limit");
    let mut reader = file.take(EXECUTABLE_LIMIT + 1);
    let mut digest = Sha256::new();
    let mut count = 0u64;
    let mut bytes = [0u8; 65536];
    loop {
        let read = reader.read(&mut bytes)?;
        if read == 0 { break; }
        count += read as u64;
        digest.update(&bytes[..read]);
    }
    ensure!(count <= EXECUTABLE_LIMIT && count == metadata.len(), "probe executable changed while reading");
    Ok((canonical, format!("{:x}", digest.finalize())))
}

fn version(kind: &str, text: &str) -> Option<String> {
    let text = text.trim();
    let token = match kind {
        "herdr" => text.strip_prefix("herdr ")?,
        "codex" => text.strip_prefix("codex-cli ").or_else(|| text.strip_prefix("codex "))?,
        "claude" => text.strip_suffix(" (Claude Code)")?,
        _ => return None,
    };
    if token.len() > 96 || token.is_empty() || !token.bytes().all(|c| c.is_ascii_alphanumeric() || b".-+".contains(&c)) { return None; }
    let core = token.split(['-', '+']).next()?;
    let parts = core.split('.').collect::<Vec<_>>();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || p.parse::<u64>().is_err()) { return None; }
    Some(token.into())
}

fn run_version(path: &Path, kind: &str, runner: &dyn Runner) -> Result<VersionEvidence> {
    let (executable, executable_digest) = identity(path)?;
    let mut evidence = VersionEvidence { executable: executable.clone(), executable_digest, version: None, output_digest: None, status: "probe_failed" };
    if !matches!(kind, "herdr" | "claude" | "codex") {
        evidence.status = "no_verified_version_adapter";
        return Ok(evidence);
    }
    let mut cmd = Cmd::new(executable.to_str().ok_or_else(|| anyhow::anyhow!("probe executable path must be UTF-8"))?, Duration::from_secs(5)).arg("--version");
    cmd.capture_limit = 4096;
    // No user arguments, model intent, credentials or environment references are applied.
    if let Ok(output) = runner.run(&cmd) {
        if output.success() && !output.stdout_truncated && !output.stderr_truncated && output.stdout.len() <= 4096 && output.stderr.len() <= 4096 {
            evidence.output_digest = Some(format!("{:x}", Sha256::digest(output.stdout.as_bytes())));
            evidence.version = version(kind, &output.stdout);
            evidence.status = if evidence.version.is_some() { "version_observed" } else { "unrecognized_version_output" };
        }
    }
    let after = identity(path)?;
    ensure!(after.0 == executable && after.1 == evidence.executable_digest, "probe executable changed during observation; evidence discarded");
    Ok(evidence)
}

pub fn probe(config: &Path, name: &str, herdr: &Path, agent: &Path, runner: &dyn Runner) -> Result<Probe> {
    let mut profile = inspect(config, name)?;
    // Validate both files before invoking either executable.
    identity(herdr)?;
    identity(agent)?;
    let herdr = run_version(herdr, "herdr", runner)?;
    let agent = run_version(agent, &profile.kind, runner)?;
    ensure!(identity(&herdr.executable)? == (herdr.executable.clone(), herdr.executable_digest.clone())
        && identity(&agent.executable)? == (agent.executable.clone(), agent.executable_digest.clone()),
        "probe executable changed before evidence completion; evidence discarded");
    let after = inspect(config, name)?;
    ensure!(after.config_digest == profile.config_digest, "profile configuration changed during probing; evidence discarded");
    profile.agent_version = agent.version.clone();
    profile.herdr_version = herdr.version.clone();
    let evidence_digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&(&profile, &herdr, &agent))?));
    Ok(Probe { schema_version: 1, scope: "local_installation_only", profile, herdr, agent, evidence_digest })
}

impl Probe {
    pub fn profile(&self) -> &Inspection { &self.profile }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Output;
    use std::cell::RefCell;

    struct Fake { calls: RefCell<Vec<Cmd>>, response: String }
    impl Runner for Fake {
        fn socket_request(&self, _: &Path, _: &str, _: Duration) -> Result<String> { panic!("probe must not access sessions") }
        fn run(&self, cmd: &Cmd) -> Result<Output> {
            self.calls.borrow_mut().push(cmd.clone());
            Ok(Output { code: Some(0), stdout: self.response.clone(), ..Default::default() })
        }
    }
    fn executable(dir: &Path) -> PathBuf {
        let path = dir.join("binary");
        std::fs::write(&path, b"fixture executable").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    #[test]
    fn version_probe_preserves_prerelease_and_bounds_commands() {
        let temp = tempfile::tempdir().unwrap();
        let path = executable(temp.path());
        let fake = Fake { calls: RefCell::new(vec![]), response: "codex-cli 0.99.0-preview.3\n".into() };
        let evidence = run_version(&path, "codex", &fake).unwrap();
        assert_eq!(evidence.version.as_deref(), Some("0.99.0-preview.3"));
        let calls = fake.calls.borrow();
        assert_eq!(calls[0].args, ["--version"]);
        assert!(calls[0].own_group && calls[0].env.is_empty() && calls[0].stdin.is_none());
        assert_eq!(calls[0].timeout, Duration::from_secs(5));
        assert_eq!(calls[0].capture_limit, 4096);
    }
    #[test]
    fn unknown_adapter_does_not_execute_and_raw_errors_are_withheld() {
        let temp = tempfile::tempdir().unwrap();
        let path = executable(temp.path());
        let fake = Fake { calls: RefCell::new(vec![]), response: "SECRET invalid output".into() };
        assert_eq!(run_version(&path, "muse", &fake).unwrap().status, "no_verified_version_adapter");
        assert!(fake.calls.borrow().is_empty());
        let evidence = run_version(&path, "claude", &fake).unwrap();
        assert_eq!(evidence.status, "unrecognized_version_output");
        assert!(!serde_json::to_string(&evidence).unwrap().contains("SECRET"));
        assert_eq!(version("claude", "2.1.0 (Claude Code)"), Some("2.1.0".into()));
        assert!(version("herdr", "herdr 0.9.1\nSECRET").is_none());
    }

    #[test]
    fn changing_executable_or_config_discards_observation() {
        struct Mutator { path: PathBuf, bytes: Vec<u8> }
        impl Runner for Mutator {
            fn socket_request(&self, _: &Path, _: &str, _: Duration) -> Result<String> { panic!("probe must not access sessions") }
            fn run(&self, _: &Cmd) -> Result<Output> {
                std::fs::write(&self.path, &self.bytes)?;
                Ok(Output { code: Some(0), stdout: "herdr 0.9.1".into(), ..Default::default() })
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let path = executable(temp.path());
        let mutator = Mutator { path: path.clone(), bytes: b"changed executable".to_vec() };
        assert!(run_version(&path, "herdr", &mutator).is_err());
        let config = temp.path().join("config.toml");
        let original = "[profiles.p]\nkind='claude'\npermission_policy='interactive'\n";
        std::fs::write(&config, original).unwrap();
        let mutator = Mutator { path: config.clone(), bytes: format!("{original}# changed").into_bytes() };
        assert!(probe(&config, "p", &path, &path, &mutator).is_err());
    }

    #[test]
    fn failed_truncated_or_timed_out_commands_cannot_supply_versions() {
        struct Failed(Output);
        impl Runner for Failed {
            fn socket_request(&self, _: &Path, _: &str, _: Duration) -> Result<String> { panic!("unexpected socket") }
            fn run(&self, _: &Cmd) -> Result<Output> { Ok(self.0.clone()) }
        }
        let temp = tempfile::tempdir().unwrap();
        let path = executable(temp.path());
        for mode in 0..4 {
            let mut out = Output { code: Some(0), stdout: "herdr 0.9.1".into(), stderr: "SECRET".into(), ..Default::default() };
            match mode { 0 => out.code = Some(1), 1 => out.timed_out = true, 2 => out.stdout_truncated = true, _ => out.cancelled = true }
            let evidence = run_version(&path, "herdr", &Failed(out)).unwrap();
            assert!(evidence.version.is_none());
            assert_eq!(evidence.status, "probe_failed");
            assert!(!serde_json::to_string(&evidence).unwrap().contains("SECRET"));
        }
    }
}
