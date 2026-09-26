//! Opt-in Phase B storage. Legacy commands do not create or open this database.
#[cfg(feature = "state-store")]
pub mod domain;
#[cfg(feature = "state-store")]
pub mod store;

#[cfg(feature = "state-store")]
pub mod migration;
#[cfg(feature = "state-store")]
pub mod projections;

#[cfg(feature = "state-store")]
pub mod operations;
#[cfg(feature = "state-store")]
pub mod runtime;
#[cfg(feature="state-store")]
pub mod reconcile;

/// Bounded external command execution shared by the CLI and trusted library ingress.
pub mod runner;
pub mod execution_guard;
pub mod supervision;
pub mod status_notice;
pub mod copy_receipt;
pub mod review_notice;
pub mod live_copy_intent;
pub mod final_copy_intent;
#[cfg(feature = "state-store")]
pub mod authority;
#[cfg(feature = "state-store")]
pub mod routines;
#[cfg(feature = "state-store")]
pub mod memory;

/// Schedule semantics shared by legacy and durable routines.
pub mod schedule;

pub mod prompt_claim;
pub mod launch_claim;
pub mod coordinator_prime;
pub mod notification_claim;
pub mod worker_supervision;
#[cfg(all(feature="state-store",target_os="linux"))]
pub mod source_tree;
#[cfg(all(feature="state-store",target_os="linux"))]
pub mod worktree_preservation;
#[cfg(feature = "state-store")]
pub mod canonical_worker;

pub mod profile_config;

#[cfg(feature = "state-store")]
pub mod profile_preparation;

#[cfg(all(feature = "state-store", target_os = "linux"))]
pub mod launch_preparation;

#[cfg(all(feature = "state-store", target_os = "linux"))]
pub mod admission;

#[cfg(feature = "state-store")]
pub mod watchdog;

#[cfg(feature = "state-store")]
pub mod factory_status;

#[cfg(all(feature = "state-store", target_os = "linux"))]
pub mod worktree_preparation;

#[cfg(all(feature = "state-store", target_os = "linux"))]
pub mod verification;

#[cfg(all(feature = "state-store", target_os = "linux"))]
pub mod integration;

#[cfg(test)]
mod agents {
    mod probe {
        use crate::runner::Runner;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;
        use std::time::Duration;

        fn discover_claude() -> Option<PathBuf> {
            let path = std::env::var_os("PATH")?;
            for dir in std::env::split_paths(&path) {
                let candidate = dir.join("claude");
                let Ok(meta) = std::fs::metadata(&candidate) else {
                    continue;
                };
                if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                    return Some(candidate);
                }
            }
            None
        }

        fn mappings_refused(kind: &str) {
            let profile: crate::profile_config::ProfileDefinition = toml::from_str(&format!(
                "kind='{kind}'\npermission_policy='interactive'\nmodel='PRIVATE_MODEL'\nreasoning_effort='PRIVATE_EFFORT'\nenvironment=['HOME']\n[budget]\nmax_wall_seconds=10\nunknown_usage='allow_with_warning'\n"
            ))
            .unwrap();
            let text = format!("{:#}", profile.validate_gated_preparation(1).unwrap_err());
            assert!(text.contains("unsupported model"), "{text}");
            assert!(!text.contains("PRIVATE_MODEL"));
            assert!(!text.contains("PRIVATE_EFFORT"));
            assert!(!text.contains("HOME"));
        }

        fn manifest(status: &str, binary_present: bool) -> serde_json::Value {
            serde_json::json!({
                "schema_version": 1,
                "kind": "claude",
                "status": status,
                "binary_present": binary_present,
                "launchable": false,
                "protocol_capable": false,
                "certified": false,
                "workflow_certified": false,
                "live_launch": false,
                "model_mapping": "refused",
                "effort_mapping": "refused",
                "environment_mapping": "refused",
                "capability_levels": [],
            })
        }

        fn assert_closed(value: &serde_json::Value) {
            assert_eq!(value["kind"], "claude");
            assert_eq!(value["launchable"], false);
            assert_eq!(value["protocol_capable"], false);
            assert_eq!(value["certified"], false);
            assert_eq!(value["workflow_certified"], false);
            assert_eq!(value["live_launch"], false);
            assert_eq!(value["model_mapping"], "refused");
            assert_eq!(value["effort_mapping"], "refused");
            assert_eq!(value["environment_mapping"], "refused");
            assert_eq!(value["capability_levels"], serde_json::json!([]));
            let status = value["status"].as_str().unwrap();
            assert!(
                status != "launchable" && status != "workflow-certified" && status != "certified"
            );
        }

        #[test]
        fn claude_capability_harness_records_unsupported_without_certifying() {
            mappings_refused("claude");
            mappings_refused("codex");
            assert_eq!(
                crate::profile_config::observed_version("codex", "codex-cli 0.99.0-preview.3\n")
                    .as_deref(),
                Some("0.99.0-preview.3")
            );
            let temp = tempfile::tempdir().unwrap();
            let controller = include_str!("canonical_controller.rs");
            assert!(controller.contains("const PREPARED_LAUNCH_DISPATCH_ENABLED: bool = true;"));
            let manifest = if let Some(binary) = discover_claude() {
                let program = binary.to_str().expect("claude path is UTF-8");
                let mut cmd = crate::runner::Cmd::new(program, Duration::from_secs(5));
                cmd.args.push("--version".into());
                cmd.env_clear = true;
                cmd.env = vec![
                    ("PATH".into(), "/usr/bin:/bin".into()),
                    ("LANG".into(), "C".into()),
                    ("LC_ALL".into(), "C".into()),
                ];
                cmd.cwd = Some(PathBuf::from("/"));
                cmd.capture_limit = 4096;
                assert_eq!(cmd.args, ["--version"]);
                // A failed --version is an uncertified probe_failed manifest, not an absent skip.
                let status = match crate::runner::RealRunner.run(&cmd) {
                    Ok(output) if output.success() => {
                        if crate::profile_config::observed_version("claude", &output.stdout)
                            .is_some()
                        {
                            "version_observed"
                        } else {
                            "unrecognized_version_output"
                        }
                    }
                    _ => "probe_failed",
                };
                let present = manifest(status, true);
                assert_ne!(present["status"], "unsupported");
                assert_eq!(present["binary_present"], true);
                present
            } else {
                manifest("unsupported", false)
            };
            let manifest_path = temp.path().join("claude-capability-manifest.json");
            let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
            std::fs::write(&manifest_path, &bytes).unwrap();
            let read = std::fs::read(&manifest_path).unwrap();
            assert_eq!(read, bytes);
            let written: serde_json::Value = serde_json::from_slice(&read).unwrap();
            assert_closed(&written);
            if discover_claude().is_none() {
                assert_eq!(written["status"], "unsupported");
                assert_eq!(written["binary_present"], false);
            } else {
                assert_eq!(written["binary_present"], true);
                assert_ne!(written["status"], "unsupported");
            }
            // This check does not open a project database or record capability evidence.
            assert!(!temp.path().join("state.db").exists());
            mappings_refused("claude");
        }
    }
}
