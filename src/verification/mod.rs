//! Independent verifier. The parent execs `unshare`; only the child switches root.
//! ```compile_fail
//! use herdr_projects::verification::VerificationReceipt;
//! let _: VerificationReceipt = serde_json::from_str("{}").unwrap();
//! ```
mod checkout;
mod manifest;
mod setup;
pub(crate) mod supervise;

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
    version: u32,
    checks: Vec<String>,
}

pub(crate) fn parse_checks(bytes: &[u8]) -> Result<Vec<String>> {
    if bytes.is_empty() || bytes.len() > 4_000 {
        bail!("policy document exceeds bounds");
    }
    let document: PolicyDocument =
        serde_json::from_slice(bytes).context("policy document is invalid")?;
    if document.version != 1 || document.checks.is_empty() || document.checks.len() > 32 {
        bail!("policy checks exceed bounds");
    }
    if document
        .checks
        .iter()
        .any(|arg| arg.is_empty() || arg.len() > 4_096 || arg.contains('\0'))
    {
        bail!("policy check argument is invalid");
    }
    if !Path::new(&document.checks[0]).is_absolute() {
        bail!("policy check program is not absolute");
    }
    Ok(document.checks)
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
pub fn verify_project(project: &Path, request: &VerifyRequest) -> Result<VerifyOutcome> {
    use std::os::unix::fs::DirBuilderExt;
    if request.timeout < Duration::from_secs(1) || request.timeout > Duration::from_secs(300) {
        bail!("verification timeout must be between 1 and 300 seconds");
    }
    if !request.work_dir.is_absolute() { bail!("verification work directory must be absolute"); }
    let _guard = crate::migration::runtime_mutation(project)?;
    let mut store = crate::migration::open_active(project)?;
    fs::DirBuilder::new().mode(0o700).create(&request.work_dir)
        .context("verification work directory must be new and have an existing parent")?;
    struct Work(Option<PathBuf>);
    impl Drop for Work { fn drop(&mut self) { if let Some(path) = &self.0 { let _ = fs::remove_dir_all(path); } } }
    let mut work = Work(Some(request.work_dir.clone()));
    let outcome = verify(&mut store, request);
    let cleanup = fs::remove_dir_all(&request.work_dir);
    if cleanup.is_ok() { work.0 = None; }
    let outcome = outcome?;
    cleanup.context("verification recorded but scratch cleanup failed")?;
    Ok(outcome)
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
    let target = store.load_verify_target(&request.submission_id, &request.policy_id)?;
    let policy_bytes = read_policy(&request.policy_path)?;
    let payload_digest = sha256(&format!(
        "{}\0{}\0{}\0{}",
        target.submission_id,
        target.policy_id,
        sha256(&String::from_utf8_lossy(&policy_bytes)),
        target.candidate_oid
    ));
    if let Some(existing) =
        store.lookup_verification(&target.project_store, &request.idempotency_key)?
    {
        if existing.payload_digest != payload_digest {
            bail!("verification idempotency conflict");
        }
        return Ok(VerifyOutcome {
            run_id: existing.run_id,
            state: existing.state,
            reason: existing.reason,
            receipt: None,
            argv: existing.argv,
            stdout: String::new(),
            replayed: true,
        });
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
        );
    }
    let checks = parse_checks(&policy_bytes)?;
    let checkout = checkout::materialize(
        &request.work_dir,
        Path::new(&target.project_store),
        &target.objects,
        &target.candidate_oid,
        &target.object_format,
    )?;
    if let Some(scopes) = &target.write_scopes {
        let reason = match checkout::changed_paths(&checkout.path, &target.base_oid, &target.candidate_oid) {
            Ok(paths) if paths.iter().any(|path| !scopes.iter().any(|scope|
                path == scope.as_bytes() || (scope.ends_with('/') && path.starts_with(scope.as_bytes())))) => Some("scope_violation"),
            Ok(_) => None,
            Err(_) => Some("scope_diff_unavailable"),
        };
        if let Some(reason) = reason {
            return persist(store, &target, request, payload_digest, Vec::new(), Vec::new(),
                Some(checkout.tree), Some(reason), None, String::new(), None);
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
            Some(checkout.tree), Some("required_output_missing"), None, String::new(), None);
    }
    if !program_allowed(&checks[0], &checkout.path) {
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
            .status()
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
            );
        }
    };
    let launch = supervise::launch(&supervise::Spec {
        unshare_program: request.unshare_program.clone(),
        timeout: request.timeout,
        checkout: checkout.path.clone(),
        policy: request.policy_path.clone(),
        scratch: request.work_dir.join("ns-root"),
        checks: checks.clone(),
        commit: checkout.commit.clone(),
        tree: checkout.tree.clone(),
        policy_digest: target.policy_digest.clone(),
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
        );
    }
    let output = match RealRunner.run(&launch.cmd) {
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
    )
}

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
) -> Result<VerifyOutcome> {
    let libraries = libraries
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    let (stored, receipt) = store.commit_verification(
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
    if output.timed_out {
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
