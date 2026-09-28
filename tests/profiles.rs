#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Worker profile preparation and native verification through the compiled
//! CLI: `profile prepare`, `profile verify-native` and `profile retained`
//! over a disposable migrated project. herdr and the agent are shell
//! fixtures that only answer `--version` and log every invocation.
use herdr_projects::{authority, domain::*, migration, runtime};
use serde_json::Value;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

struct Lab { home: tempfile::TempDir, project: PathBuf, config: PathBuf }

impl Lab {
    /// A paused migrated project whose `worker` profile passes `extra_args`.
    fn new(extra_args: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".config/herdr-projects/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key='ssh-ed25519 {}'\n[profiles.worker]\nkind='claude'\n\
permission_policy='interactive'\nextra_args={extra_args}\n[profiles.worker.budget]\nmax_wall_seconds=60\nunknown_usage='allow_with_warning'\n", "A".repeat(48))).unwrap();
        let lab = Lab { project: home.path().join("root/demo"), config, home };
        for command in ["new", "pause"] { lab.ok(&[command, "demo"]); }
        migration::apply(&lab.project, &migration::inspect_with_config(&lab.project, &lab.config).unwrap(), true).unwrap();
        fs::create_dir(lab.path("agent-home")).unwrap();
        for (name, version) in [("herdr", "herdr 0.9.1"), ("claude", "2.1.0-preview.1 (Claude Code)")] {
            let log = lab.path(&format!("{name}-calls"));
            fs::write(lab.path(name), format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n\
[ \"$#\" = 1 ] && [ \"$1\" = --version ] && [ -z \"$HOME\" ] && [ \"$PATH\" = /usr/bin:/bin ] || exit 3\nprintf '%s\\n' '{version}'\n", log.display())).unwrap();
            fs::set_permissions(lab.path(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        lab
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.path("root").to_str().unwrap()]).args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    /// `profile <verb> demo worker` over the fixture executables.
    fn profile(&self, verb: &str, extra: &[&str]) -> Output {
        let (herdr, agent, home) = (self.path("herdr"), self.path("claude"), self.path("agent-home"));
        let mut args = vec!["profile", verb, "demo", "worker", "--herdr-executable", herdr.to_str().unwrap(),
            "--agent-executable", agent.to_str().unwrap(), "--execution-home", home.to_str().unwrap()];
        args.extend(extra);
        self.cli(&args)
    }
    /// A refused `profile <verb>` that changed no project state; returns its stderr.
    fn refused(&self, verb: &str, extra: &[&str]) -> String {
        let before = runtime::snapshot(&self.project).unwrap();
        let out = self.profile(verb, extra);
        assert!(!out.status.success(), "{verb} accepted: {}", String::from_utf8_lossy(&out.stdout));
        assert_eq!(runtime::snapshot(&self.project).unwrap(), before, "{verb} was refused but wrote");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn calls(&self, name: &str) -> Vec<String> { fs::read_to_string(self.path(&format!("{name}-calls"))).unwrap_or_default().lines().map(str::to_owned).collect() }
}

/// Replaces `production_preparation_binds_inputs_without_fabricating_capabilities_or_authority`.
#[test]
fn prepare_freezes_the_installation_without_capabilities_authority_or_secrets() {
    let lab = Lab::new("['PRIVATE_ARG']");
    let before = runtime::snapshot(&lab.project).unwrap();
    let out = lab.profile("prepare", &[]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("PRIVATE_ARG"), "{text}");
    let prepared: Value = serde_json::from_str(&text).unwrap();
    let profile: FrozenProfile = serde_json::from_value(prepared["profile"].clone()).unwrap();
    assert_eq!(prepared["reference"], serde_json::to_value(profile.reference().unwrap()).unwrap());
    assert_eq!((profile.name.as_str(), profile.kind.as_str()), ("worker", "claude"));
    assert_eq!((profile.agent.version.as_str(), profile.herdr.version.as_str()), ("2.1.0-preview.1", "0.9.1"));
    assert_eq!(profile.execution_home.as_deref(), lab.path("agent-home").to_str());
    assert_eq!(profile.permission_policy, authority::policy_reference(&lab.project).unwrap());
    // Nothing is fabricated: no capability, launchability or certificate.
    let c = &profile.capabilities;
    for evidence in [&c.launch, &c.readiness_observation, &c.prompt_submission, &c.stop, &c.checkpoint_acknowledgment, &c.structured_usage, &c.resume] {
        assert_eq!(evidence, &CapabilityEvidence::Unknown);
    }
    assert!(profile.workflow_certificate.is_none() && profile.validate_for_launch().is_err());
    assert_eq!((&prepared["launchable"], &prepared["protocol_capable"], &prepared["certified"]), (&Value::Bool(false), &Value::Bool(false), &Value::Bool(false)));
    // Preparation writes nothing, retains nothing, and repeats to the same reference.
    assert_eq!(runtime::snapshot(&lab.project).unwrap(), before);
    let digest = prepared["reference"]["digest"].as_str().unwrap();
    let retained = lab.cli(&["profile", "retained", "demo", digest]);
    assert!(!retained.status.success() && String::from_utf8_lossy(&retained.stderr).contains("retained native profile not found"));
    let again: Value = serde_json::from_slice(&lab.profile("prepare", &[]).stdout).unwrap();
    assert_eq!(again["reference"], prepared["reference"]);
    // The executables were only asked for their versions.
    assert!(lab.calls("claude").iter().chain(&lab.calls("herdr")).all(|c| c == "--version"));
}

/// Replaces `native_verification_refuses_unmapped_startup_arguments_before_launch`.
#[test]
fn verify_native_refuses_unmapped_startup_arguments_before_launching_anything() {
    let lab = Lab::new("['PRIVATE_ARG']");
    let error = lab.refused("verify-native", &["--retain"]);
    assert!(error.contains("empty extra_args"), "{error}");
    assert!(!error.contains("PRIVATE_ARG"), "{error}");
    assert!(lab.calls("claude").iter().all(|c| c == "--version"), "{:?}", lab.calls("claude"));
    assert!(lab.calls("herdr").iter().all(|c| c == "--version"), "{:?}", lab.calls("herdr"));
}

/// Replaces `native_verification_does_not_promote_version_only_helpers`.
#[test]
fn verify_native_does_not_retain_a_helper_that_only_answers_its_version() {
    let lab = Lab::new("[]");
    let digest = lab.ok(&["profile", "prepare", "demo", "worker", "--herdr-executable", lab.path("herdr").to_str().unwrap(),
        "--agent-executable", lab.path("claude").to_str().unwrap(), "--execution-home", lab.path("agent-home").to_str().unwrap()])["reference"]["digest"]
        .as_str().unwrap().to_owned();
    let error = lab.refused("verify-native", &["--retain"]);
    // The helper cannot serve the disposable probe session, so nothing is observed.
    assert!(error.contains("native probe server exited"), "{error}");
    assert_eq!(lab.calls("herdr").last().map(String::as_str), Some("server"));
    let retained = lab.cli(&["profile", "retained", "demo", &digest]);
    assert!(!retained.status.success() && String::from_utf8_lossy(&retained.stderr).contains("retained native profile not found"));
    assert!(lab.calls("claude").iter().all(|c| c == "--version"), "{:?}", lab.calls("claude"));
}
