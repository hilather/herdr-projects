//! Trusted launch draft/reservation ingress. Selectors are data, never authority.
use crate::{domain::*, profile_preparation::RevalidatedProfile, runner::{Cancellation, Cmd}, store::controlled::ControlledStore};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::{Path, PathBuf}, time::{Duration, Instant}};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchSelection {
    pub task: TaskId,
    pub binding: String,
    pub profile: VersionedReference,
    pub knowledge: VersionedReference,
    #[serde(default)]
    pub repositories: Vec<PathBuf>,
}

/// Reviewable inputs and an unsigned document for the existing owner-signature
/// ingress. No task, operation, approval or reservation is created by drafting.
#[derive(Serialize)]
pub struct LaunchDraft {
    pub head: u64,
    pub inputs: LaunchInputs,
    pub approval: ApprovalGrant,
    pub brief: crate::memory::WorkerBrief,
    pub worktrees: Vec<WorktreePlan>,
}

fn now() -> i64 { crate::canonical_worker::now() }
fn pending_approval() -> VersionedReference {
    VersionedReference { id: "unsigned-launch".into(), revision: 1, digest: "0".repeat(64) }
}

fn git(proof: &RevalidatedProfile, path: &Path, args: &[&str]) -> Result<crate::runner::Output> {
    let mut command = Cmd::new("/usr/bin/git", Duration::from_secs(5))
        .args(["--no-pager", "--no-optional-locks", "-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"])
        .args(args.iter().copied());
    command.cwd = Some(path.to_str().context("repository path is not UTF-8")?.into());
    command.env_clear = true;
    command.env = [
        ("PATH", "/usr/bin:/bin"), ("LANG", "C"), ("LC_ALL", "C"),
        ("GIT_CONFIG_NOSYSTEM", "1"), ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_TERMINAL_PROMPT", "0"), ("GIT_NO_LAZY_FETCH", "1"), ("GIT_NO_REPLACE_OBJECTS", "1"),
    ].into_iter().map(|(k,v)|(k.into(),v.into())).collect();
    command.capture_limit = 8192;
    let output = crate::supervision::run(command, proof.deadline(), proof.cancellation(), &proof.inherit()?)?;
    ensure!(!output.stdout_truncated && !output.stderr_truncated, "repository observation exceeded output bounds");
    Ok(output)
}

fn git_text(proof: &RevalidatedProfile, path: &Path, args: &[&str]) -> Result<String> {
    let output = git(proof,path,args)?;
    ensure!(output.success(), "repository observation failed (output withheld)");
    Ok(std::str::from_utf8(&output.stdout_bytes).context("repository observation is not UTF-8")?.trim_end_matches('\n').to_owned())
}

fn repository(proof: &RevalidatedProfile, path: &Path) -> Result<RepositoryInput> {
    ensure!(path.is_absolute() && path.canonicalize()? == path && path.is_dir(), "repository must be a canonical directory");
    let name = path.to_str().context("repository path is not UTF-8")?;
    ensure!(name.len() <= 4096 && !name.chars().any(char::is_control), "invalid repository path");
    // Reject lazy object fetching before resolving any object. No provider or
    // credential helper is needed to prepare a local immutable base revision.
    let config = git_text(proof,path,&["config", "--includes", "--list", "--name-only", "--null"])?;
    ensure!(!config.split('\0').any(|key| {
        let key=key.to_ascii_lowercase();
        key=="extensions.partialclone" || (key.starts_with("remote.") && key.ends_with(".promisor"))
    }), "partial clone repository requires materialized local objects");
    ensure!(git_text(proof,path,&["rev-parse", "--show-toplevel"])? == name, "repository selection is not its worktree root");
    let commit = git_text(proof,path,&["rev-parse", "--verify", "HEAD^{commit}"])?;
    let oid = |s: &str| matches!(s.len(),40|64) && s.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    ensure!(oid(&commit), "invalid repository commit identity");
    let tree = git_text(proof,path,&["rev-parse", "--verify", &format!("{commit}^{{tree}}")])?;
    ensure!(oid(&tree), "invalid repository tree identity");
    ensure!(git_text(proof,path,&["rev-parse", "--verify", "HEAD^{commit}"])? == commit, "repository HEAD changed during preparation");
    Ok(RepositoryInput { repository: name.into(), commit, tree })
}

fn inputs(
    proof: &RevalidatedProfile, selection: &LaunchSelection, state: &Snapshot,
    approval: VersionedReference,
) -> Result<LaunchInputs> {
    let profile = proof.launch_profile()?.clone();
    ensure!(selection.profile == *proof.reference(), "selected profile differs from live proof");
    ensure!(selection.repositories.len() <= 64, "too many repository selections");
    let task = state.tasks.iter().find(|t|t.id==selection.task).context("selected task missing")?;
    let binding = state.runtime_bindings.iter().find(|b|b.id==selection.binding && b.task.as_ref()==Some(&task.id))
        .context("selected task binding missing")?;
    let route = RuntimeRoute::from_identity(&binding.identity);
    route.validate().map_err(anyhow::Error::msg)?;
    ensure!(route.machine.is_empty() && route.tab_id.is_empty() && route.pane_id.is_empty()
        && binding.identity.worktree_path.is_empty(), "new launch requires an unused local binding");
    let cwd = Path::new(&route.cwd);
    ensure!(cwd.is_absolute() && cwd.canonicalize()? == cwd && cwd.is_dir(), "launch working directory is not canonical");
    let mut paths = BTreeSet::new();
    for path in &selection.repositories {
        ensure!(paths.insert(path.clone()), "duplicate repository selection");
    }
    if !binding.identity.repo.is_empty() { paths.insert(PathBuf::from(&binding.identity.repo)); }
    ensure!(paths.len() <= 64, "too many bound repositories");
    let repositories = paths.iter().map(|p|repository(proof,p)).collect::<Result<Vec<_>>>()?;
    Ok(LaunchInputs {
        version: 2, project_store: proof.store_path().to_str().context("project store is not UTF-8")?.into(),
        task: task.id.clone(), task_revision: task.revision,
        scheduler_revision: state.scheduler.as_ref().context("scheduler missing")?.policy.revision,
        control_epoch: state.control.as_ref().context("project control missing")?.epoch,
        binding: binding.id.clone(), binding_revision: binding.revision,
        binding_digest: crate::store::ownership::identity_digest(binding)?,
        profile: proof.reference().clone(), config: profile.config.clone(), effective_profile: Some(profile),
        approval, repositories, dependencies: Vec::new(), memory: Some(selection.knowledge.clone()),
        budget: state.budget_policies.last().map(BudgetPolicy::reference).transpose().map_err(anyhow::Error::msg)?,
    })
}

fn brief(project: &Path, db: &mut ControlledStore, inputs: &LaunchInputs) -> Result<crate::memory::WorkerBrief> {
    let reference = inputs.memory.as_ref().context("worker knowledge is required")?;
    let value = db.render_launch_knowledge(project,&reference.id)?;
    let snapshot: MemorySnapshot = serde_json::from_value(value["snapshot"].clone())?;
    ensure!(snapshot.id.as_str()==reference.id && snapshot.manifest_hash==reference.digest
        && reference.revision==1 && snapshot.estimator==crate::memory::WORKER_BRIEF_ESTIMATOR,
        "launch requires the exact retained worker snapshot");
    let (attempt,_) = crate::store::reservations::record_ids(inputs)?;
    let worktrees=if inputs.repositories.is_empty(){vec![]}else{worktree_plans(inputs,&attempt).map_err(anyhow::Error::msg)?};
    let brief = crate::memory::preview_worker_brief(attempt.as_str(),&reference.id,snapshot.budget_bytes,
        value["text"].as_str().context("retained knowledge text missing")?,&worktrees,&worker_output_path(inputs,&attempt).map_err(anyhow::Error::msg)?)?;
    crate::profile_config::frozen_definition(inputs.effective_profile.as_ref().context("effective profile missing")?)?
        .validate_gated_preparation(brief.prompt_chars)?;
    Ok(brief)
}

/// Build a signable approval from current state, retaining no new database rows.
pub fn draft(
    project: &Path, selection: &LaunchSelection, expected_head: u64, validity: Duration,
    deadline: Instant, cancellation: Cancellation,
) -> Result<LaunchDraft> {
    ensure!((1..=86400).contains(&validity.as_secs()) && validity.subsec_nanos()==0,
        "draft approval validity must be 1–86400 whole seconds");
    let proof = crate::profile_preparation::revalidate(project,&selection.profile,deadline,cancellation)?;
    let project = project.canonicalize()?;
    let mut db = crate::migration::open_active_controlled(&project,proof.read_control())?;
    let state = db.read_snapshot(Some(expected_head))?;
    let mut inputs = inputs(&proof,selection,&state,pending_approval())?;
    db.validate_launch_draft(&inputs,expected_head,now())?;
    let issued = now();
    let approval = ApprovalGrant { version: 1, scope: ApprovalScope::for_launch(&inputs).map_err(anyhow::Error::msg)?,
        policy: inputs.effective_profile.as_ref().unwrap().permission_policy.clone(), issued_unix_ms: issued,
        expires_unix_ms: issued.checked_add(i64::try_from(validity.as_millis())?).context("approval expiry overflow")? };
    inputs.approval = approval.reference().map_err(anyhow::Error::msg)?;
    let brief = brief(&project,&mut db,&inputs)?;
    proof.validate_for_launch()?;
    db.validate_launch_draft(&inputs,expected_head,now())?;
    let (attempt,_) = crate::store::reservations::record_ids(&inputs)?;
    let binding=state.runtime_bindings.iter().find(|b|b.id==selection.binding).context("launch binding missing")?;
    worktree_execution_route(&inputs,&attempt,&binding.identity).map_err(anyhow::Error::msg)?;
    let worktrees = if inputs.repositories.is_empty() {vec![]} else {worktree_plans(&inputs,&attempt).map_err(anyhow::Error::msg)?};
    Ok(LaunchDraft { head: expected_head, inputs, approval, brief, worktrees })
}

/// Reconstruct inputs and reserve only against an already installed owner-signed
/// grant. Approval use still occurs once at the later external-effect claim.
pub fn reserve(
    project: &Path, selection: &LaunchSelection, approval: &VersionedReference, expected_head: u64,
    deadline: Instant, cancellation: Cancellation,
) -> Result<Reservation> {
    let proof = crate::profile_preparation::revalidate(project,&selection.profile,deadline,cancellation)?;
    let project = project.canonicalize()?;
    let mut db = crate::migration::open_active_controlled(&project,proof.read_control())?;
    let state = db.read_snapshot(Some(expected_head))?;
    let inputs = inputs(&proof,selection,&state,approval.clone())?;
    db.validate_launch_draft(&inputs,expected_head,now())?;
    let installed = state.approvals.iter().find(|a|a.reference==*approval).context("signed launch approval is not installed")?;
    ensure!(installed.revoked.is_none() && installed.consumed.is_none(), "launch approval is unavailable");
    installed.grant.matches_launch(&inputs,&inputs.project_store,now()).map_err(anyhow::Error::msg)?;
    brief(&project,&mut db,&inputs)?;
    let (attempt,_) = crate::store::reservations::record_ids(&inputs)?;
    let binding=state.runtime_bindings.iter().find(|b|b.id==selection.binding).context("launch binding missing")?;
    worktree_execution_route(&inputs,&attempt,&binding.identity).map_err(anyhow::Error::msg)?;
    proof.validate_for_launch()?;
    Ok(db.reserve_prepared(&[PreparedLaunch { inputs }],expected_head,now())?)
}
