//! Retained evidence plus current installation observations, never JSON authority.
use super::*;
use crate::execution_guard::RootGuard;
use crate::store::controlled::{ControlledStore, ReadControl};
use serde::Deserialize;
use std::{fs, path::PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportPreparation {
    profile: FrozenProfile,
    reference: VersionedReference,
    launchable: bool,
    protocol_capable: bool,
    certified: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    preparation: ReportPreparation,
    evidence: NativeEvidence,
    source_store: (PathBuf, u64, u64),
}

/// Current input proof held under the root maintenance barrier and the original
/// deadline. It neither reserves capacity nor authenticates an owner approval.
/// The serialized report cannot recreate the live proof.
/// ```compile_fail
/// use herdr_projects::profile_preparation::RevalidatedProfile;
/// let _: RevalidatedProfile = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Serialize)]
pub struct RevalidatedProfile {
    preparation: ProfilePreparation,
    #[serde(skip)]
    source: (PathBuf, u64, u64),
    #[serde(skip)]
    source_file: fs::File,
    #[serde(skip)]
    home_file: fs::File,
    #[serde(skip)]
    _guard: RootGuard,
    #[serde(skip)]
    deadline: Instant,
    #[serde(skip)]
    cancellation: Cancellation,
}

impl RevalidatedProfile {
    pub(crate) fn store_path(&self) -> &Path { &self.source.0 }
    pub(crate) fn read_control(&self) -> ReadControl { ReadControl::new(self.deadline, self.cancellation.clone()) }
    pub(crate) fn deadline(&self) -> Instant { self.deadline }
    pub(crate) fn cancellation(&self) -> Cancellation { self.cancellation.clone() }
    pub(crate) fn inherit(&self) -> Result<Vec<crate::runner::InheritedLock>> { self._guard.inherit() }
    pub fn reference(&self) -> &VersionedReference { &self.preparation.reference }

    /// Current frozen inputs for the trusted reservation producer. The live
    /// proof and its barrier must remain held through that producer's commit.
    pub fn launch_profile(&self) -> Result<&FrozenProfile> {
        self.validate_for_launch()?;
        Ok(&self.preparation.profile)
    }

    /// Admission must call this immediately before consuming the proof. A native
    /// launch/stop-only report stays unlaunchable after successful revalidation.
    pub fn validate_for_launch(&self) -> Result<()> {
        self.preparation.profile.validate_for_launch().map_err(anyhow::Error::msg)?;
        self.check_current()
    }

    fn check_current(&self) -> Result<()> {
        check(self.deadline, &self.cancellation)?;
        let pinned = self.source_file.metadata()?;
        let metadata = fs::symlink_metadata(&self.source.0)?;
        ensure!(metadata.is_file()
            && (metadata.dev(), metadata.ino()) == (self.source.1, self.source.2)
            && (pinned.dev(), pinned.ino()) == (self.source.1, self.source.2),
            "revalidated profile store changed");
        let project = self.source.0.parent().and_then(Path::parent).context("project store path missing")?;
        let profile = &self.preparation.profile;
        ensure!(crate::authority::routine_policy(project)?
            == (profile.permission_policy.clone(), profile.config.clone()),
            "revalidated profile policy changed");
        for identity in [&profile.agent, &profile.herdr] {
            ensure!(executable(Path::new(&identity.path), self.deadline, &self.cancellation)?
                == (identity.path.clone(), identity.digest.clone()),
                "revalidated profile executable changed");
        }
        let home = Path::new(profile.execution_home.as_deref().context("execution home missing")?);
        let metadata = fs::symlink_metadata(home)?;
        let original = self.home_file.metadata()?;
        ensure!(home.canonicalize()? == home && metadata.is_dir()
            && (metadata.dev(), metadata.ino()) == (original.dev(), original.ino())
            && metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0
            && !home.starts_with(project), "revalidated execution home changed");
        check(self.deadline, &self.cancellation)
    }
}

fn evidence_shape(report: &Report, now: i64) -> Result<()> {
    let e = &report.evidence;
    ensure!(e.version == 2 && e.native_kind == report.preparation.profile.kind
        && e.observed_unix_ms > 0 && e.observed_unix_ms <= e.stopped_unix_ms
        && e.stopped_unix_ms <= now, "invalid native evidence chronology or kind");
    e.supervisor.validate()?;
    if let Some(i) = &e.interaction {
        let text = |s: &str, max: usize| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control);
        ensure!(i.acknowledged_unix_ms > 0 && i.acknowledged_unix_ms <= e.observed_unix_ms
            && i.session.inode > 0 && i.session.born_secs > 0 && i.session.born_nanos < 1_000_000_000
            && text(&i.terminal, 512) && text(&i.readiness_manifest, 128)
            && i.prompt_digest.len() == 64
            && i.prompt_digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid retained interaction evidence");
    }
    Ok(())
}

fn derivation(report: &Report) -> Result<()> {
    let retained = &report.preparation;
    ensure!(retained.profile.arguments_digest == digest(&serde_json::to_vec(&Vec::<String>::new())?)
        && retained.profile.environment_names.is_empty(), "unverified native argument or environment mapping");
    let mut profile = retained.profile.clone();
    profile.workflow_certificate = None;
    profile.capabilities = ProfileCapabilities {
        launch: CapabilityEvidence::Unknown, readiness_observation: CapabilityEvidence::Unknown,
        prompt_submission: CapabilityEvidence::Unknown, stop: CapabilityEvidence::Unknown,
        checkpoint_acknowledgment: CapabilityEvidence::Unknown, structured_usage: CapabilityEvidence::Unknown,
        resume: CapabilityEvidence::Unknown,
    };
    let reference = profile.reference().map_err(anyhow::Error::msg)?;
    ensure!(reference == report.evidence.prepared_profile, "native baseline reference mismatch");
    let mut expected = ProfilePreparation { profile, reference, launchable: false, protocol_capable: false, certified: false };
    native::apply_evidence(&mut expected, &report.evidence)?;
    ensure!(expected.profile == retained.profile && expected.reference == retained.reference
        && expected.launchable == retained.launchable && !retained.protocol_capable && !retained.certified,
        "retained profile capability derivation mismatch");
    Ok(())
}

/// Reload only a canonical retained record, observe its exact current installed
/// inputs, and derive its capabilities again. No native session or task prompt is
/// started. The root guard remains held until the returned proof is dropped.
pub fn revalidate(
    project: &Path,
    reference: &VersionedReference,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<RevalidatedProfile> {
    let deadline = deadline.min(Instant::now() + Duration::from_secs(20));
    check(deadline, &cancellation)?;
    let project = project.canonicalize()?;
    let guard = RootGuard::exclusive(project.parent().context("project root missing")?)?;
    let path = project.join(".state/state.db");
    ensure!(path.canonicalize()? == path, "project store must be canonical");
    let source_file = fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW).open(&path)?;
    let metadata = source_file.metadata()?;
    ensure!(metadata.is_file(), "project store must be a regular file");
    let source = (path.clone(), metadata.dev(), metadata.ino());
    let mut store = ControlledStore::open(&path, ReadControl::new(deadline, cancellation.clone()).with_row_limit(2 * 1024 * 1024)?)?;
    let value = store.native_profile_report(reference)?.context("retained native profile missing")?;
    drop(store);
    let report: Report = serde_json::from_value(value)
        .map_err(|_| anyhow::anyhow!("invalid native profile report (contents withheld)"))?;
    ensure!(report.source_store == source, "retained profile store changed");
    let retained = &report.preparation;
    ensure!(retained.reference == *reference, "retained profile reference mismatch");
    evidence_shape(&report, crate::canonical_worker::now())?;
    derivation(&report)?;
    ensure!(crate::authority::routine_policy(&project)?
        == (retained.profile.permission_policy.clone(), retained.profile.config.clone()),
        "retained profile policy or configuration changed");
    // Unknown capabilities are reconstructed from current observations. No bool
    // or capability in the stored report is accepted as the resulting proof.
    let home = Path::new(retained.profile.execution_home.as_deref().context("execution home missing")?);
    let home_file = fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW).open(home)?;
    ensure!(home_file.metadata()?.is_dir(), "execution home must be a real directory");
    let mut current = prepare_locked(&project, &retained.profile.name,
        Path::new(&retained.profile.herdr.path), Path::new(&retained.profile.agent.path),
        home,
        deadline, cancellation.clone(), &guard, Some(&retained.profile))?;
    ensure!(current.reference == report.evidence.prepared_profile,
        "retained profile installation inputs changed");
    native::apply_evidence(&mut current, &report.evidence)?;
    ensure!(current.profile == retained.profile && current.reference == retained.reference
        && current.launchable == retained.launchable
        && current.protocol_capable == retained.protocol_capable && current.certified == retained.certified,
        "retained profile capability derivation mismatch");
    let proof = RevalidatedProfile { preparation: current, source, source_file, home_file, _guard: guard, deadline, cancellation };
    proof.check_current()?;
    Ok(proof)
}
