#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Named agent profiles through the compiled CLI: `profile resolve` and
//! `profile probe` over the owner's `config.toml`. The probed executables are
//! shell fixtures that log every invocation; nothing is launched.
use serde_json::Value;
use std::{fs, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

struct Home { dir: tempfile::TempDir }

impl Home {
    fn new() -> Self {
        let home = Home { dir: tempfile::tempdir().unwrap() };
        fs::create_dir_all(home.path(".config/herdr-projects")).unwrap();
        home
    }
    fn path(&self, name: &str) -> PathBuf { self.dir.path().join(name) }
    fn config(&self, text: &str) { fs::write(self.path(".config/herdr-projects/config.toml"), text).unwrap(); }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.dir.path()).env("PATH", "/usr/bin:/bin").args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> (Value, String) {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8(out.stdout).unwrap();
        (serde_json::from_str(&text).unwrap(), text)
    }
    fn refused(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(!out.status.success(), "{args:?} accepted: {}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    /// An executable that logs its arguments to `NAME-calls` and prints `output`.
    fn executable(&self, name: &str, output: &str) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s\\n' '{output}'\n", self.path(&format!("{name}-calls")).display())).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    fn calls(&self, name: &str) -> String { fs::read_to_string(self.path(&format!("{name}-calls"))).unwrap_or_default() }
}

fn budget(value: &Value) -> (Value, Value, Value, Value, Value) {
    let b = &value["budget"];
    (b["max_wall_seconds"].clone(), b["soft_input_chars"].clone(), b["soft_output_chars"].clone(), b["unknown_usage"].clone(), b["estimator"].clone())
}

/// Replaces `resolve_uses_named_profile_budget_and_never_emits_argv_or_frozen`,
/// `missing_budget_defaults_to_brief_character_cap` and
/// `unknown_usage_refuses_missing_usage_as_zero`.
///
/// A named profile's token budget becomes a character envelope (4 per token);
/// a missing budget, or one without token limits, falls back to the 32,000
/// character brief cap, never to zero. Arguments are never printed, nothing
/// is frozen, and `--agent KIND` selects the one profile whose `kind` is KIND,
/// never `profiles.KIND`.
#[test]
fn profile_resolve_turns_token_budgets_into_character_envelopes_without_arguments() {
    let home = Home::new();
    home.config("[profiles.implementation]\nkind='codex'\npermission_policy='interactive'\nextra_args=['SECRET_ARG']\n\
[profiles.implementation.budget]\nmax_wall_seconds=12\nsoft_input_tokens=100\nsoft_output_tokens=50\nunknown_usage='block'\n\
[profiles.planner]\nkind='claude'\npermission_policy='interactive'\nextra_args=['OTHER_SECRET']\n\
[profiles.guarded]\nkind='grok'\npermission_policy='interactive'\n[profiles.guarded.budget]\nmax_wall_seconds=10\nunknown_usage='block'\n");
    let (resolved, text) = home.ok(&["profile", "resolve", "implementation"]);
    assert_eq!((resolved["name"].as_str(), resolved["kind"].as_str()), (Some("implementation"), Some("codex")));
    assert_eq!(budget(&resolved), (12.into(), 400.into(), 200.into(), "block".into(), "char-count-v1".into()));
    assert!(resolved["frozen"].is_null());
    assert_eq!((&resolved["inspection"]["launchable"], &resolved["inspection"]["certified"]), (&Value::Bool(false), &Value::Bool(false)));
    assert!(!text.contains("SECRET") && !text.contains("extra_args"), "{text}");

    let (planner, text) = home.ok(&["profile", "resolve", "--agent", "claude"]);
    assert_eq!(planner["name"], "planner");
    assert_eq!(budget(&planner), (Value::Null, 32_000.into(), Value::Null, "allow_with_warning".into(), "char-count-v1".into()));
    assert!(!text.contains("SECRET"), "{text}");
    // Blocking unknown usage without token limits keeps the brief cap.
    let (guarded, _) = home.ok(&["profile", "resolve", "guarded"]);
    assert_eq!(budget(&guarded), (10.into(), 32_000.into(), Value::Null, "block".into(), "char-count-v1".into()));
    assert!(home.refused(&["profile", "resolve", "--agent", "muse"]).contains("no named profile"));

    home.config("[profiles.a]\nkind='codex'\npermission_policy='interactive'\n[profiles.b]\nkind='codex'\npermission_policy='interactive'\n");
    assert!(home.refused(&["profile", "resolve", "--agent", "codex"]).contains("multiple named profiles"));
    home.config("[profiles.implementation]\nkind='codex'\npermission_policy='interactive'\n");
    home.refused(&["profile", "resolve", "--agent", "implementation"]);
    assert_eq!(home.ok(&["profile", "resolve", "--agent", "codex"]).0["name"], "implementation");
    // A budget must say what unavailable usage means, and a zero limit is not a limit.
    for bad in ["soft_input_tokens=5", "soft_input_tokens=0\nunknown_usage='allow_with_warning'"] {
        home.config(&format!("[profiles.p]\nkind='codex'\npermission_policy='interactive'\n[profiles.p.budget]\nmax_wall_seconds=10\n{bad}\n"));
        home.refused(&["profile", "resolve", "p"]);
    }
    assert!(!home.path(".herdr-projects").exists());
}

fn probe(home: &Home, herdr: &Path, agent: &Path) -> (Value, String) {
    home.ok(&["profile", "probe", "worker", "--herdr-executable", herdr.to_str().unwrap(), "--agent-executable", agent.to_str().unwrap()])
}

/// Replaces `version_probe_preserves_prerelease_and_bounds_commands` and
/// `unknown_adapter_does_not_execute_and_raw_errors_are_withheld`.
///
/// Only kinds with a verified version adapter are executed, each once with
/// `--version` alone; a pre-release version is kept whole. Output that is
/// not a recognized version supplies no version and is never echoed.
#[test]
fn profile_probe_runs_only_verified_adapters_and_withholds_unrecognized_output() {
    let home = Home::new();
    let herdr = home.executable("herdr", "herdr 0.9.1");
    home.config("[profiles.worker]\nkind='codex'\npermission_policy='interactive'\n");
    let codex = home.executable("codex", "codex-cli 0.99.0-preview.3");
    let (report, _) = probe(&home, &herdr, &codex);
    assert_eq!((report["agent"]["status"].as_str(), report["agent"]["version"].as_str()), (Some("version_observed"), Some("0.99.0-preview.3")));
    assert_eq!(report["herdr"]["version"], "0.9.1");
    assert_eq!((home.calls("codex"), home.calls("herdr")), ("--version\n".to_owned(), "--version\n".to_owned()));

    // No verified adapter for the kind: the agent executable is never run.
    home.config("[profiles.worker]\nkind='muse'\npermission_policy='interactive'\n");
    let muse = home.executable("muse", "muse 1.0.0");
    let (report, _) = probe(&home, &herdr, &muse);
    assert_eq!(report["agent"]["status"], "no_verified_version_adapter");
    assert!(report["agent"]["version"].is_null());
    assert_eq!(home.calls("muse"), "");

    // Unrecognized output from either executable is withheld.
    home.config("[profiles.worker]\nkind='claude'\npermission_policy='interactive'\n");
    let claude = home.executable("claude", "SECRET invalid output");
    let noisy = home.executable("noisy-herdr", "herdr 0.9.1\nSECRET");
    let (report, text) = probe(&home, &noisy, &claude);
    assert_eq!((report["agent"]["status"].as_str(), report["herdr"]["status"].as_str()), (Some("unrecognized_version_output"), Some("unrecognized_version_output")));
    assert!(report["agent"]["version"].is_null() && report["herdr"]["version"].is_null());
    assert!(!text.contains("SECRET"), "{text}");
    assert_eq!(home.calls("claude"), "--version\n");
}
