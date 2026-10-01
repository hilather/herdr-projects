//! Independent verifier. The parent execs `unshare`; only the child switches root.
//! ```compile_fail
//! use herdr_projects::verification::VerificationReceipt;
//! let _: VerificationReceipt = serde_json::from_str("{}").unwrap();
//! ```
mod checkout;
mod evidence;
mod manifest;
mod setup;
mod repetitions;
pub(crate) mod supervise;

#[cfg(test)] use crate::execution_guard::GatedSpawn;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    domain::ObjectFormat,
    runner::{Output, RealRunner, Runner},
    store::{
        SqliteStore,
        verification::{RunDraft, VerifyTarget},
    },
};

pub use setup::setup_main;

pub const ISOLATION: &str = "linux-unshare-user-pid-mount-v1";

/// Native verifier receipt. JSON can display it and cannot build one.
#[derive(Debug, Serialize)]
pub struct VerificationReceipt {
    run_id: String,
    result_id: String,
    commit_oid: String,
    tree_oid: String,
    object_format: ObjectFormat,
    policy_digest: String,
    isolation: &'static str,
    exit_status: i32,
    memory_fence: u64,
    #[serde(skip)]
    store_device: i64,
    #[serde(skip)]
    store_inode: i64,
    // Keeps the store inode pinned for the accept transaction.
    #[serde(skip)]
    source_file: fs::File,
}

impl VerificationReceipt {
    pub fn run_id(&self) -> &str {
        &self.run_id
    }
    pub fn result_id(&self) -> &str {
        &self.result_id
    }
    pub fn commit_oid(&self) -> &str {
        &self.commit_oid
    }
    pub fn tree_oid(&self) -> &str {
        &self.tree_oid
    }
    pub fn policy_digest(&self) -> &str {
        &self.policy_digest
    }
    pub fn isolation(&self) -> &'static str {
        self.isolation
    }
    pub fn exit_status(&self) -> i32 {
        self.exit_status
    }
    pub fn memory_fence(&self) -> u64 {
        self.memory_fence
    }
    pub(crate) fn store_device(&self) -> i64 {
        self.store_device
    }
    pub(crate) fn store_inode(&self) -> i64 {
        self.store_inode
    }
    pub(crate) fn digest(&self) -> String {
        sha256(&format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.run_id,
            self.result_id,
            self.commit_oid,
            self.tree_oid,
            self.object_format.as_str(),
            self.policy_digest,
            self.isolation,
            self.exit_status,
            self.memory_fence,
            self.store_device,
            self.store_inode
        ))
    }
    pub(crate) fn binding_ok(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        self.source_file.metadata().ok().is_some_and(|meta| {
            i64::try_from(meta.dev()).ok() == Some(self.store_device)
                && i64::try_from(meta.ino()).ok() == Some(self.store_inode)
        })
    }
}

pub(crate) fn run_identity(project: &str, key: &str, payload: &str) -> String {
    sha256(&format!("{project}\0{key}\0{payload}"))
}
pub(crate) fn result_identity(run_id: &str, tree: &str, policy_digest: &str) -> String {
    sha256(&format!("{run_id}\0{tree}\0{policy_digest}"))
}
fn sha256(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    None,
    DirtyTree,
    DropPolicy,
    #[cfg(test)]
    StageChange,
    #[cfg(test)]
    Untracked,
    #[cfg(test)]
    BumpFence,
}

pub struct VerifyRequest {
    pub submission_id: String,
    pub policy_id: String,
    pub policy_path: PathBuf,
    pub idempotency_key: String,
    pub timeout: Duration,
    pub work_dir: PathBuf,
    pub(crate) unshare_program: PathBuf,
    pub(crate) fault: Fault,
    pub(crate) cancellation: Option<crate::runner::Cancellation>,
}

impl VerifyRequest {
    pub fn new(
        submission_id: impl Into<String>,
        policy_id: impl Into<String>,
        policy_path: impl Into<PathBuf>,
        idempotency_key: impl Into<String>,
        timeout: Duration,
        work_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            submission_id: submission_id.into(),
            policy_id: policy_id.into(),
            policy_path: policy_path.into(),
            idempotency_key: idempotency_key.into(),
            timeout,
            work_dir: work_dir.into(),
            unshare_program: PathBuf::from("/usr/bin/unshare"),
            fault: Fault::None,
            cancellation: None,
        }
    }
}

#[derive(Serialize)]
pub struct VerifyOutcome {
    pub run_id: String,
    pub state: String,
    pub reason: Option<String>,
    pub receipt: Option<VerificationReceipt>,
    pub argv: Vec<String>,
    pub stdout: String,
    pub replayed: bool,
}

#[derive(serde::Deserialize)]
struct PolicyDocument {
    /// Hidden check inputs (replay suite, TM4.6): owner-held files the
    /// candidate never sees, bound read-only into the isolated root at the
    /// same path and pinned by digest. Absent in ordinary policies.
    #[serde(default)]
    hidden: Vec<HiddenInput>,
}

/// One hidden check input: an absolute path under the project's replay
/// check store (`<projects root>/.replay/<slug>/checks/`) and its sha256.
#[derive(serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct HiddenInput {
    pub(crate) path: String,
    pub(crate) sha256: String,
}

const MAX_HIDDEN: usize = 8;
const MAX_HIDDEN_BYTES: u64 = 65_536;

/// The policy's hidden inputs, bounded: absolute normal paths, hex digests, unique.
pub(crate) fn parse_hidden(bytes: &[u8]) -> Result<Vec<HiddenInput>> {
    let document: PolicyDocument = serde_json::from_slice(bytes).context("policy document is invalid")?;
    if document.hidden.len() > MAX_HIDDEN { bail!("policy hidden inputs exceed bounds"); }
    let mut seen = std::collections::BTreeSet::new();
    for input in &document.hidden {
        let path = Path::new(&input.path);
        if input.path.len() > 1024 || input.path.contains('\0') || !path.is_absolute()
            || path.components().any(|c| !matches!(c, std::path::Component::RootDir | std::path::Component::Normal(_)))
            || input.sha256.len() != 64 || !input.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !seen.insert(input.path.clone()) {
            bail!("policy hidden input is invalid");
        }
    }
    Ok(document.hidden)
}

/// sha256 of a regular, non-symlink hidden file of at most 64 KiB.
pub(crate) fn hidden_digest(path: &Path) -> Option<String> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_HIDDEN_BYTES { return None; }
    let mut bytes = Vec::new();
    file.take(MAX_HIDDEN_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_HIDDEN_BYTES { return None; }
    Some(format!("{:x}", Sha256::digest(&bytes)))
}

/// Every hidden input lies in the project's own replay check store, is
/// canonical and still has its pinned digest.
fn hidden_inputs_ok(project_store: &Path, hidden: &[HiddenInput]) -> bool {
    let Some(project) = project_store.parent().and_then(Path::parent) else { return false };
    let (Some(root), Some(slug)) = (project.parent(), project.file_name()) else { return false };
    let Ok(store) = root.join(".replay").join(slug).join("checks").canonicalize() else { return false };
    hidden.iter().all(|input| {
        let path = Path::new(&input.path);
        path.canonicalize().is_ok_and(|c| c == path && c.starts_with(&store)) && hidden_digest(path).as_deref() == Some(input.sha256.as_str())
    })
}

pub(crate) fn parse_checks(bytes: &[u8]) -> Result<Vec<String>> {
    Ok(crate::domain::verification_policy::ExecutionPolicy::parse(bytes)?.checks)
}

pub(crate) fn program_allowed(program: &str, checkout: &Path) -> bool {
    let path = Path::new(program);
    if program == "/usr/bin/git" {
        return true;
    }
    let Ok(program) = path.canonicalize() else {
        return false;
    };
    let Ok(checkout) = checkout.canonicalize() else {
        return false;
    };
    program.starts_with(checkout)
}

/// Owner/operator CLI ingress. Work is isolated in a newly created directory;
/// existing caller files are never reused or removed by this entry point.
/// Runtime ownership covers the load and the record; the isolated check holds
/// only the shared root and the work directory's fence.
pub fn verify_project(project: &Path, request: &VerifyRequest) -> Result<VerifyOutcome> {
    use std::os::unix::fs::DirBuilderExt;
    if request.timeout < Duration::from_secs(1) || request.timeout > Duration::from_secs(300) {
        bail!("verification timeout must be between 1 and 300 seconds");
    }
    if !request.work_dir.is_absolute() { bail!("verification work directory must be absolute"); }
    let scratch = crate::execution_guard::Resource::new("scratch", request.work_dir.display().to_string())?;
    let guard = crate::migration::runtime_mutation(project)?;
    let mut store = crate::migration::open_active(project)?;
    fs::DirBuilder::new().mode(0o700).create(&request.work_dir)
        .context("verification work directory must be new and have an existing parent")?;
    struct Work(Option<PathBuf>);
    impl Drop for Work { fn drop(&mut self) { if let Some(path) = &self.0 { let _ = fs::remove_dir_all(path); } } }
    let mut work = Work(Some(request.work_dir.clone()));
    // The work directory is this call's own, so it is removed with or without ownership.
    let mut ownership = OperatorOwnership::new(guard, project, scratch, request.timeout);
    let outcome = verify_owned(&mut store, request, Some(&mut ownership));
    let cleanup = fs::remove_dir_all(&request.work_dir);
    if cleanup.is_ok() { work.0 = None; }
    let outcome = outcome?;
    cleanup.context("verification recorded but scratch cleanup failed")?;
    Ok(outcome)
}

enum OperatorSlot { Project(crate::migration::Maintenance), Check(crate::execution_guard::CheckGuard), Lost }
/// Operator runtime ownership, handed to the verifier or integrator around its isolated check.
pub(crate) struct OperatorOwnership { slot: OperatorSlot, project: PathBuf, scratch: crate::execution_guard::Resource, wait: Duration }
impl OperatorOwnership {
    /// `scratch` fences the check's work directory; `wait` bounds regaining ownership.
    pub(crate) fn new(guard: crate::migration::Maintenance, project: &Path, scratch: crate::execution_guard::Resource, wait: Duration) -> Self {
        Self { slot: OperatorSlot::Project(guard), project: project.to_path_buf(), scratch, wait }
    }
}
impl CheckOwnership for OperatorOwnership {
    fn release(&mut self) -> Result<()> {
        let OperatorSlot::Project(guard) = std::mem::replace(&mut self.slot, OperatorSlot::Lost) else { bail!("the check does not hold project ownership") };
        match guard.fence(&self.scratch) {
            Ok(fence) => { self.slot = OperatorSlot::Check(guard.narrow(fence)?); Ok(()) }
            Err(error) => { self.slot = OperatorSlot::Project(guard); Err(error) }
        }
    }
    fn reacquire(&mut self, _: &mut SqliteStore) -> Result<()> {
        let OperatorSlot::Check(check) = std::mem::replace(&mut self.slot, OperatorSlot::Lost) else { bail!("the check is not narrowed") };
        self.slot = OperatorSlot::Project(crate::migration::Maintenance::widen(check, &self.project, self.wait)?);
        Ok(())
    }
    fn held(&self) -> bool { matches!(self.slot, OperatorSlot::Project(_)) }
}

/// Automatic verifier timeout. The job lease (300 s) must cover the checkout,
/// the check and the record.
pub const AUTO_TIMEOUT: Duration = Duration::from_secs(240);

/// One automatic verification job. The policy is the stored signed body, never
/// an operator file, and the key is the job's operation id.
pub struct StoredJob<'a> {
    pub submission_id: &'a str,
    pub policy_id: &'a str,
    pub policy_digest: &'a str,
    pub key: &'a str,
    /// Deterministic per job; any leftover from an earlier attempt is removed first.
    pub scratch: &'a Path,
    pub cancellation: crate::runner::Cancellation,
}

/// Project ownership for an automatic check. The job loads and records under
/// it, and gives it up only while the isolated check runs in its own scratch.
pub trait CheckOwnership {
    /// Narrow ownership to the check's scratch fence and the shared root.
    fn release(&mut self) -> Result<()>;
    /// Take project ownership again and recheck the job's own fences; a
    /// [`FenceChanged`] error means nothing may be recorded.
    fn reacquire(&mut self, store: &mut SqliteStore) -> Result<()>;
    /// Whether project ownership is held now.
    fn held(&self) -> bool;
}

/// The job's inputs changed while the check ran; no verdict was recorded.
#[derive(Debug)]
pub struct FenceChanged(pub String);
impl std::fmt::Display for FenceChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "verification inputs changed during the check: {}", self.0)
    }
}
impl std::error::Error for FenceChanged {}

/// Controller ingress. The caller holds project ownership and a live claim;
/// `ownership` is released only around the isolated check. The scratch
/// directory is removed on every return path it can reach under ownership.
pub fn verify_stored(store: &mut SqliteStore, job: &StoredJob<'_>, ownership: &mut dyn CheckOwnership) -> Result<VerifyOutcome> {
    use std::os::unix::fs::DirBuilderExt;
    clear_scratch(job.scratch)?;
    let target = store.load_verify_target(job.submission_id, job.policy_id)?;
    if sha256(&target.policy_body) != job.policy_digest {
        bail!("stored acceptance policy changed since the job was enqueued");
    }
    let policy = target.policy_body.clone();
    drop(target);
    let parent = job.scratch.parent().context("verification scratch has no parent")?;
    if let Err(error) = fs::DirBuilder::new().mode(0o700).create(parent) {
        if error.kind() != std::io::ErrorKind::AlreadyExists { return Err(error.into()); }
    }
    if !fs::symlink_metadata(parent)?.is_dir() { bail!("verification scratch parent is not a directory"); }
    fs::DirBuilder::new().mode(0o700).create(job.scratch).context("verification scratch directory")?;
    let outcome = (|| {
        let policy_path = job.scratch.join("policy.json");
        fs::write(&policy_path, policy.as_bytes())?;
        let mut request = VerifyRequest::new(job.submission_id, job.policy_id, policy_path, job.key, AUTO_TIMEOUT, job.scratch.join("work"));
        request.cancellation = Some(job.cancellation.clone());
        fs::DirBuilder::new().mode(0o700).create(&request.work_dir)?;
        verify_owned(store, &request, Some(&mut *ownership))
    })();
    // Without ownership, recovery (observation by key) removes the scratch.
    if !ownership.held() { return outcome; }
    let cleanup = clear_scratch(job.scratch);
    let outcome = outcome?;
    cleanup.context("verification recorded but scratch cleanup failed")?;
    Ok(outcome)
}

/// Remove a job's scratch directory, refusing to follow a symlink.
pub fn clear_scratch(scratch: &Path) -> Result<()> {
    match fs::symlink_metadata(scratch) {
        Ok(meta) if meta.is_dir() => Ok(fs::remove_dir_all(scratch)?),
        Ok(_) => bail!("verification scratch is not a directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// The run recorded under `key`, if any: (run id, state). Only store rows count.
pub fn recorded_run(store: &mut SqliteStore, project_store: &str, key: &str) -> Result<Option<(String, String)>> {
    Ok(store.lookup_verification(project_store, key)?.map(|run| (run.run_id, run.state)))
}

/// Automatic runs never fall back to an unsandboxed check. Probe the same
/// namespaces the verifier uses before any claim is taken.
pub fn isolation_available() -> Result<()> {
    let unshare = Path::new("/usr/bin/unshare");
    if !unshare_ready(unshare) {
        bail!("isolation unavailable: /usr/bin/unshare is missing or not root-owned");
    }
    let mut probe = crate::runner::Cmd::new(unshare.display().to_string(), Duration::from_secs(5));
    probe.args = ["--user", "--map-root-user", "--mount", "--propagation", "private", "--pid", "--fork", "--mount-proc", "--kill-child=KILL", "--", "/bin/true"]
        .into_iter().map(String::from).collect();
    probe.env_clear = true;
    let output = RealRunner.run(&probe).context("isolation unavailable")?;
    if !output.success() {
        bail!("isolation unavailable: unshare probe failed: {}", output.stderr.trim().chars().take(512).collect::<String>());
    }
    Ok(())
}

fn read_policy(path: &Path) -> Result<Vec<u8>> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path).context("policy file is unreadable")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 4_000 { bail!("policy file must be a regular file of at most 4000 bytes"); }
    let mut bytes = Vec::new();
    file.take(4_001).read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > 4_000 { bail!("policy document exceeds bounds"); }
    Ok(bytes)
}

pub fn verify(store: &mut SqliteStore, request: &VerifyRequest) -> Result<VerifyOutcome> {
    verify_owned(store, request, None)
}

fn verify_owned(store: &mut SqliteStore, request: &VerifyRequest, ownership: Option<&mut dyn CheckOwnership>) -> Result<VerifyOutcome> {
    let target = store.load_verify_target(&request.submission_id, &request.policy_id)?;
    let policy_bytes = read_policy(&request.policy_path)?;
    let payload_digest = sha256(&format!(
        "{}\0{}\0{}\0{}",
        target.submission_id,
        target.policy_id,
        sha256(&String::from_utf8_lossy(&policy_bytes)),
        target.candidate_oid
    ));
    let replay = |store: &mut SqliteStore, target: &VerifyTarget| -> Result<Option<VerifyOutcome>> {
        let Some(existing) = store.lookup_verification(&target.project_store, &request.idempotency_key)? else { return Ok(None) };
        if existing.payload_digest != payload_digest {
            bail!("verification idempotency conflict");
        }
        Ok(Some(VerifyOutcome {
            run_id: existing.run_id,
            state: existing.state,
            reason: existing.reason,
            receipt: None,
            argv: existing.argv,
            stdout: String::new(),
            replayed: true,
        }))
    };
    if let Some(replayed) = replay(store, &target)? {
        return Ok(replayed);
    }
    if policy_bytes != target.policy_body.as_bytes() {
        return persist(
            store,
            &target,
            request,
            payload_digest,
            Vec::new(),
            Vec::new(),
            None,
            Some("policy_digest_mismatch"),
            None,
            String::new(),
            None,
            None,
        );
    }
    let checks = parse_checks(&policy_bytes)?;
    let hidden = parse_hidden(&policy_bytes)?;
    if !hidden.is_empty() && !hidden_inputs_ok(Path::new(&target.project_store), &hidden) {
        return persist(store, &target, request, payload_digest, Vec::new(), Vec::new(), None, Some("hidden_check_unavailable"), None, String::new(), None, None);
    }
    let checkout = checkout::materialize(
        &request.work_dir,
        Path::new(&target.project_store),
        &target.objects,
        &target.candidate_oid,
        &target.object_format,
    )?;
    if let Some(scopes) = &target.write_scopes {
        let reason = match checkout::changed_paths(&checkout.path, &target.base_oid, &target.candidate_oid) {
            Ok(paths) if paths.iter().any(|path| !crate::domain::in_write_scope(scopes, path)) => Some("scope_violation"),
            Ok(_) => None,
            Err(_) => Some("scope_diff_unavailable"),
        };
        if let Some(reason) = reason {
            return persist(store, &target, request, payload_digest, Vec::new(), Vec::new(),
                Some(checkout.tree), Some(reason), None, String::new(), None, None);
        }
    }
    // Inspect the materialized candidate, never the worker's artifact claims.
    // Disallow symlink ancestors as well as symlink final entries.
    if target.required_outputs.iter().any(|output| {
        let mut path = checkout.path.clone();
        let parts: Vec<_> = output.split('/').collect();
        parts.iter().enumerate().any(|(index, part)| {
            path.push(part);
            match fs::symlink_metadata(&path) {
                Ok(meta) => meta.file_type().is_symlink()
                    || if index + 1 == parts.len() { !meta.is_file() } else { !meta.is_dir() },
                Err(_) => true,
            }
        })
    }) {
        return persist(store, &target, request, payload_digest, Vec::new(), Vec::new(),
            Some(checkout.tree), Some("required_output_missing"), None, String::new(), None, None);
    }
    if !crate::domain::verification_policy::ExecutionPolicy::parse(&policy_bytes)?
        .commands().all(|args| program_allowed(&args[0], &checkout.path)) {
        return persist(
            store,
            &target,
            request,
            payload_digest,
            Vec::new(),
            Vec::new(),
            Some(checkout.tree),
            Some("checks_not_allowlisted"),
            None,
            String::new(),
            None,
            None,
        );
    }
    if request.fault == Fault::DirtyTree {
        fs::write(checkout.path.join("src/file.txt"), b"tampered\n")
            .context("could not dirty the checkout")?;
    }
    #[cfg(test)]
    if request.fault == Fault::StageChange {
        fs::write(checkout.path.join("src/file.txt"), b"staged\n")
            .context("could not stage the checkout")?;
        let status = std::process::Command::new("/usr/bin/git")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "add",
                "--",
                "src/file.txt",
            ])
            .current_dir(&checkout.path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status_gated()
            .context("git add")?;
        if !status.success() {
            bail!("could not stage the checkout change");
        }
    }
    #[cfg(test)]
    if request.fault == Fault::Untracked {
        fs::write(checkout.path.join("untracked.txt"), b"extra\n")
            .context("could not add an untracked file")?;
    }
    if request.fault == Fault::DropPolicy {
        fs::remove_file(&request.policy_path).context("could not remove the policy file")?;
    }
    let libraries = match manifest::git_libraries(Path::new("/usr/bin/git")) {
        Ok(libraries) => libraries,
        Err(_) => {
            return persist(
                store,
                &target,
                request,
                payload_digest,
                Vec::new(),
                Vec::new(),
                Some(checkout.tree),
                Some("isolation_setup_failed"),
                None,
                String::new(),
                None,
                None,
            );
        }
    };
    let mut launch = supervise::launch(&supervise::Spec {
        unshare_program: request.unshare_program.clone(),
        timeout: request.timeout,
        checkout: checkout.path.clone(),
        policy: request.policy_path.clone(),
        scratch: request.work_dir.join("ns-root"),
        checks: checks.clone(),
        commit: checkout.commit.clone(),
        tree: checkout.tree.clone(),
        policy_digest: target.policy_digest.clone(),
        hidden: hidden.iter().map(|input| PathBuf::from(&input.path)).collect(),
    })?;
    if !unshare_ready(&request.unshare_program) {
        return persist(
            store,
            &target,
            request,
            payload_digest,
            launch.argv,
            libraries,
            Some(checkout.tree),
            Some("isolation_setup_failed"),
            None,
            String::new(),
            None,
            None,
        );
    }
    launch.cmd.capture_limit = 2 * 1024 * 1024 + 1;
    launch.cmd.cancellation = request.cancellation.clone();
    let mut execution = evidence::Execution::start(Path::new(&target.project_store));
    launch.cmd.env.push(("HP_VERIFY_DEADLINE_MONOTONIC_MS".into(),
        (repetitions::monotonic_ms().saturating_add(request.timeout.as_millis() as u64)).to_string()));
    // The check touches only its own scratch; record only if the inputs still hold.
    let (ran, target) = match ownership {
        Some(ownership) => {
            ownership.release()?;
            let ran = RealRunner.run(&launch.cmd);
            execution.completed();
            ownership.reacquire(store)?;
            let fresh = store.load_verify_target(&request.submission_id, &request.policy_id)
                .map_err(|error| FenceChanged(format!("{error}")))?;
            if !fresh.same_job(&target) {
                return Err(FenceChanged("task, submission, contract, policy or attempt changed".into()).into());
            }
            // Another caller may have recorded under the same key meanwhile.
            if let Some(replayed) = replay(store, &fresh)? {
                return Ok(replayed);
            }
            (ran, fresh)
        }
        None => {
            let ran = RealRunner.run(&launch.cmd);
            execution.completed();
            (ran, target)
        },
    };
    let output = match ran {
        // A cancelled check is no verdict: record nothing and let the caller retry.
        Ok(output) if output.cancelled => bail!("verification cancelled before a verdict"),
        Ok(output) => output,
        Err(_) => {
            return persist(
                store,
                &target,
                request,
                payload_digest,
                launch.argv,
                libraries,
                Some(checkout.tree),
                Some("isolation_setup_failed"),
                None,
                String::new(),
                None,
                None,
            );
        }
    };
    let report = classify(&output, &checkout.commit, &checkout.tree);
    #[cfg(test)]
    if request.fault == Fault::BumpFence {
        store.testing_append_event()?;
    }
    let receipt = if report.success {
        Some(issue_receipt(
            &target,
            &payload_digest,
            &request.idempotency_key,
            &checkout.commit,
            report.tree.as_deref().unwrap_or_default(),
        )?)
    } else {
        None
    };
    let mut metadata = execution.metadata(&report.stdout, output.stdout_truncated);
    if crate::domain::verification_policy::ExecutionPolicy::parse(&policy_bytes)?.version == 2 {
        metadata["version"] = serde_json::json!("verification-metadata.v2");
        let observations = evidence::repetitions(&output.stderr, output.timed_out || output.code == Some(78));
        // The primary command's tests remain distinct from repetition output.
        if let Some(first) = observations.first() { metadata["tests"] = first["tests"].clone(); }
        metadata["observations"] = serde_json::json!(observations);
    }
    persist(
        store,
        &target,
        request,
        payload_digest,
        launch.argv,
        libraries,
        report.tree.or(Some(checkout.tree)),
        report.reason,
        receipt,
        report.stdout,
        report.exit_status,
        Some(metadata),
    )
}

#[allow(clippy::too_many_arguments)]
fn persist(
    store: &mut SqliteStore,
    target: &VerifyTarget,
    request: &VerifyRequest,
    payload_digest: String,
    argv: Vec<String>,
    libraries: Vec<PathBuf>,
    tree_oid: Option<String>,
    reason: Option<&str>,
    receipt: Option<VerificationReceipt>,
    stdout: String,
    exit_status: Option<i32>,
    metadata: Option<serde_json::Value>,
) -> Result<VerifyOutcome> {
    let libraries = libraries
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    let (stored, receipt) = store.commit_verification_metadata(
        target,
        RunDraft {
            idempotency_key: request.idempotency_key.clone(),
            payload_digest,
            argv,
            libraries,
            tree_oid,
            exit_status,
            reason: reason.map(str::to_string),
            receipt,
        },
        metadata.as_ref(),
    )?;
    Ok(VerifyOutcome {
        run_id: stored.run_id,
        state: stored.state,
        reason: stored.reason,
        receipt,
        argv: stored.argv,
        stdout,
        replayed: false,
    })
}

#[cfg(test)]
pub(crate) fn testing_receipt(target: &VerifyTarget, payload: &str, key: &str, commit: &str, tree: &str) -> VerificationReceipt {
    issue_receipt(target, payload, key, commit, tree).unwrap()
}

fn issue_receipt(
    target: &VerifyTarget,
    payload_digest: &str,
    key: &str,
    commit: &str,
    tree: &str,
) -> Result<VerificationReceipt> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let path = Path::new(&target.project_store);
    let source_file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
        .open(path)
        .context("project store could not be pinned")?;
    let meta = source_file
        .metadata()
        .context("project store could not be pinned")?;
    let format = match target.object_format.as_str() {
        "sha1" => ObjectFormat::Sha1,
        "sha256" => ObjectFormat::Sha256,
        _ => bail!("unsupported object format"),
    };
    let run_id = run_identity(&target.project_store, key, payload_digest);
    let result_id = result_identity(&run_id, tree, &target.policy_digest);
    Ok(VerificationReceipt {
        run_id,
        result_id,
        commit_oid: commit.to_string(),
        tree_oid: tree.to_string(),
        object_format: format,
        policy_digest: target.policy_digest.clone(),
        isolation: ISOLATION,
        exit_status: 0,
        memory_fence: target.memory_fence,
        store_device: i64::try_from(meta.dev()).context("store device does not fit")?,
        store_inode: i64::try_from(meta.ino()).context("store inode does not fit")?,
        source_file,
    })
}

struct ChildReport {
    success: bool,
    reason: Option<&'static str>,
    tree: Option<String>,
    stdout: String,
    exit_status: Option<i32>,
}

fn classify(output: &Output, commit: &str, tree: &str) -> ChildReport {
    let stdout = output.stdout.clone();
    if output.timed_out || output.code == Some(78) {
        return ChildReport {
            success: false,
            reason: Some("timeout"),
            tree: None,
            stdout,
            exit_status: None,
        };
    }
    let protocol = protocol(&output.stderr);
    let seen_tree = protocol.tree.clone();
    let exit_status=protocol.checks.filter(|code|(0..=255).contains(code));
    match output.code {
        Some(73) => ChildReport {
            success: false,
            reason: Some("tampered_tree"),
            tree: seen_tree,
            stdout,
            exit_status,
        },
        Some(74) => ChildReport {
            success: false,
            reason: Some("leftover_child"),
            tree: seen_tree,
            stdout,
            exit_status,
        },
        Some(75) => ChildReport {
            success: false,
            reason: Some("policy_digest_mismatch"),
            tree: seen_tree,
            stdout,
            exit_status,
        },
        Some(76) => ChildReport {
            success: false,
            reason: Some("checks_not_allowlisted"),
            tree: seen_tree,
            stdout,
            exit_status,
        },
        Some(0)
            if protocol.commit.as_deref() == Some(commit)
                && protocol.tree.as_deref() == Some(tree)
                && protocol.checks == Some(0) =>
        {
            ChildReport {
                success: true,
                reason: None,
                tree: seen_tree,
                stdout,
                exit_status,
            }
        }
        Some(0) => ChildReport {
            success: false,
            reason: Some("tampered_tree"),
            tree: seen_tree,
            stdout,
            exit_status,
        },
        Some(code) if ((1..71).contains(&code) || code==77) && protocol.commit.as_deref() == Some(commit) => {
            ChildReport {
                success: false,
                reason: Some("checks_failed"),
                tree: seen_tree,
                stdout,
                exit_status,
            }
        }
        _ => ChildReport {
            success: false,
            reason: Some("isolation_setup_failed"),
            tree: seen_tree,
            stdout,
            exit_status,
        },
    }
}

struct Protocol {
    commit: Option<String>,
    tree: Option<String>,
    checks: Option<i32>,
}

fn protocol(stderr: &str) -> Protocol {
    let mut parsed = Protocol {
        commit: None,
        tree: None,
        checks: None,
    };
    for line in stderr.lines() {
        if let Some(value) = line.strip_prefix("hp-verify commit=") {
            parsed.commit = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("hp-verify tree=") {
            parsed.tree = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("hp-verify checks=") {
            parsed.checks = value.trim().parse().ok();
        }
    }
    parsed
}

pub(crate) fn isolated_check_ok(output: &Output, commit: &str, tree: &str) -> bool {
    classify(output, commit, tree).success
}

pub(crate) fn unshare_ready(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(root) = fs::metadata("/") else {
        return false;
    };
    meta.is_file()
        && meta.uid() == root.uid()
        && meta.mode() & 0o022 == 0
        && meta.mode() & 0o111 != 0
}

#[cfg(test)]
mod tests;
