//! Test-only native capability evidence; never vendor certification.
use super::*;
use std::os::unix::fs::PermissionsExt;

/// Exercise real installation preparation and retained-report validation while
/// supplying the native interaction evidence at an explicit test boundary.
pub(crate) fn retain(project: &Path, herdr: &Path, agent: &Path, home: &Path) -> FrozenProfile {
    let mut preparation=prepare(project,"fixture",herdr,agent,home,Instant::now()+Duration::from_secs(20),Default::default()).unwrap();
    let evidence=NativeEvidence {
        version:2, prepared_profile:preparation.reference.clone(),
        supervisor:crate::worker_supervision::SupervisorIdentity {
            version:1,boot_id:"00000000-0000-0000-0000-000000000001".into(),host_id:None,
            observer_namespace:(1,2),worker_namespace:(1,3),
            outer:crate::worker_supervision::ProcessIncarnation{pid:20,device:1,inode:4},
            init:crate::worker_supervision::ProcessIncarnation{pid:21,device:1,inode:5},
        },
        native_kind:preparation.profile.kind.clone(),observed_unix_ms:1000,stopped_unix_ms:1001,
        interaction:Some(InteractionEvidence {
            session:ResourceIdentity{device:1,inode:2,born_secs:1,born_nanos:0},
            terminal:"fixture-terminal".into(),readiness_manifest:"fixture-manifest".into(),
            prompt_digest:"a".repeat(64),acknowledged_unix_ms:999,
        }),
    };
    native::apply_evidence(&mut preparation,&evidence).unwrap();
    let source=project.join(".state/state.db").canonicalize().unwrap();
    let metadata=std::fs::metadata(&source).unwrap();
    let source_file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_NOFOLLOW).open(&source).unwrap();
    let verified=NativePreparation{preparation,evidence,source:(source,metadata.dev(),metadata.ino()),source_file};
    verified.retain(project).unwrap();
    verified.preparation.profile
}

/// Explicitly opted-in live acceptance setup. Unlike `retain`, every capability
/// here comes from the native verifier. Credentials remain in the temporary home.
pub(crate) fn retain_live(project:&Path,herdr:&Path,agent:&Path,home:&Path) -> FrozenProfile {
    let auth=std::path::PathBuf::from(std::env::var_os("HP_LIVE_AUTH_FILE").expect("explicit live authentication file required"));
    let metadata=std::fs::symlink_metadata(&auth).unwrap();
    assert!(metadata.is_file() && metadata.len()<=256*1024 && metadata.mode()&0o077==0);
    let config=home.join(".codex");std::fs::create_dir(&config).unwrap();
    std::fs::set_permissions(&config,std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::copy(&auth,config.join("auth.json")).unwrap();
    std::fs::set_permissions(config.join("auth.json"),std::fs::Permissions::from_mode(0o600)).unwrap();
    let lab=native::Lab::new().unwrap();
    let quoted=|p:&Path|serde_json::to_string(p.to_str().unwrap()).unwrap();
    // Only the disposable project is writable. Probe and worker each get an
    // exact trust entry; no user configuration, hooks or plugins are copied.
    std::fs::write(config.join("config.toml"),format!(
        "approval_policy = 'never'\nsandbox_mode = 'workspace-write'\n[sandbox_workspace_write]\nnetwork_access = false\nwritable_roots = [{}]\n[projects.{}]\ntrust_level = 'trusted'\n[projects.{}]\ntrust_level = 'trusted'\n",
        quoted(project),quoted(project),quoted(&lab.root.join("work")))).unwrap();
    let verified=native::verify(project,"fixture",herdr,agent,home,
        Instant::now()+Duration::from_secs(120),Default::default(),true,lab).unwrap();
    assert!(verified.preparation.launchable && !verified.preparation.certified);
    assert!(verified.evidence.interaction.is_some());
    verified.retain(project).unwrap();
    verified.preparation.profile
}
