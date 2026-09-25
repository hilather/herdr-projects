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
    runner::{RealRunner, Runner},
    store::{
        SqliteStore,
        integration::{
            IntegrationBegin, IntegrationFinish, IntegrationView, LEASE_OWNER, VerifiedIntegration,
        },
    },
    verification::{self, parse_checks, supervise},
};

use git::{BuiltCommit, GitRepo};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    None,
    #[cfg(test)]
    StaleBase,
    #[cfg(test)]
    CrashAfterRefUpdate,
    #[cfg(test)]
    CrashBeforeBuild,
    #[cfg(test)]
    CrashBeforeChecks,
    #[cfg(test)]
    UpdateRefRejected,
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
        // An existing generation is classified from the ref. Do not discard a candidate
        // that is already the ref tip.
        return resume(store, &repo, request, existing);
    }
    if repo.is_checked_out(&reference)? {
        bail!("integration ref is checked out");
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
    let now = now_ms();
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
    resume(store, &repo, &request, view)
}

fn resume(
    store: &mut SqliteStore,
    repo: &GitRepo,
    request: &IntegrateRequest,
    view: IntegrationView,
) -> Result<IntegrateOutcome> {
    if matches!(
        view.state.as_str(),
        "integrated" | "blocked" | "discarded" | "reconciliation_required"
    ) {
        return Ok(outcome_of(&view));
    }
    if !view.checks_passed {
        return resume_incomplete(store, repo, request, &view);
    }
    settle(store, repo, request, &view, true)
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn drive_new(
    store: &mut SqliteStore,
    repo: &GitRepo,
    request: &IntegrateRequest,
    verified: &VerifiedIntegration,
    base: &str,
    claim: Claim,
) -> Result<IntegrateOutcome> {
    #[cfg(test)]
    if request.fault == Fault::CrashBeforeBuild {
        let view = store.load_integration_operation(claim.operation.as_str())?;
        return Ok(outcome_of(&view));
    }
    let built = repo.build_merge(&request.work_dir, base, &verified.commit_oid)?;
    let BuiltCommit::Ready {
        oid,
        tree,
        checkout,
    } = built
    else {
        return finish(
            store,
            claim.operation.as_str(),
            Some(&claim),
            IntegrationFinish::Blocked {
                reason: "merge_conflict",
            },
        );
    };
    let now = now_ms();
    // Persist M before update-ref so crash recovery can confirm only this oid.
    store.record_candidate(&claim, &oid, &tree, &verified.commit_oid, now)?;
    #[cfg(test)]
    if request.fault == Fault::CrashBeforeChecks {
        let view = store.load_integration_operation(claim.operation.as_str())?;
        return Ok(outcome_of(&view));
    }
    if !checks_pass(&request.work_dir, &checkout, verified, &oid, &tree)? {
        return finish(
            store,
            claim.operation.as_str(),
            Some(&claim),
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
    settle(store, repo, request, &view, true)
}

fn resume_incomplete(
    store: &mut SqliteStore,
    repo: &GitRepo,
    request: &IntegrateRequest,
    view: &IntegrationView,
) -> Result<IntegrateOutcome> {
    let Some(current) = repo.ref_oid(&view.ref_name)? else {
        bail!("integration ref is missing");
    };
    let candidate = view.commit_oid.clone();
    if current != view.expected_old_oid && candidate.as_deref() != Some(current.as_str()) {
        let claim = fresh_claim(store, &view.operation_id)?;
        return finish(
            store,
            &view.operation_id,
            Some(&claim),
            IntegrationFinish::Discarded { reason: "stale_base" },
        );
    }
    let claim = fresh_claim(store, &view.operation_id)?;
    if view.state == "effect_pending" {
        let verified = store.load_verified_for_integration(&view.verified_result_id)?;
        return drive_new(
            store,
            repo,
            request,
            &verified,
            &view.expected_old_oid,
            claim,
        );
    }
    let oid = view
        .commit_oid
        .clone()
        .context("integration candidate is missing")?;
    let tree = view
        .tree_oid
        .clone()
        .context("integration candidate is missing")?;
    let verified = store.load_verified_for_integration(&view.verified_result_id)?;
    let checkout = repo.checkout_candidate(&request.work_dir, &oid)?;
    if !checks_pass(&request.work_dir, &checkout, &verified, &oid, &tree)? {
        return finish(
            store,
            claim.operation.as_str(),
            Some(&claim),
            IntegrationFinish::Blocked {
                reason: "checks_failed",
            },
        );
    }
    store.mark_checks_passed(&claim, now_ms())?;
    repo.copy_objects_from(&checkout)?;
    if !repo.commit_matches(&oid, &tree, &view.expected_old_oid, &verified.commit_oid)? {
        bail!("candidate objects are not in the repository");
    }
    let view = store.load_integration_operation(&view.operation_id)?;
    settle(store, repo, request, &view, true)
}

/// Classify the ref, then publish only under a fresh claim. The update-ref status is not the decision.
fn settle(
    store: &mut SqliteStore,
    repo: &GitRepo,
    request: &IntegrateRequest,
    view: &IntegrationView,
    may_publish: bool,
) -> Result<IntegrateOutcome> {
    #[cfg(not(test))]
    let _ = request;
    if view.generation <= 0 || view.parent_base.as_deref() != Some(view.expected_old_oid.as_str()) {
        return finish_flexible(
            store,
            view,
            None,
            IntegrationFinish::Reconciliation {
                reason: "ambiguous_ref",
            },
        );
    }
    #[cfg(test)]
    if request.fault == Fault::StaleBase && may_publish {
        repo.advance_ref(&view.ref_name, &view.expected_old_oid)?;
    }
    let commit = view
        .commit_oid
        .clone()
        .context("integration candidate is missing")?;
    let tree = view
        .tree_oid
        .clone()
        .context("integration candidate is missing")?;
    let parent = view
        .parent_verified
        .clone()
        .context("integration candidate is missing")?;
    let Some(current) = repo.ref_oid(&view.ref_name)? else {
        bail!("integration ref is missing");
    };
    if repo.commit_matches(&commit, &tree, &view.expected_old_oid, &parent)? && current == commit {
        return finish_flexible(store, view, None, IntegrationFinish::Confirm);
    }
    if current == view.expected_old_oid {
        if !may_publish {
            let loaded = store.load_integration_operation(&view.operation_id)?;
            return Ok(outcome_of(&loaded));
        }
        let other = store.other_integration_generation(
            &view.repository,
            &view.ref_name,
            &view.operation_id,
        )?;
        if other {
            let loaded = store.load_integration_operation(&view.operation_id)?;
            return Ok(outcome_of(&loaded));
        }
        if repo.is_checked_out(&view.ref_name)? {
            bail!("integration ref is checked out");
        }
        let claim = fresh_claim(store, &view.operation_id)?;
        store.mark_publish_attempted(&claim, now_ms())?;
        #[cfg(test)]
        if request.fault == Fault::UpdateRefRejected {
            let loaded = store.load_integration_operation(&view.operation_id)?;
            return Ok(outcome_of(&loaded));
        }
        // A non-zero exit is not the classification. The following read-back is.
        let _ = repo.cas_ref(&view.ref_name, &commit, &view.expected_old_oid)?;
        #[cfg(test)]
        if request.fault == Fault::CrashAfterRefUpdate {
            let after = repo.ref_oid(&view.ref_name)?;
            if after.as_deref() == Some(commit.as_str()) {
                let loaded = store.load_integration_operation(&view.operation_id)?;
                return Ok(outcome_of(&loaded));
            }
        }
        return classify_readback(store, repo, view, &commit, &tree, &parent);
    }
    if view.reason.as_deref() == Some("publish_attempted") || current == commit {
        return finish_flexible(
            store,
            view,
            None,
            IntegrationFinish::Reconciliation {
                reason: "ambiguous_ref",
            },
        );
    }
    let claim = fresh_claim(store, &view.operation_id).ok();
    finish_flexible(
        store,
        view,
        claim.as_ref(),
        IntegrationFinish::Discarded { reason: "stale_base" },
    )
}

fn classify_readback(
    store: &mut SqliteStore,
    repo: &GitRepo,
    view: &IntegrationView,
    commit: &str,
    tree: &str,
    parent: &str,
) -> Result<IntegrateOutcome> {
    let Some(after) = repo.ref_oid(&view.ref_name)? else {
        bail!("integration ref is missing");
    };
    if after == commit && repo.commit_matches(commit, tree, &view.expected_old_oid, parent)? {
        return finish_flexible(store, view, None, IntegrationFinish::Confirm);
    }
    if after == view.expected_old_oid {
        // The ref did not move. A later claim may retry update-ref.
        let loaded = store.load_integration_operation(&view.operation_id)?;
        return Ok(outcome_of(&loaded));
    }
    finish_flexible(
        store,
        view,
        None,
        IntegrationFinish::Reconciliation {
            reason: "ambiguous_ref",
        },
    )
}

fn fresh_claim(store: &mut SqliteStore, operation_id: &str) -> Result<Claim> {
    let now = now_ms();
    if let Ok(claim) = store.held_claim(operation_id, now) {
        store.validate_claim(&claim, now)?;
        return Ok(claim);
    }
    let revision = store.requeue_expired_lease(operation_id, now)?;
    let id = OperationId::new(operation_id.to_string()).map_err(anyhow::Error::msg)?;
    let claim = store.claim_operation(&id, revision, LEASE_OWNER, now, 60_000)?;
    store.validate_claim(&claim, now)?;
    Ok(claim)
}

fn finish_flexible(
    store: &mut SqliteStore,
    view: &IntegrationView,
    claim: Option<&Claim>,
    kind: IntegrationFinish,
) -> Result<IntegrateOutcome> {
    let now = now_ms();
    if let Some(claim) = claim {
        if store.held_claim(claim.operation.as_str(), now).is_ok() {
            store.finish_integration(claim, kind, now)?;
            let loaded = store.load_integration_operation(&view.operation_id)?;
            return Ok(outcome_of(&loaded));
        }
    }
    store.finish_integration_observed(&view.operation_id, kind, now)?;
    let loaded = store.load_integration_operation(&view.operation_id)?;
    Ok(outcome_of(&loaded))
}

fn finish(
    store: &mut SqliteStore,
    operation_id: &str,
    claim: Option<&Claim>,
    kind: IntegrationFinish,
) -> Result<IntegrateOutcome> {
    let view = store.load_integration_operation(operation_id)?;
    finish_flexible(store, &view, claim, kind)
}

fn checks_pass(
    work: &std::path::Path,
    checkout: &std::path::Path,
    verified: &VerifiedIntegration,
    commit: &str,
    tree: &str,
) -> Result<bool> {
    let checks = match parse_checks(verified.policy_body.as_bytes()) {
        Ok(checks) => checks,
        Err(_) => return Ok(false),
    };
    let unshare = std::path::PathBuf::from("/usr/bin/unshare");
    if !verification::unshare_ready(&unshare) {
        return Ok(false);
    }
    let policy_path = work.join("integration-policy.json");
    std::fs::write(&policy_path, verified.policy_body.as_bytes()).context("policy file")?;
    let policy_digest = format!("{:x}", Sha256::digest(verified.policy_body.as_bytes()));
    // The child copies the candidate checkout into its tmpfs. Do not point it at the host branch.
    let launch = supervise::launch(&supervise::Spec {
        unshare_program: unshare,
        timeout: Duration::from_secs(30),
        checkout: checkout.to_path_buf(),
        policy: policy_path,
        scratch: work.join("ns-root"),
        checks,
        commit: commit.to_string(),
        tree: tree.to_string(),
        policy_digest,
    });
    let Ok(launch) = launch else {
        return Ok(false);
    };
    let output = match RealRunner.run(&launch.cmd) {
        Ok(output) => output,
        Err(_) => return Ok(false),
    };
    Ok(verification::isolated_check_ok(&output, commit, tree))
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
