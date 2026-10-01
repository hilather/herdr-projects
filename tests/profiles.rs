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
    fn new(extra_args: &str) -> Self { Self::with_budget(extra_args, "[profiles.worker.budget]\nmax_wall_seconds=60\nunknown_usage='allow_with_warning'\n") }
    /// As `new`, with `budget` as the rest of the profile definition.
    fn with_budget(extra_args: &str, budget: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".config/herdr-projects/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key='ssh-ed25519 {}'\n[profiles.worker]\nkind='claude'\n\
permission_policy='interactive'\nextra_args={extra_args}\n{budget}", "A".repeat(48))).unwrap();
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

/// Replaces `preparation_checks_whole_brief_and_supported_budget_policy`.
///
/// A worker profile is prepared only with a wall deadline of one second to
/// one week and positive token limits whose character estimate fits; the
/// refusal writes nothing. (A brief over the input budget and a profile that
/// blocks on unknown usage are refused in tests/canonical_worker.rs.)
#[test]
fn prepare_refuses_worker_budgets_outside_the_supported_bounds() {
    let budget = |fields: &str| format!("[profiles.worker.budget]\n{fields}unknown_usage='allow_with_warning'\n");
    for (definition, error) in [
        (String::new(), "worker profile requires a wall deadline"),
        (budget(""), "worker profile requires a wall deadline"),
        (budget("max_wall_seconds=0\n"), "profile budget limits must be positive bounded integers"),
        (budget("max_wall_seconds=604801\n"), "worker wall deadline exceeds supported bounds"),
        (budget(&format!("max_wall_seconds=60\nsoft_input_tokens={}\n", i64::MAX)), "profile token budget exceeds character conversion bounds"),
        (budget("max_wall_seconds=60\nsoft_output_tokens=0\n"), "profile budget limits must be positive bounded integers"),
    ] {
        let lab = Lab::with_budget("[]", &definition);
        let refused = lab.refused("prepare", &[]);
        assert!(refused.contains(error), "{definition}: {refused}");
    }
    let lab = Lab::with_budget("[]", &budget("max_wall_seconds=604800\nsoft_input_tokens=100\n"));
    let out = lab.profile("prepare", &[]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

impl Lab {
    /// A migrated project whose `worker` profile has `kind` and `fields` (extra TOML
    /// lines such as pins) and whose agent fixture answers `version` (Codex is
    /// probed with an execution-home `HOME`, Claude without one).
    fn pinned(kind: &str, fields: &str, version: &str) -> Self {
        let lab = Lab::with_budget("[]", &format!("{fields}\n[profiles.worker.budget]\nmax_wall_seconds=60\nunknown_usage='allow_with_warning'\n"));
        let config = fs::read_to_string(&lab.config).unwrap();
        // The kind is part of the pinned configuration: rebuild the project over it.
        drop(lab);
        let home = tempfile::tempdir().unwrap();
        let config_path = home.path().join(".config/herdr-projects/config.toml");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(&config_path, config.replace("kind='claude'", &format!("kind='{kind}'"))).unwrap();
        let lab = Lab { project: home.path().join("root/demo"), config: config_path, home };
        for command in ["new", "pause"] { lab.ok(&[command, "demo"]); }
        migration::apply(&lab.project, &migration::inspect_with_config(&lab.project, &lab.config).unwrap(), true).unwrap();
        fs::create_dir(lab.path("agent-home")).unwrap();
        fs::write(lab.path("herdr"), "#!/bin/sh\n[ \"$1\" = --version ] && echo 'herdr 0.9.1' && exit 0\nexit 3\n").unwrap();
        fs::write(lab.path("claude"), format!("#!/bin/sh\n[ \"$1\" = --version ] && echo '{version}' && exit 0\nexit 3\n")).unwrap();
        for name in ["herdr", "claude"] { fs::set_permissions(lab.path(name), fs::Permissions::from_mode(0o700)).unwrap(); }
        lab
    }
}

/// Model and reasoning effort are validated profile fields for Codex and Claude
/// (no passthrough arguments): preparation freezes them into the profile
/// identity and `profile inspect` shows what will be pinned.
#[test]
fn model_and_effort_pins_are_profile_fields_that_inspect_shows_and_identity_binds() {
    for (kind, model, version) in [("codex", "gpt-6.1-sol", "codex-cli 0.154.0"), ("claude", "claude-sonnet-5-5", "2.1.0 (Claude Code)")] {
        let pinned = Lab::pinned(kind, &format!("model='{model}'\nreasoning_effort='low'"), version);
        let inspected = pinned.ok(&["profile", "inspect", "worker"]);
        assert_eq!((inspected["kind"].as_str(), inspected["pinned_model"].as_str(), inspected["pinned_reasoning_effort"].as_str()), (Some(kind), Some(model), Some("low")), "{inspected}");
        assert_eq!(inspected["extra_argument_count"], 0);
        assert!(!inspected["blockers"].to_string().contains("model request") && !inspected["blockers"].to_string().contains("reasoning effort"), "{inspected}");
        let prepared = pinned.profile("prepare", &[]);
        assert!(prepared.status.success(), "{kind}: {}", String::from_utf8_lossy(&prepared.stderr));
        let prepared: Value = serde_json::from_slice(&prepared.stdout).unwrap();
        let profile: FrozenProfile = serde_json::from_value(prepared["profile"].clone()).unwrap();
        assert_eq!(profile.kind, kind);
        // No passthrough argument carries the pin; the definition identity does.
        assert_eq!(profile.arguments_digest, "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945", "empty extra_args");
        let plain = Lab::pinned(kind, "", version);
        let other: Value = serde_json::from_slice(&plain.profile("prepare", &[]).stdout).unwrap();
        assert_ne!(other["profile"]["definition_digest"], prepared["profile"]["definition_digest"], "the pins are part of the profile identity");
        // Pins write nothing by themselves: the home is prepared by verification and launch.
        assert!(fs::read_dir(pinned.path("agent-home")).unwrap().next().is_none());
    }
}

/// Anything that is not a plain lowercase model or effort name is refused
/// before any work, without echoing the value.
#[test]
fn invalid_pins_are_refused_without_echoing_them() {
    for fields in ["model='Private Model!'", "reasoning_effort='PRIVATE'", "model='gpt-6.1-sol'\nreasoning_effort=''"] {
        let lab = Lab::pinned("codex", fields, "codex-cli 0.154.0");
        let before = runtime::snapshot(&lab.project).unwrap();
        let refused = lab.profile("prepare", &[]);
        assert!(!refused.status.success(), "{fields}");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(!stderr.contains("Private") && !stderr.contains("PRIVATE"), "{stderr}");
        assert_eq!(runtime::snapshot(&lab.project).unwrap(), before);
    }
}
