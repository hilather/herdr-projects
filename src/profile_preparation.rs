//! Installation preparation and observed native transport evidence.
//! Neither version output nor caller-provided JSON grants launch authority.
#[cfg(target_os = "linux")]
mod native;
#[cfg(target_os = "linux")]
mod revalidation;
#[cfg(target_os = "linux")]
pub use revalidation::{RevalidatedProfile, revalidate};
#[cfg(target_os = "linux")]
pub use native::{
    InteractionEvidence, NativeEvidence, NativePreparation, verify_interaction, verify_native,
};

use crate::{
    domain::*,
    runner::{Cancellation, Cmd},
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    time::{Duration, Instant},
};

#[derive(Serialize)]
pub struct ProfilePreparation {
    pub profile: FrozenProfile,
    pub reference: VersionedReference,
    pub launchable: bool,
    pub protocol_capable: bool,
    pub certified: bool,
}

fn check(deadline: Instant, cancellation: &Cancellation) -> Result<()> {
    ensure!(
        !cancellation.is_cancelled() && Instant::now() < deadline,
        "profile preparation cancelled or expired"
    );
    Ok(())
}
fn digest(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
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
fn executable(
    path: &Path,
    deadline: Instant,
    cancellation: &Cancellation,
) -> Result<(String, String)> {
    check(deadline, cancellation)?;
    ensure!(path.is_absolute(), "profile executable must be absolute");
    let canonical = path.canonicalize()?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&canonical)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.mode() & 0o111 != 0 && before.len() <= 512 * 1024 * 1024,
        "invalid profile executable"
    );
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 65536];
    let mut count = 0u64;
    loop {
        check(deadline, cancellation)?;
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        ensure!(
            count <= 512 * 1024 * 1024,
            "profile executable exceeds bounds"
        );
        hash.update(&bytes[..n]);
    }
    ensure!(
        count == before.len()
            && stamp(&before) == stamp(&file.metadata()?)
            && stamp(&before) == stamp(&std::fs::symlink_metadata(&canonical)?)
            && path.canonicalize()? == canonical,
        "profile executable changed during preparation"
    );
    Ok((
        canonical
            .to_str()
            .context("profile executable path is not UTF-8")?
            .into(),
        format!("{:x}", hash.finalize()),
    ))
}

/// Reads the project's pinned owner configuration, observes exact installation
/// identities with a clean environment, and freezes credential-free inputs. The
/// returned profile retains Unknown capabilities until a native workflow probe
/// supplies real evidence. No caller-supplied Runner, policy or evidence is used.
pub fn prepare(
    project: &Path,
    name: &str,
    herdr_path: &Path,
    agent_path: &Path,
    execution_home: &Path,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<ProfilePreparation> {
    check(deadline, &cancellation)?;
    let project = project.canonicalize()?;
    let guard = crate::execution_guard::RootGuard::exclusive(
        project.parent().context("project root missing")?,
    )?;
    prepare_locked(&project, name, herdr_path, agent_path, execution_home, deadline, cancellation, &guard, None)
}

fn prepare_locked(
    project: &Path,
    name: &str,
    herdr_path: &Path,
    agent_path: &Path,
    execution_home: &Path,
    deadline: Instant,
    cancellation: Cancellation,
    guard: &crate::execution_guard::RootGuard,
    expected: Option<&FrozenProfile>,
) -> Result<ProfilePreparation> {
    let deadline = deadline.min(Instant::now() + Duration::from_secs(20));
    check(deadline, &cancellation)?;
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name.as_bytes()[0].is_ascii_alphabetic()
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
        "invalid profile name"
    );
    let (permission_policy, config) = crate::authority::routine_policy(project)?;
    let bytes = crate::migration::read_plan_file(Path::new(&config.path))?;
    ensure!(
        bytes.len() <= 1_048_576 && config.digest.as_deref() == Some(digest(&bytes).as_str()),
        "profile configuration changed"
    );
    let value: toml::Value = toml::from_str(
        std::str::from_utf8(&bytes).context("invalid profile configuration encoding")?,
    )
    .map_err(|_| anyhow::anyhow!("invalid profile configuration (contents withheld)"))?;
    let definition: crate::profile_config::ProfileDefinition = value
        .get("profiles")
        .and_then(|p| p.get(name))
        .context("named profile missing")?
        .clone()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid profile fields (contents withheld)"))?;
    definition.validate()?;
    ensure!(
        definition.permission_policy == "interactive",
        "profile permission policy has no verified mapping"
    );
    // Preparation can retain requested arguments, but they convey no capability
    // evidence. Unsupported environment/model/effort and budget mappings refuse.
    definition.validate_gated_preparation(0)?;
    ensure!(
        execution_home.is_absolute() && execution_home.canonicalize()? == execution_home,
        "execution home must be canonical and absolute"
    );
    let home = std::fs::symlink_metadata(execution_home)?;
    ensure!(
        home.is_dir()
            && home.uid() == unsafe { libc::geteuid() }
            && home.mode() & 0o022 == 0
            && !execution_home.starts_with(&project),
        "execution home must be owner-controlled and outside the project"
    );
    let agent_identity = executable(agent_path, deadline, &cancellation)?;
    let herdr_identity = executable(herdr_path, deadline, &cancellation)?;
    if let Some(expected) = expected {
        ensure!(agent_identity == (expected.agent.path.clone(), expected.agent.digest.clone())
            && herdr_identity == (expected.herdr.path.clone(), expected.herdr.digest.clone()),
            "retained executable changed before version probing");
    }
    let observe = |kind: &str, identity: &(String, String)| -> Result<ExecutableIdentity> {
        ensure!(
            matches!(kind, "herdr" | "claude" | "codex"),
            "profile kind has no verified version adapter"
        );
        check(deadline, &cancellation)?;
        let mut cmd = Cmd::new(&identity.0, Duration::from_secs(5)).arg("--version");
        cmd.env_clear = true;
        cmd.env = vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("LANG".into(), "C".into()),
            ("LC_ALL".into(), "C".into()),
        ];
        cmd.cwd = Some("/".into());
        cmd.capture_limit = 4096;
        ensure!(
            executable(Path::new(&identity.0), deadline, &cancellation)? == *identity,
            "profile executable changed before probing"
        );
        let output =
            crate::supervision::run(cmd, deadline, cancellation.clone(), &guard.inherit()?)?;
        ensure!(
            output.success() && !output.stdout_truncated && !output.stderr_truncated,
            "profile version probe failed (output withheld)"
        );
        let version = crate::profile_config::observed_version(kind, &output.stdout)
            .context("unrecognized profile version output")?;
        ensure!(
            executable(Path::new(&identity.0), deadline, &cancellation)? == *identity,
            "profile executable changed during probing"
        );
        Ok(ExecutableIdentity {
            path: identity.0.clone(),
            digest: identity.1.clone(),
            version,
        })
    };
    let agent = observe(&definition.kind, &agent_identity)?;
    let herdr = observe("herdr", &herdr_identity)?;
    ensure!(
        herdr.version == "0.9.1",
        "native profile adapter requires Herdr 0.9.1"
    );
    ensure!(
        crate::authority::routine_policy(&project)? == (permission_policy.clone(), config.clone()),
        "profile policy or configuration changed during probing"
    );
    ensure!(
        executable(agent_path, deadline, &cancellation)? == agent_identity
            && executable(herdr_path, deadline, &cancellation)? == herdr_identity
            && stamp(&std::fs::symlink_metadata(execution_home)?) == stamp(&home),
        "profile installation or execution home changed during probing"
    );
    let profile = FrozenProfile {
        version: 1,
        name: name.into(),
        kind: definition.kind.clone(),
        definition_digest: digest(&serde_json::to_vec(&definition)?),
        config,
        arguments_digest: digest(&serde_json::to_vec(&definition.extra_args)?),
        environment_names: definition.environment.clone(),
        execution_home: Some(
            execution_home
                .to_str()
                .context("execution home is not UTF-8")?
                .into(),
        ),
        permission_policy,
        agent,
        herdr,
        adapter: VersionedReference {
            id: "canonical-local-herdr-0.9.1".into(),
            revision: 2,
            digest: digest(
                b"canonical-local-herdr-0.9.1;isolated-gate-v1;direct-exec-v1;native-name-v1;workspace.create_command-v1;pane.get",
            ),
        },
        capabilities: ProfileCapabilities {
            launch: CapabilityEvidence::Unknown,
            readiness_observation: CapabilityEvidence::Unknown,
            prompt_submission: CapabilityEvidence::Unknown,
            stop: CapabilityEvidence::Unknown,
            checkpoint_acknowledgment: CapabilityEvidence::Unknown,
            structured_usage: CapabilityEvidence::Unknown,
            resume: CapabilityEvidence::Unknown,
        },
        workflow_certificate: None,
    };
    let reference = profile.reference().map_err(anyhow::Error::msg)?;
    check(deadline, &cancellation)?;
    Ok(ProfilePreparation {
        profile,
        reference,
        launchable: false,
        protocol_capable: false,
        certified: false,
    })
}

#[cfg(all(test,target_os="linux"))]
pub(crate) mod fixture;

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    mod launch_ingress;
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
    struct Fixture {
        _root: tempfile::TempDir,
        project: PathBuf,
        config: PathBuf,
        home: PathBuf,
        herdr: PathBuf,
        agent: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            Self::with_kind("claude")
        }
        fn with_kind(kind: &str) -> Self {
            let root = tempfile::tempdir().unwrap();
            let project = root.path().join("project");
            fs::create_dir(&project).unwrap();
            for child in [".state", "threads", "inbox"] {
                fs::create_dir(project.join(child)).unwrap();
            }
            fs::write(
                project.join("PROJECT.md"),
                "+++\nname='Profile preparation'\n+++\n",
            )
            .unwrap();
            fs::write(project.join("TASKS.md"), "").unwrap();
            fs::write(project.join("MEMORY.md"), "").unwrap();
            fs::write(
                project.join(".state/project.json"),
                r#"{"status":"paused"}"#,
            )
            .unwrap();
            let config = root.path().join("owner.toml");
            fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key='ssh-ed25519 {}'\n[profiles.worker]\nkind='claude'\npermission_policy='interactive'\nextra_args=['PRIVATE_ARG']\n[profiles.worker.budget]\nmax_wall_seconds=60\nunknown_usage='allow_with_warning'\n", "A".repeat(48))).unwrap();
            if kind == "codex" {
                let bytes = fs::read_to_string(&config)
                    .unwrap()
                    .replace("kind='claude'", "kind='codex'")
                    .replace("extra_args=['PRIVATE_ARG']", "extra_args=[]");
                fs::write(&config, bytes).unwrap();
            }
            let plan = crate::migration::inspect_with_config(&project, &config).unwrap();
            crate::migration::apply(&project, &plan, true).unwrap();
            let home = root.path().join("agent-home");
            fs::create_dir(&home).unwrap();
            let herdr = root.path().join("herdr");
            let agent = root.path().join("claude");
            for (path, version) in [
                (&herdr, "herdr 0.9.1"),
                (&agent, "2.1.0-preview.1 (Claude Code)"),
            ] {
                fs::write(path, format!("#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = --version ] && [ -z \"$HOME\" ] && [ \"$PATH\" = /usr/bin:/bin ] || exit 3\nprintf '%s\\n' '{version}'\n")).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self {
                _root: root,
                project,
                config,
                home,
                herdr,
                agent,
            }
        }
        fn prepare(&self) -> Result<ProfilePreparation> {
            prepare(
                &self.project,
                "worker",
                &self.herdr,
                &self.agent,
                &self.home,
                Instant::now() + Duration::from_secs(10),
                Default::default(),
            )
        }
    }
    #[test]
    fn production_preparation_binds_inputs_without_fabricating_capabilities_or_authority() {
        let f = Fixture::new();
        let before = crate::runtime::snapshot(&f.project).unwrap();
        let prepared = f.prepare().unwrap();
        assert_eq!(prepared.reference, prepared.profile.reference().unwrap());
        assert_eq!(prepared.profile.agent.version, "2.1.0-preview.1");
        assert_eq!(
            prepared.profile.permission_policy,
            crate::authority::policy_reference(&f.project).unwrap()
        );
        assert_eq!(prepared.profile.execution_home.as_deref(), f.home.to_str());
        assert!(!prepared.launchable && !prepared.protocol_capable && !prepared.certified);
        assert!(prepared.profile.validate_for_launch().is_err());
        assert!(
            !serde_json::to_string(&prepared)
                .unwrap()
                .contains("PRIVATE_ARG")
        );
        assert_eq!(crate::runtime::snapshot(&f.project).unwrap(), before);
        assert_eq!(f.prepare().unwrap().reference, prepared.reference);
    }
    #[test]
    fn changed_policy_binary_home_or_unmapped_configuration_discards_preparation() {
        for case in [
            "config-during-probe",
            "binary-during-probe",
            "home-mode",
            "home-in-project",
            "policy",
            "model",
            "version",
            "kind",
        ] {
            let mut f = Fixture::new();
            match case {
                "config-during-probe" => fs::write(&f.agent,format!("#!/bin/sh\nprintf '\\n# changed' >> '{}'\nprintf '2.1.0 (Claude Code)\\n'\n",f.config.display())).unwrap(),
                "binary-during-probe" => fs::write(&f.agent,format!("#!/bin/sh\nprintf 'changed' >> '{}'\nprintf '2.1.0 (Claude Code)\\n'\n",f.herdr.display())).unwrap(),
                "home-mode" => fs::set_permissions(&f.home,fs::Permissions::from_mode(0o777)).unwrap(),
                "home-in-project" => f.home=f.project.clone(),
                "policy" => fs::write(&f.config,fs::read_to_string(&f.config).unwrap().replace("permission_policy='interactive'","permission_policy='unmapped'")).unwrap(),
                "model" => fs::write(&f.config,fs::read_to_string(&f.config).unwrap().replace("kind='claude'","kind='claude'\nmodel='PRIVATE_MODEL'")).unwrap(),
                "version" => fs::write(&f.herdr,"#!/bin/sh\nprintf 'herdr 0.9.2\\n'\n").unwrap(),
                "kind" => fs::write(&f.config,fs::read_to_string(&f.config).unwrap().replace("kind='claude'","kind='unmapped'")).unwrap(),
                _ => unreachable!(),
            }
            let before = crate::runtime::snapshot(&f.project).unwrap();
            let error = f
                .prepare()
                .err()
                .unwrap_or_else(|| panic!("accepted {case}"));
            assert!(!format!("{error:#}").contains("PRIVATE_"), "{case}");
            assert_eq!(
                crate::runtime::snapshot(&f.project).unwrap(),
                before,
                "{case}"
            );
        }
    }
    #[test]
    fn cancelled_preparation_does_not_probe() {
        let f = Fixture::new();
        let marker = f._root.path().join("probed");
        fs::write(
            &f.agent,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        let cancellation = Cancellation::default();
        cancellation.cancel();
        assert!(
            prepare(
                &f.project,
                "worker",
                &f.herdr,
                &f.agent,
                &f.home,
                Instant::now() + Duration::from_secs(10),
                cancellation
            )
            .is_err()
        );
        assert!(!marker.exists());
    }

    #[cfg(target_os = "linux")]
    fn retention_fixture(f: &Fixture) -> NativePreparation {
        // Storage fixture only: no production path can construct this sealed type.
        let mut preparation = f.prepare().unwrap();
        let evidence = NativeEvidence {
            version: 2,
            prepared_profile: preparation.reference.clone(),
            supervisor: crate::worker_supervision::SupervisorIdentity {
                version: 1, boot_id: "00000000-0000-0000-0000-000000000001".into(), host_id: None,
                observer_namespace: (1, 2), worker_namespace: (1, 3),
                outer: crate::worker_supervision::ProcessIncarnation { pid: 20, device: 1, inode: 4 },
                init: crate::worker_supervision::ProcessIncarnation { pid: 21, device: 1, inode: 5 },
            },
            native_kind: preparation.profile.kind.clone(),
            observed_unix_ms: 1000, stopped_unix_ms: 1001, interaction: None,
        };
        let hash = digest(&serde_json::to_vec(&evidence).unwrap());
        let reference = VersionedReference { id: format!("native-transport-{hash}"), revision: 1, digest: hash };
        preparation.profile.capabilities.launch = CapabilityEvidence::Supported { evidence: reference.clone() };
        preparation.profile.capabilities.stop = CapabilityEvidence::Supported { evidence: reference };
        preparation.reference = preparation.profile.reference().unwrap();
        let path = f.project.join(".state/state.db").canonicalize().unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let source_file = fs::OpenOptions::new().read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW).open(&path).unwrap();
        NativePreparation { preparation, evidence, source: (path, metadata.dev(), metadata.ino()), source_file }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn native_retention_survives_restart_is_idempotent_and_remains_a_report() {
        let f = Fixture::new();
        let verified = retention_fixture(&f);
        let before = crate::runtime::snapshot(&f.project).unwrap();
        let reference = verified.retain(&f.project).unwrap();
        verified.retain(&f.project).unwrap();
        let mut store = crate::store::SqliteStore::open(&f.project.join(".state/state.db")).unwrap();
        let report = store.native_profile_report(&reference).unwrap().unwrap();
        assert_eq!(report, serde_json::to_value(&verified).unwrap());
        assert_eq!(report["preparation"]["launchable"], false);
        assert_eq!(report["preparation"]["certified"], false);
        let after = store.read_snapshot(None).unwrap();
        assert_eq!(after.head, before.head + 1);
        assert_eq!(after.tasks, before.tasks);
        assert_eq!(after.attempts, before.attempts);
        assert_eq!(after.operations, before.operations);
        assert_eq!(after.events.last().unwrap().kind, "profile.native_retained");
        assert!(!serde_json::to_string(&report).unwrap().contains("PRIVATE_ARG"));
        let raw = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
        assert!(raw.execute("DELETE FROM native_profiles", []).is_err());
        assert!(raw.execute("UPDATE native_profiles SET report='{}'", []).is_err());
        drop(raw);
        drop(store);
        let copy = f._root.path().join("copied.db");
        fs::copy(f.project.join(".state/state.db"), &copy).unwrap();
        let mut copied = crate::store::SqliteStore::open(&copy).unwrap();
        assert!(copied.native_profile_report(&reference).is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn native_retention_refuses_foreign_replaced_store_and_changed_policy() {
        let f = Fixture::new();
        let verified = retention_fixture(&f);
        let foreign = Fixture::new();
        let before = crate::runtime::snapshot(&foreign.project).unwrap();
        assert!(verified.retain(&foreign.project).is_err());
        assert_eq!(crate::runtime::snapshot(&foreign.project).unwrap(), before);
        let before = crate::runtime::snapshot(&f.project).unwrap();
        let original_config = fs::read(&f.config).unwrap();
        fs::write(&f.config, [original_config.as_slice(), b"\n# changed\n"].concat()).unwrap();
        assert!(verified.retain(&f.project).is_err());
        fs::write(&f.config, original_config).unwrap();
        assert_eq!(crate::runtime::snapshot(&f.project).unwrap(), before);
        let path = f.project.join(".state/state.db");
        // Rename keeps the original inode allocated, so replacement cannot reuse it.
        fs::rename(&path, path.with_extension("original")).unwrap();
        crate::store::SqliteStore::create(&path).unwrap();
        assert!(verified.retain(&f.project).is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn native_retention_requires_explicit_schema_upgrade() {
        let f = Fixture::new();
        let verified = retention_fixture(&f);
        let path = f.project.join(".state/state.db");
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch("DROP TABLE native_profiles; UPDATE store_meta SET schema_version=24; PRAGMA user_version=24;").unwrap();
        drop(raw);
        assert!(verified.retain(&f.project).is_err());
        let mut store = crate::store::SqliteStore::open(&path).unwrap();
        assert!(store.check_native_profile_retention().is_err());
        assert_eq!(store.read_snapshot(None).unwrap().schema_version, 24);
        store.upgrade_v1().unwrap();
        store.check_native_profile_retention().unwrap();
        drop(store);
        verified.retain(&f.project).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn native_retention_rolls_back_event_on_insert_failure() {
        let f = Fixture::new();
        let verified = retention_fixture(&f);
        let before = crate::runtime::snapshot(&f.project).unwrap();
        let path = f.project.join(".state/state.db");
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch("CREATE TRIGGER fail_native_insert BEFORE INSERT ON native_profiles BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
        assert!(verified.retain(&f.project).is_err());
        assert_eq!(crate::runtime::snapshot(&f.project).unwrap(), before);
        raw.execute_batch("DROP TRIGGER fail_native_insert;").unwrap();
        let reference = verified.retain(&f.project).unwrap();
        raw.execute_batch("DROP TRIGGER native_profiles_no_update; UPDATE native_profiles SET report_digest=printf('%064d',0);").unwrap();
        let mut store = crate::store::SqliteStore::open(&path).unwrap();
        assert!(store.native_profile_report(&reference).is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn native_retention_pin_close_preserves_database_lock() {
        let f = Fixture::new();
        let verified = retention_fixture(&f);
        let path = f.project.join(".state/state.db");
        let raw = rusqlite::Connection::open(&path).unwrap();
        // Rollback-journal mode places the write lock on the database inode
        // itself, so an unrelated I/O descriptor close would release it.
        raw.execute_batch("PRAGMA journal_mode=DELETE; BEGIN IMMEDIATE;").unwrap();
        drop(verified);
        // A normal I/O descriptor close can release this process's POSIX locks.
        // Check exclusion from another process, not SQLite's in-process bookkeeping.
        let output = std::process::Command::new("/usr/bin/python3").args(["-c", r#"
import sqlite3, sys
db = sqlite3.connect(sys.argv[1], timeout=0)
try:
    db.execute('BEGIN IMMEDIATE')
except sqlite3.OperationalError as error:
    if 'locked' not in str(error):
        raise
else:
    raise SystemExit('database exclusion was lost')
"#]).arg(&path).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        raw.execute_batch("ROLLBACK;").unwrap();
    }

    #[cfg(target_os = "linux")]
    fn revalidation_fixture(interaction: bool) -> (Fixture, VersionedReference) {
        let f = Fixture::new();
        fs::write(&f.config, fs::read_to_string(&f.config).unwrap().replace("extra_args=['PRIVATE_ARG']", "extra_args=[]")).unwrap();
        let mut verified = retention_fixture(&f);
        if interaction {
            // Synthetic transport evidence tests only the retention/revalidation
            // boundary; this is not a live vendor capability result.
            verified.evidence.interaction = Some(InteractionEvidence {
                session: ResourceIdentity { device: 1, inode: 2, born_secs: 1, born_nanos: 0 },
                terminal: "fixture-terminal".into(), readiness_manifest: "fixture-manifest".into(),
                prompt_digest: "a".repeat(64), acknowledged_unix_ms: 999,
            });
            verified.preparation = f.prepare().unwrap();
            super::native::apply_evidence(&mut verified.preparation, &verified.evidence).unwrap();
        }
        let reference = verified.retain(&f.project).unwrap();
        (f, reference)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn retained_revalidation_preserves_capability_distinctions_and_owns_the_barrier() {
        for launchable in [false, true] {
            let (f, reference) = revalidation_fixture(launchable);
            let before = crate::runtime::snapshot(&f.project).unwrap();
            let proof = revalidate(&f.project, &reference, Instant::now()+Duration::from_secs(10), Default::default()).unwrap();
            assert_eq!(proof.reference(), &reference);
            assert_eq!(proof.validate_for_launch().is_ok(), launchable);
            assert_eq!(proof.launch_profile().is_ok(), launchable);
            let report = serde_json::to_value(&proof).unwrap();
            assert_eq!(report["preparation"]["launchable"], launchable);
            assert_eq!(report["preparation"]["certified"], false);
            assert_eq!(report["preparation"]["protocol_capable"], false);
            assert!(crate::execution_guard::RootGuard::exclusive(f.project.parent().unwrap()).is_err());
            drop(proof);
            assert_eq!(crate::runtime::snapshot(&f.project).unwrap(), before);
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn retained_revalidation_refuses_changed_executable_without_running_it() {
        for target in ["agent", "herdr"] {
            let (f, reference) = revalidation_fixture(true);
            let marker = f._root.path().join("changed-probe-ran");
            fs::write(if target == "agent" { &f.agent } else { &f.herdr },
                format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
            let error = revalidate(&f.project, &reference, Instant::now()+Duration::from_secs(10), Default::default()).err().unwrap();
            assert!(error.to_string().contains("before version probing"), "{error:#}");
            assert!(!marker.exists());
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn retained_revalidation_proof_rechecks_mutable_inputs_and_cancellation() {
        for changed in ["agent", "herdr", "policy", "home-mode", "home-inode", "store", "cancel"] {
            let (f, reference) = revalidation_fixture(true);
            let cancellation = Cancellation::default();
            let proof = revalidate(&f.project, &reference, Instant::now()+Duration::from_secs(10), cancellation.clone()).unwrap();
            proof.validate_for_launch().unwrap();
            match changed {
                "agent" => fs::write(&f.agent, "changed").unwrap(),
                "herdr" => fs::write(&f.herdr, "changed").unwrap(),
                "policy" => fs::write(&f.config, [fs::read(&f.config).unwrap(), b"\n# changed\n".to_vec()].concat()).unwrap(),
                "home-mode" => fs::set_permissions(&f.home, fs::Permissions::from_mode(0o777)).unwrap(),
                "home-inode" => { fs::rename(&f.home, f.home.with_extension("old")).unwrap(); fs::create_dir(&f.home).unwrap(); }
                "store" => { let path=f.project.join(".state/state.db"); fs::rename(&path,path.with_extension("old")).unwrap(); crate::store::SqliteStore::create(&path).unwrap(); }
                "cancel" => cancellation.cancel(),
                _ => unreachable!(),
            }
            assert!(proof.validate_for_launch().is_err(), "{changed}");
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn retained_revalidation_rejects_consistently_rehashed_false_claims() {
        for changed in ["certified", "protocol", "launchable", "version", "time", "interaction", "capability", "workflow", "unknown-field"] {
            let (f, reference) = revalidation_fixture(true);
            let path = f.project.join(".state/state.db");
            let mut value = crate::store::SqliteStore::open(&path).unwrap().native_profile_report(&reference).unwrap().unwrap();
            match changed {
                "certified" => value["preparation"]["certified"] = true.into(),
                "protocol" => value["preparation"]["protocol_capable"] = true.into(),
                "launchable" => value["preparation"]["launchable"] = false.into(),
                "version" => value["evidence"]["version"] = 99.into(),
                "time" => value["evidence"]["stopped_unix_ms"] = 1.into(),
                "interaction" => value["evidence"].as_object_mut().unwrap().remove("interaction").map(|_| ()).unwrap(),
                "capability" => value["preparation"]["profile"]["capabilities"]["resume"] = value["preparation"]["profile"]["capabilities"]["launch"].clone(),
                "workflow" => value["preparation"]["profile"]["workflow_certificate"] = serde_json::to_value(&reference).unwrap(),
                "unknown-field" => value["evidence"]["invented"] = true.into(),
                _ => unreachable!(),
            }
            let profile: FrozenProfile = serde_json::from_value(value["preparation"]["profile"].clone()).unwrap();
            let altered = profile.reference().unwrap();
            value["preparation"]["reference"] = serde_json::to_value(&altered).unwrap();
            let report = serde_json::to_string(&value).unwrap();
            let raw = rusqlite::Connection::open(&path).unwrap();
            raw.execute_batch("DROP TRIGGER native_profiles_no_update;").unwrap();
            raw.execute("UPDATE native_profiles SET profile_digest=?1, report=?2, report_digest=?3", rusqlite::params![altered.digest, report, digest(report.as_bytes())]).unwrap();
            assert!(crate::store::SqliteStore::open(&path).unwrap().native_profile_report(&altered).unwrap().is_some());
            assert!(revalidate(&f.project, &altered, Instant::now()+Duration::from_secs(10), Default::default()).is_err(), "{changed}");
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn retained_revalidation_honors_original_deadline() {
        let (f, reference) = revalidation_fixture(true);
        assert!(revalidate(&f.project, &reference, Instant::now(), Default::default()).is_err());
        let proof = revalidate(&f.project, &reference, Instant::now()+Duration::from_secs(8), Default::default()).unwrap();
        proof.validate_for_launch().unwrap();
        std::thread::sleep(Duration::from_secs(8));
        assert!(proof.validate_for_launch().is_err());
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn native_verification_does_not_promote_version_only_helpers() {
        let f = Fixture::new();
        let config = fs::read_to_string(&f.config)
            .unwrap()
            .replace("extra_args=['PRIVATE_ARG']", "extra_args=[]");
        fs::write(&f.config, config).unwrap();
        f.prepare().unwrap();
        let before = crate::runtime::snapshot(&f.project).unwrap();
        assert!(
            verify_native(
                &f.project,
                "worker",
                &f.herdr,
                &f.agent,
                &f.home,
                Instant::now() + Duration::from_secs(10),
                Default::default()
            )
            .is_err()
        );
        assert_eq!(crate::runtime::snapshot(&f.project).unwrap(), before);
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires HP_LIVE_HERDR patched runtime and HP_LIVE_AGENT native Codex; isolated home, no prompt"]
    fn live_production_native_profile_verification() {
        let f = Fixture::with_kind("codex");
        let herdr = PathBuf::from(std::env::var_os("HP_LIVE_HERDR").expect("set patched Herdr"));
        let agent = PathBuf::from(std::env::var_os("HP_LIVE_AGENT").expect("set native Codex"));
        let before = crate::runtime::snapshot(&f.project).unwrap();
        let verified = verify_native(
            &f.project,
            "worker",
            &herdr,
            &agent,
            &f.home,
            Instant::now() + Duration::from_secs(120),
            Default::default(),
        )
        .unwrap();
        let profile = &verified.preparation.profile;
        assert!(matches!(
            profile.capabilities.launch,
            CapabilityEvidence::Supported { .. }
        ));
        assert!(matches!(
            profile.capabilities.stop,
            CapabilityEvidence::Supported { .. }
        ));
        assert_eq!(
            profile.capabilities.readiness_observation,
            CapabilityEvidence::Unknown
        );
        assert_eq!(
            profile.capabilities.prompt_submission,
            CapabilityEvidence::Unknown
        );
        assert!(profile.workflow_certificate.is_none());
        assert!(
            !verified.preparation.launchable
                && !verified.preparation.protocol_capable
                && !verified.preparation.certified
        );
        assert!(profile.validate_for_launch().is_err());
        assert_eq!(verified.preparation.reference, profile.reference().unwrap());
        assert_eq!(crate::runtime::snapshot(&f.project).unwrap(), before);
        assert!(
            crate::worker_supervision::SupervisorObservation::recover_exited(
                &verified.evidence.supervisor
            )
            .unwrap()
        );
        let reference = verified.retain(&f.project).unwrap();
        let mut reopened = crate::store::SqliteStore::open(&f.project.join(".state/state.db")).unwrap();
        assert_eq!(reopened.native_profile_report(&reference).unwrap().unwrap(), serde_json::to_value(&verified).unwrap());
        let head = reopened.read_snapshot(None).unwrap().head;
        let proof = revalidate(&f.project, &reference, Instant::now()+Duration::from_secs(20), Default::default()).unwrap();
        assert_eq!(proof.reference(), &reference);
        assert!(proof.launch_profile().is_err());
        drop(proof);
        assert_eq!(reopened.read_snapshot(None).unwrap().head, head);
        eprintln!(
            "Production native verification observed exact {} {} launch and termination, retained and revalidated its evidence; no prompt or credentials",
            profile.kind, profile.agent.version
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn native_verification_refuses_unmapped_startup_arguments_before_launch() {
        let f = Fixture::new();
        let before = crate::runtime::snapshot(&f.project).unwrap();
        let error = verify_native(
            &f.project,
            "worker",
            &f.herdr,
            &f.agent,
            &f.home,
            Instant::now() + Duration::from_secs(10),
            Default::default(),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("empty extra_args"));
        assert!(!error.to_string().contains("PRIVATE_ARG"));
        assert_eq!(crate::runtime::snapshot(&f.project).unwrap(), before);
    }
    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires patched HP_LIVE_HERDR, HP_LIVE_AGENT and explicit HP_LIVE_AUTH_FILE; submits one fixed prompt"]
    fn live_production_native_interaction_verification() {
        let f=Fixture::with_kind("codex");
        let herdr=PathBuf::from(std::env::var_os("HP_LIVE_HERDR").expect("set patched Herdr"));
        let agent=PathBuf::from(std::env::var_os("HP_LIVE_AGENT").expect("set native Codex"));
        let auth=PathBuf::from(std::env::var_os("HP_LIVE_AUTH_FILE").expect("explicit authentication cache required"));
        let metadata=fs::symlink_metadata(&auth).unwrap();
        assert!(metadata.is_file() && metadata.len()<=256*1024 && metadata.mode()&0o077==0);
        let config=f.home.join(".codex");fs::create_dir(&config).unwrap();
        fs::set_permissions(&config,fs::Permissions::from_mode(0o700)).unwrap();
        fs::copy(&auth,config.join("auth.json")).unwrap();
        fs::set_permissions(config.join("auth.json"),fs::Permissions::from_mode(0o600)).unwrap();
        // Isolated configuration only: no live user settings, hooks or plugins.
        let lab=native::Lab::new().unwrap();
        let workspace=serde_json::to_string(lab.root.join("work").to_str().unwrap()).unwrap();
        fs::write(config.join("config.toml"), format!("approval_policy = 'on-request'\nsandbox_mode = 'read-only'\n[projects.{workspace}]\ntrust_level = 'trusted'\n")).unwrap();
        let before=crate::runtime::snapshot(&f.project).unwrap();
        // Provision only this disposable directory's trust before invoking the
        // same verifier used by the public interaction entry point.
        let verified=native::verify(&f.project,"worker",&herdr,&agent,&f.home,
            Instant::now()+Duration::from_secs(120),Default::default(),true,lab).unwrap();
        let profile=&verified.preparation.profile;
        assert!(verified.preparation.launchable);
        assert!(profile.validate_for_launch().is_ok());
        assert!(!verified.preparation.protocol_capable && !verified.preparation.certified);
        assert!(profile.workflow_certificate.is_none());
        assert!(verified.evidence.interaction.is_some());
        assert_eq!(crate::runtime::snapshot(&f.project).unwrap(),before);
        assert!(crate::worker_supervision::SupervisorObservation::recover_exited(&verified.evidence.supervisor).unwrap());
        let temporary_home_root=f._root.path().to_owned();
        drop(f);
        assert!(!temporary_home_root.exists(),"temporary login copy must be removed");
        eprintln!("Authenticated native readiness, one-use prompt acknowledgment, termination and temporary-home removal passed; workflow certification remains untested");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn prompt_capability_requires_typed_exact_native_acknowledgment() {
        use serde_json::json;
        let route=RuntimeRoute {socket:"/fixture/native.sock".into(),workspace_id:"w1".into(),tab_id:"w1:t1".into(),pane_id:"w1:p1".into(),cwd:"/fixture/work".into(),..Default::default()};
        let valid=json!({"type":"agent_prompted","agent":{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","cwd":"/fixture/work","terminal_id":"term1","agent":"codex"}});
        super::native::validate_prompt_response(&valid,&route,"term1","codex").unwrap();
        for field in ["pane_id","workspace_id","tab_id","cwd","terminal_id","agent"] {
            let mut foreign=valid.clone();foreign["agent"][field]=json!("foreign");
            assert!(super::native::validate_prompt_response(&foreign,&route,"term1","codex").is_err(),"{field}");
        }
        for invalid in [json!({"type":"ok"}),json!({"type":"agent_prompted"}),json!({"text":"successfully submitted"})] {
            assert!(super::native::validate_prompt_response(&invalid,&route,"term1","codex").is_err());
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires patched HP_LIVE_HERDR and HP_LIVE_AGENT; empty home, no authentication or prompt"]
    fn live_unauthenticated_interaction_is_not_certified() {
        let f=Fixture::with_kind("codex");
        let herdr=PathBuf::from(std::env::var_os("HP_LIVE_HERDR").expect("set patched Herdr"));
        let agent=PathBuf::from(std::env::var_os("HP_LIVE_AGENT").expect("set native Codex"));
        let before=crate::runtime::snapshot(&f.project).unwrap();
        let error=verify_interaction(&f.project,"worker",&herdr,&agent,&f.home,
            Instant::now()+Duration::from_secs(120),Default::default()).err().expect("onboarding must not certify readiness");
        eprintln!("Unauthenticated readiness refusal: {error:#}");
        assert!(format!("{error:#}").contains("prompt readiness"),"{error:#}");
        assert_eq!(crate::runtime::snapshot(&f.project).unwrap(),before);
        assert!(!f.home.join(".codex/auth.json").exists());
    }

}
