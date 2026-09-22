//! Bounded local installation evidence. Never authorizes a launch or certifies a workflow.
use super::profiles::{Inspection, inspect};
use crate::runner::{Cancellation, Cmd, Runner};
use anyhow::{Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

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
    #[serde(skip)]
    identity: Identity,
}

const PROBE_BUDGET: Duration = Duration::from_secs(20);
struct Control {
    deadline: Instant,
    cancellation: Cancellation,
}
impl Control {
    fn check(&self) -> Result<()> {
        ensure!(
            !self.cancellation.is_cancelled() && Instant::now() < self.deadline,
            "profile probe cancelled or deadline exhausted; evidence discarded"
        );
        Ok(())
    }
}
#[derive(Clone, PartialEq, Eq)]
struct Identity {
    path: PathBuf,
    digest: String,
    stamp: (u64, u64, u64, i64, i64, i64, i64, u32),
}
fn stamp(m: &std::fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64, u32) {
    (
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
        m.mode(),
    )
}
fn identity(path: &Path, control: &Control) -> Result<Identity> {
    control.check()?;
    ensure!(
        path.is_absolute(),
        "probe executable paths must be absolute"
    );
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| anyhow::anyhow!("probe executable cannot be resolved"))?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(&canonical)
        .map_err(|_| anyhow::anyhow!("probe executable cannot be opened"))?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.permissions().mode() & 0o111 != 0,
        "probe executable must be an executable regular file"
    );
    ensure!(
        before.len() <= EXECUTABLE_LIMIT,
        "probe executable exceeds size limit"
    );
    let mut digest = Sha256::new();
    let mut count = 0u64;
    let mut bytes = [0u8; 65536];
    loop {
        control.check()?;
        let read = file.read(&mut bytes)?;
        if read == 0 {
            break;
        }
        count += read as u64;
        ensure!(
            count <= EXECUTABLE_LIMIT,
            "probe executable exceeds read limit"
        );
        digest.update(&bytes[..read]);
    }
    let after = file.metadata()?;
    let named = std::fs::symlink_metadata(&canonical)?;
    ensure!(
        count == before.len()
            && stamp(&before) == stamp(&after)
            && named.is_file()
            && stamp(&before) == stamp(&named)
            && std::fs::canonicalize(path)? == canonical,
        "probe executable changed while reading; evidence discarded"
    );
    control.check()?;
    Ok(Identity {
        path: canonical,
        digest: format!("{:x}", digest.finalize()),
        stamp: stamp(&before),
    })
}

fn version(kind: &str, text: &str) -> Option<String> {
    herdr_projects::profile_config::observed_version(kind, text)
}

#[cfg(test)]
fn run_version(path: &Path, kind: &str, runner: &dyn Runner) -> Result<VersionEvidence> {
    let control = Control {
        deadline: Instant::now() + PROBE_BUDGET,
        cancellation: Default::default(),
    };
    let expected = identity(path, &control)?;
    run_version_controlled(path, kind, runner, &expected, &control)
}
fn run_version_controlled(
    path: &Path,
    kind: &str,
    runner: &dyn Runner,
    expected: &Identity,
    control: &Control,
) -> Result<VersionEvidence> {
    ensure!(
        identity(path, control)? == *expected,
        "probe executable changed before execution; evidence discarded"
    );
    let mut evidence = VersionEvidence {
        executable: expected.path.clone(),
        executable_digest: expected.digest.clone(),
        version: None,
        output_digest: None,
        status: "probe_failed",
        identity: expected.clone(),
    };
    if !matches!(kind, "herdr" | "claude" | "codex") {
        evidence.status = "no_verified_version_adapter";
        return Ok(evidence);
    }
    control.check()?;
    let timeout =
        Duration::from_secs(5).min(control.deadline.saturating_duration_since(Instant::now()));
    let mut cmd = Cmd::new(
        expected
            .path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("probe executable path must be UTF-8"))?,
        timeout,
    )
    .arg("--version");
    cmd.capture_limit = 4096;
    cmd.deadline = Some(control.deadline);
    cmd.cancellation = Some(control.cancellation.clone());
    // No inherited credentials, loader hooks, session selection, config paths or
    // user arguments. Only a fixed helper search path and locale reach --version.
    cmd.env_clear = true;
    cmd.env = vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("LANG".into(), "C".into()),
        ("LC_ALL".into(), "C".into()),
    ];
    cmd.cwd = Some(PathBuf::from("/"));
    if let Ok(output) = runner.run(&cmd) {
        if output.success()
            && !output.stdout_truncated
            && !output.stderr_truncated
            && output.stdout.len() <= 4096
            && output.stderr.len() <= 4096
        {
            evidence.output_digest =
                Some(format!("{:x}", Sha256::digest(output.stdout.as_bytes())));
            evidence.version = version(kind, &output.stdout);
            evidence.status = if evidence.version.is_some() {
                "version_observed"
            } else {
                "unrecognized_version_output"
            };
        }
    }
    control.check()?;
    ensure!(
        identity(path, control)? == *expected,
        "probe executable changed during observation; evidence discarded"
    );
    Ok(evidence)
}

pub fn probe(
    config: &Path,
    name: &str,
    herdr: &Path,
    agent: &Path,
    runner: &dyn Runner,
) -> Result<Probe> {
    probe_controlled(
        config,
        name,
        herdr,
        agent,
        runner,
        Instant::now() + PROBE_BUDGET,
        Default::default(),
    )
}
fn probe_controlled(
    config: &Path,
    name: &str,
    herdr_path: &Path,
    agent_path: &Path,
    runner: &dyn Runner,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<Probe> {
    let control = Control {
        deadline: deadline.min(Instant::now() + PROBE_BUDGET),
        cancellation,
    };
    control.check()?;
    let mut profile = inspect(config, name)?;
    // Validate and pin both files before invoking either executable. Keep the
    // originally selected paths to detect version-manager symlink retargeting.
    let herdr_identity = identity(herdr_path, &control)?;
    let agent_identity = identity(agent_path, &control)?;
    let herdr = run_version_controlled(herdr_path, "herdr", runner, &herdr_identity, &control)?;
    let agent =
        run_version_controlled(agent_path, &profile.kind, runner, &agent_identity, &control)?;
    ensure!(
        identity(herdr_path, &control)? == herdr.identity
            && identity(agent_path, &control)? == agent.identity,
        "probe executable changed before evidence completion; evidence discarded"
    );
    let after = inspect(config, name)?;
    ensure!(
        after.config_digest == profile.config_digest,
        "profile configuration changed during probing; evidence discarded"
    );
    control.check()?;
    profile.agent_version = agent.version.clone();
    profile.herdr_version = herdr.version.clone();
    let evidence_digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(&profile, &herdr, &agent))?)
    );
    Ok(Probe {
        schema_version: 1,
        scope: "local_installation_only",
        profile,
        herdr,
        agent,
        evidence_digest,
    })
}

impl Probe {
    pub fn profile(&self) -> &Inspection {
        &self.profile
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Output;
    use std::cell::RefCell;

    struct Fake {
        calls: RefCell<Vec<Cmd>>,
        response: String,
    }
    impl Runner for Fake {
        fn socket_request(&self, _: &Path, _: &str, _: Duration) -> Result<String> {
            panic!("probe must not access sessions")
        }
        fn run(&self, cmd: &Cmd) -> Result<Output> {
            self.calls.borrow_mut().push(cmd.clone());
            Ok(Output {
                code: Some(0),
                stdout: self.response.clone(),
                ..Default::default()
            })
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
        let fake = Fake {
            calls: RefCell::new(vec![]),
            response: "codex-cli 0.99.0-preview.3\n".into(),
        };
        let evidence = run_version(&path, "codex", &fake).unwrap();
        assert_eq!(evidence.version.as_deref(), Some("0.99.0-preview.3"));
        let calls = fake.calls.borrow();
        assert_eq!(calls[0].args, ["--version"]);
        assert!(calls[0].own_group && calls[0].env_clear && calls[0].stdin.is_none());
        assert_eq!(calls[0].timeout, Duration::from_secs(5));
        assert_eq!(calls[0].capture_limit, 4096);
    }
    #[test]
    fn unknown_adapter_does_not_execute_and_raw_errors_are_withheld() {
        let temp = tempfile::tempdir().unwrap();
        let path = executable(temp.path());
        let fake = Fake {
            calls: RefCell::new(vec![]),
            response: "SECRET invalid output".into(),
        };
        assert_eq!(
            run_version(&path, "muse", &fake).unwrap().status,
            "no_verified_version_adapter"
        );
        assert!(fake.calls.borrow().is_empty());
        let evidence = run_version(&path, "claude", &fake).unwrap();
        assert_eq!(evidence.status, "unrecognized_version_output");
        assert!(!serde_json::to_string(&evidence).unwrap().contains("SECRET"));
        assert_eq!(
            version("claude", "2.1.0 (Claude Code)"),
            Some("2.1.0".into())
        );
        assert!(version("herdr", "herdr 0.9.1\nSECRET").is_none());
    }

    #[test]
    fn changing_executable_or_config_discards_observation() {
        struct Mutator {
            path: PathBuf,
            bytes: Vec<u8>,
        }
        impl Runner for Mutator {
            fn socket_request(&self, _: &Path, _: &str, _: Duration) -> Result<String> {
                panic!("probe must not access sessions")
            }
            fn run(&self, _: &Cmd) -> Result<Output> {
                std::fs::write(&self.path, &self.bytes)?;
                Ok(Output {
                    code: Some(0),
                    stdout: "herdr 0.9.1".into(),
                    ..Default::default()
                })
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let path = executable(temp.path());
        let mutator = Mutator {
            path: path.clone(),
            bytes: b"changed executable".to_vec(),
        };
        assert!(run_version(&path, "herdr", &mutator).is_err());
        let config = temp.path().join("config.toml");
        let original = "[profiles.p]\nkind='claude'\npermission_policy='interactive'\n";
        std::fs::write(&config, original).unwrap();
        let mutator = Mutator {
            path: config.clone(),
            bytes: format!("{original}# changed").into_bytes(),
        };
        assert!(probe(&config, "p", &path, &path, &mutator).is_err());
    }

    #[test]
    fn failed_truncated_or_timed_out_commands_cannot_supply_versions() {
        struct Failed(Output);
        impl Runner for Failed {
            fn socket_request(&self, _: &Path, _: &str, _: Duration) -> Result<String> {
                panic!("unexpected socket")
            }
            fn run(&self, _: &Cmd) -> Result<Output> {
                Ok(self.0.clone())
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let path = executable(temp.path());
        for mode in 0..4 {
            let mut out = Output {
                code: Some(0),
                stdout: "herdr 0.9.1".into(),
                stderr: "SECRET".into(),
                ..Default::default()
            };
            match mode {
                0 => out.code = Some(1),
                1 => out.timed_out = true,
                2 => out.stdout_truncated = true,
                _ => out.cancelled = true,
            }
            let evidence = run_version(&path, "herdr", &Failed(out)).unwrap();
            assert!(evidence.version.is_none());
            assert_eq!(evidence.status, "probe_failed");
            assert!(!serde_json::to_string(&evidence).unwrap().contains("SECRET"));
        }
    }
    #[test]
    fn identical_bytes_in_a_replaced_executable_do_not_preserve_probe_identity() {
        struct Replacer {
            path: PathBuf,
        }
        impl Runner for Replacer {
            fn socket_request(&self, _: &Path, _: &str, _: Duration) -> Result<String> {
                unreachable!()
            }
            fn run(&self, _: &Cmd) -> Result<Output> {
                let replacement = self.path.with_extension("replacement");
                std::fs::copy(&self.path, &replacement)?;
                std::fs::rename(replacement, &self.path)?;
                Ok(Output {
                    code: Some(0),
                    stdout: "herdr 0.9.1".into(),
                    ..Default::default()
                })
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let path = executable(temp.path());
        let original = std::fs::File::open(&path).unwrap();
        assert!(run_version(&path, "herdr", &Replacer { path: path.clone() }).is_err());
        assert_ne!(
            original.metadata().unwrap().ino(),
            std::fs::metadata(path).unwrap().ino()
        );
    }

    #[test]
    fn second_probe_cannot_hide_a_retargeted_first_executable_alias() {
        struct Retarget {
            alias: PathBuf,
            other: PathBuf,
            calls: RefCell<usize>,
        }
        impl Runner for Retarget {
            fn socket_request(&self, _: &Path, _: &str, _: Duration) -> Result<String> {
                unreachable!()
            }
            fn run(&self, _: &Cmd) -> Result<Output> {
                let mut calls = self.calls.borrow_mut();
                *calls += 1;
                if *calls == 2 {
                    std::fs::remove_file(&self.alias)?;
                    std::os::unix::fs::symlink(&self.other, &self.alias)?;
                }
                Ok(Output {
                    code: Some(0),
                    stdout: if *calls == 1 {
                        "herdr 0.9.1"
                    } else {
                        "2.1.0 (Claude Code)"
                    }
                    .into(),
                    ..Default::default()
                })
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let path = executable(temp.path());
        let other = temp.path().join("other");
        std::fs::copy(&path, &other).unwrap();
        let alias = temp.path().join("selected");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let config = temp.path().join("config.toml");
        std::fs::write(
            &config,
            "[profiles.p]\nkind='claude'\npermission_policy='interactive'\n",
        )
        .unwrap();
        let runner = Retarget {
            alias: alias.clone(),
            other,
            calls: RefCell::new(0),
        };
        assert!(probe(&config, "p", &alias, &path, &runner).is_err());
        assert_eq!(*runner.calls.borrow(), 2);
    }

    #[test]
    fn cancellation_and_one_original_deadline_prevent_followup_version_execution() {
        struct Stop {
            calls: RefCell<usize>,
            cancellation: Cancellation,
            expire: bool,
        }
        impl Runner for Stop {
            fn socket_request(&self, _: &Path, _: &str, _: Duration) -> Result<String> {
                unreachable!()
            }
            fn run(&self, cmd: &Cmd) -> Result<Output> {
                *self.calls.borrow_mut() += 1;
                assert!(cmd.env_clear && cmd.cancellation.is_some());
                assert_eq!(cmd.cwd.as_deref(), Some(Path::new("/")));
                assert_eq!(
                    cmd.env,
                    vec![
                        ("PATH".into(), "/usr/bin:/bin".into()),
                        ("LANG".into(), "C".into()),
                        ("LC_ALL".into(), "C".into())
                    ]
                );
                if self.expire {
                    std::thread::sleep(
                        cmd.deadline
                            .unwrap()
                            .saturating_duration_since(Instant::now())
                            + Duration::from_millis(2),
                    );
                } else {
                    self.cancellation.cancel();
                }
                Ok(Output {
                    code: Some(0),
                    stdout: "herdr 0.9.1".into(),
                    ..Default::default()
                })
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let path = executable(temp.path());
        let config = temp.path().join("config.toml");
        std::fs::write(
            &config,
            "[profiles.p]\nkind='claude'\npermission_policy='interactive'\n",
        )
        .unwrap();
        for expire in [false, true] {
            let cancellation = Cancellation::default();
            let runner = Stop {
                calls: RefCell::new(0),
                cancellation: cancellation.clone(),
                expire,
            };
            assert!(
                probe_controlled(
                    &config,
                    "p",
                    &path,
                    &path,
                    &runner,
                    Instant::now() + Duration::from_millis(100),
                    cancellation
                )
                .is_err()
            );
            assert_eq!(*runner.calls.borrow(), 1);
        }
        let runner = Fake {
            calls: RefCell::new(vec![]),
            response: "herdr 0.9.1".into(),
        };
        assert!(
            probe_controlled(
                &config,
                "p",
                &path,
                &path,
                &runner,
                Instant::now(),
                Default::default()
            )
            .is_err()
        );
        assert!(runner.calls.borrow().is_empty());
    }
    #[test]
    fn real_version_process_receives_only_the_explicit_probe_environment() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("agent");
        std::fs::write(&path, b"#!/bin/sh\n[ -z \"${HOME+x}${HERDR_SESSION+x}${HERDR_SOCKET_PATH+x}${LD_PRELOAD+x}${BASH_ENV+x}\" ] || exit 41\n[ \"$PATH\" = /usr/bin:/bin ] && [ \"$LC_ALL\" = C ] && [ \"$PWD\" = / ] || exit 42\nprintf 'herdr 0.9.1\\n'\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let evidence = run_version(&path, "herdr", &crate::runner::RealRunner).unwrap();
        assert_eq!(evidence.version.as_deref(), Some("0.9.1"));
    }
}
