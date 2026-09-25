//! Local integrator. Checks run on the candidate checkout, not the worker branch.
//! The configured ref moves only through `update-ref` with the expected old oid.
//! A lost reply confirms that exact oid and no other. Dependencies are not satisfied.
mod git;

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::{
    domain::OperationId,
    operations::Claim,
    runner::{Cmd, RealRunner, Runner},
    store::{
        SqliteStore,
        integration::{
            IntegrationBegin, IntegrationFinish, IntegrationView, LEASE_OWNER, VerifiedIntegration,
        },
    },
    verification::{parse_checks, program_allowed},
};

use git::{BuiltCommit, GitRepo};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    None,
    #[cfg(test)]
    StaleBase,
    #[cfg(test)]
    CrashAfterRefUpdate,
}

impl Default for Fault {
    fn default() -> Self {
        Self::None
    }
}

pub struct IntegrateRequest {
    pub result_id: String,
    pub idempotency_key: String,
    pub repository: PathBuf,
    pub work_dir: PathBuf,
    pub fault: Fault,
}

#[derive(Debug)]
pub struct IntegrateOutcome {
    pub operation_id: String,
    pub state: String,
    pub commit_oid: Option<String>,
    pub reason: Option<String>,
}

fn payload_digest(key: &str, result_id: &str, repository: &str, reference: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{key}\0{result_id}\0{repository}\0{reference}").as_bytes())
    )
}

fn outcome_of(view: &IntegrationView) -> IntegrateOutcome {
    IntegrateOutcome {
        operation_id: view.operation_id.clone(),
        state: view.state.clone(),
        commit_oid: view.commit_oid.clone(),
        reason: view.reason.clone(),
    }
}

pub fn configure_integration_ref(
    store: &mut SqliteStore,
    repository: &PathBuf,
    ref_name: &str,
) -> Result<()> {
    let repo = GitRepo::open(repository)?;
    store
        .configure_integration_ref(&repo.identity, ref_name)
        .map_err(anyhow::Error::from)
}

pub fn integrate(store: &mut SqliteStore, request: &IntegrateRequest) -> Result<IntegrateOutcome> {
    let repo = GitRepo::open(&request.repository)?;
    let Some(reference) = store.integration_ref(&repo.identity)? else {
        bail!("integration ref is not configured");
    };
    if repo.is_checked_out(&reference)? {
        bail!("integration ref is checked out");
    }
    let verified = store.load_verified_for_integration(&request.result_id)?;
    if verified.repository != repo.identity {
        bail!("verified result belongs to another repository");
    }
    if verified.route != "verify_then_integrate" {
        bail!("verified result is not routed to integration");
    }
    let digest = payload_digest(
        &request.idempotency_key,
        &request.result_id,
        &repo.identity,
        &reference,
    );
    if let Some(existing) = store.find_integration(&repo.identity, &request.idempotency_key)? {
        if existing.payload_digest != digest || existing.verified_result_id != request.result_id {
            bail!("integration idempotency conflict");
        }
        return resume(store, &repo, request, existing, true);
    }
    let Some(base) = repo.ref_oid(&reference)? else {
        bail!("integration ref is missing");
    };
    if base.len() != oid_len(&verified.object_format)? {
        bail!("integration ref oid does not match object format");
    }
    let operation_id = store.begin_integration(&IntegrationBegin {
        repository: repo.identity.clone(),
        ref_name: reference,
        expected_old_oid: base.clone(),
        verified: verified.clone(),
        idempotency_key: request.idempotency_key.clone(),
        payload_digest: digest,
    })?;
    let now = jiff::Timestamp::now().as_millisecond();
    let operation = OperationId::new(operation_id.clone()).map_err(anyhow::Error::msg)?;
    let claim = match store.claim_operation(&operation, 1, LEASE_OWNER, now, 60_000) {
        Ok(claim) => claim,
        Err(error) => {
            let _ = store.abandon_unclaimed(&operation_id);
            return Err(error.into());
        }
    };
    drive_new(store, &repo, request, &verified, &base, claim)
}

pub fn reconcile_integration(
    store: &mut SqliteStore,
    repository: &PathBuf,
    idempotency_key: &str,
) -> Result<IntegrateOutcome> {
    let repo = GitRepo::open(repository)?;
    let Some(view) = store.find_integration(&repo.identity, idempotency_key)? else {
        bail!("integration operation is missing");
    };
    let request = IntegrateRequest {
        result_id: view.verified_result_id.clone(),
        idempotency_key: idempotency_key.to_string(),
        repository: repository.clone(),
        work_dir: PathBuf::new(),
        fault: Fault::None,
    };
    // Reconcile never builds another merge. It only classifies the ref we already named.
    resume(store, &repo, &request, view, false)
}

fn resume(
    store: &mut SqliteStore,
    repo: &GitRepo,
    request: &IntegrateRequest,
    view: IntegrationView,
    first_attempt: bool,
) -> Result<IntegrateOutcome> {
    if matches!(
        view.state.as_str(),
        "integrated" | "blocked" | "discarded" | "reconciliation_required"
    ) {
        return Ok(outcome_of(&view));
    }
    if !view.checks_passed {
        return Ok(outcome_of(&view));
    }
    let now = jiff::Timestamp::now().as_millisecond();
    let claim = store.held_claim(&view.operation_id, now)?;
    if first_attempt {
        publish(store, repo, request, &view, claim)
    } else {
        observe(store, repo, &view, claim)
    }
}

fn drive_new(
    store: &mut SqliteStore,
    repo: &GitRepo,
    request: &IntegrateRequest,
    verified: &VerifiedIntegration,
    base: &str,
    claim: Claim,
) -> Result<IntegrateOutcome> {
    let built = repo.build_merge(&request.work_dir, base, &verified.commit_oid)?;
    let BuiltCommit::Ready {
        oid,
        tree,
        checkout,
    } = built
    else {
        return finish(
            store,
            &claim,
            IntegrationFinish::Blocked {
                reason: "merge_conflict",
            },
        );
    };
    let now = jiff::Timestamp::now().as_millisecond();
    // Persist M before update-ref so crash recovery can confirm only this oid.
    store.record_candidate(&claim, &oid, &tree, &verified.commit_oid, now)?;
    if !checks_pass(&checkout, &verified.policy_body, &oid)? {
        return finish(
            store,
            &claim,
            IntegrationFinish::Blocked {
                reason: "checks_failed",
            },
        );
    }
    store.mark_checks_passed(&claim, now)?;
    repo.copy_objects_from(&checkout)?;
    if !repo.commit_matches(&oid, &tree, base, &verified.commit_oid)? {
        bail!("candidate objects are not in the repository");
    }
    let view = store.load_integration_operation(claim.operation.as_str())?;
    publish(store, repo, request, &view, claim)
}

fn publish(
    store: &mut SqliteStore,
    repo: &GitRepo,
    request: &IntegrateRequest,
    view: &IntegrationView,
    claim: Claim,
) -> Result<IntegrateOutcome> {
    #[cfg(test)]
    if request.fault == Fault::StaleBase {
        repo.advance_ref(&view.ref_name, &view.expected_old_oid)?;
    }
    #[cfg(not(test))]
    let _ = request;
    let Some(current) = repo.ref_oid(&view.ref_name)? else {
        bail!("integration ref is missing");
    };
    if current != view.expected_old_oid {
        return finish(
            store,
            &claim,
            IntegrationFinish::Discarded { reason: "stale_base" },
        );
    }
    if repo.is_checked_out(&view.ref_name)? {
        bail!("integration ref is checked out");
    }
    let commit = view
        .commit_oid
        .clone()
        .context("integration candidate is missing")?;
    let published = repo.cas_ref(&view.ref_name, &commit, &view.expected_old_oid)?;
    #[cfg(test)]
    if request.fault == Fault::CrashAfterRefUpdate && published {
        let view = store.load_integration_operation(claim.operation.as_str())?;
        return Ok(outcome_of(&view));
    }
    if !published {
        return observe(store, repo, view, claim);
    }
    observe(store, repo, view, claim)
}

/// Lost-reply classification. Confirms only the recorded candidate oid.
fn observe(
    store: &mut SqliteStore,
    repo: &GitRepo,
    view: &IntegrationView,
    claim: Claim,
) -> Result<IntegrateOutcome> {
    if view.generation <= 0 || view.parent_base.as_deref() != Some(view.expected_old_oid.as_str()) {
        return finish(
            store,
            &claim,
            IntegrationFinish::Reconciliation {
                reason: "ambiguous_ref",
            },
        );
    }
    let commit = view
        .commit_oid
        .clone()
        .context("integration candidate is missing")?;
    let tree = view.tree_oid.clone().context("integration candidate is missing")?;
    let parent = view
        .parent_verified
        .clone()
        .context("integration candidate is missing")?;
    let Some(current) = repo.ref_oid(&view.ref_name)? else {
        bail!("integration ref is missing");
    };
    if current == commit && repo.commit_matches(&commit, &tree, &view.expected_old_oid, &parent)? {
        return finish(store, &claim, IntegrationFinish::Confirm);
    }
    let other = store.other_integration_generation(&view.repository, &view.ref_name, &view.operation_id)?;
    if current == view.expected_old_oid && !other {
        if repo.is_checked_out(&view.ref_name)? {
            bail!("integration ref is checked out");
        }
        let updated = repo.cas_ref(&view.ref_name, &commit, &view.expected_old_oid)?;
        let Some(after) = repo.ref_oid(&view.ref_name)? else {
            bail!("integration ref is missing");
        };
        if updated && after == commit && repo.commit_matches(&commit, &tree, &view.expected_old_oid, &parent)?
        {
            return finish(store, &claim, IntegrationFinish::Confirm);
        }
    }
    finish(
        store,
        &claim,
        IntegrationFinish::Reconciliation {
            reason: "ambiguous_ref",
        },
    )
}

fn finish(store: &mut SqliteStore, claim: &Claim, kind: IntegrationFinish) -> Result<IntegrateOutcome> {
    let now = jiff::Timestamp::now().as_millisecond();
    store.finish_integration(claim, kind, now)?;
    let view = store.load_integration_operation(claim.operation.as_str())?;
    Ok(outcome_of(&view))
}

fn checks_pass(checkout: &std::path::Path, policy_body: &str, commit: &str) -> Result<bool> {
    let checks = match parse_checks(policy_body.as_bytes()) {
        Ok(checks) => checks,
        Err(_) => return Ok(false),
    };
    if !program_allowed(&checks[0], checkout) {
        return Ok(false);
    }
    let head = run_check(
        checkout,
        &["/usr/bin/git".into(), "rev-parse".into(), "HEAD".into()],
    )?;
    if head.as_deref() != Some(commit) {
        return Ok(false);
    }
    let args = checks.iter().map(|part| part.to_string()).collect::<Vec<_>>();
    Ok(run_check(checkout, &args)?.is_some())
}

fn run_check(checkout: &std::path::Path, args: &[String]) -> Result<Option<String>> {
    if args.is_empty() {
        return Ok(None);
    }
    let mut command = Cmd::new(args[0].clone(), Duration::from_secs(30));
    command.args = args[1..].to_vec();
    command.cwd = Some(checkout.to_path_buf());
    command.env_clear = true;
    command.env = vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), "/".into()),
        ("LANG".into(), "C".into()),
        ("LC_ALL".into(), "C".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_COUNT".into(), "2".into()),
        ("GIT_CONFIG_KEY_0".into(), "core.hooksPath".into()),
        ("GIT_CONFIG_VALUE_0".into(), "/dev/null".into()),
        ("GIT_CONFIG_KEY_1".into(), "safe.directory".into()),
        ("GIT_CONFIG_VALUE_1".into(), "*".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("GIT_NO_LAZY_FETCH".into(), "1".into()),
        ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
        ("GIT_ALLOW_PROTOCOL".into(), "".into()),
    ];
    let output = RealRunner.run(&command).context("integration check")?;
    if !output.success() {
        return Ok(None);
    }
    Ok(Some(output.stdout.trim().to_string()))
}

fn oid_len(format: &str) -> Result<usize> {
    match format {
        "sha1" => Ok(40),
        "sha256" => Ok(64),
        _ => bail!("unsupported object format"),
    }
}

#[cfg(test)]
mod tests;
